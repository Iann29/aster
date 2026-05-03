# Benchmark results v0.2

## Validation status

This v0.2 artifact was validated with a real Rust and protoc toolchain in the working environment.

Toolchain excerpt:

```text
cargo 1.94.1 (29ea6fb6a 2026-03-24)
rustc 1.94.1 (e408947bf 2026-03-25)
libprotoc 34.1
Linux omarchy 6.19.13-arch1-1 x86_64 GNU/Linux
```

Commands executed:

```bash
cargo build --workspace
cargo test --workspace
cargo run --release -p aster-host --bin aster_bench -- 10000 32
protoc --proto_path=proto --descriptor_set_out=/tmp/aster-v0.2.pb proto/aster.proto
```

Captured logs live in:

- `docs/cargo_build_v0.2.txt`
- `docs/cargo_test_v0.2.txt`
- `docs/bench_results_v0.2.json`
- `docs/bench_results_v0.2.stderr`
- `docs/protoc_v0.2.txt`
- `docs/toolchain_check_v0.2.txt`

## Rust microbenchmark

The benchmark binary remains:

```bash
cargo run --release -p aster-host --bin aster_bench -- 10000 32
```

One captured v0.2 run:

```json
{"iterations":10000,"keys":32,"warm_query_avg_ns":37077,"cold_trap_query_avg_ns":76319,"mutation_avg_ns":963}
```

A later post-format run produced similar numbers:

```json
{"iterations":10000,"keys":32,"warm_query_avg_ns":36335,"cold_trap_query_avg_ns":76740,"mutation_avg_ns":965}
```

Interpretation:

- Warm query remains comparable to v0.1's validated hot path.
- Cold trap query remains roughly twice warm query in the in-process prototype; production traps will include IPC/RPC and broker read latency, so prewarming and locality remain critical.
- Mutation hot path remains sub-microsecond to about one microsecond in this in-memory OCC model.

## New benchmark caveat

The V8 proof is tested but not included in the hot-path benchmark. That is intentional: v0.2's benchmark preserves v0.1's microbenchmark comparability. A future `aster_v8_bench` should separately measure isolate startup, warm async read, missing-read Promise trap, microtask checkpoint overhead, and module-cache reuse.
