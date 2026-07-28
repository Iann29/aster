#!/usr/bin/env bash
# First-ever REAL Aster benchmark: V8 cell + UDS broker + Postgres store.
# Methodology: three JS workloads (0 traps / 1 trap / K traps) run N times each
# as one-shot v8cell containers against a long-lived brokerd. Docker container
# spawn overhead cancels in the subtractions:
#   per-trap marginal  = (T_K - T_1) / (K - 1)
#   first-trap cost    = T_1 - T_0   (UDS round trip + PG point read + reseal)
set -euo pipefail

N="${N:-30}"
K="${K:-16}"
TAG=bench
HERE="$(cd "$(dirname "$0")" && pwd)"
ASTER="${HERE}/aster"
SUFFIX="bench-$$"
NETWORK="aster-${SUFFIX}"
VOLUME="aster-sock-${SUFFIX}"
BROKER="aster-brokerd-${SUFFIX}"
PG="aster-pg-${SUFFIX}"
DOC_ID="0123456789abcdef0123456789abcdef/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

cleanup() {
    rc=$?
    set +e
    docker rm -f "${BROKER}" "${PG}" >/dev/null 2>&1
    docker volume rm "${VOLUME}" >/dev/null 2>&1
    docker network rm "${NETWORK}" >/dev/null 2>&1
    rm -rf "${TENANT_DIR}"
    exit "${rc}"
}
trap cleanup EXIT

docker network create "${NETWORK}" >/dev/null
docker volume create "${VOLUME}" >/dev/null

echo "==> postgres:16 + fixtures"
docker run -d --name "${PG}" --network "${NETWORK}" --network-alias postgres \
    -e POSTGRES_USER=aster -e POSTGRES_PASSWORD=aster -e POSTGRES_DB=aster \
    postgres:16 >/dev/null
for i in $(seq 1 60); do
    docker exec "${PG}" psql -U aster -d aster -tAc 'SELECT 1' >/dev/null 2>&1 && break
    [[ "$i" == 60 ]] && { echo "postgres not ready"; exit 1; }
    sleep 0.5
done
docker exec -i "${PG}" psql -U aster -d aster < "${ASTER}/crates/store-postgres/tests/fixtures/schema.sql" >/dev/null
docker exec -i "${PG}" psql -U aster -d aster < "${ASTER}/crates/store-postgres/tests/fixtures/seed.sql" >/dev/null

echo "==> brokerd (postgres store)"
docker run -d --name "${BROKER}" --network "${NETWORK}" \
    -v "${VOLUME}:/run/aster" \
    -e ASTER_BROKER_SOCK=/run/aster/broker.sock \
    -e ASTER_TENANT=tenant-bench -e ASTER_DEPLOYMENT=dep-bench \
    -e ASTER_SEAL_SEED=bench-seed \
    -e ASTER_STORE=postgres \
    -e ASTER_DB_URL="postgres://aster:aster@postgres:5432/aster" \
    -e ASTER_DB_SCHEMA=convex_dev \
    -e ASTER_SNAPSHOT_TS=200 \
    -e ASTER_MAX_CONNECTIONS=10000000 \
    "aster-brokerd:${TAG}" >/dev/null
for i in $(seq 1 100); do
    docker logs "${BROKER}" 2>&1 | grep -q "ready socket=" && break
    [[ "$i" == 100 ]] && { echo "broker not ready"; docker logs "${BROKER}"; exit 1; }
    sleep 0.1
done

# S9a: postgres-mode brokerd takes its epoch from the lease authority at
# boot; cells must claim exactly that epoch.
LEASE_EPOCH="$(docker logs "${BROKER}" 2>&1 | sed -n 's/.*lease epoch=\([0-9][0-9]*\).*/\1/p' | head -1)"
[[ -z "${LEASE_EPOCH}" ]] && { echo "broker did not log its lease epoch"; docker logs "${BROKER}"; exit 1; }
echo "==> broker lease epoch: ${LEASE_EPOCH}"

TENANT_DIR="$(mktemp -d /tmp/aster-bench-tenant.XXXXXX)"
chmod 0755 "${TENANT_DIR}"

