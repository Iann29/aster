//! Cryptographic capsule seals for Aster.
//!
//! v0.1 used `DefaultHasher` as a cheap deterministic root. v0.2 introduced
//! the production-shaped capability primitive: a canonical BLAKE3 digest of
//! the capsule plus a keyed BLAKE3 MAC bound to the intended cell and lease
//! epoch. That construction MACed the *digest* (a prehash), which made the
//! security proof depend on collision resistance of unkeyed BLAKE3 in
//! addition to the keyed MAC assumption (Capsule Transaction Theorem,
//! Counterexample 2.2 / Repair K-PREHASH).
//!
//! v0.7 seals MAC the full framed canonical capsule bytes directly
//! (the theorem's Remark 3.4 direct-MAC seal), so forging any accepted
//! capsule reduces to a keyed-MAC forgery alone. The canonical digest is
//! still computed and carried in the seal, but only as an audit/tooling
//! convenience — it is not a MAC input.
//!
//! v3 (`aster-blake3-keyed-v3`) additionally binds each seal to the broker
//! session it was issued for (Repair C-CHANNEL): the wire broker mints an
//! unguessable 32-byte session id per InitialCapsule, and the binding
//! enters the MAC input with explicit domain separation. A capsule leaked
//! to another cell that reuses the same `cell_id` string, or replayed to a
//! re-spawned cell holding a fresh session, no longer verifies. In-process
//! brokers keep Unbound contexts — binding is mandatory where hostile
//! cells actually live, on the wire path.

use crate::canon::{encode_capsule_bytes, put_bytes, put_str, put_u64};
use crate::SnapshotCapsule;
use serde::{Deserialize, Serialize};

const ASTER_SEAL_ALG: &str = "aster-blake3-keyed-v3";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapsuleSealKey([u8; 32]);

impl CapsuleSealKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Deterministic helper for tests and fixtures. Production keys must come
    /// from the broker's secret store or KMS, not from a public seed string.
    pub fn derive_for_tests(seed: &[u8]) -> Self {
        Self(*blake3::hash(seed).as_bytes())
    }

    fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// A broker-minted, unguessable 32-byte session identifier (Repair
/// C-CHANNEL). Minted from OS entropy at InitialCapsule time; never derived
/// from cell-supplied content. Presenting a session id on a hydrate verb is
/// only a lookup key into the broker's trusted session table — the seal
/// binding below is what makes cross-session capsule transplant fail the
/// MAC, not the lookup itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct SessionBinding([u8; 32]);

impl SessionBinding {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SealContext {
    pub cell_id: String,
    pub lease_epoch: u64,
    /// `None` = Unbound, the in-process broker shape where no wire channel
    /// exists. Unbound is MAC-distinguishable from every bound state via a
    /// domain-separation tag byte in `seal_mac`, so stripping a binding can
    /// never reinterpret a bound seal as unbound (or vice versa).
    #[serde(default)]
    pub session: Option<SessionBinding>,
}

impl SealContext {
    /// Unbound context: `LocalCapsuleBroker` and other in-process callers.
    /// Wire brokers must construct contexts with [`SealContext::bound`].
    pub fn new(cell_id: impl Into<String>, lease_epoch: u64) -> Self {
        Self {
            cell_id: cell_id.into(),
            lease_epoch,
            session: None,
        }
    }

