//! Aster's own write plane: lease authority, commit log, retention floor,
//! and the commit fence — the storage half of the Capsule Transaction
//! Theorem's `CommitFence` (§1.8), on Aster-owned tables in the `aster`
//! schema.
//!
//! Scope (referee finding F9): this plane owns its commit log. It never
//! writes Convex's tables — integrating with a live convex-backend
//! deployment (lease takeover vs. commit gateway) is a separate product
//! decision outside v0.7.
//!
//! Theorem ↔ implementation map:
//! - A7 (single-writer lease, strictly increasing epochs — referee F1):
//!   `acquire_lease` bumps `aster.lease.epoch` under the row lock, so
//!   epochs are never reused, even across failback A→B→A.
//! - A8 (atomic commit fence): `commit` runs V2–V5 and the append inside
//!   ONE transaction whose first statement locks the lease row
//!   `FOR UPDATE`. Every commit takes that lock, so validation and append
//!   share one stable horizon and no commit interposes (Lemma 3.10).
//!   A concurrent `acquire_lease` blocks on the same row until an
//!   in-flight fence commits — an old-epoch append therefore linearizes
//!   before the new epoch's first fence, and later old-epoch fences fail
//!   the epoch equality check (Lemma 3.11).
//! - A9 / Lemma R (retention pin): the fence locks the `aster.retention`
//!   row `FOR UPDATE` from its coverage check (`g <= s`) through append;
//!   `advance_retention` takes the same lock, so GC cannot remove an
//!   event in `(s, h]` mid-validation. Coverage is watermark-based here,
//!   which is the theorem's *exact* admission condition; a wall-clock
//!   max-age rule (V3's `s >= Now − Δ`) is a product admission policy the
//!   caller may add on top.
//! - A10 (tenant confinement): the log is keyed by (tenant, deployment)
//!   columns supplied by the trusted committer, so an authorized write
//!   physically cannot land in another tenant's key space.

use std::sync::Arc;
use std::time::Duration;

use aster_broker::{CommitFence, StoreError};
// The fence input/outcome types moved to `aster_broker::fence` in S9a so
// the brokerd binary can dispatch on the `CommitFence` seam; re-exported
// here so `aster_store_postgres::{FenceInput, CommitOutcome}` call sites
// (and the write_plane_it suite) stay source-compatible.
pub use aster_broker::{CommitOutcome, FenceInput};
use aster_capsule::{Document, DocumentId, Timestamp, VersionedDocument};
use deadpool_postgres::{
    Manager, ManagerConfig, Pool, RecyclingMethod, Runtime as DeadpoolRuntime,
};
use tokio::runtime::{Builder, Runtime};
use tokio_postgres::{Config as PgConfig, NoTls};

/// TCP keepalive timers for the plane's connections. Deliberately short:
/// a committer whose peer vanished (partition, killed VM) must notice in
/// tens of seconds, not the OS default ~2h11m — until then its pool slots
/// hang on dead sockets. The server-side mirror of this problem (a dead
/// COMMITTER holding row locks) is what `idle_in_transaction_session_timeout`
/// bounds; keepalives cover the client-side half.
const TCP_KEEPALIVE_IDLE: Duration = Duration::from_secs(10);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
const TCP_KEEPALIVE_RETRIES: u32 = 3;

#[derive(Clone, Debug)]
pub struct WritePlaneConfig {
    /// libpq-style connection URL. Same caveats as `PostgresConfig::url`.
    pub url: String,
    pub pool_max_size: usize,
    pub runtime_worker_threads: usize,
    /// Per-checkout `SET statement_timeout`. Bounds how long any single
    /// statement may EXECUTE. It does NOT bound how long a fence holds
    /// the lease/retention row locks: the fence transaction spans many
    /// client round-trips, and a session that is idle *between* statements
    /// (mid-transaction) never trips statement_timeout — that case is
    /// bounded by `idle_in_transaction_session_timeout` below.
    pub statement_timeout: Duration,
    /// Per-checkout `SET idle_in_transaction_session_timeout`. Bounds how
    /// long a fence transaction may sit idle between round-trips while
    /// holding the lease/retention row locks. Without it, a SIGSTOPped or
    /// partitioned committer wedges failover, every commit, and GC until
    /// TCP gives up (~2h11m by kernel default; forever under SIGSTOP with
    /// keepalives answered by the healthy kernel) — with it, Postgres
    /// kills the idle session and its locks roll back. Defaults to the
    /// same bound as `statement_timeout`.
    pub idle_in_transaction_session_timeout: Duration,
}

