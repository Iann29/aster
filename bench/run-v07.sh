#!/usr/bin/env bash
# S10 — the v0.7 measurement campaign. Successor of the 2026-07-16 scratch
# harness (paper/sources/aster-bench.sh), now versioned in-repo.
#
# Four benches, each writing a raw log to bench/results/v07/:
#
#   B1  read path re-run (post warm-hit): V8 one-shot host processes +
#       long-lived brokerd (ASTER_STORE=postgres) + Convex-schema fixture
#       DB. Same subtraction methodology as the 2026-07-16 run (T0 / T1 /
#       TK; spawn cost cancels), with two deltas stated in the results:
#       cells are HOST processes (not docker one-shots — the old ~390 ms
#       floor was container spawn), and TK now comes in two shapes:
#       same-key ×K (asserts the S7 warm-hit fix: 1 trap, K-1 warm hits)
#       and distinct-key ×K (K traps over a growing capsule).
#   B2  reseal curve (EQ2): bench_v07 reseal against a memory-store
#       brokerd — per-trap latency at constant capsule size n, plus the
#       cumulative climb, n ∈ {1..1000}.
#   B3  fence isolated (EQ3): bench_v07 fence — direct WritePlane commits
#       against real Postgres (blind / p points / w windows / conflict).
#   B4  write path e2e (EQ3/EQ4): bench_v07 e2e — InitialCapsule → V8 JS
#       (1.0/get trap + 1.0/insert) → Commit verb over UDS → Postgres
#       fence, serial.
#
# Requirements: a running postgres:16 container (default `aster-pg-dev`,
# port 5433, user/pass/db `aster`), docker exec access, and a release
# build (the script builds it). Everything else is created per-run in a
# dedicated bench database and torn down.
#
# Usage: bash bench/run-v07.sh [--only=b1,b2,b3,b4]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "${HERE}/.." && pwd)"
RESULTS="${HERE}/results/v07"
TARGET_DIR="${CARGO_TARGET_DIR:-${REPO}/target}"
BIN="${TARGET_DIR}/release"

PG_CONTAINER="${ASTER_PG_CONTAINER:-aster-pg-dev}"
PG_PORT="${ASTER_PG_PORT:-5433}"
BENCH_DB="${ASTER_BENCH_DB:-aster_bench}"
DB_URL="postgres://aster:aster@127.0.0.1:${PG_PORT}/${BENCH_DB}"

N="${N:-12}"          # one-shot invocations per B1 workload (old-notes N)
K="${K:-200}"         # reads per TK workload (old-notes K)
DOC_ID="0123456789abcdef0123456789abcdef/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

ONLY="b1,b2,b3,b4"
for arg in "$@"; do
    case "$arg" in
        --only=*) ONLY="${arg#--only=}" ;;
        *) echo "unknown arg: $arg (want --only=b1,b2,...)"; exit 2 ;;
    esac
done
runs() { [[ ",${ONLY}," == *",$1,"* ]]; }

RUN_DIR="$(mktemp -d /tmp/aster-bench-v07.XXXXXX)"
SOCK="${RUN_DIR}/broker.sock"
BROKER_PID=""

cleanup() {
    rc=$?
    set +e
    [[ -n "${BROKER_PID}" ]] && kill "${BROKER_PID}" >/dev/null 2>&1
    rm -rf "${RUN_DIR}"
    exit "${rc}"
}
trap cleanup EXIT

mkdir -p "${RESULTS}"

psql_c()  { docker exec "${PG_CONTAINER}" psql -U aster -d "$1" -tAc "$2"; }
psql_in() { docker exec -i "${PG_CONTAINER}" psql -U aster -d "$1"; }

# ------------------------------------------------------------ machine log --
{
    echo "date: $(date -Iseconds)"
    echo "git: $(git -C "${REPO}" rev-parse HEAD) ($(git -C "${REPO}" rev-parse --abbrev-ref HEAD))"
    echo "kernel: $(uname -sr)"
    echo "cpu: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //') (nproc=$(nproc))"
    echo "mem: $(grep MemTotal /proc/meminfo)"
    echo "rustc: $(rustc -V)"
    echo "cargo: $(cargo -V)"
    echo "postgres: $(psql_c postgres 'SELECT version()')"
    # Durability GUCs the EQ3 interpretation rests on (review A6): record
    # them, don't assert them.
    echo "postgres GUCs: synchronous_commit=$(psql_c postgres 'SHOW synchronous_commit'), fsync=$(psql_c postgres 'SHOW fsync'), shared_buffers=$(psql_c postgres 'SHOW shared_buffers')"
    echo "pg container: ${PG_CONTAINER} on 127.0.0.1:${PG_PORT}"
    echo
    echo "background load (this is a developer workstation — a docker dev"
    echo "stack runs alongside the bench; snapshot below):"
    uptime
    docker stats --no-stream --format '{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}' 2>/dev/null
} | tee "${RESULTS}/machine.log"

