//! V8-backed read-trap continuation runtime for Aster.
//!
//! This crate answers the load-bearing v0.1 question: can a tenant JavaScript
//! function suspend on a missing capsule read, let the host hydrate data, and
//! resume inside a real V8 isolate without rebuilding V8's scheduler?
//!
//! The answer demonstrated here is "yes, if the continuation boundary is an
//! `await` over a host-created Promise". We do not attempt to capture arbitrary
//! synchronous JS stacks. The legacy host API `Aster.read(key, field)` and the
//! Convex-shaped `Convex.asyncSyscall("1.0/get", argsJson)` shim both return
//! values immediately for warm capsule entries and pending Promises for missing
//! reads. V8 preserves the async continuation. The Rust host receives a typed
//! trap, hydrates the capsule through its broker, resolves the promise, runs a
//! microtask checkpoint, and the same JS async function returns the final value.
//!
//! v0.7 makes the cell an honest Variante B runtime (Capsule Transaction
//! Theorem + referee finding F7): every key the JS actually observes — warm
//! hit, post-trap read, or absence read — lands in a deduped, deterministic
//! consumption ledger surfaced as `V8ExecutionResult::consumed_reads`, the
//! declared dependency set the commit verb submits. Traps count broker round
//! trips; consumption counts observations. The two are no longer conflated.
//!
//! S9b: the write set is born in the cell. The upstream mutation syscalls
//! (`1.0/insert`, `1.0/shallowMerge` = db.patch, `1.0/replace`,
//! `1.0/remove` = db.delete — the names the real esbuild bundle emits, see
//! tests/fixtures/messages.bundled.js `database_impl.js` section) accumulate
//! into `V8CellState::writes` and NEVER touch the broker mid-execution: the
//! only wire verbs a running cell issues remain reads. Reads are answered
//! through the invocation view — the write set applied over the sealed
//! capsule — so the JS observes its own writes without a trap. The pair
//! (`consumed_reads`, `write_set`) on the execution result is exactly the
//! `(S, W)` the S9a Commit verb submits for fenced commit.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::c_void;
use std::sync::{Mutex, Once};

use aster_broker::{BrokerError, CapsuleBrokerClient};
use aster_capsule::{
    DeploymentId, Document, DocumentId, MvccStore, SealContext, SealedCapsule, SnapshotCapsule,
    TenantId, Value,
};

static V8_INIT: Once = Once::new();

fn init_v8() {
    V8_INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

/// A typed read trap emitted by a real V8 isolate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct V8ReadTrap {
    pub key: DocumentId,
    pub field: String,
}

/// Generic trap descriptor — either the toy `Aster.read(key, field)` API
/// the prototype shipped with, or a Convex async syscall (`Convex.asyncSyscall`)
/// matching the upstream backend's wire shape. The cell scheduler dispatches
/// on this enum and resolves the embedded `PromiseResolver` either way.
#[derive(Debug)]
enum PendingTrap {
    /// Legacy `Aster.read(key, field)` — point read for one document field.
    AsterRead {
        key: DocumentId,
        field: String,
        resolver: v8::Global<v8::PromiseResolver>,
    },
    /// `Convex.asyncSyscall(name, args_json_string)` — the Convex backend's
    /// real wire shape. A parked syscall is one the invocation view could
    /// not answer synchronously: a read (or a mutation's implicit base
    /// read) of an unheld key awaiting hydration, or a malformed payload
    /// surfacing its host-level error through the loop.
    ConvexSyscall {
        name: String,
        args_json: String,
        resolver: v8::Global<v8::PromiseResolver>,
    },
}

impl PendingTrap {
    fn resolver(&self) -> &v8::Global<v8::PromiseResolver> {
        match self {
            Self::AsterRead { resolver, .. } => resolver,
            Self::ConvexSyscall { resolver, .. } => resolver,
        }
    }
}

#[derive(Debug, Default)]
struct V8CellState {
    capsule: Option<SnapshotCapsule>,
    traps: VecDeque<PendingTrap>,
    /// Review-C3 gate: true for a query invocation — the four mutating
    /// syscalls refuse with a catchable JS error, mirroring upstream
    /// Convex's "db writes are not allowed in queries". The legacy script
    /// path and `Default` stay permissive (their harnesses predate
    /// invocation kinds); `execute_module_core` arms this per invocation.
    deny_writes: bool,
    /// Variante B consumption ledger (referee finding F7): every key the JS
    /// actually observed through a get — warm hits, post-trap reads, and
    /// absence reads all count; prewarm the function never touched does not.
    /// This set (not the trap log) is the declared dependency set `S` the
    /// commit verb submits.
    consumed: BTreeSet<DocumentId>,
    /// S9b write set — the theorem's `W`, born in the cell and applied over
    /// the capsule for reads within this invocation. `Some(document)` puts
    /// (insert / patch / replace), `None` is a delete tombstone. Writes
    /// never reach the broker mid-execution; the harness submits this map
    /// (BTreeMap order, deterministic) with the S9a Commit verb.
    ///
    /// ORDERING RULE (consumption vs writes): what decides is observation
    /// order, not syscall-issuance order. A read RESOLVED from the capsule
    /// observed snapshot state and lands in `consumed` — permanently, even
    /// if the key is overwritten later (the set never un-records: the
    /// transaction really did depend on that pre-state). A read resolved
    /// from the write set observed the transaction's own effect and records
    /// nothing — it is not a snapshot dependency, and letting it into the
    /// ledger would manufacture spurious conflicts for keys the cell
    /// invented. Mutations that branch on snapshot state (patch's merge
    /// base, replace/remove's existence check) consume their key exactly
    /// when that base came from the capsule rather than from a pending
    /// write.
    writes: BTreeMap<DocumentId, Option<Document>>,
}

/// What one `Convex.asyncSyscall` dispatch decided. `Resolve`/`Reject`
/// answer the JS promise locally (no broker involvement); `NeedsHydration`
/// parks the syscall as a `PendingTrap` so the host loop can fetch the key
/// through the broker and dispatch again.
#[derive(Debug)]
enum SyscallAnswer {
    /// Resolve the promise with this JSON string (the bundle's
    /// `performAsyncSyscall` runs `JSON.parse` on it).
    Resolve(String),
    /// Reject the promise with a structured, catchable JS `Error` carrying
    /// this message — the shape user handlers observe for semantic
    /// failures (e.g. patch on a nonexistent document), mirroring how the
    /// upstream backend surfaces syscall errors.
    Reject(String),
    /// The invocation view has no entry for this key: hydrate it into the
    /// capsule, then dispatch the same syscall again (which must then
    /// answer — hydration always installs an entry, absent included).
    NeedsHydration(DocumentId),
}

/// The base document a mutating verb builds on, resolved through the
/// invocation view (pending write first, then capsule). Live bases carry
/// the parsed `_raw` JSON object — the canonical doc form of the Convex
/// syscall surface (a live doc without a `_raw` object merges from an
/// empty base; only legacy hand-seeded test docs look like that).
enum MutationBase {
    PendingLive(serde_json::Map<String, serde_json::Value>),
    PendingDeleted,
    CapsuleLive(serde_json::Map<String, serde_json::Value>),
    CapsuleAbsent,
    Unheld,
}

impl V8CellState {
    fn read_field(&self, key: &DocumentId, field: &str) -> Option<Value> {
        let capsule = self.capsule.as_ref()?;
        let versioned = capsule.get(key)?;
        let document = versioned.document.as_ref()?;
        document.get(field).cloned()
    }

    /// True when the capsule holds *any* entry for `key`. A live document, a
    /// tombstone, and a hydrated-absent entry are all warm — each pins the
    /// version (or absence) at the snapshot, so serving it locally observes
    /// exactly what a broker trap would. Only a key with no entry traps.
    fn holds(&self, key: &DocumentId) -> bool {
        self.capsule
            .as_ref()
            .is_some_and(|capsule| capsule.contains(key))
    }

    /// Serve a `1.0/get` out of the capsule AND record the key in the
    /// consumption ledger. Missing/tombstoned documents serve as `"null"` —
    /// that absence is still a recorded read: an absence flip must conflict
    /// at commit time, so the theorem obliges the ledger to carry it.
    fn consume_get_raw_json(&mut self, key: &DocumentId) -> String {
        self.consumed.insert(key.clone());
        let raw = self
            .capsule
            .as_ref()
            .and_then(|capsule| capsule.get(key))
            .and_then(|versioned| versioned.document.as_ref())
            .and_then(|doc| doc.get("_raw"))
            .cloned();
        match raw {
            Some(Value::Text(s)) => s,
            _ => "null".to_string(),
        }
    }

    /// Legacy `Aster.read` warm probe through the invocation view. A key the
    /// invocation wrote answers from the write set un-ledgered (ORDERING
    /// RULE); any HELD entry answers consumed (absence serves `Null`);
    /// `None` means trap.
    fn legacy_warm_read(&mut self, key: &DocumentId, field: &str) -> Option<Value> {
        if let Some(pending) = self.writes.get(key) {
            return Some(pending_field_value(pending.as_ref(), field));
        }
        // A held entry is warm whatever it holds (review C7): a hydrated
        // absence, a tombstone, and a live doc without the field all pin
        // what a trap would observe — re-trapping would hydrate, land in
        // this same state, and re-trap forever. Absence is still a consumed
        // read (an absence flip must conflict at commit). Only an unheld
        // key traps.
        if self.holds(key) {
            self.consumed.insert(key.clone());
            return Some(self.read_field(key, field).unwrap_or(Value::Null));
        }
        None
    }

    /// Legacy `Aster.read` answer after the host hydrated `key`: the view
    /// now holds it one way or another. Same write-set-first rule as the
    /// warm probe (a `Promise.all` interleave can land a write between trap
    /// enqueue and dispatch); capsule-served reads — absence included —
    /// are consumed.
    fn legacy_read_after_hydrate(&mut self, key: &DocumentId, field: &str) -> Value {
        if let Some(pending) = self.writes.get(key) {
            return pending_field_value(pending.as_ref(), field);
        }
        self.consumed.insert(key.clone());
        self.read_field(key, field).unwrap_or(Value::Null)
    }

    /// Upstream Convex rejects db writes inside query invocations (review
    /// C3); the module entry point arms `deny_writes` per invocation kind
    /// before any JS runs. `Some` is the typed JS-visible rejection the
    /// four mutating verbs return unchanged — a catchable `Error`,
    /// upstream-shaped, not a host abort.
    fn write_gate(&self) -> Option<SyscallAnswer> {
        self.deny_writes.then(|| {
            SyscallAnswer::Reject(
                "aster-v8cell: db write syscalls are not allowed in a query invocation \
                 (upstream Convex parity)"
                    .to_string(),
            )
        })
    }

    /// The single dispatch point for every `Convex.asyncSyscall` name the
    /// cell understands, shared by the JS callback (synchronous answers)
    /// and the trap loop (post-hydration answers). `Err` is a host-level
    /// failure that aborts the execution (malformed syscall payloads —
    /// unreachable from a real bundle); JS-visible failures are
    /// `Ok(Reject(..))`.
    fn dispatch_convex_syscall(
        &mut self,
        name: &str,
        args_json: &str,
    ) -> Result<SyscallAnswer, V8CellError> {
        match name {
            "1.0/get" => self.syscall_get(args_json),
            "1.0/insert" => self.syscall_insert(args_json),
            "1.0/shallowMerge" => self.syscall_patch(args_json),
            "1.0/replace" => self.syscall_replace(args_json),
            "1.0/remove" => self.syscall_remove(args_json),
            other => Ok(SyscallAnswer::Reject(format!(
                "aster-v8cell: unsupported convex syscall {other:?} — wired verbs: 1.0/get, \
                 1.0/insert, 1.0/shallowMerge (db.patch), 1.0/replace, 1.0/remove (db.delete)"
            ))),
        }
    }

    fn syscall_get(&mut self, args_json: &str) -> Result<SyscallAnswer, V8CellError> {
        let id = parse_get_id(args_json)?;
        let key = DocumentId::new(id);
        // Read-your-own-writes: a key this invocation wrote answers from
        // the write set — no trap, no consumption (ORDERING RULE on
        // `V8CellState::writes`).
        if let Some(pending) = self.writes.get(&key) {
            return Ok(SyscallAnswer::Resolve(pending_raw_json(pending.as_ref())));
        }
        if self.holds(&key) {
            return Ok(SyscallAnswer::Resolve(self.consume_get_raw_json(&key)));
        }
        Ok(SyscallAnswer::NeedsHydration(key))
    }