impl Default for WritePlaneConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            pool_max_size: 16,
            runtime_worker_threads: 2,
            statement_timeout: Duration::from_secs(30),
            idle_in_transaction_session_timeout: Duration::from_secs(30),
        }
    }
}

pub struct WritePlane {
    runtime: Arc<Runtime>,
    pool: Pool,
    statement_timeout_ms: u64,
    idle_in_transaction_ms: u64,
}

impl WritePlane {
    pub fn connect(config: WritePlaneConfig) -> Result<Self, StoreError> {
        if config.url.is_empty() {
            return Err(StoreError::Backend(
                "WritePlaneConfig.url is empty".into(),
            ));
        }
        let pg_config = parse_pg_config(&config.url)?;
        let runtime = Builder::new_multi_thread()
            .worker_threads(config.runtime_worker_threads.max(1))
            .enable_all()
            .build()
            .map_err(|err| StoreError::Backend(format!("tokio runtime: {err}")))?;
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
            runtime: Arc::new(runtime),
            pool,
            statement_timeout_ms: config.statement_timeout.as_millis() as u64,
            idle_in_transaction_ms: config
                .idle_in_transaction_session_timeout
                .as_millis() as u64,
        })
    }

    fn block_on<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        self.runtime.block_on(fut)
    }

    async fn client(&self) -> Result<deadpool_postgres::Object, StoreError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|err| StoreError::Unavailable(format!("pool checkout: {err}")))?;
        // Both GUCs are session-scoped and deadpool's Fast recycling does
        // not reset them, but re-stamping per checkout keeps the value in
        // lockstep with this plane's config even if the session was first
        // opened by a differently-configured plane sharing the database.
        client
            .batch_execute(&format!(
                "SET statement_timeout = {}; \
                 SET idle_in_transaction_session_timeout = {}",
                self.statement_timeout_ms, self.idle_in_transaction_ms
            ))
            .await
            .map_err(|err| StoreError::Backend(format!("set session timeouts: {err}")))?;
        Ok(client)
    }

    /// Idempotent bootstrap of the `aster` schema. Call once at committer
    /// startup (and from tests).
    pub fn ensure_schema(&self) -> Result<(), StoreError> {
        self.block_on(async {
            let client = self.client().await?;
            client
                .batch_execute(
                    "CREATE SCHEMA IF NOT EXISTS aster;
                     CREATE TABLE IF NOT EXISTS aster.lease (
                         tenant text NOT NULL,
                         deployment text NOT NULL,
                         epoch bigint NOT NULL,
                         holder text NOT NULL DEFAULT '',
                         acquired_at timestamptz NOT NULL DEFAULT now(),
                         PRIMARY KEY (tenant, deployment)
                     );
                     CREATE TABLE IF NOT EXISTS aster.log (
                         tenant text NOT NULL,
                         deployment text NOT NULL,
                         ts bigint NOT NULL,
                         key text NOT NULL,
                         epoch bigint NOT NULL,
                         document text,
                         PRIMARY KEY (tenant, deployment, ts, key)
                     );
                     CREATE INDEX IF NOT EXISTS log_key_ts
                         ON aster.log (tenant, deployment, key, ts);
                     CREATE TABLE IF NOT EXISTS aster.retention (
                         tenant text NOT NULL,
                         deployment text NOT NULL,
                         low_watermark bigint NOT NULL DEFAULT 0,
                         PRIMARY KEY (tenant, deployment)
                     );",
                )
                .await
                .map_err(|err| StoreError::Backend(format!("ensure_schema: {err}")))
        })
    }

    /// Acquire (or take over) the single-writer lease. Every acquisition
    /// bumps the epoch — strictly increasing, never reused (F1). The
    /// UPDATE waits on any in-flight commit fence holding the row lock,
    /// which is exactly the failover ordering Lemma 3.11 requires.
    pub fn acquire_lease(
        &self,
        tenant: &str,
        deployment: &str,
        holder: &str,
    ) -> Result<u64, StoreError> {
        self.block_on(async {
            let client = self.client().await?;
            let row = client
                .query_one(
                    "INSERT INTO aster.lease (tenant, deployment, epoch, holder)
                     VALUES ($1, $2, 1, $3)
                     ON CONFLICT (tenant, deployment) DO UPDATE
                         SET epoch = aster.lease.epoch + 1,
                             holder = EXCLUDED.holder,
                             acquired_at = now()
                     RETURNING epoch",
                    &[&tenant, &deployment, &holder],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("acquire_lease: {err}")))?;
            client
                .execute(
                    "INSERT INTO aster.retention (tenant, deployment, low_watermark)
                     VALUES ($1, $2, 0)
                     ON CONFLICT (tenant, deployment) DO NOTHING",
                    &[&tenant, &deployment],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("seed retention: {err}")))?;
            // R0 repair at authority boot (re-referee F11): the invariant
            // 0 <= g <= tip(L) is what makes "retry from a fresh snapshot"
            // admissible. The clamp keeps NEW advances below the tip, but a
            // floor already above it (pre-clamp binary, manual writes)
            // would refuse every future snapshot forever — and lowering it
            // to the tip un-protects nothing: no event above the tip ever
            // existed, so none was compacted.
            let repaired = client
                .execute(
                    "UPDATE aster.retention AS r SET low_watermark = t.tip
                     FROM (SELECT COALESCE(MAX(ts), 0) AS tip FROM aster.log
                           WHERE tenant = $1 AND deployment = $2) AS t
                     WHERE r.tenant = $1 AND r.deployment = $2
                       AND r.low_watermark > t.tip",
                    &[&tenant, &deployment],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("retention R0 repair: {err}")))?;
            if repaired > 0 {
                eprintln!(
                    "aster-write-plane: retention floor was above the log tip for \
                     {tenant}/{deployment} — repaired to the tip at lease acquisition \
                     (invariant R0)"
                );
            }
            Ok(row.get::<_, i64>(0) as u64)
        })
    }

    pub fn current_epoch(
        &self,
        tenant: &str,
        deployment: &str,
    ) -> Result<Option<u64>, StoreError> {
        self.block_on(async {
            let client = self.client().await?;
            let row = client
                .query_opt(
                    "SELECT epoch FROM aster.lease WHERE tenant = $1 AND deployment = $2",
                    &[&tenant, &deployment],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("current_epoch: {err}")))?;
            Ok(row.map(|row| row.get::<_, i64>(0) as u64))
        })
    }

    /// Latest committed timestamp — the log tip outside any fence. Inside
    /// the fence the tip is re-read under the lease lock; this value is
    /// only a snapshot hint for capsule issuance.
    pub fn snapshot_ts(&self, tenant: &str, deployment: &str) -> Result<Timestamp, StoreError> {
        self.block_on(async {
            let client = self.client().await?;
            let row = client
                .query_one(
                    "SELECT COALESCE(MAX(ts), 0) FROM aster.log
                     WHERE tenant = $1 AND deployment = $2",
                    &[&tenant, &deployment],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("snapshot_ts: {err}")))?;
            Ok(row.get::<_, i64>(0) as u64)
        })
    }

    pub fn read_point(
        &self,
        tenant: &str,
        deployment: &str,
        key: &DocumentId,
        ts: Timestamp,
    ) -> Result<VersionedDocument, StoreError> {
        self.block_on(async {
            let client = self.client().await?;
            let row = client
                .query_opt(
                    "SELECT ts, document FROM aster.log
                     WHERE tenant = $1 AND deployment = $2 AND key = $3 AND ts <= $4
                     ORDER BY ts DESC LIMIT 1",
                    &[&tenant, &deployment, &key.0, &as_i64(ts)?],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("read_point: {err}")))?;
            // Post-read retention guard for the plane's own MVCC reads —
            // the C3-class rule one seam over (review B4): the floor never
            // lowers, so `floor <= ts` observed AFTER the query proves
            // coverage held throughout it. A snapshot below the floor may
            // have had shadowed revisions compacted away — its absences are
            // not evidence — so refuse as Stale rather than answer.
            let floor: i64 = client
                .query_one(
                    "SELECT COALESCE((SELECT low_watermark FROM aster.retention
                      WHERE tenant = $1 AND deployment = $2), 0)",
                    &[&tenant, &deployment],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("read_point floor: {err}")))?
                .get(0);
            if as_i64(ts)? < floor {
                return Err(StoreError::Stale {
                    requested: ts,
                    latest: floor.max(0) as u64,
                });
            }
            match row {
                None => Ok(VersionedDocument::missing()),
                Some(row) => Ok(VersionedDocument {
                    version: Some(row.get::<_, i64>(0) as u64),
                    document: decode_document(row.get::<_, Option<String>>(1))?,
                }),
            }
        })
    }

    /// First `limit` LIVE keys with the given prefix at snapshot `ts`, in
    /// ascending key order — the `Scan_σ(I, ℓ)` the range certificate
    /// seals. Tombstones and post-snapshot keys never consume limit slots.
    pub fn read_prefix_live(
        &self,
        tenant: &str,
        deployment: &str,
        prefix: &str,
        limit: usize,
        ts: Timestamp,
    ) -> Result<Vec<(DocumentId, VersionedDocument)>, StoreError> {
        self.block_on(async {
            let client = self.client().await?;
            let pattern = format!("{}%", escape_like(prefix));
            let rows = client
                .query(
                    // Bytewise order contract (re-referee F7): a range
                    // certificate's interval semantics assume ONE total key
                    // order, shared by this scan and Rust's `str` ordering.
                    // COLLATE "C" pins the SQL side to byte order — under a
                    // locale collation (the postgres image default) a
                    // Boundary certificate could protect the wrong prefix.
                    "SELECT key, ts, document FROM (
                         SELECT DISTINCT ON (key COLLATE \"C\") key, ts, document
                         FROM aster.log
                         WHERE tenant = $1 AND deployment = $2
                           AND ts <= $3 AND key LIKE $4 ESCAPE '\\'
                         ORDER BY key COLLATE \"C\", ts DESC
                     ) latest
                     WHERE document IS NOT NULL
                     ORDER BY key COLLATE \"C\"
                     LIMIT $5",
                    &[
                        &tenant,
                        &deployment,
                        &as_i64(ts)?,
                        &pattern,
                        &(limit as i64),
                    ],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("read_prefix_live: {err}")))?;
            // Same post-read retention guard as `read_point` (review B4):
            // a snapshot below the floor may scan a compacted history —
            // the certificate-shaped result would be false evidence.
            let floor: i64 = client
                .query_one(
                    "SELECT COALESCE((SELECT low_watermark FROM aster.retention
                      WHERE tenant = $1 AND deployment = $2), 0)",
                    &[&tenant, &deployment],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("read_prefix_live floor: {err}")))?
                .get(0);
            if as_i64(ts)? < floor {
                return Err(StoreError::Stale {
                    requested: ts,
                    latest: floor.max(0) as u64,
                });
            }
            rows.into_iter()
                .map(|row| {
                    Ok((
                        DocumentId::new(row.get::<_, String>(0)),
                        VersionedDocument {
                            version: Some(row.get::<_, i64>(1) as u64),
                            document: decode_document(row.get::<_, Option<String>>(2))?,
                        },
                    ))
                })
                .collect()
        })
    }

    /// The commit fence — the theorem's `CommitFence(ctx, Cap, R, W)` as
    /// one Postgres transaction. See the module docs for the assumption
    /// map; the inline step numbers follow §1.8.
    pub fn commit(&self, input: &FenceInput<'_>) -> Result<CommitOutcome, StoreError> {
        if input.writes.is_empty() {
            return Ok(CommitOutcome::EmptyWriteSet);
        }
        let snapshot = as_i64(input.snapshot)?;
        self.block_on(async {
            let mut client = self.client().await?;
            let tx = client
                .transaction()
                .await
                .map_err(|err| StoreError::Backend(format!("begin fence: {err}")))?;

            // Fence entry: every commit serializes on the lease row lock.
            let lease = tx
                .query_opt(
                    "SELECT epoch FROM aster.lease
                     WHERE tenant = $1 AND deployment = $2 FOR UPDATE",
                    &[&input.tenant, &input.deployment],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("fence lease lock: {err}")))?;
            let lease_epoch = match lease {
                None => {
                    return Err(StoreError::Backend(
                        "no lease acquired for this deployment".into(),
                    ))
                }
                Some(row) => row.get::<_, i64>(0) as u64,
            };
            // V2: both the committer and the capsule context must match the
            // live lease epoch.
            if lease_epoch != input.committer_epoch || input.context_epoch != lease_epoch {
                // Every rejection path rolls back EXPLICITLY and awaited
                // (re-referee F15): dropping the transaction also rolls
                // back, but detached — the awaited form confirms the
                // server released the lease/retention row locks before the
                // caller sees the outcome, so a measured rejection latency
                // includes completed rollback.
                tx.rollback()
                    .await
                    .map_err(|err| StoreError::Backend(format!("fence rollback: {err}")))?;
                return Ok(CommitOutcome::StaleEpoch { lease_epoch });
            }

            // Stable horizon h, read only after the epoch fence.
            let h: i64 = tx
                .query_one(
                    "SELECT COALESCE(MAX(ts), 0) FROM aster.log
                     WHERE tenant = $1 AND deployment = $2",
                    &[&input.tenant, &input.deployment],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("fence horizon: {err}")))?
                .get(0);
            if snapshot > h {
                tx.rollback()
                    .await
                    .map_err(|err| StoreError::Backend(format!("fence rollback: {err}")))?;
                return Ok(CommitOutcome::SnapshotBeyondHorizon {
                    horizon: h as u64,
                });
            }

            // V3 (coverage form) + retention pin: lock the watermark row so
            // GC cannot advance past s while we validate (Lemma R).
            let g: i64 = tx
                .query_one(
                    "SELECT low_watermark FROM aster.retention
                     WHERE tenant = $1 AND deployment = $2 FOR UPDATE",
                    &[&input.tenant, &input.deployment],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("fence retention pin: {err}")))?
                .get(0);
            if snapshot < g {
                tx.rollback()
                    .await
                    .map_err(|err| StoreError::Backend(format!("fence rollback: {err}")))?;
                return Ok(CommitOutcome::RetentionViolated {
                    low_watermark: g as u64,
                });
            }

            // V4: no committed write in (s, h] may intersect a declared
            // observation — point keys first, then range windows.
            if !input.read_points.is_empty() {
                let keys: Vec<&str> = input
                    .read_points
                    .iter()
                    .map(|key| key.0.as_str())
                    .collect();
                let conflict = tx
                    .query_opt(
                        "SELECT key FROM aster.log
                         WHERE tenant = $1 AND deployment = $2
                           AND ts > $3 AND ts <= $4 AND key = ANY($5)
                         LIMIT 1",
                        &[&input.tenant, &input.deployment, &snapshot, &h, &keys],
                    )
                    .await
                    .map_err(|err| StoreError::Backend(format!("fence point scan: {err}")))?;
                if let Some(row) = conflict {
                    let key = DocumentId::new(row.get::<_, String>(0));
                    tx.rollback()
                        .await
                        .map_err(|err| StoreError::Backend(format!("fence rollback: {err}")))?;
                    return Ok(CommitOutcome::Conflict { key });
                }
            }
            if !input.read_windows.is_empty() {
                let rows = tx
                    .query(
                        "SELECT DISTINCT key FROM aster.log
                         WHERE tenant = $1 AND deployment = $2
                           AND ts > $3 AND ts <= $4",
                        &[&input.tenant, &input.deployment, &snapshot, &h],
                    )
                    .await
                    .map_err(|err| StoreError::Backend(format!("fence window scan: {err}")))?;
                for row in rows {
                    let key = DocumentId::new(row.get::<_, String>(0));
                    if input.read_windows.iter().any(|window| window.contains(&key)) {
                        tx.rollback()
                            .await
                            .map_err(|err| StoreError::Backend(format!("fence rollback: {err}")))?;
                        return Ok(CommitOutcome::Conflict { key });
                    }
                }
            }

            // Atomic append at fresh c = h + 1; timestamp assignment is part
            // of the same transaction as the epoch check.
            let ts_new = h + 1;
            for (key, document) in input.writes {
                let encoded = document
                    .as_ref()
                    .map(|document| {
                        serde_json::to_string(document).map_err(|err| {
                            StoreError::Backend(format!("encode document: {err}"))
                        })
                    })
                    .transpose()?;
                tx.execute(
                    "INSERT INTO aster.log (tenant, deployment, ts, key, epoch, document)
                     VALUES ($1, $2, $3, $4, $5, $6)",
                    &[
                        &input.tenant,
                        &input.deployment,
                        &ts_new,
                        &key.0,
                        &(input.committer_epoch as i64),
                        &encoded,
                    ],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("fence append: {err}")))?;
            }
            tx.commit()
                .await
                .map_err(|err| StoreError::Backend(format!("fence commit: {err}")))?;
            Ok(CommitOutcome::Committed { ts: ts_new as u64 })
        })
    }

    /// GC: raise the retention low-watermark (never lowers, never advances
    /// past the log tip) and compact revisions at or below it that are
    /// shadowed by a newer revision of the same key. Serialized with
    /// in-flight fences via the retention row lock — this is the sweeper
    /// half of Lemma R's pin.
    pub fn advance_retention(
        &self,
        tenant: &str,
        deployment: &str,
        low_watermark: Timestamp,
    ) -> Result<Timestamp, StoreError> {
        let requested = as_i64(low_watermark)?;
        self.block_on(async {
            let mut client = self.client().await?;
            let tx = client
                .transaction()
                .await
                .map_err(|err| StoreError::Backend(format!("begin retention: {err}")))?;
            let current: i64 = tx
                .query_one(
                    "SELECT low_watermark FROM aster.retention
                     WHERE tenant = $1 AND deployment = $2 FOR UPDATE",
                    &[&tenant, &deployment],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("retention lock: {err}")))?
                .get(0);
            // Clamp to the log tip (stable here: fences also take the
            // retention row lock before appending). A watermark past the tip
            // claims retention over events that don't exist yet and would
            // wedge commit admission permanently — every future fence's
            // snapshot floor would sit below it, and monotonicity forbids
            // walking it back.
            let tip: i64 = tx
                .query_one(
                    "SELECT COALESCE(MAX(ts), 0) FROM aster.log
                     WHERE tenant = $1 AND deployment = $2",
                    &[&tenant, &deployment],
                )
                .await
                .map_err(|err| StoreError::Backend(format!("retention tip: {err}")))?
                .get(0);
            let effective = current.max(requested.min(tip));
            tx.execute(
                "UPDATE aster.retention SET low_watermark = $3
                 WHERE tenant = $1 AND deployment = $2",
                &[&tenant, &deployment, &effective],
            )
            .await
            .map_err(|err| StoreError::Backend(format!("retention update: {err}")))?;
            tx.execute(
                "DELETE FROM aster.log l
                 WHERE l.tenant = $1 AND l.deployment = $2 AND l.ts <= $3
                   AND EXISTS (
                       SELECT 1 FROM aster.log n
                       WHERE n.tenant = l.tenant AND n.deployment = l.deployment
                         AND n.key = l.key AND n.ts > l.ts AND n.ts <= $3
                   )",
                &[&tenant, &deployment, &effective],
            )
            .await
            .map_err(|err| StoreError::Backend(format!("retention compact: {err}")))?;
            tx.commit()
                .await
                .map_err(|err| StoreError::Backend(format!("retention commit: {err}")))?;
            Ok(effective as u64)
        })
    }
}

