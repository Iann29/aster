# Aster Runner v0.2: V8-compatible capsule execution

Status: research prototype with compiling Rust implementation. Scope: self-hosted Convex function execution; v0.2 specifically validates real V8 read-trap continuations and cryptographic capsule seals. Out of scope: production gRPC/UDS services, real Convex remote-runner wire fixture, Linux sandbox supervisor, durable effect ledger, and cluster scheduler.

## Executive summary

Aster v0.1 proposed **Snapshot Capsules + Read-Trap Continuations + Tenant-Pinned Sandbox Cells**. It correctly modeled the transaction and capability shape, but two core claims were not implemented: a real V8 isolate did not suspend/resume on missing reads, and capsule integrity was a 64-bit `DefaultHasher` debug name rather than a cryptographic capability.

Aster v0.2 changes the shape of the research program. The central V8 result is positive but narrower than v0.1 implied: **read-trap continuations are feasible in V8 when the continuation boundary is an async host Promise, not arbitrary stack capture.** The new `aster-v8cell` crate embeds the real Rust `v8` crate. A JS function calls `await Aster.read("counters/b", "value")`; the host callback returns a pending Promise for a missing capsule entry; Rust records a typed trap and its `PromiseResolver`; the host hydrates the capsule; Rust resolves the Promise and explicitly runs V8's microtask checkpoint; the original JS async function resumes and returns the right value. This is an end-to-end test, not a sketch.

Aster v0.2 also replaces v0.1's “hash as security” placeholder with `SealedCapsule`: a canonical BLAKE3 digest plus keyed BLAKE3 MAC bound to cell identity and lease epoch. The seal is still prototype-local, but the property is real: wrong-cell replay and post-seal tampering are rejected by tests.

The conclusion is that Aster should keep the read-trap architecture, but rewrite its language: **Aster does not need serializable V8 continuations. It needs trap-aware async syscalls.** That aligns with Convex's own isolate runner, which already executes UDFs as Promises, drains microtasks, batches pending database syscalls, and resolves `PromiseResolver`s. The production integration work is therefore not “teach V8 to checkpoint stacks”; it is “make Convex's database syscall executor capsule-aware and broker-backed.”

## What changed since v0.1

### 1. V8 moved from modeled to demonstrated

v0.1's `Program` enum was a good semantic model but did not answer whether V8 could actually resume. v0.2 adds `crates/v8cell`, depends on the real `v8` crate, initializes an isolate, installs a minimal `Aster.read` host API, and passes `v8_async_function_resumes_after_read_trap`.

**Rationale:** The whole design rested on this. The result changes the architecture from vague “continuations” to precise “Promise-trap continuations.”

### 2. Capsule integrity moved from debug hash to capability seal

v0.1 used `DefaultHasher` as `root_hash`. v0.2 keeps that field only for legacy benchmark continuity and adds `aster-capsule::seal`: canonical BLAKE3 digest + keyed BLAKE3 MAC + cell/lease binding.

**Rationale:** A bearer capsule must be authenticated. Collision resistance alone is insufficient; the seal must be scoped to who may present it.

### 3. The Convex integration theory shifted from wire cloning to syscall inversion

v0.1 said `aster-adapter` would speak whatever the unmodified backend expects. That remains the production goal, but v0.2's code and repository reading point to a lower-level seam: Convex's isolate runner already has pending async database syscalls and Promise resolvers.

**Rationale:** If Aster can integrate where `1.0/get` and `1.0/queryStreamNext` are executed, it avoids pretending to know Funrun's private server ABI before building a compatibility fixture.

### 4. The threat model now assumes real V8 and real capsule bearer tokens

v0.1 discussed V8 escapes in general. v0.2 has real V8 code and real seals, so replay, resolver confusion, Promise job abuse, and seal-key custody become concrete threats.

### 5. v0.2 adds registers instead of burying speculation

Theories and absurd ideas live in separate files. This keeps the architecture document evaluable while preserving research directions.

## Ground facts from v0.2 validation

Executed in this environment:

```bash
cargo --version
# cargo 1.94.1 (29ea6fb6a 2026-03-24)

rustc --version
# rustc 1.94.1 (e408947bf 2026-03-25)

cargo build --workspace
cargo test --workspace
cargo run --release -p aster-host --bin aster_bench -- 10000 32
protoc --proto_path=proto --descriptor_set_out=/tmp/aster-v0.2.pb proto/aster.proto
```

Results:

