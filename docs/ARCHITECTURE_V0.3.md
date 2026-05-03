# Aster Runner v0.3: process-separated brokered V8 cells

See `docs/ARCHITECTURE.md` for the active v0.3 architecture document. This archive copy marks the v0.3 development turn: real V8 Promise read traps now hydrate over a Unix-domain-socket broker process instead of an in-process broker trait.

Key runnable property:

```bash
cargo test -p aster-ipc --test process_boundary -- --nocapture
```

That test spawns `aster_brokerd` and `aster_v8cell`, resumes JS to produce `42`, and verifies wrong-cell capsule replay is rejected over the socket.
