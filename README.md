# Aster

**Run untrusted Convex code without giving it your database credentials.**

Aster is an open-source execution plane for [Convex](https://www.convex.dev/) apps where the JavaScript that runs your queries **never holds a Postgres handle**. A separate broker process owns the database; tenant code lives in a V8 cell that gets sealed snapshot capsules over a Unix-domain socket and nothing else. Even a CVE-class V8 escape leaves the attacker with an empty isolate — they can't reach the database, the modules dir, or other tenants.

It runs **real `npx convex deploy` bundles** unmodified. You don't rewrite your app — you just run it somewhere with stronger isolation than the Convex backend gives you out of the box.

```text
$ ./docker/smoke-bundle.sh 0.4-modulequery
==> staged bundle at /tmp/.../test-bundle.blob (14854 bytes, sha256 ef11...)
==> sourcePackageId = r4zexvjnaqqewnanxvq5anfexsana5t4
==> starting postgres:16
==> starting brokerd (ASTER_STORE=postgres, modules dir mounted)
==> running v8cell (module=messages.js, function=getById, id=...)
==> v8cell stdout: {"capsule_hash":3703888312439000736,
                    "output":"{\"_id\":\"messages|aaaa...\",\"name\":\"ian\"}",
                    "traps":1}
OK: aster brokerd(postgres) + v8cell module-query smoke passed —
    real npx-convex-deploy bundle compiled as ESM,
    getById invoked with args=[{"id":"..."}],
    db.get(id) traversed Convex.asyncSyscall("1.0/get") → broker → postgres,
    document body returned with name="ian".
```

That's a real bundle. A real Postgres. A real V8 isolate. Real `db.get(id)` over the wire. **One** trap drained — exactly the syscall the user's `getById` query made. The cell never had a database connection.

## Who this is for

Aster solves three different problems for three different audiences. Pick whichever describes you.

### You're hosting Convex apps for other people (PaaS / multi-tenant SaaS)

Convex Cloud is closed. Self-hosted Convex is open but the executor and the database authority sit in the same process — if you run customer code on your infra, a single V8 escape gives that customer your other customers' data.

**Aster is the missing piece.** Tenant code goes in cells with no credentials; the broker is the only thing that talks to Postgres. You can colocate 50 customer apps on one VPS without taking the "shared V8 sandbox" risk.

### You're in a regulated industry (HIPAA, SOC 2, GDPR, financial)

"How do you guarantee the application code can't reach data outside its scope?" is a question your auditor asks. With Convex's standard runtime the answer is "code review, V8 sandbox, IAM." With Aster the answer is **"the code physically does not have credentials. Every read passes through a sealed capsule with a per-invocation context. Here's the audit trail."**

### You want to learn how a capability-narrowed runtime is actually built

Aster is roughly 10k lines of Rust covering: V8 ESM compilation, BLAKE3 keyed-MAC capsule sealing, Convex IDv6/ConvexValue codec ports, a Postgres adapter that reads the same schema the upstream backend writes to, an `_modules` × `_source_packages` join + ZIP unzip pipeline that resolves module bundles, and a process-separated broker/cell architecture with read-trap continuations. **Every piece is tested. Real Convex bundles run end-to-end.** It's a working reference for how to build this kind of system.

## How Aster compares

| | Per-tenant isolation | Tenant code holds DB creds? | Snapshot semantics | Self-hostable | Runs `npx convex` bundles |
|---|---|---|---|---|---|
| Convex Cloud | V8 isolate | yes | MVCC | ❌ | yes |
| Convex self-hosted | V8 isolate | yes | MVCC | ✅ | yes |
| Cloudflare Workers | V8 isolate | yes (env-var) | none | ❌ | no |
| Deno Deploy | V8 isolate | yes | none | ❌ | no |
| AWS Lambda | container | yes (IAM role) | none | ❌ | no |
| Fly.io Machines | Firecracker microVM | yes | none | partial | no |
| **Aster** | V8 isolate **+ process boundary + sealed capsules** | **NO** | MVCC inherited | ✅ | **yes** |

The "tenant code holds DB creds?" column is the difference. Everywhere else, application code has some path to credentials (env-var, IAM role, fetch endpoint). On Aster the cell has a Unix-domain socket to a broker; that's the entire surface.

## Quick start — see it work

You need Docker. The first build is slow (~5 min cold for V8); subsequent builds are seconds.

```bash
# Build both binaries from this repo
docker build --target=runtime-broker -t aster-brokerd:0.4 -f docker/Dockerfile .
docker build --target=runtime-v8cell -t aster-v8cell:0.4 -f docker/Dockerfile .

# Three smokes from least to most ambitious:
./docker/smoke.sh           0.4              # UDS + V8 + memory store. ~5s.
./docker/smoke-postgres.sh  0.4              # add postgres-backed reads. ~30s.
./docker/smoke-bundle.sh    0.4-modulequery  # real `npx convex deploy` bundle, end-to-end.
```

Each script is self-contained: spins containers, asserts an exact stdout, tears everything down. The third one is the killer demo — copy it to read what staging a real Convex bundle on disk + driving a real query through the broker actually looks like in practice.

## What's working in v0.6

- **Real `npx convex deploy` bundles execute end-to-end.** Cell compiles the bundle as a V8 ES module, finds the named export, asserts it's a `query`, calls `<export>.invokeQuery(args_json)`, drives the `Convex.asyncSyscall("1.0/get")` trap loop while the user's `db.get` awaits — same shape Convex's own runner uses, with the bundle's own `convex/server` / `convex/values` / `_generated/*` already inlined by esbuild.
- **Read path against real Postgres.** Broker reads the same `documents` / `_tables` / `_modules` / `_source_packages` schema the upstream Convex backend writes to. IDv6 strings the JS bundle emits resolve to tablet UUIDs via a `_tables`-backed mapping cache.
- **Cryptographic capsule integrity.** BLAKE3 keyed-MAC seals every snapshot a cell receives. A cell can't forge or replay another cell's capsule — context is bound to `(cell_id, lease_epoch)`.
- **Process-separated broker + cell over UDS.** The cell binary cannot dial the database. The broker binary cannot execute user code. Two different attack surfaces; neither one alone gets the attacker anywhere useful.
- **Two layers of CI proof.** A library-level test runs the byte-for-byte 58 KB output of `npx convex deploy` for `aster-e2e-fixture/messages.ts` through `V8SandboxCell::execute_module_query_with_broker` and asserts the document round-trips with exactly **1** syscall trap. A docker-level smoke script does the same against `postgres:16` + the actual binaries.
- **VPS-validated through the Synapse control plane.** The full `HTTP → Synapse → spawn cell → broker → Postgres → response` path is documented end-to-end at `docs/ASTER_VPS_SMOKE.md` with the captured stdout from a Hetzner CPX22 invocation.

## What's deliberately out of scope for v0.6

- **Mutations and actions.** v0.6 is read-only by design. The cell rejects `isMutation === true` exports with a typed error so the OCC commit story can land separately with its own review surface.
- **Convex-shaped HTTP frontend.** Aster ends at "given a module path + function + args, run it." The `/api/query/<module>:<fn>` path that Convex CLI clients speak lives in [Iann29/convex-synapse](https://github.com/Iann29/convex-synapse), not here.
- **OS-level cell sandboxing.** The cell container runs as a non-root UID but doesn't yet have cgroups, seccomp, read-only rootfs, or per-tenant UID. P2 hardening, planned but not done.
- **Cell warm-pool reincarnation.** Every invocation spawns a fresh container right now. Warm pooling is on the roadmap (see `docs/ABSURD_IDEAS.md`).

## How it works (one picture)

```text
  HTTP request                                                       
       │                                                             
       ▼                                                             
  ┌─────────┐    spawn (per invocation)    ┌────────────────────┐
  │ Synapse │ ──────────────────────────▶ │ aster-v8cell        │
  │ control │                              │  - V8 isolate       │
  │ plane   │                              │  - module loader    │
  └─────────┘                              │  - Convex shims     │
                                            │  - capability:      │
                                            │    UDS + capsules   │
                                            └─────────┬───────────┘
                                                      │
                                                      │ LoadModuleBundle(capsule, path)
                                                      │ HydratePoint(capsule, doc_id)
                                                      │  (length-prefixed JSON over UDS)
                                                      ▼
                                            ┌─────────────────────┐
                                            │ aster-brokerd       │
                                            │  - owns Postgres    │
                                            │  - seals capsules   │
                                            │    (BLAKE3 keyed)   │
                                            │  - resolves IDv6    │
                                            │  - reads modules    │
                                            └─────────┬───────────┘
                                                      │
                                                      │ SQL
                                                      ▼
                                            ┌─────────────────────┐
                                            │ postgres            │
                                            │  - documents        │
                                            │  - _tables          │
                                            │  - _modules         │
                                            │  - _source_packages │
                                            │  - ZIP blobs on FS  │
                                            └─────────────────────┘
```

The cell never has a row from the broker's address space. It gets bytes that have been sealed, and a context (cell_id + lease_epoch) that was used as input to the seal. Replay a sealed capsule under a different cell_id — MAC fails. Try to use a capsule past its snapshot_ts — MAC fails. The integrity boundary is mathematical, not code-review.

## Run

```bash
cargo fmt --all -- --check
cargo build --workspace --locked
cargo test --workspace --locked
cargo run --release -p aster-host --bin aster_bench -- 10000 32
```

Workspace tests: 106 unit + 19 Postgres-integration (gated by `--features postgres-it` + `ASTER_DB_URL`) + 1 cross-process E2E + 3 docker smokes. All green on CI.

### Postgres adapter local lane

```bash
docker run -d --rm --name aster-pg-dev -p 5433:5432 \
    -e POSTGRES_USER=aster -e POSTGRES_PASSWORD=aster \
    -e POSTGRES_DB=aster postgres:16
ASTER_DB_URL=postgres://aster:aster@127.0.0.1:5433/aster \
    cargo test -p aster-store-postgres --features postgres-it -- --test-threads=1
```

CI runs the same lane via a service container; locally without `ASTER_DB_URL` the gated tests skip silently.

## Crates

| Crate | Owns |
|---|---|
| `crates/capsule/` | MVCC store, snapshot capsules, BLAKE3 keyed seals, OCC committer |
| `crates/broker/` | `CapsuleBrokerClient` (cell-facing trait) + `CapsuleStore` (storage backend trait) + `LocalCapsuleBroker` |
| `crates/store-postgres/` | `PostgresCapsuleStore` — real Convex `documents` reads via `tokio-postgres` + `deadpool-postgres`. Includes the `_tables` mapping cache, the `_modules` × `_source_packages` index, and the local-FS modules-storage adapter that resolves a path to bundle bytes (`load_module_bundle`). |
| `crates/convex-codec/` | Std-only port of `convex-backend@main:crates/value/src/{base32,id_v6,json}`. `DocumentIdV6` (encode/decode), Crockford lowercase base32, `ConvexValue` (`$integer` / `$float` / `$bytes` JSON wrappers). |
| `crates/runner/` | Tenant-pinned sandbox cells, in-process toy program runner |
| `crates/v8cell/` | Real V8 isolate. `Aster.read` (legacy), `Convex.asyncSyscall("1.0/get")`, and the v0.6 `execute_module_query_with_broker` that runs real Convex bundles. Locked by `tests/module_loader.rs`. |
| `crates/ipc/` | Length-prefixed JSON over UDS. `aster_brokerd` + `aster_v8cell` binaries. v0.6 adds `LoadModuleBundle` (capsule-gated bundle bytes) and `bundle::extract_module_source` (ZIP unzip with `modules/<path>.js` priority). |
| `crates/host/` | In-process facade + benchmark binary + smoke harnesses |

## Version history

| | What landed |
|---|---|
| v0.1 | Snapshot capsules + read-trap continuations + tenant-pinned sandbox cells (modeled in pure Rust). |
| v0.2 | Real V8 isolate suspend/resume on missing reads via `await Aster.read(...)`. Cryptographic capsule seals (BLAKE3 keyed MAC + cell binding). |
| v0.3 | Broker and cell run as **separate OS processes** over a Unix-domain socket. Cell can never reach the broker's address space. |
| v0.4 | Broker reads from **real Postgres** (the same database a Convex backend writes to). Cell exposes `Convex.asyncSyscall("1.0/get")`. A Convex-compiled `await ctx.db.get(id)` resolves end-to-end against the cell's hydrated capsule. |
| v0.5 | Convex IDv6 codec, `_tables`-backed table-mapping cache, `ConvexValue` `$integer`/`$float`/`$bytes` JSON wrappers. |
| v0.6 | **Real Convex bundles run end-to-end.** `_modules` × `_source_packages` index, local-FS module-storage adapter, `LoadModuleBundle` IPC, V8 ESM compile + `<export>.invokeQuery(args)` dispatch, binary `ASTER_FUNCTION_NAME`/`ASTER_ARGS_JSON` envs, full docker smoke against real Postgres. |

## Design docs

- `docs/ARCHITECTURE.md` — current architecture
- `docs/CONVEX_POSTGRES_REFERENCE.md` — the schema we read against, with 12 known gotchas, verbatim from `get-convex/convex-backend`
- `docs/POSTGRES_ADAPTER_PLAN.md` — historical 5-commit plan, with follow-up status
- `docs/THEORY_REGISTER.md` — research theories
- `docs/ABSURD_IDEAS.md` — intentionally strange/falsifiable ideas (warm pools, cell reincarnation, ring-buffer trap diodes, etc.)
- `docs/V8_QUESTION.md` — V8 experiment memo
- `docs/COMPARISON_MATRIX_V0.3.md` — prior-art matrix
- `docs/LOCAL_VALIDATION.md` — what passed on the developer machine
- Synapse-side integration status, runbook, and pointers: [`docs/ASTER_INTEGRATION.md`](https://github.com/Iann29/convex-synapse/blob/main/docs/ASTER_INTEGRATION.md) in `Iann29/convex-synapse`

## License

Apache 2.0 OR MIT, your choice.