    pub fn bound(cell_id: impl Into<String>, lease_epoch: u64, session: SessionBinding) -> Self {
        Self {
            cell_id: cell_id.into(),
            lease_epoch,
            session: Some(session),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapsuleSeal {
    pub algorithm: String,
    pub digest: [u8; 32],
    pub mac: [u8; 32],
    pub cell_id: String,
    pub lease_epoch: u64,
    #[serde(default)]
    pub session: Option<SessionBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SealedCapsule {
    capsule: SnapshotCapsule,
    seal: CapsuleSeal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SealError {
    WrongAlgorithm,
    DigestMismatch,
    MacMismatch,
    WrongCell,
    WrongLeaseEpoch,
    WrongSession,
    MalformedCapsule(String),
}

impl std::fmt::Display for SealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongAlgorithm => write!(f, "capsule seal uses an unsupported algorithm"),
            Self::DigestMismatch => write!(f, "capsule digest does not match capsule bytes"),
            Self::MacMismatch => write!(f, "capsule MAC verification failed"),
            Self::WrongCell => write!(f, "capsule seal is bound to a different cell"),
            Self::WrongLeaseEpoch => write!(f, "capsule seal is bound to a different lease epoch"),
            Self::WrongSession => write!(f, "capsule seal is bound to a different broker session"),
            Self::MalformedCapsule(message) => {
                write!(f, "capsule structure is invalid: {message}")
            }
        }
    }
}

impl std::error::Error for SealError {}

impl SealedCapsule {
    pub fn new(capsule: SnapshotCapsule, key: &CapsuleSealKey, context: &SealContext) -> Self {
        let encoded = encode_capsule_bytes(&capsule);
        let digest = *blake3::hash(&encoded).as_bytes();
        let mac = seal_mac(&encoded, key, context);
        Self {
            capsule,
            seal: CapsuleSeal {
                algorithm: ASTER_SEAL_ALG.to_string(),
                digest,
                mac,
                cell_id: context.cell_id.clone(),
                lease_epoch: context.lease_epoch,
                session: context.session,
            },
        }
    }

    pub fn capsule(&self) -> &SnapshotCapsule {
        &self.capsule
    }

    pub fn capsule_mut_for_test(&mut self) -> &mut SnapshotCapsule {
        &mut self.capsule
    }

    pub fn seal_mut_for_test(&mut self) -> &mut CapsuleSeal {
        &mut self.seal
    }

    pub fn seal(&self) -> &CapsuleSeal {
        &self.seal
    }

    pub fn into_capsule(
        self,
        key: &CapsuleSealKey,
        context: &SealContext,
    ) -> Result<SnapshotCapsule, SealError> {
        self.verify(key, context)?;
        Ok(self.capsule)
    }

    pub fn verify(&self, key: &CapsuleSealKey, context: &SealContext) -> Result<(), SealError> {
        if self.seal.algorithm != ASTER_SEAL_ALG {
            return Err(SealError::WrongAlgorithm);
        }
        if self.seal.cell_id != context.cell_id {
            return Err(SealError::WrongCell);
        }
        if self.seal.lease_epoch != context.lease_epoch {
            return Err(SealError::WrongLeaseEpoch);
        }
        // Session ids are secret from every cell but their holder — the
        // bound/bound comparison is constant-time so a hostile cell cannot
        // use verify latency as an oracle to recover another session's id.
        // The MAC also binds the session (with domain separation), so this
        // pre-check is a friendly error, not the enforcement point.
        match (&self.seal.session, &context.session) {
            (None, None) => {}
            (Some(sealed), Some(expected)) if ct_eq(sealed.as_bytes(), expected.as_bytes()) => {}
            _ => return Err(SealError::WrongSession),
        }
        // W-CANON chokepoint: the JSON wire path deserializes permissively,
        // so structural validity (range certificates + cross-references) is
        // enforced here where every capsule must pass, not only in the
        // canonical-bytes decoder.
        if let Err(error) = self.capsule.validate_structure() {
            return Err(SealError::MalformedCapsule(error.to_string()));
        }
        let encoded = encode_capsule_bytes(&self.capsule);
        let digest = *blake3::hash(&encoded).as_bytes();
        if !ct_eq(&digest, &self.seal.digest) {
            return Err(SealError::DigestMismatch);
        }
        let mac = seal_mac(&encoded, key, context);
        if !ct_eq(&mac, &self.seal.mac) {
            return Err(SealError::MacMismatch);
        }
        Ok(())
    }
}

/// Constant-time 32-byte comparison via `blake3::Hash`, whose `PartialEq`
/// is documented constant-time. Avoids a timing oracle on MAC verification
/// without adding a dependency.
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    blake3::Hash::from(*a) == blake3::Hash::from(*b)
}

pub fn capsule_digest(capsule: &SnapshotCapsule) -> [u8; 32] {
    *blake3::hash(&encode_capsule_bytes(capsule)).as_bytes()
}

/// The MAC input is `alg ∥ lp(cid) ∥ le64(epoch) ∥ session ∥ lp(E(capsule))`:
/// the full framed canonical encoding, not its digest. `session` is a
/// domain-separated binding: tag byte 0 for Unbound, or tag byte 1 followed
/// by the 32 raw session bytes — the tag alone determines the frame length,
/// so unbound and bound messages can never collide. Tenant, deployment, and
/// snapshot are bound through `E(capsule)`, which frames them right after
/// the domain string.
fn seal_mac(encoded_capsule: &[u8], key: &CapsuleSealKey, context: &SealContext) -> [u8; 32] {
    let mut msg = Vec::with_capacity(
        ASTER_SEAL_ALG.len() + 8 + context.cell_id.len() + 8 + 33 + 8 + encoded_capsule.len(),
    );
    msg.extend_from_slice(ASTER_SEAL_ALG.as_bytes());
    put_str(&mut msg, &context.cell_id);
    put_u64(&mut msg, context.lease_epoch);
    match &context.session {
        None => msg.push(0),
        Some(session) => {
            msg.push(1);
            msg.extend_from_slice(session.as_bytes());
        }
    }
    put_bytes(&mut msg, encoded_capsule);
    *blake3::keyed_hash(key.bytes(), &msg).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        doc_with_i64, DeploymentId, DocumentId, KeyInterval, MvccStore, RangeCertificate,
        ScanDirection, ScanStop, TenantId, VersionedDocument,
    };

