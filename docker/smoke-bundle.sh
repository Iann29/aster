#!/usr/bin/env bash
# End-to-end smoke for the Aster module-query path.
#
# Drives a real `npx convex deploy` bundle through `aster-brokerd` +
# `aster-v8cell` over a UDS:
#
#   1. Boot postgres:16, apply Convex schema fixture + base seed.
#   2. Stage the bundle on disk as `<modules_dir>/<storage_key>.blob` —
#      a ZIP whose `modules/messages.js` entry IS the 58KB fixture
#      checked in at `crates/v8cell/tests/fixtures/messages.bundled.js`.
#   3. Insert `_source_packages` + `_modules` rows pointing at the
#      bundle, with the real SHA-256 of the bytes on disk so the
#      broker's verify-pass at fetch time succeeds.
#   4. Boot brokerd with `ASTER_STORE=postgres` + `ASTER_MODULES_DIR`
#      mounted; wait for `ready socket=`.
#   5. Run v8cell with `ASTER_MODULE_PATH=messages.js` +
#      `ASTER_FUNCTION_NAME=getById` +
#      `ASTER_ARGS_JSON=[{"id":"<aster wire id>"}]`. The cell loads the
#      bundle ZIP via `LoadModuleBundle`, compiles it as ESM, calls
#      `getById.invokeQuery(args)`, which fires
#      `Convex.asyncSyscall("1.0/get", {id})` — that lands at the
#      broker → Postgres → user document, hashes back through the
#      capsule → resolves the JS Promise → cell prints the JSON shape
#      Convex would have produced.
#   6. Assert the printed envelope contains the seeded document's
#      `name` field, and exactly one trap was drained (the `db.get`).
#
# What this catches that the lib-level test
# (`crates/v8cell/tests/module_loader.rs`) doesn't:
#
#   - The v8cell BINARY's env-parse path (slice 1 of #98 — wired in PR #23).
#   - The brokerd BINARY's `LoadModuleBundle` IPC dispatch against a
#     real Postgres-backed store, not the in-memory `LocalCapsuleBroker`
#     used for the lib test.
#   - The on-disk bundle layout (`<modules_dir>/<storage_key>.blob`) +
#     SHA-256 verification path the storage adapter ships.
#   - The IDv6 + `_modules` × `_source_packages` join — exactly the
#     surface a Synapse-provisioned cell will hit in production.
#
# Reuses the conventions of `docker/smoke-postgres.sh`: shared volume
# for the UDS, transient network, EXIT trap that cleans up containers
# + volumes + tmp dirs.
#
# Usage:
#   docker/smoke-bundle.sh [tag]
#
#   tag defaults to "0.4-modulequery" — must match what
#   `docker build --target=runtime-{broker,v8cell}` tagged the images.

set -euo pipefail

TAG="${1:-0.4-modulequery}"
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${HERE}/.." && pwd)"
BROKERD_IMAGE="${ASTER_BROKERD_IMAGE:-aster-brokerd:${TAG}}"
V8CELL_IMAGE="${ASTER_V8CELL_IMAGE:-aster-v8cell:${TAG}}"
SUFFIX="$(date +%s)-$$"
NETWORK="aster-bundle-smoke-${SUFFIX}"
VOLUME="aster-bundle-smoke-sock-${SUFFIX}"
BROKER="aster-bundle-smoke-brokerd-${SUFFIX}"
PG_CONTAINER="aster-bundle-smoke-postgres-${SUFFIX}"
PG_PASSWORD="${ASTER_PG_PASSWORD:-aster}"
SCHEMA="convex_dev"

# Bundle staging directory on the host. The brokerd container bind-
# mounts this read-only at `/run/aster/modules`; the storage adapter
# resolves `<storage_key>.blob` relative to the mount point.
MODULES_DIR=""
TMPDIR_TENANT=""

# ---------------------------------------------------------------------
# Constants chosen to match `crates/store-postgres/tests/fixtures/seed.sql`.
# Keep these in lockstep with the IT fixture or the module index won't
# resolve `_modules` / `_source_packages` to live tablets.

