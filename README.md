# Aster Runner v0.2

Aster Runner is a research prototype for capability-narrowed Convex function execution.

v0.2 adds two real properties over v0.1:

1. **V8-backed Promise read traps.** `aster-v8cell` embeds the real Rust `v8` crate. A JavaScript async function can `await Aster.read(...)`, suspend on a missing capsule entry, let the host hydrate the capsule, and resume inside the same isolate.
2. **Cryptographic capsule seals.** `aster-capsule::seal` provides a canonical BLAKE3 digest plus keyed BLAKE3 MAC bound to cell identity and lease epoch. Wrong-cell replay and tampering are tested.

Run:

```bash
cargo build --workspace
cargo test --workspace
cargo run --release -p aster-host --bin aster_bench -- 10000 32
protoc --proto_path=proto --descriptor_set_out=/tmp/aster-v0.2.pb proto/aster.proto
```

Important docs:

- `docs/ARCHITECTURE.md` — v0.2 architecture
- `docs/V8_QUESTION.md` — V8 experiment memo
- `docs/THEORY_REGISTER.md` — research theories
- `docs/ABSURD_IDEAS.md` — intentionally strange/falsifiable ideas
- `docs/COMPARISON_MATRIX_V0.2.md` — prior-art matrix
- `docs/SYNAPSE_MIGRATION_V0.2.md` — operator migration path
