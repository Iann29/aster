# Aster Runner v0.6

Aster Runner is a research prototype for **capability-narrowed Convex function
execution**: tenant JavaScript runs in a V8 cell that holds zero database
credentials, fed sealed snapshot capsules by a broker that owns the Postgres
handle. Cells get bytes; cells never get a connection.

The story-so-far stack:

| Version | Property |
|---|---|
| v0.1 | Snapshot Capsules + Read-Trap Continuations + Tenant-Pinned Sandbox Cells (modeled in pure Rust) |
| v0.2 | Real V8 isolate suspend/resume on missing reads via `await Aster.read(...)`. Cryptographic capsule seals (BLAKE3 keyed MAC + cell binding). |
| v0.3 | Broker and cell run as **separate OS processes** over a Unix-domain socket. Cell can never reach the broker's address space. |
| v0.4 | Broker reads from **real Postgres** (the same database a Convex backend writes to). Cell exposes the upstream **`Convex.asyncSyscall("1.0/get")`** wire shape — a Convex-compiled function calling `await ctx.db.get(id)` resolves end-to-end against the cell's hydrated capsule. |
| v0.5 | **Convex IDv6 codec** (Crockford base32 + VInt + Fletcher16). **Table-mapping cache** reads the `_tables` system tablet so an IDv6 string a JS bundle hands to `db.get(id)` resolves to the right tablet UUID without a tablet-aware caller. **ConvexValue codec** locks the `$integer`/`$float`/`$bytes` JSON wire shape so the cell can round-trip user values losslessly. |
| v0.6 | **Real Convex bundle execution end-to-end.** `_modules` × `_source_packages` join (#15), local-FS bundle storage adapter (#17), `LoadModuleBundle` IPC capsule-gated by the broker (#19), cell-side ZIP unzip + `modules/<path>.js` resolution (#20, #21), **V8 ESM compile + `<export>.invokeQuery(args)` dispatch with `Convex.{syscall,asyncSyscall}` globals** (#22), v8cell binary wired with `ASTER_FUNCTION_NAME`+`ASTER_ARGS_JSON` envs (#23), and a `docker/smoke-bundle.sh` end-to-end harness (#24) that runs a real `npx convex deploy` ZIP through the binaries against real Postgres. **The cell now executes real Convex queries.** |

The proof point lives at two layers, both in CI:

- **Library:** `crates/v8cell/tests/module_loader.rs::module_get_by_id_through_fake_broker_returns_doc` runs the byte-for-byte 58 KB `npx convex deploy` output of `aster-e2e-fixture/convex/messages.ts` through `V8SandboxCell::execute_module_query_with_broker`, asserts the seeded document round-trips with `name`, `body`, `_id` intact and exactly **1** `db.get` syscall trap drained.
- **Binary + Postgres:** `docker/smoke-bundle.sh 0.4-modulequery` boots `postgres:16`, stages a real ZIP at `<modules_dir>/<storage_key>.blob`, runs `aster-brokerd` and `aster-v8cell` containers, and asserts `output:"{\"_id\":\"messages|...\",\"name\":\"ian\"}"` comes back through `Convex.asyncSyscall("1.0/get") → broker → postgres`.

What's still under construction (not in v0.6):

- **Mutations and actions.** The cell explicitly rejects non-query exports
  (`isMutation === true` → typed error). v0.6 is read-only on purpose; commit
  paths land separately so the OCC story has its own review surface.
- **Convex-shaped HTTP frontend.** Synapse already has a raw-JS `aster/invoke`
  endpoint and now a module-mode binary, but `/api/query/<module>:<fn>` →
  cell invocation is still on the Synapse side, not Aster's.
- **Per-deployment source binding (Synapse).** Today
  `SYNAPSE_ASTER_POSTGRES_URL` + `SYNAPSE_ASTER_MODULES_DIR` are
  process-level config; production needs a durable "this Aster deployment
  mirrors that Convex deployment" record.

## Run

```bash
cargo fmt --all -- --check
cargo build --workspace
cargo test --workspace
cargo test -p aster-ipc --test process_boundary -- --nocapture
cargo run --release -p aster-host --bin aster_bench -- 10000 32
protoc --proto_path=proto --descriptor_set_out=/tmp/aster-v0.4.pb proto/aster.proto
```

### Postgres adapter (v0.4)

The store-postgres integration tests need a live `postgres:16`. Locally:

```bash
docker run -d --rm --name aster-pg-dev -p 5433:5432 \
    -e POSTGRES_USER=aster -e POSTGRES_PASSWORD=aster \
    -e POSTGRES_DB=aster postgres:16
ASTER_DB_URL=postgres://aster:aster@127.0.0.1:5433/aster \
    cargo test -p aster-store-postgres --features postgres-it -- --test-threads=1
```

CI does the same via the `postgres-it` lane (a service container + a
`--test-threads=1` run). Skip the lane locally without setting
`ASTER_DB_URL` and the gated tests stay invisible.

## Docker images

Both binaries ship as separate runtime images out of the same multi-stage
Dockerfile:

```bash
# Build
docker build --target=runtime-broker -t aster-brokerd:0.4 -f docker/Dockerfile .
docker build --target=runtime-v8cell -t aster-v8cell:0.4 -f docker/Dockerfile .

# End-to-end smoke (assertions inside the script)
./docker/smoke.sh 0.4
```

The repo does not publish these images to a registry yet. For VPS smoke,
build locally and ship them with `docker save | scp | docker load` unless
a release workflow has been added since this note.

The `docker/smoke.sh` script runs `aster-brokerd` as a long-lived service
behind a per-deployment Docker volume, then runs `aster-v8cell` as a
one-shot container that opens the shared socket and prints
`{"output":42,"traps":1,"capsule_hash":...}` if the capability boundary
holds.

For the v0.6 module-query path against real Postgres + a real
`npx convex deploy` ZIP:

```bash
# Builds whatever tag you pass, stages a ZIP at <modules_dir>/<key>.blob,
# spins postgres:16 + brokerd(postgres) + v8cell, asserts getById returns
# the seeded document.
./docker/smoke-bundle.sh 0.4-modulequery
```

Expected stdout from the cell when the smoke runs green:

```
{"capsule_hash":3703888312439000736,
 "output":"{\"_id\":\"messages|aaaa...\",\"name\":\"ian\"}",
 "traps":1}
```

## Crates

| Crate | What |
|---|---|
| `crates/capsule/` | MVCC store, snapshot capsules, BLAKE3 keyed seals, OCC committer |
| `crates/broker/` | `CapsuleBrokerClient` (cell-facing trait) + `CapsuleStore` (storage backend trait) + `LocalCapsuleBroker` |
| `crates/store-postgres/` (v0.4 SQL, v0.5 mapping cache, v0.6 module index + storage adapter) | `PostgresCapsuleStore` — real Convex `documents` reads via `tokio-postgres` + `deadpool-postgres`, sync API + async island. The `_tables`-backed mapping cache makes `read_point` accept both `<table_hex>/<id_hex>` and IDv6 strings; the module index (`_modules` × `_source_packages` join) + local-FS storage adapter resolve a module path to bundle bytes via `load_module_bundle`. |
| `crates/convex-codec/` (v0.5) | Std-only port of `convex-backend@main:crates/value/src/{base32,id_v6,json}`. `DocumentIdV6` (encode/decode) + Crockford lowercase base32 + `ConvexValue` (`$integer`/`$float`/`$bytes` JSON wrappers). |
| `crates/runner/` | Tenant-pinned sandbox cells, in-process toy program runner |
| `crates/v8cell/` (v0.6 module loader) | Real V8 isolate. Exposes `Aster.read` (legacy), `Convex.asyncSyscall("1.0/get")` (v0.4), AND `execute_module_query_with_broker` — compiles a real Convex bundle as ESM, calls `<export>.invokeQuery(args)`, drives the `Convex.asyncSyscall` trap loop. Locked by `tests/module_loader.rs` against the byte-for-byte `npx convex deploy` output of `aster-e2e-fixture/messages.ts`. |
| `crates/ipc/` (v0.6 bundle IPC) | Length-prefixed JSON over UDS. `aster_brokerd` + `aster_v8cell` binaries + the cross-process E2E test. v0.6 adds `LoadModuleBundle` (capsule-gated bundle bytes) and `bundle::extract_module_source` (ZIP unzip with `modules/<path>.js` priority). |
| `crates/host/` | In-process facade + benchmark binary + the `e2e.rs` + `crypto_and_v8.rs` smoke harnesses |

## Important docs

- `docs/ARCHITECTURE.md` — current v0.5 architecture and Synapse boundary
- `docs/POSTGRES_ADAPTER_PLAN.md` — historical five-commit plan, plus follow-up status
- `docs/CONVEX_POSTGRES_REFERENCE.md` — DDL, read SQL templates, 12 gotchas, verbatim from `get-convex/convex-backend`
- `docs/V8_QUESTION.md` — V8 experiment memo
- `docs/THEORY_REGISTER.md` — research theories
- `docs/ABSURD_IDEAS.md` — intentionally strange/falsifiable ideas
- `docs/COMPARISON_MATRIX_V0.3.md` — prior-art matrix
- `docs/SYNAPSE_MIGRATION_V0.3.md` — operator migration path
- `docs/LOCAL_VALIDATION.md` — what passed on the developer machine

## What this lets you demo today

- **Run a real `npx convex deploy` bundle inside an Aster cell** —
  `docker/smoke-bundle.sh` spins postgres:16, stages a real ZIP at
  `<modules_dir>/<storage_key>.blob`, runs the binaries, and the cell
  prints the seeded document JSON in response to `getById({id})`.
  Same proof at the library level via `cargo test -p aster-v8cell --test module_loader`.
- Spawn `aster-brokerd:0.4` against a real Convex Postgres deployment
  (point it at the same DB the upstream backend writes to). The broker
  reads `documents` rows directly — `snapshot_ts`, `read_point`, and
  `read_prefix` are wired and tested.
- Hand the broker an IDv6 string (the same string `db.get(id)` would
  produce in a Convex JS bundle). The `_tables`-backed mapping cache
  resolves `table_number → tablet_uuid` on the broker side; the cell
  never sees the table mapping.
- Round-trip a typed Convex value (`Int64`, `Float64`, `Bytes`,
  arrays, sorted objects) through the JSON wire shape.
  `aster_convex_codec::ConvexValue::{from_json,to_json}` is the entry
  point; tests in `crates/convex-codec/src/value.rs` lock the shape
  bit-for-bit against `convex-backend@main:crates/value/src/json/`.

## What this does NOT let you demo today

- **Mutations / actions.** The cell explicitly rejects `isMutation` and
  `isAction` exports (typed error). v0.6 is read-only on purpose; the
  commit / OCC story will land separately so its review surface stays
  isolated from the read path.
- **Convex-shaped HTTP frontend.** Synapse has a raw-JS
  `POST /v1/deployments/{name}/aster/invoke` endpoint and now a
  module-mode binary, but `/api/query/<module>:<fn>` → cell invocation
  routing still has to land on the Synapse side.
- **Real-VPS smoke through Synapse.** Today's `smoke-bundle.sh` uses
  raw `docker run`, not Synapse's `provisionAster` + `aster/invoke`
  flow. The Synapse path was VPS-smoked earlier with raw-JS only.
- **Hostile multi-tenant isolation.** The cell container runs as a
  non-root UID but doesn't yet have cgroups / seccomp / read-only
  rootfs / per-tenant UID separation.
