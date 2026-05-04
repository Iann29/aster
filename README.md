# Aster Runner v0.5

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

What's still under construction (not in v0.5):

- **Convex module loader** — today the v8cell runs an `async function main()`
  defined in a single source string. Reading `_modules` + `_source_packages`
  + the modules-storage layer (S3 / local FS) so the cell can execute
  arbitrary `convex/*.ts` exports is the next-largest piece (#98).
- **HTTP frontend** — there is no `/api/query/<module>:<fn>` endpoint yet.
  Driving cells from real client traffic needs a Synapse-side cell-on-demand
  spawn (#100) and a request router (#101).
- **Cell-on-demand spawn** from the operator side (lives in
  [`Iann29/convex-synapse`](https://github.com/Iann29/convex-synapse) — see
  [`docs/ASTER_INTEGRATION.md`](https://github.com/Iann29/convex-synapse/blob/main/docs/ASTER_INTEGRATION.md)).

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

## Crates

| Crate | What |
|---|---|
| `crates/capsule/` | MVCC store, snapshot capsules, BLAKE3 keyed seals, OCC committer |
| `crates/broker/` | `CapsuleBrokerClient` (cell-facing trait) + `CapsuleStore` (storage backend trait) + `LocalCapsuleBroker` |
| `crates/store-postgres/` (v0.4 SQL, v0.5 mapping cache) | `PostgresCapsuleStore` — real Convex `documents` reads via `tokio-postgres` + `deadpool-postgres`, sync API + async island. v0.5 adds the `_tables`-backed mapping cache so `read_point` accepts both `<table_hex>/<id_hex>` (Aster wire form) and IDv6 strings. |
| `crates/convex-codec/` (v0.5) | Std-only port of `convex-backend@main:crates/value/src/{base32,id_v6,json}`. `DocumentIdV6` (encode/decode) + Crockford lowercase base32 + `ConvexValue` (`$integer`/`$float`/`$bytes` JSON wrappers). |
| `crates/runner/` | Tenant-pinned sandbox cells, in-process toy program runner |
| `crates/v8cell/` | Real V8 isolate. Exposes `Aster.read` (legacy) **and** `Convex.asyncSyscall("1.0/get")` (v0.4) |
| `crates/ipc/` | Length-prefixed JSON over UDS. `aster_brokerd` + `aster_v8cell` binaries + the cross-process E2E test |
| `crates/host/` | In-process facade + benchmark binary + the `e2e.rs` + `crypto_and_v8.rs` smoke harnesses |

## Important docs

- `docs/ARCHITECTURE.md` — current architecture (v0.3 + v0.4 deltas)
- `docs/POSTGRES_ADAPTER_PLAN.md` — five-commit plan, all done as of v0.4
- `docs/CONVEX_POSTGRES_REFERENCE.md` — DDL, read SQL templates, 12 gotchas, verbatim from `get-convex/convex-backend`
- `docs/V8_QUESTION.md` — V8 experiment memo
- `docs/THEORY_REGISTER.md` — research theories
- `docs/ABSURD_IDEAS.md` — intentionally strange/falsifiable ideas
- `docs/COMPARISON_MATRIX_V0.3.md` — prior-art matrix
- `docs/SYNAPSE_MIGRATION_V0.3.md` — operator migration path
- `docs/LOCAL_VALIDATION.md` — what passed on the developer machine

## What this lets you demo today

- Spawn `aster-brokerd:0.4` against a real Convex Postgres deployment
  (point it at the same DB the upstream backend writes to). The broker
  reads `documents` rows directly — `snapshot_ts`, `read_point`, and
  `read_prefix` are wired and tested.
- Spawn `aster-v8cell:0.4` against the broker's socket. The cell can run
  hand-written JS that calls `await Convex.asyncSyscall("1.0/get",
  JSON.stringify({id: "<idv6_or_table_hex>/<id_hex>"}))` and gets the
  document bytes back as a JSON string.
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

- Running an `npx convex deploy`-bundled module. The cell only knows
  about an `async function main()` in a single source string; the
  module loader (#98) needs to read `_modules` + `_source_packages`
  and pull bundles from the modules-storage layer.
- HTTP requests against a running deployment. There is no
  `/api/query/<module>:<fn>` frontend yet; cells today are spawned
  directly via `aster_v8cell` with hand-written JS over IPC.
- Hostile multi-tenant isolation. The cell container runs as a
  non-root UID but doesn't yet have cgroups / seccomp / read-only
  rootfs / per-tenant UID separation.
