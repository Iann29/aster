//! Snapshot capsules: the production spine of Aster Runner.
//!
//! This file is deliberately small enough to read in one sitting, but it is
//! not a toy API. The production service would replace the in-memory MVCC map
//! with a Postgres/WAL-backed snapshot fabric and replace `DefaultHasher` with
//! BLAKE3 or SHA-256. The shapes are the important part: a runner receives a
//! bounded, immutable, tenant-scoped capsule; all missing data is requested via
//! explicit read traps; all writes are committed by a single writer that checks
//! the read set against the live MVCC state.
//!
//! The main design intent is capability separation. Aster runners never need
//! database credentials. They do not get an open-ended "read from the DB" RPC.
//! They get a cryptographically named capsule and may ask the capsule broker to
//! hydrate particular missing keys for the same tenant/deployment/snapshot.
//! v0.2 keeps the cheap `root_hash` for the legacy microbenchmark but adds
//! `seal::SealedCapsule`, a BLAKE3 keyed capability seal suitable for the
//! broker/cell boundary.

pub mod seal;

pub use seal::{capsule_digest, CapsuleSeal, CapsuleSealKey, SealContext, SealError, SealedCapsule};

use std::collections::{BTreeMap, BTreeSet};
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

/// Convex deployments belong to tenants in a self-hosted control plane.
///
/// The type is a newtype instead of a bare string so APIs cannot accidentally
/// pass a deployment name where a tenant boundary is required. In production
/// this identifier would be minted by Synapse or another OSS control plane.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TenantId(pub String);

impl TenantId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// A single Convex deployment under a tenant.
///
/// The lease-backed Convex backend remains authoritative for commits. Aster's
/// deployment identifier is used to make capsules unforgeably narrow: a capsule
/// for deployment A is never accepted by a runner cell pinned to deployment B.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DeploymentId(pub String);

impl DeploymentId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// Prototype timestamp. Convex's real timestamp type is richer, but the
/// relevant property here is a monotonically increasing MVCC version.
pub type Timestamp = u64;

/// Prototype document key.
///
/// A production adapter maps Convex's internal document/table IDs and index
/// ranges into this key space. The core machinery only requires total ordering
/// so capsules can be hashed deterministically.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DocumentId(pub String);

impl DocumentId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// Minimal Convex-like value domain for the prototype.
///
/// Keeping the value domain tiny lets the example focus on the distributed
/// systems idea: snapshot shipping, read traps, and OCC. Production code would
/// use Convex's existing `ConvexValue` serialization.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Value {
    Int(i64),
    Text(String),
    Bool(bool),
    Null,
}

impl Value {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(value) => Some(*value),
            _ => None,
        }
    }
}

/// A document is an ordered map so hashing is deterministic.
pub type Document = BTreeMap<String, Value>;

/// A document plus the exact MVCC version visible in a capsule.
///
/// `document == None` is a tombstone/nonexistent read. The version remains
/// explicit because reading absence is part of OCC: if a document appears after
/// the snapshot began, a mutation that depended on absence must conflict.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct VersionedDocument {
    pub version: Option<Timestamp>,
    pub document: Option<Document>,
}

impl VersionedDocument {
    pub fn missing() -> Self {
        Self {
            version: None,
            document: None,
        }
    }
}

/// A point-read set with observed versions.
///
/// The committer validates this structure against the live MVCC state. This is
/// the same logical contract Convex relies on: functions execute against a
/// consistent snapshot; the single writer rejects a mutation when a read changed
/// before commit.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReadSet {
    points: BTreeMap<DocumentId, Option<Timestamp>>,
}

impl ReadSet {
    pub fn observe(&mut self, key: DocumentId, version: Option<Timestamp>) {
        self.points.entry(key).or_insert(version);
    }

    pub fn contains(&self, key: &DocumentId) -> bool {
        self.points.contains_key(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&DocumentId, &Option<Timestamp>)> {
        self.points.iter()
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
}

/// A mutation write set. Queries and actions return an empty write set.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WriteSet {
    writes: BTreeMap<DocumentId, Option<Document>>,
}

impl WriteSet {
    pub fn put(&mut self, key: DocumentId, document: Document) {
        self.writes.insert(key, Some(document));
    }

    pub fn delete(&mut self, key: DocumentId) {
        self.writes.insert(key, None);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&DocumentId, &Option<Document>)> {
        self.writes.iter()
    }

    pub fn len(&self) -> usize {
        self.writes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }
}

/// A read trap is a structured continuation request, not a database query.
///
/// The runner says "I cannot continue this function until the capsule contains
/// this key/range at the same snapshot timestamp." The broker may hydrate the
/// capsule, reject it as stale, or route the invocation to a better-prewarmed
/// cell. The runner itself still never receives general database authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadTrap {
    Point(DocumentId),
    Prefix { prefix: String, limit: usize },
}

/// The immutable capability passed into a sandbox cell.
///
/// `root_hash` is recomputed whenever the broker hydrates more data. A worker
/// can include it in every result so the host can audit exactly which snapshot
/// bytes influenced the function outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotCapsule {
    pub tenant: TenantId,
    pub deployment: DeploymentId,
    pub ts: Timestamp,
    pub docs: BTreeMap<DocumentId, VersionedDocument>,
    pub root_hash: u64,
}

impl SnapshotCapsule {
    pub fn empty(tenant: TenantId, deployment: DeploymentId, ts: Timestamp) -> Self {
        let mut capsule = Self {
            tenant,
            deployment,
            ts,
            docs: BTreeMap::new(),
            root_hash: 0,
        };
        capsule.root_hash = capsule.compute_root_hash();
        capsule
    }

    pub fn contains(&self, key: &DocumentId) -> bool {
        self.docs.contains_key(key)
    }

