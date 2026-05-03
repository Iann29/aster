# Aster Runner

Aster Runner is a prototype open-source execution plane for self-hosted Convex deployments. It demonstrates a design that scales function execution past the per-process V8 isolate-thread ceiling without copying Convex Cloud's Funrun architecture.

The core idea is **Snapshot Capsules + Read-Trap Continuations + Tenant-Pinned Sandbox Cells**:

- The Convex backend remains the single writer and committer.
- Runner cells receive immutable, tenant-scoped MVCC capsules instead of database credentials.
- Missing reads become explicit read traps against a broker at the same snapshot timestamp.
- Mutations return read/write sets; OCC decides whether they commit.
- Actions return effect fences so side effects are not blindly retried.

This repository is a prototype for review, not production infrastructure. The code is intentionally `std`-only so a reviewer can inspect the invariants without generated code or service scaffolding.

## Layout

```text
crates/capsule   MVCC store, snapshot capsules, read/write sets, OCC committer
crates/runner    Tenant-pinned sandbox cells and read-trap execution loop
crates/host      Adapter-like facade, E2E tests, and benchmark binary
proto/aster.proto Open internal capsule-fabric protocol
docs/ARCHITECTURE.md Full design analysis
docs/bench_results.md Benchmark and theoretical analysis
```

## Intended commands on a Rust-equipped machine

```bash
cargo test --workspace
cargo run --release -p aster-host --bin aster_bench -- 10000 32
```

The generation environment for this artifact did not include `rustc`, `cargo`, or `protoc`; see `docs/toolchain_check.txt`. The Python mirror benchmark in `scripts/mirror_bench.py` was executed in that environment and records the same core algorithmic path.

## License

Dual licensed under Apache-2.0 or MIT, at your option.
