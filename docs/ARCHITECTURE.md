# Aster Runner: capsule execution for self-hosted Convex

Status: prototype design. License target: Apache-2.0 OR MIT. Scope: Convex queries, mutations, and actions. Out of scope: search, scheduled jobs, production V8 embedding, generated Convex wire adapter, and cluster autoscaling.

## Executive summary

Self-hosted Convex deployments inherit a practical ceiling from the open-source backend's in-process function runner: one deployment backend owns one function-runner process, and that process is bounded by the V8 isolate-worker limit used by the backend. Convex Cloud's answer is Funrun, a proprietary Rust gRPC service that moves execution out of the per-deployment backend, lets execution replicas fetch snapshots, and returns read/write sets so the deployment backend can conflict-check and commit. That is a good design for Convex Cloud, but a direct open-source clone would be the wrong target for this task: it reproduces the same trust shape, the same database-credential distribution problem, and the same “remote runner reads the database” operational model in a smaller self-hosted environment.

Aster Runner proposes a different shape: **Snapshot Capsules + Read-Trap Continuations + Tenant-Pinned Sandbox Cells**.

A Snapshot Capsule is an immutable, hash-addressed, tenant-scoped MVCC bundle. It contains a snapshot timestamp, a tenant ID, a deployment ID, a root hash, and a bounded set of versioned documents. A runner cell receives a capsule instead of database credentials. If a function needs data not present in the capsule, it does not query Postgres. It emits a typed `ReadTrap`, for example “hydrate document `users/123` at snapshot timestamp 981`.” The Capsule Broker validates that the trap is for the same tenant, deployment, and timestamp, reads the missing value using Convex's read path, and returns a capsule delta. The function resumes against the extended capsule. Mutations return read/write sets and go back to Convex's existing single committer. Actions return an `EffectFence` before external IO so the host can enforce idempotency and freshness without blindly retrying side effects.

This is not merely “Funrun, but open source.” Funrun's public description is a read-only remote execution service: the runner loads a snapshot, runs JavaScript in V8, computes results, and sends them to the backend, while the backend performs conflict detection and commit. Aster keeps the important Convex correctness boundary — the deployment backend remains the single writer — but moves database authority out of the runner tier. Runners become capability consumers, not read replicas. The self-hosted operator gets many execution cells per deployment, but does not have to hand every cell database credentials or trust V8 as the only cross-tenant boundary.

The prototype in this repository demonstrates the core novelty without external dependencies. `aster-capsule` implements an in-memory MVCC store, immutable snapshot capsules, read/write sets, read traps, and a single OCC committer. `aster-runner` implements tenant-pinned sandbox cells and the continuation loop. `aster-host` demonstrates an adapter-like facade with query, mutation, and action coverage, E2E tests, and a microbenchmark driver. Production would replace the in-memory store with Convex/Postgres integration, replace `DefaultHasher` with BLAKE3 or SHA-256 plus signatures, and replace the built-in toy programs with the existing Convex isolate executor.

## Ground facts from Convex

The design assumes these Convex facts:

1. Convex has three application function types. Queries read data and are cached/subscribable; mutations are transactional writes; actions can perform side effects and interact with Convex through queries and mutations rather than running as one database transaction.
2. Convex mutations execute transactionally against a consistent view and commit atomically. Convex uses optimistic concurrency control: if a mutation's read dependencies become stale before commit, the transaction is retried or fails with a conflict.
3. Convex's durable writer remains single-writer per deployment through a database lease. In the open-source code, the lease transaction path is the guardrail that prevents concurrent writers from committing for the same deployment.
4. Convex's Cloud Funrun post states that V8 limits a process to 128 isolate threads, that isolate startup is around 10 ms while AWS Lambda cold starts are usually far slower, and that Funrun is a read-only gRPC service that returns results to the deployment backend for conflict checking and commit.
5. The open-source backend already contains a remote-runner client surface and environment knobs. Aster's production compatibility story uses that existing surface as an ABI facade; it does not require a backend fork for the core design.

The prototype does not import Convex code. It models the relevant semantics in small, auditable types so the architecture can be reviewed independently of Convex's large codebase.

## Explicit novelty claim

Aster's novelty is **capability-narrowed snapshot execution**.

A normal remote runner architecture asks, “How do I place more JS isolate workers near a database snapshot?” It then gives those workers a way to obtain snapshot data. Funrun's documented answer is sensible: create a read-only runner service, let it pull snapshots, and keep the committer in the backend. That solves the 128-thread bottleneck but leaves a broad read authority in the execution tier. In a self-hosted multi-tenant setting, that broad authority matters because the operator might run many unrelated tenants on one VPS or one small cluster. If a malicious tenant finds a V8 escape in a runner process, the blast radius depends heavily on what that process can do after escape.

Aster asks a different question: “Can a runner execute useful work without ever becoming a database reader?” The answer is yes if the runner is treated as a resumable continuation over an immutable data capability. The function starts with a small capsule. Missing reads are not failures; they are typed continuation traps. The broker, not the runner, is the database reader. The runner can only resume with data that is consistent with the original snapshot timestamp. That means Aster can add many runner cells while keeping database credentials, MVCC-retention policy, and read authorization concentrated in one broker tier.

The difference matters in the self-hosted OSS context for four reasons.

First, small operators usually do not have a separate security team. They need a design whose safe default is conservative. A tenant-pinned cell with no DB credentials, no ambient network, a read-only rootfs, and cgroup limits is easier to reason about than a general read-only database client embedded inside a multi-tenant runner.

Second, density on a single VPS matters. Firecracker-like microVM isolation is excellent for hostile multi-tenant compute, but using one microVM per Convex function invocation would throw away the fast-start property Convex explicitly values. Aster's unit is not per-invocation VM. It is a long-lived **cell** pinned to a tenant/deployment. Within the cell, V8 isolates can be reused. The cell still has an OS sandbox boundary around it, but the common case does not pay a microVM cold start per request.

Third, Convex's committer is already the serialized truth. The design should not fight that. Aster scales only the part that can safely scale: JavaScript execution and snapshot reads for queries, mutation attempt execution, and action orchestration. Mutation commits still bottleneck at the deployment's single writer, which is a feature, not a bug. The system should make this visible in metrics rather than hide it behind a fake “horizontal mutation scale” claim.

Fourth, the architecture can degrade gracefully. If prewarming is poor, functions issue more read traps and run slower, but they remain correct. If cells crash, the backend can retry queries and mutations before commit. If the broker rejects a stale snapshot, the invocation restarts at a fresh timestamp. If the remote-runner facade is disabled, an operator can set the backend back to in-process execution and accept the old 128-worker ceiling while preserving data correctness.

## System sketch

```text
                 unmodified convex-backend
                 remote runner client ABI
                            |
                            v
                    +----------------+
                    | aster-adapter  |
                    +----------------+
                      | capsule req  | result/read-write-set
                      v              |
              +----------------+     |
              | capsule broker|<----+
              +----------------+
                ^          | signed capsule deltas
 read-only MVCC|          v
      snapshot |   +-----------------------+
                +---| tenant-pinned cells  |
                    | V8 isolates, no DB   |
                    | creds, read traps    |
                    +-----------------------+
                              |
                              v
                    action egress broker
                    effect fences only