- Build: clean.
- Tests: all workspace tests pass, including all v0.1 tests plus V8 and seal e2e tests.
- Benchmark on this machine: `warm_query_avg_ns=37077`, `cold_trap_query_avg_ns=76319`, `mutation_avg_ns=963`.
- Protobuf descriptor: `/tmp/aster-v0.2.pb`, 3,348 bytes.

## System sketch v0.2

```text
             unmodified / minimally adapted Convex backend
                   async UDF syscall / remote-runner seam
                                  |
                                  v
                         +----------------+
                         | aster-adapter  |
                         +----------------+
                                  |
                    sealed capsule request/result
                                  |
             +--------------------+--------------------+
             |                                         |
             v                                         v
      +--------------+                         +----------------+
      | capsule      |  sealed deltas / traps  | V8 sandbox     |
      | broker       |<----------------------->| cells          |
      | read auth    |                         | no DB creds    |
      +--------------+                         +----------------+
             |
             v
      Convex read path / Postgres snapshot

             Convex backend remains the only committer
```

The missing arrow is still the point: V8 cells do not talk to Postgres.

## The V8 question

### Answer

V8 can support Aster-style read traps if Aster uses V8's existing async model. It should not attempt arbitrary stack serialization. The continuation is the suspended async function behind a Promise.

### Experiment

`aster-v8cell` creates a V8 isolate and installs `Aster.read`. For warm capsule entries, `Aster.read` returns the field value. For missing entries, it:

1. creates `v8::PromiseResolver::new(scope)`,
2. returns `resolver.get_promise(scope)` to JS,
3. stores the resolver in Rust alongside `DocumentId` and field,
4. lets the top-level JS `main()` Promise become pending.

The Rust scheduler sets `MicrotasksPolicy::Explicit`, calls `perform_microtask_checkpoint`, observes pending state, pops the typed trap, hydrates the capsule from `MvccStore` at the original timestamp, resolves the stored resolver, checkpoints microtasks again, and reads the fulfilled result.

The passing JS body is:

```js
async function main() {
  const a = await Aster.read("counters/a", "value");
  const b = await Aster.read("counters/b", "value");
  return a + b;
}
```

With only `counters/a` prewarmed, the result is `42` and `traps == 1`.

### V8 internals referenced

The Rust `v8` crate wraps V8 APIs documented in bundled headers:

- `v8-promise.h`: `Promise::Resolver::New`, `GetPromise`, `Resolve`, `Reject`, `Promise::State`, `Promise::Result`.
- `v8-isolate.h`: `PerformMicrotaskCheckpoint`, `SetMicrotasksPolicy`.
- `v8-function-callback.h`: embedder callbacks and `FunctionCallbackInfo::Data`, used to pass host state through an `External`.

These APIs are stable embedder primitives. They do not provide arbitrary call-stack serialization; they provide Promise continuations.

### Convex-specific evidence

A clone of `get-convex/convex-backend` shows the same execution pattern in `crates/isolate/src/environment/udf/mod.rs`: invoke UDF, get a V8 Promise, drain microtasks, pop pending syscalls, execute database syscall batches, resolve stored PromiseResolvers, repeat. `crates/isolate/src/environment/udf/async_syscall.rs` groups reads such as `1.0/get` and `1.0/queryStreamNext` into batches. That strongly suggests Aster's production seam is the database syscall executor.

### Consequence

Aster v0.3 should not chase stack checkpointing. It should build a Convex-version-pinned compatibility fixture around async database syscalls and query journals.

## New theories

### Theory 1: Promise-trap continuations are the V8-compatible form of read traps

**Hypothesis:** The viable continuation boundary is `await` over a host Promise.

**Argument:** v0.2 demonstrates it with real V8, and Convex's own runner already uses pending PromiseResolvers for syscalls.

**Falsification:** A real Convex UDF using production `db.get` cannot be made to route through a trap-aware Promise executor without a backend patch.

**Consequences if true:** The architecture keeps continuations but scopes them to async syscalls. All database APIs exposed to tenant JS must remain async.

**Consequences if false:** Use deterministic replay: abort on missing read, hydrate, restart from entry.

### Theory 2: Capsule seals are authorization, root hashes are observability

**Hypothesis:** Only a keyed, context-bound seal should authorize hydration.

**Argument:** v0.2's seal tests reject wrong-cell replay and tampering. A root hash cannot express caller identity or lease epoch.

**Falsification:** Fuzz canonical encoding and serialized seal verification until a modified capsule verifies.

**Consequences if true:** Broker APIs reject unsealed hydrate requests in production. Root hashes stay in logs/traces only.

**Consequences if false:** Change cryptographic construction, not the need for a seal.

### Theory 3: Syscall inversion beats Funrun wire emulation as the first compatibility target

