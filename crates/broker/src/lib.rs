//! Capsule broker boundary for Aster v0.2+.
//!
//! The v0.1 runner accepted `&MvccStore` directly, which modeled the shape of
//! read traps but not the authority split. This crate makes the split explicit:
//! cells talk to a `CapsuleBrokerClient`; the broker owns the read-capable
//! store and the capsule seal key. The provided `LocalCapsuleBroker` is still
//! in-process for tests, but the cell-facing API contains no database handle.
//!
//! v0.3+ generalises the storage backend behind a `CapsuleStore` trait
//! (see `store.rs`) so a real Postgres adapter can plug in without touching
//! the cell-facing IPC. v0.7 adds the write-side twin: `CommitFence`
//! (see `fence.rs`) abstracts the lease authority + commit fence so the
//! brokerd binary drives the same admission semantics against Postgres
//! (`WritePlane`) or the in-memory prototype store.

pub mod fence;
pub mod store;

pub use fence::{CommitFence, CommitOutcome, FenceInput, MemoryFence};
pub use store::{mint_opaque_document_id, CapsuleStore, StoreError};

use aster_capsule::{
    CapsuleSealKey, DeploymentId, DocumentId, SealContext, SealError, SealedCapsule, TenantId,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrokerError {
    Seal(SealError),
    TenantMismatch,
    DeploymentMismatch,
    /// A prefix hydrate asked for `limit == 0`, which can never produce a
    /// valid range certificate (Definition 1.1 requires ℓ >= 1). Rejected
    /// before the seal or store is consulted.
    ZeroScanLimit,
    Remote(String),
}

impl From<SealError> for BrokerError {
    fn from(value: SealError) -> Self {
        Self::Seal(value)
    }
}

impl From<StoreError> for BrokerError {
    fn from(value: StoreError) -> Self {
        // Cells never see StoreError directly — collapse it onto the
        // existing Remote variant with a structured prefix so operator
        // logs can still grep for the sub-class.
        match value {
            StoreError::Unavailable(msg) => Self::Remote(format!("unavailable: {msg}")),
            StoreError::Stale { requested, latest } => Self::Remote(format!(
                "stale_snapshot: requested={requested} latest={latest}"
            )),
            StoreError::Backend(msg) => Self::Remote(format!("backend: {msg}")),
        }
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
            Self::ZeroScanLimit => write!(f, "prefix scan limit must be >= 1"),
            Self::Remote(error) => write!(f, "remote broker error: {error}"),
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

    /// Certified prefix hydrate: run a limited ascending scan at the
    /// capsule's snapshot and merge the resulting `RangeCertificate` plus
    /// live entries into the capsule (`hydrate_range`), resealing it. The
    /// certificate is evidence about the capsule snapshot — implementations
    /// must scan at `capsule.ts`, never at the store head.
    fn hydrate_prefix(
        &self,
        context: &SealContext,
        capsule: SealedCapsule,
        prefix: String,
        limit: usize,
    ) -> Result<SealedCapsule, BrokerError>;

    /// Allocate a fresh id for `db.insert` within the active invocation.
    /// This grants no write by itself; the id reaches storage only through
    /// the sealed-capsule Commit fence.
    fn mint_document_id(
        &self,
        context: &SealContext,
        table: &str,
    ) -> Result<DocumentId, BrokerError>;
}

/// In-process broker used by the prototype and tests.
///
/// Generic over any `CapsuleStore` so a Postgres adapter can plug in without
/// changing the cell-facing API. Today's call sites (`LocalCapsuleBroker::new(&store, ...)`)
/// keep compiling because `&MvccStore: CapsuleStore` via the blanket in
/// `store.rs`.
pub struct LocalCapsuleBroker<S: CapsuleStore> {
    store: S,
    seal_key: CapsuleSealKey,
}

impl<S: CapsuleStore> LocalCapsuleBroker<S> {
    pub fn new(store: S, seal_key: CapsuleSealKey) -> Self {
        Self { store, seal_key }
    }

    pub fn seal_key(&self) -> &CapsuleSealKey {
        &self.seal_key
    }
}

impl<S: CapsuleStore> CapsuleBrokerClient for LocalCapsuleBroker<S> {
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
            .build_capsule(tenant, deployment, snapshot_ts, prewarm)?;
        Ok(SealedCapsule::new(capsule, &self.seal_key, context))
    }

    fn hydrate_point(
        &self,
        context: &SealContext,
        capsule: SealedCapsule,
        key: DocumentId,
    ) -> Result<SealedCapsule, BrokerError> {
        let mut capsule = capsule.into_capsule(&self.seal_key, context)?;
        let value = self.store.read_point(&key, capsule.ts)?;
        capsule.hydrate_point(key, value);
        Ok(SealedCapsule::new(capsule, &self.seal_key, context))
    }

    fn hydrate_prefix(
        &self,
        context: &SealContext,
        capsule: SealedCapsule,
        prefix: String,
        limit: usize,
    ) -> Result<SealedCapsule, BrokerError> {
        if limit == 0 {
            return Err(BrokerError::ZeroScanLimit);
        }
        let mut capsule = capsule.into_capsule(&self.seal_key, context)?;
        let (certificate, entries) = self.store.scan_prefix(&prefix, limit, capsule.ts)?;
        capsule.hydrate_range(certificate, entries);
        Ok(SealedCapsule::new(capsule, &self.seal_key, context))
    }

    fn mint_document_id(
        &self,
        _context: &SealContext,
        table: &str,
    ) -> Result<DocumentId, BrokerError> {
        self.store
            .mint_document_id(table)
            .map_err(BrokerError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aster_capsule::{doc_with_i64, MvccStore, Value};

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

    #[test]
    fn broker_hydrate_prefix_reseals_with_certificate_evidence() {
        use aster_capsule::{KeyInterval, ScanStop, WriteSet};

        let store = MvccStore::new();
        let tenant = TenantId::new("tenant-broker");
        let deployment = DeploymentId::new("dep-broker");
        store.seed(DocumentId::new("docs/a"), doc_with_i64("value", 1));
        store.seed(DocumentId::new("docs/b"), doc_with_i64("value", 2));
        let mut deletion = WriteSet::default();
        deletion.delete(DocumentId::new("docs/a"));
        store
            .commit(store.snapshot_ts(), &Default::default(), &deletion)
            .expect("tombstone commit");
        let ts = store.snapshot_ts();

        let broker = LocalCapsuleBroker::new(
            &store,
            CapsuleSealKey::derive_for_tests(b"broker-unit-test"),
        );
        let context = SealContext::new("cell-1", 9);
        let sealed = broker
            .initial_capsule(&context, tenant, deployment, ts, Vec::new())
            .expect("initial capsule");

        let sealed = broker
            .hydrate_prefix(&context, sealed, "docs/".to_string(), 1)
            .expect("hydrate prefix");
        sealed
            .verify(broker.seal_key(), &context)
            .expect("resealed capsule verifies");

        let capsule = sealed.capsule();
        assert_eq!(capsule.ranges.len(), 1);
        let certificate = &capsule.ranges[0];
        certificate.validate().expect("valid certificate");
        assert_eq!(certificate.interval, KeyInterval::Prefix("docs/".into()));
        // docs/a is tombstoned at ts — the single limit slot goes to docs/b.
        assert_eq!(certificate.keys, vec![DocumentId::new("docs/b")]);
        assert_eq!(certificate.stop, ScanStop::Boundary);
        let value = capsule
            .get(&DocumentId::new("docs/b"))
            .and_then(|doc| doc.document.as_ref())
            .and_then(|doc| doc.get("value"));
        assert_eq!(value, Some(&Value::Int(2)));

        // Repeated scans are legal and append in order.
        let sealed = broker
            .hydrate_prefix(&context, sealed, "docs/".to_string(), 5)
            .expect("second hydrate");
        assert_eq!(sealed.capsule().ranges.len(), 2);
        assert_eq!(sealed.capsule().ranges[1].stop, ScanStop::Exhausted);
    }

    /// Certificates are evidence about the capsule snapshot: a prefix
    /// hydrate on a capsule issued at `ts` must scan at `ts` even after
    /// the store head has advanced (trait contract on `hydrate_prefix`).
    /// A mutant that scans at `store.snapshot_ts()` leaks docs/b into the
    /// certificate and the capsule.
    #[test]
    fn broker_hydrate_prefix_scans_at_capsule_ts_not_store_head() {
        use aster_capsule::ScanStop;

        let store = MvccStore::new();
        let tenant = TenantId::new("tenant-broker");
        let deployment = DeploymentId::new("dep-broker");
        store.seed(DocumentId::new("docs/a"), doc_with_i64("value", 1));
        let ts = store.snapshot_ts();
        let broker = LocalCapsuleBroker::new(
            &store,
            CapsuleSealKey::derive_for_tests(b"broker-unit-test"),
        );
        let context = SealContext::new("cell-1", 9);
        let sealed = broker
            .initial_capsule(&context, tenant, deployment, ts, Vec::new())
            .expect("initial capsule");

        // Head advances past the capsule's snapshot.
        store.seed(DocumentId::new("docs/b"), doc_with_i64("value", 2));
        assert!(store.snapshot_ts() > ts, "head must advance");

        let sealed = broker
            .hydrate_prefix(&context, sealed, "docs/".to_string(), 10)
            .expect("hydrate prefix");
        let capsule = sealed.capsule();
        assert_eq!(capsule.ranges.len(), 1);
        let certificate = &capsule.ranges[0];
        assert_eq!(certificate.keys, vec![DocumentId::new("docs/a")]);
        // docs/b is invisible at ts, so the scan ran out of the prefix
        // without filling the limit.
        assert_eq!(certificate.stop, ScanStop::Exhausted);
        assert!(
            capsule.get(&DocumentId::new("docs/b")).is_none(),
            "post-snapshot key must not hydrate into the capsule"
        );
    }

    /// Point hydrates resolve at the capsule snapshot too: a second
    /// revision committed after issuance stays invisible — the capsule
    /// carries the ts-time version, never the head version.
    #[test]
    fn broker_hydrate_point_reads_at_capsule_ts_not_store_head() {
        let store = MvccStore::new();
        let tenant = TenantId::new("tenant-broker");
        let deployment = DeploymentId::new("dep-broker");
        let key = DocumentId::new("docs/1");
        store.seed(key.clone(), doc_with_i64("value", 1));
        let ts = store.snapshot_ts();
        let broker = LocalCapsuleBroker::new(
            &store,
            CapsuleSealKey::derive_for_tests(b"broker-unit-test"),
        );
        let context = SealContext::new("cell-1", 9);
        let sealed = broker
            .initial_capsule(&context, tenant, deployment, ts, Vec::new())
            .expect("initial capsule");

        // A second revision of the same key lands after issuance.
        store.seed(key.clone(), doc_with_i64("value", 99));

        let sealed = broker
            .hydrate_point(&context, sealed, key.clone())
            .expect("hydrate point");
        let hydrated = sealed.capsule().get(&key).expect("key hydrated");
        assert_eq!(
            hydrated.version,
            Some(ts),
            "capsule must carry the ts-time version, not the head version"
        );
        let value = hydrated.document.as_ref().and_then(|doc| doc.get("value"));
        assert_eq!(value, Some(&Value::Int(1)));
    }

    #[test]
    fn broker_rejects_wrong_cell_seal_on_hydrate_prefix() {
        let store = MvccStore::new();
        let tenant = TenantId::new("tenant-broker");
        let deployment = DeploymentId::new("dep-broker");
        store.seed(DocumentId::new("docs/1"), doc_with_i64("value", 5));
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
            broker.hydrate_prefix(&cell_b, sealed, "docs/".to_string(), 4),
            Err(BrokerError::Seal(SealError::WrongCell))
        );
    }

    #[test]
    fn broker_rejects_zero_limit_prefix_hydrate() {
        let store = MvccStore::new();
        let broker = LocalCapsuleBroker::new(
            &store,
            CapsuleSealKey::derive_for_tests(b"broker-unit-test"),
        );
        let context = SealContext::new("cell-1", 9);
        let sealed = broker
            .initial_capsule(
                &context,
                TenantId::new("tenant-broker"),
                DeploymentId::new("dep-broker"),
                store.snapshot_ts(),
                Vec::new(),
            )
            .expect("initial capsule");

        assert_eq!(
            broker.hydrate_prefix(&context, sealed, "docs/".to_string(), 0),
            Err(BrokerError::ZeroScanLimit)
        );
    }
}