    /// `db.insert(table, value)` → `1.0/insert {table, value}`; the syscall
    /// resolves `{"_id": <id>}` (the bundle reads `y(r)._id`).
    ///
    /// Upstream Convex mints the document id server-side. Aster's id scheme
    /// is the store's, and broker-side minting is future work — so the
    /// prototype accepts an explicit `_id` supplied in the value (a shape
    /// only hand-written modules can produce; the real client lib never
    /// sends system fields) and REJECTS mint-me inserts with a clear error
    /// rather than inventing an id scheme silently. A supplied-id insert is
    /// a plain put: it does not probe the snapshot for collisions (no
    /// consumption), so colliding with an existing doc is an authorized
    /// blind overwrite the fence orders like any other write (T2).
    fn syscall_insert(&mut self, args_json: &str) -> Result<SyscallAnswer, V8CellError> {
        if let Some(reject) = self.write_gate() {
            return Ok(reject);
        }
        let args = parse_syscall_args("1.0/insert", args_json)?;
        let Some(value) = args.get("value").and_then(|v| v.as_object()) else {
            return Err(V8CellError::Run(
                "convex 1.0/insert: missing object field `value` in args".to_string(),
            ));
        };
        let Some(id) = value.get("_id").and_then(|v| v.as_str()) else {
            return Ok(SyscallAnswer::Reject(
                "aster-v8cell: db.insert without an `_id` field in the value is not supported \
                 yet — upstream Convex mints document ids server-side and Aster's broker-side \
                 id minting is future work; supply an explicit `_id` (the store's wire form) \
                 to insert from a cell"
                    .to_string(),
            ));
        };
        let key = DocumentId::new(id);
        let raw = serde_json::Value::Object(value.clone()).to_string();
        self.writes.insert(key, Some(raw_doc(raw)));
        Ok(SyscallAnswer::Resolve(
            serde_json::json!({ "_id": id }).to_string(),
        ))
    }

    /// `db.patch(id, value)` → `1.0/shallowMerge {id, value, table?}`.
    /// Merges the patch fields into the current in-cell view of the
    /// document (pending write first, else capsule doc); a field valued
    /// with the `{"$undefined": null}` marker (`patchValueToJson`) is
    /// removed. Resolves with the merged document JSON (what the upstream
    /// syscall returns; the bundle discards it after `JSON.parse`).
    ///
    /// A key absent in both view layers is a structured JS error, mirroring
    /// upstream Convex, which errors when patching a nonexistent document.
    /// A key with NO view entry at all is not "absent" — it is unobserved:
    /// the syscall hydrates it first (upstream's implicit read), and that
    /// snapshot observation is consumed like any read.
    fn syscall_patch(&mut self, args_json: &str) -> Result<SyscallAnswer, V8CellError> {
        if let Some(reject) = self.write_gate() {
            return Ok(reject);
        }
        let args = parse_syscall_args("1.0/shallowMerge", args_json)?;
        let id = require_id("1.0/shallowMerge", &args)?;
        let Some(patch) = args.get("value").and_then(|v| v.as_object()) else {
            return Err(V8CellError::Run(
                "convex 1.0/shallowMerge: missing object field `value` in args".to_string(),
            ));
        };
        let key = DocumentId::new(id.clone());
        let (mut base, from_capsule) = match self.mutation_base(&key)? {
            MutationBase::Unheld => return Ok(SyscallAnswer::NeedsHydration(key)),
            MutationBase::PendingDeleted => {
                return Ok(SyscallAnswer::Reject(nonexistent_doc_message("patch", &id)));
            }
            MutationBase::CapsuleAbsent => {
                // The absence WAS observed from the snapshot — it is a
                // consumed read even though the verb then fails: had the
                // doc existed, this transaction would have behaved
                // differently.
                self.consumed.insert(key);
                return Ok(SyscallAnswer::Reject(nonexistent_doc_message("patch", &id)));
            }
            MutationBase::PendingLive(base) => (base, false),
            MutationBase::CapsuleLive(base) => (base, true),
        };
        if from_capsule {
            self.consumed.insert(key.clone());
        }
        for (field, value) in patch {
            if is_undefined_marker(value) {
                base.remove(field);
            } else {
                base.insert(field.clone(), value.clone());
            }
        }
        // The id is never patchable — pin the args id as the document's
        // `_id`, canonical whatever the patch carried.
        base.insert("_id".to_string(), serde_json::Value::String(id));
        let merged = serde_json::Value::Object(base).to_string();
        self.writes.insert(key, Some(raw_doc(merged.clone())));
        Ok(SyscallAnswer::Resolve(merged))
    }

    /// `db.replace(id, value)` → `1.0/replace {id, value, table?}`. Replaces
    /// the whole document, preserving `_id` (from the args) and, when the
    /// replacement omits it, `_creationTime` from the base — upstream keeps
    /// both system fields across a replace. Errors on a nonexistent
    /// document like upstream; the existence check consumes the key when it
    /// was answered from the snapshot (same rule as patch).
    fn syscall_replace(&mut self, args_json: &str) -> Result<SyscallAnswer, V8CellError> {
        if let Some(reject) = self.write_gate() {
            return Ok(reject);
        }
        let args = parse_syscall_args("1.0/replace", args_json)?;
        let id = require_id("1.0/replace", &args)?;
        let Some(value) = args.get("value").and_then(|v| v.as_object()) else {
            return Err(V8CellError::Run(
                "convex 1.0/replace: missing object field `value` in args".to_string(),
            ));
        };
        let key = DocumentId::new(id.clone());
        let (base, from_capsule) = match self.mutation_base(&key)? {
            MutationBase::Unheld => return Ok(SyscallAnswer::NeedsHydration(key)),
            MutationBase::PendingDeleted => {
                return Ok(SyscallAnswer::Reject(nonexistent_doc_message(
                    "replace", &id,
                )));
            }
            MutationBase::CapsuleAbsent => {
                self.consumed.insert(key);
                return Ok(SyscallAnswer::Reject(nonexistent_doc_message(
                    "replace", &id,
                )));
            }
            MutationBase::PendingLive(base) => (base, false),
            MutationBase::CapsuleLive(base) => (base, true),
        };
        if from_capsule {
            self.consumed.insert(key.clone());
        }
        let mut doc = value.clone();
        doc.insert("_id".to_string(), serde_json::Value::String(id));
        if !doc.contains_key("_creationTime") {
            if let Some(creation) = base.get("_creationTime") {
                doc.insert("_creationTime".to_string(), creation.clone());
            }
        }
        let replaced = serde_json::Value::Object(doc).to_string();
        self.writes.insert(key, Some(raw_doc(replaced.clone())));
        Ok(SyscallAnswer::Resolve(replaced))
    }

    /// `db.delete(id)` → `1.0/remove {id, table?}`. Produces a tombstone
    /// (`None`) in the write set and resolves with the removed document's
    /// JSON (what upstream returns). Errors on a nonexistent document like
    /// upstream — deleting requires the doc to exist, and that existence
    /// check consumes the key when answered from the snapshot.
    fn syscall_remove(&mut self, args_json: &str) -> Result<SyscallAnswer, V8CellError> {
        if let Some(reject) = self.write_gate() {
            return Ok(reject);
        }
        let args = parse_syscall_args("1.0/remove", args_json)?;
        let id = require_id("1.0/remove", &args)?;
        let key = DocumentId::new(id.clone());
        let (base, from_capsule) = match self.mutation_base(&key)? {
            MutationBase::Unheld => return Ok(SyscallAnswer::NeedsHydration(key)),
            MutationBase::PendingDeleted => {
                return Ok(SyscallAnswer::Reject(nonexistent_doc_message("delete", &id)));
            }
            MutationBase::CapsuleAbsent => {
                self.consumed.insert(key);
                return Ok(SyscallAnswer::Reject(nonexistent_doc_message("delete", &id)));
            }
            MutationBase::PendingLive(base) => (base, false),
            MutationBase::CapsuleLive(base) => (base, true),
        };
        if from_capsule {
            self.consumed.insert(key.clone());
        }
        let removed = serde_json::Value::Object(base).to_string();
        self.writes.insert(key, None);
        Ok(SyscallAnswer::Resolve(removed))
    }

    /// Resolve the document a mutating verb builds on, through the
    /// invocation view: the pending write wins, then the capsule entry;
    /// no entry anywhere is `Unheld` (hydrate first).
    fn mutation_base(&self, key: &DocumentId) -> Result<MutationBase, V8CellError> {
        if let Some(pending) = self.writes.get(key) {
            return Ok(match pending {
                Some(doc) => MutationBase::PendingLive(raw_json_object(doc)?),
                None => MutationBase::PendingDeleted,
            });
        }
        let Some(versioned) = self.capsule.as_ref().and_then(|capsule| capsule.get(key)) else {
            return Ok(MutationBase::Unheld);
        };
        Ok(match versioned.document.as_ref() {
            Some(doc) => MutationBase::CapsuleLive(raw_json_object(doc)?),
            None => MutationBase::CapsuleAbsent,
        })
    }
}

/// Serve a write-set entry the way `1.0/get` serves capsule docs: the
/// `_raw` envelope verbatim, `"null"` for tombstones.
fn pending_raw_json(pending: Option<&Document>) -> String {
    match pending.and_then(|doc| doc.get("_raw")) {
        Some(Value::Text(raw)) => raw.clone(),
        _ => "null".to_string(),
    }
}

/// Field view of a write-set entry for the legacy `Aster.read` surface.
/// Tombstones and missing fields read as `Null`, matching how the capsule
/// path answers absent fields.
fn pending_field_value(pending: Option<&Document>, field: &str) -> Value {
    pending
        .and_then(|doc| doc.get(field))
        .cloned()
        .unwrap_or(Value::Null)
}

/// The `_raw` envelope of a live doc parsed to a JSON object. The Convex
/// syscall surface's canonical document form IS the `_raw` envelope (the
/// Postgres adapter's upstream JSON bytes — see `consume_get_raw_json`);
/// a live doc without one (legacy hand-seeded typed docs) merges from an
/// empty base rather than failing, keeping the toy fixtures runnable.
/// A PRESENT but unparseable/non-object `_raw` is a different animal: it
/// means the store handed us a corrupt envelope, and silently merging from
/// an empty base would let a patch replace the whole document — error out.
fn raw_json_object(doc: &Document) -> Result<serde_json::Map<String, serde_json::Value>, V8CellError> {
    match doc.get("_raw") {
        Some(Value::Text(raw)) => serde_json::from_str::<serde_json::Value>(raw)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .ok_or_else(|| {
                V8CellError::Run(
                    "convex mutation: document `_raw` envelope is not a JSON object".to_string(),
                )
            }),
        _ => Ok(serde_json::Map::new()),
    }
}

/// Wrap upstream-envelope JSON bytes into the store's document shape.
fn raw_doc(raw_json: String) -> Document {
    let mut doc = Document::new();
    doc.insert("_raw".to_string(), Value::Text(raw_json));
    doc
}

/// `patchValueToJson` encodes a JS `undefined` patch field — "remove this
/// field" — as `{"$undefined": null}` (bundle `convexOrUndefinedToJson`).
fn is_undefined_marker(value: &serde_json::Value) -> bool {
    value
        .as_object()
        .is_some_and(|obj| obj.len() == 1 && obj.contains_key("$undefined"))
}

/// Parse a mutation syscall's args payload to a JSON object. Host-level
/// error on malformed payloads (a real bundle cannot produce them), same
/// severity as `parse_get_id`.
fn parse_syscall_args(
    name: &str,
    args_json: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, V8CellError> {
    let value: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|err| V8CellError::Run(format!("convex {name} bad JSON args: {err}")))?;
    match value {
        serde_json::Value::Object(map) => Ok(map),
        _ => Err(V8CellError::Run(format!(
            "convex {name}: args must be a JSON object"
        ))),
    }
}

fn require_id(
    name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, V8CellError> {
    args.get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            V8CellError::Run(format!("convex {name}: missing string field `id` in args"))
        })
}

/// The structured JS error for a mutation aimed at a document the view
/// says does not exist. Upstream Convex errors on patch/replace/delete of
/// a missing document; the exact upstream wording is not pinned in this
/// repo's reference doc, so the message mirrors the behavior and names the
/// operation + id (the safe choice per the S9b spec).
fn nonexistent_doc_message(verb: &str, id: &str) -> String {
    format!(
        "aster-v8cell: {verb} on nonexistent document {id:?} — the document does not exist in \
         this transaction's view (upstream Convex rejects {verb} of a missing document)"
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct V8ExecutionResult {
    pub output: Value,
    pub traps: usize,
    pub capsule_hash: u64,
    /// The declared dependency set `S` of the Capsule Transaction Theorem's
    /// Variante B: the deduped, deterministically ordered (BTreeSet order)
    /// keys the JS actually observed — warm hits, post-trap reads, and
    /// absence reads. Unconsumed prewarm is absent by design; the commit
    /// verb submits exactly this set.
    pub consumed_reads: Vec<DocumentId>,
    /// The write set `W` born in the cell (S9b), in deterministic BTreeMap
    /// order: `Some(document)` puts, `None` delete tombstones. Reads within
    /// the invocation already observed these entries (read-your-own-writes)
    /// without ledgering them — see the ORDERING RULE on
    /// `V8CellState::writes`. Shaped exactly like the S9a Commit verb's
    /// `writes` field so a harness passes it through unchanged.
    pub write_set: Vec<(DocumentId, Option<Document>)>,
    /// The final sealed capsule after every hydration this execution
    /// performed — the evidence object the Commit verb submits together
    /// with (`consumed_reads`, `write_set`). `Some` on the broker-backed
    /// entry points; `None` on the store-direct test paths, which have no
    /// seal key or session to seal under.
    pub sealed_capsule: Option<SealedCapsule>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum V8CellError {
    WrongTenant,
    WrongDeployment,
    Compile(String),
    Run(String),
    NotAPromise,
    TooManyTraps { limit: usize },
    PendingWithoutTrap,
    Rejected(String),
    UnsupportedValue(String),
    Broker(String),
}

impl std::fmt::Display for V8CellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongTenant => write!(f, "invocation tenant does not match V8 cell tenant"),
            Self::WrongDeployment => {
                write!(f, "invocation deployment does not match V8 cell deployment")
            }
            Self::Compile(error) => write!(f, "JavaScript compile error: {error}"),
            Self::Run(error) => write!(f, "JavaScript run error: {error}"),
            Self::NotAPromise => write!(f, "JavaScript entrypoint did not return a Promise"),
            Self::TooManyTraps { limit } => write!(f, "too many V8 read traps, limit {limit}"),
            Self::PendingWithoutTrap => {
                write!(f, "Promise is pending but no read trap was emitted")
            }
            Self::Rejected(error) => write!(f, "JavaScript promise rejected: {error}"),
            Self::UnsupportedValue(error) => write!(f, "unsupported JavaScript value: {error}"),
            Self::Broker(error) => write!(f, "broker error: {error}"),
        }
    }
}