# Storage key the `_source_packages` row points at. The broker
# resolves the on-disk path as `<MODULES_DIR>/<storage_key>.blob`
# (no `modules/` prefix — `LocalDirModulesStorage` already takes the
# `<convex_storage>/modules` directory directly; double-prefixing is
# the foot-gun the brief calls out).
STORAGE_KEY="test-bundle"

# Source-package internal_id — 16 raw bytes. We pin a memorable
# `eeee5555` pattern (matches the IT test's
# `SOURCE_PACKAGE_INTERNAL_ID`) so the IDv6 helper output below is
# deterministic and the smoke is byte-stable across runs.
SP_INTERNAL_ID_HEX="eeee5555eeee5555eeee5555eeee5555"

# `_source_packages` tablet UUID + table_number. Must match seed.sql.
SP_TABLET_HEX="bbbb2222bbbb2222bbbb2222bbbb2222"
SP_TABLE_NUMBER="8001"

# `_modules` tablet UUID. Must match seed.sql.
MODULES_TABLET_HEX="aaaa1111aaaa1111aaaa1111aaaa1111"

# A second module row id — any 16 bytes, just needs to be unique
# inside the `_modules` tablet. Picking `cafe...` for grep-ability.
MODULE_ROW_HEX="cafecafecafecafecafecafecafecafe"

# User-table tablet (`messages`) — also from seed.sql. Used to build
# the Aster wire form `<table_hex>/<id_hex>` for the `db.get(id)` arg.
MESSAGES_TABLET_HEX="0123456789abcdef0123456789abcdef"

# Existing seeded document under that tablet (body
# `{"name":"ian", ...}`).
MESSAGE_DOC_HEX="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

# Aster wire form the bundle's `getById` will receive. The bundle
# pipes `args.id` straight into `Convex.asyncSyscall("1.0/get", {id})`;
# the cell-side syscall handler keys off the literal string. The
# Postgres adapter accepts EITHER an IDv6 string OR this Aster wire
# form (`<table_hex>/<id_hex>`) — the latter is shorter and skips the
# IDv6→tablet roundtrip, so we use it in the smoke.
WIRE_ID="${MESSAGES_TABLET_HEX}/${MESSAGE_DOC_HEX}"

cleanup() {
    rc=$?
    set +e
    docker rm -f "${BROKER}" >/dev/null 2>&1
    docker rm -f "${PG_CONTAINER}" >/dev/null 2>&1
    docker volume rm "${VOLUME}" >/dev/null 2>&1
    docker network rm "${NETWORK}" >/dev/null 2>&1
    [[ -n "${MODULES_DIR}" ]] && rm -rf "${MODULES_DIR}"
    [[ -n "${TMPDIR_TENANT}" ]] && rm -rf "${TMPDIR_TENANT}"
    exit "${rc}"
}
trap cleanup EXIT

echo "==> creating network + volume (suffix: ${SUFFIX})"
docker network create "${NETWORK}" >/dev/null
docker volume create "${VOLUME}" >/dev/null

# ---------------------------------------------------------------------
# Stage the bundle on disk.
#
# `messages.bundled.js` is byte-identical to the output of
# `npx convex deploy --debug-bundle-path` — see PR #22 for the
# reproduction commands. The ZIP layout the broker expects is the one
# Convex's source-package uploader writes upstream:
#
#     modules/<canonical_path>.js
#     modules/<canonical_path>.js.map
#     metadata.json
#
# `extract_module_source` (PR #21) tries `modules/<path>.js` first, so
# omitting the prefix WOULD work for this smoke (the bare-name
# fallback would catch it) — but we mirror real Convex so a future
# loader change that drops the bare-name fallback doesn't quietly
# regress the smoke.

MODULES_DIR="$(mktemp -d /tmp/aster-bundle-smoke-modules.XXXXXX)"
chmod 0755 "${MODULES_DIR}"

BUNDLE_FIXTURE="${REPO_ROOT}/crates/v8cell/tests/fixtures/messages.bundled.js"
if [[ ! -f "${BUNDLE_FIXTURE}" ]]; then
    echo "ERROR: bundle fixture not found at ${BUNDLE_FIXTURE}"
    exit 1
fi

