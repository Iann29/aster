# Postgres adapter plan (v0.4)

The v0.3 broker reads from an in-memory `MvccStore`. This document is the
implementation plan for replacing that with a real Postgres adapter so the
broker can serve the same database the Convex backend writes to.

The plan is staged into five commits, each independently
`cargo test --workspace`-able so CI stays green at every step.

## What's already done

- **Commit 1** — `CapsuleStore` trait + `StoreError` in
  `crates/broker/src/store.rs`. `LocalCapsuleBroker` generic over `S:
  CapsuleStore`. Blanket impls cover `&MvccStore`, `MvccStore`, `Arc<S>`.
- **Commit 2** — `aster_brokerd` uses `Arc<dyn CapsuleStore + Send + Sync>`.
  Same behaviour, narrower type — the Postgres impl plugs in here.
- **Commit 3** — `crates/store-postgres/` scaffold with `tokio-postgres` +
  `deadpool-postgres`, sync API + internal tokio runtime, stub queries.
- **Commit 4** — real SQL: `snapshot_ts` queries `documents` + the
  `max_repeatable_ts` fence; `read_point` does a direct `DISTINCT ON (id)`
  on `documents`; `read_prefix` is a bounded variant. Integration tests
  against `postgres:16` cover snapshot_ts, point reads at multiple ts
  values, prefix scans honouring limit + ts, malformed-id classification,
  and unreachable-server handling.
- **Commit 5** — CI lane with `postgres:16` service container running the
  gated `postgres-it` tests.

## Remaining work

### Commit 2 — push trait through brokerd
- `crates/ipc/src/bin/aster_brokerd.rs`: `ProcessBroker.store: MvccStore` →
  `store: Arc<dyn CapsuleStore + Send + Sync>` (or generic `S: CapsuleStore`).
- Construction path stays in-memory — only the type narrows.
- CI green: same store, just behind `dyn`.

### Commit 3 — new crate `crates/store-postgres/`
- Workspace member with `tokio-postgres`, `deadpool-postgres`,
  `tokio` (`rt-multi-thread`).
- `PostgresCapsuleStore`: owns a `tokio::runtime::Runtime` + a
  `deadpool_postgres::Pool`, exposes a sync `impl CapsuleStore` that
  `block_on`s internally. Sync broker, async island.
- Schema queries are stubs returning `StoreError::Backend("not implemented")`
  until Commit 4. Convex schema reference is in
  `docs/CONVEX_POSTGRES_REFERENCE.md`.
- Tests gated behind feature `postgres-it` so default CI without a DB
  still passes.

### Commit 4 — wire dispatch in brokerd
- Add `ASTER_STORE` env (`memory` | `postgres`, default `memory`).
- Add URL discovery: `ASTER_DB_URL_FILE` > `ASTER_DB_URL`. Hard error if
  `ASTER_STORE=postgres` but neither is set.
- Implement the actual SQL from `docs/CONVEX_POSTGRES_REFERENCE.md`:
  `read_point` is an index point lookup via `by_id`,
  `read_prefix` is a bounded `INDEX_QUERIES` range scan,
  `snapshot_ts` is `max(documents.ts, persistence_globals['max_repeatable_ts'])`.
- Default `memory` keeps `compose.smoke.yml` and `process_boundary.rs` green.

### Commit 5 — CI lane
- Add `postgres-it` job to `.github/workflows/ci.yml` that spins up
  `postgres:16` as a service container and runs
  `cargo test -p aster-store-postgres --features postgres-it`.

## Decisions locked in

- **Sync broker, async island.** brokerd stays sync; the Postgres store
  owns its own tokio runtime. Rationale: the cell-facing crates already
  link `aster-ipc` and going tokio everywhere drags it into a short-lived
  short-running cell binary for no win.
- **`tokio-postgres` + `deadpool-postgres`.** No `sqlx::query!` macro
  (CI hostility, requires live DB at compile time). Hand-written SQL
  matches Aster's "we know our schema" stance.
- **URL discovery.** File path > env var. File path appears nowhere in
  `ps` output and matches the Synapse / k8s secret-mount idiom.
- **Pool defaults.** Max 16 connections, min idle 2, connect timeout 5s,
  per-checkout `SET statement_timeout = 30s`. All overridable via env.
- **Cell-facing API.** `CapsuleBrokerClient` does **not** change. The
  store error story collapses into `BrokerError::Remote("backend: ...")`.

## Gotchas the Commit 4 implementer must read first

1. **No `from_ts`/`to_ts` window.** Convex MVCC is "latest revision with
   `ts <= T`", not a range overlap. Use `DISTINCT ON (key) ORDER BY ts DESC`.
2. **`db.get(id)` is not `SELECT FROM documents`.** It's a `by_id` index
   point lookup. Going through `documents` directly diverges on retention.
3. **`table_id` is a 16-byte tablet UUID, not the table name.** Must read
   the system table mapping (stored as documents in the system tablet)
   before any user query works.
4. **Tombstones live in both `documents` and `indexes`.** Don't dereference
   `document_id` until you check `deleted = false`.
5. **`key_prefix` truncated at 2500 bytes.** Long index keys produce rows
   that share prefix and must be re-sorted in memory by full key.
6. **`json_value` is `BYTEA` (not `JSONB`).** It's serialized
   `ConvexValue` — deserialize in Rust, no PG JSON operators.
7. **Snapshot-ts source.** On startup,
   `T = max(SELECT ts FROM documents ORDER BY ts DESC LIMIT 1,
   SELECT json_value FROM persistence_globals WHERE key='max_repeatable_ts')`.
   Do **not** invent a timestamp.
8. **Retention validator.** Every read must check
   `min_document_snapshot_ts` / `min_index_snapshot_ts` from
   `persistence_globals`; reads outside the window must surface as
   `StoreError::Stale`.
9. **Planner hints are load-bearing.** Without `pg_hint_plan` (or
   equivalent forced index-only plans), Postgres picks seq scans on
   `documents` at scale. Either install the extension or replicate the
   plan choice manually.
10. **Multi-schema layout.** Tables live in `@db_name`, not `public`.
    Use `SET search_path` per checkout or qualify every query.

## Out of scope here (follow-ups)

- Multi-tenant routing inside one brokerd. Today brokerd is hard-pinned
  to one `(tenant, deployment, snapshot_ts)`. Postgres makes lifting
  that pin attractive but it's a follow-up.
- Switching brokerd's accept loop to threaded/async. Recommended after
  the DB lands so we have measurable contention to size against.
- Replacing JSON IPC with protobuf (the `proto/` dir hints at it but
  nothing lands wire changes here).
- Real seal-key provenance. `derive_for_tests` survives this PR; a
  proper `CapsuleSealKey::from_secret_file` is a separate slice.
