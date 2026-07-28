#!/usr/bin/env bash
# Black-box smoke for the Postgres-backed Aster path.
#
# Boots `postgres:16`, applies the Convex schema fixture + seeds two
# documents, spins up `aster-brokerd` with ASTER_STORE=postgres, then
# runs `aster-v8cell` against it executing a JS function that calls
# `Convex.asyncSyscall("1.0/get", ...)` and asserts the cell prints
# `"output":"ian"`. End-to-ends the Postgres adapter (PR #7), the v0.4
# dispatch, and the Convex.asyncSyscall wire (PR #8).

set -euo pipefail

TAG="${1:-0.4}"
HERE="$(cd "$(dirname "$0")" && pwd)"
BROKERD_IMAGE="${ASTER_BROKERD_IMAGE:-aster-brokerd:${TAG}}"
V8CELL_IMAGE="${ASTER_V8CELL_IMAGE:-aster-v8cell:${TAG}}"
SUFFIX="$(date +%s)-$$"
NETWORK="aster-pg-smoke-${SUFFIX}"
STATE_DIR="$(mktemp -d /tmp/aster-pg-smoke-state.XXXXXX)"
BROKER="aster-pg-smoke-brokerd-${SUFFIX}"
PG_CONTAINER="aster-pg-smoke-postgres-${SUFFIX}"
PG_PASSWORD="${ASTER_PG_PASSWORD:-aster}"
POSTGRES_IMAGE="${ASTER_POSTGRES_IMAGE:-postgres:16@sha256:17e67d7b9890c99b055ba1e0d5c5be4ec27c9d3a72bda32db24a5e5d8a85af0c}"
RUNTIME_UID="$(id -u)"
RUNTIME_GID="$(id -g)"

cleanup() {
    rc=$?
    set +e
    docker rm -f "${BROKER}" >/dev/null 2>&1
    docker rm -f "${PG_CONTAINER}" >/dev/null 2>&1
    rm -rf "${STATE_DIR}"
    docker network rm "${NETWORK}" >/dev/null 2>&1
    exit "${rc}"
}
trap cleanup EXIT

echo "==> creating network + runtime state (suffix: ${SUFFIX})"
docker network create "${NETWORK}" >/dev/null
dd if=/dev/urandom of="${STATE_DIR}/seal.key" bs=32 count=1 status=none
dd if=/dev/urandom of="${STATE_DIR}/launch.key" bs=32 count=1 status=none
printf 'postgres://aster:%s@postgres:5432/aster\n' "${PG_PASSWORD}" >"${STATE_DIR}/db_url"
cat >"${STATE_DIR}/policy.json" <<'JSON'
{
  "version": 1,
  "read_prefixes": ["k01gh001m4cxmxq3000000000000000"],
  "write_prefixes": ["__none__/"],
  "module_prefixes": ["__none__/"],
  "insert_tables": [],
  "max_reads_per_transaction": 8,
  "max_writes_per_transaction": 1,
  "max_scan_limit": 8,
  "max_concurrent_sessions": 4,
  "session_ttl_seconds": 60
}
JSON
chmod 0600 "${STATE_DIR}/seal.key" "${STATE_DIR}/launch.key" "${STATE_DIR}/db_url"

echo "==> starting postgres:16"
docker run -d --name "${PG_CONTAINER}" --network "${NETWORK}" \
    --network-alias postgres \
    -e POSTGRES_USER=aster -e POSTGRES_PASSWORD="${PG_PASSWORD}" -e POSTGRES_DB=aster \
    "${POSTGRES_IMAGE}" >/dev/null

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

echo "==> applying schema + seed"
docker exec -i "${PG_CONTAINER}" psql -U aster -d aster < "${HERE}/../crates/store-postgres/tests/fixtures/schema.sql" >/dev/null
docker exec -i "${PG_CONTAINER}" psql -U aster -d aster < "${HERE}/../crates/store-postgres/tests/fixtures/seed.sql" >/dev/null

echo "==> starting brokerd (ASTER_STORE=postgres)"
docker run -d --name "${BROKER}" --network "${NETWORK}" \
    --user "${RUNTIME_UID}:${RUNTIME_GID}" \
    -v "${STATE_DIR}:/run/aster" \
    -e ASTER_BROKER_SOCK=/run/aster/broker.sock \
    -e ASTER_TENANT=tenant-pg-smoke \
    -e ASTER_DEPLOYMENT=dep-pg-smoke \
    -e ASTER_SEAL_KEY_FILE=/run/aster/seal.key \
    -e ASTER_LAUNCH_KEY_FILE=/run/aster/launch.key \
    -e ASTER_AUTHORITY_EPOCH_FILE=/run/aster/authority_epoch \
    -e ASTER_POLICY_FILE=/run/aster/policy.json \
    -e ASTER_STORE=postgres \
    -e ASTER_DB_URL_FILE=/run/aster/db_url \
    -e ASTER_DB_SCHEMA=convex_dev \
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