    fn sealed_fixture() -> (SealedCapsule, CapsuleSealKey, SealContext) {
        let store = MvccStore::new();
        let tenant = TenantId::new("tenant-a");
        let deployment = DeploymentId::new("dep-a");
        let key = DocumentId::new("docs/1");
        store.seed(key.clone(), doc_with_i64("value", 7));
        let capsule = store.build_capsule(tenant, deployment, store.snapshot_ts(), vec![key]);
        let seal_key = CapsuleSealKey::derive_for_tests(b"unit-test-key");
        let context = SealContext::new("cell-a", 11);
        let sealed = SealedCapsule::new(capsule, &seal_key, &context);
        (sealed, seal_key, context)
    }

    #[test]
    fn sealed_capsule_accepts_unchanged_bytes() {
        let (sealed, seal_key, context) = sealed_fixture();
        assert!(sealed.verify(&seal_key, &context).is_ok());
    }

    #[test]
    fn sealed_capsule_rejects_tampered_document() {
        let store = MvccStore::new();
        let tenant = TenantId::new("tenant-a");
        let deployment = DeploymentId::new("dep-a");
        let key = DocumentId::new("docs/1");
        store.seed(key.clone(), doc_with_i64("value", 7));
        let capsule =
            store.build_capsule(tenant, deployment, store.snapshot_ts(), vec![key.clone()]);
        let seal_key = CapsuleSealKey::derive_for_tests(b"unit-test-key");
        let context = SealContext::new("cell-a", 11);
        let mut sealed = SealedCapsule::new(capsule, &seal_key, &context);
        sealed.capsule_mut_for_test().hydrate_point(
            key,
            VersionedDocument {
                version: Some(99),
                document: Some(doc_with_i64("value", 8)),
            },
        );
        assert_eq!(
            sealed.verify(&seal_key, &context),
            Err(SealError::DigestMismatch)
        );
    }

