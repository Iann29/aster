# Aster v0.8 architecture

Status: operational research system with a hardened Docker Compose deployment profile. Aster proves and implements the executor/authority split for the supported Convex query and mutation subset. It has not received an independent production security audit.

Archived design snapshots remain in `ARCHITECTURE_V0.{1,2,3}.md`. This document describes the current mainline.

## Trust boundary

```text
untrusted                                           trusted

Convex tenant bundle                               operator / control plane
        |                                                     |
        v                                                     |
+------------------------+        UDS        +----------------v-------+
| aster_v8cell           |------------------>| aster_brokerd          |
| one invocation/process |                   | policy + session table |
| V8 isolate             |<------------------| capsule seal authority |
| no database secret     | sealed capsules   | launch-token verifier  |
| no module mount        | commit outcomes   | lease + commit fence   |
| no network             |                   +-----------+------------+
+------------------------+                               |
                                                         | SQL
                                             +-----------v------------+
                                             | Postgres               |
                                             | aster transaction data |
                                             | Convex module metadata |
                                             +------------------------+
```

The cell is untrusted. The broker, Postgres, operator, container runtime, host kernel, policy, and module-ingestion pipeline are trusted. A V8 escape yields native execution inside the cell container, but the shipped profile still gives that process no database URL, no network interface, no module directory, no seal/launch keys, a read-only root filesystem, and only the broker UDS.

This claim does not cover a Docker/container-runtime or kernel escape.

## Production topology

`docker/compose.production.yml` runs three roles:

- `brokerd`: long-lived authority attached to the external database network;
- `token-issuer`: isolated one-shot helper used only to issue a short-lived launch token;
- `v8cell`: isolated one-shot tenant execution process.

The operator chooses a host UID/GID. All roles use it so a `0700` host runtime directory can hold the socket while excluding other host users. Runtime restrictions include:

- non-root user;
- read-only root filesystem;
- all Linux capabilities dropped;
- `no-new-privileges`;
- default Docker seccomp;
- PID, CPU, memory, file-descriptor, heap, trap, and wall-clock limits;
- bounded `noexec,nosuid,nodev` tmpfs;
- no network namespace for token issuer and cell;
- database network only on `brokerd`.

The broker receives secrets as read-only bind-mounted files and rejects key/database-url files with group or other permission bits. The runtime directory is writable only by the broker and read-only in cells.

The current Postgres client uses `NoTls`. The production profile therefore assumes Postgres is on the same trusted host/private Docker network or behind a separately secured transport. Do not expose this database connection over an untrusted network.

## Authority startup

For `ASTER_STORE=postgres`, broker startup is fail-closed:

1. Load and validate deployment policy.
2. Read the database URL, seal key, and launch key from restricted files.
3. Connect the module source to the configured Convex metadata schema.
4. Create/connect the `WritePlane` and idempotently ensure the `aster` schema.
5. Acquire the tenant/deployment lease. Each acquisition strictly increments its epoch.
6. Repair an impossible retention floor above the log tip down to the tip.
7. Publish the authority epoch atomically to `ASTER_AUTHORITY_EPOCH_FILE`.
8. Bind the UDS and admit only peers whose Linux `SO_PEERCRED` UID equals the configured runtime UID (the broker's own effective UID by default).

The epoch file is a supervisor-facing rendezvous value, not a cell credential. The launch key authenticates the epoch and the rest of the launch claim.

## Launch and session identity

`aster-invoke` performs this sequence for every invocation:

1. Wait for healthy `brokerd`.
2. Read the broker-published authority epoch.
3. Run `aster_launch_token` in a networkless, capability-free helper container.
4. Issue a token bound to `(cell_id, tenant, deployment, lease_epoch, expiry, random nonce)`.
5. Start one cell with that token and an outer supervisor timeout.

The broker verifies and consumes the token exactly once. It then mints a random session ID and records an immutable `SealContext` in its live session table. Every capsule request carries the session ID; the broker reconstructs the trusted context from its table and treats the request's serialized context only as an equality check.

One session intentionally spans many one-request UDS connections. Session slots are bounded by policy and expire monotonically; `Commit` and `Abort` consume the entry atomically before any fence work, while an abandoned session is reclaimed at TTL. Concurrent commit replays therefore cannot double-spend one bearer session.

## Capsule and transaction model

A `SnapshotCapsule` contains:

- tenant and deployment;
- authoritative snapshot timestamp;
- point-read entries, including authenticated absence;
- certified prefix ranges;
- canonical serialization inputs.

Seal v3 computes a BLAKE3 keyed MAC over the full canonical capsule bytes plus the bound cell ID, lease epoch, and random session binding. Verification uses constant-time equality. The capsule digest is retained for audit/tooling; it is not the MAC input.

### Reads

The invocation starts at snapshot `s`, normally the current `aster.log` tip. On a missing point read:

1. V8 parks the Promise and emits a typed trap.
2. The host sends the current sealed capsule plus key to the broker.
3. The broker verifies session, policy, namespace, snapshot mode, and capsule MAC.
4. `AuthoritativeCapsuleStore` reads the key from the tenant/deployment `aster.log` history at `s`.
5. The broker checks retention after the read, hydrates presence or absence, reseals, and returns the capsule.
6. The cell resolves the Promise and records the read only when application code consumes it.

Certified prefix reads carry a `RangeCertificate` with interval, bytewise order, limit, returned keys, and `Boundary` or `Exhausted` stop reason. Postgres pins bytewise ordering with `COLLATE "C"` so Rust and SQL protect the same interval.

### In-cell mutation view

The cell implements these Convex syscalls:

- `1.0/get`;
- `1.0/insert`;
- `1.0/shallowMerge` (`db.patch`);
- `1.0/replace`;
- `1.0/remove` (`db.delete`).

The write set is a deterministic `BTreeMap`. Reads consult the pending write first, then the sealed snapshot. Capsule-served reads and absences enter the consumption ledger; read-your-own-writes do not create false historical dependencies.

For `db.insert` without `_id`, the cell requests `MintDocumentId`. Policy must grant the table. In Postgres mode the broker resolves the active Convex table number, draws a random 128-bit internal ID from the OS, and returns a strict canonical IDv6. Minting alone grants no write: the ID reaches history only through commit.

### Commit fence

A mutation submits:

```text
(sealed final capsule, consumed point reads, certified ranges, write set)
```

`WritePlane::commit` performs one Postgres transaction:

1. lock the tenant/deployment lease row `FOR UPDATE`;
2. reject a stale epoch;
3. read the authoritative log tip and retention floor while the lease is held;
4. reject a snapshot beyond the horizon or below retention;
5. scan writes since snapshot `s`;
6. test them against every consumed point key and certified conflict interval;
7. append the whole write set at one new timestamp;
8. commit.

Lease acquisition uses the same row lock, so failover cannot cross an in-flight fence. Reads, snapshot selection, conflict validation, retention, and append all use the same `aster.log` history. This shared history is the implementation premise that v0.7 lacked.

Conflict and retention outcomes are retryable only when the invocation requested the latest snapshot (`ASTER_SNAPSHOT_TS=0`) and the configured attempt budget remains. A retry obtains a fresh capsule and reruns the function. Stale epoch and invalid-horizon outcomes are not silently retried.

## Postgres data split

Aster intentionally uses two Postgres surfaces:

### Authoritative transaction data

Owned by Aster:

- `aster.lease` — one strictly increasing authority epoch per tenant/deployment;
- `aster.log` — versioned document writes, keyed by tenant/deployment/timestamp/key;
- `aster.retention` — monotonic low-watermark state.

All query/mutation document reads and commit validation use this history.

### Convex deployment inputs

Read through `PostgresCapsuleStore`:

- `persistence_globals` and `_tables` mapping data;
- `_modules` and `_source_packages` rows;
- hash-verified bundle blobs under the read-only module directory.

This surface supplies module code and table-number metadata. It is not a second transaction history. Bundle storage keys reject absolute paths, traversal, aliases, and non-canonical components; reads are capped before allocation and SHA-256 verified.

## IPC protocol

`crates/ipc` uses a length-prefixed JSON protocol over one UDS connection per cell request. Frames are capped before allocation. The request verbs are:

- `InitialCapsule`;
- `HydratePoint`;
- `HydratePrefix`;
- `MintDocumentId`;
- `LoadModuleBundle`;
- `Commit`;
- `Abort`;
- `Shutdown` (memory-test mode only; rejected by production brokers).

Every accepted connection is authenticated with `SO_PEERCRED`, given read/write deadlines, and dispatched under a bounded live-connection budget. A silent peer occupies one slot until its deadline instead of head-of-line blocking the deployment; overload returns `broker_busy`. Structured wire errors carry stable codes for policy denial, launch rejection, non-canonical IDs, stale snapshots, frame overflow, and peer rejection.

The protocol is deliberately not a general RPC framework. Cells cannot submit SQL, paths, credentials, arbitrary broker methods, or caller-selected routing namespaces.

## V8 execution

`V8SandboxCell` creates one isolate per cell process and compiles the selected bundle source as ESM. The module must export Convex's registration object and the invocation kind must match the export marker. Query exports reject write syscalls with catchable JavaScript errors; mutation exports receive the in-cell write view.

The scheduler uses explicit microtask checkpoints. Missing reads and document-ID requests become typed pending traps; the host services each trap and resolves the exact Promise before continuing. Trap counting bounds both broker round trips and warm redispatch loops.

Resource control has two layers:

- V8 `CreateParams::heap_limits` caps the isolate heap;
- a host watchdog owns a thread-safe isolate handle and calls `terminate_execution` at the deadline.

The watchdog covers synchronous infinite loops as well as async execution. The container supervisor adds a harder outer deadline and kills the one-shot container if process-level cleanup stalls.

## Policy

`DeploymentPolicy` is strict JSON with unknown-field rejection. It validates:

- readable document prefixes;
- writable document prefixes;
- loadable module prefixes;
- insertable table names;
- maximum reads, writes, scan limit, concurrent sessions, and session TTL.

Empty grant lists deny all access and are the generated production default. `"*"` is an explicit deployment-wide grant; it is never inferred from a missing field. Read, write, module, and insert authority are independent. Policy is checked at the broker seam, not trusted to the tenant bundle.

Aster presently has deployment-level authority only. It does not provide Convex auth identity/context or per-user authorization by itself.

## Supported and unsupported surfaces

Supported end to end:

- real Convex bundled point queries;
- real Convex bundled mutations for get/insert/patch/replace/delete;
- canonical inserted IDs;
- fresh-cell visibility after commit;
- conflict retry, range evidence, and retention checks;
- process and container deployment paths.

Not implemented:

- actions, HTTP actions, scheduling, external egress;
- storage/search/vector/auth syscalls;
- full Convex query builder/filter/collect semantics;
- component namespaces;
- a Convex-compatible HTTP API;
- warm cell pools.

The one-shot cell model is intentional until lifecycle, identity, and resource reset can be proved across reuse.

## Verification map

- `crates/v8cell/tests/module_loader.rs`: real bundled query/mutation behavior and watchdog limits.
- `crates/ipc/tests/process_boundary.rs`: real UDS process failures and lifecycle.
- `crates/ipc/tests/authoritative_postgres.rs`: write, canonical server ID mint, commit, and fresh-cell read on one history.
- `crates/store-postgres/tests/write_plane_it.rs`: lease/fence/retention/conflict invariants against real Postgres.
- `docker/smoke-bundle.sh`: query, mutation, canonical ID, and fresh read through either direct containers or the exact production Compose topology.
- `AsterFence.tla`: exhaustive positive model plus required counterexamples for broken epoch, retention, and atomicity variants.