# S9a: a postgres-mode brokerd acquires its epoch from the storage lease
# authority at boot (ASTER_LEASE_EPOCH is ignored) and refuses to mint
# sessions for any other epoch — so the cell must claim exactly the epoch
# the broker logged.
LEASE_EPOCH="$(docker logs "${BROKER}" 2>&1 | sed -n 's/.*lease epoch=\([0-9][0-9]*\).*/\1/p' | head -1)"
if [[ -z "${LEASE_EPOCH}" ]]; then
    echo "ERROR: broker did not log its lease epoch"
    docker logs "${BROKER}" 2>&1 | sed 's/^/  /'
    exit 1
fi
echo "==> broker lease epoch: ${LEASE_EPOCH}"
docker exec "${PG_CONTAINER}" psql -U aster -d aster -v ON_ERROR_STOP=1 \
    -c "INSERT INTO aster.log (tenant, deployment, ts, key, epoch, document)
        VALUES (
          'tenant-pg-smoke',
          'dep-pg-smoke',
          1,
          'k01gh001m4cxmxq3000000000000000',
          ${LEASE_EPOCH},
          '{\"_raw\":{\"Text\":\"{\\\"_id\\\":\\\"k01gh001m4cxmxq3000000000000000\\\",\\\"name\\\":\\\"ian\\\"}\"}}'
        )" >/dev/null
LAUNCH_TOKEN="$(docker run --rm --network none --read-only \
    --user "${RUNTIME_UID}:${RUNTIME_GID}" \
    --cap-drop ALL --security-opt no-new-privileges:true \
    --entrypoint /usr/local/bin/aster_launch_token \
    -v "${STATE_DIR}:/run/aster:ro" \
    -e ASTER_LAUNCH_KEY_FILE=/run/aster/launch.key \
    -e ASTER_AUTHORITY_EPOCH_FILE=/run/aster/authority_epoch \
    "${BROKERD_IMAGE}" \
    cell-pg-smoke-1 tenant-pg-smoke dep-pg-smoke current 60)"

TENANT_DIR="$(mktemp -d /tmp/aster-pg-smoke-tenant.XXXXXX)"
# mktemp -d defaults to 0700, which the v8cell UID inside the container
# can't read. Loosen so the bind-mount actually serves the JS file.
chmod 0755 "${TENANT_DIR}"
cleanup_tenant() { rm -rf "${TENANT_DIR}"; cleanup; }
trap cleanup_tenant EXIT
cat > "${TENANT_DIR}/main.js" <<'JS'
async function main() {
  const json = await Convex.asyncSyscall("1.0/get", JSON.stringify({
    id: "k01gh001m4cxmxq3000000000000000"
  }));
  const doc = JSON.parse(json);
  return doc.name;
}
JS

echo "==> running v8cell"
output="$(docker run --rm \
    --network none \
    --user "${RUNTIME_UID}:${RUNTIME_GID}" \
    --read-only --cap-drop ALL --security-opt no-new-privileges:true \
    -v "${STATE_DIR}:/run/aster:ro" \
    -v "${TENANT_DIR}:/tenant:ro" \
    -e ASTER_BROKER_SOCK=/run/aster/broker.sock \
    -e ASTER_TENANT=tenant-pg-smoke \
    -e ASTER_DEPLOYMENT=dep-pg-smoke \
    -e ASTER_CELL_ID=cell-pg-smoke-1 \
    -e ASTER_LEASE_EPOCH="${LEASE_EPOCH}" \
    -e ASTER_LAUNCH_TOKEN="${LAUNCH_TOKEN}" \
    -e ASTER_PREWARM= \
    -e ASTER_MAX_TRAPS=8 \
    -e ASTER_JS=/tenant/main.js \
    "${V8CELL_IMAGE}")"

echo "==> v8cell stdout: ${output}"

if ! grep -q '"output":"ian"' <<<"${output}"; then
    echo "ERROR: expected output=\"ian\" in cell stdout (read from postgres)"
    exit 1
fi
if ! grep -q '"traps":1' <<<"${output}"; then
    echo "ERROR: expected exactly one read trap"
    exit 1
fi

echo "OK: aster v${TAG} brokerd(postgres) + v8cell smoke passed — read 'ian' from postgres via Convex.asyncSyscall(\"1.0/get\")"
