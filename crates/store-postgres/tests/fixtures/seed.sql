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
