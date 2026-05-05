# Aster v0.6 architecture

Status: runnable research prototype, not a production sandbox. The arrangement
proves the core authority split for self-hosted Convex execution: tenant
JavaScript runs in a V8 cell with no database credentials, while a broker
process owns read authority and hydrates sealed snapshot capsules over a
Unix-domain socket. As of v0.6, real `npx convex deploy` bundles execute
end-to-end against real Postgres through this split.

The archived design trail lives in `docs/ARCHITECTURE_V0.{1,2,3}.md`. This
file describes the current mainline shape.

## Runtime shape

```text
Synapse / operator
      |
      | create kind=aster deployment
      v
aster_brokerd container  (long-lived per deployment)
      |  UDS: /run/aster/broker.sock
      |  + capsule seal key, store backend, modules dir
      v
aster_v8cell one-shot container  (per invocation)
      |
      | tenant JS: real `npx convex deploy` ESM bundle
      |   <export>.invokeQuery(args_json)
      |   driving Convex.asyncSyscall("1.0/get") trap loop
      v
stdout JSON envelope
{"output": "...", "traps": N, "capsule_hash": ...}
```

`aster_brokerd` is long-lived per deployment. It owns the capsule seal key,
the store backend, hydrate policy, the modules-storage directory, and the UDS
listener. `aster_v8cell` is one-shot per invocation. It owns a real V8 isolate,
receives module identity through `ASTER_MODULE_PATH` + `ASTER_FUNCTION_NAME` +
`ASTER_ARGS_JSON` (or raw source through `ASTER_JS` / `ASTER_JS_INLINE` for
the legacy direct path), opens the broker socket, compiles the bundle as an
ES module, calls the named query export, and exits after printing
`{"output":...,"traps":...,"capsule_hash":...}`.

The cell process never receives Postgres credentials or the seal key. Its only
data path is the broker socket plus sealed capsules.

## Store backends

Brokerd chooses its read backend at startup:

- `ASTER_STORE=memory` (default): deterministic in-memory `MvccStore` seeded by
  `ASTER_SEED_I64`. Used by the basic Docker smoke and the cross-process
  boundary tests.
- `ASTER_STORE=postgres`: `PostgresCapsuleStore` reads the same Convex Postgres
  schema the upstream backend writes to. Configure with `ASTER_DB_URL_FILE` or
  `ASTER_DB_URL`; `ASTER_DB_SCHEMA` defaults to `public`. `ASTER_MODULES_DIR`
  points at Convex's modules storage directory (so the broker can resolve
  bundle bytes from the local filesystem).

The Postgres store currently implements:

- `snapshot_ts()`: latest safe read timestamp from `documents` plus
  `persistence_globals['max_repeatable_ts']`.
- `read_point()`: latest non-deleted document revision at or before the
  requested timestamp.
- `read_prefix()`: bounded prefix reads for the Aster-native API.
- Convex IDv6 decoding and `_tables`-backed `table_number -> tablet_uuid`
  mapping, so a JS `db.get(id)` string can resolve without the cell knowing
  tablet UUIDs.
- ConvexValue JSON codecs for the `$integer` / `$float` / `$bytes` wire shape.
- Module metadata lookup: `_modules` joined to `_source_packages`.
- Local-FS module bundle bytes via
  `PostgresCapsuleStore::load_module_bundle(path)`.

The store returns document bodies to the v8cell as raw JSON strings under the
internal `_raw` field. That keeps the current `Convex.asyncSyscall("1.0/get")`
path small while the fuller Convex runtime surface is built.

## Cell API

The V8 global exposes two read surfaces and one entry surface:

- `Aster.read(key, field)`: legacy toy API used by v0.2/v0.3 tests.
- `Convex.asyncSyscall(name, argsJson)`: Convex-shaped syscall shim. Today
  `name == "1.0/get"` is implemented; unsupported syscall names reject the
  Promise with a typed error.
