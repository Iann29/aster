//! Postgres-backed implementation of `aster_broker::CapsuleStore`.
//!
//! Reads from the same Postgres database the Convex backend writes to.
//! The schema is captured verbatim in `docs/CONVEX_POSTGRES_REFERENCE.md`
//! — this module follows its DDL and SQL templates.
//!
//! Design pillars:
//!
//! 1. **Crate boundary.** The cell-facing crates (broker, capsule, ipc,
//!    v8cell) link none of `tokio-postgres` / `deadpool-postgres`; only
//!    the brokerd binary depends on this crate.
//! 2. **Sync broker, async island.** This struct owns a
//!    `tokio::runtime::Runtime` and `block_on`s into it from sync
//!    callers. Cells (and brokerd's accept loop) never touch tokio.
//! 3. **DocumentId encoding** comes in two forms — the legacy Aster
//!    wire form `"<table_hex>/<id_hex>"` (used by tests + Aster-native
//!    callers) and the Convex IDv6 string a JS bundle hands to
//!    `db.get(id)`. `resolve_document_id` dispatches between them; the
//!    IDv6 path consults a `_tables`-backed mapping cache.
//!
//! Coverage today (v0.5):
//! - `snapshot_ts()` — `max(latest documents.ts, persistence_globals['max_repeatable_ts'])`.
//! - `read_point()` — direct `documents` query with `DISTINCT ON (id)`,
//!   `ORDER BY ts DESC LIMIT 1`. Skips the by_id index for now (gotcha
//!   #5 in the reference doc) — adequate for the v0.5 read-only read
//!   path and the integration tests, will move through the index when
//!   the broker grows the table-mapping cache.
//! - `read_prefix()` — bounded `DISTINCT ON (id)` table scan.
//! - Document body is currently passed through as raw JSON bytes under
//!   a `_raw` field. Decoding the actual `ConvexValue` blob lands when
//!   the cell's JS runtime grows the `Convex.asyncSyscall("1.0/get")`
//!   path that consumes it.
//!
//! ## DocumentId formats
//!
//! The adapter accepts two forms on the `read_point` / `read_prefix`
//! interfaces, dispatched at parse time:
//!
//! 1. **Aster wire form** — `"<table_hex>/<id_hex>"` (32 hex + slash +
//!    32 hex). Used by Aster-native callers and integration tests that
//!    work directly with raw tablet UUIDs.
//! 2. **Convex IDv6** — Crockford base32 string of `(table_number,
//!    internal_id, fletcher16)`. Forwarded by the cell when JS calls
//!    `db.get(id)` against an upstream Convex bundle. The adapter
//!    decodes the IDv6, resolves `table_number → tablet_uuid` via the
//!    table-mapping cache, and runs the same SQL as form #1.
//!
//! Form #1 is detected first by the presence of `/`. Form #2 is
//! everything else. Garbage falls through to a clear error message
//! that mentions both forms.
//!
//! Why not `sqlx`: its `query!` macro requires a live database at
//! compile time, which CI cannot satisfy without a service container
//! and a checked-in offline-query-data file. We hand-write the SQL
//! against the Convex reference doc instead.

mod module_index;
mod modules_storage;
mod table_mapping;

pub use module_index::ModuleDescriptor;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aster_broker::{CapsuleStore, StoreError};
use aster_capsule::{
    DeploymentId, DocumentId, SnapshotCapsule, TenantId, Timestamp, VersionedDocument,
};
use aster_convex_codec::DocumentIdV6;
use deadpool_postgres::{
    Manager, ManagerConfig, Pool, RecyclingMethod, Runtime as DeadpoolRuntime,
};
use tokio::runtime::{Builder, Runtime};
use tokio_postgres::{Config as PgConfig, NoTls};

use crate::module_index::ModuleIndex;
use crate::modules_storage::{LocalDirModulesStorage, ModulesStorage};
use crate::table_mapping::TableMappingCache;

