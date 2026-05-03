//! Postgres-backed implementation of `aster_broker::CapsuleStore`.
//!
//! Stubs only in this commit (3/5 of the Postgres adapter plan). Every
//! read returns `StoreError::Backend("not implemented")` until commit 4
//! wires the actual SQL from the Convex schema reference. The point of
//! this commit is:
//!
//! 1. Establish the crate boundary so the cell-facing crates stay free
//!    of `tokio-postgres` / `deadpool-postgres`.
//! 2. Pin the **sync broker, async island** strategy: this struct owns
//!    a `tokio::runtime::Runtime` and a `deadpool_postgres::Pool`, and
//!    exposes a fully sync `impl CapsuleStore` that `block_on`s
//!    internally. Cells (and brokerd's accept loop) never touch tokio.
//! 3. Lock down the connect-time configuration — pool sizing, statement
//!    timeout, search_path — so commit 4 can focus on SQL.
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
        // Stub. Real impl (commit 4): T = max(SELECT ts FROM @schema.documents
        // ORDER BY ts DESC LIMIT 1, SELECT json_value FROM @schema.persistence_globals
        // WHERE key='max_repeatable_ts'). See docs/POSTGRES_ADAPTER_PLAN.md gotcha #7.
        // Touch every field so dead_code lint stays silent before commit 4 wires
        // them in. Cheap; CI runs with -D warnings.
        let _ = (&self.pool, &self.schema);
        let _ = self.block_on(async { 0u64 });
        Err(StoreError::Backend(
            "PostgresCapsuleStore::snapshot_ts not implemented yet (commit 4/5)".into(),
        ))
    }

    fn read_point(
        &self,
        _key: &DocumentId,
        _ts: Timestamp,
    ) -> Result<VersionedDocument, StoreError> {
        // Stub. Real impl (commit 4): the read goes through the `by_id`
        // index, NOT a direct documents query. SQL template lives in
        // INDEX_QUERIES (`crates/postgres/src/sql.rs` upstream).
        Err(StoreError::Backend(
            "PostgresCapsuleStore::read_point not implemented yet (commit 4/5)".into(),
        ))
    }

    fn read_prefix(
        &self,
        _prefix: &str,
        _limit: usize,
        _ts: Timestamp,
    ) -> Result<Vec<(DocumentId, VersionedDocument)>, StoreError> {
        // Stub. Real impl (commit 4): bounded INDEX_QUERIES range scan
        // with `key_prefix` collation; documents whose full key exceeds
        // 2500 bytes need an in-memory re-sort by full key.
        Err(StoreError::Backend(
            "PostgresCapsuleStore::read_prefix not implemented yet (commit 4/5)".into(),
        ))
    }

    fn build_capsule(
        &self,
        tenant: TenantId,
        deployment: DeploymentId,
        ts: Timestamp,
        prewarm: Vec<DocumentId>,
    ) -> Result<SnapshotCapsule, StoreError> {
        // Default trait impl loops read_point. Once that returns real
        // data, this works without further changes; the v0.5+ override
        // will batch prewarm into one `WHERE id = ANY($1)` round-trip.
        let mut capsule = SnapshotCapsule::empty(tenant, deployment, ts);
        for key in prewarm {
            capsule.hydrate_point(key.clone(), self.read_point(&key, ts)?);
        }
        Ok(capsule)
    }
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
    fn stubs_classify_unimplemented_as_backend() {
        // Pool isn't actually consulted because the stubs return early.
        // We use a syntactically-valid but unreachable URL.
        let cfg = PostgresConfig {
            url: "postgres://stub:stub@127.0.0.1:1/stub".into(),
            ..PostgresConfig::default()
        };
        let store = PostgresCapsuleStore::connect(cfg).expect("config parses");
        let key = DocumentId::new("doc/stub");
        match store.read_point(&key, 0) {
            Err(StoreError::Backend(msg)) => assert!(msg.contains("not implemented")),
            other => panic!("expected Backend(...), got {other:?}"),
        }
    }
}
