//! Capsule broker boundary for Aster v0.2.
//!
//! The v0.1 runner accepted `&MvccStore` directly, which modeled the shape of
//! read traps but not the authority split. This crate makes the split explicit:
//! cells talk to a `CapsuleBrokerClient`; the broker owns the read-capable
//! store and the capsule seal key. The provided `LocalCapsuleBroker` is still
//! in-process for tests, but the cell-facing API contains no database handle.

use aster_capsule::{
    CapsuleSealKey, DeploymentId, DocumentId, MvccStore, SealContext, SealError, SealedCapsule,
    TenantId,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrokerError {
    Seal(SealError),
    TenantMismatch,
    DeploymentMismatch,
}

impl From<SealError> for BrokerError {
    fn from(value: SealError) -> Self {
        Self::Seal(value)
    }
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Seal(error) => write!(f, "capsule seal rejected: {error}"),
            Self::TenantMismatch => write!(f, "hydrate tenant did not match broker tenant"),
            Self::DeploymentMismatch => {
                write!(f, "hydrate deployment did not match broker deployment")
            }
        }
    }
}

impl std::error::Error for BrokerError {}

/// Cell-facing broker capability.
///
/// Production implementations can back this trait with UDS/gRPC. The important
/// property is that the cell receives this narrow interface, not a database
/// pool or store.
pub trait CapsuleBrokerClient {
    fn initial_capsule(
        &self,
        context: &SealContext,
        tenant: TenantId,
        deployment: DeploymentId,
        snapshot_ts: u64,
        prewarm: Vec<DocumentId>,
    ) -> Result<SealedCapsule, BrokerError>;

    fn hydrate_point(
        &self,
        context: &SealContext,
        capsule: SealedCapsule,
        key: DocumentId,
    ) -> Result<SealedCapsule, BrokerError>;
}

/// In-process broker used by the prototype and tests.
///
/// It intentionally owns the only path from read traps to `MvccStore`; V8 cells
/// in v0.2 can execute entirely through this trait and never receive `&MvccStore`.
pub struct LocalCapsuleBroker<'a> {
    store: &'a MvccStore,
    seal_key: CapsuleSealKey,
}

impl<'a> LocalCapsuleBroker<'a> {
    pub fn new(store: &'a MvccStore, seal_key: CapsuleSealKey) -> Self {
        Self { store, seal_key }
    }

    pub fn seal_key(&self) -> &CapsuleSealKey {
        &self.seal_key
    }
}

impl CapsuleBrokerClient for LocalCapsuleBroker<'_> {
    fn initial_capsule(
        &self,
        context: &SealContext,
        tenant: TenantId,
        deployment: DeploymentId,
        snapshot_ts: u64,
        prewarm: Vec<DocumentId>,
    ) -> Result<SealedCapsule, BrokerError> {
        let capsule = self
            .store
            .build_capsule(tenant, deployment, snapshot_ts, prewarm);
        Ok(SealedCapsule::new(capsule, &self.seal_key, context))
    }

    fn hydrate_point(
        &self,
        context: &SealContext,
        capsule: SealedCapsule,
        key: DocumentId,
    ) -> Result<SealedCapsule, BrokerError> {
        let mut capsule = capsule.into_capsule(&self.seal_key, context)?;
        let value = self.store.read_at(&key, capsule.ts);
        capsule.hydrate_point(key, value);
        Ok(SealedCapsule::new(capsule, &self.seal_key, context))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aster_capsule::{doc_with_i64, Value};

    #[test]
    fn broker_hydrates_and_reseals_without_exposing_store_to_cell() {
        let store = MvccStore::new();
        let tenant = TenantId::new("tenant-broker");
        let deployment = DeploymentId::new("dep-broker");
        let key = DocumentId::new("docs/1");
        store.seed(key.clone(), doc_with_i64("value", 5));
        let broker = LocalCapsuleBroker::new(
            &store,
            CapsuleSealKey::derive_for_tests(b"broker-unit-test"),
        );
        let context = SealContext::new("cell-1", 9);
        let sealed = broker
            .initial_capsule(
                &context,
                tenant,
                deployment,
                store.snapshot_ts(),
                Vec::new(),
            )
            .expect("initial capsule");
        assert!(sealed.capsule().get(&key).is_none());

        let sealed = broker
            .hydrate_point(&context, sealed, key.clone())
            .expect("hydrate");
        let value = sealed
            .capsule()
            .get(&key)
            .and_then(|doc| doc.document.as_ref())
            .and_then(|doc| doc.get("value"));
        assert_eq!(value, Some(&Value::Int(5)));
    }

    #[test]
    fn broker_rejects_wrong_cell_seal_on_hydrate() {
        let store = MvccStore::new();
        let tenant = TenantId::new("tenant-broker");
        let deployment = DeploymentId::new("dep-broker");
        let key = DocumentId::new("docs/1");
        store.seed(key.clone(), doc_with_i64("value", 5));
        let broker = LocalCapsuleBroker::new(
            &store,
            CapsuleSealKey::derive_for_tests(b"broker-unit-test"),
        );
        let cell_a = SealContext::new("cell-a", 9);
        let cell_b = SealContext::new("cell-b", 9);
        let sealed = broker
            .initial_capsule(&cell_a, tenant, deployment, store.snapshot_ts(), Vec::new())
            .expect("initial capsule");

        assert_eq!(
            broker.hydrate_point(&cell_b, sealed, key),
            Err(BrokerError::Seal(SealError::WrongCell))
        );
    }
}