/// Connection / pool tunables for `PostgresCapsuleStore::connect`.
#[derive(Clone, Debug)]
pub struct PostgresConfig {
    /// libpq-style connection URL — `postgres://user:pass@host:port/db?sslmode=...`.
    /// Today only NoTls is wired; `sslmode=require` is documented but not
    /// enforced — operators must provide their own TLS terminator (e.g.
    /// pgbouncer) until we add `tokio-postgres-rustls`.
    pub url: String,

    /// Postgres schema where Convex tables live (Convex's `@db_name`).
    /// Applied to every checked-out connection via
    /// `SET search_path = $1, public`.
    pub schema: String,

    /// Local-FS path where Convex's `_source_packages` storage layer
    /// writes module bundles. Maps to upstream's
    /// `<storage_dir>/modules/`. Empty disables module loading
    /// entirely — `find_module` still works (it's pure SQL) but
    /// `load_module_bundle` returns `Backend(_)` saying the
    /// adapter is unconfigured. Set this when the brokerd has
    /// access to Convex's module storage on disk.
    pub modules_dir: Option<PathBuf>,

    /// Hard cap on connections. Each in-flight cell hydrate may take
    /// one. Default 16 — sized against the `ASTER_MAX_CONNECTIONS=1024`
    /// brokerd cap; raise if you increase that.
    pub pool_max_size: usize,

    /// Per-checkout `SET statement_timeout`. Prevents a slow query from
    /// pinning a pool slot. Default 30 s.
    pub statement_timeout: Duration,

    /// How many tokio worker threads to spawn for the runtime that owns
    /// the pool. Default 2 — Postgres calls are I/O-bound, oversubscribing
    /// hurts more than it helps.
    pub runtime_worker_threads: usize,
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            schema: "public".into(),
            modules_dir: None,
            pool_max_size: 16,
            statement_timeout: Duration::from_secs(30),
            runtime_worker_threads: 2,
        }
    }
}

/// `CapsuleStore` backed by a real Postgres database.
///
/// Owns its tokio runtime — callers stay sync. Drop the store to drain
/// in-flight queries; the runtime's `Drop` waits up to a few seconds
/// before forcing termination.
pub struct PostgresCapsuleStore {
    runtime: Arc<Runtime>,
    pool: Pool,
    schema: String,
    /// Lazy `table_number → tablet_uuid` resolver. Populated on first
    /// IDv6-shaped lookup, refreshed on miss. Wrapped in Arc so it can
    /// be cloned cheaply into the per-call async closures.
    table_mapping: Arc<TableMappingCache>,
    /// Lazy `module_path → ModuleDescriptor` resolver. Refreshed on
    /// `find_module` miss. Together with the table-mapping cache this
    /// is the full `_modules` ↔ `_source_packages` indirection — the
    /// storage adapter (next slice) takes a `ModuleDescriptor` and
    /// returns bundle bytes.
    module_index: Arc<ModuleIndex>,
    /// Optional bundle-bytes adapter. `None` when `modules_dir`
    /// wasn't configured; `load_module_bundle` returns a typed
    /// "modules dir not configured" error in that case.
    modules_storage: Option<Arc<dyn ModulesStorage>>,
}

impl PostgresCapsuleStore {
    /// Build a store from a `PostgresConfig`. Returns an error if the URL
    /// is malformed or the runtime cannot be created. **Does not** open
    /// a connection — first checkout happens at the first read. This
    /// matches the v0.4 design choice "Postgres down at startup must not
    /// crash brokerd".
    pub fn connect(config: PostgresConfig) -> Result<Self, StoreError> {
        if config.url.is_empty() {
            return Err(StoreError::Backend(
                "PostgresConfig.url is empty — set ASTER_DB_URL or ASTER_DB_URL_FILE".into(),
            ));
        }
        let pg_config: PgConfig = config
            .url
            .parse()
            .map_err(|err: tokio_postgres::Error| StoreError::Backend(format!("bad url: {err}")))?;

        let runtime = Builder::new_multi_thread()
            .worker_threads(config.runtime_worker_threads.max(1))
            .enable_all()
            .build()
            .map_err(|err| StoreError::Backend(format!("tokio runtime: {err}")))?;
        let runtime = Arc::new(runtime);

        let manager = Manager::from_config(
            pg_config,
            NoTls,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        let pool = Pool::builder(manager)
            .max_size(config.pool_max_size)
            .runtime(DeadpoolRuntime::Tokio1)
            .build()
            .map_err(|err| StoreError::Backend(format!("pool builder: {err}")))?;

        let modules_storage: Option<Arc<dyn ModulesStorage>> = config
            .modules_dir
            .clone()
            .map(|dir| Arc::new(LocalDirModulesStorage::new(dir)) as Arc<dyn ModulesStorage>);

        Ok(Self {
            runtime,
            pool,
            schema: config.schema,
            table_mapping: Arc::new(TableMappingCache::new()),
            module_index: Arc::new(ModuleIndex::new()),
            modules_storage,
        })
    }

    /// Block on a tokio future from a sync thread, threading our runtime
    /// instead of relying on whatever `Handle::current()` happens to find.
    fn block_on<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        self.runtime.block_on(fut)
    }
}

