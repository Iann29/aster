# Convex Postgres reference for Aster

This document is a frozen reference of the Postgres schema and read-path SQL
the Convex backend uses, captured from `get-convex/convex-backend@main`. It is
the spec the `aster-store-postgres` adapter (commit 4/5 of the Postgres
adapter plan) implements against.

All SQL/DDL below is verbatim from `crates/postgres/src/sql.rs` upstream.
The backend creates everything inside a per-deployment **schema**
(`@db_name`, e.g. `convex_self_hosted`), not the public schema. The
`multitenant` flag adds an `instance_name TEXT` column at the front of
every PK and index — single-tenant self-hosted deployments don't have it,
so Aster v0.4 ignores it.

## Schema (DDL, single-tenant form)

```sql
CREATE TABLE documents (
    id           BYTEA  NOT NULL,                -- 16-byte InternalId
    ts           BIGINT NOT NULL,                -- write timestamp (microseconds since epoch)
    table_id     BYTEA  NOT NULL,                -- 16-byte tablet id (NOT the table name)
    json_value   BYTEA  NOT NULL,                -- serialized ConvexValue (JSON bytes)
    deleted      BOOLEAN DEFAULT false,          -- tombstone flag
    prev_ts      BIGINT,                         -- ts of previous revision of (table_id, id), NULL on first insert
    PRIMARY KEY (ts, table_id, id)
);
CREATE INDEX documents_by_table_and_id    ON documents (table_id, id, ts);
CREATE INDEX documents_by_table_ts_and_id ON documents (table_id, ts, id);

CREATE TABLE indexes (
    index_id    BYTEA NOT NULL,                  -- which index this row belongs to
    ts          BIGINT NOT NULL,
    key_prefix  BYTEA NOT NULL,                  -- first 2500 bytes of the IndexKey
    key_suffix  BYTEA NULL,                      -- remainder, NULL when key fits in prefix
    key_sha256  BYTEA NOT NULL,                  -- sha256 of full key (disambiguates prefix collisions)
    deleted     BOOLEAN,
    table_id    BYTEA NULL,                      -- populated iff deleted = false
    document_id BYTEA NULL,                      -- populated iff deleted = false
    PRIMARY KEY (index_id, key_sha256, ts)
);
CREATE INDEX indexes_by_index_id_key_prefix_key_sha256 ON indexes (index_id, key_prefix, key_sha256);

CREATE TABLE leases               (id BIGINT PRIMARY KEY, ts BIGINT NOT NULL);
CREATE TABLE read_only            (id BIGINT PRIMARY KEY);
CREATE TABLE persistence_globals  (key TEXT PRIMARY KEY, json_value BYTEA NOT NULL);
```

Notes:
- There is **no `_creationTime` column** at this layer — that's a userland
  JSON field inside `json_value`.