/// `WritePlane` IS the theorem's commit fence; the trait impl exposes the
/// inherent methods through the backend-agnostic seam the brokerd binary
/// dispatches on (memory mode gets `aster_broker::MemoryFence` instead).
impl CommitFence for WritePlane {
    fn acquire_lease(
        &self,
        tenant: &str,
        deployment: &str,
        holder: &str,
    ) -> Result<u64, StoreError> {
        WritePlane::acquire_lease(self, tenant, deployment, holder)
    }

    fn commit(&self, input: &FenceInput<'_>) -> Result<CommitOutcome, StoreError> {
        WritePlane::commit(self, input)
    }
}

/// Parse the URL and stamp the short TCP keepalive timers on the connect
/// config — every pooled connection the write plane opens gets them.
fn parse_pg_config(url: &str) -> Result<PgConfig, StoreError> {
    let mut pg_config: PgConfig = url
        .parse()
        .map_err(|err: tokio_postgres::Error| StoreError::Backend(format!("bad url: {err}")))?;
    pg_config
        .keepalives(true)
        .keepalives_idle(TCP_KEEPALIVE_IDLE)
        .keepalives_interval(TCP_KEEPALIVE_INTERVAL)
        .keepalives_retries(TCP_KEEPALIVE_RETRIES);
    Ok(pg_config)
}

