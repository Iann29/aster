# Aster Runner v0.3 theory register

## Theory 1: Promise-trap continuations are the V8-compatible form of read traps

**Hypothesis:** Aster should not try to capture arbitrary V8 stacks. If Convex database reads are represented as host-created Promises, then `await db.get(...)` is already a delimited continuation boundary: V8 preserves the JS async frame, the host hydrates the capsule, resolves the promise, and a microtask checkpoint resumes the function.

**Argument:** The v0.2 `aster-v8cell` crate demonstrates this with real `v8` bindings. `Aster.read` returns a value for warm capsule entries and returns a pending `Promise` for missing entries. The Rust cell stores the `PromiseResolver`, emits a typed trap, hydrates from `MvccStore` at the original timestamp, resolves the same resolver, calls `perform_microtask_checkpoint`, and the original async JS function completes with `42`.

**Falsification:** Run the same experiment against Convex's real bundled `convex/server` database APIs, not the toy `Aster.read`, and show that `db.get` or query iteration contains synchronous assumptions that cannot return a host Promise without backend changes. A stronger falsifier is a real user Convex function where a missing read occurs below multiple async helper layers and V8 loses state or reorders effects.

**If true, consequences:** Aster can keep the read-trap architecture, but must define the continuation boundary as `await` over Convex async syscalls. Synchronous `db.getSync`-style APIs are out of scope. The adapter work becomes “swap the database syscall executor for a trap-aware capsule syscall executor,” not “serialize V8 stacks.”

**If false, consequences:** Aster falls back to a two-pass deterministic replay model: first run records missing reads and aborts; the broker hydrates; second run restarts from entry with a larger capsule. That preserves capability separation but loses mid-execution continuation latency advantages.

**Status:** demonstrated for a minimal V8 API in-process and across a UDS process boundary; partially demonstrated for Convex because Convex's own runner already drives UDF async syscalls through pending PromiseResolvers and explicit microtask checkpoints.

## Theory 2: Capsule seals are the security boundary, root hashes are just names

**Hypothesis:** A capsule root hash only names bytes. A production cell/broker boundary needs a keyed, context-bound seal over canonical capsule bytes, cell identity, and lease epoch. Without this, a compromised cell can replay or splice capsules across contexts even if hashes are collision-resistant.

**Argument:** v0.2 adds `aster-capsule::seal`: canonical BLAKE3 digest plus keyed BLAKE3 MAC. The e2e test proves unchanged capsules verify, wrong-cell replay fails, and post-seal document tampering fails. BLAKE3 digest gives content addressing; keyed mode gives bearer-capability authenticity. Binding `cell_id` and `lease_epoch` turns the seal into a narrow grant, not a universal snapshot token.

**Falsification:** Build a broker/cell IPC fuzz harness that mutates serialized capsules and shows a modified capsule can still verify or be hydrated. Cryptographic falsification would be a practical BLAKE3 keyed-mode forgery, which is not expected; engineering falsification is more likely through canonicalization bugs.

**If true, consequences:** Every hydrate request in production must carry a sealed capsule reference. The broker should reject unsealed capsules except in dev mode. Root hashes remain useful in traces, but never as authorization.

**If false, consequences:** Replace keyed BLAKE3 with Ed25519 signatures over a protobuf canonical form or with HMAC-SHA-256 over a separately audited canonical encoding. The architecture still needs a seal; only the construction changes.

**Status:** demonstrated in prototype for in-memory capsules and serialized JSON IPC capsules. Production still needs protobuf/canonical-wire fuzzing and key rotation.

## Theory 3: Syscall inversion is a smaller Convex integration than wire emulation

**Hypothesis:** The fastest path to unmodified Convex compatibility is not to clone Funrun's full private service. It is to exploit the existing isolate syscall loop: represent database reads as async syscalls that can return “trap” to an adapter, hydrate capsules, and resolve the existing PromiseResolvers.

**Argument:** Reading `convex-backend` shows `crates/isolate/src/environment/udf/mod.rs` already invokes the user function, drains microtasks, pops pending syscalls, batches database reads (`1.0/get`, `1.0/queryStreamNext`), runs them outside JS, then resolves their PromiseResolvers. Aster's V8 proof is the same control shape with a different syscall executor. Therefore, the architectural seam is inside the function-runner service, not inside V8 itself.

**Falsification:** Generate the actual remote-runner ABI fixture and show that the open-source backend cannot route database syscalls through a remote runner, or that the transaction object must remain in-process with the backend for read authorization and query journals in a way that cannot be represented by Aster results.

