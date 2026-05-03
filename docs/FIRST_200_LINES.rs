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
//! That is the architectural line between this prototype and a conventional
//! remote function runner.

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