```

The arrow that is intentionally missing is the important one: tenant cells do not talk to the database.

## Components

### 1. Convex backend adapter

The adapter is the only component that speaks to unmodified `convex-backend`. In production it implements the public remote-runner ABI expected by the OSS backend knobs. It receives a Convex invocation request, extracts tenant/deployment/function identity, asks the Capsule Broker for an initial capsule, chooses a cell, and translates the Aster result back into the response shape expected by Convex's committer.

The adapter is deliberately boring. It should contain no database commit logic, no custom transaction protocol, and no side-effect execution beyond action fences. Its job is translation and policy enforcement. It can run as a sidecar on a single VPS or as a small replicated service behind a local load balancer.

The prototype equivalent is `aster-host`. It chooses a cell, calls `SandboxCell::execute`, commits mutation write sets through `MvccStore::commit`, and records action effect fences.

### 2. Capsule Broker

The broker is the only Aster service with read authority. It can read Convex's MVCC snapshot through the existing backend/read-pool path or through a read-only database account that is scoped to one self-hosted installation. It is not a writer and does not need the deployment writer lease. Its responsibilities are:

- Build initial capsules from prewarm hints, cached read-set history, or empty cold-start policy.
- Hydrate read traps at the original snapshot timestamp.
- Reject stale traps when MVCC retention no longer covers the requested timestamp.
- Sign or MAC capsule roots so runner cells cannot forge cross-tenant capsules.
- Enforce per-tenant read budgets and range limits.
- Emit observability events for capsule size, trap count, hydration latency, and stale-snapshot failures.

A broker may be stateless if it can rebuild capsules from the database, or it may maintain a memory-mapped capsule cache keyed by `(tenant, deployment, snapshot_ts, root_hash)`. The first production version should be stateless plus an LRU capsule delta cache. Statelessness keeps crash recovery simple and makes it possible to run the broker on the same VPS as the backend.

### 3. Tenant-pinned sandbox cells

A cell is a long-lived execution process pinned to one tenant and deployment. It contains a pool of V8 isolates, module cache, and memory budget. It has no database credentials. It has no direct outbound network unless it is an action cell with egress routed through the Action Egress Broker. Its root filesystem is read-only; it runs under a tenant-specific UID; it is constrained by cgroups; and its syscall surface is reduced by seccomp-bpf or a stronger optional sandbox such as gVisor for hostile tenants.

The cell boundary is intentionally coarser than “one process per invocation.” A single tenant/deployment may receive multiple cells when load exceeds one process's isolate limit. For example, a single VPS can run eight cells for a hot deployment, each with up to 128 V8 isolate workers. That yields roughly 1024 execution slots while preserving the deployment backend's single writer. The cells are disposable. A crash loses module cache and in-flight queries/mutations, not database correctness.

The prototype equivalent is `SandboxCell`. It verifies the tenant and deployment before executing, starts from an immutable capsule, emits `ReadTrap` values for missing data, and resumes until completion or a trap budget is exceeded.

### 4. Read-trap continuation loop

Aster's central mechanism is the read-trap loop:

1. The adapter asks the broker for an initial capsule at timestamp `T`.
2. The cell starts the function with that capsule.
3. When the function reads a missing key or range, the cell returns `ReadTrap::Point(key)` or `ReadTrap::Prefix(prefix, limit)`.
4. The broker validates `(tenant, deployment, T, root_hash, trap)`, reads the missing values at `T`, and returns a capsule delta.
5. The cell extends the capsule, recomputes the root hash, and resumes the function.
6. On completion, the cell returns result bytes, read set, write set, effect fence if any, final capsule hash, and trap count.

A trap is not a database query controlled by tenant code. It is a constrained continuation request. The broker can deny it, cap it, audit it, or route it differently. The continuation loop also creates a natural prewarming feedback loop: if a function repeatedly traps on the same keys or ranges, the adapter learns to include those keys in the next initial capsule.

### 5. Committer and OCC boundary

Aster does not introduce a new mutation committer. In production, mutations return read/write sets to the unmodified Convex backend. The existing committer and database lease remain the only path to durable writes. This is non-negotiable: two writers committing concurrently must remain impossible.

The prototype includes an in-memory `MvccStore::commit` only to demonstrate the invariant. It serializes commits through one mutex and validates every observed read version against the live version. Two mutation executions over the same snapshot can both produce write sets, but only one can commit if they read the same document and one changes it. The E2E test `two_mutation_results_over_same_snapshot_cannot_both_commit` demonstrates this.

### 6. Action effect fences

Actions are not mutations. They can perform side effects, and once an external effect has happened, automatic retries can duplicate work. Aster handles this by splitting action execution into two phases:

- **Plan phase:** run tenant JS in a cell against a capsule. The result is either normal data or an `EffectFence` containing effect kind, idempotency key, capsule hash, and read observations.
- **Effect phase:** the host egress broker verifies the fence, checks idempotency, checks that the capsule is fresh enough under tenant policy, performs the external call, records the outcome, and returns it to the action continuation if needed.

The prototype demonstrates only the fence. It intentionally does not perform network IO. Production support for actions would also need a durable effect ledger so a crashed adapter can determine whether an effect was performed before retrying the continuation.

## Compatibility with unmodified `convex-backend`

Aster's production integration point is the existing remote-runner client surface in the OSS backend. The outside-facing adapter speaks the wire format that the unmodified backend already expects. Internally, the adapter immediately converts the request into Aster's open capsule protocol. This is analogous to implementing an existing interface with a different internal architecture. The backend does not need to know that the runner is not Funrun.

No backend patch is required for the core path if the remote-runner knob is available and the client protocol remains public. The degraded fallback is the standard in-process runner: disable the remote-runner environment knob and restart the deployment backend. In that fallback, correctness is unchanged but the deployment returns to the 128-worker ceiling.

A production Aster distribution should include a compatibility test fixture compiled against a pinned `convex-backend` commit. The fixture should boot the backend with the remote-runner endpoint pointed at `aster-adapter`, execute one query, one mutation, and one action, and assert that Convex's own committer receives the expected read/write/effect outputs. This prototype does not include that generated adapter because the execution environment used for this artifact lacked the Rust toolchain and the closed Funrun server is not public. The internal Aster protocol is still real and intentionally separate from the compatibility ABI.

One small optional upstream patch could improve prewarming but is not required. It would pass a stable function cache key and a previous read-set hint to the remote runner request. Without that patch, Aster begins with empty or heuristic capsules and uses read traps. With it, the adapter can reduce traps for hot functions. Because the design remains correct without the hint, this document does not propose a mandatory diff.

## Correctness model

### Queries

A query runs against one snapshot timestamp. Its capsule may be extended by traps, but every hydrated value is read at the same timestamp. The result includes a read journal and final capsule hash. If a snapshot becomes unavailable before all traps resolve, the broker rejects the invocation and the adapter asks the backend to rerun the query at a newer timestamp. Query results are cacheable only under the same conditions Convex already uses; Aster does not invent a new query cache coherency model.

### Mutations

A mutation executes optimistically. It reads from a consistent snapshot, produces a write set, and returns to the backend committer. The committer checks whether the read set is still valid. If not, Convex's existing retry/conflict behavior applies. Aster must not make commits directly to the deployment database. Even if a future version runs the broker near Postgres, it remains read-only.

A mutation that reads document absence must include the absence observation in the read set. The prototype models this with `VersionedDocument { version: None, document: None }` and `ReadSet::observe(key, None)`. If a later transaction creates the document before commit, the second mutation conflicts.

### Actions

Actions are orchestrations, not database transactions. Aster lets an action read capsule data for planning and lets it call nested queries/mutations through the normal Convex API path. External IO is mediated by effect fences. The key rule is: never automatically replay an action segment after an effect unless the idempotency ledger proves the replay is safe.

### Snapshot consistency

The capsule root hash covers tenant, deployment, timestamp, and the ordered document map. Production roots should use a collision-resistant hash and be signed or MACed by the broker. Runner cells should treat capsules as opaque capabilities. They can ask for extensions through traps; they cannot mint a capsule for another tenant or timestamp.

### Single writer

The deployment writer lease remains the commit authority. Aster may scale from one cell to many cells, but it does not scale the number of durable writers. For write-heavy workloads, the committer may become the bottleneck. That bottleneck is correct and should be surfaced through metrics such as mutation queue time, conflict rate, and commit latency.

## Threat model

### Assets

- Tenant data in Convex documents, indexes, and snapshots.
- Deployment writer lease and commit path.
- Capsule signing keys and broker credentials.
- Action egress credentials and webhook/API secrets.
- Module cache contents and compiled JS artifacts.
- Operator control plane state, especially Synapse tenant/deployment mapping.

### Adversaries

- A tenant who can upload arbitrary Convex JavaScript and intentionally tries to escape the sandbox.
- A tenant who writes expensive functions to exhaust CPU, memory, traps, or file descriptors.
- A network attacker who can delay, replay, or drop messages between adapter, broker, and cells.
- A compromised runner cell process.
- A buggy or stale adapter version speaking an incompatible backend ABI.
- A malicious operator is out of scope; self-hosted systems ultimately trust the operator with the database.

### Security goals

1. A compromised tenant cell must not read another tenant's data.
2. A compromised tenant cell must not commit writes directly.
3. A compromised tenant cell must not perform arbitrary network side effects.
4. A malformed capsule or replayed hydrate response must be rejected.
5. Resource exhaustion in one tenant should not take down unrelated tenants.
6. A broker crash should not corrupt data or require manual recovery.

### Defense in depth

**V8 is not the only boundary.** V8 isolates are fast and useful, but V8 has had sandbox escape vulnerabilities. Aster assumes tenant JS may eventually gain native code execution inside a cell. The question is what that code can do next.

**No DB credentials in cells.** This is the primary defense. A cell escape does not yield Postgres credentials or Convex writer authority. The attacker can only access capsule bytes already delivered to that tenant/deployment, plus whatever narrowly scoped hydrate endpoint the broker still accepts for that cell identity.

**Tenant-pinned cell identity.** Cells are pinned to one tenant and deployment. Cross-tenant isolate reuse is disabled by default. Higher density modes may allow same-tenant multi-deployment cells, but never unrelated tenants in the same OS process unless the operator explicitly accepts the risk.

**mTLS and signed capsules.** Adapter, broker, and cells use mutually authenticated local TLS or Unix-domain sockets with peer credentials on a single host. Capsules include tenant/deployment/timestamp/root hash and are signed or MACed by the broker. Hydrate requests must present the previous root hash and cell identity.

**OS sandboxing.** The production cell supervisor launches cells with Linux namespaces, cgroups v2, seccomp-bpf, `no_new_privs`, read-only root, a private `/tmp`, no ambient capabilities, RLIMITs, and per-tenant UIDs. For hostile public multi-tenant hosting, a pluggable gVisor/Kata backend can trade performance for stronger syscall isolation. Aster does not require Firecracker per invocation, but it does not forbid microVM-backed cells for high-risk tenants.

**Network egress broker.** Query and mutation cells have no direct outbound network. Action cells can request effects through an egress broker that enforces allowlists, rate limits, secret access policy, and idempotency. The egress broker records every effect fence and outcome.

**Trap budgets and range caps.** A malicious function can try to enumerate data through repeated traps. The broker caps traps per invocation, range limits, bytes per capsule, and time per snapshot. Exceeding a budget fails the function with a typed error visible to the operator.

**Cache hygiene.** Module caches are tenant/deployment keyed. When a deployment is deleted or tenant keys rotate, the supervisor kills the relevant cells rather than attempting partial cache scrubbing.

## Failure-mode catalog

### Runner crash

Queries and mutations that crash before returning are safe to retry because no durable write or external effect has happened. The adapter records the attempt, increments `aster_runner_cell_restarts_total`, and dispatches to another cell if capacity exists. Actions are safe to retry only before an effect fence is marked as executed. After that point, the effect ledger decides whether replay is idempotent.

### Broker crash

The broker is read-only and should be restartable. In-flight hydrations fail; cells return transient errors; the adapter retries the invocation if the function kind permits. If the broker has an LRU capsule cache, losing it increases traps and read latency but not correctness risk.

### Adapter crash

Queries and mutations may be retried by the backend. Mutation results not yet delivered to the committer are lost, not partially committed. Action fences must be durably recorded before external IO. If the adapter crashes after recording a fence but before receiving the external response, the effect ledger's state machine resolves the ambiguity on restart.

### Network partition

On a single VPS this is usually a process/socket failure. In a small cluster it can split adapter, broker, and cells. Broker-to-cell partitions fail invocations quickly. Backend-to-adapter partitions surface as remote-runner failures. The operator can disable the remote-runner knob to fall back to in-process execution. Aster should not silently commit or perform side effects during uncertain partitions.

### Snapshot stale

If MVCC retention no longer covers timestamp `T`, the broker rejects a trap with `stale_snapshot=true`. The adapter asks Convex to rerun queries/mutations at a fresh timestamp. For actions, stale planning snapshots fail before effect execution unless the action explicitly tolerates stale data.

### Committer slow

Aster cannot and should not bypass Convex's single committer. If mutation commit latency rises, the adapter applies backpressure to mutation execution so cells do not produce an unbounded queue of doomed write sets. Queries and action planning can continue if read pool capacity exists. Metrics distinguish execution saturation from commit saturation.

### Read pool slow

The broker can reduce initial capsule size, prefer hot cached deltas, and surface `capsule_hydrate_seconds` and `read_trap_queue_depth`. Poor prewarming increases trap count but preserves consistency. If read latency is systemic, adding cells will not help; the operator should add read capacity or tune query patterns.

### Malicious or buggy function loops

Cells enforce CPU time, memory, and trap budgets. V8 termination handles JS loops; cgroups handle native runaway after an escape. The supervisor kills cells that exceed budgets and reports tenant/deployment labels to Synapse.

### Capsule forgery or replay

The broker signs capsules with short TTLs and includes tenant, deployment, snapshot timestamp, lease epoch, and root hash. Hydrate responses include the prior root and new root. Replays across tenants or timestamps fail signature validation. Replays within TTL for the same capsule are harmless because capsule contents are immutable.

## Observability surface

Aster should ship with metrics, traces, logs, and audit events from day one. Operators need to know whether they are limited by V8 workers, broker reads, read traps, or Convex commits.

### Metrics

- `aster_invocations_total{tenant,deployment,kind,status}`
- `aster_invocation_duration_seconds{kind}`
- `aster_cell_queue_depth{tenant,deployment,cell}`
- `aster_cell_active_isolates{tenant,deployment,cell}`
- `aster_cell_restarts_total{tenant,deployment,reason}`
- `aster_capsule_initial_bytes{tenant,deployment}`
- `aster_capsule_final_bytes{tenant,deployment}`
- `aster_read_traps_total{tenant,deployment,kind}`
- `aster_traps_per_invocation{tenant,deployment,kind}`
- `aster_capsule_hydrate_seconds{tenant,deployment}`
- `aster_stale_snapshot_total{tenant,deployment}`
- `aster_occ_conflicts_total{tenant,deployment,function}`
- `aster_commit_wait_seconds{tenant,deployment}`
- `aster_effect_fences_total{tenant,deployment,effect_kind,status}`
- `aster_egress_denied_total{tenant,deployment,reason}`

Tenant labels may be hashed or redacted depending on operator policy.

### Traces

One invocation trace should contain spans for backend adapter receive, initial capsule build, cell queue wait, cell execution, each hydrate trap, result translation, commit wait, and action effect fence handling. The trace should include module hash, function kind, capsule root hash, snapshot timestamp, trap count, and result status. It should not include document values by default.

### Logs and audit events

Structured logs should capture cell lifecycle, trap budget failures, stale snapshot failures, ABI translation errors, and egress denials. Audit events are separate from debug logs and should be durable: capsule signature failures, cross-tenant hydrate attempts, effect fence execution, secret access, and operator rollback.

## Migration path for Convex Synapse operators

Synapse is an open-source control plane for self-hosted Convex deployments. It already owns concepts such as teams, projects, deployments, provisioning, custom domains, RBAC, backups, and audit logs. Aster should integrate there rather than invent another control plane.

A practical migration path is:

1. Install `aster-adapter`, `aster-broker`, and `aster-cell-supervisor` on the same VPS or cluster as Synapse and the Convex backend.
2. Synapse mints a tenant ID and deployment ID for each Convex deployment and writes an Aster execution-plane record: `execution_plane = local | aster`, cell count, risk class, trap budget, capsule byte budget, and remote-runner endpoint.
3. Start Aster in observe-only mode. It watches deployment metadata and warms module/capsule caches but does not receive production traffic.
4. Enable the remote-runner environment knobs for one staging deployment, pointing the backend at `aster-adapter`.
5. Run smoke tests: one query, one mutation, one action with an idempotency key. Confirm Convex commit behavior and Synapse audit logs.
6. Increase cell count gradually. For a single VPS, start with two cells per hot deployment and scale to eight only when CPU and memory permit. For a small cluster, place cells by tenant/deployment affinity and keep the broker close to the database read path.
7. Watch four dashboards: cell saturation, read-trap rate, stale-snapshot rate, and commit wait time.
8. Roll back by setting `execution_plane=local`, disabling the remote-runner knob, and restarting the affected backend. No database migration is needed because Convex remained the writer throughout.

Synapse can expose this as a per-deployment feature flag. The operator should never need to manually repair data after common failures because Aster does not own durable writes.

## Benchmarks and theoretical comparison

The Rust benchmark binary is `cargo run --release -p aster-host --bin aster_bench -- 10000 32`. It measures three paths:

- Warm query: initial capsule contains all 32 documents; zero traps.
- Cold trap query: initial capsule is empty; each document read emits a trap.
- Mutation: one counter read, one write, OCC commit.

Because the artifact environment had no Rust compiler, the recorded numbers come from `scripts/mirror_bench.py`, a Python mirror of the same algorithm. On this container, 5000 iterations with 32 keys produced:

- Warm query: average 13,090 ns, p50 12,492 ns, p95 16,540 ns.
- Cold trap query: average 56,476 ns, p50 53,631 ns, p95 73,086 ns.
- Mutation: average 2,354 ns, p50 1,978 ns, p95 3,836 ns.

These are not production latency claims. They are useful because they show the intended cost shape: a read trap is a measurable continuation boundary, and prewarming matters. In production, each trap becomes an IPC or RPC round trip plus a database/cache read. The architecture is therefore designed to learn prewarm sets aggressively. A hot function should usually run with zero or one trap.

Against the in-process baseline, Aster's execution concurrency scales with cell count. One process remains limited by 128 V8 worker threads. Eight cells on one VPS produce an execution-slot budget near `8 * 128 = 1024` before CPU and memory become the real limits. This does not multiply mutation commit throughput by eight because the committer is still single-threaded per deployment. It does multiply query execution and action planning capacity and reduces queueing for long-running actions.

Against a Funrun-style replica, Aster has similar horizontal execution capacity but a different security and operational profile. Funrun-style replicas are read-only execution services that load snapshots themselves. Aster cells do not read the database; the broker does. That adds a continuation protocol and can add trap latency on cold capsules. In exchange, it removes database credentials from the high-risk code-execution tier, centralizes MVCC retention policy, and gives self-hosted operators a clearer blast-radius story.

## Production roadmap

A realistic 6-12 month implementation by one or two senior Rust engineers can be staged as follows.

### Month 1-2: compatibility and harness

- Pin a `convex-backend` commit and generate the remote-runner client ABI.
- Implement `aster-adapter` that accepts backend calls and returns mocked successful query/mutation/action responses.
- Build a docker-compose harness with Postgres, Convex backend, Synapse, and Aster services.
- Add compatibility tests for one query, mutation, and action.

### Month 2-4: broker and capsules

- Implement capsule root hashing with BLAKE3 or SHA-256.
- Add signed capsule references and mTLS between adapter, broker, and cells.
- Connect broker reads to Convex snapshot/read pool.
- Implement point-read traps, then bounded range traps.
- Add trap budgets, capsule byte budgets, and stale-snapshot rejection.

### Month 4-6: cell supervisor

- Move cells into separate OS processes.
- Add tenant/deployment pinning, cgroups v2, seccomp profiles, read-only rootfs, no ambient caps, and per-tenant UID isolation.
- Embed the real Convex isolate executor or call it through the existing crates.
- Add module cache keyed by deployment/module hash.
- Add crash recovery and cell health checks.

### Month 6-8: actions and egress

- Implement effect fence ledger.
- Add action egress broker with allowlists and idempotency keys.
- Add secret access policy and audit logs.
- Add action continuation semantics for multi-step actions.

### Month 8-10: observability and Synapse integration

- Export Prometheus metrics and OpenTelemetry traces.
- Add Synapse execution-plane configuration and rollback controls.
- Build dashboards for cell saturation, trap rate, stale snapshots, and commit wait.
- Add operator documentation for single VPS and small clusters.

### Month 10-12: hardening

- Fuzz capsule parsing and signature validation.
- Run chaos tests for cell crashes, broker restarts, partitions, and stale snapshots.
- Add load tests against real Convex workloads.
- Document threat-model limits and high-risk tenant settings.

## Why a senior engineer might ship it

Aster avoids the two easiest but unsatisfying answers: cloning Funrun or wrapping every invocation in a heavyweight sandbox. It keeps Convex's proven transactional boundary, scales the V8 execution pool with ordinary Linux processes, and changes the authority distribution in a way that directly helps self-hosted multi-tenancy. Its failure modes are understandable: more traps mean slower functions, slow commits mean mutation backpressure, stale snapshots mean retries, and cell crashes mean lost execution attempts rather than lost data.

The design is not free. It adds a broker, a capsule protocol, and a prewarming problem. Cold workloads can pay extra trap round trips. A production adapter must track Convex's remote-runner ABI carefully. But these costs buy a materially different trust model: malicious tenant code can compromise a cell and still not obtain database credentials or another tenant's snapshot. In a self-hosted OSS system, that is a trade worth exploring.

## Source notes

- Convex Funrun architecture: https://stack.convex.dev/horizontally-scaling-functions
- Convex OSS backend: https://github.com/get-convex/convex-backend
- Convex function docs: https://docs.convex.dev/functions
- Convex mutation docs: https://docs.convex.dev/functions/mutation-functions
- Convex action docs: https://docs.convex.dev/functions/actions
- Convex OCC/conflict docs: https://docs.convex.dev/error#1-conflicting-changes-from-parallel-mutation-executions
- Synapse project: https://github.com/Iann29/convex-synapse
- Cloudflare Workers isolate model: https://developers.cloudflare.com/workers/reference/how-workers-works/
- Firecracker paper/repo: https://github.com/firecracker-microvm/firecracker
- gVisor security model: https://gvisor.dev/docs/architecture_guide/security/