# ------------------------------------------------------------------ build --
echo "==> cargo build --release (bins + bench example)"
(cd "${REPO}" && CARGO_TARGET_DIR="${TARGET_DIR}" \
    cargo build --release -p aster-ipc --bins --example bench_v07)

# ---------------------------------------------------------------- DB prep --
echo "==> recreating bench database ${BENCH_DB} in ${PG_CONTAINER}"
psql_c postgres "DROP DATABASE IF EXISTS ${BENCH_DB} WITH (FORCE)" >/dev/null
psql_c postgres "CREATE DATABASE ${BENCH_DB}" >/dev/null
psql_in "${BENCH_DB}" < "${REPO}/crates/store-postgres/tests/fixtures/schema.sql" >/dev/null
psql_in "${BENCH_DB}" < "${REPO}/crates/store-postgres/tests/fixtures/seed.sql" >/dev/null
# 1000 extra bench documents (ids d0000000..d00003e7 hex-extended to 16
# bytes) at ts=150 <= the pinned snapshot 200, so B1's distinct-key
# workload reads real live documents.
psql_c "${BENCH_DB}" "
    INSERT INTO convex_dev.documents (id, ts, table_id, json_value, deleted, prev_ts)
    SELECT decode(lpad(to_hex(3489660928 + g), 32, '0'), 'hex'),
           150,
           decode('0123456789abcdef0123456789abcdef', 'hex'),
           convert_to('{\"name\":\"bench-doc\",\"_id\":\"messages|bench\"}', 'UTF8'),
           false, NULL
    FROM generate_series(0, 999) g" >/dev/null

# --------------------------------------------------------- broker control --
start_broker() { # $1 log file; rest: extra env as KEY=VALUE args
    local log="$1"; shift
    env "$@" \
        ASTER_BROKER_SOCK="${SOCK}" \
        "${BIN}/aster_brokerd" >>"${log}" 2>&1 &
    BROKER_PID=$!
    for _ in $(seq 1 100); do
        grep -q "ready socket=" "${log}" 2>/dev/null && return 0
        kill -0 "${BROKER_PID}" 2>/dev/null || { echo "brokerd died:"; cat "${log}"; exit 1; }
        sleep 0.1
    done
    echo "brokerd not ready"; cat "${log}"; exit 1
}

stop_broker() {
    [[ -n "${BROKER_PID}" ]] && kill "${BROKER_PID}" >/dev/null 2>&1 || true
    wait "${BROKER_PID}" 2>/dev/null || true
    BROKER_PID=""
    rm -f "${SOCK}"
}

lease_epoch_from() { # $1 log file
    sed -n 's/.*lease epoch=\([0-9][0-9]*\).*/\1/p' "$1" | head -1
}

# ------------------------------------------------------------- B1 helpers --
percentile() { # $1 sorted file, $2 fraction; ceil-index order statistic
    awk -v q="$2" '{ a[NR]=$1 } END { i=int(NR*q); if (i<1) i=1; if (NR*q>i) i++; if (i>NR) i=NR; print a[i] }' "$1"
}

run_cell() { # $1 js file, $2 lease epoch
    env ASTER_BROKER_SOCK="${SOCK}" \
        ASTER_TENANT=tenant-bench ASTER_DEPLOYMENT=dep-bench \
        ASTER_SNAPSHOT_TS=200 ASTER_CELL_ID=cell-bench \
        ASTER_LEASE_EPOCH="$2" ASTER_MAX_TRAPS=512 \
        ASTER_JS="$1" \
        "${BIN}/aster_v8cell" 2>>"${RUN_DIR}/cell-err.log"
}

bench_workload() { # $1 js file, $2 label, $3 epoch, $4 log
    local times=() i start end
    for i in $(seq 1 "${N}"); do
        start=$(date +%s%N)
        run_cell "$1" "$3" >/dev/null || {
            echo "RUN FAIL: $2 iter $i"; tail -5 "${RUN_DIR}/cell-err.log"; exit 1;
        }
        end=$(date +%s%N)
        times+=( $(( (end - start) / 1000000 )) )
    done
    printf '%s\n' "${times[@]}" | sort -n > "${RUN_DIR}/$2.sorted"
    echo "RAW read-$2 ms=[$(IFS=,; echo "${times[*]}")]" >> "$4"
    local p50 p95 mn
    mn=$(head -1 "${RUN_DIR}/$2.sorted")
    p50=$(percentile "${RUN_DIR}/$2.sorted" 0.5)
    p95=$(percentile "${RUN_DIR}/$2.sorted" 0.95)
    echo "RESULT {\"phase\":\"read-$2\",\"samples\":${N},\"median_ms\":${p50},\"p95_ms\":${p95},\"min_ms\":${mn}}" | tee -a "$4"
}