impl std::error::Error for V8CellError {}

impl From<BrokerError> for V8CellError {
    fn from(value: BrokerError) -> Self {
        Self::Broker(value.to_string())
    }
}

/// A tenant/deployment pinned V8 cell.
///
/// The isolate is real and the broker may be in-process (unit tests) or a
/// UDS-backed process (`aster_v8cell`). The cell global object intentionally
/// exposes only the narrow syscall surfaces (`Aster.read` legacy plus
/// `Convex.asyncSyscall` — reads and the S9b write-set mutation verbs); no
/// `fetch`, no timers, no filesystem, and no database handle are installed.
/// Mutations grant no store authority: they accumulate locally and only the
/// harness-driven Commit verb can land them, through the fence.
pub struct V8SandboxCell {
    tenant: TenantId,
    deployment: DeploymentId,
    max_traps: usize,
}

/// What the namespace lookup yielded when the cell-side ESM loader
/// inspects the bundled module.
///
/// The Convex factory `query()`/`mutation()`/`action()` produces a
/// callable function with `isQuery|isMutation|isAction` flags hung off
/// it (see /tmp/aster-research-bundle-ground-truth.md §3.3). Queries and
/// (since S9b) mutations run through their matching entry points; actions
/// are rejected up front rather than half-executed.
#[derive(Debug, Eq, PartialEq)]
enum ExportShape {
    Query,
    Mutation,
    Action,
    Other,
}

/// Which Convex function shape a module invocation drives. The bundle's
/// factory hangs `invokeQuery` / `invokeMutation` off the export next to
/// the matching `isQuery` / `isMutation` marker (registration_impl.js —
/// `queryGeneric` / `mutationGeneric` in the fixture bundle); both return
/// `Promise<string>` of the JSON-encoded result. The cell dispatches on
/// the marker so a mutation cannot slip through the query gate and vice
/// versa.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModuleInvocation {
    Query,
    Mutation,
}

impl ModuleInvocation {
    fn invoke_key(self) -> &'static str {
        match self {
            Self::Query => "invokeQuery",
            Self::Mutation => "invokeMutation",
        }
    }
}

impl V8SandboxCell {
    pub fn new(tenant: TenantId, deployment: DeploymentId, max_traps: usize) -> Self {
        init_v8();
        Self {
            tenant,
            deployment,
            max_traps,
        }
    }

    /// Execute `source` as an async JS program named `main`.
    ///
    /// `main` must return a Promise. Missing reads suspend at `await
    /// Aster.read(key, field)`. This method drains all typed traps by reading
    /// from `store` at the original snapshot timestamp and resolving the exact
    /// V8 `PromiseResolver` that caused the trap.
    pub fn execute_async_main_with_broker(
        &self,
        broker: &impl CapsuleBrokerClient,
        cell_id: impl Into<String>,
        lease_epoch: u64,
        tenant: TenantId,
        deployment: DeploymentId,
        snapshot_ts: u64,
        prewarm: Vec<DocumentId>,
        source: &str,
    ) -> Result<V8ExecutionResult, V8CellError> {
        if tenant != self.tenant {
            return Err(V8CellError::WrongTenant);
        }
        if deployment != self.deployment {
            return Err(V8CellError::WrongDeployment);
        }

        let context = SealContext::new(cell_id, lease_epoch);
        let initial = broker.initial_capsule(&context, tenant, deployment, snapshot_ts, prewarm)?;
        let boxed_state = Box::new(Mutex::new(V8CellState {
            capsule: Some(initial.capsule().clone()),
            traps: VecDeque::new(),
            deny_writes: false,
            consumed: BTreeSet::new(),
            writes: BTreeMap::new(),
        }));
        let state_ptr: *mut Mutex<V8CellState> = Box::into_raw(boxed_state);

        let result = unsafe {
            self.execute_with_broker_state_ptr(broker, &context, initial, state_ptr, source)
        };

        let state_box = unsafe { Box::from_raw(state_ptr) };
        match result {
            Ok(mut output) => {
                let state = state_box.lock().expect("v8 state mutex poisoned");
                let hash = state
                    .capsule
                    .as_ref()
                    .map(|capsule| capsule.root_hash)
                    .unwrap_or_default();
                output.capsule_hash = hash;
                output.consumed_reads = state.consumed.iter().cloned().collect();
                output.write_set = state
                    .writes
                    .iter()
                    .map(|(key, document)| (key.clone(), document.clone()))
                    .collect();
                Ok(output)
            }
            Err(error) => Err(error),
        }
    }

    pub fn execute_async_main(
        &self,
        store: &MvccStore,
        tenant: TenantId,
        deployment: DeploymentId,
        snapshot_ts: u64,
        prewarm: Vec<DocumentId>,
        source: &str,
    ) -> Result<V8ExecutionResult, V8CellError> {
        if tenant != self.tenant {
            return Err(V8CellError::WrongTenant);
        }
        if deployment != self.deployment {
            return Err(V8CellError::WrongDeployment);
        }

        let initial_capsule = store.build_capsule(tenant, deployment, snapshot_ts, prewarm);
        let boxed_state = Box::new(Mutex::new(V8CellState {
            capsule: Some(initial_capsule),
            traps: VecDeque::new(),
            deny_writes: false,
            consumed: BTreeSet::new(),
            writes: BTreeMap::new(),
        }));
        let state_ptr: *mut Mutex<V8CellState> = Box::into_raw(boxed_state);

        // SAFETY: `state_ptr` is stored in one V8 External and all JS execution
        // is synchronous on this Rust thread. We reclaim the Box before return.
        let result = unsafe { self.execute_with_state_ptr(store, state_ptr, source) };

        // SAFETY: V8 is done with callbacks by this point; reclaim ownership.
        let state_box = unsafe { Box::from_raw(state_ptr) };
        match result {
            Ok(mut output) => {
                let state = state_box.lock().expect("v8 state mutex poisoned");
                let hash = state
                    .capsule
                    .as_ref()
                    .map(|capsule| capsule.root_hash)
                    .unwrap_or_default();
                output.capsule_hash = hash;
                output.consumed_reads = state.consumed.iter().cloned().collect();
                output.write_set = state
                    .writes
                    .iter()
                    .map(|(key, document)| (key.clone(), document.clone()))
                    .collect();
                Ok(output)
            }
            Err(error) => Err(error),
        }
    }

