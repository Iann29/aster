# Aster

**Run untrusted Convex code without giving it database authority.**

Aster is an open-source execution plane for Convex applications. Tenant JavaScript runs in a one-shot V8 cell with no database credentials, no module-directory mount, and no network interface. A separate broker owns Postgres, module loading, policy, capsule sealing, and commit admission. The only cell capability is a Unix-domain socket carrying a small, length-prefixed protocol.

Aster executes real `npx convex deploy` bundles without rewriting application code. Queries and mutations suspend on Convex syscalls, the broker hydrates a MAC-sealed snapshot capsule, and mutations submit their observed reads plus write set through one transactional OCC fence.

The production Compose profile now proves the complete supported path:

```text
real Convex bundle
  -> query db.get
  -> one-time launch token
  -> rootless, networkless V8 cell
  -> broker over UDS
  -> authoritative Postgres history
  -> mutation db.insert
  -> canonical Convex IDv6 allocation
  -> transactional commit fence
  -> fresh cell reads the committed document
```

Run that proof locally:

```bash
docker build --target runtime-broker -t aster-brokerd:0.8 -f docker/Dockerfile .
docker build --target runtime-v8cell -t aster-v8cell:0.8 -f docker/Dockerfile .
ASTER_PRODUCTION_COMPOSE_SMOKE=1 ./docker/smoke-bundle.sh 0.8
```

The smoke uses real Postgres, the checked-in byte-for-byte output of a real Convex deployment bundle, the hardened production topology, a query, a mutation, and a fresh-cell readback. The cell never receives a Postgres handle, database URL, seal key, launch key, policy file, or module storage path.

## Security boundary

Aster's distinction is not merely “JavaScript runs in V8.” It separates execution from authority:

- **Cell:** untrusted tenant bundle, V8 isolate, sealed capsule, UDS client.
- **Broker:** trusted policy and transaction authority, Postgres credentials, seal key, launch-token key, module source.
- **Postgres:** one Aster history per tenant/deployment for transaction data; Convex module metadata remains a deployment input.
- **Container supervisor:** trusted to enforce the production profile.

The shipped production profile enforces:

- non-root broker and cell processes under an operator-selected UID/GID;
- read-only root filesystems, all Linux capabilities dropped, `no-new-privileges`, default Docker seccomp, PID/CPU/memory/file-descriptor limits, and bounded tmpfs;
- Docker `network_mode: none` for cells and the launch-token issuer;
- a database network attached only to the broker;
- a host-owned `0700` runtime directory mounted read-only into cells;
- broker-side `SO_PEERCRED` UID admission on the UDS;
- one-use, short-lived launch tokens bound to cell, tenant, deployment, and authority epoch;
- session TTL and concurrency limits;
- deployment policy for document reads, writes, scans, module loads, and insert tables;
- BLAKE3 keyed-MAC capsule seals bound to the live session and lease epoch;
- V8 heap limits, trap limits, invocation retries, and a host watchdog that terminates runaway JavaScript.

The trusted computing base still includes the host kernel, Docker/container runtime, broker, Postgres, deployment policy, module-ingestion path, and operator. Aster is not an independently audited sandbox or a defense against a container-runtime/kernel escape.

## Supported Convex surface

Aster v0.8 intentionally implements a narrow, testable subset:

- real bundled **queries** using `db.get`;
- real bundled **mutations** using `db.get`, `db.insert`, `db.patch`, `db.replace`, and `db.delete`;
- read-your-own-writes and absence tracking inside an invocation;
- server-side canonical IDv6 allocation for inserts;
- commit conflict retry from a fresh authoritative snapshot;
- module resolution through Convex `_modules` and `_source_packages` metadata plus hash-verified bundle blobs;
- point reads and certified prefix reads in the broker/storage protocol.

