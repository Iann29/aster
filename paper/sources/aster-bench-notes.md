# Aster — first real end-to-end benchmark (2026-07-16)

The only numbers that existed before today (`docs/bench_results*.json`, ~36µs warm) measured the in-memory TOY runner. These are the first measurements of the REAL pipeline: one-shot `aster-v8cell` container → V8 isolate → `Convex.asyncSyscall("1.0/get")` trap → UDS → `aster-brokerd` (seal verify + reseal) → Postgres 16 point read at a pinned snapshot.

## Setup

- Host: Ian's workstation (Arch, docker). Images built from repo @ `815ed0a` (`docker/Dockerfile`, targets `runtime-broker`/`runtime-v8cell`).
- Store: `postgres:16` seeded with the repo's own Convex-schema fixtures (`store-postgres/tests/fixtures/{schema,seed}.sql`), `ASTER_SNAPSHOT_TS=200`, `ASTER_DB_SCHEMA=convex_dev`.
- Method: three JS workloads — T0 (no syscall), T1 (1 × get), TK (K sequential gets of the same doc id) — each run N times as a fresh one-shot container against a long-lived broker. Docker container-spawn overhead cancels in the subtractions. Sanity asserted via the cell's own `"traps":N` output.
- Harness: `aster-bench.sh` (this scratchpad). Final run: N=12, K=200, exit 0.

## Results (p50 unless noted)

| Measurement | Value |
|---|---|
| Cold one-shot invocation, no reads (T0) | 390 ms (min 351, p95 398) |
| Cold one-shot invocation, 1 read (T1) | 390 ms (min 362, p95 400) |
| Cold one-shot invocation, 200 reads (TK) | 458 ms (min 422, p95 490) |
| **Marginal cost per trap — (TK−T1)/199** | **0.34 ms/trap** |
| First-trap cost (T1−T0) | below measurement noise (<±10 ms) |
| Broker sequential throughput implied | ~2,900 traps/s (single-threaded, 1 connection per trap) |

A K=16 run measured 0.47 ms/trap; K=200 tightens it to 0.34 ms/trap (signal 68 ms ≫ noise ±10 ms). Call it **~0.3–0.5 ms per trap**.

## What the per-trap number contains

Fresh UDS connect + u32-framed JSON request + broker-side full seal **verify** of the accumulated capsule + Postgres point read at the snapshot (`ts<=$ts ORDER BY ts DESC LIMIT 1`) + capsule merge + full canonical re-digest + re-MAC (**reseal**) + JSON response + V8 promise resolution. In other words: at ~0.34 ms, the *entire* capability apparatus (crypto + IPC + broker) adds roughly nothing on top of what a warm-connection Postgres point read costs by itself. The security tax on the read path is ~zero — this is the eval headline.

Caveat for honesty: the capsule stayed tiny (200 reads of the SAME id = 1 entry), so reseal cost was constant. Reseal re-encodes the WHOLE capsule (O(capsule size) per trap → O(n²) over a growing read-set). Measuring the growing-capsule curve is the next bench; incremental/Merkle-ized sealing is the obvious v0.7+ optimization if the curve bites.

The ~390 ms cold-invocation floor is docker container spawn + V8 boot — i.e., the warm-pool roadmap item (README "cell warm-pool reincarnation"), not the capsule machinery.

## Two real findings (filed for v0.7)

1. **Broker lifetime budget bug**: `ASTER_MAX_CONNECTIONS` is enforced as `listener.incoming().enumerate()` + `break` (`aster_brokerd.rs:230-234`) — it counts TOTAL connections since boot, never decrements, and the broker EXITS when crossed. With one-connection-per-trap, a default broker self-terminates after 1024 traps served. Found because the K=200 bench crossed it at invocation #4. Fix: concurrency cap (or drop the guard), not a lifetime counter.
2. **No warm-capsule short-circuit on the real syscall path**: `Convex.asyncSyscall("1.0/get")` ALWAYS traps, even when the doc is already in the sealed capsule (confirmed: 200 reads of the same id = 200 traps; the warm check exists only on the legacy `Aster.read` toy path). Costs ~0.34 ms × redundant reads today, and it makes prewarming/dream-capsules useless on the production path until fixed. Cheap, high-value v0.7 item.

## Context vs. the RAM discussion (2026-07-15)

The predicted band for "leave reads on disk via a broker" was 0.1–1 ms per access. Measured: 0.34 ms. The read-plane offload story (heavy analytical/module queries out of the reactive backend, one-shot, no subscription) is now backed by a real number: a 1,000-read report query costs ~0.4 s of trap time + ~0.4 s cold spawn — perfectly acceptable for one-shot workloads, with zero pressure on the backend's resident memory.
