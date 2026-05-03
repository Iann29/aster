-- Minimal Convex Postgres schema fixture for Aster's integration tests.
--
-- Subset of get-convex/convex-backend's crates/postgres/src/sql.rs DDL —
-- enough to exercise read_point + snapshot_ts. We skip the multitenant
-- variant (no leading instance_name column) and only create the four
-- tables Aster actually queries.
--
-- Schema name is configurable per-test (different test → different
-- schema → no cross-test interference). Default `convex_dev`.

CREATE SCHEMA IF NOT EXISTS convex_dev;

CREATE TABLE IF NOT EXISTS convex_dev.documents (
    id           BYTEA  NOT NULL,
    ts           BIGINT NOT NULL,
    table_id     BYTEA  NOT NULL,
    json_value   BYTEA  NOT NULL,
    deleted      BOOLEAN DEFAULT false,
    prev_ts      BIGINT,
    PRIMARY KEY (ts, table_id, id)
);

CREATE INDEX IF NOT EXISTS documents_by_table_and_id
    ON convex_dev.documents (table_id, id, ts);

CREATE INDEX IF NOT EXISTS documents_by_table_ts_and_id
    ON convex_dev.documents (table_id, ts, id);

CREATE TABLE IF NOT EXISTS convex_dev.indexes (
    index_id    BYTEA NOT NULL,
    ts          BIGINT NOT NULL,
    key_prefix  BYTEA NOT NULL,
    key_suffix  BYTEA NULL,
    key_sha256  BYTEA NOT NULL,
    deleted     BOOLEAN,
    table_id    BYTEA NULL,
    document_id BYTEA NULL,
    PRIMARY KEY (index_id, key_sha256, ts)
);

CREATE INDEX IF NOT EXISTS indexes_by_index_id_key_prefix_key_sha256
    ON convex_dev.indexes (index_id, key_prefix, key_sha256);

CREATE TABLE IF NOT EXISTS convex_dev.persistence_globals (
    key        TEXT  PRIMARY KEY,
    json_value BYTEA NOT NULL
);

CREATE TABLE IF NOT EXISTS convex_dev.leases (
    id BIGINT PRIMARY KEY,
    ts BIGINT NOT NULL
);