- `Convex.syscall(name, argsJson)`: synchronous variant, present so bundles
  that touch sync syscalls don't crash on undefined.

Both async read surfaces use the same continuation mechanism: a cold read
stores the V8 `PromiseResolver`, emits a typed trap, asks the broker to
hydrate the capsule, resolves the Promise, runs a microtask checkpoint, and
lets the original async JS frame continue.

### Module-query entry path (v0.6)

`V8SandboxCell::execute_module_query_with_broker` is the v0.6 entry point.
Given a sealed capsule, a module path (e.g. `messages.js`), an export name
(e.g. `getById`), and an `args_json` string, it:

1. Asks the broker for module bundle bytes via `LoadModuleBundle` IPC (the
   broker enforces capsule-gated access; the cell never reads the modules
   directory directly).
2. Unzips the bundle in-process and resolves `modules/<path>.js` first,
   matching Convex's on-disk layout, falling back to `<path>.js`.
3. Compiles the source as a V8 ES module. The bundle's own `convex/server`,
   `convex/values`, and `_generated/*` are already inlined by `esbuild` at
   `npx convex deploy` time, so the cell does not need to provide those
   modules at runtime.
4. Locates the named export, asserts `isQuery === true` (mutation and action
   exports are rejected with a typed error in v0.6), and calls
   `<export>.invokeQuery(args_json)`.
5. Drives the trap loop: each `Convex.asyncSyscall("1.0/get")` Promise from
   the user's `db.get(id)` is paired with a broker `HydratePoint` request,
   resolved with the document JSON, and the JS frame continues.
6. Returns the resolved query value, the trap count, and the capsule hash.

The library-level lock for this path is
`crates/v8cell/tests/module_loader.rs::module_get_by_id_through_fake_broker_returns_doc`,
which runs the byte-for-byte 58 KB output of `npx convex deploy` for
`aster-e2e-fixture/messages.ts` and asserts the seeded document round-trips
with `name`, `body`, `_id` intact and exactly **1** trap drained.

## IPC protocol

`crates/ipc` uses a deliberately small UDS protocol:

1. u32 big-endian frame length.
2. JSON payload.
3. hard cap at `MAX_FRAME_BYTES = 1 MiB`.
4. one request per connection.
5. typed success or `WireBrokerError { code, message }`.

Implemented requests are `InitialCapsule`, `HydratePoint`, `LoadModuleBundle`,
and `Shutdown`. `LoadModuleBundle` deliberately requires a sealed capsule and
seal context, so the broker reuses the existing capsule authority before
serving JS bytes. Capsule hydration and module loading remain conceptually
separate: loading JS bytes is not a document read trap, and the module bytes
are not encoded into the capsule.

## Synapse boundary

Synapse (in `Iann29/convex-synapse`) owns the operator-facing control plane:

- `kind: "aster"` on deployment creation.
- Docker provisioning for `aster-brokerd:0.4` (long-lived) and one-shot
  `aster-v8cell:0.4` containers per invocation.