    /// Run a Convex-deploy bundled module's named query end to end.
    ///
    /// This is the entry point a v8cell binary uses when the broker hands it
    /// a real `npx convex deploy` bundle (one fully-flattened ESM JS string,
    /// no remaining `import`s — see
    /// `/tmp/aster-research-bundle-ground-truth.md` §4) plus a function name
    /// and a JSON-encoded args object. The cell:
    ///
    /// 1. Builds an isolate + context with the same global surface as
    ///    `execute_async_main_with_broker` (Aster.read, Convex.asyncSyscall),
    ///    plus a `Convex.syscall` stub that throws on call so the bundle's
    ///    own `performSyscall` guard at lines 906–913 of the bundle finds a
    ///    function (not `undefined`) when it lazy-checks the surface — see
    ///    `/tmp/aster-research-bundle-ground-truth.md` §3.1.
    /// 2. Compiles `module_source` as an ES module. Per
    ///    `/tmp/aster-research-bundle-ground-truth.md` §4 the bundle has zero
    ///    `import` statements, so the resolver callback is defensive only.
    /// 3. Instantiates and evaluates the module, pumping microtasks and
    ///    draining `Convex.asyncSyscall` traps via the existing trap dispatch
    ///    loop (the same path that backs `execute_async_main_with_broker`).
    /// 4. Reads `module.namespace[function_name]`. If absent, errors with the
    ///    typed `Run` variant listing the actually-available exports.
    /// 5. Validates the export's shape: must be a Convex `query()` (i.e.
    ///    `isQuery === true`). Mutations go through
    ///    `execute_module_mutation_with_broker` instead — never through the
    ///    query gate; actions are rejected outright. See
    ///    `/tmp/aster-research-convex-udf-runner.md` §8 D6.
    /// 6. Calls `<export>.invokeQuery(args_json_string)`, which returns a
    ///    `Promise<string>` in the bundle's wire shape (`registration_impl.ts:328-345`).
    /// 7. Drives that promise to fulfilment with the same trap loop, then
    ///    returns the resolved string verbatim as `Value::Text` — the JSON
    ///    on the wire is the JS-side `JSON.stringify(convexToJson(result))`,
    ///    matching what the upstream Convex backend would have sent.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_module_query_with_broker(
        &self,
        broker: &impl CapsuleBrokerClient,
        cell_id: impl Into<String>,
        lease_epoch: u64,
        tenant: TenantId,
        deployment: DeploymentId,
        snapshot_ts: u64,
        prewarm: Vec<DocumentId>,
        module_source: &str,
        function_name: &str,
        args_json: &str,
    ) -> Result<V8ExecutionResult, V8CellError> {
        self.execute_module_with_broker(
            broker,
            cell_id,
            lease_epoch,
            tenant,
            deployment,
            snapshot_ts,
            prewarm,
            module_source,
            function_name,
            args_json,
            ModuleInvocation::Query,
        )
    }

    /// The mutation sibling of `execute_module_query_with_broker` (S9b):
    /// same ESM pipeline, but the export must be a Convex `mutation()`
    /// (`isMutation === true`) and the dispatch goes through
    /// `<export>.invokeMutation(args_json)` — the identical
    /// `Promise<string>` contract (`registration_impl.js::invokeMutation`).
    ///
    /// Running a mutation grants NO write capability: `db.insert/patch/
    /// replace/delete` inside the handler land in the in-cell write set
    /// (`V8ExecutionResult::write_set`), never on the broker. Committing
    /// the accumulated writes is the harness's move, via the S9a Commit
    /// verb with this result's (`sealed_capsule`, `consumed_reads`,
    /// `write_set`).
    #[allow(clippy::too_many_arguments)]
    pub fn execute_module_mutation_with_broker(
        &self,
        broker: &impl CapsuleBrokerClient,
        cell_id: impl Into<String>,
        lease_epoch: u64,
        tenant: TenantId,
        deployment: DeploymentId,
        snapshot_ts: u64,
        prewarm: Vec<DocumentId>,
        module_source: &str,
        function_name: &str,
        args_json: &str,
    ) -> Result<V8ExecutionResult, V8CellError> {
        self.execute_module_with_broker(
            broker,
            cell_id,
            lease_epoch,
            tenant,
            deployment,
            snapshot_ts,
            prewarm,
            module_source,
            function_name,
            args_json,
            ModuleInvocation::Mutation,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_module_with_broker(
        &self,
        broker: &impl CapsuleBrokerClient,
        cell_id: impl Into<String>,
        lease_epoch: u64,
        tenant: TenantId,
        deployment: DeploymentId,
        snapshot_ts: u64,
        prewarm: Vec<DocumentId>,
        module_source: &str,
        function_name: &str,
        args_json: &str,
        invocation: ModuleInvocation,
    ) -> Result<V8ExecutionResult, V8CellError> {
        if tenant != self.tenant {
            return Err(V8CellError::WrongTenant);
        }
        if deployment != self.deployment {
            return Err(V8CellError::WrongDeployment);
        }

        let context = SealContext::new(cell_id, lease_epoch);
        let initial = broker.initial_capsule(&context, tenant, deployment, snapshot_ts, prewarm)?;
        let boxed_state = Box::new(Mutex::new(V8CellState {
            capsule: Some(initial.capsule().clone()),
            traps: VecDeque::new(),
            deny_writes: false,
            consumed: BTreeSet::new(),
            writes: BTreeMap::new(),
        }));
        let state_ptr: *mut Mutex<V8CellState> = Box::into_raw(boxed_state);

        let result = unsafe {
            self.execute_module_with_broker_state_ptr(
                broker,
                &context,
                initial,
                state_ptr,
                module_source,
                function_name,
                args_json,
                invocation,
            )
        };

        let state_box = unsafe { Box::from_raw(state_ptr) };
        match result {
            Ok(mut output) => {
                let state = state_box.lock().expect("v8 state mutex poisoned");
                let hash = state
                    .capsule
                    .as_ref()
                    .map(|capsule| capsule.root_hash)
                    .unwrap_or_default();
                output.capsule_hash = hash;
                output.consumed_reads = state.consumed.iter().cloned().collect();
                output.write_set = state
                    .writes
                    .iter()
                    .map(|(key, document)| (key.clone(), document.clone()))
                    .collect();
                Ok(output)
            }
            Err(error) => Err(error),
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn execute_module_with_broker_state_ptr(
        &self,
        broker: &impl CapsuleBrokerClient,
        context: &SealContext,
        mut sealed_capsule: SealedCapsule,
        state_ptr: *mut Mutex<V8CellState>,
        module_source: &str,
        function_name: &str,
        args_json: &str,
        invocation: ModuleInvocation,
    ) -> Result<V8ExecutionResult, V8CellError> {
        let result = self.execute_module_core(
            state_ptr,
            module_source,
            function_name,
            args_json,
            invocation,
            |key| {
                sealed_capsule =
                    broker.hydrate_point(context, sealed_capsule.clone(), key.clone())?;
                let capsule = sealed_capsule.capsule().clone();
                let state = &*state_ptr;
                state.lock().expect("v8 state mutex poisoned").capsule = Some(capsule);
                Ok(())
            },
        );
        // The evidence object the Commit verb needs is the capsule as
        // finally hydrated — hand it back on the result so the harness
        // commits exactly what this execution observed.
        result.map(|mut output| {
            output.sealed_capsule = Some(sealed_capsule);
            output
        })
    }

    unsafe fn execute_with_broker_state_ptr(
        &self,
        broker: &impl CapsuleBrokerClient,
        context: &SealContext,
        mut sealed_capsule: SealedCapsule,
        state_ptr: *mut Mutex<V8CellState>,
        source: &str,
    ) -> Result<V8ExecutionResult, V8CellError> {
        let result = self.execute_core(state_ptr, source, |key| {
            sealed_capsule = broker.hydrate_point(context, sealed_capsule.clone(), key.clone())?;
            let capsule = sealed_capsule.capsule().clone();
            let state = &*state_ptr;
            state.lock().expect("v8 state mutex poisoned").capsule = Some(capsule);
            Ok(())
        });
        // Same rule as the module path: the final sealed capsule rides the
        // result for the commit half of the transaction.
        result.map(|mut output| {
            output.sealed_capsule = Some(sealed_capsule);
            output
        })
    }

    unsafe fn execute_with_state_ptr(
        &self,
        store: &MvccStore,
        state_ptr: *mut Mutex<V8CellState>,
        source: &str,
    ) -> Result<V8ExecutionResult, V8CellError> {
        self.execute_core(state_ptr, source, |key| {
            let ts = {
                let state = &*state_ptr;
                state
                    .lock()
                    .expect("v8 state mutex poisoned")
                    .capsule
                    .as_ref()
                    .expect("capsule present")
                    .ts
            };
            let value = store.read_at(key, ts);
            let state = &*state_ptr;
            let mut state = state.lock().expect("v8 state mutex poisoned");
            state
                .capsule
                .as_mut()
                .expect("capsule present")
                .hydrate_point(key.clone(), value);
            Ok(())
        })
    }

    unsafe fn execute_core(
        &self,
        state_ptr: *mut Mutex<V8CellState>,
        source: &str,
        mut hydrate: impl FnMut(&DocumentId) -> Result<(), V8CellError>,
    ) -> Result<V8ExecutionResult, V8CellError> {
        let create_params = v8::CreateParams::default();
        let mut isolate = v8::Isolate::new(create_params);
        // Host-controlled continuation: V8 should not decide when to drain
        // Promise jobs. The cell scheduler hydrates traps, resolves exactly one
        // host promise, then explicitly checkpoints microtasks.
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        let mut hs = v8::HandleScope::new(&mut isolate);
        let scope = &mut hs;

        let global = v8::ObjectTemplate::new(scope);
        let external = v8::External::new(scope, state_ptr.cast::<c_void>());

        // `Aster.read(key, field)` — legacy toy API, kept for v0.3-era
        // tests and the `process_boundary` E2E. Not used by Convex apps.
        let read_template = v8::FunctionTemplate::builder(aster_read_callback)
            .data(external.into())
            .build(scope);
        let aster_template = v8::ObjectTemplate::new(scope);
        let read_name = v8::String::new(scope, "read").unwrap();
        aster_template.set(read_name.into(), read_template.into());
        let aster_name = v8::String::new(scope, "Aster").unwrap();
        global.set(aster_name.into(), aster_template.into());

        // `Convex.asyncSyscall(name, argsJson)` — the upstream Convex
        // backend's wire shape, matched verbatim so a Convex-compiled
        // function can `await ctx.db.get(id)` (which expands to
        // `performAsyncSyscall("1.0/get", {id})` in the user's bundle)
        // and the db writer verbs without modification.
        let convex_async_template = v8::FunctionTemplate::builder(convex_async_syscall_callback)
            .data(external.into())
            .build(scope);
        let convex_template = v8::ObjectTemplate::new(scope);
        let async_name = v8::String::new(scope, "asyncSyscall").unwrap();
        convex_template.set(async_name.into(), convex_async_template.into());
        let convex_name = v8::String::new(scope, "Convex").unwrap();
        global.set(convex_name.into(), convex_template.into());

        let context = v8::Context::new(
            scope,
            v8::ContextOptions {
                global_template: Some(global),
                ..Default::default()
            },
        );
        let scope = &mut v8::ContextScope::new(scope, context);

        let source = v8::String::new(scope, source).unwrap();
        let script = v8::Script::compile(scope, source, None)
            .ok_or_else(|| V8CellError::Compile("Script::compile returned None".to_string()))?;
        script
            .run(scope)
            .ok_or_else(|| V8CellError::Run("top-level script threw".to_string()))?;

        let main_name = v8::String::new(scope, "main").unwrap();
        let main_value = context
            .global(scope)
            .get(scope, main_name.into())
            .ok_or_else(|| V8CellError::Run("globalThis.main is missing".to_string()))?;
        let main_fn = v8::Local::<v8::Function>::try_from(main_value)
            .map_err(|_| V8CellError::Run("globalThis.main is not a function".to_string()))?;
        let undefined = v8::undefined(scope).into();
        let promise_value = main_fn.call(scope, undefined, &[]).ok_or_else(|| {
            V8CellError::Run("main() threw before returning a Promise".to_string())
        })?;
        let promise = v8::Local::<v8::Promise>::try_from(promise_value)
            .map_err(|_| V8CellError::NotAPromise)?;
        let promise_global = v8::Global::new(scope, promise);

        // One trap pump serves every entry point since S9b — this loop and
        // the module path used to carry duplicated dispatch arms, which is
        // exactly where the read/write view rules would have drifted.
        let mut traps = 0usize;
        run_trap_pump_loop(
            scope,
            &promise_global,
            state_ptr,
            self.max_traps,
            &mut traps,
            &mut hydrate,
        )?;
        // The pump returns Ok only on Fulfilled (rejections and stuck
        // promises are typed errors), so the result is ready to read.
        let promise = v8::Local::new(scope, &promise_global);
        let value = promise.result(scope);
        let output = v8_value_to_capsule_value(scope, value)?;
        Ok(V8ExecutionResult {
            output,
            traps,
            capsule_hash: 0,
            consumed_reads: Vec::new(),
            write_set: Vec::new(),
            sealed_capsule: None,
        })
    }

    /// ESM-flavoured sibling of `execute_core`. Compiles `module_source`
    /// as a V8 ES module, instantiates + evaluates it, then reads the
    /// requested export and dispatches `invokeQuery` / `invokeMutation`
    /// (per `invocation`) with `args_json` against it. Reuses the same
    /// trap-pump dispatch as `execute_core` so the full
    /// `Convex.asyncSyscall` surface keeps working inside an ESM body.
    ///
    /// The caller (`execute_module_with_broker_state_ptr`) supplies
    /// `hydrate` so the broker (real or in-process) can fulfil read traps
    /// without leaking the store handle into this method.
    unsafe fn execute_module_core(
        &self,
        state_ptr: *mut Mutex<V8CellState>,
        module_source: &str,
        function_name: &str,
        args_json: &str,
        invocation: ModuleInvocation,
        mut hydrate: impl FnMut(&DocumentId) -> Result<(), V8CellError>,
    ) -> Result<V8ExecutionResult, V8CellError> {
        // Arm the query/mutation write gate before any JS runs (review C3):
        // upstream Convex rejects db writes inside queries, and the export
        // marker alone doesn't stop a hand-written `isQuery` export from
        // reaching the mutating syscalls through `invokeQuery`.
        {
            let state = unsafe { &*state_ptr };
            state.lock().expect("v8 state mutex poisoned").deny_writes =
                matches!(invocation, ModuleInvocation::Query);
        }
        let create_params = v8::CreateParams::default();
        let mut isolate = v8::Isolate::new(create_params);
        // Same explicit microtask policy as the legacy path. ESM evaluation
        // queues a microtask for the module's body even when there's no
        // top-level `await`; we drain that with `perform_microtask_checkpoint`
        // inside the trap loop, identical to the legacy entry point.
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        let mut hs = v8::HandleScope::new(&mut isolate);
        let scope = &mut hs;

        let global = v8::ObjectTemplate::new(scope);
        let external = v8::External::new(scope, state_ptr.cast::<c_void>());

        // Aster.read — kept on for parity with the legacy entry point so
        // the same isolate could run either shape. The Convex bundle
        // doesn't reference this global, so leaving it installed is free.
        let read_template = v8::FunctionTemplate::builder(aster_read_callback)
            .data(external.into())
            .build(scope);
        let aster_template = v8::ObjectTemplate::new(scope);
        let read_name = v8::String::new(scope, "read").unwrap();
        aster_template.set(read_name.into(), read_template.into());
        let aster_name = v8::String::new(scope, "Aster").unwrap();
        global.set(aster_name.into(), aster_template.into());

        // The Convex global. ESM bodies are strict-mode but globals still
        // resolve via the realm's global proxy — `Convex.asyncSyscall(...)`
        // inside the bundle hits this template, same as the legacy script
        // path. See /tmp/aster-research-rusty-v8-esm.md §7.
        let convex_template = v8::ObjectTemplate::new(scope);

        // `Convex.asyncSyscall(name, argsJson) -> Promise<string>`. The
        // bundle's `db.get(id)` flow lands here via `performAsyncSyscall`
        // (bundle lines 906–931, /tmp/aster-research-bundle-ground-truth.md §3.1).
        let convex_async_template = v8::FunctionTemplate::builder(convex_async_syscall_callback)
            .data(external.into())
            .build(scope);
        let async_name = v8::String::new(scope, "asyncSyscall").unwrap();
        convex_template.set(async_name.into(), convex_async_template.into());

        // `Convex.syscall(name, argsJson) -> string`. The bundle's
        // `performSyscall` (lines 905–913) lazy-checks
        // `Convex.syscall === void 0` and throws "outside of a Convex backend"
        // ONLY when called. The fixture's `db.get` path uses the async
        // surface only, so the stub is never invoked — it's installed so
        // the surface matches the bundle's expectations and so a future
        // user handler that reaches a sync syscall fails with a clear
        // typed JS error instead of the misleading "outside of a Convex
        // backend" string.
        let convex_sync_template = v8::FunctionTemplate::builder(convex_sync_syscall_stub_callback)
            .data(external.into())
            .build(scope);
        let sync_name = v8::String::new(scope, "syscall").unwrap();
        convex_template.set(sync_name.into(), convex_sync_template.into());

        let convex_name = v8::String::new(scope, "Convex").unwrap();
        global.set(convex_name.into(), convex_template.into());

        let context = v8::Context::new(
            scope,
            v8::ContextOptions {
                global_template: Some(global),
                ..Default::default()
            },
        );
        let scope = &mut v8::ContextScope::new(scope, context);

        // Compile the bundle as ES module. `is_module: true` is the 9th
        // boolean of `ScriptOrigin::new` — easy to miss; without it
        // `compile_module` returns None with a "Cannot use import statement
        // outside a module" error (memo /tmp/aster-research-rusty-v8-esm.md §9).
        let resource_name = v8::String::new(scope, "convex:/messages.js").unwrap();
        let origin = v8::ScriptOrigin::new(
            scope,
            resource_name.into(),
            /* line   */ 0,
            /* col    */ 0,
            /* shared */ false,
            /* script */ -1,
            /* sm url */ None,
            /* opaque */ false,
            /* wasm   */ false,
            /* module */ true,
            None,
        );
        let src_str = v8::String::new(scope, module_source)
            .ok_or_else(|| V8CellError::Compile("module source too large for V8".to_string()))?;
        let mut source = v8::script_compiler::Source::new(src_str, Some(&origin));
        let module = {
            let try_catch = &mut v8::TryCatch::new(scope);
            match v8::script_compiler::compile_module(try_catch, &mut source) {
                Some(m) => m,
                None => {
                    let err = try_catch
                        .exception()
                        .map(|v| value_to_string(try_catch, v))
                        .unwrap_or_else(|| "compile_module returned None".to_string());
                    return Err(V8CellError::Compile(err));
                }
            }
        };

        // Instantiate. The bundle is fully flattened by esbuild
        // (/tmp/aster-research-bundle-ground-truth.md §4 — `grep '^import '`
        // returns zero hits in the 2037-line bundle), so the resolver
        // should never fire. We pass a defensive stub that returns None
        // and surfaces a typed error if a future bundler emits an `import`.
        let ok = {
            let try_catch = &mut v8::TryCatch::new(scope);
            match module.instantiate_module(try_catch, resolve_unexpected_import_callback) {
                Some(b) => b,
                None => {
                    let err = try_catch
                        .exception()
                        .map(|v| value_to_string(try_catch, v))
                        .unwrap_or_else(|| {
                            "instantiate_module threw without exception".to_string()
                        });
                    return Err(V8CellError::Compile(format!(
                        "module instantiate failed: {err}"
                    )));
                }
            }
        };
        if !ok {
            return Err(V8CellError::Compile(
                "module instantiate_module returned false (resolver rejected an import — \
                 the bundle should be fully flattened by esbuild)"
                    .to_string(),
            ));
        }

        // Evaluate. Returns a Promise — even for modules without top-level
        // `await`, V8 wraps the resolution in a Promise for uniformity in
        // this binding. /tmp/aster-research-rusty-v8-esm.md §6.
        let eval_value = {
            let try_catch = &mut v8::TryCatch::new(scope);
            match module.evaluate(try_catch) {
                Some(v) => v,
                None => {
                    let err = try_catch
                        .exception()
                        .map(|v| value_to_string(try_catch, v))
                        .unwrap_or_else(|| "evaluate returned None".to_string());
                    return Err(V8CellError::Run(format!("module evaluate failed: {err}")));
                }
            }
        };
        let eval_promise = v8::Local::<v8::Promise>::try_from(eval_value).map_err(|_| {
            V8CellError::Run("module.evaluate did not return a Promise".to_string())
        })?;
        let eval_promise_global = v8::Global::new(scope, eval_promise);

        // Drive the evaluation promise to fulfilment via the standard
        // trap-pump loop. The bundle has no `await` on a host syscall at
        // top level (the fixture's only async work is inside `invokeQuery`),
        // so this loop typically completes in one microtask checkpoint
        // with zero traps consumed.
        let mut traps = 0usize;
        run_trap_pump_loop(
            scope,
            &eval_promise_global,
            state_ptr,
            self.max_traps,
            &mut traps,
            &mut hydrate,
        )?;
        // Defensive: a Module that hit Errored after Pending → Fulfilled
        // can't happen (V8 transitions Errored before resolving), but
        // assert anyway so we don't read a corrupt namespace.
        if module.get_status() != v8::ModuleStatus::Evaluated {
            return Err(V8CellError::Run(format!(
                "module status after evaluate is {:?}, expected Evaluated",
                module.get_status()
            )));
        }

        // Look up the requested export by name on the namespace object.
        // /tmp/aster-research-convex-udf-runner.md §3 — the runner reads
        // exactly `module.get_module_namespace().to_object(scope).get(name)`.
        let namespace = module
            .get_module_namespace()
            .to_object(scope)
            .ok_or_else(|| V8CellError::Run("module namespace was not an object".to_string()))?;
        let fn_key = v8::String::new(scope, function_name)
            .ok_or_else(|| V8CellError::Run("function name is not valid v8 string".to_string()))?;
        let has = namespace
            .has(scope, fn_key.into())
            .ok_or_else(|| V8CellError::Run("namespace.has threw".to_string()))?;
        if !has {
            // Enumerate available exports for the operator. Better debug
            // signal than "not found" — typo'd `getById` is the most
            // common failure mode here.
            let available = namespace
                .get_property_names(scope, v8::GetPropertyNamesArgsBuilder::default().build())
                .map(|arr| {
                    let mut out = Vec::with_capacity(arr.length() as usize);
                    for i in 0..arr.length() {
                        if let Some(v) = arr.get_index(scope, i) {
                            out.push(value_to_string(scope, v));
                        }
                    }
                    out
                })
                .unwrap_or_default();
            return Err(V8CellError::Run(format!(
                "module export {function_name:?} not found; available exports: {available:?}"
            )));
        }
        let export = namespace
            .get(scope, fn_key.into())
            .ok_or_else(|| V8CellError::Run("namespace.get returned None".to_string()))?;
        let export_obj = export.to_object(scope).ok_or_else(|| {
            V8CellError::Run(format!(
                "module export {function_name:?} is not an object \
                 (Convex query/mutation/action factories produce a callable object \
                 with isQuery/isMutation/isAction props — see registration_impl.ts:400-422)"
            ))
        })?;

        // Validate the export's shape against the invocation mode: each
        // entry point runs exactly its own kind, so a mutation can never
        // ride the query gate (and vice versa) and actions never run.
        let shape = inspect_export_shape(scope, export_obj);
        match (invocation, shape) {
            (ModuleInvocation::Query, ExportShape::Query)
            | (ModuleInvocation::Mutation, ExportShape::Mutation) => {}
            (ModuleInvocation::Query, ExportShape::Mutation) => {
                return Err(V8CellError::Run(format!(
                    "module export {function_name:?} is a mutation; this entry point runs \
                     queries — invoke it through the mutation path \
                     (execute_module_mutation_with_broker), whose writes accumulate in the \
                     in-cell write set for fenced commit (never direct DB write capability)"
                )));
            }
            (ModuleInvocation::Mutation, ExportShape::Query) => {
                return Err(V8CellError::Run(format!(
                    "module export {function_name:?} is a query; this entry point runs \
                     mutations — invoke it through execute_module_query_with_broker"
                )));
            }
            (_, ExportShape::Action) => {
                return Err(V8CellError::Run(format!(
                    "module export {function_name:?} is an action; \
                     Aster cells do not run actions (no fetch/timers surface)"
                )));
            }
            (_, ExportShape::Other) => {
                return Err(V8CellError::Run(format!(
                    "module export {function_name:?} is not a Convex query/mutation/action — \
                     missing isQuery/isMutation/isAction marker"
                )));
            }
        }

        // Read `<export>.invokeQuery` / `.invokeMutation` and call with the
        // args JSON string.
        // /tmp/aster-research-bundle-ground-truth.md §3.3 + memo #3 §3.
        let invoke_key_name = invocation.invoke_key();
        let invoke_key = v8::String::new(scope, invoke_key_name).unwrap();
        let invoke = export_obj.get(scope, invoke_key.into()).ok_or_else(|| {
            V8CellError::Run(format!("export has no {invoke_key_name} property"))
        })?;
        let invoke_fn = v8::Local::<v8::Function>::try_from(invoke).map_err(|_| {
            V8CellError::Run(format!(
                "export {function_name:?} has {invoke_key_name} property but it is not a function"
            ))
        })?;
        let args_v8 = v8::String::new(scope, args_json)
            .ok_or_else(|| V8CellError::Run("args_json too large for v8 string".to_string()))?;

        let invoke_result = {
            let try_catch = &mut v8::TryCatch::new(scope);
            // Per /tmp/aster-research-convex-udf-runner.md §5, upstream
            // calls invoke with `global.into()` as `this`. Match that.
            let global_obj: v8::Local<v8::Value> = context.global(try_catch).into();
            match invoke_fn.call(try_catch, global_obj, &[args_v8.into()]) {
                Some(v) => v,
                None => {
                    let err = try_catch
                        .exception()
                        .map(|v| value_to_string(try_catch, v))
                        .unwrap_or_else(|| "invokeQuery threw without exception".to_string());
                    return Err(V8CellError::Run(format!(
                        "{invoke_key_name}({function_name:?}) threw before returning a \
                         Promise: {err}"
                    )));
                }
            }
        };
        let invoke_promise = v8::Local::<v8::Promise>::try_from(invoke_result)
            .map_err(|_| V8CellError::NotAPromise)?;
        let invoke_promise_global = v8::Global::new(scope, invoke_promise);

        // Drive the invoke promise — this is where the
        // `Convex.asyncSyscall` traps actually fire: `1.0/get` activates
        // the broker hydrate path, and (S9b) the mutation verbs accumulate
        // into the in-cell write set without any broker involvement.
        run_trap_pump_loop(
            scope,
            &invoke_promise_global,
            state_ptr,
            self.max_traps,
            &mut traps,
            &mut hydrate,
        )?;

        // Pull the resolved string. invokeQuery and invokeMutation both
        // return `JSON.stringify(convexToJson(result ?? null))` per
        // registration_impl.js — so the resolved value is always a
        // JSON-encoded ConvexValue string. We return it as Value::Text
        // unchanged; the broker / client decodes via aster-convex-codec.
        let promise = v8::Local::new(scope, &invoke_promise_global);
        let result_value = match promise.state() {
            v8::PromiseState::Fulfilled => promise.result(scope),
            v8::PromiseState::Rejected => {
                let rejected = promise.result(scope);
                return Err(V8CellError::Rejected(value_to_string(scope, rejected)));
            }
            v8::PromiseState::Pending => {
                return Err(V8CellError::PendingWithoutTrap);
            }
        };
        let result_str = if result_value.is_string() {
            value_to_string(scope, result_value)
        } else {
            // Both invoke surfaces are contractually `Promise<string>` —
            // anything else is an upstream contract break.
            return Err(V8CellError::Run(format!(
                "{invoke_key_name} resolved to non-string {:?}",
                value_to_string(scope, result_value)
            )));
        };

        Ok(V8ExecutionResult {
            output: Value::Text(result_str),
            traps,
            capsule_hash: 0,
            consumed_reads: Vec::new(),
            write_set: Vec::new(),
            sealed_capsule: None,
        })
    }
}

/// Inspect the Convex query/mutation/action factory's marker properties.
/// /tmp/aster-research-bundle-ground-truth.md §3.3:
///
/// ```text
/// { isQuery|isMutation|isAction: true,
///   isPublic: true,
///   invokeQuery|invokeMutation|invokeAction: fn,
///   exportArgs, exportReturns, _handler }
/// ```
fn inspect_export_shape(scope: &mut v8::HandleScope, obj: v8::Local<v8::Object>) -> ExportShape {
    fn flag(scope: &mut v8::HandleScope, obj: v8::Local<v8::Object>, key: &str) -> bool {
        let Some(k) = v8::String::new(scope, key) else {
            return false;
        };
        let Some(v) = obj.get(scope, k.into()) else {
            return false;
        };
        v.is_true()
    }
    if flag(scope, obj, "isQuery") {
        ExportShape::Query
    } else if flag(scope, obj, "isMutation") {
        ExportShape::Mutation
    } else if flag(scope, obj, "isAction") {
        ExportShape::Action
    } else {
        ExportShape::Other
    }
}

/// Pump microtasks until `target` settles, draining traps along the way.
/// The single promise driver behind every entry point — legacy scripts,
/// module evaluation, and the invoke dispatch — so the view rules
/// (write set over capsule, consumption on snapshot observation) live in
/// exactly one place.
unsafe fn run_trap_pump_loop(
    scope: &mut v8::HandleScope,
    target: &v8::Global<v8::Promise>,
    state_ptr: *mut Mutex<V8CellState>,
    max_traps: usize,
    traps: &mut usize,
    hydrate: &mut dyn FnMut(&DocumentId) -> Result<(), V8CellError>,
) -> Result<(), V8CellError> {
    // Two counters since the review-C5 fix: `pumped` bounds pump dispatches
    // (the anti-runaway guard must count every parked syscall, warm or not,
    // or a warm re-dispatch loop would pump forever), while the
    // caller-visible `traps` counts broker round trips only — incremented
    // at the hydrate sites, which is the semantics the module header
    // promises and the S10 bench asserts.
    let mut pumped = 0usize;
    loop {
        scope.perform_microtask_checkpoint();
        let promise = v8::Local::new(scope, target);
        match promise.state() {
            v8::PromiseState::Fulfilled => return Ok(()),
            v8::PromiseState::Rejected => {
                let rejected = promise.result(scope);
                return Err(V8CellError::Rejected(value_to_string(scope, rejected)));
            }
            v8::PromiseState::Pending => {
                let pending = {
                    let state = &*state_ptr;
                    state
                        .lock()
                        .expect("v8 state mutex poisoned")
                        .traps
                        .pop_front()
                };
                let Some(pending) = pending else {
                    return Err(V8CellError::PendingWithoutTrap);
                };
                if pumped >= max_traps {
                    return Err(V8CellError::TooManyTraps { limit: max_traps });
                }
                pumped += 1;

                let resolver = v8::Local::new(scope, pending.resolver());
                match &pending {
                    PendingTrap::AsterRead { key, field, .. } => {
                        *traps += 1;
                        hydrate(key)?;
                        let value = {
                            let state = &*state_ptr;
                            let mut state = state.lock().expect("v8 state mutex poisoned");
                            state.legacy_read_after_hydrate(key, field)
                        };
                        let js_value = capsule_value_to_v8(scope, &value);
                        resolver.resolve(scope, js_value).ok_or_else(|| {
                            V8CellError::Run("PromiseResolver::resolve failed".to_string())
                        })?;
                    }
                    PendingTrap::ConvexSyscall {
                        name, args_json, ..
                    } => {
                        // The callback parked this trap because the view
                        // could not answer without the broker (or because
                        // the payload was malformed — the re-dispatch
                        // surfaces that as the same host-level error the
                        // pre-S9b dispatcher raised). Hydrate on demand,
                        // then ask the same dispatcher again: after a
                        // hydration the key is held one way or another,
                        // so a second NeedsHydration is a host bug.
                        let mut answer = {
                            let state = &*state_ptr;
                            let mut state = state.lock().expect("v8 state mutex poisoned");
                            state.dispatch_convex_syscall(name, args_json)?
                        };
                        if let SyscallAnswer::NeedsHydration(key) = &answer {
                            *traps += 1;
                            hydrate(key)?;
                            answer = {
                                let state = &*state_ptr;
                                let mut state = state.lock().expect("v8 state mutex poisoned");
                                state.dispatch_convex_syscall(name, args_json)?
                            };
                        }
                        match answer {
                            SyscallAnswer::Resolve(json_str) => {
                                let js_str = v8::String::new(scope, &json_str)
                                    .unwrap_or_else(|| v8::String::empty(scope));
                                resolver.resolve(scope, js_str.into()).ok_or_else(|| {
                                    V8CellError::Run("PromiseResolver::resolve failed".to_string())
                                })?;
                            }
                            SyscallAnswer::Reject(message) => {
                                let v8_msg = v8::String::new(scope, &message)
                                    .unwrap_or_else(|| v8::String::empty(scope));
                                let err = v8::Exception::error(scope, v8_msg);
                                resolver.reject(scope, err).ok_or_else(|| {
                                    V8CellError::Run("PromiseResolver::reject failed".to_string())
                                })?;
                            }
                            SyscallAnswer::NeedsHydration(key) => {
                                return Err(V8CellError::Run(format!(
                                    "hydrate did not install an entry for {:?} — \
                                     broker hydration must answer every key, absence included",
                                    key.0
                                )));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Defensive resolver used when the bundle ought to be self-contained.
/// `extern "C" fn` cannot capture (memo /tmp/aster-research-rusty-v8-esm.md §9),
/// so we can't smuggle host state in here; instead we throw a JS error
/// describing what happened so the operator sees a clear failure mode.
/// Returns `None` to signal resolution failure to V8.
fn resolve_unexpected_import_callback<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _import_attrs: v8::Local<'s, v8::FixedArray>,
    _referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    // SAFETY: V8 invoked us inside `instantiate_module` on this isolate.
    let scope = &mut unsafe { v8::CallbackScope::new(context) };
    let spec = specifier.to_rust_string_lossy(scope);
    let msg = format!(
        "aster-v8cell v0.5: bundle has unexpected `import {spec:?}` — \
         the cell expects a fully flattened esbuild output \
         (see /tmp/aster-research-bundle-ground-truth.md §4)"
    );
    let v8_msg = v8::String::new(scope, &msg).unwrap();
    let err = v8::Exception::error(scope, v8_msg);
    scope.throw_exception(err);
    None
}

fn aster_read_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let Some(external) = v8::Local::<v8::External>::try_from(args.data()).ok() else {
        throw(scope, "Aster.read missing host state");
        return;
    };
    let state_ptr = external.value() as *mut Mutex<V8CellState>;
    if state_ptr.is_null() {
        throw(scope, "Aster.read null host state");
        return;
    }

    let key = match args.get(0).to_string(scope) {
        Some(value) => value.to_rust_string_lossy(scope),
        None => {
            throw(scope, "Aster.read key must be string-like");
            return;
        }
    };
    let field = match args.get(1).to_string(scope) {
        Some(value) => value.to_rust_string_lossy(scope),
        None => {
            throw(scope, "Aster.read field must be string-like");
            return;
        }
    };
    let key = DocumentId::new(key);

    let state = unsafe { &*state_ptr };
    let warm_value = {
        let mut state = state.lock().expect("v8 state mutex poisoned");
        // Warm hits are consumption too — the ledger tracks what the JS
        // observed, not what trapped. Own-writes answer un-ledgered; see
        // the ORDERING RULE on `V8CellState::writes`.
        state.legacy_warm_read(&key, &field)
    };
    if let Some(value) = warm_value {
        let value = capsule_value_to_v8(scope, &value);
        rv.set(value);
        return;
    }

    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        throw(scope, "failed to allocate V8 PromiseResolver");
        return;
    };
    let promise = resolver.get_promise(scope);
    let resolver_global = v8::Global::new(scope, resolver);
    state
        .lock()
        .expect("v8 state mutex poisoned")
        .traps
        .push_back(PendingTrap::AsterRead {
            key,
            field,
            resolver: resolver_global,
        });
    rv.set(promise.into());
}

/// JS callback for `Convex.asyncSyscall(name, args_json_string)`. Mirrors
/// the upstream backend's wire shape so a Convex-compiled module can call
/// `db.get(id)` (which expands to `performAsyncSyscall("1.0/get", ...)`)
/// against an Aster cell without modification.
///
/// One dispatch (`V8CellState::dispatch_convex_syscall`) decides what
/// happens: reads answered by the invocation view (warm capsule hits —
/// bench 2026-07-15 bug (b) + referee F7 — and read-your-own-writes) and
/// every mutation verb (S9b: `1.0/insert`, `1.0/shallowMerge`,
/// `1.0/replace`, `1.0/remove` accumulate in the in-cell write set)
/// resolve right here with an already-settled Promise — no `PendingTrap`,
/// no broker round trip. Semantic failures (unsupported syscall names,
/// mutations of nonexistent documents, insert without an id) reject the
/// Promise with a catchable JS `Error`. Only a syscall the view cannot
/// answer — a read of an unheld key, including the implicit base read of
/// the mutation verbs — parks a `PendingTrap` for the host loop to
/// hydrate; malformed payloads park too, so they surface as the same
/// host-level typed error the pre-S9b dispatcher raised.
fn convex_async_syscall_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let Some(external) = v8::Local::<v8::External>::try_from(args.data()).ok() else {
        throw(scope, "Convex.asyncSyscall missing host state");
        return;
    };
    let state_ptr = external.value() as *mut Mutex<V8CellState>;
    if state_ptr.is_null() {
        throw(scope, "Convex.asyncSyscall null host state");
        return;
    }

    let name = match args.get(0).to_string(scope) {
        Some(value) => value.to_rust_string_lossy(scope),
        None => {
            throw(scope, "Convex.asyncSyscall name must be string-like");
            return;
        }
    };
    // Convex's JS shim sends the args object as a stringified JSON. The
    // Aster path also accepts a raw JS object — convert with
    // JSON.stringify on the host side via v8's `to_string`.
    let args_json = match args.get(1).to_string(scope) {
        Some(value) => value.to_rust_string_lossy(scope),
        None => {
            throw(
                scope,
                "Convex.asyncSyscall args must be string-like or stringifiable",
            );
            return;
        }
    };

    let state = unsafe { &*state_ptr };
    let answer = {
        let mut state = state.lock().expect("v8 state mutex poisoned");
        state.dispatch_convex_syscall(&name, &args_json)
    };

    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        throw(scope, "failed to allocate V8 PromiseResolver");
        return;
    };
    let promise = resolver.get_promise(scope);
    match answer {
        Ok(SyscallAnswer::Resolve(json_str)) => {
            let js_str =
                v8::String::new(scope, &json_str).unwrap_or_else(|| v8::String::empty(scope));
            if resolver.resolve(scope, js_str.into()).is_none() {
                throw(scope, "PromiseResolver::resolve failed");
                return;
            }
        }
        Ok(SyscallAnswer::Reject(message)) => {
            let v8_msg =
                v8::String::new(scope, &message).unwrap_or_else(|| v8::String::empty(scope));
            let err = v8::Exception::error(scope, v8_msg);
            if resolver.reject(scope, err).is_none() {
                throw(scope, "PromiseResolver::reject failed");
                return;
            }
        }
        Ok(SyscallAnswer::NeedsHydration(_)) | Err(_) => {
            let resolver_global = v8::Global::new(scope, resolver);
            state
                .lock()
                .expect("v8 state mutex poisoned")
                .traps
                .push_back(PendingTrap::ConvexSyscall {
                    name,
                    args_json,
                    resolver: resolver_global,
                });
        }
    }
    rv.set(promise.into());
}

/// JS callback for `Convex.syscall(name, argsJson)`. The bundle's
/// `performSyscall` (lines 905–913 of /tmp/aster-research/bundle-out/isolate/messages.js)
/// lazy-checks `Convex.syscall === void 0` and only then throws — so an
/// installed-but-throwing function is observably the same as "feature
/// unsupported, here's a clear error" from the user-handler's POV. The
/// fixture's `db.get` path doesn't reach this; it's installed for
/// surface-completeness and so a future user handler that does call
/// `performSyscall` gets a typed JS exception rather than the bundle's
/// misleading default message about "outside of a Convex backend".
fn convex_sync_syscall_stub_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let name = match args.get(0).to_string(scope) {
        Some(value) => value.to_rust_string_lossy(scope),
        None => "<unstringifiable>".to_string(),
    };
    let msg = format!(
        "aster-v8cell: synchronous Convex.syscall({name:?}) is not implemented — \
         only the async Convex.asyncSyscall surface (1.0/get + the S9b mutation \
         verbs) is wired in this cell"
    );
    throw(scope, &msg);
}

/// Extract `id` from a `Convex.asyncSyscall("1.0/get", argsJson)` payload.
/// Convex's JS shim sends this as `JSON.stringify({ id, isSystem, ...})`;
/// we only care about `id`. The broker accepts either Aster's
/// `<table_hex>/<id_hex>` wire form or a Convex IDv6 string.
fn parse_get_id(args_json: &str) -> Result<String, V8CellError> {
    let v: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|err| V8CellError::Run(format!("convex 1.0/get bad JSON args: {err}")))?;
    let id = v
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            V8CellError::Run("convex 1.0/get: missing string field `id` in args".to_string())
        })?
        .to_string();
    Ok(id)
}

fn throw(scope: &mut v8::HandleScope, message: &str) {
    let message = v8::String::new(scope, message).unwrap();
    let error = v8::Exception::error(scope, message);
    scope.throw_exception(error);
}

fn capsule_value_to_v8<'s>(
    scope: &mut v8::HandleScope<'s>,
    value: &Value,
) -> v8::Local<'s, v8::Value> {
    match value {
        Value::Int(value) => v8::Number::new(scope, *value as f64).into(),
        Value::Text(value) => v8::String::new(scope, value).unwrap().into(),
        Value::Bool(value) => v8::Boolean::new(scope, *value).into(),
        Value::Null => v8::null(scope).into(),
    }
}