    #[test]
    fn sealed_capsule_rejects_wrong_cell_context() {
        let capsule =
            SnapshotCapsule::empty(TenantId::new("tenant-a"), DeploymentId::new("dep-a"), 1);
        let seal_key = CapsuleSealKey::derive_for_tests(b"unit-test-key");
        let context = SealContext::new("cell-a", 11);
        let sealed = SealedCapsule::new(capsule, &seal_key, &context);
        let wrong_cell = SealContext::new("cell-b", 11);
        assert_eq!(
            sealed.verify(&seal_key, &wrong_cell),
            Err(SealError::WrongCell)
        );
    }

    #[test]
    fn sealed_capsule_rejects_wrong_lease_epoch() {
        let (sealed, seal_key, _context) = sealed_fixture();
        let wrong_epoch = SealContext::new("cell-a", 12);
        assert_eq!(
            sealed.verify(&seal_key, &wrong_epoch),
            Err(SealError::WrongLeaseEpoch)
        );
    }

    #[test]
    fn sealed_capsule_rejects_flipped_mac_bit() {
        let (mut sealed, seal_key, context) = sealed_fixture();
        sealed.seal_mut_for_test().mac[0] ^= 0x01;
        assert_eq!(
            sealed.verify(&seal_key, &context),
            Err(SealError::MacMismatch)
        );
    }

    #[test]
    fn sealed_capsule_rejects_legacy_v1_algorithm() {
        let (mut sealed, seal_key, context) = sealed_fixture();
        sealed.seal_mut_for_test().algorithm = "aster-blake3-keyed-v1".to_string();
        assert_eq!(
            sealed.verify(&seal_key, &context),
            Err(SealError::WrongAlgorithm)
        );
    }

    #[test]
    fn sealed_capsule_rejects_legacy_v2_algorithm() {
        let (mut sealed, seal_key, context) = sealed_fixture();
        sealed.seal_mut_for_test().algorithm = "aster-blake3-keyed-v2".to_string();
        assert_eq!(
            sealed.verify(&seal_key, &context),
            Err(SealError::WrongAlgorithm)
        );
    }

    #[test]
    fn sealed_capsule_rejects_wrong_session() {
        let store = MvccStore::new();
        let key = DocumentId::new("docs/1");
        store.seed(key.clone(), doc_with_i64("value", 7));
        let capsule = store.build_capsule(
            TenantId::new("tenant-a"),
            DeploymentId::new("dep-a"),
            store.snapshot_ts(),
            vec![key],
        );
        let seal_key = CapsuleSealKey::derive_for_tests(b"unit-test-key");
        let session_a = SessionBinding::from_bytes([0xaa; 32]);
        let session_b = SessionBinding::from_bytes([0xbb; 32]);
        let sealed = SealedCapsule::new(
            capsule,
            &seal_key,
            &SealContext::bound("cell-a", 11, session_a),
        );

        assert!(sealed
            .verify(&seal_key, &SealContext::bound("cell-a", 11, session_a))
            .is_ok());
        assert_eq!(
            sealed.verify(&seal_key, &SealContext::bound("cell-a", 11, session_b)),
            Err(SealError::WrongSession)
        );
    }

    /// Bound and Unbound must reject each other in both directions: a
    /// capsule sealed for a wire session never verifies in an unbound
    /// context, and an in-process unbound capsule never verifies on a
    /// session-bound channel.
    #[test]
    fn sealed_capsule_rejects_bound_unbound_mixups() {
        let (unbound_sealed, seal_key, unbound_context) = sealed_fixture();
        let session = SessionBinding::from_bytes([0xcc; 32]);
        let bound_context = SealContext::bound("cell-a", 11, session);
        assert_eq!(
            unbound_sealed.verify(&seal_key, &bound_context),
            Err(SealError::WrongSession)
        );

        let bound_sealed = SealedCapsule::new(
            SnapshotCapsule::empty(TenantId::new("tenant-a"), DeploymentId::new("dep-a"), 1),
            &seal_key,
            &bound_context,
        );
        assert_eq!(
            bound_sealed.verify(&seal_key, &unbound_context.clone()),
            Err(SealError::WrongSession)
        );
    }