impl CapsuleStore for PostgresCapsuleStore {
    fn snapshot_ts(&self) -> Result<Timestamp, StoreError> {
        // T = max(SELECT ts FROM @schema.documents ORDER BY ts DESC LIMIT 1,
        // SELECT json_value FROM @schema.persistence_globals WHERE key='max_repeatable_ts').
        // Both queries go through the same checkout so an inflight commit
        // doesn't race the read. See CONVEX_POSTGRES_REFERENCE.md, the
        // "Picking T (snapshot timestamp)" section.
        self.block_on(async {
            let client = self.checkout().await?;

            // Highest committed write — uses the documents PK
            // (ts, table_id, id) DESC index implicitly. ORDER BY only the
            // ts column would still match the PK because it's the leading
            // key, but Postgres needs the LIMIT 1 + DESC to not seq scan.
            let row = client
                .query_opt(
                    &format!(
                        "SELECT ts FROM {schema}.documents ORDER BY ts DESC LIMIT 1",
                        schema = self.schema
                    ),
                    &[],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("snapshot_ts(documents): {err}")))?;
            let docs_ts: i64 = row.map(|r| r.get::<_, i64>(0)).unwrap_or(0);

            // Fenced ceiling — what the committer told us is durable.
            // Convex stores it as JSON bytes; we parse to int64.
            let row = client
                .query_opt(
                    &format!(
                        "SELECT json_value FROM {schema}.persistence_globals \
                         WHERE key = 'max_repeatable_ts'",
                        schema = self.schema
                    ),
                    &[],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("snapshot_ts(globals): {err}")))?;
            let fence_ts: i64 = match row {
                Some(r) => {
                    let bytes: Vec<u8> = r.get(0);
                    let s = std::str::from_utf8(&bytes).map_err(|err| {
                        StoreError::Backend(format!("max_repeatable_ts utf8: {err}"))
                    })?;
                    s.trim().parse::<i64>().map_err(|err| {
                        StoreError::Backend(format!("max_repeatable_ts parse: {err}"))
                    })?
                }
                None => 0,
            };

            let max = docs_ts.max(fence_ts);
            // i64 → u64 (Aster's Timestamp). Negative ts is impossible by
            // construction in Convex (microseconds since epoch) but guard.
            if max < 0 {
                return Err(StoreError::Backend(format!(
                    "snapshot_ts: backend returned negative ts {max}"
                )));
            }
            Ok(max as u64)
        })
    }

    fn read_point(&self, key: &DocumentId, ts: Timestamp) -> Result<VersionedDocument, StoreError> {
        // Aster's DocumentId is opaque to Convex: callers send either
        // the Aster wire form `"<table_hex>/<id_hex>"` (used by tests
        // and any caller that already resolved a tablet) or a Convex
        // IDv6 string (the JS bundle's `db.get(id)` path). Dispatching
        // on the form lives inside the async block so the IDv6 case
        // can refresh the table-mapping cache via Postgres on a miss.
        self.block_on(async {
            let (table_id, doc_id) = self.resolve_document_id(key).await?;
            let client = self.checkout().await?;
            let ts_signed = ts as i64;
            let row = client
                .query_opt(
                    &format!(
                        "SELECT ts, json_value, deleted \
                         FROM {schema}.documents \
                         WHERE table_id = $1 AND id = $2 AND ts <= $3 \
                         ORDER BY ts DESC LIMIT 1",
                        schema = self.schema
                    ),
                    &[&table_id, &doc_id, &ts_signed],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("read_point: {err}")))?;

            let Some(row) = row else {
                return Ok(VersionedDocument::missing());
            };
            let row_ts: i64 = row.get(0);
            let bytes: Vec<u8> = row.get(1);
            let deleted: Option<bool> = row.try_get(2).ok();
            if deleted.unwrap_or(false) {
                // Tombstone — the row at this snapshot is a delete.
                // Aster surfaces that as `(version=Some(ts), document=None)`
                // so OCC can detect "absence observed at this ts".
                return Ok(VersionedDocument {
                    version: Some(row_ts as u64),
                    document: None,
                });
            }

            // For v0.5 we don't decode the Convex `ConvexValue` blob.
            // We hand the raw bytes through as a single `_raw` field so
            // the cell can return them; richer decoding lands when the
            // module loader does. The cell side will eventually deserialize.
            Ok(VersionedDocument {
                version: Some(row_ts as u64),
                document: Some(raw_document(bytes)),
            })
        })
    }

    fn read_prefix(
        &self,
        prefix: &str,
        limit: usize,
        ts: Timestamp,
    ) -> Result<Vec<(DocumentId, VersionedDocument)>, StoreError> {
        // Convex's read_prefix is normally an INDEX_QUERIES range scan;
        // for v0.5 we accept the simpler form: prefix is "<table_hex>/"
        // and we list every document under that table. Bounded by
        // `limit` to keep capsule size predictable.
        let table_id = match prefix.strip_suffix('/') {
            Some(s) => decode_hex(s).ok_or_else(|| {
                StoreError::Backend(format!(
                    "read_prefix: prefix {prefix:?} is not '<table_hex>/'"
                ))
            })?,
            None => {
                return Err(StoreError::Backend(format!(
                    "read_prefix: prefix {prefix:?} must end with '/'"
                )))
            }
        };
        if limit == 0 {
            return Ok(Vec::new());
        }

        self.block_on(async {
            let client = self.checkout().await?;
            let ts_signed = ts as i64;
            let limit_i64 = limit.min(i64::MAX as usize) as i64;
            let rows = client
                .query(
                    &format!(
                        "SELECT DISTINCT ON (id) id, ts, json_value, deleted \
                         FROM {schema}.documents \
                         WHERE table_id = $1 AND ts <= $2 \
                         ORDER BY id ASC, ts DESC \
                         LIMIT $3",
                        schema = self.schema
                    ),
                    &[&table_id, &ts_signed, &limit_i64],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("read_prefix: {err}")))?;

            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id_bytes: Vec<u8> = row.get(0);
                let row_ts: i64 = row.get(1);
                let bytes: Vec<u8> = row.get(2);
                let deleted: Option<bool> = row.try_get(3).ok();

                let doc_id = DocumentId::new(format!(
                    "{}/{}",
                    encode_hex(&table_id),
                    encode_hex(&id_bytes)
                ));
                let value = if deleted.unwrap_or(false) {
                    VersionedDocument {
                        version: Some(row_ts as u64),
                        document: None,
                    }
                } else {
                    VersionedDocument {
                        version: Some(row_ts as u64),
                        document: Some(raw_document(bytes)),
                    }
                };
                out.push((doc_id, value));
            }
            Ok(out)
        })
    }

    fn build_capsule(
        &self,
        tenant: TenantId,
        deployment: DeploymentId,
        ts: Timestamp,
        prewarm: Vec<DocumentId>,
    ) -> Result<SnapshotCapsule, StoreError> {
        // Default trait impl loops read_point. v0.6+ will batch with
        // `WHERE (table_id, id) IN ($1, $2, ...)` once the wire shape
        // settles; for now correctness > batched round-trips.
        let mut capsule = SnapshotCapsule::empty(tenant, deployment, ts);
        for key in prewarm {
            capsule.hydrate_point(key.clone(), self.read_point(&key, ts)?);
        }
        Ok(capsule)
    }
}

