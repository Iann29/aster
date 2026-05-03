# Migration path v0.3 for Convex Synapse operators

This updates the v0.2 operator notes for Aster Runner. The new v0.3 property is a real process boundary between broker and V8 cell over Unix-domain sockets.

## What stays from v0.2

1. Aster remains opt-in per deployment.
2. Convex remains the durable writer and rollback stays `execution_plane=local`.
3. `ASTER_CAPSULE_SEAL_KEY` is broker-only secret material.
4. Cells have explicit `cell_id` and `lease_epoch`.
5. V8 continuation support is Promise/syscall-based, not stack serialization.

## What changes in v0.3

### 1. Broker and cell are separate services

Prototype binaries:

- `aster_brokerd`: owns read authority and seal key.
- `aster_v8cell`: owns V8 and tenant JS execution.

Synapse packaging should mount the broker socket into cells but not mount database URLs or seal keys into cell containers/processes.

### 2. UDS path becomes an operator-visible boundary

Suggested environment:

```env
ASTER_BROKER_UDS=/run/aster/<deployment>/broker.sock
ASTER_CELL_ID=<deployment>/<epoch>/<ordinal>
ASTER_LEASE_EPOCH=<monotonic epoch>
```

The socket directory should be owned by the Aster supervisor/broker group with the narrowest permissions possible. v0.3 does not yet check `SO_PEERCRED`; production packaging must add that before hostile tenants.

### 3. New health checks

Add to the v0.2 list:

- `aster_broker_ipc_connections_total`
- `aster_broker_ipc_bad_frames_total`
- `aster_broker_ipc_frame_too_large_total`
- `aster_broker_ipc_peer_reject_total`
- `aster_broker_hydrate_replay_reject_total`
- `aster_cell_broker_io_errors_total`
- `aster_cell_pending_trap_seconds`

### 4. New rollout smoke

Before enabling a deployment, run the process-boundary fixture equivalent:

```bash
cargo test -p aster-ipc --test process_boundary -- --nocapture
```

Operator-visible success is a cell output like:

```json
{"output":42,"traps":1,"capsule_hash":14555800972481595658}
```

and a wrong-cell hydrate rejection in the test.

## Suggested docker-compose primitives

Illustrative only; this research artifact does not ship production compose files.

```yaml
services:
  aster-broker:
    image: synapse/aster-broker:local
    environment:
      ASTER_CAPSULE_SEAL_KEY: ${ASTER_CAPSULE_SEAL_KEY}
      ASTER_BROKER_UDS: /run/aster/broker.sock
    volumes:
      - aster-run:/run/aster
      # database/read snapshot credentials mount here only

  aster-cell:
    image: synapse/aster-cell:local
    environment:
      ASTER_BROKER_UDS: /run/aster/broker.sock
      ASTER_CELL_ID: ${ASTER_CELL_ID}
      ASTER_LEASE_EPOCH: ${ASTER_LEASE_EPOCH}
    volumes:
      - aster-run:/run/aster:ro
      # no DB URL, no seal key
```

## Rollout sequence v0.3

1. Generate/persist broker seal key.
2. Start `aster-broker` and verify socket creation.
3. Start one cell for staging with no DB credentials in its environment.
4. Run synthetic V8 read-trap smoke through UDS.
5. Verify wrong-cell and wrong-epoch hydrate attempts are rejected.
6. Enable one staging Convex query once the Convex syscall fixture exists.
7. Add cgroups/seccomp/namespaces before hostile or multi-tenant production exposure.

## Emergency rollback

Same as v0.2: set the deployment execution plane to local and restart the Convex backend. If broker IPC rejects spike, kill cells first, preserve broker logs, and rotate the capsule seal key before re-enabling.

## Operator warning

v0.3 is a real boundary but not a production sandbox. A same-host attacker who can connect to the broker socket can still attempt parser and replay attacks. Treat UDS permissions and future peer-credential checks as part of the security boundary.
