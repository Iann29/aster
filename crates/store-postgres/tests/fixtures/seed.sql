-- Seed a minimal document fixture. The Aster integration test will
-- read these back through PostgresCapsuleStore::read_point.
--
-- Convex's table_id is a 16-byte tablet UUID. For the fixture we hard-
-- code one — `0123456789abcdef0123456789abcdef` — and use it for all
-- documents. The id is a 16-byte InternalId, also hex-encoded here for
-- determinism.
--
-- Document body: `{"name": "ian"}`. The real Convex uses a custom
-- encoding (msgpack-ish over BYTEA), but for this fixture we use raw
-- JSON bytes — Aster's adapter just hands the bytes back to the cell,
-- it doesn't parse them.

-- One revision of one document.
INSERT INTO convex_dev.documents (id, ts, table_id, json_value, deleted, prev_ts)
VALUES (
    decode('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'hex'),
    100,
    decode('0123456789abcdef0123456789abcdef', 'hex'),
    convert_to('{"name":"ian","_id":"messages|aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}', 'UTF8'),
    false,
    NULL
);

-- A second document at a later ts so snapshot_ts has something
-- non-trivial to find.
INSERT INTO convex_dev.documents (id, ts, table_id, json_value, deleted, prev_ts)
VALUES (
    decode('bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'hex'),
    200,
    decode('0123456789abcdef0123456789abcdef', 'hex'),
    convert_to('{"name":"caue","_id":"messages|bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}', 'UTF8'),
    false,
    NULL
);

-- And the persistence_globals fence Convex uses to record max_repeatable_ts.
-- The real backend stores this as a JSON-encoded ConvexValue; for the
-- fixture we use a JSON string holding the number.
INSERT INTO convex_dev.persistence_globals (key, json_value)
VALUES ('max_repeatable_ts', convert_to('200', 'UTF8'))
ON CONFLICT (key) DO UPDATE SET json_value = EXCLUDED.json_value;

-- ---------------------------------------------------------------------
-- Table-mapping fixture (#96): the bootstrap pointer + a `_tables` row
-- so the IDv6 → tablet UUID resolver can be exercised end-to-end.
--
-- The `_tables` tablet has its own well-known UUID. Convex stores it
-- in persistence_globals['tables_table_id'] as JSON ("<base64url-no-pad
-- of 16 bytes>"). We pick `cccccccccccccccccccccccccccccccc` (16 bytes
-- of 0xCC) for visibility; its base64url-no-pad form is exactly
-- `zMzMzMzMzMzMzMzMzMzMzA` (21 alternating z/M pairs + final 'A' for
-- the 4-bit zero pad), wrapped in JSON quotes.
INSERT INTO convex_dev.persistence_globals (key, json_value)
VALUES (
    'tables_table_id',
    convert_to('"zMzMzMzMzMzMzMzMzMzMzA"', 'UTF8')
)
ON CONFLICT (key) DO UPDATE SET json_value = EXCLUDED.json_value;

-- A `_tables` row whose `id` (the tablet UUID for the user table) is
-- the same `0123456789abcdef0123456789abcdef` used by the documents
-- above. The body has the `number` (10001) we'll encode into IDv6 +
-- the `state = "active"` the cache filters on.
INSERT INTO convex_dev.documents (id, ts, table_id, json_value, deleted, prev_ts)
VALUES (
    decode('0123456789abcdef0123456789abcdef', 'hex'),
    50,
    decode('cccccccccccccccccccccccccccccccc', 'hex'),
    convert_to(
        '{"name":"messages","number":10001,"state":"active"}',
        'UTF8'
    ),
    false,
    NULL
);

-- A second `_tables` row in `deleting` state. The mapping cache must
-- skip this one — a deleted/hidden row's number could be reused for a
-- live tablet, returning the wrong document.
INSERT INTO convex_dev.documents (id, ts, table_id, json_value, deleted, prev_ts)
VALUES (
    decode('99999999999999999999999999999999', 'hex'),
    50,
    decode('cccccccccccccccccccccccccccccccc', 'hex'),
    convert_to(
        '{"name":"old_messages","number":9999,"state":"deleting"}',
        'UTF8'
    ),
    false,
    NULL
);

-- ---------------------------------------------------------------------
-- Module-index fixture (#98 fatia 1): two more `_tables` rows so the
-- `lookup_by_name` path can resolve the system tablets `_modules` and
-- `_source_packages`. The actual `_modules` / `_source_packages` body
-- rows live alongside the user docs above and get inserted by the
-- integration test itself (the IDv6 string for `sourcePackageId`
-- needs to be computed in Rust, not handwritten in SQL).
INSERT INTO convex_dev.documents (id, ts, table_id, json_value, deleted, prev_ts)
VALUES (
    decode('aaaa1111aaaa1111aaaa1111aaaa1111', 'hex'),  -- `_modules` tablet UUID
    60,
    decode('cccccccccccccccccccccccccccccccc', 'hex'),  -- `_tables` tablet UUID
    convert_to(
        '{"name":"_modules","number":8002,"state":"active"}',
        'UTF8'
    ),
    false,
    NULL
),
(
    decode('bbbb2222bbbb2222bbbb2222bbbb2222', 'hex'),  -- `_source_packages` tablet UUID
    60,
    decode('cccccccccccccccccccccccccccccccc', 'hex'),  -- `_tables` tablet UUID
    convert_to(
        '{"name":"_source_packages","number":8001,"state":"active"}',
        'UTF8'
    ),
    false,
    NULL
);