- Delete/status dispatch for Aster broker containers.
- `POST /v1/deployments/{name}/aster/invoke` accepts two body shapes:
    - **Module mode** (v0.6): `{modulePath, functionName, argsJson}` —
      Synapse forwards to the cell with `ASTER_MODULE_PATH` /
      `ASTER_FUNCTION_NAME` / `ASTER_ARGS_JSON` envs, then returns the
      cell's stdout/stderr. This is the path that runs real `npx convex
      deploy` bundles end-to-end.
    - **Raw-JS mode** (legacy): `{js}` — Synapse forwards the source
      directly via `ASTER_JS_INLINE`. Useful for diagnostics.
- Honest HTTP proxy behavior: `/d/{name}/*` returns `501 aster_not_proxied`
  until a Convex-shaped Aster HTTP frontend exists.

End-to-end Synapse-driven smoke against a real VPS is captured in
`Iann29/convex-synapse:docs/ASTER_VPS_SMOKE.md`. Wider integration runbook
is in `Iann29/convex-synapse:docs/ASTER_INTEGRATION.md`.

## What works today

- Docker images build for broker and cell.
- Broker/cell process separation works over UDS, with capsule seals binding
  every hydrated capsule to `(cell_id, lease_epoch)`.
- Memory-store smoke returns `42` with one read trap.
- Postgres-store smoke reads a Convex-shaped document through
  `Convex.asyncSyscall("1.0/get")`.
- IDv6 strings resolve through `_tables` without the cell ever seeing tablet
  UUIDs.
- ConvexValue wire-shape parsing/encoding is tested.
- Module metadata and local module bundle bytes resolve inside the Postgres
  store via `_modules` × `_source_packages` join + local-FS adapter.
- Brokerd serves module bundle bytes over `LoadModuleBundle` IPC when
  `ASTER_STORE=postgres` and `ASTER_MODULES_DIR` points at Convex's modules
  storage directory.
- **Real `npx convex deploy` bundles execute end-to-end inside the cell.**
  V8 ESM compile + `<export>.invokeQuery(args_json)` dispatch + `Convex.{
  syscall, asyncSyscall }` globals + `db.get(id)` trap drain → Postgres
  document round-trip. Locked by `crates/v8cell/tests/module_loader.rs` and
  by `docker/smoke-bundle.sh 0.4-modulequery` against `postgres:16`.
- Synapse can create/delete a `kind=aster` deployment, invoke raw JS, and
  invoke a real bundle by `(modulePath, functionName, argsJson)`.
  VPS-validated against a Hetzner CPX22.

## What does not work yet

- **Mutations and actions.** The cell rejects `isMutation === true` and
  `isAction === true` exports with a typed error. v0.6 is read-only on
  purpose; the OCC commit story lands separately so its review surface is
  isolated.
- **Convex-shaped HTTP frontend.** `/api/query/<module>:<fn>` and the
  upstream Convex CLI / client URL shape are not served by the cell or the
  broker. Synapse exposes module-mode invocation at
  `aster/invoke`, not at the Convex-CLI URL shape.
- **Cell warm pooling.** Every invocation spawns a fresh container. There is
  no reuse, no warm isolate cache, no socket-per-invocation reincarnation.
  Roadmap item under `docs/ABSURD_IDEAS.md`.
- **Production multi-tenant hardening.** The cell container runs as a
  non-root UID but does not yet have cgroups, seccomp filters, a read-only
  rootfs, or per-tenant UID separation. UDS peer-credential checks, IPC
  read/write timeouts, and tokio async on the broker accept loop are P2.
- **Resource budgets and metrics.** No CPU / wall-clock / memory limits per
  invocation, no Prometheus metrics, no graceful-shutdown story.
- **Per-deployment source binding inside Synapse.** Today
  `SYNAPSE_ASTER_POSTGRES_URL` / `SYNAPSE_ASTER_DB_SCHEMA` /
  `SYNAPSE_ASTER_MODULES_DIR` are process-level config. A production
  surface needs a durable "this Aster deployment mirrors that Convex
  deployment" record per row.

## Security line

The security claim demonstrated in v0.6 is narrow but real: the cell process
executes tenant JS, loads tenant module bundles, and receives hydrated
document bytes without ever holding database credentials, the seal key, or a
direct path to the modules directory. Every read passes through a sealed
capsule whose MAC is keyed on `(cell_id, lease_epoch)`, so a cell cannot
forge or replay another cell's capsule.

That property is meaningful for trust boundaries between the executor and the
authority, but it is not yet sufficient for hostile multi-tenant production.
A production deployment still needs kernel-level cell sandboxing
(cgroups + seccomp + read-only rootfs + per-tenant UID), stricter IPC
authentication (peer credential checks on the UDS), per-invocation resource
budgets, fuzzing of the IPC and bundle-unzip surfaces, and a durable effect
story before tenant mutations or external egress are allowed.