cat > "${TENANT_DIR}/t0.js" <<'JS'
async function main() { return "noop"; }
JS
cat > "${TENANT_DIR}/t1.js" <<JS
async function main() {
  const json = await Convex.asyncSyscall("1.0/get", JSON.stringify({ id: "${DOC_ID}" }));
  return JSON.parse(json).name;
}
JS
cat > "${TENANT_DIR}/tk.js" <<JS
async function main() {
  let last = "";
  for (let i = 0; i < ${K}; i++) {
    const json = await Convex.asyncSyscall("1.0/get", JSON.stringify({ id: "${DOC_ID}" }));
    last = JSON.parse(json).name;
  }
  return last;
}
JS

run_cell() { # $1 = js file
    docker run --rm --network "${NETWORK}" \
        -v "${VOLUME}:/run/aster" -v "${TENANT_DIR}:/tenant:ro" \
        -e ASTER_BROKER_SOCK=/run/aster/broker.sock \
        -e ASTER_TENANT=tenant-bench -e ASTER_DEPLOYMENT=dep-bench \
        -e ASTER_SNAPSHOT_TS=200 -e ASTER_CELL_ID=cell-bench \
        -e ASTER_LEASE_EPOCH="${LEASE_EPOCH}" -e ASTER_PREWARM= \
        -e ASTER_MAX_TRAPS=512 -e "ASTER_JS=/tenant/$1" \
        "aster-v8cell:${TAG}" 2>>"${TENANT_DIR}/cell-err.log"
}

# sanity: expected trap counts
out1="$(run_cell t1.js)"; grep -q '"traps":1' <<<"$out1" || { echo "SANITY FAIL t1: $out1"; exit 1; }
outk="$(run_cell tk.js)"; grep -q "\"traps\":${K}" <<<"$outk" || { echo "SANITY FAIL tk: $outk"; exit 1; }
echo "==> sanity OK: t1 traps=1, tk traps=${K} (Convex.asyncSyscall always traps — no warm-capsule path)"

bench() { # $1 = js file, $2 = label
    local times=()
    for i in $(seq 1 "${N}"); do
        local start end
        start=$(date +%s%N)
        run_cell "$1" >/dev/null || {
            echo "RUN FAIL: $2 iter $i — last cell stderr:"
            tail -8 "${TENANT_DIR}/cell-err.log" 2>/dev/null | sed 's/^/  /'
            echo "broker log tail:"
            docker logs --tail 8 "${BROKER}" 2>&1 | sed 's/^/  /'
            exit 1
        }
        end=$(date +%s%N)
        times+=( $(( (end - start) / 1000000 )) )
    done
    printf '%s\n' "${times[@]}" | sort -n > "/tmp/aster-bench-$2.txt"
    local p50 p95 min
    min=$(head -1 "/tmp/aster-bench-$2.txt")
    p50=$(awk -v n="${N}" 'NR==int(n*0.5)+1' "/tmp/aster-bench-$2.txt")
    p95=$(awk -v n="${N}" 'NR==int(n*0.95)' "/tmp/aster-bench-$2.txt")
    echo "$2: min=${min}ms p50=${p50}ms p95=${p95}ms (N=${N})"
}

echo "==> benching (N=${N} each; one-shot container per invocation)"
bench t0.js T0_noop
bench t1.js T1_one_trap
bench tk.js "TK_${K}_traps"

t0=$(awk -v n="${N}" 'NR==int(n*0.5)+1' /tmp/aster-bench-T0_noop.txt)
t1=$(awk -v n="${N}" 'NR==int(n*0.5)+1' /tmp/aster-bench-T1_one_trap.txt)
tk=$(awk -v n="${N}" 'NR==int(n*0.5)+1' "/tmp/aster-bench-TK_${K}_traps.txt")
echo
echo "=== DERIVED (p50, docker spawn cancels) ==="
echo "first-trap cost (T1-T0):            $(( t1 - t0 )) ms   [UDS + PG point read + verify + reseal]"
echo "marginal per-trap ((TK-T1)/(K-1)):  $(awk -v a="$tk" -v b="$t1" -v k="$K" 'BEGIN{printf "%.2f", (a-b)/(k-1)}') ms/trap"
echo
echo "=== baseline: same point-read via psql (10x) ==="
docker exec "${PG}" bash -c 'time for i in $(seq 1 10); do psql -U aster -d aster -tAc "SELECT 1 FROM convex_dev.documents LIMIT 1" >/dev/null; done' 2>&1 | grep real