# Stage the bundle into a temp build dir then `zip` to .blob. Using
# `zip -j` would flatten paths; we want the `modules/` prefix
# preserved, so we mirror the layout via cd + zip.
BUNDLE_BUILD_DIR="$(mktemp -d /tmp/aster-bundle-smoke-build.XXXXXX)"
mkdir -p "${BUNDLE_BUILD_DIR}/modules"
cp "${BUNDLE_FIXTURE}" "${BUNDLE_BUILD_DIR}/modules/messages.js"
# Source map + metadata stubs. Real Convex bundles ship rich source
# maps — the cell-side loader doesn't consume them in v0.5, so empty
# objects are sufficient for the smoke. The `metadata.json` stays
# symmetric with the upstream layout in case a future check looks for it.
echo '{}' > "${BUNDLE_BUILD_DIR}/modules/messages.js.map"
echo '{}' > "${BUNDLE_BUILD_DIR}/metadata.json"

BLOB_PATH="${MODULES_DIR}/${STORAGE_KEY}.blob"
( cd "${BUNDLE_BUILD_DIR}" && zip -qrX "${BLOB_PATH}" modules metadata.json )
rm -rf "${BUNDLE_BUILD_DIR}"

# Compute the bundle's SHA-256 (raw bytes — `_source_packages.sha256`
# is `Vec<u8>` after `$bytes` decoding) and a base64 form for the
# ConvexValue `$bytes` wrapper.
BUNDLE_SHA_HEX="$(sha256sum "${BLOB_PATH}" | awk '{print $1}')"
BUNDLE_SHA_B64="$(python3 -c '
import sys, base64
hex_str = sys.argv[1]
print(base64.b64encode(bytes.fromhex(hex_str)).decode())
' "${BUNDLE_SHA_HEX}")"

BUNDLE_BYTES="$(stat -c %s "${BLOB_PATH}")"

echo "==> staged bundle at ${BLOB_PATH} (${BUNDLE_BYTES} bytes, sha256 ${BUNDLE_SHA_HEX:0:16}...)"

# Resolve the IDv6 string the `_modules.sourcePackageId` field needs.
# Convex's IDv6 = base32(VInt(table_number) || internal_id ||
# Fletcher16-footer). Computing this from shell is painful — we lean
# on the workspace's IDv6 codec via a `--example` helper so the
# encoding stays in lockstep with what the broker decodes.
echo "==> computing IDv6 sourcePackageId via example helper"
SP_IDV6="$(cd "${REPO_ROOT}" && cargo run --release --quiet \
    -p aster-convex-codec --example idv6_smoke_helper -- \
    "${SP_TABLE_NUMBER}" "${SP_INTERNAL_ID_HEX}")"
if [[ -z "${SP_IDV6}" ]]; then
    echo "ERROR: idv6 helper produced empty output"
    exit 1
fi
echo "==> sourcePackageId = ${SP_IDV6}"

# ---------------------------------------------------------------------
# Build the JSON bodies for `_source_packages` + `_modules` rows.
# Done in Python because:
#   - `$bytes` / `$integer` are ConvexValue wrappers we must produce
#     in their canonical wire form (base64 of LE 8 bytes for integers).
#   - bash's quoting story for nested JSON literals is bleak.
# Output is fed straight into psql via stdin so no shell-escape
# round-trip is needed.

SP_BODY_JSON="$(python3 - "${STORAGE_KEY}" "${BUNDLE_SHA_B64}" "${BUNDLE_BYTES}" <<'PY'
import sys, json, base64, struct
storage_key, sha_b64, zipped_size = sys.argv[1], sys.argv[2], int(sys.argv[3])
def i64(value):
    return {"$integer": base64.b64encode(struct.pack("<q", value)).decode()}
body = {
    "storageKey": storage_key,
    "sha256": {"$bytes": sha_b64},
    "externalPackageId": None,
    "packageSize": {
        "zippedSizeBytes": i64(zipped_size),
        # `unzippedSizeBytes` is a hint for the cache, not a verify
        # gate — a stable round number is fine.
        "unzippedSizeBytes": i64(4096),
    },
    "nodeVersion": None,
}
print(json.dumps(body))
PY
)"