Not implemented: actions, HTTP actions, scheduled functions, search/vector syscalls, arbitrary Convex query-filter/collect syscalls, file storage syscalls, auth context, external egress, components, and a Convex-compatible HTTP frontend. Aster ends at “invoke this module export with these arguments.” The HTTP/control-plane integration belongs in [Iann29/convex-synapse](https://github.com/Iann29/convex-synapse).

## Install and operate

Requirements: Docker Engine with Compose v2, a reachable Postgres deployment, and access to the Convex module-blob directory. Source verification uses Rust 1.94.1.

### 1. Build the images

```bash
docker build --target runtime-broker -t aster-brokerd:0.8 -f docker/Dockerfile .
docker build --target runtime-v8cell -t aster-v8cell:0.8 -f docker/Dockerfile .
```

The Dockerfile pins its builder/runtime base digests and verifies the uncompressed rusty_v8 native archive hash before linking. Both runtime images execute as non-root by default; Compose maps them to the host operator UID/GID so bind-mounted secrets and the socket directory remain private.

### 2. Initialize deployment state

```bash
./docker/aster-init .aster
cp docker/.env.production.example .env.aster
```

`aster-init` creates independent random 256-bit seal and launch keys, a `0700` runtime directory, a `0600` database URL file, and a deny-all policy template. Put a container-reachable Postgres URL in `.aster/db_url`; for example, use the Postgres service name on the external database network rather than `127.0.0.1`.

Edit `.env.aster`:

- replace every absolute path;
- set the existing external Docker network in `ASTER_DATABASE_NETWORK`;
- set the Postgres schema containing Convex module metadata;
- set tenant and deployment identifiers;
- grant only the required document/module prefixes and insert table names in `policy.json`.

Load the environment and start the authority:

```bash
set -a
. ./.env.aster
set +a

docker compose -f docker/compose.production.yml up -d --wait brokerd
```

### 3. Invoke a function

```bash
export ASTER_MODULE_PATH=messages.js
export ASTER_FUNCTION_NAME=getById
export ASTER_ARGS_JSON='[{"id":"j57ananananananananananananamzxg"}]'

./docker/aster-invoke
```

`aster-invoke` waits for the broker, reads its published authority epoch, obtains a one-use token in an isolated helper container, launches exactly one networkless cell, enforces an outer timeout, and removes the cell afterward.

Operational state is intentionally small:

```text
.aster/
  db_url          # 0600; broker only
  seal.key        # 0600; broker only
  launch.key      # 0600; broker + isolated token issuer
  policy.json     # deployment authority limits
  run/
    broker.sock
    authority_epoch
```

## How the transaction path works

```text
operator / control plane
        |
        | module, export, args, one-use launch token
        v
+--------------------------+
| aster_v8cell             |
| V8 + Convex bundle       |
| no DB credentials        |
| no module mount          |
| no network               |
+------------+-------------+
             | UDS
             | InitialCapsule / HydratePoint / HydratePrefix
             | MintDocumentId / LoadModuleBundle / Commit / Abort
             v
+--------------------------+          +-------------------------+
| aster_brokerd            |  SQL     | Postgres                |
| policy + session table   +--------->| aster.log               |
| seal + launch keys       |          | aster.lease             |
| lease + commit fence     |          | aster.retention         |
+------------+-------------+          | Convex module metadata  |
             |                        +-------------------------+
             | sealed capsule or commit outcome
             v
       invocation envelope
```

For a mutation, the final evidence object is:

```text
(snapshot capsule, consumed point reads, certified ranges, write set, lease epoch)
```

The Postgres fence takes the tenant/deployment lease row `FOR UPDATE`, checks the epoch and snapshot horizon, validates every authenticated conflict window, and appends the whole write set before the transaction commits. Reads and validation consult the same `aster.log` history. Retention is monotonic and checked after reads to prevent half-compacted snapshots from becoming evidence.

## Verification

Fast local lane:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Authoritative Postgres lane:

```bash
docker run -d --rm --name aster-pg-dev -p 5433:5432 \
  -e POSTGRES_USER=aster -e POSTGRES_PASSWORD=aster \
  -e POSTGRES_DB=aster \
  postgres:16@sha256:17e67d7b9890c99b055ba1e0d5c5be4ec27c9d3a72bda32db24a5e5d8a85af0c

ASTER_DB_URL=postgres://aster:aster@127.0.0.1:5433/aster \
  cargo test --locked -p aster-store-postgres --features postgres-it -- --test-threads=1
ASTER_DB_URL=postgres://aster:aster@127.0.0.1:5433/aster \
  cargo test --locked -p aster-ipc --features postgres-it -- --test-threads=1
```

Container lanes:

```bash
./docker/smoke.sh 0.8
./docker/smoke-postgres.sh 0.8
ASTER_PRODUCTION_COMPOSE_SMOKE=1 ./docker/smoke-bundle.sh 0.8
```

CI runs on Blacksmith runners and gates ShellCheck, formatting, Clippy, the workspace suite, Postgres integration, hardened image builds with SBOM/provenance, Trivy scans, the three container lanes, dependency review, RustSec, and TLA+ positive/negative models. Third-party actions and container bases are pinned.

## Formal and adversarial evidence

`AsterFence.tla` models lease acquisition, fenced commit, conflict validation, retention, compaction, and epoch failover. CI requires:

- the positive model to exhaust without an invariant violation;
- shadowed-only compaction to remain sound;
- epoch reuse, missing retention pinning, and non-atomic fencing mutants to produce their named counterexamples.

The paper's `paper/CLAIMS.md` is an evidence ledger rather than a marketing checklist. The hostile re-referee report and its repairs remain in `paper/sources/` and `V07_BOARD.md`.

## Workspace

| Crate | Responsibility |
|---|---|
| `crates/capsule` | Canonical capsules, range certificates, MVCC values, keyed seals. |
| `crates/broker` | Cell-facing capability trait, store trait, in-memory broker, commit-fence types. |
| `crates/store-postgres` | Authoritative Aster history, transactional write plane, Convex table mapping, module index/storage. |
| `crates/convex-codec` | Strict Convex IDv6/base32 and ConvexValue codecs. |
| `crates/v8cell` | V8 execution, Convex syscall bridge, in-cell read/write view, resource watchdog. |
| `crates/ipc` | UDS wire protocol, policy, launch tokens, broker/cell/token binaries, bundle extraction. |
| `crates/runner` | Tenant-pinned pure-Rust runner used by focused tests. |
| `crates/host` | In-process facade and benchmark harness. |

## Version history

| Version | Main result |
|---|---|
| v0.1 | Snapshot capsules and read-trap continuations in pure Rust. |
| v0.2 | Real V8 suspend/resume and keyed capsule seals. |
| v0.3 | Broker/cell process split over UDS. |
| v0.4 | Real Postgres reads and the Convex `1.0/get` syscall. |
| v0.5 | Strict IDv6/ConvexValue codecs and Convex table mapping. |
| v0.6 | Real Convex bundle loading and ESM query invocation. |
| v0.7 | Authenticated read consumption, certified ranges, lease epochs, transactional commit fence, mutations, retention/compaction proofs, and measured benchmarks. |
| v0.8 | Shared authoritative history, server-side ID minting, deployment policy, one-use launch tokens, peer-authenticated concurrent IPC, rootless/networkless topology, resource watchdogs, and Blacksmith CI/security gates. |

## Design material

- `docs/ARCHITECTURE.md` — runtime and trust-boundary details.
- `docs/CONVEX_POSTGRES_REFERENCE.md` — upstream Convex Postgres schema notes used by module ingestion.
- `docs/THEORY_REGISTER.md` — research theories and status.
- `docs/ABSURD_IDEAS.md` — intentionally falsifiable follow-up ideas.
- `docs/V8_QUESTION.md` — V8 experiments.
- `docs/LOCAL_VALIDATION.md` — historical local evidence.
- `paper/authenticate-the-reads.md` and `paper/CLAIMS.md` — technical report and claim ledger.

## License

Apache 2.0 OR MIT, at your option.
