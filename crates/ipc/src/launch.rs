use std::fmt;

use base64::Engine as _;

const VERSION: u8 = 1;
const DOMAIN: &[u8] = b"aster-launch-token-v1\0";
const MAC_BYTES: usize = 32;
const NONCE_BYTES: usize = 16;
const MAX_TOKEN_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchTokenClaims {
    pub cell_id: String,
    pub tenant: String,
    pub deployment: String,
    pub lease_epoch: u64,
    pub expires_at_unix_s: u64,
    pub nonce: [u8; NONCE_BYTES],
}

#[derive(Clone)]
pub struct LaunchTokenKey([u8; 32]);

impl LaunchTokenKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for LaunchTokenKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LaunchTokenKey([REDACTED])")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchTokenError {
    Malformed(String),
    WrongMac,
    Expired,
    IdentityMismatch,
}

impl fmt::Display for LaunchTokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(message) => write!(f, "malformed launch token: {message}"),
            Self::WrongMac => f.write_str("launch token MAC verification failed"),
            Self::Expired => f.write_str("launch token expired"),
            Self::IdentityMismatch => {
                f.write_str("launch token is not bound to this cell/deployment/epoch")
            }
        }
    }
}

impl std::error::Error for LaunchTokenError {}

/// Issue a token from trusted launch metadata. The caller supplies a random
/// nonce so token generation stays deterministic and testable in this module;
/// production issuers must fill it with OS entropy.
pub fn issue_launch_token(
    key: &LaunchTokenKey,
    claims: &LaunchTokenClaims,
) -> Result<String, LaunchTokenError> {
    let mut payload = Vec::with_capacity(
        1 + 8
            + 8
            + NONCE_BYTES
            + 2
            + claims.tenant.len()
            + 2
            + claims.deployment.len()
            + 2
            + claims.cell_id.len()
            + MAC_BYTES,
    );
    payload.push(VERSION);
    payload.extend_from_slice(&claims.expires_at_unix_s.to_le_bytes());
    payload.extend_from_slice(&claims.lease_epoch.to_le_bytes());
    payload.extend_from_slice(&claims.nonce);
    put_string(&mut payload, "tenant", &claims.tenant)?;
    put_string(&mut payload, "deployment", &claims.deployment)?;
    put_string(&mut payload, "cell_id", &claims.cell_id)?;
    let mac = token_mac(key, &payload);
    payload.extend_from_slice(&mac);
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload))
}

pub fn verify_launch_token(
    key: &LaunchTokenKey,
    encoded: &str,
    now_unix_s: u64,
) -> Result<LaunchTokenClaims, LaunchTokenError> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|error| LaunchTokenError::Malformed(format!("base64url: {error}")))?;
    if bytes.len() > MAX_TOKEN_BYTES {
        return Err(LaunchTokenError::Malformed(format!(
            "{} bytes exceeds {MAX_TOKEN_BYTES}-byte cap",
            bytes.len()
        )));
    }
    if bytes.len() < 1 + 8 + 8 + NONCE_BYTES + 2 * 3 + MAC_BYTES {
        return Err(LaunchTokenError::Malformed("truncated".into()));
    }
    let (payload, tag) = bytes.split_at(bytes.len() - MAC_BYTES);
    let expected = token_mac(key, payload);
    let tag: [u8; MAC_BYTES] = tag
        .try_into()
        .map_err(|_| LaunchTokenError::Malformed("bad MAC length".into()))?;
    if blake3::Hash::from(tag) != blake3::Hash::from(expected) {
        return Err(LaunchTokenError::WrongMac);
    }

    let mut cursor = Cursor::new(payload);
    let version = cursor.take_u8()?;
    if version != VERSION {
        return Err(LaunchTokenError::Malformed(format!(
            "unsupported version {version}"
        )));
    }
    let expires_at_unix_s = cursor.take_u64()?;
    let lease_epoch = cursor.take_u64()?;
    let nonce = cursor.take_array::<NONCE_BYTES>()?;
    let tenant = cursor.take_string("tenant")?;
    let deployment = cursor.take_string("deployment")?;
    let cell_id = cursor.take_string("cell_id")?;
    if !cursor.is_empty() {
        return Err(LaunchTokenError::Malformed("trailing payload bytes".into()));
    }
    if expires_at_unix_s <= now_unix_s {
        return Err(LaunchTokenError::Expired);
    }
    Ok(LaunchTokenClaims {
        cell_id,
        tenant,
        deployment,
        lease_epoch,
        expires_at_unix_s,
        nonce,
    })
}

/// Verifies a short-lived launch capability. The same token may authorize
/// multiple transaction attempts for one invocation (module acquisition and
/// OCC retries); expiry, epoch, and identity binding prevent cross-launch use.
pub struct LaunchAuthorizer {
    key: LaunchTokenKey,
}