fn as_i64(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("timestamp {value} exceeds i64 range")))
}

fn decode_document(raw: Option<String>) -> Result<Option<Document>, StoreError> {
    raw.map(|json| {
        serde_json::from_str(&json)
            .map_err(|err| StoreError::Backend(format!("decode document: {err}")))
    })
    .transpose()
}

/// Escape `%`, `_`, and `\` so a caller-supplied prefix is matched
/// literally by `LIKE ... ESCAPE '\'`.
fn escape_like(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len());
    for ch in prefix.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_like_neutralizes_wildcards() {
        assert_eq!(escape_like("docs/"), "docs/");
        assert_eq!(escape_like("do%cs_"), "do\\%cs\\_");
        assert_eq!(escape_like("a\\b"), "a\\\\b");
    }

    #[test]
    fn connect_config_carries_short_tcp_keepalives() {
        let pg_config =
            parse_pg_config("postgres://stub:stub@127.0.0.1:1/stub").expect("parse url");
        assert!(pg_config.get_keepalives());
        assert_eq!(pg_config.get_keepalives_idle(), TCP_KEEPALIVE_IDLE);
        assert_eq!(
            pg_config.get_keepalives_interval(),
            Some(TCP_KEEPALIVE_INTERVAL)
        );
        assert_eq!(pg_config.get_keepalives_retries(), Some(TCP_KEEPALIVE_RETRIES));
    }
}

