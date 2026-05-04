# Aster Runner v0.5 architecture

Status: runnable research prototype, not a production sandbox. Aster proves
the core authority split for self-hosted Convex execution: tenant JavaScript
runs in a V8 cell with no database credentials, while a broker process owns
read authority and hydrates sealed snapshot capsules over a Unix-domain socket.

The archived design trail lives in `docs/ARCHITECTURE_V0.{1,2,3}.md`. This
file describes the current mainline shape.

## Runtime shape

```text
Synapse / operator
      |
      | create kind=aster deployment
      v
aster_brokerd container
      |  UDS: /run/aster/broker.sock
      v
aster_v8cell one-shot container
      |
      | tenant JS: async function main()
      v
stdout JSON envelope
```

`aster_brokerd` is long-lived per deployment. It owns the capsule seal key,
store backend, hydrate policy, and UDS listener. `aster_v8cell` is one-shot per
invocation. It owns a real V8 isolate, receives JS source through `ASTER_JS` or
`ASTER_JS_INLINE`, opens the broker socket, and exits after printing
`{"output":...,"traps":...,"capsule_hash":...}`.

The cell process never receives Postgres credentials. Its only data path is the
broker socket plus sealed capsules.

## Store backends

Brokerd chooses its read backend at startup:

- `ASTER_STORE=memory` (default): deterministic in-memory `MvccStore` seeded by
  `ASTER_SEED_I64`. Used by the basic Docker smoke and old process-boundary
  tests.
- `ASTER_STORE=postgres`: `PostgresCapsuleStore` reads the same Convex Postgres
  schema the upstream backend writes to. Configure with `ASTER_DB_URL_FILE` or
  `ASTER_DB_URL`; `ASTER_DB_SCHEMA` defaults to `public`.

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

The store still returns document bodies to the v8cell as raw JSON strings under
the internal `_raw` field. That keeps the current `Convex.asyncSyscall("1.0/get")`
path small while the fuller Convex runtime surface is built.

## Cell API

The V8 global exposes two read surfaces:

- `Aster.read(key, field)`: legacy toy API used by v0.2/v0.3 tests.
- `Convex.asyncSyscall(name, argsJson)`: Convex-shaped syscall shim. Today
  `name == "1.0/get"` is implemented; unsupported syscall names reject the
  Promise with a typed error.

Both surfaces use the same continuation mechanism: a cold read stores the V8
`PromiseResolver`, emits a typed trap, asks the broker to hydrate the capsule,
resolves the Promise, runs a microtask checkpoint, and lets the original async
JS frame continue.

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
serving JS bytes. Keep capsule hydration and module loading conceptually
separate; loading JS bytes is not a document read trap.

## Synapse boundary

Synapse owns the operator-facing control plane:

- `kind: "aster"` on deployment creation.
- Docker provisioning for `aster-brokerd:0.4`.
- delete/status dispatch for Aster broker containers.
- `POST /v1/deployments/{name}/aster/invoke`, which spawns
  `aster-v8cell:0.4` against the brokerd UDS volume and returns stdout/stderr.
- honest HTTP proxy behavior: `/d/{name}/*` returns `501 aster_not_proxied`
  until a Convex-shaped Aster HTTP frontend exists.

Current Synapse docs live in
`Iann29/convex-synapse:docs/ASTER_INTEGRATION.md`.

## What works today

- Docker images build for broker and cell.
- Broker/cell process separation works over UDS.
- Memory-store smoke returns `42` with one read trap.
- Postgres-store smoke reads a Convex-shaped document through
  `Convex.asyncSyscall("1.0/get")`.
- IDv6 strings resolve through `_tables`.
- ConvexValue wire-shape parsing/encoding is tested.
- Module metadata and local module bundle bytes can be resolved inside the
  Postgres store.
- Brokerd exposes module bundle bytes over IPC when `ASTER_STORE=postgres`
  and `ASTER_MODULES_DIR` points at Convex's modules storage directory.
- Synapse can create/delete a `kind=aster` deployment and invoke raw JS through
  a one-shot v8cell.

## What does not work yet

- Running an `npx convex deploy` bundled module directly in the cell.
- Unzipping bundle bytes and instantiating V8 ESM modules with Convex shims
  such as `convex/server`, `convex/values`, and `_generated/api`.
- Routing `module:function(args)` to the correct export.
- Serving `/api/query/<module>:<fn>` or other Convex-shaped HTTP endpoints.
- Production multi-tenant hardening: cgroups, seccomp, read-only rootfs,
  per-tenant UID, peer credential checks, request sequencing, and key rotation.

## Security line

The security claim demonstrated so far is narrow: the cell process can execute
tenant JS and receive hydrated bytes without direct database credentials or seal
key custody. That is meaningful, but not sufficient for hostile multi-tenant
production. A production deployment still needs kernel sandboxing, stricter IPC
authentication, resource budgets, fuzzing, and a durable effect story before
tenant actions or external egress are allowed.