impl LaunchAuthorizer {
    pub fn new(key: LaunchTokenKey) -> Self {
        Self { key }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn verify(
        &self,
        token: &str,
        cell_id: &str,
        tenant: &str,
        deployment: &str,
        lease_epoch: u64,
        now_unix_s: u64,
    ) -> Result<(), LaunchTokenError> {
        let claims = verify_launch_token(&self.key, token, now_unix_s)?;
        if claims.cell_id != cell_id
            || claims.tenant != tenant
            || claims.deployment != deployment
            || claims.lease_epoch != lease_epoch
        {
            return Err(LaunchTokenError::IdentityMismatch);
        }
        Ok(())
    }
}

fn token_mac(key: &LaunchTokenKey, payload: &[u8]) -> [u8; MAC_BYTES] {
    let mut hasher = blake3::Hasher::new_keyed(&key.0);
    hasher.update(DOMAIN);
    hasher.update(payload);
    *hasher.finalize().as_bytes()
}

fn put_string(out: &mut Vec<u8>, name: &str, value: &str) -> Result<(), LaunchTokenError> {
    if value.is_empty() {
        return Err(LaunchTokenError::Malformed(format!("{name} is empty")));
    }
    let len = u16::try_from(value.len())
        .map_err(|_| LaunchTokenError::Malformed(format!("{name} is too long")))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take_u8(&mut self) -> Result<u8, LaunchTokenError> {
        Ok(self.take_array::<1>()?[0])
    }

    fn take_u64(&mut self) -> Result<u64, LaunchTokenError> {
        Ok(u64::from_le_bytes(self.take_array::<8>()?))
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N], LaunchTokenError> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or_else(|| LaunchTokenError::Malformed("length overflow".into()))?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| LaunchTokenError::Malformed("truncated".into()))?;
        self.offset = end;
        bytes
            .try_into()
            .map_err(|_| LaunchTokenError::Malformed("truncated".into()))
    }

    fn take_string(&mut self, name: &str) -> Result<String, LaunchTokenError> {
        let len = u16::from_le_bytes(self.take_array::<2>()?) as usize;
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| LaunchTokenError::Malformed("length overflow".into()))?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| LaunchTokenError::Malformed(format!("truncated {name}")))?;
        self.offset = end;
        let value = std::str::from_utf8(bytes)
            .map_err(|error| LaunchTokenError::Malformed(format!("{name} utf8: {error}")))?;
        if value.is_empty() {
            return Err(LaunchTokenError::Malformed(format!("{name} is empty")));
        }
        Ok(value.to_string())
    }

    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> LaunchTokenClaims {
        LaunchTokenClaims {
            cell_id: "cell-1".into(),
            tenant: "tenant-a".into(),
            deployment: "dep-a".into(),
            lease_epoch: 7,
            expires_at_unix_s: 1_100,
            nonce: [0x33; NONCE_BYTES],
        }
    }

    #[test]
    fn token_round_trip_binds_every_launch_dimension() {
        let key = LaunchTokenKey::from_bytes([0x11; 32]);
        let token = issue_launch_token(&key, &claims()).expect("issue");
        let decoded = verify_launch_token(&key, &token, 1_000).expect("verify");
        assert_eq!(decoded, claims());
    }

    #[test]
    fn wrong_key_expiry_and_truncation_fail_closed() {
        let key = LaunchTokenKey::from_bytes([0x11; 32]);
        let token = issue_launch_token(&key, &claims()).expect("issue");
        assert_eq!(
            verify_launch_token(&LaunchTokenKey::from_bytes([0x22; 32]), &token, 1_000),
            Err(LaunchTokenError::WrongMac)
        );
        assert_eq!(
            verify_launch_token(&key, &token, 1_100),
            Err(LaunchTokenError::Expired)
        );
        assert!(matches!(
            verify_launch_token(&key, "AQ", 1_000),
            Err(LaunchTokenError::Malformed(_))
        ));
    }

    #[test]
    fn authorizer_checks_identity_and_allows_bounded_retry_reuse() {
        let key = LaunchTokenKey::from_bytes([0x11; 32]);
        let token = issue_launch_token(&key, &claims()).expect("issue");
        let authorizer = LaunchAuthorizer::new(key);
        assert_eq!(
            authorizer.verify(&token, "other", "tenant-a", "dep-a", 7, 1_000),
            Err(LaunchTokenError::IdentityMismatch)
        );
        authorizer
            .verify(&token, "cell-1", "tenant-a", "dep-a", 7, 1_000)
            .expect("first transaction attempt");
        authorizer
            .verify(&token, "cell-1", "tenant-a", "dep-a", 7, 1_000)
            .expect("same invocation may retry before expiry");
    }
}