impl PostgresCapsuleStore {
    /// Acquire a connection from the pool. Surfaces the error as
    /// `StoreError::Unavailable` so callers (and through them, cells)
    /// can distinguish "Postgres is down" from "the SQL itself failed".
    async fn checkout(&self) -> Result<deadpool_postgres::Object, StoreError> {
        self.pool
            .get()
            .await
            .map_err(|err| StoreError::Unavailable(format!("pool checkout: {err}")))
    }

    /// Resolve a `DocumentId` to the `(table_id, id)` byte pair the
    /// `documents` index expects. Two input forms are accepted:
    ///
    /// 1. **Aster wire form** — `"<table_hex>/<id_hex>"`. Used by tests
    ///    and any caller that already holds a tablet UUID. Parsed
    ///    without touching Postgres.
    /// 2. **Convex IDv6** — Crockford base32 of `(table_number,
    ///    internal_id, footer)`. The table-mapping cache is consulted;
    ///    a miss triggers a refresh and one retry. Failure to resolve
    ///    after the refresh surfaces as `StoreError::Backend` with the
    ///    table number named so operators can audit `_tables`.
    ///
    /// Visible for tests via `#[cfg(test)]` callers.
    async fn resolve_document_id(
        &self,
        key: &DocumentId,
    ) -> Result<(Vec<u8>, Vec<u8>), StoreError> {
        let raw: &str = &key.0;
        if raw.contains('/') {
            return parse_aster_document_id(raw);
        }
        // IDv6 path. Decode first — a malformed IDv6 should surface as
        // a clear error instead of triggering a Postgres roundtrip.
        let id = DocumentIdV6::decode(raw).map_err(|err| {
            StoreError::Backend(format!(
                "DocumentId {raw:?}: not '<table_hex>/<id_hex>' and not a valid IDv6 string: {err}"
            ))
        })?;
        let tablet = self
            .resolve_table_number(id.table_number)
            .await?
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "DocumentId {raw:?}: table_number {} unknown — \
                     refresh ran and `_tables` does not list it (deleted? hidden?)",
                    id.table_number
                ))
            })?;
        Ok((tablet.to_vec(), id.internal_id.to_vec()))
    }

    /// Lookup with one refresh-and-retry on miss. Two roundtrips at
    /// most: one for `persistence_globals['tables_table_id']`, one for
    /// the `_tables` body scan. Cold-start cost is paid once per
    /// brokerd lifetime + once per genuinely-new tablet.
    async fn resolve_table_number(
        &self,
        table_number: u32,
    ) -> Result<Option<[u8; 16]>, StoreError> {
        if let Some(uuid) = self.table_mapping.lookup(table_number) {
            return Ok(Some(uuid));
        }
        let client = self.checkout().await?;
        self.table_mapping.refresh(&client, &self.schema).await?;
        Ok(self.table_mapping.lookup(table_number))
    }

    /// Resolve a Convex module path (e.g. `"messages.js"`,
    /// `"_generated/api.js"`) to the descriptor the storage adapter
    /// will use to fetch bundle bytes. Cache miss triggers one
    /// refresh + retry (which also reloads the table-mapping cache,
    /// since the module index is keyed off `_modules` /
    /// `_source_packages` tablet UUIDs).
    ///
    /// Returns `None` if the path genuinely doesn't exist after
    /// refresh — distinct from a `Backend(_)` error which means a
    /// row was malformed or Postgres was unreachable.
    pub fn find_module(&self, path: &str) -> Result<Option<ModuleDescriptor>, StoreError> {
        if let Some(d) = self.module_index.find(path) {
            return Ok(Some(d));
        }
        self.block_on(async {
            let client = self.checkout().await?;
            self.table_mapping.refresh(&client, &self.schema).await?;
            self.module_index
                .refresh(&client, &self.schema, &self.table_mapping)
                .await?;
            Ok(self.module_index.find(path))
        })
    }

    /// All known modules, sorted by path. Triggers a refresh if the
    /// index has never been populated. Useful for "list deployed
    /// functions" telemetry; the hot path goes through `find_module`.
    pub fn list_modules(&self) -> Result<Vec<ModuleDescriptor>, StoreError> {
        let cached = self.module_index.list();
        if !cached.is_empty() {
            return Ok(cached);
        }
        self.block_on(async {
            let client = self.checkout().await?;
            self.table_mapping.refresh(&client, &self.schema).await?;
            self.module_index
                .refresh(&client, &self.schema, &self.table_mapping)
                .await?;
            Ok(self.module_index.list())
        })
    }

    /// Resolve a module path AND fetch its bundle bytes — the join
    /// of `find_module` + the storage adapter. Returns the raw ZIP
    /// bytes the upstream bundler emitted (still zipped; unzip lives
    /// in the cell-side loader, next slice). The storage adapter
    /// hash-checks the bytes against `_source_packages.sha256`
    /// before returning.
    ///
    /// Errors:
    ///   - `Backend("modules dir not configured")` — operator didn't
    ///     set `modules_dir` on the `PostgresConfig`. Module loading
    ///     is opt-in for v0.5; brokerds that only do `db.get`-style
    ///     reads don't need it.
    ///   - `Backend(_)` from `find_module` — Postgres failure or a
    ///     malformed `_modules` / `_source_packages` row.
    ///   - `Backend(_)` from the storage adapter — file missing
    ///     (mount misconfiguration), I/O error, or sha256 mismatch.
    ///   - `Ok(None)` — module path resolved cleanly but isn't
    ///     present in `_modules`. Caller chooses 404 vs 500.
    pub fn load_module_bundle(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let storage = self.modules_storage.as_ref().ok_or_else(|| {
            StoreError::Backend(
                "modules storage not configured — set PostgresConfig.modules_dir \
                 (typically ASTER_MODULES_DIR pointing at convex's <storage>/modules)"
                    .into(),
            )
        })?;

        let descriptor = match self.find_module(path)? {
            Some(d) => d,
            None => return Ok(None),
        };
        // The storage adapter is sync; we're already inside the
        // `block_on` if `find_module` triggered a refresh, but
        // because find_module returns BEFORE we call `fetch`, we're
        // back on the caller's thread here. That's fine — the FS
        // read doesn't need tokio.
        let bytes = storage.fetch(&descriptor)?;
        Ok(Some(bytes))
    }
}