# --------------------------------------------------------------------- B1 --
if runs b1; then
    LOG="${RESULTS}/b1-read-path.log"
    : > "${LOG}"
    echo "==> B1: read path (V8 one-shot host processes + brokerd + postgres fixture store)" | tee -a "${LOG}"
    start_broker "${LOG}" \
        ASTER_TENANT=tenant-bench ASTER_DEPLOYMENT=dep-bench \
        ASTER_SEAL_SEED=bench-seed ASTER_STORE=postgres \
        ASTER_DB_URL="${DB_URL}" ASTER_DB_SCHEMA=convex_dev \
        ASTER_SNAPSHOT_TS=200
    EPOCH="$(lease_epoch_from "${LOG}")"
    echo "broker lease epoch: ${EPOCH}" | tee -a "${LOG}"

    cat > "${RUN_DIR}/t0.js" <<'JS'
async function main() { return "noop"; }
JS
    cat > "${RUN_DIR}/t1.js" <<JS
async function main() {
  const json = await Convex.asyncSyscall("1.0/get", JSON.stringify({ id: "${DOC_ID}" }));
  return JSON.parse(json).name;
}
JS
    cat > "${RUN_DIR}/tk_same.js" <<JS
async function main() {
  let last = "";
  for (let i = 0; i < ${K}; i++) {
    const json = await Convex.asyncSyscall("1.0/get", JSON.stringify({ id: "${DOC_ID}" }));
    last = JSON.parse(json).name;
  }
  return last;
}
JS
    cat > "${RUN_DIR}/tk_distinct.js" <<JS
async function main() {
  let count = 0;
  for (let i = 0; i < ${K}; i++) {
    const id = "0123456789abcdef0123456789abcdef/" +
      (0xd0000000 + i).toString(16).padStart(32, "0");
    const json = await Convex.asyncSyscall("1.0/get", JSON.stringify({ id }));
    if (JSON.parse(json).name === "bench-doc") count++;
  }
  return count;
}
JS

    # Sanity: trap counts pin the semantics under measurement. Same-key ×K
    # is ONE trap since the S7 warm-hit fix (it was K traps when the
    # 2026-07-16 numbers were taken); distinct-key ×K is K traps.
    out="$(run_cell "${RUN_DIR}/t1.js" "${EPOCH}")"
    grep -q '"traps":1' <<<"${out}" || { echo "SANITY FAIL t1: ${out}"; exit 1; }
    out="$(run_cell "${RUN_DIR}/tk_same.js" "${EPOCH}")"
    grep -q '"traps":1' <<<"${out}" || { echo "SANITY FAIL tk_same (warm-hit regressed?): ${out}"; exit 1; }
    grep -q '"output":"ian"' <<<"${out}" || { echo "SANITY FAIL tk_same output: ${out}"; exit 1; }
    out="$(run_cell "${RUN_DIR}/tk_distinct.js" "${EPOCH}")"
    grep -q "\"traps\":${K}" <<<"${out}" || { echo "SANITY FAIL tk_distinct: ${out}"; exit 1; }
    grep -q "\"output\":${K}" <<<"${out}" || { echo "SANITY FAIL tk_distinct output: ${out}"; exit 1; }
    echo "sanity OK: t1=1 trap; tk_same(K=${K})=1 trap (warm hits); tk_distinct(K=${K})=${K} traps" | tee -a "${LOG}"

    bench_workload "${RUN_DIR}/t0.js" T0_noop "${EPOCH}" "${LOG}"
    bench_workload "${RUN_DIR}/t1.js" T1_one_trap "${EPOCH}" "${LOG}"
    bench_workload "${RUN_DIR}/tk_same.js" TK_same "${EPOCH}" "${LOG}"
    bench_workload "${RUN_DIR}/tk_distinct.js" TK_distinct "${EPOCH}" "${LOG}"

    t0=$(percentile "${RUN_DIR}/T0_noop.sorted" 0.5)
    t1=$(percentile "${RUN_DIR}/T1_one_trap.sorted" 0.5)
    tks=$(percentile "${RUN_DIR}/TK_same.sorted" 0.5)
    tkd=$(percentile "${RUN_DIR}/TK_distinct.sorted" 0.5)
    {
        echo "derived (p50, host-process spawn cancels in subtraction):"
        echo "  first-trap cost (T1-T0):                  $(( t1 - t0 )) ms"
        awk -v a="$tks" -v b="$t1" -v k="$K" 'BEGIN{printf "  warm-hit marginal ((TKsame-T1)/(K-1)):    %.4f ms/read\n", (a-b)/(k-1)}'
        awk -v a="$tkd" -v b="$t1" -v k="$K" 'BEGIN{printf "  distinct-trap marginal ((TKdist-T1)/(K-1)): %.3f ms/trap (capsule grows 1->%d)\n", (a-b)/(k-1), k}'
    } | tee -a "${LOG}"

    echo "psql baseline (10x point read, in-container):" | tee -a "${LOG}"
    docker exec "${PG_CONTAINER}" bash -c \
        "time for i in \$(seq 1 10); do psql -U aster -d ${BENCH_DB} -tAc 'SELECT 1 FROM convex_dev.documents LIMIT 1' >/dev/null; done" 2>&1 \
        | grep real | tee -a "${LOG}"
    stop_broker
