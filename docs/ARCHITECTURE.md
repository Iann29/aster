# Aster Runner v0.3: process-separated brokered V8 cells

Status: research prototype with compiling Rust implementation. Scope: self-hosted Convex function execution; v0.3 specifically validates that the V8 cell can cross a real OS process boundary and hydrate missing capsule data through a Unix-domain-socket broker. Out of scope: production gRPC, seccomp/cgroups/namespaces, real Convex remote-runner compatibility, durable action ledger, and cluster scheduler.

## Executive summary

v0.2 proved the two hardest semantic claims: real V8 can resume an async function after a host Promise read trap, and capsule integrity can be cryptographically sealed with BLAKE3 keyed MACs bound to `cell_id` and `lease_epoch`.

v0.3 changes the architecture's shape again: **the broker/cell boundary is now a real Unix-domain-socket process boundary, not just a Rust trait boundary.** The new `aster-ipc` crate adds length-prefixed JSON frames over UDS, a `UdsCapsuleBrokerClient` that implements the existing `CapsuleBrokerClient` trait, and two binaries:

- `aster_brokerd`: owns the `MvccStore`, capsule seal key, hydrate policy, and UDS listener.
- `aster_v8cell`: owns the real V8 isolate and JS execution, receives no `MvccStore` and no seal key, and hydrates missing reads only by presenting sealed capsules to the broker socket.

The new end-to-end test `crates/ipc/tests/process_boundary.rs` spawns both binaries as child processes. JS starts with only `counters/a` prewarmed, awaits `Aster.read("counters/b", "value")`, emits a trap over IPC, the broker verifies the sealed capsule context, hydrates and reseals, the V8 Promise resolves, and the cell process prints `{"output":42,"traps":1,...}`. The same test then simulates wrong-cell replay over the real socket and observes a broker rejection.

The result is still not a production sandbox. A compromised cell can still parse JSON, consume CPU, and access whatever the OS grants the process. But v0.3 removes the most misleading v0.2 simplification: cells no longer need to be in the same address space as the store or seal key.

## What changed since v0.2

### 1. Broker boundary moved from trait-only to UDS IPC

v0.2 introduced `CapsuleBrokerClient` and `LocalCapsuleBroker`, but everything still ran in one process. v0.3 adds `aster-ipc`:

- `IpcRequest::InitialCapsule`
- `IpcRequest::HydratePoint`
- `IpcRequest::Shutdown`
- matching `IpcResponse` values
- `write_frame` / `read_frame` with a 1 MiB cap
- `UdsCapsuleBrokerClient`, which contains only a socket path

**Rationale:** Capability separation is not meaningful if a compromised V8 cell shares an address space with the broker's read store and seal key. UDS is the smallest runnable step toward a production broker service.

### 2. The V8 cell can run as a separate binary

`aster_v8cell` reads JS source from `ASTER_JS`, connects to `ASTER_BROKER_SOCK`, and calls the existing `V8SandboxCell::execute_async_main_with_broker`. It does not import or receive `MvccStore` or `CapsuleSealKey`.

**Rationale:** This keeps v0.2's real V8 Promise-continuation result intact while proving it survives an IPC hydrate path.

### 3. The broker can run as a separate binary

`aster_brokerd` creates the store from deterministic fixture seeds, derives a prototype seal key, binds a UDS socket, verifies incoming capsule seals, hydrates point reads, and reseals the capsule.

**Rationale:** The broker process is now the only code path that holds read authority and the seal key in the v0.3 fixture.

### 4. Capsule wire types became serializable

`aster-capsule` types now derive `serde::{Serialize, Deserialize}` where needed for IPC. `CapsuleSeal.algorithm` is now an owned `String` instead of `&'static str` so seals can cross process boundaries as JSON.

**Rationale:** A sealed capsule must be transmitted, not just borrowed in one process.

### 5. `proto/aster.proto` learned the initial-capsule flow

The protobuf file now includes `InitialCapsuleRequest` / `InitialCapsuleResponse` and adds `cell_id` to `HydrateRequest`. The running v0.3 transport is JSON/UDS, but the proto now reflects the architectural protocol.

**Rationale:** v0.2's proto had hydrate but not broker-issued initial capsules, which made the late broker work underdocumented.

## v0.3 system sketch

```text
                 host / supervisor / test harness
                            |
              spawn brokerd | spawn v8cell
                            v
+-------------------------------+          UDS length-prefixed JSON          +---------------------------+
| broker process: aster_brokerd |<------------------------------------------>| cell process: aster_v8cell |
|-------------------------------|                                           |---------------------------|
| owns MvccStore fixture        |  InitialCapsule(context, prewarm)          | owns real V8 isolate       |
| owns CapsuleSealKey           |  HydratePoint(sealed_capsule, key)         | owns JS async function     |
| verifies cell_id/lease seals  |  sealed capsule responses                 | no MvccStore               |
| hydrates at snapshot_ts       |                                           | no seal key                |
+-------------------------------+                                           +---------------------------+
```

The v0.3 process test is intentionally simple but load-bearing: the V8 continuation does not know whether `CapsuleBrokerClient` is local or UDS-backed. That means the v0.2 Promise boundary composes with a real broker boundary.

## IPC protocol

The implemented transport is not tonic/gRPC. It is a deliberately small prototype protocol:

1. u32 big-endian frame length;
2. JSON payload;
3. hard cap at `MAX_FRAME_BYTES = 1 MiB`;
4. one request per connection in the current server;
5. broker returns typed success or `WireBrokerError { code, message }`.

This is sufficient to prove the authority split. Production should replace or harden it with peer credential checks, stricter schemas, fuzzing, and backpressure.

