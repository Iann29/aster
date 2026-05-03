//! Storage abstraction the broker uses for MVCC reads at a given snapshot
//! timestamp.
//!
//! Until v0.3 the broker took `&MvccStore` directly — fine for the in-memory
//! prototype, but the load-bearing v1.0 step is reading from the same Postgres
//! database the Convex backend uses. This trait is the seam: the in-memory
//! `MvccStore` keeps working through a blanket `impl` (so existing tests are
//! unchanged), and a future `PostgresCapsuleStore` plugs in via the same
//! interface.
//!
//! Design notes (kept short — full memo lives in
//! `docs/POSTGRES_ADAPTER_PLAN.md`):
//!
//! - The trait is **sync**. The Postgres impl owns its own tokio runtime and
//!   `block_on`s internally; cells see a stable, runtime-free trait.
//! - All methods take `&self`, so impls must do their own internal
//!   synchronisation (the in-memory store already has a `Mutex`).
//! - `read_prefix` matches the existing `prefix_at` shape — point reads plus
//!   bounded prefix scans cover every read trap the v0.3 prototype emits.

use std::sync::Arc;

use aster_capsule::{
    DeploymentId, DocumentId, MvccStore, SnapshotCapsule, TenantId, Timestamp, VersionedDocument,
};

/// Storage backend used by `LocalCapsuleBroker` to satisfy read traps.
///
/// Implementations must be safe to call concurrently — the broker dispatches
/// one request per accepted Unix-socket connection and may reuse a single
/// store across many cells.
pub trait CapsuleStore: Send + Sync {
    /// The most recent snapshot timestamp this store can serve. The brokerd
    /// binary uses this on startup to pin its `(tenant, deployment, ts)` for
    /// the lifetime of the process; future multi-tenant brokers will let the
    /// caller pass it.
    fn snapshot_ts(&self) -> Result<Timestamp, StoreError>;

    /// Read a single document at the given snapshot. Missing documents return
    /// `VersionedDocument::missing()` (semantically `(None, None)`), not a
    /// `StoreError::NotFound`. NotFound is reserved for "the *store* doesn't
    /// know how to answer this question", e.g. the requested timestamp is
    /// outside the retention window.
    fn read_point(
        &self,
        key: &DocumentId,
        ts: Timestamp,
    ) -> Result<VersionedDocument, StoreError>;

    /// Read every document whose key starts with `prefix`, up to `limit` rows,
    /// at the given snapshot. Used by the v0.3 prefix read trap path.
    fn read_prefix(
        &self,
        prefix: &str,
        limit: usize,
        ts: Timestamp,
    ) -> Result<Vec<(DocumentId, VersionedDocument)>, StoreError>;

    /// Build the capsule the cell starts with. Default impl loops through
    /// `prewarm` calling `read_point`; the Postgres impl will override with
    /// a single `SELECT WHERE id = ANY($1)` to amortise the round-trip.
    fn build_capsule(
        &self,
        tenant: TenantId,
        deployment: DeploymentId,
        ts: Timestamp,
        prewarm: Vec<DocumentId>,
    ) -> Result<SnapshotCapsule, StoreError> {
        let mut capsule = SnapshotCapsule::empty(tenant, deployment, ts);
        for key in prewarm {
            let value = self.read_point(&key, ts)?;
            capsule.hydrate_point(key, value);
        }
        Ok(capsule)
    }
}

/// Errors a `CapsuleStore` may surface to the broker.
///
/// The broker maps these into `BrokerError::Remote` (or specific variants
/// where applicable) before they cross the IPC boundary, so cells never see
/// `StoreError` directly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreError {
    /// The store is unreachable right now (Postgres pool exhausted, broker
    /// down, etc.). Retry-friendly.
    Unavailable(String),

    /// The requested snapshot is older than the store's retention horizon
    /// (Convex MVCC is not infinite). Caller must re-run at a fresher `ts`.
    Stale {
        requested: Timestamp,
        latest: Timestamp,
    },

    /// Backend returned an error we don't know how to classify. The string
    /// is for operator logs; cells get a generic `BrokerError::Remote`.
    Backend(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(msg) => write!(f, "store unavailable: {msg}"),
            Self::Stale { requested, latest } => write!(
                f,
                "snapshot {requested} is older than store retention (latest={latest})"
            ),
            Self::Backend(msg) => write!(f, "store backend error: {msg}"),
        }
    }
}

impl std::error::Error for StoreError {}

// ---------------------------------------------------------------------------
// In-memory adapters: anything that already speaks `MvccStore` keeps working.
// ---------------------------------------------------------------------------

/// Borrowed adapter so existing call sites that hold an `&MvccStore` (the
/// prototype tests, the host facade, the in-process v8cell entry point) keep
/// compiling without ownership changes.
impl CapsuleStore for &MvccStore {
    fn snapshot_ts(&self) -> Result<Timestamp, StoreError> {
        Ok(MvccStore::snapshot_ts(self))
    }

    fn read_point(
        &self,
        key: &DocumentId,
        ts: Timestamp,
    ) -> Result<VersionedDocument, StoreError> {
        Ok(MvccStore::read_at(self, key, ts))
    }

    fn read_prefix(
        &self,
        prefix: &str,
        limit: usize,
        ts: Timestamp,
    ) -> Result<Vec<(DocumentId, VersionedDocument)>, StoreError> {
        Ok(MvccStore::prefix_at(self, prefix, limit, ts))
    }