/// Parse the Aster wire form `"<table_hex>/<id_hex>"` into raw bytes.
/// Standalone so callers in `read_prefix` can reuse it without
/// borrowing `&self` (no Postgres roundtrip needed for this form).
fn parse_aster_document_id(raw: &str) -> Result<(Vec<u8>, Vec<u8>), StoreError> {
    let (t, i) = raw.split_once('/').ok_or_else(|| {
        StoreError::Backend(format!(
            "DocumentId {raw:?}: expected '<table_hex>/<id_hex>'"
        ))
    })?;
    let table_id = decode_hex(t).ok_or_else(|| {
        StoreError::Backend(format!("DocumentId {raw:?}: table_hex {t:?} not hex"))
    })?;
    let doc_id = decode_hex(i)
        .ok_or_else(|| StoreError::Backend(format!("DocumentId {raw:?}: id_hex {i:?} not hex")))?;
    Ok((table_id, doc_id))
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Wrap raw Convex `json_value` bytes in an Aster `Document` so the
/// cell can pass them along untouched. v0.6+ will decode the actual
/// `ConvexValue` blob; for v0.5 we just stash the bytes as a string
/// under `_raw`.
fn raw_document(bytes: Vec<u8>) -> aster_capsule::Document {
    use aster_capsule::Value;
    let mut doc = aster_capsule::Document::new();
    let s = String::from_utf8_lossy(&bytes).into_owned();
    doc.insert("_raw".to_string(), Value::Text(s));
    doc
}

impl Drop for PostgresCapsuleStore {
    fn drop(&mut self) {
        // Best-effort drain. If the runtime is still hosting in-flight
        // queries when brokerd shuts down, give them 2 s to finish before
        // tokio force-terminates. shutdown_timeout takes ownership, so
        // we extract from the Arc when we're the sole holder; otherwise
        // we just drop our reference and let the last holder do it.
        if let Some(rt) = Arc::get_mut(&mut self.runtime) {
            // SAFETY: replace with a no-op runtime so we own the value.
            let owned = std::mem::replace(
                rt,
                Builder::new_multi_thread()
                    .worker_threads(1)
                    .build()
                    .expect("placeholder runtime"),
            );
            owned.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_with_empty_url_fails_fast() {
        let err = match PostgresCapsuleStore::connect(PostgresConfig::default()) {
            Ok(_) => panic!("empty URL should fail"),
            Err(e) => e,
        };
        assert!(
            matches!(err, StoreError::Backend(ref msg) if msg.contains("ASTER_DB_URL")),
            "expected actionable error mentioning the env var, got {err}"
        );
    }

    #[test]
    fn connect_with_malformed_url_classifies_as_backend() {
        let cfg = PostgresConfig {
            url: "not-a-postgres-url".into(),
            ..PostgresConfig::default()
        };
        let err = match PostgresCapsuleStore::connect(cfg) {
            Ok(_) => panic!("malformed URL should fail"),
            Err(e) => e,
        };
        assert!(
            matches!(err, StoreError::Backend(_)),
            "expected Backend(_), got {err}"
        );
    }

    #[test]
    fn read_point_with_unreachable_postgres_surfaces_unavailable() {
        // Pool is built lazily so connect() succeeds even when there's no
        // server. The first read tries to open a TCP connection and
        // fails — we want that to surface as Unavailable, not Backend,
        // so callers can retry rather than treat it as a permanent
        // bug. Port 1 is the unreachable canary (root-only on Linux,
        // never bound by any sane service).
        let cfg = PostgresConfig {
            url: "postgres://stub:stub@127.0.0.1:1/stub".into(),
            ..PostgresConfig::default()
        };
        let store = PostgresCapsuleStore::connect(cfg).expect("config parses");
        let key =
            DocumentId::new("0123456789abcdef0123456789abcdef/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        match store.read_point(&key, 200) {
            Err(StoreError::Unavailable(_)) => {}
            other => panic!("expected Unavailable(...) on dead Postgres, got {other:?}"),
        }
    }

    #[test]
    fn aster_wire_form_round_trips() {
        // Sanity: encoded form survives through the parser back to
        // the same bytes we'd send to Postgres.
        let raw = "0123456789abcdef0123456789abcdef/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let (table_id, doc_id) = parse_aster_document_id(raw).expect("parse");
        assert_eq!(table_id.len(), 16);
        assert_eq!(doc_id.len(), 16);
        assert_eq!(encode_hex(&table_id), "0123456789abcdef0123456789abcdef");
        assert_eq!(encode_hex(&doc_id), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    }

    #[test]
    fn aster_wire_form_rejects_malformed_with_slash() {
        // Slash but neither half is hex.
        assert!(parse_aster_document_id("zz/aa").is_err());
        // Odd-length hex.
        assert!(parse_aster_document_id("abc/aaaa").is_err());
    }

    /// IDv6 dispatch: a string with no `/` runs through the IDv6
    /// codec. Garbage that isn't valid IDv6 must error before any
    /// Postgres roundtrip — operators see one consistent error
    /// regardless of which form the caller intended.
    #[test]
    fn dispatch_rejects_garbage_idv6_without_postgres() {
        // Build a store with no real Postgres behind it. `connect()`
        // is lazy so this never opens a TCP socket.
        let cfg = PostgresConfig {
            url: "postgres://stub:stub@127.0.0.1:1/stub".into(),
            ..PostgresConfig::default()
        };
        let store = PostgresCapsuleStore::connect(cfg).expect("config parses");
        let key = DocumentId::new("not-a-real-idv6-string");
        let err = store
            .runtime
            .block_on(store.resolve_document_id(&key))
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Backend(ref m) if m.contains("not a valid IDv6 string")),
            "expected IDv6 parse error, got {err:?}"
        );
    }

    /// IDv6 dispatch: a well-formed IDv6 with an unknown table_number
    /// triggers a Postgres roundtrip. Without a real DB we expect
    /// `Unavailable` — confirms the dispatch reached the resolver.
    #[test]
    fn dispatch_attempts_refresh_when_idv6_table_number_unknown() {
        let cfg = PostgresConfig {
            url: "postgres://stub:stub@127.0.0.1:1/stub".into(),
            ..PostgresConfig::default()
        };
        let store = PostgresCapsuleStore::connect(cfg).expect("config parses");
        // Build a real IDv6 — table_number=42, internal_id=zeroes.
        let id = aster_convex_codec::DocumentIdV6::new(42, [0u8; 16]);
        let key = DocumentId::new(id.encode());
        let err = store
            .runtime
            .block_on(store.resolve_document_id(&key))
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Unavailable(_)),
            "expected Unavailable from refresh attempt, got {err:?}"
        );
    }

    /// Cache hit short-circuits the Postgres roundtrip — useful when
    /// Postgres is briefly unreachable but we already cached the
    /// mapping. Verifies we don't open a connection on the hot path.
    #[test]
    fn dispatch_uses_cache_when_table_number_known() {
        use std::collections::BTreeMap;
        let cfg = PostgresConfig {
            url: "postgres://stub:stub@127.0.0.1:1/stub".into(),
            ..PostgresConfig::default()
        };
        let store = PostgresCapsuleStore::connect(cfg).expect("config parses");
        let mut mapping = BTreeMap::new();
        let tablet = [0xCDu8; 16];
        mapping.insert(10001, tablet);
        store.table_mapping.install_for_test(mapping);

        let internal = [0xEFu8; 16];
        let id = aster_convex_codec::DocumentIdV6::new(10001, internal);
        let key = DocumentId::new(id.encode());
        let (table_id, doc_id) = store
            .runtime
            .block_on(store.resolve_document_id(&key))
            .expect("cache hit");
        assert_eq!(table_id, tablet.to_vec());
        assert_eq!(doc_id, internal.to_vec());
    }

    /// Module index hot path: pre-installed descriptor returns
    /// without opening a Postgres connection. `find_module` short-
    /// circuits the refresh when the index already has the entry —
    /// matches the `dispatch_uses_cache_when_table_number_known`
    /// pattern, but on the module-loader cache.
    #[test]
    fn find_module_uses_cache_when_path_known() {
        use std::collections::BTreeMap;
        let cfg = PostgresConfig {
            url: "postgres://stub:stub@127.0.0.1:1/stub".into(),
            ..PostgresConfig::default()
        };
        let store = PostgresCapsuleStore::connect(cfg).expect("config parses");

        // Pre-warm both caches so neither needs Postgres.
        let mut by_number = BTreeMap::new();
        by_number.insert(10001, [0xAA; 16]);
        let mut by_name = BTreeMap::new();
        by_name.insert("_modules".to_string(), [0xBB; 16]);
        by_name.insert("_source_packages".to_string(), [0xCC; 16]);
        store
            .table_mapping
            .install_named_for_test(by_number, by_name);

        let descriptor = ModuleDescriptor {
            path: "messages.js".into(),
            source_package_internal_id: [0xDD; 16],
            storage_key: "modules/abc".into(),
            environment: "isolate".into(),
            module_sha256_base64: "deadbeef".into(),
            source_package_sha256: vec![1, 2, 3, 4],
            source_package_unzipped_size: Some(1024),
        };
        store
            .module_index
            .install_for_test(vec![descriptor.clone()]);

        let found = store
            .find_module("messages.js")
            .expect("Postgres-free lookup")
            .expect("module present");
        assert_eq!(found, descriptor);

        // list_modules also short-circuits when the cache is non-empty.
        let listed = store.list_modules().expect("list");
        assert_eq!(listed, vec![descriptor]);
    }
}
