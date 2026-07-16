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

pub mod canon;
pub mod seal;

pub use canon::{decode_capsule_bytes, encode_capsule_bytes, DecodeError};
pub use seal::{
    capsule_digest, CapsuleSeal, CapsuleSealKey, SealContext, SealError, SealedCapsule,
    SessionBinding,
};

use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

/// Convex deployments belong to tenants in a self-hosted control plane.
///
/// The type is a newtype instead of a bare string so APIs cannot accidentally
/// pass a deployment name where a tenant boundary is required. In production
/// this identifier would be minted by Synapse or another OSS control plane.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReadTrap {
    Point(DocumentId),
    Prefix { prefix: String, limit: usize },
}

/// One endpoint of a general key range.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum IntervalEndpoint {
    Unbounded,
    Inclusive(DocumentId),
    Exclusive(DocumentId),
}

/// The key interval `I` a limited scan observed.
///
/// `Prefix` is kept as its own semantic form (not desugared to a `Range`)
/// because computing a prefix's exclusive upper bound requires byte-level
/// successor surgery that can leave UTF-8 — binding "all keys starting with
/// p" directly is exact and decidable.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum KeyInterval {
    Prefix(String),
    Range {
        start: IntervalEndpoint,
        end: IntervalEndpoint,
    },
}

impl KeyInterval {
    pub fn contains(&self, key: &DocumentId) -> bool {
        match self {
            Self::Prefix(prefix) => key.0.starts_with(prefix.as_str()),
            Self::Range { start, end } => {
                let after_start = match start {
                    IntervalEndpoint::Unbounded => true,
                    IntervalEndpoint::Inclusive(bound) => key >= bound,
                    IntervalEndpoint::Exclusive(bound) => key > bound,
                };
                let before_end = match end {
                    IntervalEndpoint::Unbounded => true,
                    IntervalEndpoint::Inclusive(bound) => key <= bound,
                    IntervalEndpoint::Exclusive(bound) => key < bound,
                };
                after_start && before_end
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum ScanDirection {
    Ascending,
    Descending,
}

/// Why the scan stopped. `Boundary` means the limit was hit at the last
/// returned key; `Exhausted` means the whole interval had no further live
/// key — the suffix after the last key is an observed exhaustion gap.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum ScanStop {
    Exhausted,
    Boundary,
}

/// Sealed evidence of one limited scan (Capsule Transaction Theorem,
/// Definition 1.1 + Repair R-RANGE): interval semantics, direction, positive
/// limit, the ordered returned keys, and the exhausted-versus-boundary stop.
/// This is the minimal certificate from which the phantom-safe conflict
/// window can be reconstructed at commit time.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct RangeCertificate {
    pub interval: KeyInterval,
    pub direction: ScanDirection,
    pub limit: u64,
    pub keys: Vec<DocumentId>,
    pub stop: ScanStop,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RangeCertificateError {
    ZeroLimit,
    TooManyKeys,
    KeysNotMonotonic,
    KeyOutsideInterval(DocumentId),
    StopInconsistent,
}

impl fmt::Display for RangeCertificateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroLimit => write!(f, "range certificate limit must be >= 1"),
            Self::TooManyKeys => write!(f, "range certificate returned more keys than its limit"),
            Self::KeysNotMonotonic => {
                write!(f, "range certificate keys are not strictly ordered")
            }
            Self::KeyOutsideInterval(key) => {
                write!(f, "range certificate key {:?} is outside its interval", key.0)
            }
            Self::StopInconsistent => write!(
                f,
                "range certificate stop variant does not match key count vs limit"
            ),
        }
    }
}

impl std::error::Error for RangeCertificateError {}

impl RangeCertificate {
    pub fn validate(&self) -> Result<(), RangeCertificateError> {
        if self.limit == 0 {
            return Err(RangeCertificateError::ZeroLimit);
        }
        if self.keys.len() as u64 > self.limit {
            return Err(RangeCertificateError::TooManyKeys);
        }
        for pair in self.keys.windows(2) {
            let ordered = match self.direction {
                ScanDirection::Ascending => pair[0] < pair[1],
                ScanDirection::Descending => pair[0] > pair[1],
            };
            if !ordered {
                return Err(RangeCertificateError::KeysNotMonotonic);
            }
        }
        for key in &self.keys {
            if !self.interval.contains(key) {
                return Err(RangeCertificateError::KeyOutsideInterval(key.clone()));
            }
        }
        let hit_limit = self.keys.len() as u64 == self.limit;
        let is_boundary = matches!(self.stop, ScanStop::Boundary);
        if hit_limit != is_boundary {
            return Err(RangeCertificateError::StopInconsistent);
        }
        Ok(())
    }

