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
//! (`aster-blake3-keyed-v2`, the theorem's Remark 3.4 direct-MAC seal), so
//! forging any accepted capsule reduces to a keyed-MAC forgery alone. The
//! canonical digest is still computed and carried in the seal, but only as
//! an audit/tooling convenience — it is not a MAC input.

use crate::canon::{encode_capsule_bytes, put_bytes, put_str, put_u64};
use crate::SnapshotCapsule;
use serde::{Deserialize, Serialize};

const ASTER_SEAL_ALG: &str = "aster-blake3-keyed-v2";

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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SealContext {
    pub cell_id: String,
    pub lease_epoch: u64,
}

impl SealContext {
    pub fn new(cell_id: impl Into<String>, lease_epoch: u64) -> Self {
        Self {
            cell_id: cell_id.into(),
            lease_epoch,
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

/// The MAC input is `alg ∥ lp(cid) ∥ le64(epoch) ∥ lp(E(capsule))`: the full
/// framed canonical encoding, not its digest. Tenant, deployment, and
/// snapshot are bound through `E(capsule)`, which frames them right after
/// the domain string.
fn seal_mac(encoded_capsule: &[u8], key: &CapsuleSealKey, context: &SealContext) -> [u8; 32] {
    let mut msg = Vec::with_capacity(
        ASTER_SEAL_ALG.len() + 8 + context.cell_id.len() + 8 + 8 + encoded_capsule.len(),
    );
    msg.extend_from_slice(ASTER_SEAL_ALG.as_bytes());
    put_str(&mut msg, &context.cell_id);
    put_u64(&mut msg, context.lease_epoch);
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
        // Deliberate re-pin (2026-07-16): capsule domain bumped to
        // aster-capsule-v3 when the range-certificate sequence entered the
        // canonical encoding.
        assert_eq!(
            mac_hex,
            "c28b56db9ec48aec56ff10f216550ceb1bfaeb049c42dc14c87fa88ee15ea21d"
        );
    }
}
