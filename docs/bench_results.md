# Benchmark results and capacity analysis

## Validation status

The artifact was generated in a container that did not include `rustc`, `cargo`, or `protoc`. The attempted toolchain check is stored in `docs/toolchain_check.txt` and begins with:

```text
bash: line 1: rustc: command not found
```

Network installation was not possible in the same container; `apt-get update` failed with temporary DNS resolution failures. Therefore I cannot honestly claim that `cargo test`, `cargo run --release`, or `protoc` were executed here. The workspace is written as a `std`-only stable Rust workspace and is intended to be validated with:

```bash
cargo test --workspace
cargo run --release -p aster-host --bin aster_bench -- 10000 32
protoc --proto_path=proto --descriptor_set_out=/tmp/aster.pb proto/aster.proto
```

A Python mirror benchmark was executed to provide real, reproducible numbers for the same algorithmic path.

## Executed Python mirror benchmark

Command:

```bash
python3 scripts/mirror_bench.py --iterations 5000 --keys 32
```

Output:

```json
{
  "cold_trap_query_avg_ns": 56476,
  "cold_trap_query_p50_ns": 53631,
  "cold_trap_query_p95_ns": 73086,
  "cold_trap_query_total_ns": 282381317,
  "iterations": 5000,
  "keys": 32,
  "mutation_avg_ns": 2354,
  "mutation_p50_ns": 1978,
  "mutation_p95_ns": 3836,
  "mutation_total_ns": 11773697,
  "warm_query_avg_ns": 13090,
  "warm_query_p50_ns": 12492,
  "warm_query_p95_ns": 16540,
  "warm_query_total_ns": 65454440
}
```

Interpretation:

- Warm capsule query, 32 point reads, zero traps: **13.1 microseconds average** in Python.
- Cold capsule query, 32 point reads, 32 traps: **56.5 microseconds average** in Python.
- Single-key mutation attempt plus OCC commit: **2.35 microseconds average** in Python.

These numbers are not production latency claims. They show the intended cost shape. A cold capsule is about 4.3x slower than a warm capsule in the Python mirror because every missing read forces a continuation boundary. In production each trap would add at least local IPC/RPC latency plus broker cache/database read time. The design therefore relies on read-set learning and prewarming for hot functions.

## Rust microbenchmark included

The Rust benchmark binary in `crates/host/src/bin/aster_bench.rs` measures the same three paths:

1. Warm query: all keys are prewarmed into the capsule.
2. Cold trap query: the capsule starts empty and hydrates every key through read traps.
3. Mutation: one read, one write, and a single-writer OCC commit.

Expected command on a Rust-equipped machine:

```bash
cargo run --release -p aster-host --bin aster_bench -- 10000 32
```

The benchmark prints a JSON line:

```json
{"iterations":10000,"keys":32,"warm_query_avg_ns":...,"cold_trap_query_avg_ns":...,"mutation_avg_ns":...}
```

## Capacity model

Let:

- `W = 128`, the worker-thread ceiling per V8 process under the Convex deployment runner model.
- `C = number of Aster cells for one deployment`.
- `S = average function execution service time in seconds, excluding commit wait`.
- `M = average mutation commit service time in seconds`.
- `R = average trap round-trip time`.
- `T = average traps per invocation`.

### In-process baseline

The deployment can run at most `W` concurrent executions. For read-heavy workloads, the execution throughput ceiling is roughly:

```text
throughput_baseline ~= W / S
```

For example, if average query/action execution service time is 25 ms, the execution pool saturates near:

```text
128 / 0.025 = 5120 executions/second
```

The visible operator symptom is not only throughput; it is queueing. A burst of 500 simultaneous 100 ms actions must run in waves:

```text
ceil(500 / 128) * 100ms = 400ms minimum execution-wave time
```

This excludes backend scheduling overhead and commit waits.

### Funrun-style remote replicas

A conventional remote-runner design with `N` runner replicas, each with `W` V8 worker threads, has an execution-slot budget:

```text
slots_funrun ~= N * W
```

The documented Funrun design also lets replicas load snapshots and cache modules/indexes. It preserves the backend as committer, so mutation commit throughput remains limited by the deployment committer:

```text
mutation_throughput <= 1 / M
```

Remote execution removes the one-process ceiling but spreads read authority into the runner tier.

### Aster cells

Aster has the same first-order execution-slot budget as remote replicas:

```text
slots_aster ~= C * W
```

With eight cells for one hot deployment:

```text
8 * 128 = 1024 execution slots
```

The per-invocation service time is:

```text
S_aster ~= S_v8 + S_capsule_local + T * (R + S_broker_read)
```

For warm capsules, `T` should be zero or near zero. For cold capsules, trap latency dominates. Aster therefore trades cold-read latency for a narrower trust boundary: cells do not receive database credentials.

### Mutation reality check

Aster does not claim to multiply durable write throughput by `C`. If the committer takes 2 ms per committed mutation, the deployment writer can commit at most about 500 mutations/second regardless of the number of execution cells. More cells can still help because failed/conflicting mutation attempts and JS-heavy mutation bodies no longer occupy the backend's in-process runner pool, but the commit bottleneck remains real and observable.

## What to measure next

A production implementation should add:

- End-to-end Convex compatibility benchmark: unmodified backend -> `aster-adapter` -> cells -> backend committer.
- Trap-latency benchmark over Unix-domain sockets and loopback TCP.
- Capsule-size sweep: 1, 8, 32, 256, and 1024 documents.
- Cell-count sweep: 1, 2, 4, 8 cells on one VPS.
- Conflict workload: concurrent increments over one hot document.
- Action workload: effect fence ledger with idempotency replay.
- Security overhead sweep: native process sandbox vs gVisor-backed cells.
