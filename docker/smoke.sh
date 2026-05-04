#!/usr/bin/env bash
# Black-box smoke for the Aster Docker images.
#
# Boots `aster-brokerd` + `aster-v8cell` from the published image tags,
# wires them through a shared volume containing a Unix-domain socket,
# loads a tenant JS file from a host-mounted directory, and asserts the
# cell exits with the expected output JSON.
#
# Why a shell script and not a Go test: this exercises the *images*, not
# the Rust crate. CI builds the images first (one cargo download +
# compile), then runs this once, then publishes the digests. The Rust
# `cargo test --workspace` already covers everything in-process.
#
# Usage:
#   docker/smoke.sh [tag]
#
#   tag defaults to "0.4" — must match what `docker build --target=...`
#   tagged the broker/v8cell images as.

set -euo pipefail

TAG="${1:-0.4}"
HERE="$(cd "$(dirname "$0")" && pwd)"
BROKERD_IMAGE="${ASTER_BROKERD_IMAGE:-aster-brokerd:${TAG}}"
V8CELL_IMAGE="${ASTER_V8CELL_IMAGE:-aster-v8cell:${TAG}}"
SUFFIX="$(date +%s)-$$"
NETWORK="aster-smoke-${SUFFIX}"
VOLUME="aster-smoke-sock-${SUFFIX}"
BROKER="aster-smoke-brokerd-${SUFFIX}"

cleanup() {
    rc=$?
    set +e
    docker rm -f "${BROKER}" >/dev/null 2>&1
    docker volume rm "${VOLUME}" >/dev/null 2>&1
    docker network rm "${NETWORK}" >/dev/null 2>&1
    exit "${rc}"
}
trap cleanup EXIT

echo "==> creating network + volume (suffix: ${SUFFIX})"
docker network create "${NETWORK}" >/dev/null
docker volume create "${VOLUME}" >/dev/null

echo "==> starting brokerd (image: ${BROKERD_IMAGE})"
docker run -d \
    --name "${BROKER}" \
    --network "${NETWORK}" \
    -v "${VOLUME}:/run/aster" \
    -e ASTER_BROKER_SOCK=/run/aster/broker.sock \
    -e ASTER_TENANT=tenant-smoke \
    -e ASTER_DEPLOYMENT=dep-smoke \
    -e ASTER_SEED_I64='counters/a:value:20,counters/b:value:22' \
    -e ASTER_SEAL_SEED=smoke-seed \
    -e ASTER_MAX_CONNECTIONS=8 \
    "${BROKERD_IMAGE}" >/dev/null

# The broker logs "ready socket=..." on stderr the moment it binds.
# Poll up to 10s — production startup is well under 100ms but CI can
# be slower under cgroup contention.
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

echo "==> running v8cell (image: ${V8CELL_IMAGE})"
output="$(docker run --rm \
    --network "${NETWORK}" \
    -v "${VOLUME}:/run/aster" \
    -v "${HERE}/tenant:/tenant:ro" \
    -e ASTER_BROKER_SOCK=/run/aster/broker.sock \
    -e ASTER_TENANT=tenant-smoke \
    -e ASTER_DEPLOYMENT=dep-smoke \
    -e ASTER_SNAPSHOT_TS=2 \
    -e ASTER_CELL_ID=cell-smoke-1 \
    -e ASTER_LEASE_EPOCH=7 \
    -e ASTER_PREWARM=counters/a \
    -e ASTER_MAX_TRAPS=8 \
    -e ASTER_JS=/tenant/main.js \
    "${V8CELL_IMAGE}")"

echo "==> v8cell stdout: ${output}"

# Don't pin the capsule_hash — it depends on the seal seed and the BLAKE3
# digest input ordering, both of which are stable but tested elsewhere.
# What we lock in here is "the function returned 42 with exactly one trap".
if ! grep -q '"output":42' <<<"${output}"; then
    echo "ERROR: expected output=42 in cell stdout"
    exit 1
fi
if ! grep -q '"traps":1' <<<"${output}"; then
    echo "ERROR: expected exactly one read trap (cold counter/b)"
    exit 1
fi

echo "OK: aster v${TAG} brokerd + v8cell smoke passed"