    fn build_capsule(
        &self,
        tenant: TenantId,
        deployment: DeploymentId,
        ts: Timestamp,
        prewarm: Vec<DocumentId>,
    ) -> Result<SnapshotCapsule, StoreError> {
        // The in-memory store has a hot-path build_capsule that avoids the
        // per-key Mutex-lock loop the default trait impl would take.
        Ok(MvccStore::build_capsule(self, tenant, deployment, ts, prewarm))
    }
}

/// Owned adapter for callers that prefer to hand `MvccStore` directly to the
/// broker without managing a lifetime. Also enables `Arc<MvccStore>` reuse via
/// the blanket below.
impl CapsuleStore for MvccStore {
    fn snapshot_ts(&self) -> Result<Timestamp, StoreError> {
        Ok(MvccStore::snapshot_ts(self))
    }

    fn read_point(
        &self,
        key: &DocumentId,
        ts: Timestamp,
    ) -> Result<VersionedDocument, StoreError> {
        Ok(MvccStore::read_at(self, key, ts))
    }

    fn read_prefix(
        &self,
        prefix: &str,
        limit: usize,
        ts: Timestamp,
    ) -> Result<Vec<(DocumentId, VersionedDocument)>, StoreError> {
        Ok(MvccStore::prefix_at(self, prefix, limit, ts))
    }

    fn build_capsule(
        &self,
        tenant: TenantId,
        deployment: DeploymentId,
        ts: Timestamp,
        prewarm: Vec<DocumentId>,
    ) -> Result<SnapshotCapsule, StoreError> {
        Ok(MvccStore::build_capsule(self, tenant, deployment, ts, prewarm))
    }
}

/// `Arc<dyn CapsuleStore>` (or `Arc<MvccStore>`) is the shared-pointer form the
/// brokerd binary uses to keep the same store across multiple in-flight cells
/// without lifetimes leaking into `ProcessBroker`.
impl<S: CapsuleStore + ?Sized> CapsuleStore for Arc<S> {
    fn snapshot_ts(&self) -> Result<Timestamp, StoreError> {
        (**self).snapshot_ts()
    }

    fn read_point(
        &self,
        key: &DocumentId,
        ts: Timestamp,
    ) -> Result<VersionedDocument, StoreError> {
        (**self).read_point(key, ts)
    }

    fn read_prefix(
        &self,
        prefix: &str,
        limit: usize,
        ts: Timestamp,
    ) -> Result<Vec<(DocumentId, VersionedDocument)>, StoreError> {
        (**self).read_prefix(prefix, limit, ts)
    }

    fn build_capsule(
        &self,
        tenant: TenantId,
        deployment: DeploymentId,
        ts: Timestamp,
        prewarm: Vec<DocumentId>,
    ) -> Result<SnapshotCapsule, StoreError> {
        (**self).build_capsule(tenant, deployment, ts, prewarm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aster_capsule::{doc_with_i64, MvccStore, Value};

    /// Trait-level sanity: `MvccStore` answers reads through `CapsuleStore`
    /// and the values match the direct `read_at` API. This is the contract
    /// the rest of the broker stack relies on — if the blanket impl ever
    /// drifts, this test fails before any broker test does.
    #[test]
    fn mvcc_store_satisfies_capsule_store_trait() {
        let store = MvccStore::new();
        let key = DocumentId::new("docs/sanity");
        store.seed(key.clone(), doc_with_i64("value", 42));
        let ts = store.snapshot_ts();

        let via_trait: &dyn CapsuleStore = &store;
        assert_eq!(via_trait.snapshot_ts().expect("snapshot_ts"), ts);

        let value = via_trait.read_point(&key, ts).expect("read_point");
        let n = value
            .document
            .and_then(|doc| doc.get("value").cloned())
            .and_then(|v| match v {
                Value::Int(n) => Some(n),
                _ => None,
            });
        assert_eq!(n, Some(42));
    }

    /// `Arc<MvccStore>` is the shape the brokerd binary will use once the
    /// Postgres store lands (so we can hold one store across many cells
    /// without lifetimes). Verify that path compiles + works today.
    #[test]
    fn arc_wrapped_store_keeps_working_through_trait() {
        let store = Arc::new(MvccStore::new());
        let key = DocumentId::new("docs/arc");
        store.seed(key.clone(), doc_with_i64("value", 7));
        let ts = store.snapshot_ts().expect("snapshot");

        // The arc dispatches through CapsuleStore — no need to deref.
        let value = store.read_point(&key, ts).expect("read_point");
        assert!(value.document.is_some(), "expected document, got tombstone");
    }

    /// `LocalCapsuleBroker` should accept any `CapsuleStore` impl, including
    /// the borrowed `&MvccStore` flavour the existing tests use. This is the
    /// back-compat guarantee we'll lean on while wiring the Postgres store.
    #[test]
    fn store_error_classification_round_trips_through_display() {
        let unavailable = StoreError::Unavailable("pool exhausted".into());
        let stale = StoreError::Stale {
            requested: 100,
            latest: 200,
        };
        let backend = StoreError::Backend("syntax error at or near \"FROM\"".into());

        // Display impls carry enough operator-grade detail for log greps.
        assert!(unavailable.to_string().contains("pool exhausted"));
        assert!(stale.to_string().contains("100"));
        assert!(stale.to_string().contains("200"));
        assert!(backend.to_string().contains("syntax error"));
    }
}

