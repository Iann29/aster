# Postgres adapter plan (v0.4) — DONE

The v0.3 broker read from an in-memory `MvccStore`. This document captured
the five-commit plan to replace that with a real Postgres adapter so the
broker could serve the same database the Convex backend writes to. **All
five commits are now merged on `main`.**

## What landed (5/5)

- **Commit 1** ([PR #2](https://github.com/Iann29/aster/pull/2)) —
  `CapsuleStore` trait + `StoreError` in `crates/broker/src/store.rs`.
  `LocalCapsuleBroker` generic over `S: CapsuleStore`. Blanket impls
  cover `&MvccStore`, `MvccStore`, `Arc<S>`. No behaviour change.
- **Commit 2** ([PR #3](https://github.com/Iann29/aster/pull/3)) —
  `aster_brokerd` uses `Arc<dyn CapsuleStore + Send + Sync>`. Same
  behaviour, narrower type — the Postgres impl plugs in here.
- **Commit 3** ([PR #4](https://github.com/Iann29/aster/pull/4)) —
  `crates/store-postgres/` scaffold with `tokio-postgres` +
  `deadpool-postgres`, sync API + internal tokio runtime, stub queries.
- **Commit 4** ([PR #7](https://github.com/Iann29/aster/pull/7)) —
  real SQL: `snapshot_ts` queries `documents` + the `max_repeatable_ts`
  fence; `read_point` does a direct `DISTINCT ON (id)` on `documents`;
  `read_prefix` is a bounded variant. **8 integration tests** against
  `postgres:16` cover snapshot_ts, point reads at multiple ts values,
  prefix scans honouring limit + ts, malformed-id classification, and
  unreachable-server handling.
- **Commit 5** ([PR #6](https://github.com/Iann29/aster/pull/6)) — CI
  lane with `postgres:16` service container running the gated
  `postgres-it` tests with `--test-threads=1`.

## What's still NOT wired (the open work)

The plan was about getting the **store** to read from real Postgres.
That's done. To get a **Convex app actually executing through Aster**,
three more pieces have to land:

1. **`ASTER_STORE` dispatch in `aster_brokerd`.** Today `aster_brokerd`
   constructs `Arc<dyn CapsuleStore>` hardcoded to `Arc::new(MvccStore::new())`.
   Adding an env-driven dispatch (`ASTER_STORE=memory|postgres`,
   `ASTER_DB_URL_FILE > ASTER_DB_URL > error`) is one small commit; the
   `PostgresCapsuleStore::connect` plus the existing `Arc<dyn>` slot
   means the brokerd binary just needs to choose at startup. **The
   `ASTER_DB_URL` env is already what the postgres-it lane uses, so the
   plumbing is half-there.**
2. **IDv6 ↔ Aster `DocumentId` mapping.** `Convex.asyncSyscall("1.0/get")`
   ([PR #8](https://github.com/Iann29/aster/pull/8)) accepts `id` as a
   string and feeds it to `read_point` verbatim. The real Convex CLI
   hands a base32-encoded `IDv6` (table-number prefix + 16-byte
   InternalId). The broker needs:
   - A port of `crates/value/src/id_v6.rs` (base32 codec).
   - A table-mapping cache (`_tables` system tablet read on startup,
     refresh on schema-change events) so it can translate
     `(table_number, internal_id)` ↔ `(table_id, id)` for the SQL.
3. **Convex module loader.** Today the v8cell runs an `async function
   main()` defined in a single source string. A real Convex module is
   `npx convex deploy`-bundled with `_generated/server.ts`, schema,
   multiple exports. The cell needs to drive
   `module.<funcName>.invokeQuery(JSON.stringify(args))` the way
   Convex's own runner does (see upstream
   `crates/isolate/src/environment/udf/mod.rs:818-940`). The
   ["Convex JS runtime" research memo from PR #8's trail](https://github.com/Iann29/aster/pull/8#issue-comment)
   names this as the largest remaining piece.

The Synapse-side pair (cell-on-demand spawn) is tracked in the Synapse
repo's [`docs/ASTER_INTEGRATION.md`](https://github.com/Iann29/convex-synapse/blob/main/docs/ASTER_INTEGRATION.md);
it's the operator-facing endpoint that hands a JS source + args to the
cell when an HTTP request arrives.

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