- There is **no explicit `from_ts/to_ts` window**. The MVCC chain is
  encoded as a linked list via `prev_ts` (newer row points back to its
  predecessor's `ts`).
- The "table" Convex devs see is mapped to a 16-byte `table_id` (a
  *tablet*) by the in-memory `TableMapping`. Aster has to load that
  mapping from the metadata documents.

## Reading "at timestamp T"

Convex does **not** use a `from_ts <= T AND (to_ts IS NULL OR to_ts > T)`
window. It uses **"latest revision with `ts <= T`"**, i.e. for each
`(table_id, id)` (or each index key) take the row with the highest `ts`
such that `ts <= T`, then drop it if `deleted = true`.

### `db.get(id)` — point read

At the database/transaction layer this is **not** a direct `documents`
query. It's a `by_id` index lookup
(`crates/database/src/transaction.rs::Transaction::get_inner` →
`IndexKey::new(vec![], id.into())` → `index_scan` with a singleton
interval). The SQL is the same template used for all index scans
(`sql::INDEX_QUERIES` in `sql.rs:783-926` upstream):

```sql
SELECT A.index_id, A.key_prefix, A.key_sha256, A.key_suffix, A.ts, A.deleted,
       A.document_id, D.table_id, D.json_value, D.prev_ts
FROM (
    SELECT DISTINCT ON (key_prefix, key_sha256)
        index_id, key_prefix, key_sha256, key_suffix, ts, deleted, document_id, table_id
    FROM indexes
    WHERE index_id = $1
      AND ts <= $2                              -- read timestamp T
      AND (key_prefix, key_sha256) >= ($3, $4)  -- lower bound (Included for point get)
      AND (key_prefix, key_sha256) <= ($5, $6)  -- upper bound (Included for point get)
    ORDER BY key_prefix ASC, key_sha256 ASC, ts DESC
    LIMIT $7
) A
LEFT JOIN documents D
    ON D.ts = A.ts AND D.table_id = A.table_id AND D.id = A.document_id
ORDER BY key_prefix ASC, key_sha256 ASC;
```

`DISTINCT ON (key_prefix, key_sha256)` + `ORDER BY ... ts DESC` collapses
to the latest revision per key. The caller drops rows where
`A.deleted = true`.

### `db.query("table").<filter>` — range scan

Same template; `index_id` is whichever index the planner chose (a
user-defined index, or `by_id` / `by_creation_time` for unindexed
scans), and the bounds are derived from the filter. Convex pre-builds 36
variants of this query for `(lower_bound, upper_bound, order)`
combinations (`INDEX_QUERIES` LazyLock).

### Walking the document log (changefeed-style)

`crates/postgres/src/sql.rs::load_docs_by_ts_page_asc`:

```sql
SELECT D.id, D.ts, D.table_id, D.json_value, D.deleted, D.prev_ts
FROM documents D
WHERE (D.ts, D.table_id, D.id) > ($1, $2, $3)   -- cursor
  AND D.ts < $4                                 -- upper bound
ORDER BY D.ts ASC, D.table_id ASC, D.id ASC
LIMIT $5;
```

This is what Aster wants for tailing changes; uses the
`documents_pkey (ts, table_id, id)` index.

### Picking T (snapshot timestamp)

A read transaction reads at a `RepeatableTimestamp`. The backend gets it
from the in-memory `SnapshotManager`
(`crates/database/src/snapshot_manager.rs::latest_ts`), which is the
timestamp of the most recent successful commit. **It is not derived from
SQL.** On cold start the backend computes it via
`PersistenceReader::max_ts()`
(`crates/common/src/persistence.rs:556-575`):

```sql
-- 1. Highest committed write
SELECT id, ts, table_id, json_value, deleted, prev_ts
FROM documents
ORDER BY ts DESC, table_id DESC, id DESC
LIMIT 1;

-- 2. Highest fenced timestamp
SELECT json_value FROM persistence_globals WHERE key = 'max_repeatable_ts';
```

`T = max(those two)`. Aster must do the same on startup, then advance
`T` from its own commit notifications (or by polling `documents.ts` /
the `persistence_globals` row). A Convex backend holding the lease will
be advancing `max_repeatable_ts`; if Aster reads concurrently with the
official backend, **the lease in the `leases` table is what coordinates
writers** — readers don't take it but must respect retention.

## Gotchas (read these before coding)

1. **`ts` is `i64` microseconds since epoch**, monotonic per-deployment,
   allocated by the committer — not by Postgres. Two rows can share `ts`
   only if `(table_id, id)` differ (it's part of the PK).
2. **No NULL `to_ts`.** Determining "is this row live at T" requires a
   `DISTINCT ON / ORDER BY ts DESC LIMIT 1` per key, then checking
   `deleted`. There is no row that says "this is the current version".
3. **Tombstones live in both tables.** A delete writes a new `documents`
   row with `deleted = true` and a new `indexes` row with
   `deleted = true, table_id = NULL, document_id = NULL`. Don't
   dereference `document_id` until you check `deleted`.
4. **`prev_ts` is the linked-list pointer**, not a window end. For
   "give me the version that was live just before T", use
   `previous_revisions` SQL (`prev_rev` in `sql.rs:973`):
   `WHERE table_id=$1 AND id=$2 AND ts < $3 ORDER BY ts DESC LIMIT 1`.
   Convex chunks 8 of these into one round-trip (`prev_rev_chunk`).
5. **`db.get(id)` does not query `documents` directly.** It goes through
   the `by_id` index, which means an `indexes` row exists for every
   live document. If you bypass the index and read `documents` by
   `(table_id, id)`, you'll diverge from Convex semantics for retention
   (a doc may be retention-deleted from `indexes` but still in
   `documents`, or vice versa — there are *two* retention cursors:
   `confirmed_deleted_ts` for indexes,
   `document_confirmed_deleted_ts` for documents).
6. **`table_id` is a tablet UUID, not a table name.** Aster must read
   the system table mapping (stored as documents in a system tablet) to
   translate names ↔ tablet ids before any user query works.
7. **`key_prefix` is truncated to 2500 bytes** (Postgres PK length
   limit). For long index keys you get rows that share
   `(index_id, key_prefix)` and must be re-sorted in memory by full key
   (`key_prefix || key_suffix`). The postgres crate does this in
   `_index_scan_inner` (`lib.rs:1178-1237` upstream). Don't naively
   trust the SQL order for long keys.
8. **Multi-schema layout.** All tables live in `@db_name` (a Postgres
   schema, default name comes from the deployment). Hardcode-search-path
   or qualify every query — there is nothing in `public`.
9. **The `leases` table is a fencing token, not advisory locking.** A
   row with `(id=1, ts=N)` says "the writer with token N is current".
   Aster as a read-only reader doesn't take it, but if Aster ever writes
   it must `UPDATE leases SET ts=$new WHERE id=1 AND ts<$new` and verify
   the row count, then run every write transaction with
   `SELECT 1 FROM leases WHERE id=1 AND ts=$new FOR SHARE` as a
   precondition (`sql::lease_precond`).
10. **Retention validator.** Every read takes a `retention_validator`
    that re-checks `min_document_snapshot_ts` /
    `min_index_snapshot_ts` (stored in `persistence_globals`) before
    yielding rows; if `T < min_snapshot_ts` the read fails with "out of
    retention". Aster needs the same check or it'll silently return
    half-vacuumed results.
11. **Planner hints are load-bearing.** Every read query starts with a
    `pg_hint_plan` block (`Set(enable_seqscan OFF)` etc.,
    `Set(plan_cache_mode force_generic_plan)`). Without `pg_hint_plan`
    installed (or equivalent index-scan-only plans), Postgres frequently
    picks seq scans on the documents table at scale. Either install the
    extension or replicate the index-only plan choice.
12. **`json_value` is `BYTEA`, not `JSONB`.** It's the serialized Convex
    value (a JSON encoding of `ConvexValue`, which is a superset of JSON
    — sets, bytes, bigints, etc.). Don't try to filter it with Postgres
    JSON operators; deserialize in Rust.

## Critical upstream files

- `crates/postgres/src/sql.rs` — all DDL and read/write SQL templates,
  including `INDEX_QUERIES` and `load_docs_by_ts_page_asc`
- `crates/postgres/src/lib.rs` — `PostgresReader::_index_scan_inner`
  (lib.rs:1161), `previous_revisions` (lib.rs:1472), `index_query`
  builder (lib.rs:2051), and the long-key re-sort logic
- `crates/common/src/persistence.rs` — `PersistenceReader` trait
  (line 411), `max_ts` (line 556), `RepeatablePersistence` (line 610),
  retention semantics
- `crates/database/src/transaction.rs` — `Transaction::get_inner`
  (line 881) showing that `db.get(id)` is a `by_id` index point lookup,
  not a direct `documents` query
- `crates/database/src/snapshot_manager.rs` —
  `SnapshotManager::latest_ts` (line 593),
  `persisted_max_repeatable_ts` (line 652) — how the in-memory committer
  picks T

## Mapping to Aster's `CapsuleStore`

| Aster method | Convex SQL (commit 4/5 will implement) |
|---|---|
| `snapshot_ts()` | `max(SELECT ts FROM documents ORDER BY ts DESC LIMIT 1, persistence_globals.max_repeatable_ts)` |
| `read_point(key, ts)` | `INDEX_QUERIES`-style point lookup against `by_id` index where `index_id` is the `by_id` of the requested table's tablet, key encoded from `DocumentId` |
| `read_prefix(prefix, limit, ts)` | `INDEX_QUERIES`-style range scan with `(key_prefix, key_sha256)` bounds derived from the prefix |
| `build_capsule(prewarm)` | Default trait impl loops `read_point`. v0.5+ override batches into one query. |

The one Aster concept that doesn't map directly is `DocumentId(String)`.
Convex's `InternalId` is a 16-byte binary; `table_id` is a separate
16-byte tablet UUID. Aster's adapter has to define the canonical text
encoding (probably `<tablet_hex>/<id_hex>` or `<table_name>/<id>` after
loading the table mapping) and assert it in a property test, otherwise
capsule hashes will diverge from cell expectations.

## Out of scope

- Writes. Aster is a reader. Writes go through the Convex backend's
  committer; the `leases` table is mentioned only so a future writer
  knows where to look.
- Streaming changefeed. `load_docs_by_ts_page_asc` is documented for
  completeness but Aster v0.4 polls instead.
- TLS to Postgres. v0.4 ships with `NoTls`. Operators wanting TLS run
  pgbouncer in front; a `tokio-postgres-rustls` integration is a
  follow-up.
