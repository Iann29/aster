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

TAG="${1:-0.3}"
HERE="$(cd "$(dirname "$0")" && pwd)"
BROKERD_IMAGE="${ASTER_BROKERD_IMAGE:-aster-brokerd:${TAG}}"
V8CELL_IMAGE="${ASTER_V8CELL_IMAGE:-aster-v8cell:${TAG}}"
SUFFIX="$(date +%s)-$$"
NETWORK="aster-pg-smoke-${SUFFIX}"
VOLUME="aster-pg-smoke-sock-${SUFFIX}"
BROKER="aster-pg-smoke-brokerd-${SUFFIX}"
PG_CONTAINER="aster-pg-smoke-postgres-${SUFFIX}"
PG_PASSWORD="${ASTER_PG_PASSWORD:-aster}"

cleanup() {
    rc=$?
    set +e
    docker rm -f "${BROKER}" >/dev/null 2>&1
    docker rm -f "${PG_CONTAINER}" >/dev/null 2>&1
    docker volume rm "${VOLUME}" >/dev/null 2>&1
    docker network rm "${NETWORK}" >/dev/null 2>&1
    exit "${rc}"
}
trap cleanup EXIT

echo "==> creating network + volume (suffix: ${SUFFIX})"
docker network create "${NETWORK}" >/dev/null
docker volume create "${VOLUME}" >/dev/null

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

echo "==> applying schema + seed"
docker exec -i "${PG_CONTAINER}" psql -U aster -d aster < "${HERE}/../crates/store-postgres/tests/fixtures/schema.sql" >/dev/null
docker exec -i "${PG_CONTAINER}" psql -U aster -d aster < "${HERE}/../crates/store-postgres/tests/fixtures/seed.sql" >/dev/null

echo "==> starting brokerd (ASTER_STORE=postgres)"
docker run -d --name "${BROKER}" --network "${NETWORK}" \
    -v "${VOLUME}:/run/aster" \
    -e ASTER_BROKER_SOCK=/run/aster/broker.sock \
    -e ASTER_TENANT=tenant-pg-smoke \
    -e ASTER_DEPLOYMENT=dep-pg-smoke \
    -e ASTER_SEAL_SEED=pg-smoke-seed \
    -e ASTER_STORE=postgres \
    -e ASTER_DB_URL="postgres://aster:${PG_PASSWORD}@postgres:5432/aster" \
    -e ASTER_DB_SCHEMA=convex_dev \
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

TENANT_DIR="$(mktemp -d /tmp/aster-pg-smoke-tenant.XXXXXX)"
# mktemp -d defaults to 0700, which the v8cell UID inside the container
# can't read. Loosen so the bind-mount actually serves the JS file.
chmod 0755 "${TENANT_DIR}"
cleanup_tenant() { rm -rf "${TENANT_DIR}"; cleanup; }
trap cleanup_tenant EXIT
cat > "${TENANT_DIR}/main.js" <<'JS'
async function main() {
  const json = await Convex.asyncSyscall("1.0/get", JSON.stringify({
    id: "0123456789abcdef0123456789abcdef/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  }));
  const doc = JSON.parse(json);
  return doc.name;
}
JS

echo "==> running v8cell"
output="$(docker run --rm \
    --network "${NETWORK}" \
    -v "${VOLUME}:/run/aster" \
    -v "${TENANT_DIR}:/tenant:ro" \
    -e ASTER_BROKER_SOCK=/run/aster/broker.sock \
    -e ASTER_TENANT=tenant-pg-smoke \
    -e ASTER_DEPLOYMENT=dep-pg-smoke \
    -e ASTER_SNAPSHOT_TS=200 \
    -e ASTER_CELL_ID=cell-pg-smoke-1 \
    -e ASTER_LEASE_EPOCH=7 \
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