**Hypothesis:** Aster should first wrap Convex database async syscall execution rather than clone the full private Funrun ABI.

**Argument:** Convex already exposes the scheduler loop Aster needs. The hardest part is preserving query journals/read-write sets, not V8 scheduling.

**Falsification:** A compatibility fixture proves the unmodified backend cannot externalize these syscalls or accept equivalent results.

**Consequences if true:** v0.3 builds a pinned compatibility harness around `1.0/get`, query iteration, write sets, and journals.

**Consequences if false:** Aster needs an upstreamable backend patch or a full remote-runner ABI implementation.

Additional theories are in `docs/THEORY_REGISTER.md`.

## Absurd idea: probabilistic dream capsules

The absurd idea v0.2 takes seriously is that many hot queries can run with **no synchronous database reads at all** because the broker can pre-ship a “dream capsule” predicted from readset history, Bloom filters, and key importance scores. It sounds wrong because databases are supposed to answer reads, not guess them. But Aster's correctness fallback is a trap. A wrong dream capsule is not a wrong result; it is either extra bytes or a missing-read trap.

**Falsification path:** record real `(function, args fingerprint, read set)` traces, compare LFU/last-readset/Bloom/learned predictors, and measure traps avoided per wasted byte. If held-out trap reduction is weak or capsule bloat is unacceptable, abandon it.

More absurd ideas are in `docs/ABSURD_IDEAS.md`.

## Updated threat model

### Assets

- Tenant documents, indexes, query journals, and snapshot contents.
- Capsule seal key and any key-rotation overlap keys.
- Cell identities and lease epochs.
- V8 isolate heap contents, module cache, and pending PromiseResolvers.
- Convex writer lease and committer path.
- Action effect ledger and external credentials.
- Synapse execution-plane configuration.

### Adversaries

- Tenant-supplied JS trying to escape V8 or confuse the host API.
- A compromised cell process with access to capsule bytes and pending resolvers.
- A network/local attacker replaying hydrate requests or sealed capsules.
- A malicious tenant causing resource exhaustion through trap storms or Promise storms.
- A buggy adapter speaking the wrong Convex syscall/wire version.
- A compromised broker seal key.
- A malicious self-hosted operator remains out of scope.

### New attacks introduced by v0.2

1. **Promise resolver confusion:** a cell could try to resolve a pending read with data for a different key. Mitigation: the broker/cell scheduler stores resolver with typed key/field and only resolves after hydrating that exact trap.
2. **Microtask starvation:** tenant JS can create unbounded Promise jobs. Mitigation: explicit microtask checkpoints under isolate time/memory budgets and termination.
3. **Warm value type confusion:** JS may treat `null` for missing field as missing document. Production must encode Convex values precisely, not v0.2's toy value domain.
4. **Seal replay across cells:** mitigated by binding seal MAC to `cell_id` and lease epoch.
5. **Seal key exfiltration:** broker compromise now compromises capsule authenticity. Mitigation: key only in broker, not cells; rotate; audit; consider KMS/TEE later.
6. **Canonicalization bugs:** two encodings of the same capsule could hash differently or, worse, verify incorrectly. Mitigation: one canonical encoder, fuzzing, descriptor-based tests.
7. **Hydrate oracle abuse:** even without DB creds, a compromised cell may spam valid traps for its tenant. Mitigation: trap budgets, range caps, per-cell rate limits, anomaly detection.
8. **Version skew:** Convex syscall names or journals change. Mitigation: pin Convex commit/image and run compatibility fixtures before upgrades.
9. **V8 supply-chain/linking risk:** embedding `v8` adds a large binary dependency. Mitigation: pin crate, track CVEs, rebuild in CI, sandbox cells as if V8 escape will happen.

### Security invariants

- Cells never receive database credentials or seal keys.
- Hydrate requests require a valid seal for caller cell and lease epoch.
- Every hydrated value is read at the original snapshot timestamp.
- Mutations still commit only through Convex's committer/OCC boundary.
- Action effects require durable fences before external IO.

## Updated failure mode catalogue

### V8 promise remains pending with no trap

This indicates a JS Promise waiting on something Aster did not authorize, or a host bug that lost a trap. v0.2 returns `PendingWithoutTrap`. Production should terminate the invocation with a structured error and increment `aster_v8_pending_without_trap_total`.

### Resolver lost after cell crash

The invocation is lost; no commit has happened. Query/mutation attempts can be retried. Action attempts can retry only before an effect ledger entry reaches “executed.”

### Seal verification fails

Reject the hydrate. Classify reason: wrong cell, wrong epoch, digest mismatch, MAC mismatch, unsupported algorithm. Wrong cell/epoch may be stale supervisor state; digest/MAC mismatch is security-relevant.