    /// The observed window the committer must check for conflicting writes
    /// (Theorem, Definition 1.1): the whole interval when the scan exhausted
    /// it, and the prefix through the last returned key when the scan
    /// stopped at its limit. A write strictly past the boundary cannot
    /// change the first-ℓ answer, so it is deliberately outside the window.
    pub fn window(&self) -> ObservedWindow {
        let boundary = match self.stop {
            ScanStop::Exhausted => None,
            ScanStop::Boundary => self.keys.last().cloned(),
        };
        ObservedWindow {
            interval: self.interval.clone(),
            direction: self.direction,
            boundary,
        }
    }
}

/// A validated conflict window: `contains` answers "could a write to this
/// key invalidate the certified scan result?".
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedWindow {
    pub interval: KeyInterval,
    pub direction: ScanDirection,
    pub boundary: Option<DocumentId>,
}

impl ObservedWindow {
    pub fn contains(&self, key: &DocumentId) -> bool {
        if !self.interval.contains(key) {
            return false;
        }
        match (&self.boundary, self.direction) {
            (None, _) => true,
            (Some(bound), ScanDirection::Ascending) => key <= bound,
            (Some(bound), ScanDirection::Descending) => key >= bound,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapsuleStructureError {
    Range {
        index: usize,
        error: RangeCertificateError,
    },
    RangeKeyNotInDocs { index: usize, key: DocumentId },
    RangeKeyNotLive { index: usize, key: DocumentId },
}

impl fmt::Display for CapsuleStructureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Range { index, error } => {
                write!(f, "range certificate {index} is invalid: {error}")
            }
            Self::RangeKeyNotInDocs { index, key } => write!(
                f,
                "range certificate {index} returned key {:?} missing from docs",
                key.0
            ),
            Self::RangeKeyNotLive { index, key } => write!(
                f,
                "range certificate {index} returned key {:?} that is not a live document",
                key.0
            ),
        }
    }
}

impl std::error::Error for CapsuleStructureError {}

/// The immutable capability passed into a sandbox cell.
///
/// `root_hash` is recomputed whenever the broker hydrates more data. A worker
/// can include it in every result so the host can audit exactly which snapshot
/// bytes influenced the function outcome.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotCapsule {
    pub tenant: TenantId,
    pub deployment: DeploymentId,
    pub ts: Timestamp,
    pub docs: BTreeMap<DocumentId, VersionedDocument>,
    /// Ordered sequence of limited-scan observations. Position is part of
    /// the canonical content; repeated scans are legal.
    #[serde(default)]
    pub ranges: Vec<RangeCertificate>,
    pub root_hash: u64,
}

