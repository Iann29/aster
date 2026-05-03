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
//! 3. **DocumentId encoding** is `"<table_hex>/<id_hex>"` —
//!    `parse_document_id` is the canonical splitter. The wire form is
//!    set so the broker can route reads without knowing Convex's IDv6
//!    string codec.
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
//! Why not `sqlx`: its `query!` macro requires a live database at
//! compile time, which CI cannot satisfy without a service container
//! and a checked-in offline-query-data file. We hand-write the SQL
//! against the Convex reference doc instead.

use std::sync::Arc;
use std::time::Duration;

use aster_broker::{CapsuleStore, StoreError};
use aster_capsule::{
    DeploymentId, DocumentId, SnapshotCapsule, TenantId, Timestamp, VersionedDocument,
};
use deadpool_postgres::{
    Manager, ManagerConfig, Pool, RecyclingMethod, Runtime as DeadpoolRuntime,
};
use tokio::runtime::{Builder, Runtime};
use tokio_postgres::{Config as PgConfig, NoTls};

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

        Ok(Self {
            runtime,
            pool,
            schema: config.schema,
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
        // Aster's DocumentId is opaque to Convex: we encode it as
        // "<table_hex>/<id_hex>" so the broker can split it back out.
        // Real Convex `db.get(id)` carries the table tag inside the id
        // string; the v0.5 wire-up converts that into our format before
        // the read trap fires.
        let (table_id, doc_id) = parse_document_id(key)?;

        // Using documents directly instead of the `by_id` index for v0.5.
        // The index path is more correct under retention (see gotcha #5)
        // but requires loading the table mapping first; we'll add it once
        // the broker has a real Convex frontend driving it.
        self.block_on(async {
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
}

/// Aster's `DocumentId` is the public-facing string "<table_hex>/<id_hex>".
/// We split it back into raw bytes for the SQL bind parameters.
fn parse_document_id(key: &DocumentId) -> Result<(Vec<u8>, Vec<u8>), StoreError> {
    let raw: &str = &key.0;
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
    fn document_id_parser_round_trips() {
        // Sanity: encoded form survives through the parser back to
        // the same bytes we'd send to Postgres.
        let key =
            DocumentId::new("0123456789abcdef0123456789abcdef/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let (table_id, doc_id) = parse_document_id(&key).expect("parse");
        assert_eq!(table_id.len(), 16);
        assert_eq!(doc_id.len(), 16);
        assert_eq!(encode_hex(&table_id), "0123456789abcdef0123456789abcdef");
        assert_eq!(encode_hex(&doc_id), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    }

    #[test]
    fn document_id_parser_rejects_malformed() {
        // No slash → no split.
        assert!(parse_document_id(&DocumentId::new("notslashed")).is_err());
        // Slash but neither half is hex.
        assert!(parse_document_id(&DocumentId::new("zz/aa")).is_err());
        // Odd-length hex.
        assert!(parse_document_id(&DocumentId::new("abc/aaaa")).is_err());
    }
}