fi

# --------------------------------------------------------------------- B2 --
if runs b2; then
    LOG="${RESULTS}/b2-reseal-curve.log"
    : > "${LOG}"
    echo "==> B2: reseal curve (memory-store brokerd; per-trap cost at constant capsule size n)" | tee -a "${LOG}"
    SEEDS=""
    for i in $(seq 0 999); do
        SEEDS+="items/k$(printf '%06d' "$i"):value:${i},"
    done
    start_broker "${LOG}" \
        ASTER_TENANT=tenant-bench ASTER_DEPLOYMENT=dep-bench \
        ASTER_SEAL_SEED=bench-seed ASTER_SNAPSHOT_TS=1 \
        ASTER_SEED_I64="${SEEDS%,}"
    "${BIN}/examples/bench_v07" reseal \
        --sock="${SOCK}" --tenant=tenant-bench --deployment=dep-bench \
        --snapshot=1 --epoch=1 \
        --sizes=1,10,50,100,200,500,1000 --samples=300 --warmup=30 \
        | tee -a "${LOG}"
    stop_broker
fi

# --------------------------------------------------------------------- B3 --
if runs b3; then
    LOG="${RESULTS}/b3-fence-isolated.log"
    : > "${LOG}"
    echo "==> B3: fence isolated (direct WritePlane against ${BENCH_DB})" | tee -a "${LOG}"
    "${BIN}/examples/bench_v07" fence \
        --db-url="${DB_URL}" \
        --tenant=bench-fence --deployment="dep-fence-$(date +%s)" \
        --blind=1500 --points=1,10,50,200 --windows=1,10,50 \
        --window-log=1000 --val-samples=200 --conflicts=100 \
        | tee -a "${LOG}"
fi

# --------------------------------------------------------------------- B4 --
if runs b4; then
    LOG="${RESULTS}/b4-e2e-write.log"
    : > "${LOG}"
    echo "==> B4: write path e2e (V8 cell -> UDS Commit verb -> Postgres fence)" | tee -a "${LOG}"
    E2E_DEP="dep-e2e-$(date +%s)"
    # The brokerd pins its capsule snapshot to the Convex fixture ts (200),
    # and the fence requires snapshot <= aster-log horizon — two timestamp
    # spaces v0.7 does not unify (F9 scope). Seed the log tip to 200 first.
    "${BIN}/examples/bench_v07" fence-seed \
        --db-url="${DB_URL}" --tenant=tenant-bench --deployment="${E2E_DEP}" \
        --target-tip=200 | tee -a "${LOG}"
    start_broker "${LOG}" \
        ASTER_TENANT=tenant-bench ASTER_DEPLOYMENT="${E2E_DEP}" \
        ASTER_SEAL_SEED=bench-seed ASTER_STORE=postgres \
        ASTER_DB_URL="${DB_URL}" ASTER_DB_SCHEMA=convex_dev \
        ASTER_SNAPSHOT_TS=200
    EPOCH="$(lease_epoch_from "${LOG}")"
    echo "broker lease epoch: ${EPOCH}" | tee -a "${LOG}"
    "${BIN}/examples/bench_v07" e2e \
        --sock="${SOCK}" --tenant=tenant-bench --deployment="${E2E_DEP}" \
        --snapshot=200 --epoch="${EPOCH}" --read-id="${DOC_ID}" \
        --samples=500 --warmup=30 \
        | tee -a "${LOG}"
    stop_broker
fi

echo "==> done; raw logs in ${RESULTS}"