fn v8_value_to_capsule_value(
    scope: &mut v8::HandleScope,
    value: v8::Local<v8::Value>,
) -> Result<Value, V8CellError> {
    if value.is_int32() {
        Ok(Value::Int(
            value.int32_value(scope).unwrap_or_default() as i64
        ))
    } else if value.is_number() {
        Ok(Value::Int(
            value.number_value(scope).unwrap_or_default() as i64
        ))
    } else if value.is_boolean() {
        Ok(Value::Bool(value.boolean_value(scope)))
    } else if value.is_string() {
        Ok(Value::Text(value_to_string(scope, value)))
    } else if value.is_null_or_undefined() {
        Ok(Value::Null)
    } else {
        Err(V8CellError::UnsupportedValue(value_to_string(scope, value)))
    }
}

fn value_to_string(scope: &mut v8::HandleScope, value: v8::Local<v8::Value>) -> String {
    value
        .to_string(scope)
        .map(|value| value.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "<unprintable>".to_string())
}

/// Helper used by tests and future host integration. It keeps this crate's
/// public surface free of V8 types.
pub fn document_from_i64(field: &str, value: i64) -> Document {
    let mut document = BTreeMap::new();
    document.insert(field.to_string(), Value::Int(value));
    document
}

#[cfg(test)]
mod tests {
    use super::*;
    use aster_capsule::doc_with_i64;

