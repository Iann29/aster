# Aster Runner v0.3 comparison matrix

| System | Runtime / isolation | V8 / continuation model | Capability boundary | State model | Recovery story | Where Aster v0.3 differs |
|---|---|---|---|---|---|---|
| Convex in-process OSS runner | V8 isolates inside backend process | Async Promise syscalls for DB operations | Backend process has transaction and execution authority | Transaction object remains in backend | Backend retries; process restart loses in-flight work | Aster splits V8 execution into a cell process and read authority into a broker process |
| Convex Funrun-style remote runner | Remote Rust/V8 execution service | Normal V8 async execution, remote snapshot loading | Runner tier can load/read snapshots | Read-only execution replicas return outcomes to backend | Remote runner failover and backend committer | Aster cells still do not become read replicas; they request sealed capsule hydration from broker |
| Deno / deno_core | V8 ops and async resources | Promise-backed async ops, microtask checkpoints | Host ops enforce permissions | External services, no built-in MVCC capsule | Isolate restart/platform retry | Aster now uses a similar async-op shape but sends cold reads to a separate UDS broker |
| Cloudflare workerd | Dense V8 isolates in hardened process model | Event loop and async host APIs | Bindings mediate services | KV/D1/R2/Durable Objects | Isolate eviction/restart | Aster borrows Promise-hostcall shape but ties data authority to MVCC capsule seals |
| PostgreSQL sidecar/proxy | Separate process owns DB credentials | Not a V8 system | App talks to proxy instead of DB | SQL/state proxy | Proxy reconnect/retry | Aster broker is narrower: hydrate sealed point/range traps, not arbitrary SQL |
| Envoy external auth / service mesh sidecars | Separate process mediates network authority | Not a V8 system | Local socket/proxy capability | Network requests | Retry/backoff policies | Aster v0.3 similarly makes local IPC the authority seam, but for database snapshot reads |
| Firecracker/gVisor sandboxes | Strong VM/kernel boundary | V8 inside guest/process if used | VM/syscall boundary | App-defined | Restart/reschedule VM | Aster v0.3 is lighter but weaker: process+UDS only, no syscall sandbox yet |
| Shopify Functions | WASM bounded input | No V8; no interactive reads | Host passes bounded input | Snapshot-like input payload | Rerun function | Aster capsules are bounded input plus interactive trap fallback |
| FoundationDB clients | Client library has DB credentials | No V8 | DB auth at client | Serializable transactions | Transaction retries | Aster removes DB credentials from cells and leaves commit authority outside runner |

## v0.3 note

The new prior-art axis is not only “does it use V8?” but “where does read authority live?” v0.3 moves Aster closer to sidecar/proxy systems operationally while keeping the distinctive capsule seal: a cell does not ask a proxy for arbitrary reads; it presents a sealed snapshot capability and a typed missing-read trap.