#[cfg(all(test, feature = "postgres-it"))]
mod pg_it {
    use super::*;

    /// Both per-checkout GUCs must be stamped on every pooled session.
    /// The idle-in-transaction bound is the load-bearing half: without it
    /// a session idle mid-fence holds the lease/retention row locks
    /// unbounded (statement_timeout never fires while no statement runs).
    /// The wedge-resolution proof lives in `write_plane_it.rs`
    /// (`idle_wedged_lock_holder_is_killed_so_failover_resumes`); this
    /// test pins the config → session wiring it relies on.
    #[test]
    fn client_checkout_stamps_statement_and_idle_timeouts() {
        let url = std::env::var("ASTER_DB_URL")
            .expect("set ASTER_DB_URL to run postgres-it tests");
        // Millisecond values chosen to not reduce to whole seconds so
        // SHOW echoes them back in ms and the assertion stays exact.
        let plane = WritePlane::connect(WritePlaneConfig {
            url,
            statement_timeout: Duration::from_millis(4321),
            idle_in_transaction_session_timeout: Duration::from_millis(1234),
            ..WritePlaneConfig::default()
        })
        .expect("connect");
        let (statement, idle) = plane.block_on(async {
            let client = plane.client().await.expect("checkout");
            let statement: String = client
                .query_one("SHOW statement_timeout", &[])
                .await
                .expect("show statement_timeout")
                .get(0);
            let idle: String = client
                .query_one("SHOW idle_in_transaction_session_timeout", &[])
                .await
                .expect("show idle_in_transaction_session_timeout")
                .get(0);
            (statement, idle)
        });
        assert_eq!(statement, "4321ms");
        assert_eq!(idle, "1234ms");
    }
}