impl SnapshotCapsule {
    pub fn empty(tenant: TenantId, deployment: DeploymentId, ts: Timestamp) -> Self {
        let mut capsule = Self {
            tenant,
            deployment,
            ts,
            docs: BTreeMap::new(),
            ranges: Vec::new(),
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

    /// Merge one certified scan: every returned entry lands in `docs` and the
    /// certificate is appended to the ordered `ranges` sequence.
    pub fn hydrate_range(
        &mut self,
        certificate: RangeCertificate,
        entries: Vec<(DocumentId, VersionedDocument)>,
    ) {
        for (key, value) in entries {
            self.docs.insert(key, value);
        }
        self.ranges.push(certificate);
        self.root_hash = self.compute_root_hash();
    }

    /// Structural validity of the observation set (W-CANON for ranges):
    /// every certificate is internally valid and every returned key
    /// cross-references a live entry in the document map. Issued capsules
    /// satisfy this by construction; verification rejects anything else
    /// before it can reach conflict validation.
    pub fn validate_structure(&self) -> Result<(), CapsuleStructureError> {
        for (index, certificate) in self.ranges.iter().enumerate() {
            certificate
                .validate()
                .map_err(|error| CapsuleStructureError::Range { index, error })?;
            for key in &certificate.keys {
                match self.docs.get(key) {
                    None => {
                        return Err(CapsuleStructureError::RangeKeyNotInDocs {
                            index,
                            key: key.clone(),
                        })
                    }
                    Some(value) if value.version.is_none() || value.document.is_none() => {
                        return Err(CapsuleStructureError::RangeKeyNotLive {
                            index,
                            key: key.clone(),
                        })
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(())
    }

    pub fn compute_root_hash(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.tenant.hash(&mut hasher);
        self.deployment.hash(&mut hasher);
        self.ts.hash(&mut hasher);
        self.docs.hash(&mut hasher);
        self.ranges.hash(&mut hasher);
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

    /// Certified ascending prefix scan at snapshot `ts` — the theorem's
    /// `Scan_σs(I, ℓ)` for `I = Prefix(prefix)` (Definition 1.1).
    ///
    /// Visibility is the latest revision with `rev.ts <= ts`. Keys whose
    /// visible state is a tombstone or that do not exist yet at `ts` are
    /// skipped and do NOT consume limit slots; keys created after `ts` are
    /// invisible. The scan collects until `limit` live entries:
    /// `stop == Boundary` iff exactly `limit` were collected, `Exhausted`
    /// otherwise (`RangeCertificate::validate` couples the two by design).
    ///
    /// Referee finding F2 — completeness is a CALLER contract: a `Boundary`
    /// certificate only proves the first-`limit` answer, so a caller that
    /// wants to assert "this is the whole prefix" must request `limit + 1`
    /// (or otherwise check for an `Exhausted` stop) instead of treating a
    /// full-limit result as complete.
    ///
    /// `limit == 0` cannot produce a valid certificate and is rejected with
    /// `RangeCertificateError::ZeroLimit` before scanning.
    pub fn scan_prefix_at(
        &self,
        prefix: &str,
        limit: usize,
        ts: Timestamp,
    ) -> Result<(RangeCertificate, Vec<(DocumentId, VersionedDocument)>), RangeCertificateError>
    {
        if limit == 0 {
            return Err(RangeCertificateError::ZeroLimit);
        }
        let inner = self.inner.lock().expect("mvcc mutex poisoned");
        let mut entries = Vec::new();
        // Keys sharing a prefix form one contiguous BTreeMap run starting
        // at the prefix itself.
        for key in inner.docs.range(DocumentId::new(prefix)..).map(|(k, _)| k) {
            if !key.0.starts_with(prefix) {
                break;
            }
            // Reuse the point-read visibility rule so scan and point can
            // never disagree about what a snapshot contains.
            let value = Self::read_at_inner(&inner, key, ts);
            if value.version.is_none() || value.document.is_none() {
                continue;
            }
            entries.push((key.clone(), value));
            if entries.len() == limit {
                break;
            }
        }
        let stop = if entries.len() == limit {
            ScanStop::Boundary
        } else {
            ScanStop::Exhausted
        };
        let certificate = RangeCertificate {
            interval: KeyInterval::Prefix(prefix.to_string()),
            direction: ScanDirection::Ascending,
            limit: limit as u64,
            keys: entries.iter().map(|(key, _)| key.clone()).collect(),
            stop,
        };
        Ok((certificate, entries))
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
            CommitError::Conflict {
                key,
                observed,
                live,
            } => write!(
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

    fn live_doc(version: Timestamp, value: i64) -> VersionedDocument {
        VersionedDocument {
            version: Some(version),
            document: Some(doc_with_i64("value", value)),
        }
    }

    fn asc_cert(
        interval: KeyInterval,
        limit: u64,
        keys: &[&str],
        stop: ScanStop,
    ) -> RangeCertificate {
        RangeCertificate {
            interval,
            direction: ScanDirection::Ascending,
            limit,
            keys: keys.iter().map(|key| DocumentId::new(*key)).collect(),
            stop,
        }
    }

    #[test]
    fn boundary_window_covers_prefix_through_last_key_only() {
        let cert = asc_cert(
            KeyInterval::Prefix("docs/".into()),
            3,
            &["docs/a", "docs/b", "docs/c"],
            ScanStop::Boundary,
        );
        cert.validate().expect("valid certificate");
        let window = cert.window();
        // Inserts between returned keys are phantoms and must be caught.
        assert!(window.contains(&DocumentId::new("docs/a")));
        assert!(window.contains(&DocumentId::new("docs/b2")));
        assert!(window.contains(&DocumentId::new("docs/c")));
        // Referee finding F2: a full-limit scan observes only the prefix
        // through its last key. Writes strictly past the boundary cannot
        // change the first-ℓ answer and are deliberately outside the window.
        assert!(!window.contains(&DocumentId::new("docs/d")));
        assert!(!window.contains(&DocumentId::new("other/x")));
    }

    #[test]
    fn exhausted_window_covers_the_entire_interval() {
        let cert = asc_cert(
            KeyInterval::Prefix("docs/".into()),
            7,
            &["docs/a"],
            ScanStop::Exhausted,
        );
        cert.validate().expect("valid certificate");
        let window = cert.window();
        // The exhaustion gap: an insert anywhere in the interval — even far
        // past the last returned key — would add a live key the scan proved
        // absent, so the whole interval is the conflict window.
        assert!(window.contains(&DocumentId::new("docs/zzz")));
        assert!(!window.contains(&DocumentId::new("other/x")));
    }

    #[test]
    fn descending_boundary_window_mirrors_ascending() {
        let cert = RangeCertificate {
            interval: KeyInterval::Prefix("docs/".into()),
            direction: ScanDirection::Descending,
            limit: 2,
            keys: vec![DocumentId::new("docs/c"), DocumentId::new("docs/b")],
            stop: ScanStop::Boundary,
        };
        cert.validate().expect("valid certificate");
        let window = cert.window();
        assert!(window.contains(&DocumentId::new("docs/b")));
        assert!(window.contains(&DocumentId::new("docs/bb")));
        assert!(!window.contains(&DocumentId::new("docs/a")));
    }

    #[test]
    fn range_interval_bounds_are_respected() {
        let interval = KeyInterval::Range {
            start: IntervalEndpoint::Inclusive(DocumentId::new("docs/b")),
            end: IntervalEndpoint::Exclusive(DocumentId::new("docs/e")),
        };
        assert!(!interval.contains(&DocumentId::new("docs/a")));
        assert!(interval.contains(&DocumentId::new("docs/b")));
        assert!(interval.contains(&DocumentId::new("docs/d")));
        assert!(!interval.contains(&DocumentId::new("docs/e")));
    }

    #[test]
    fn certificate_validation_rejects_malformed_shapes() {
        let prefix = || KeyInterval::Prefix("docs/".into());
        assert_eq!(
            asc_cert(prefix(), 0, &[], ScanStop::Exhausted).validate(),
            Err(RangeCertificateError::ZeroLimit)
        );
        assert_eq!(
            asc_cert(prefix(), 1, &["docs/a", "docs/b"], ScanStop::Boundary).validate(),
            Err(RangeCertificateError::TooManyKeys)
        );
        assert_eq!(
            asc_cert(prefix(), 3, &["docs/b", "docs/a"], ScanStop::Exhausted).validate(),
            Err(RangeCertificateError::KeysNotMonotonic)
        );
        assert_eq!(
            asc_cert(prefix(), 3, &["docs/a", "docs/a"], ScanStop::Exhausted).validate(),
            Err(RangeCertificateError::KeysNotMonotonic)
        );
        assert_eq!(
            asc_cert(prefix(), 3, &["other/x"], ScanStop::Exhausted).validate(),
            Err(RangeCertificateError::KeyOutsideInterval(DocumentId::new(
                "other/x"
            )))
        );
        // The stop variant is derivable from |keys| vs limit; carrying an
        // inconsistent explicit tag is a forged certificate shape.
        assert_eq!(
            asc_cert(prefix(), 3, &["docs/a"], ScanStop::Boundary).validate(),
            Err(RangeCertificateError::StopInconsistent)
        );
        assert_eq!(
            asc_cert(prefix(), 1, &["docs/a"], ScanStop::Exhausted).validate(),
            Err(RangeCertificateError::StopInconsistent)
        );
    }

    #[test]
    fn scan_prefix_skips_tombstones_without_consuming_limit() {
        let store = MvccStore::new();
        store.seed(DocumentId::new("docs/a"), doc_with_i64("value", 1)); // ts=1
        store.seed(DocumentId::new("docs/b"), doc_with_i64("value", 2)); // ts=2
        store.seed(DocumentId::new("docs/c"), doc_with_i64("value", 3)); // ts=3
        let mut deletion = WriteSet::default();
        deletion.delete(DocumentId::new("docs/b"));
        store
            .commit(store.snapshot_ts(), &ReadSet::default(), &deletion)
            .expect("tombstone commit"); // ts=4

        // The buggy prefix_at counted docs/b toward the limit and returned
        // its tombstone; the certified scan must return the 2 live keys.
        let (cert, entries) = store
            .scan_prefix_at("docs/", 2, store.snapshot_ts())
            .expect("scan");
        cert.validate().expect("valid certificate");
        assert_eq!(
            cert.keys,
            vec![DocumentId::new("docs/a"), DocumentId::new("docs/c")]
        );
        assert_eq!(cert.stop, ScanStop::Boundary);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].1.version, Some(1));
        assert_eq!(entries[1].1.version, Some(3));
    }

    #[test]
    fn scan_prefix_hides_keys_created_after_snapshot() {
        let store = MvccStore::new();
        store.seed(DocumentId::new("docs/a"), doc_with_i64("value", 1)); // ts=1
        let snapshot = store.snapshot_ts();
        store.seed(DocumentId::new("docs/b"), doc_with_i64("value", 2)); // ts=2

        let (cert, entries) = store.scan_prefix_at("docs/", 10, snapshot).expect("scan");
        cert.validate().expect("valid certificate");
        // docs/b exists in the key space but not at the snapshot — it must
        // be invisible, and the scan proves the rest of the interval empty.
        assert_eq!(cert.keys, vec![DocumentId::new("docs/a")]);
        assert_eq!(cert.stop, ScanStop::Exhausted);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn scan_prefix_stop_is_boundary_iff_limit_hit() {
        let store = MvccStore::new();
        store.seed(DocumentId::new("docs/a"), doc_with_i64("value", 1));
        store.seed(DocumentId::new("docs/b"), doc_with_i64("value", 2));
        let ts = store.snapshot_ts();

        // Exactly limit live keys → Boundary, even though nothing follows.
        // Completeness needs limit+1 (referee F2) — this is the footgun case.
        let (boundary, _) = store.scan_prefix_at("docs/", 2, ts).expect("scan");
        assert_eq!(boundary.stop, ScanStop::Boundary);
        boundary.validate().expect("valid boundary certificate");

        let (exhausted, _) = store.scan_prefix_at("docs/", 3, ts).expect("scan");
        assert_eq!(exhausted.stop, ScanStop::Exhausted);
        assert_eq!(exhausted.keys.len(), 2);
        exhausted.validate().expect("valid exhausted certificate");
    }

    #[test]
    fn scan_prefix_rejects_zero_limit() {
        let store = MvccStore::new();
        assert_eq!(
            store.scan_prefix_at("docs/", 0, 1),
            Err(RangeCertificateError::ZeroLimit)
        );
    }

    #[test]
    fn scan_prefix_hydrates_capsule_that_validates() {
        let store = MvccStore::new();
        store.seed(DocumentId::new("docs/a"), doc_with_i64("value", 1));
        store.seed(DocumentId::new("docs/b"), doc_with_i64("value", 2));
        let ts = store.snapshot_ts();

        let mut capsule =
            SnapshotCapsule::empty(TenantId::new("tenant-a"), DeploymentId::new("dep-a"), ts);
        let (cert, entries) = store.scan_prefix_at("docs/", 2, ts).expect("scan");
        capsule.hydrate_range(cert, entries);
        capsule
            .validate_structure()
            .expect("issued capsules satisfy structure by construction");
        assert_eq!(capsule.ranges.len(), 1);
        assert!(capsule.get(&DocumentId::new("docs/a")).is_some());
    }

    #[test]
    fn validate_structure_requires_live_crossreferenced_range_keys() {
        let mut capsule = SnapshotCapsule::empty(
            TenantId::new("tenant-a"),
            DeploymentId::new("dep-a"),
            10,
        );
        capsule.hydrate_point(DocumentId::new("docs/live"), live_doc(3, 1));
        capsule.hydrate_range(
            asc_cert(
                KeyInterval::Prefix("docs/".into()),
                5,
                &["docs/live"],
                ScanStop::Exhausted,
            ),
            Vec::new(),
        );
        capsule.validate_structure().expect("valid structure");

        // Certificate returning a key the docs map does not contain.
        let mut missing = capsule.clone();
        missing.ranges[0].keys = vec![DocumentId::new("docs/ghost")];
        assert!(matches!(
            missing.validate_structure(),
            Err(CapsuleStructureError::RangeKeyNotInDocs { .. })
        ));

        // Certificate returning a key whose docs entry is a tombstone.
        let mut dead = capsule.clone();
        dead.hydrate_point(
            DocumentId::new("docs/dead"),
            VersionedDocument {
                version: Some(4),
                document: None,
            },
        );
        dead.ranges[0].keys = vec![DocumentId::new("docs/dead")];
        assert!(matches!(
            dead.validate_structure(),
            Err(CapsuleStructureError::RangeKeyNotLive { .. })
        ));
    }
}