    /// MAC distinguishability of the Unbound state: stripping the session
    /// field from a bound seal (so the friendly None/None pre-check passes
    /// against an unbound context) must still die on the MAC, because the
    /// domain-separation tag byte differs. The seal field is a claim; the
    /// MAC is the enforcement.
    #[test]
    fn stripping_session_from_bound_seal_fails_mac() {
        let seal_key = CapsuleSealKey::derive_for_tests(b"unit-test-key");
        let session = SessionBinding::from_bytes([0xdd; 32]);
        let mut sealed = SealedCapsule::new(
            SnapshotCapsule::empty(TenantId::new("tenant-a"), DeploymentId::new("dep-a"), 1),
            &seal_key,
            &SealContext::bound("cell-a", 11, session),
        );
        sealed.seal_mut_for_test().session = None;
        assert_eq!(
            sealed.verify(&seal_key, &SealContext::new("cell-a", 11)),
            Err(SealError::MacMismatch)
        );
    }

    #[test]
    fn sealed_capsule_rejects_tampered_digest_field() {
        let (mut sealed, seal_key, context) = sealed_fixture();
        sealed.seal_mut_for_test().digest = [0u8; 32];
        assert_eq!(
            sealed.verify(&seal_key, &context),
            Err(SealError::DigestMismatch)
        );
    }

    #[test]
    fn verify_rejects_malformed_range_structure() {
        let (mut sealed, seal_key, context) = sealed_fixture();
        sealed.capsule_mut_for_test().ranges.push(RangeCertificate {
            interval: KeyInterval::Prefix("docs/".into()),
            direction: ScanDirection::Ascending,
            limit: 5,
            keys: vec![DocumentId::new("docs/ghost")],
            stop: ScanStop::Exhausted,
        });
        assert!(matches!(
            sealed.verify(&seal_key, &context),
            Err(SealError::MalformedCapsule(_))
        ));
    }

    /// Pins the exact wire construction. If this test breaks, the seal
    /// format changed and every issued capsule in the wild is invalidated —
    /// that must be a deliberate, versioned decision, never drift.
    #[test]
    fn seal_test_vector_is_stable() {
        let capsule =
            SnapshotCapsule::empty(TenantId::new("tenant-a"), DeploymentId::new("dep-a"), 42);
        let seal_key = CapsuleSealKey::derive_for_tests(b"test-vector-key");
        let context = SealContext::new("cell-tv", 7);
        let sealed = SealedCapsule::new(capsule, &seal_key, &context);
        let mac_hex: String = sealed
            .seal()
            .mac
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        // Deliberate re-pin (2026-07-16): seal domain bumped to
        // aster-blake3-keyed-v3 when the session binding entered the MAC
        // input with domain separation (Repair C-CHANNEL). Precedent: the
        // aster-capsule-v3 re-pin when range certificates entered the
        // canonical encoding.
        assert_eq!(
            mac_hex,
            "003c43a8bf784bcc173a1a93b20013b89f8a4f4313b14b84a0597e7616385d6a"
        );
    }

    /// Pins the bound-seal wire construction (tag byte 1 + 32 raw session
    /// bytes after the lease epoch). Same re-pin discipline as the unbound
    /// vector above: a break here means every issued capsule is
    /// invalidated, which must be deliberate and versioned.
    #[test]
    fn bound_seal_test_vector_is_stable() {
        let capsule =
            SnapshotCapsule::empty(TenantId::new("tenant-a"), DeploymentId::new("dep-a"), 42);
        let seal_key = CapsuleSealKey::derive_for_tests(b"test-vector-key");
        let session = SessionBinding::from_bytes([0x5e; 32]);
        let context = SealContext::bound("cell-tv", 7, session);
        let sealed = SealedCapsule::new(capsule, &seal_key, &context);
        let mac_hex: String = sealed
            .seal()
            .mac
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            mac_hex,
            "a9677638b50e75c226f0413d1d8cf4f9168c2a9be666a229dd1dd9bf3d58c30e"
        );
    }
}