### Broker seal key rotation during in-flight invocation

Use a short overlap window where broker verifies old+new seals but issues only new seals. Drain old cells, then drop old key. If overlap is not configured, in-flight invocations fail and retry.

### V8 crate unavailable on target

v0.2 built on x86_64 Linux here. Production packaging must verify target support. If a target cannot link V8, Aster can still run the v0.1 model and negative-result/replay experiments, but not the V8 cell backend.

### Convex ABI drift

If a Convex upgrade changes syscall names, query journal encoding, or remote-runner behavior, Aster-enabled deployments may fail. Pin compatibility and roll back to local execution.

### Trap storm

A malicious or cold function can issue many missing reads. Enforce `max_read_traps`, capsule byte caps, and broker rate limits. High trap rate is also a scheduler/prewarm signal.

### Dream capsule bloat

If predictive prewarm over-ships documents, cells waste memory and leak more tenant data into the cell than needed. Enforce capsule max bytes and track wasted prewarm bytes.

## Production roadmap v0.2

### Months 1-2: Convex compatibility fixture

- Pin a `convex-backend` commit.
- Build a harness that executes one real Convex query through an Aster-like syscall executor.
- Capture actual request/result/journal shapes for `db.get`, query iteration, and mutation writes.
- Decide whether unmodified backend is enough or whether a small upstream patch is required.

### Months 2-3: production capsule seals

- Replace ad hoc in-memory canonicalization with protobuf/serde canonical bytes.
- Add key rotation and multi-key verification windows.
- Add fuzz tests for seal verification and capsule decoding.
- Require seals on all hydrate requests outside dev mode.

### Months 3-5: broker service boundary

- Implement broker over UDS or tonic with message caps.
- Read through Convex/Postgres snapshot path at a fixed timestamp.
- Implement point traps, bounded range traps, stale-snapshot errors, and metrics.
- Ensure broker has seal key and DB read authority; cells have neither.

### Months 4-6: V8 cell integration

- Replace toy `Aster.read` with Convex module loading and database syscall interception.
- Keep explicit microtask scheduling and trap budgets.
- Add query journal and read/write-set fidelity tests.
- Add module cache keyed by deployment/module hash.

### Months 5-7: OS cell supervisor

- Launch cells as separate processes with tenant/deployment identity.
- Add cgroups v2, seccomp, namespaces, read-only rootfs, no ambient capabilities, and per-tenant UIDs.
- Add health checks, crash restart, cell reincarnation policy, and kill-on-key-rotation.

### Months 7-9: actions and effect ledger

- Implement durable effect fences.
- Route fetch/HTTP/storage/scheduled-job authority through brokers.
- Add idempotency and ambiguity resolution after crashes.

### Months 9-10: scheduler locality

- Add rendezvous hashing by `(tenant, deployment, module_hash, readset_fingerprint)`.
- Add broker/cell capsule delta caches.
- Compare against round-robin on real traces.

### Months 10-12: Synapse productization and hardening

- Add Synapse feature flags, secrets, audit logs, rollback UI, and runbooks.
- Run chaos tests: broker crash, seal rotation, cell crash, stale snapshots, Convex upgrade mismatch.
- Fuzz V8 host API and broker protocol.
- Build dashboards for traps, seals, cells, broker latency, and commit wait.

## Why a senior engineer might now argue from first principles

v0.2 removes the biggest hand-wave. The system no longer depends on magical V8 stack capture. It uses the same async embedding model that V8, Deno, and Convex already rely on. The price is a clearer constraint: database reads must be async host operations. The payoff is still the original Aster payoff: many V8 cells can execute tenant code without becoming database readers or writers.

The remaining risk is integration, not physics. We still need the Convex compatibility fixture, real process isolation, and production broker. But v0.2 demonstrates a new property end-to-end: a real JS function suspends on a missing capsule read and resumes after host hydration. That makes Aster worth continuing.

## Source notes

- `crates/v8cell/src/lib.rs` for the V8 proof.
- `crates/capsule/src/seal.rs` for capsule seals.
- `docs/V8_QUESTION.md` for a focused memo.
- `docs/THEORY_REGISTER.md`, `docs/ABSURD_IDEAS.md`, `docs/COMPARISON_MATRIX_V0.2.md`, `docs/SYNAPSE_MIGRATION_V0.2.md` for registers and migration details.
- Convex files read: `crates/isolate/src/environment/udf/mod.rs`, `crates/isolate/src/environment/udf/async_syscall.rs`, `crates/common/src/knobs.rs`.