## V8 answer after v0.3

v0.3 reinforces the v0.2 answer. Aster still should not pursue arbitrary V8 stack serialization. The successful continuation boundary remains an async host Promise. What changed is the hydrate source: the Promise resolver can now be satisfied by a UDS broker response rather than an in-process `MvccStore` read.

The essential invariant is unchanged:

- warm read: `Aster.read` returns a JS value immediately;
- cold read: `Aster.read` returns a pending Promise and records a typed trap;
- broker hydrate: verify seal, read at the original snapshot, reseal;
- resume: resolve the stored V8 `PromiseResolver` and run a microtask checkpoint.

## Threat model delta

New v0.3 threats introduced by IPC/process separation:

1. **Socket credential spoofing:** any same-host process that can connect to the socket can attempt requests. v0.3 relies on filesystem path secrecy/permissions only. Production must check `SO_PEERCRED`, tenant/cell identity, and supervisor-issued leases.
2. **Stale cell IDs:** a dead cell's sealed capsule could be replayed by a later process with the same `cell_id` and lease epoch. v0.3 seal binding catches wrong IDs but does not implement lease revocation. Production should make cell IDs incarnation-specific and broker leases short-lived.
3. **Broker parser bugs:** JSON parsing now sits in the privileged broker process. v0.3 caps frame size; production needs fuzzing and perhaps a smaller binary codec or fixed trap records.
4. **Replay of length-prefixed messages:** v0.3 does not include nonces or sequence numbers. A replayed hydrate with a still-valid sealed capsule can be accepted. Production should bind requests to invocation IDs and monotonic trap sequence numbers.
5. **Confused deputy hydrate requests:** a cell can ask for any point key at the same tenant/deployment/snapshot if it has a valid capsule. v0.3 intentionally allows this for the toy API. Production must enforce trap budgets, Convex auth/query rules, and syscall provenance.
6. **Cell retains sealed capsules after lease expiry:** v0.3 has a lease epoch field but no wall-clock expiry. Production must reject expired leases and kill old cells on key rotation.
7. **Broker crash mid-hydrate:** v0.3 point hydrate is stateless and can be retried, but in-flight invocation fails if the socket disappears. Production needs supervisor restart and structured retry behavior.
8. **Backpressure:** the current broker is single-threaded, one request per connection, no queue metrics. Trap storms can exhaust accept backlog or CPU. Production needs rate limits and per-cell budgets.

Existing v0.2 threats remain: V8 escape, microtask storms, seal key exfiltration, canonicalization bugs, Convex ABI drift, and action effect ambiguity.

## Failure-mode delta

- **Broker socket missing:** `UdsCapsuleBrokerClient` returns `BrokerError::Remote("IPC I/O error...")`; the V8 invocation fails before commit.
- **Bad frame or oversized frame:** broker returns `bad_request` when possible or closes the connection. The 1 MiB cap prevents unbounded allocation.
- **Wrong-cell hydrate:** broker rejects before reading from the store. The process E2E test proves this over UDS.
- **Snapshot mismatch:** `aster_brokerd` rejects requests whose `snapshot_ts` differs from its fixture snapshot.
- **Broker exits during cell run:** the pending Promise cannot be hydrated; the invocation fails. There is no durable resume in v0.3.
- **Cell exits after receiving a capsule:** no database commit occurred; query/mutation can be retried by the host. Actions remain future ledger work.

## Production roadmap delta

v0.3 pulls the broker-service work earlier and makes the next milestones sharper:

1. **Harden UDS broker:** peer credentials, socket directory permissions, request IDs, trap sequence numbers, structured metrics, and fuzz tests.
2. **Supervisor and OS sandbox:** launch cells under tenant-specific UIDs, cgroups v2, seccomp, namespaces, read-only rootfs, no ambient network/filesystem, and kill-on-lease/key rotation.
3. **Convex-shaped syscall API:** replace toy `Aster.read(key, field)` with a `ctx.db.get` / `1.0/get` fixture while preserving the same Promise/IPC hydrate path.
4. **Production store adapter:** replace the fixture `MvccStore` in `aster_brokerd` with Convex/Postgres read authority at a fixed timestamp.
5. **Durable actions:** effect ledger before giving cells any external egress.
6. **Scheduler locality:** once process costs are measurable, route by tenant/deployment/module/readset fingerprint.

## Validation facts for v0.3

Recorded logs live under `docs/`:

- `toolchain_check_v0.3.txt`
- `cargo_build_v0.3.txt`
- `cargo_test_v0.3.txt`
- `bench_results_v0.3.json`
- `bench_results_v0.3.stderr`
- `protoc_v0.3.txt`
- `ipc_process_e2e_v0.3.txt`
- `ipc_manual_run_v0.3.txt`

Latest benchmark in this environment:

```json
{"iterations":10000,"keys":32,"warm_query_avg_ns":36466,"cold_trap_query_avg_ns":76169,"mutation_avg_ns":971}
```

The benchmark remains the v0.1/v0.2 in-process host benchmark for comparability; it is not an IPC latency benchmark.

## Why this was the right v0.3 gap

The remaining v0.2 gaps included Convex compatibility, scheduler locality, action ledger, and OS sandboxing. The process-separated broker was the best next target because it directly tests the core security claim: cells should execute tenant code without database read authority or seal-key custody. The new E2E is runnable by a senior engineer and visibly crosses the boundary: two processes, one socket, real V8, sealed capsule verification, hydrate, reseal, resume, and wrong-cell rejection.

Aster is now less like a semantic model and more like an execution plane. It still needs hardening, but the boundary is no longer just a comment.