    pub fn get(&self, key: &DocumentId) -> Option<&VersionedDocument> {
        self.docs.get(key)
    }

    pub fn hydrate_point(&mut self, key: DocumentId, value: VersionedDocument) {
        self.docs.insert(key, value);
        self.root_hash = self.compute_root_hash();
    }

    pub fn compute_root_hash(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.tenant.hash(&mut hasher);
        self.deployment.hash(&mut hasher);
        self.ts.hash(&mut hasher);
        self.docs.hash(&mut hasher);
        hasher.finish()
    }
}

#[derive(Clone, Debug)]
struct Revision {
    ts: Timestamp,
    document: Option<Document>,
}

#[derive(Debug, Default)]
struct MvccInner {
    now: Timestamp,
    docs: BTreeMap<DocumentId, Vec<Revision>>,
}

/// In-memory MVCC store plus single-writer commit lock.
///
/// This intentionally mirrors Convex's correctness boundary rather than trying
/// to replace it. The production Aster adapter would return read/write sets to
/// Convex's backend committer. The prototype stores them here so tests can prove
/// that two concurrent mutation results cannot both commit over the same read.
#[derive(Debug, Default)]
pub struct MvccStore {
    inner: Mutex<MvccInner>,
}

impl MvccStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed(&self, key: DocumentId, document: Document) -> Timestamp {
        let mut inner = self.inner.lock().expect("mvcc mutex poisoned");
        inner.now += 1;
        let ts = inner.now;
        inner.docs.entry(key).or_default().push(Revision {
            ts,
            document: Some(document),
        });
        ts
    }

    pub fn snapshot_ts(&self) -> Timestamp {
        self.inner.lock().expect("mvcc mutex poisoned").now
    }

    pub fn read_at(&self, key: &DocumentId, ts: Timestamp) -> VersionedDocument {
        let inner = self.inner.lock().expect("mvcc mutex poisoned");
        Self::read_at_inner(&inner, key, ts)
    }

    pub fn prefix_at(&self, prefix: &str, limit: usize, ts: Timestamp) -> Vec<(DocumentId, VersionedDocument)> {
        let inner = self.inner.lock().expect("mvcc mutex poisoned");
        inner
            .docs
            .keys()
            .filter(|key| key.0.starts_with(prefix))
            .take(limit)
            .cloned()
            .map(|key| {
                let value = Self::read_at_inner(&inner, &key, ts);
                (key, value)
            })
            .collect()
    }

    pub fn build_capsule(
        &self,
        tenant: TenantId,
        deployment: DeploymentId,
        ts: Timestamp,
        keys: impl IntoIterator<Item = DocumentId>,
    ) -> SnapshotCapsule {
        let mut capsule = SnapshotCapsule::empty(tenant, deployment, ts);
        for key in keys {
            let value = self.read_at(&key, ts);
            capsule.hydrate_point(key, value);
        }
        capsule
    }

    pub fn commit(
        &self,
        base_ts: Timestamp,
        read_set: &ReadSet,
        write_set: &WriteSet,
    ) -> Result<Timestamp, CommitError> {
        if write_set.is_empty() {
            return Ok(base_ts);
        }
        let mut inner = self.inner.lock().expect("mvcc mutex poisoned");
        for (key, observed) in read_set.iter() {
            let live = Self::read_at_inner(&inner, key, inner.now);
            if &live.version != observed {
                return Err(CommitError::Conflict {
                    key: key.clone(),
                    observed: *observed,
                    live: live.version,
                });
            }
        }
        inner.now += 1;
        let commit_ts = inner.now;
        for (key, value) in write_set.iter() {
            inner.docs.entry(key.clone()).or_default().push(Revision {
                ts: commit_ts,
                document: value.clone(),
            });
        }
        Ok(commit_ts)
    }

    fn read_at_inner(inner: &MvccInner, key: &DocumentId, ts: Timestamp) -> VersionedDocument {
        let Some(revisions) = inner.docs.get(key) else {
            return VersionedDocument::missing();
        };
        revisions
            .iter()
            .rev()
            .find(|revision| revision.ts <= ts)
            .map(|revision| VersionedDocument {
                version: Some(revision.ts),
                document: revision.document.clone(),
            })
            .unwrap_or_else(VersionedDocument::missing)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitError {
    Conflict {
        key: DocumentId,
        observed: Option<Timestamp>,
        live: Option<Timestamp>,
    },
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommitError::Conflict { key, observed, live } => write!(
                f,
                "OCC conflict on {:?}: observed {:?}, live {:?}",
                key, observed, live
            ),
        }
    }
}

impl std::error::Error for CommitError {}

/// Helper used by tests and examples.
pub fn doc_with_i64(field: &str, value: i64) -> Document {
    let mut document = Document::new();
    document.insert(field.to_string(), Value::Int(value));
    document
}

/// Return a deterministic set of keys for capsule prewarming.
pub fn key_set(keys: &[&str]) -> BTreeSet<DocumentId> {
    keys.iter().map(|key| DocumentId::new(*key)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_rejects_changed_read() {
        let store = MvccStore::new();
        let key = DocumentId::new("counters/main");
        store.seed(key.clone(), doc_with_i64("value", 1));
        let base_ts = store.snapshot_ts();
        let value = store.read_at(&key, base_ts);

        let mut read_set = ReadSet::default();
        read_set.observe(key.clone(), value.version);

        let mut first = WriteSet::default();
        first.put(key.clone(), doc_with_i64("value", 2));
        assert!(store.commit(base_ts, &read_set, &first).is_ok());

        let mut second = WriteSet::default();
        second.put(key, doc_with_i64("value", 3));
        assert!(matches!(
            store.commit(base_ts, &read_set, &second),
            Err(CommitError::Conflict { .. })
        ));
    }
}