**If true, consequences:** `aster-adapter` should first implement a Convex-version-pinned compatibility harness around UDF syscall batches, not a general “Funrun clone.” Query journals and read/write sets become the wire contract that matters.

**If false, consequences:** The integration shifts to a backend patch: add an explicit open remote-runner ABI for snapshot read syscalls. The capability architecture survives, but “unmodified backend” becomes “small upstreamable diff.”

**Status:** speculative/partially demonstrated. The code experiment proves the V8 mechanism; repository reading identifies the Convex seam; no real Convex compatibility fixture yet.

## Theory 4: Process boundaries are a necessary but insufficient authority boundary

**Hypothesis:** Moving the broker and V8 cell into separate OS processes materially improves the authority story because cells no longer share an address space with the store or seal key, but it is not sufficient without kernel sandboxing, peer credentials, lease enforcement, and request sequencing.

**Argument:** v0.3's `aster-ipc` E2E spawns `aster_brokerd` and `aster_v8cell`. The cell binary uses `UdsCapsuleBrokerClient`, has no `MvccStore` import, receives no seal key, emits a missing read over UDS, and resumes V8 after the broker reseals. Wrong-cell hydrate replay is rejected over the same socket. This closes the v0.2 in-process authority gap. However, any same-host process that can connect to the socket can still attempt parser attacks or replay valid sealed capsules unless the broker checks peer credentials and leases.

**Falsification:** Show that the cell process can obtain document values or forge hydrated capsules without using the broker socket, or show that the process boundary adds no containment under a realistic V8 escape because the cell still has equivalent OS authority. A subtler falsifier is a replay/credential-spoofing attack that succeeds despite correct seals.

**If true, consequences:** v0.4 should harden the process boundary rather than adding more in-process features: `SO_PEERCRED`, per-cell socket directories, supervisor-issued lease epochs, trap sequence numbers, cgroups/seccomp/namespaces, and broker parser fuzzing.

**If false, consequences:** If process separation is insufficient in practice, Aster needs a stronger isolation primitive earlier: gVisor/Firecracker per cell, a broker request diode, or deterministic replay back in the trusted backend.

**Status:** demonstrated as a runnable authority split, not yet production-hardened.

## Theory 5: Cells should be routed by capsule locality, not only by tenant affinity

**Hypothesis:** Tenant pinning is necessary for security, but insufficient for performance. For hot deployments, the scheduler should route an invocation to the cell that is most likely to already hold the capsule deltas for that function/read-set shape.

**Argument:** v0.1 round-robin maximizes fairness but discards locality. Cold traps are roughly 2x the warm path in the v0.2 microbenchmark even before IPC/database latency. In production, a trap can be a UDS round trip and read-pool hit. A rendezvous hash over `(deployment, module_hash, learned_readset_fingerprint)` should keep similar invocations on the same warm cells while still allowing failover.

**Falsification:** Implement three schedulers—round-robin, tenant-only rendezvous, and readset-fingerprint rendezvous—then replay a production trace or synthetic Zipf workload. If trap rate or p95 latency does not improve enough to offset skew, the theory is false.

**If true, consequences:** The host needs a readset-fingerprint cache and cell-local capsule delta cache. Metrics should include trap rate by scheduler decision.

**If false, consequences:** Keep round-robin plus broker LRU. Simpler scheduling wins, and prewarming belongs entirely in the broker.

**Status:** speculative.

## Theory 6: Effect fences can be generalized into a causal ledger for all non-database authority

**Hypothesis:** The action effect fence should not be a special case for webhooks. It should be a generic causal ledger entry for every external authority: network egress, secret access, file storage, scheduled jobs, and nested Convex calls.

**Argument:** A cell that has no ambient authority must request capabilities. Each request can name the capsule digest, read observations, idempotency key, and freshness policy. The egress broker can then decide whether replay is safe. This mirrors database OCC for non-database effects.

**Falsification:** Implement a durable ledger and run crash tests around every point in the effect state machine. If ambiguous states cannot be resolved without operator intervention or duplicate effects under realistic APIs, the generic ledger is too broad.

**If true, consequences:** Aster action support becomes a capability ledger problem, not just an HTTP proxy problem. Synapse can expose audit trails per effect type.

**If false, consequences:** Keep fences only for idempotent HTTP effects; route complex actions back to Convex's local runner or require user-supplied idempotency contracts.

**Status:** speculative; v0.1/v0.2 only demonstrate the fence shape.