MODULE_BODY_JSON="$(python3 - "${SP_IDV6}" <<'PY'
import sys, json
source_package_id = sys.argv[1]
body = {
    "path": "messages.js",
    "sourcePackageId": source_package_id,
    "environment": "isolate",
    "analyzeResult": None,
    # Module-level sha256 is parsed by `parse_module_row` but only
    # surfaced as the descriptor's `module_sha256_base64` — never
    # verified against the stored bundle (the source-package sha
    # covers integrity for the whole archive).
    "sha256": "module-sha256-base64-stub",
}
print(json.dumps(body))
PY
)"

# ---------------------------------------------------------------------
# Postgres lifecycle. Same dance as smoke-postgres.sh.

echo "==> starting postgres:16"
docker run -d --name "${PG_CONTAINER}" --network "${NETWORK}" \
    --network-alias postgres \
    -e POSTGRES_USER=aster -e POSTGRES_PASSWORD="${PG_PASSWORD}" -e POSTGRES_DB=aster \
    postgres:16 >/dev/null

echo "==> waiting for postgres ready"
for i in $(seq 1 60); do
    if docker exec "${PG_CONTAINER}" psql -U aster -d aster -tAc 'SELECT 1' >/dev/null 2>&1; then
        break
    fi
    if [[ "$i" == "60" ]]; then
        echo "ERROR: postgres did not become reachable within 30s"
        docker logs "${PG_CONTAINER}" 2>&1 | tail -20 | sed 's/^/  /'
        exit 1
    fi
    sleep 0.5
done

echo "==> applying schema + base seed"
docker exec -i "${PG_CONTAINER}" psql -U aster -d aster \
    < "${REPO_ROOT}/crates/store-postgres/tests/fixtures/schema.sql" >/dev/null
docker exec -i "${PG_CONTAINER}" psql -U aster -d aster \
    < "${REPO_ROOT}/crates/store-postgres/tests/fixtures/seed.sql" >/dev/null

echo "==> seeding module-index rows (_source_packages, _modules)"
# We pipe the SQL into psql with `ON_ERROR_STOP=1` so the script bails
# loudly on a malformed statement. The JSON bodies use Postgres
# dollar-quoted strings (`$json_sp$...$json_sp$`) so we don't have to
# escape single-quotes inside the JSON. Both INSERTs run inside one
# implicit transaction — atomic seeding.
docker exec -i "${PG_CONTAINER}" \
    psql -v ON_ERROR_STOP=1 -U aster -d aster >/dev/null <<SQL
INSERT INTO ${SCHEMA}.documents (id, ts, table_id, json_value, deleted, prev_ts)
VALUES (
    decode('${SP_INTERNAL_ID_HEX}', 'hex'),
    70,
    decode('${SP_TABLET_HEX}', 'hex'),
    convert_to(\$json_sp\$${SP_BODY_JSON}\$json_sp\$, 'UTF8'),
    false,
    NULL
);
INSERT INTO ${SCHEMA}.documents (id, ts, table_id, json_value, deleted, prev_ts)
VALUES (
    decode('${MODULE_ROW_HEX}', 'hex'),
    70,
    decode('${MODULES_TABLET_HEX}', 'hex'),
    convert_to(\$json_mod\$${MODULE_BODY_JSON}\$json_mod\$, 'UTF8'),
    false,
    NULL
);
SQL

# ---------------------------------------------------------------------
# Boot brokerd with the bundle dir mounted + ASTER_STORE=postgres.

echo "==> starting brokerd (ASTER_STORE=postgres, modules dir mounted)"
docker run -d --name "${BROKER}" --network "${NETWORK}" \
    -v "${VOLUME}:/run/aster" \
    -v "${MODULES_DIR}:/run/aster/modules:ro" \
    -e ASTER_BROKER_SOCK=/run/aster/broker.sock \
    -e ASTER_TENANT=tenant-bundle-smoke \
    -e ASTER_DEPLOYMENT=dep-bundle-smoke \
    -e ASTER_SEAL_SEED=bundle-smoke-seed \
    -e ASTER_STORE=postgres \
    -e ASTER_DB_URL="postgres://aster:${PG_PASSWORD}@postgres:5432/aster" \
    -e ASTER_DB_SCHEMA="${SCHEMA}" \
    -e ASTER_MODULES_DIR=/run/aster/modules \
    -e ASTER_SNAPSHOT_TS=200 \
    -e ASTER_MAX_CONNECTIONS=8 \
    "${BROKERD_IMAGE}" >/dev/null

