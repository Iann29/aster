# Local validation — 2026-05-03

The original artifact was generated in a container without `rustc`, so
`docs/bench_results.md` could only ship a Python mirror benchmark. This
document records a real Rust validation run on a developer machine.

## Environment

- OS: Linux 6.19.13-arch1-1
- CPU: 16 cores
- RAM: 31 GiB
- Rust: 1.94.1 (e408947bf 2026-03-25)
- Cargo: 1.94.1 (29ea6fb6a 2026-03-24)
- protoc: 34.1

## Build

```bash
cargo build --workspace
```

Result: `Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.01s`.
The `v8 = "0.106"` crate downloads a prebuilt `libv8.a` (~150 MB) on first
build; subsequent builds use the cached archive.

## Test suite

```bash
cargo test --workspace
```

Result: **11 passed, 0 failed** across 7 binaries.

Notable assertions verified:

- `aster_capsule::seal::tests::sealed_capsule_rejects_tampered_document`
  — BLAKE3 keyed MAC catches digest mutation
- `aster_capsule::seal::tests::sealed_capsule_rejects_wrong_cell_context`
  — capsule sealed for `cell-a` is rejected by `cell-b`
- `aster_capsule::tests::commit_rejects_changed_read`
  — OCC conflict detection on the in-memory MVCC store
- `aster_runner::tests::query_hydrates_missing_key_with_read_trap`
  — read trap loop hydrates a missing key and resumes
- `aster_v8cell::tests::v8_async_function_resumes_after_read_trap`
  — real V8 isolate suspends on `await Aster.read(...)`, host hydrates
  via `PromiseResolver`, microtask checkpoint resumes the async function
- E2E (`tests/e2e.rs`) covers query+mutation+action through the host
  facade plus the OCC conflict invariant on two mutations over the same
  snapshot
- E2E (`tests/crypto_and_v8.rs`) couples the seal verifier with the V8
  cell to prove the two security primitives interoperate

## Release benchmark

```bash
cargo run --release -p aster-host --bin aster_bench -- 10000 32
```

Output:

```json
{"iterations":10000,"keys":32,"warm_query_avg_ns":38337,"cold_trap_query_avg_ns":79232,"mutation_avg_ns":972}
```

Interpretation, with the same caveats as `docs/bench_results.md`: these are
in-process microbenchmarks, not production latency claims. The cost shape
matches the design intent — the cold path that emits 32 traps is roughly
2× the warm path because every missing read forces a continuation
boundary; the mutation path is dominated by the OCC commit and stays
under 1 µs because no JavaScript is involved.

## What is intentionally not validated here

- Convex backend wire compatibility — no compatibility fixture exists yet.
- gRPC server for `proto/aster.proto` — no `tonic` integration yet.
- OS-level cell sandboxing (cgroups v2, seccomp, read-only rootfs).
- Action egress broker / effect ledger durability.

These are the open work items called out in the production roadmap
section of `docs/ARCHITECTURE.md`.