    /// V8 isolates on parallel test-harness threads segfault intermittently
    /// (SIGSEGV, roughly 1 in 3 full-suite runs). Production runs a single
    /// isolate per cell process, so serializing is a harness-only concern.
    fn v8_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static V8_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        V8_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Review C3: a query invocation must never reach the mutating
    /// syscalls — upstream Convex rejects db writes inside queries.
    #[test]
    fn query_invocation_write_gate_rejects_mutation_syscalls() {
        let mut state = V8CellState {
            deny_writes: true,
            ..Default::default()
        };
        for (name, args) in [
            ("1.0/insert", r#"{"table":"docs","value":{"_id":"docs/aa"}}"#),
            ("1.0/shallowMerge", r#"{"id":"docs/aa","value":{}}"#),
            ("1.0/replace", r#"{"id":"docs/aa","value":{}}"#),
            ("1.0/remove", r#"{"id":"docs/aa"}"#),
        ] {
            match state.dispatch_convex_syscall(name, args) {
                Ok(SyscallAnswer::Reject(message)) => {
                    assert!(message.contains("query invocation"), "{name}: {message}");
                }
                other => panic!("{name} must die at the write gate, got {other:?}"),
            }
        }
        // The gate is write-only: a read passes through untouched.
        let get = state.dispatch_convex_syscall("1.0/get", r#"{"id":"docs/aa"}"#);
        assert!(
            !matches!(&get, Ok(SyscallAnswer::Reject(message)) if message.contains("query invocation")),
            "reads must not be write-gated: {get:?}"
        );
    }

    /// Review C2 sentinel: a PRESENT but non-object `_raw` envelope is an
    /// error — merging from an empty base would let a patch silently
    /// replace the whole document.
    #[test]
    fn malformed_raw_envelope_is_an_error_not_an_empty_base() {
        let mut doc = Document::new();
        doc.insert("_raw".to_string(), Value::Text("{not json".to_string()));
        assert!(raw_json_object(&doc).is_err());
        // Raw-less docs (legacy hand-seeded fixtures) still merge from an
        // empty base — that keeps the toy paths runnable.
        let clean = doc_with_i64("n", 1);
        assert!(raw_json_object(&clean).expect("raw-less doc").is_empty());
    }

    /// Review C7: a held-but-valueless entry (hydrated absence) is WARM
    /// for the legacy read — it answers Null, consumes, and never re-traps.
    #[test]
    fn legacy_read_of_held_absence_is_warm_and_consumed() {
        let mut capsule = SnapshotCapsule::empty(
            TenantId::new("tenant-a"),
            DeploymentId::new("dep-a"),
            7,
        );
        let key = DocumentId::new("k/absent");
        capsule.hydrate_point(key.clone(), aster_capsule::VersionedDocument::missing());
        let mut state = V8CellState {
            capsule: Some(capsule),
            ..Default::default()
        };
        assert_eq!(state.legacy_warm_read(&key, "f"), Some(Value::Null));
        assert!(
            state.consumed.contains(&key),
            "absence read must be ledgered"
        );
    }

    #[test]
    fn v8_async_function_resumes_through_broker_without_store_handle() {
        let _v8 = v8_test_guard();
        let tenant = TenantId::new("tenant-v8-broker");
        let deployment = DeploymentId::new("dep-v8-broker");
        let store = MvccStore::new();
        store.seed(DocumentId::new("counters/a"), doc_with_i64("value", 20));
        store.seed(DocumentId::new("counters/b"), doc_with_i64("value", 22));
        let ts = store.snapshot_ts();
        let broker = aster_broker::LocalCapsuleBroker::new(
            &store,
            aster_capsule::CapsuleSealKey::derive_for_tests(b"v8-broker-test"),
        );

        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              const a = await Aster.read("counters/a", "value");
              const b = await Aster.read("counters/b", "value");
              return a + b;
            }
        "#;
        let result = cell
            .execute_async_main_with_broker(
                &broker,
                "cell-v8-1",
                3,
                tenant,
                deployment,
                ts,
                vec![DocumentId::new("counters/a")],
                source,
            )
            .expect("V8 async function should complete through broker");
        assert_eq!(result.output, Value::Int(42));
        assert_eq!(result.traps, 1);
        assert_ne!(result.capsule_hash, 0);
    }

    #[test]
    fn v8_async_function_resumes_after_read_trap() {
        let _v8 = v8_test_guard();
        let tenant = TenantId::new("tenant-v8");
        let deployment = DeploymentId::new("dep-v8");
        let store = MvccStore::new();
        store.seed(DocumentId::new("counters/a"), doc_with_i64("value", 40));
        store.seed(DocumentId::new("counters/b"), doc_with_i64("value", 2));
        let ts = store.snapshot_ts();

        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              const a = await Aster.read("counters/a", "value");
              const b = await Aster.read("counters/b", "value");
              return a + b;
            }
        "#;
        let result = cell
            .execute_async_main(
                &store,
                tenant,
                deployment,
                ts,
                vec![DocumentId::new("counters/a")],
                source,
            )
            .expect("V8 async function should complete");
        assert_eq!(result.output, Value::Int(42));
        assert_eq!(result.traps, 1);
        assert_ne!(result.capsule_hash, 0);
    }

    #[test]
    fn v8_convex_async_syscall_get_returns_doc_raw_as_json() {
        let _v8 = v8_test_guard();
        // Build a doc whose `_raw` field carries a JSON blob the way
        // PostgresCapsuleStore would after reading from convex.documents.
        // The JS function fires `Convex.asyncSyscall("1.0/get", ...)`,
        // gets the raw JSON string back, parses it, and returns one
        // field — proving the wire shape matches what a Convex-compiled
        // module would do via `await ctx.db.get(id)`.
        let tenant = TenantId::new("tenant-convex");
        let deployment = DeploymentId::new("dep-convex");
        let store = MvccStore::new();
        let doc_id = DocumentId::new("aabb/ccdd");
        let mut doc = aster_capsule::Document::new();
        doc.insert(
            "_raw".to_string(),
            Value::Text(r#"{"name":"ian","_id":"aabb/ccdd"}"#.to_string()),
        );
        store.seed(doc_id.clone(), doc);
        let ts = store.snapshot_ts();

        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              const json = await Convex.asyncSyscall(
                "1.0/get",
                JSON.stringify({ id: "aabb/ccdd", isSystem: false })
              );
              const doc = JSON.parse(json);
              return doc.name;
            }
        "#;
        let result = cell
            .execute_async_main(&store, tenant, deployment, ts, vec![], source)
            .expect("Convex.asyncSyscall(1.0/get) should complete");
        assert_eq!(result.output, Value::Text("ian".to_string()));
        assert_eq!(result.traps, 1, "exactly one async syscall trap");
        assert_eq!(
            result.consumed_reads,
            vec![doc_id],
            "the post-trap read must land in the consumption ledger"
        );
    }

    /// Bench 2026-07-15 bug (b): 200 reads of the same id measured 200 broker
    /// traps because the real Convex syscall path had no warm check. With the
    /// warm hit, the first get traps and hydrates; the other 199 serve from
    /// the sealed capsule. `max_traps: 8` makes the assertion structural —
    /// the old behavior dies with `TooManyTraps` long before finishing.
    #[test]
    fn v8_convex_repeated_get_same_key_traps_once() {
        let _v8 = v8_test_guard();
        let tenant = TenantId::new("tenant-warm");
        let deployment = DeploymentId::new("dep-warm");
        let store = MvccStore::new();
        let key = DocumentId::new("docs/hot");
        let mut doc = aster_capsule::Document::new();
        doc.insert(
            "_raw".to_string(),
            Value::Text(r#"{"n":7,"_id":"docs/hot"}"#.to_string()),
        );
        store.seed(key.clone(), doc);
        let ts = store.snapshot_ts();
        let broker = aster_broker::LocalCapsuleBroker::new(
            &store,
            aster_capsule::CapsuleSealKey::derive_for_tests(b"v8-warm-test"),
        );

        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              let sum = 0;
              for (let i = 0; i < 200; i++) {
                const json = await Convex.asyncSyscall(
                  "1.0/get",
                  JSON.stringify({ id: "docs/hot", isSystem: false })
                );
                const doc = JSON.parse(json);
                if (doc.n !== 7) throw new Error("read " + i + " saw " + json);
                sum += doc.n;
              }
              return sum;
            }
        "#;
        let result = cell
            .execute_async_main_with_broker(
                &broker,
                "cell-warm",
                1,
                tenant,
                deployment,
                ts,
                vec![],
                source,
            )
            .expect("200 warm reads of one key should complete");
        assert_eq!(result.output, Value::Int(200 * 7));
        assert_eq!(
            result.traps, 1,
            "only the first get may trap; the rest are warm hits"
        );
        assert_eq!(
            result.consumed_reads,
            vec![key],
            "200 reads of one id are one consumed dependency"
        );
    }

    /// A prewarmed key never traps — the dream-capsule enabler. The
    /// observation still lands in the ledger: consumption tracks what the
    /// JS saw, not what crossed the broker boundary.
    #[test]
    fn v8_convex_prewarmed_get_zero_traps_and_ledgered() {
        let _v8 = v8_test_guard();
        let tenant = TenantId::new("tenant-prewarm");
        let deployment = DeploymentId::new("dep-prewarm");
        let store = MvccStore::new();
        let key = DocumentId::new("docs/pre");
        let mut doc = aster_capsule::Document::new();
        doc.insert(
            "_raw".to_string(),
            Value::Text(r#"{"n":3,"_id":"docs/pre"}"#.to_string()),
        );
        store.seed(key.clone(), doc);
        let ts = store.snapshot_ts();
        let broker = aster_broker::LocalCapsuleBroker::new(
            &store,
            aster_capsule::CapsuleSealKey::derive_for_tests(b"v8-prewarm-test"),
        );

        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              const doc = JSON.parse(await Convex.asyncSyscall(
                "1.0/get",
                JSON.stringify({ id: "docs/pre" })
              ));
              return doc.n;
            }
        "#;
        let result = cell
            .execute_async_main_with_broker(
                &broker,
                "cell-prewarm",
                1,
                tenant,
                deployment,
                ts,
                vec![key.clone()],
                source,
            )
            .expect("prewarmed get should complete");
        assert_eq!(result.output, Value::Int(3));
        assert_eq!(result.traps, 0, "prewarmed key must not trap");
        assert_eq!(result.consumed_reads, vec![key]);
    }

    /// The ledger is exactly the distinct keys the JS observed, in
    /// deterministic (BTreeSet) order: a live doc read twice, an absent key
    /// read twice (the absence read is a theorem obligation — an absence
    /// flip must conflict at commit), and NOT the untouched prewarm entry.
    /// The second absent read also proves hydrated-absent entries are warm:
    /// two reads of the missing key cost one trap.
    #[test]
    fn v8_consumption_ledger_tracks_observed_keys_including_absence() {
        let _v8 = v8_test_guard();
        let tenant = TenantId::new("tenant-ledger");
        let deployment = DeploymentId::new("dep-ledger");
        let store = MvccStore::new();
        let key_a = DocumentId::new("docs/a");
        let key_absent = DocumentId::new("docs/b-absent");
        let key_prewarm = DocumentId::new("docs/c-prewarm");
        let mut doc = aster_capsule::Document::new();
        doc.insert(
            "_raw".to_string(),
            Value::Text(r#"{"n":21,"_id":"docs/a"}"#.to_string()),
        );
        store.seed(key_a.clone(), doc);
        store.seed(key_prewarm.clone(), doc_with_i64("value", 9));
        let ts = store.snapshot_ts();
        let broker = aster_broker::LocalCapsuleBroker::new(
            &store,
            aster_capsule::CapsuleSealKey::derive_for_tests(b"v8-ledger-test"),
        );

        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              const get = (id) =>
                Convex.asyncSyscall("1.0/get", JSON.stringify({ id }));
              const a1 = JSON.parse(await get("docs/a"));
              const missing1 = JSON.parse(await get("docs/b-absent"));
              const missing2 = JSON.parse(await get("docs/b-absent"));
              const a2 = JSON.parse(await get("docs/a"));
              if (missing1 !== null || missing2 !== null) {
                throw new Error("absent key must read null");
              }
              if (a1.n !== a2.n) throw new Error("warm re-read changed the value");
              return a1.n + a2.n;
            }
        "#;
        let result = cell
            .execute_async_main_with_broker(
                &broker,
                "cell-ledger",
                1,
                tenant,
                deployment,
                ts,
                vec![key_prewarm],
                source,
            )
            .expect("ledger scenario should complete");
        assert_eq!(result.output, Value::Int(42));
        assert_eq!(
            result.traps, 2,
            "one trap per distinct cold key; re-reads (present AND absent) are warm"
        );
        assert_eq!(
            result.consumed_reads,
            vec![key_a, key_absent],
            "ledger = distinct observed keys incl. the absence read, minus unused prewarm"
        );
    }

    /// A tombstone is a warm entry too: it pins "deleted as of the snapshot",
    /// which is exactly what a broker trap would re-answer. Zero traps, and
    /// the absence observation is ledgered.
    #[test]
    fn v8_convex_tombstone_prewarm_is_warm_and_ledgered() {
        let _v8 = v8_test_guard();
        use aster_capsule::{ReadSet, WriteSet};

        let tenant = TenantId::new("tenant-tomb");
        let deployment = DeploymentId::new("dep-tomb");
        let store = MvccStore::new();
        let key = DocumentId::new("docs/dead");
        store.seed(key.clone(), doc_with_i64("value", 1));
        let mut deletes = WriteSet::default();
        deletes.delete(key.clone());
        store
            .commit(store.snapshot_ts(), &ReadSet::default(), &deletes)
            .expect("delete commit");
        let ts = store.snapshot_ts();

        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              const doc = JSON.parse(await Convex.asyncSyscall(
                "1.0/get",
                JSON.stringify({ id: "docs/dead" })
              ));
              return doc === null;
            }
        "#;
        let result = cell
            .execute_async_main(&store, tenant, deployment, ts, vec![key.clone()], source)
            .expect("tombstone get should complete");
        assert_eq!(result.output, Value::Bool(true));
        assert_eq!(result.traps, 0, "tombstoned prewarm entry must not trap");
        assert_eq!(result.consumed_reads, vec![key]);
    }

    #[test]
    fn v8_convex_async_syscall_unsupported_name_rejects_promise() {
        let _v8 = v8_test_guard();
        // Anything other than the wired syscall names becomes a
        // typed JS rejection. The cell scheduler still completes
        // (V8 propagates the rejection to top-level main) — we observe
        // the rejection as a Run/Rejected V8CellError on the host side.
        let tenant = TenantId::new("tenant-rej");
        let deployment = DeploymentId::new("dep-rej");
        let store = MvccStore::new();
        let ts = store.snapshot_ts();
        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              return await Convex.asyncSyscall("1.0/totally-fake", "{}");
            }
        "#;
        let err = cell
            .execute_async_main(&store, tenant, deployment, ts, vec![], source)
            .expect_err("unsupported syscall must reject");
        match err {
            V8CellError::Rejected(msg) => {
                assert!(
                    msg.contains("unsupported convex syscall"),
                    "rejection should name the syscall, got {msg:?}"
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // S9b — mutation syscalls: the write set is born in the cell.
    // ------------------------------------------------------------------

    /// Seed a doc the way the Postgres adapter shapes them: `_raw` carries
    /// the upstream JSON envelope the Convex syscall surface serves.
    fn seed_raw(store: &MvccStore, key: &str, raw_json: &str) -> DocumentId {
        let id = DocumentId::new(key);
        let mut doc = aster_capsule::Document::new();
        doc.insert("_raw".to_string(), Value::Text(raw_json.to_string()));
        store.seed(id.clone(), doc);
        id
    }

    /// Parse the `_raw` envelope of a write-set put for structural asserts.
    fn write_raw(result: &V8ExecutionResult, key: &str) -> serde_json::Value {
        let entry = result
            .write_set
            .iter()
            .find(|(k, _)| k.0 == key)
            .unwrap_or_else(|| panic!("write set has no entry for {key}: {:?}", result.write_set));
        let doc = entry
            .1
            .as_ref()
            .unwrap_or_else(|| panic!("{key} is a tombstone, expected a put"));
        match doc.get("_raw") {
            Some(Value::Text(raw)) => serde_json::from_str(raw).expect("write _raw is JSON"),
            other => panic!("write doc lacks a _raw text envelope, got {other:?}"),
        }
    }

    /// `db.insert` (with an explicit `_id` — the prototype's supported
    /// shape) accumulates in the write set without any broker traffic, and
    /// a `1.0/get` of the key answers the written value with no trap and
    /// no consumption: the value observed came from the transaction
    /// itself, not the snapshot (ORDERING RULE).
    #[test]
    fn v8_convex_insert_accumulates_write_and_serves_read_your_own_write() {
        let _v8 = v8_test_guard();
        let tenant = TenantId::new("tenant-ins");
        let deployment = DeploymentId::new("dep-ins");
        let store = MvccStore::new();
        let ts = store.snapshot_ts();
        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              const syscall = async (name, args) =>
                JSON.parse(await Convex.asyncSyscall(name, JSON.stringify(args)));
              const minted = await syscall("1.0/insert", {
                table: "docs",
                value: { _id: "docs/n1", n: 7 },
              });
              const doc = await syscall("1.0/get", { id: "docs/n1" });
              return minted._id === "docs/n1" && doc.n === 7;
            }
        "#;
        let result = cell
            .execute_async_main(&store, tenant, deployment, ts, vec![], source)
            .expect("insert + read-your-own-write should complete");
        assert_eq!(result.output, Value::Bool(true));
        assert_eq!(
            result.traps, 0,
            "neither the insert nor the own-write get may reach the broker"
        );
        assert!(
            result.consumed_reads.is_empty(),
            "a post-write read of the written key is not a snapshot dependency: {:?}",
            result.consumed_reads
        );
        assert_eq!(result.write_set.len(), 1);
        assert_eq!(
            write_raw(&result, "docs/n1"),
            serde_json::json!({ "_id": "docs/n1", "n": 7 })
        );
    }

    /// `db.delete` produces a tombstone (`None`) write; the removed doc's
    /// JSON is the syscall's resolution (upstream returns it), and a get
    /// after the delete reads null from the write set without another
    /// trap. The pre-delete get stays in the ledger — a read that happened
    /// before the write still counts.
    #[test]
    fn v8_convex_delete_after_get_is_tombstone_write() {
        let _v8 = v8_test_guard();
        let tenant = TenantId::new("tenant-del");
        let deployment = DeploymentId::new("dep-del");
        let store = MvccStore::new();
        let key = seed_raw(&store, "docs/d1", r#"{"_id":"docs/d1","n":1}"#);
        let ts = store.snapshot_ts();
        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              const syscall = async (name, args) =>
                JSON.parse(await Convex.asyncSyscall(name, JSON.stringify(args)));
              const before = await syscall("1.0/get", { id: "docs/d1" });
              const removed = await syscall("1.0/remove", { id: "docs/d1" });
              const after = await syscall("1.0/get", { id: "docs/d1" });
              return before.n === 1 && removed.n === 1 && after === null;
            }
        "#;
        let result = cell
            .execute_async_main(&store, tenant, deployment, ts, vec![], source)
            .expect("get + delete + get should complete");
        assert_eq!(result.output, Value::Bool(true));
        assert_eq!(result.traps, 1, "only the first get hydrates");
        assert_eq!(
            result.consumed_reads,
            vec![key.clone()],
            "the pre-delete get is a snapshot dependency and stays ledgered"
        );
        assert_eq!(
            result.write_set,
            vec![(key, None)],
            "delete must record a tombstone write"
        );
    }

    /// The ordering rule end to end: a key read BEFORE any write lands in
    /// the ledger; a key first observed AFTER the invocation wrote it does
    /// not — even across repeated reads — so the declared set carries
    /// exactly the snapshot dependencies.
    #[test]
    fn v8_consumed_reads_unaffected_by_post_write_reads() {
        let _v8 = v8_test_guard();
        let tenant = TenantId::new("tenant-ord");
        let deployment = DeploymentId::new("dep-ord");
        let store = MvccStore::new();
        let key_a = seed_raw(&store, "docs/a", r#"{"_id":"docs/a","n":1}"#);
        let ts = store.snapshot_ts();
        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              const syscall = async (name, args) =>
                JSON.parse(await Convex.asyncSyscall(name, JSON.stringify(args)));
              const a = await syscall("1.0/get", { id: "docs/a" });        // snapshot read
              await syscall("1.0/insert", {
                table: "docs",
                value: { _id: "docs/b", n: a.n + 1 },
              });
              const b1 = await syscall("1.0/get", { id: "docs/b" });       // own write
              const b2 = await syscall("1.0/get", { id: "docs/b" });       // own write again
              const a2 = await syscall("1.0/get", { id: "docs/a" });       // warm snapshot re-read
              return a.n + b1.n + b2.n + a2.n;
            }
        "#;
        let result = cell
            .execute_async_main(&store, tenant, deployment, ts, vec![], source)
            .expect("ordering-rule scenario should complete");
        assert_eq!(result.output, Value::Int(1 + 2 + 2 + 1));
        assert_eq!(result.traps, 1, "docs/a hydrates once; docs/b never traps");
        assert_eq!(
            result.consumed_reads,
            vec![key_a],
            "only the snapshot read is a dependency; post-write reads of docs/b record nothing"
        );
        assert_eq!(result.write_set.len(), 1);
        assert_eq!(
            write_raw(&result, "docs/b"),
            serde_json::json!({ "_id": "docs/b", "n": 2 })
        );
    }

    /// `db.patch` (`1.0/shallowMerge`) without a prior get: the implicit
    /// base read traps + hydrates + consumes (the merge depends on the
    /// snapshot state), fields merge shallowly, and the
    /// `{"$undefined": null}` marker removes a field, mirroring
    /// `patchValueToJson`.
    #[test]
    fn v8_convex_patch_merges_over_capsule_consumes_base_and_removes_fields() {
        let _v8 = v8_test_guard();
        let tenant = TenantId::new("tenant-patch");
        let deployment = DeploymentId::new("dep-patch");
        let store = MvccStore::new();
        let key = seed_raw(&store, "docs/p", r#"{"_id":"docs/p","a":1,"b":2}"#);
        let ts = store.snapshot_ts();
        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              const syscall = async (name, args) =>
                JSON.parse(await Convex.asyncSyscall(name, JSON.stringify(args)));
              const merged = await syscall("1.0/shallowMerge", {
                id: "docs/p",
                value: { b: 9, c: 3, a: { $undefined: null } },
              });
              const seen = await syscall("1.0/get", { id: "docs/p" });
              return JSON.stringify(merged) === JSON.stringify(seen)
                ? JSON.stringify(seen)
                : "merge and view disagree";
            }
        "#;
        let result = cell
            .execute_async_main(&store, tenant, deployment, ts, vec![], source)
            .expect("patch without prior get should complete");
        let output_json: serde_json::Value = match &result.output {
            Value::Text(s) => serde_json::from_str(s).expect("output is JSON"),
            other => panic!("expected Text output, got {other:?}"),
        };
        assert_eq!(
            output_json,
            serde_json::json!({ "_id": "docs/p", "b": 9, "c": 3 }),
            "shallow merge overwrites b, adds c, and $undefined removes a"
        );
        assert_eq!(result.traps, 1, "the patch's implicit base read hydrates once");
        assert_eq!(
            result.consumed_reads,
            vec![key],
            "the merge base is a snapshot observation and must be declared"
        );
        assert_eq!(write_raw(&result, "docs/p"), output_json);
    }

    /// Patch of a document that does not exist is a structured, catchable
    /// JS error (mirroring upstream Convex), and the absence it observed
    /// is still a consumed read: had the doc existed, the transaction
    /// would have behaved differently, so an absence flip must conflict.
    #[test]
    fn v8_convex_patch_nonexistent_rejects_catchably_and_consumes_absence() {
        let _v8 = v8_test_guard();
        let tenant = TenantId::new("tenant-ghost");
        let deployment = DeploymentId::new("dep-ghost");
        let store = MvccStore::new();
        let ts = store.snapshot_ts();
        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              try {
                await Convex.asyncSyscall("1.0/shallowMerge", JSON.stringify({
                  id: "docs/ghost",
                  value: { a: 1 },
                }));
                return "patch unexpectedly succeeded";
              } catch (e) {
                return "caught: " + e.message;
              }
            }
        "#;
        let result = cell
            .execute_async_main(&store, tenant, deployment, ts, vec![], source)
            .expect("the rejection must be catchable — execution completes");
        match &result.output {
            Value::Text(msg) => {
                assert!(
                    msg.contains("nonexistent document") && msg.contains("docs/ghost"),
                    "expected the structured nonexistent-doc error, got {msg:?}"
                );
            }
            other => panic!("expected Text output, got {other:?}"),
        }
        assert_eq!(result.traps, 1, "learning the absence costs one hydration");
        assert_eq!(
            result.consumed_reads,
            vec![DocumentId::new("docs/ghost")],
            "the observed absence is a declared dependency"
        );
        assert!(
            result.write_set.is_empty(),
            "a failed patch writes nothing: {:?}",
            result.write_set
        );
    }

    /// `db.replace` swaps the whole document while preserving `_id` (from
    /// the args) and `_creationTime` (from the base when omitted) — and a
    /// patch after a delete of the same key is the nonexistent-doc error,
    /// because the view's tombstone wins over the capsule doc.
    #[test]
    fn v8_convex_replace_preserves_system_fields_and_patch_after_delete_rejects() {
        let _v8 = v8_test_guard();
        let tenant = TenantId::new("tenant-repl");
        let deployment = DeploymentId::new("dep-repl");
        let store = MvccStore::new();
        let key_r = seed_raw(
            &store,
            "docs/r",
            r#"{"_id":"docs/r","_creationTime":123.5,"n":1,"stale":true}"#,
        );
        let key_x = seed_raw(&store, "docs/x", r#"{"_id":"docs/x","n":5}"#);
        let ts = store.snapshot_ts();
        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              const syscall = async (name, args) =>
                JSON.parse(await Convex.asyncSyscall(name, JSON.stringify(args)));
              const replaced = await syscall("1.0/replace", {
                id: "docs/r",
                value: { n: 9 },
              });
              await syscall("1.0/remove", { id: "docs/x" });
              try {
                await syscall("1.0/shallowMerge", { id: "docs/x", value: { n: 6 } });
                return "patch after delete unexpectedly succeeded";
              } catch (e) {
                return JSON.stringify(replaced) + " | " + e.message;
              }
            }
        "#;
        let result = cell
            .execute_async_main(&store, tenant, deployment, ts, vec![], source)
            .expect("replace + delete + failing patch should complete");
        let output = match &result.output {
            Value::Text(s) => s.clone(),
            other => panic!("expected Text output, got {other:?}"),
        };
        let (replaced_json, patch_error) =
            output.split_once(" | ").expect("output carries both parts");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(replaced_json).expect("replaced is JSON"),
            serde_json::json!({ "_id": "docs/r", "_creationTime": 123.5, "n": 9 }),
            "replace drops old fields but keeps _id + _creationTime"
        );
        assert!(
            patch_error.contains("nonexistent document") && patch_error.contains("docs/x"),
            "patch after delete must be the nonexistent-doc error, got {patch_error:?}"
        );
        assert_eq!(
            result.traps, 2,
            "one hydration per distinct key (replace base + delete base); \
             the failing patch answered from the pending tombstone"
        );
        assert_eq!(
            result.consumed_reads,
            vec![key_r.clone(), key_x.clone()],
            "replace and delete existence checks observed the snapshot"
        );
        assert_eq!(result.write_set.len(), 2);
        assert_eq!(
            write_raw(&result, "docs/r"),
            serde_json::json!({ "_id": "docs/r", "_creationTime": 123.5, "n": 9 })
        );
        assert_eq!(
            result.write_set.iter().find(|(k, _)| *k == key_x),
            Some(&(key_x, None)),
            "the delete is a tombstone in the write set"
        );
    }

    /// `db.insert` without an explicit `_id` needs server-side id minting,
    /// which the prototype does not have — a clear, catchable
    /// not-yet-supported rejection, never a silently invented id scheme.
    #[test]
    fn v8_convex_insert_without_id_rejects_as_not_supported() {
        let _v8 = v8_test_guard();
        let tenant = TenantId::new("tenant-mint");
        let deployment = DeploymentId::new("dep-mint");
        let store = MvccStore::new();
        let ts = store.snapshot_ts();
        let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
        let source = r#"
            async function main() {
              try {
                await Convex.asyncSyscall("1.0/insert", JSON.stringify({
                  table: "docs",
                  value: { n: 1 },
                }));
                return "insert unexpectedly succeeded";
              } catch (e) {
                return "caught: " + e.message;
              }
            }
        "#;
        let result = cell
            .execute_async_main(&store, tenant, deployment, ts, vec![], source)
            .expect("the rejection must be catchable — execution completes");
        match &result.output {
            Value::Text(msg) => {
                assert!(
                    msg.contains("not supported yet") && msg.contains("_id"),
                    "expected the id-minting rejection, got {msg:?}"
                );
            }
            other => panic!("expected Text output, got {other:?}"),
        }
        assert_eq!(result.traps, 0, "the rejection is local — no broker traffic");
        assert!(result.consumed_reads.is_empty());
        assert!(result.write_set.is_empty(), "nothing may be written");
    }
}