echo "==> waiting for broker ready"
for i in $(seq 1 100); do
    if docker logs "${BROKER}" 2>&1 | grep -q "ready socket="; then
        break
    fi
    if [[ "$i" == "100" ]]; then
        echo "ERROR: broker did not log 'ready socket=' within 10s"
        docker logs "${BROKER}" 2>&1 | sed 's/^/  /'
        exit 1
    fi
    sleep 0.1
done

if ! docker logs "${BROKER}" 2>&1 | grep -q "store=postgres"; then
    echo "ERROR: broker did not log 'store=postgres' — dispatch failed"
    docker logs "${BROKER}" 2>&1 | sed 's/^/  /'
    exit 1
fi

# ---------------------------------------------------------------------
# Run v8cell against the bundle.
#
# Args wire shape: per `/tmp/aster-research-bundle-ground-truth.md` §3.3,
# `invokeFunction(handler, ctx, args)` does `handler(ctx, ...args)`, so
# `args` is a JSON ARRAY of arg objects. For getById, that's
# `[{"id": "..."}]` — wrap, don't unwrap.

ARGS_JSON='[{"id":"'"${WIRE_ID}"'"}]'
echo "==> running v8cell (module=messages.js, function=getById, id=${WIRE_ID})"
output="$(docker run --rm \
    --network "${NETWORK}" \
    -v "${VOLUME}:/run/aster" \
    -e ASTER_BROKER_SOCK=/run/aster/broker.sock \
    -e ASTER_TENANT=tenant-bundle-smoke \
    -e ASTER_DEPLOYMENT=dep-bundle-smoke \
    -e ASTER_SNAPSHOT_TS=200 \
    -e ASTER_CELL_ID=cell-bundle-smoke-1 \
    -e ASTER_LEASE_EPOCH=11 \
    -e ASTER_PREWARM= \
    -e ASTER_MAX_TRAPS=8 \
    -e ASTER_MODULE_PATH=messages.js \
    -e ASTER_FUNCTION_NAME=getById \
    -e ASTER_ARGS_JSON="${ARGS_JSON}" \
    "${V8CELL_IMAGE}")"

echo "==> v8cell stdout: ${output}"

# The cell prints a JSON envelope `{"output": ..., "traps": N, "capsule_hash": [...]}`.
# `output` is the resolved string from `invokeQuery`, which is the
# JS-side `JSON.stringify(convexToJson(result))` — exactly what
# upstream Convex would have sent over the wire. For a `db.get(id)`
# that hits a non-deleted row, the resolved object IS the document
# body, so we should see `name` (and `_id`) in there.

# The envelope's `output` field is itself a JSON-encoded string —
# the resolved value `JSON.stringify(convexToJson(doc))`. So the
# document fields appear inside escaped quotes (`\"name\":\"ian\"`).
# Match against the escaped form so a bash literal stays correct,
# regardless of what `grep`'s glob does with bare double-quotes.
if ! grep -q '\\"name\\":\\"ian\\"' <<<"${output}"; then
    echo "ERROR: expected name=\"ian\" in cell stdout (read from postgres via module-query)"
    echo "  full output: ${output}"
    docker logs "${BROKER}" 2>&1 | tail -30 | sed 's/^/  brokerd: /'
    exit 1
fi
if ! grep -q '\\"_id\\":\\"messages|aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\"' <<<"${output}"; then
    echo "ERROR: expected the seeded _id field in cell stdout"
    echo "  full output: ${output}"
    exit 1
fi
if ! grep -q '"traps":1' <<<"${output}"; then
    echo "ERROR: expected exactly one trap (the db.get → 1.0/get syscall)"
    exit 1
fi

echo "OK: aster brokerd(postgres) + v8cell module-query smoke passed —"
echo "    real npx-convex-deploy bundle compiled as ESM,"
echo "    getById invoked with args=${ARGS_JSON},"
echo "    db.get(id) traversed Convex.asyncSyscall(\"1.0/get\") → broker → postgres,"
echo "    document body returned with name=\"ian\"."
