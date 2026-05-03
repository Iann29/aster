# Aster Runner v0.3

Aster Runner is a research prototype for capability-narrowed Convex function execution.

v0.3 adds a new real property over v0.2:

1. **Process-separated brokered V8 cells over Unix-domain sockets.** `aster-ipc` provides length-prefixed JSON frames over UDS, a broker process (`aster_brokerd`) that owns `MvccStore` + seal key, and a V8 cell process (`aster_v8cell`) that owns JS execution but receives no store or seal key.
2. **V8-backed Promise read traps still work.** A JavaScript async function can `await Aster.read(...)`, suspend on a missing capsule entry, hydrate through the broker socket, and resume inside the same real V8 isolate.
3. **Cryptographic capsule seals cross the boundary.** Sealed capsules serialize over IPC; wrong-cell replay and tampering are rejected, including in the process E2E.

Run:

```bash
cargo fmt --all -- --check
cargo build --workspace
cargo test --workspace
cargo test -p aster-ipc --test process_boundary -- --nocapture
cargo run --release -p aster-host --bin aster_bench -- 10000 32
protoc --proto_path=proto --descriptor_set_out=/tmp/aster-v0.3.pb proto/aster.proto
```

## Docker images

Both binaries ship as separate runtime images out of the same multi-stage
Dockerfile:

```bash
# Build
docker build --target=runtime-broker -t aster-brokerd:0.3 -f docker/Dockerfile .
docker build --target=runtime-v8cell -t aster-v8cell:0.3 -f docker/Dockerfile .

# End-to-end smoke (assertions inside the script)
./docker/smoke.sh 0.3
```

The `docker/smoke.sh` script runs `aster-brokerd` as a long-lived service
behind a per-deployment Docker volume, then runs `aster-v8cell` as a
one-shot container that opens the shared socket and prints
`{"output":42,"traps":1,"capsule_hash":...}` if the capability boundary
holds.

## Important docs

- `docs/ARCHITECTURE.md` — v0.3 architecture
- `docs/V8_QUESTION.md` — V8 experiment memo updated for IPC
- `docs/THEORY_REGISTER.md` — research theories
- `docs/ABSURD_IDEAS.md` — intentionally strange/falsifiable ideas
- `docs/COMPARISON_MATRIX_V0.3.md` — prior-art matrix
- `docs/SYNAPSE_MIGRATION_V0.3.md` — operator migration path
