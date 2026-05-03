//! Cryptographic capsule seals for Aster v0.2.
//!
//! v0.1 used `DefaultHasher` as a cheap deterministic root. That was useful
//! for exercising the continuation loop but it was not a security boundary.
//! This module adds the production-shaped capability primitive: a canonical
//! BLAKE3 digest of the capsule plus a keyed BLAKE3 MAC bound to the intended
//! cell and lease epoch. Runner cells can carry sealed capsule references as
//! bearer capabilities, while the broker can reject tampering, cross-cell
//! replay, and stale lease epochs before hydrating data.

use crate::{
    DeploymentId, Document, DocumentId, SnapshotCapsule, TenantId, Value, VersionedDocument,
};

const ASTER_SEAL_ALG: &str = "aster-blake3-keyed-v1";

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

#[derive(Clone, Debug, Eq, PartialEq)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapsuleSeal {
    pub algorithm: &'static str,
    pub digest: [u8; 32],
    pub mac: [u8; 32],
    pub cell_id: String,
    pub lease_epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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
}

impl std::fmt::Display for SealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongAlgorithm => write!(f, "capsule seal uses an unsupported algorithm"),
            Self::DigestMismatch => write!(f, "capsule digest does not match capsule bytes"),
            Self::MacMismatch => write!(f, "capsule MAC verification failed"),
            Self::WrongCell => write!(f, "capsule seal is bound to a different cell"),
            Self::WrongLeaseEpoch => write!(f, "capsule seal is bound to a different lease epoch"),
        }
    }
}

impl std::error::Error for SealError {}

impl SealedCapsule {
    pub fn new(capsule: SnapshotCapsule, key: &CapsuleSealKey, context: &SealContext) -> Self {
        let digest = capsule_digest(&capsule);
        let mac = capsule_mac(&capsule, &digest, key, context);
        Self {
            capsule,
            seal: CapsuleSeal {
                algorithm: ASTER_SEAL_ALG,
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
        let digest = capsule_digest(&self.capsule);
        if digest != self.seal.digest {
            return Err(SealError::DigestMismatch);
        }
        let mac = capsule_mac(&self.capsule, &digest, key, context);
        if mac != self.seal.mac {
            return Err(SealError::MacMismatch);
        }
        Ok(())
    }
}

pub fn capsule_digest(capsule: &SnapshotCapsule) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    encode_capsule(&mut hasher, capsule);
    *hasher.finalize().as_bytes()
}

fn capsule_mac(
    capsule: &SnapshotCapsule,
    digest: &[u8; 32],
    key: &CapsuleSealKey,
    context: &SealContext,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_keyed(key.bytes());
    hasher.update(ASTER_SEAL_ALG.as_bytes());
    put_str(&mut hasher, &context.cell_id);
    put_u64(&mut hasher, context.lease_epoch);
    put_bytes(&mut hasher, digest);
    // Bind the identity fields twice: once through the digest, once as explicit
    // MAC domain separation. This makes audit tooling able to inspect a seal
    // without canonical-decoding the full capsule.
    put_identity(
        &mut hasher,
        &capsule.tenant,
        &capsule.deployment,
        capsule.ts,
    );
    *hasher.finalize().as_bytes()
}

fn encode_capsule(hasher: &mut blake3::Hasher, capsule: &SnapshotCapsule) {
    hasher.update(b"aster-capsule-v2\0");
    put_identity(hasher, &capsule.tenant, &capsule.deployment, capsule.ts);
    put_u64(hasher, capsule.docs.len() as u64);
    for (key, value) in &capsule.docs {
        put_document_id(hasher, key);
        put_versioned_document(hasher, value);
    }
}

fn put_identity(
    hasher: &mut blake3::Hasher,
    tenant: &TenantId,
    deployment: &DeploymentId,
    ts: u64,
) {
    put_str(hasher, &tenant.0);
    put_str(hasher, &deployment.0);
    put_u64(hasher, ts);
}

fn put_document_id(hasher: &mut blake3::Hasher, key: &DocumentId) {
    put_str(hasher, &key.0);
}

fn put_versioned_document(hasher: &mut blake3::Hasher, value: &VersionedDocument) {
    match value.version {
        Some(version) => {
            hasher.update(&[1]);
            put_u64(hasher, version);
        }
        None => {
            hasher.update(&[0]);
        }
    }
    match &value.document {
        Some(document) => {
            hasher.update(&[1]);
            put_document(hasher, document);
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn put_document(hasher: &mut blake3::Hasher, document: &Document) {
    put_u64(hasher, document.len() as u64);
    for (field, value) in document {
        put_str(hasher, field);
        put_value(hasher, value);
    }
}

fn put_value(hasher: &mut blake3::Hasher, value: &Value) {
    match value {
        Value::Int(value) => {
            hasher.update(&[b'i']);
            hasher.update(&value.to_le_bytes());
        }
        Value::Text(value) => {
            hasher.update(&[b's']);
            put_str(hasher, value);
        }
        Value::Bool(value) => {
            hasher.update(&[b'b', u8::from(*value)]);
        }
        Value::Null => {
            hasher.update(&[b'n']);
        }
    }
}

fn put_str(hasher: &mut blake3::Hasher, value: &str) {
    put_bytes(hasher, value.as_bytes());
}

fn put_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    put_u64(hasher, bytes.len() as u64);
    hasher.update(bytes);
}

fn put_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{doc_with_i64, MvccStore};

    #[test]
    fn sealed_capsule_accepts_unchanged_bytes() {
        let store = MvccStore::new();
        let tenant = TenantId::new("tenant-a");
        let deployment = DeploymentId::new("dep-a");
        let key = DocumentId::new("docs/1");
        store.seed(key.clone(), doc_with_i64("value", 7));
        let capsule = store.build_capsule(tenant, deployment, store.snapshot_ts(), vec![key]);
        let seal_key = CapsuleSealKey::derive_for_tests(b"unit-test-key");
        let context = SealContext::new("cell-a", 11);
        let sealed = SealedCapsule::new(capsule, &seal_key, &context);
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
}
