# Migration path v0.2 for Convex Synapse operators

This document updates the v0.1 Synapse migration path for Aster Runner. It assumes Synapse remains the operator control plane and Convex remains the durable writer.

## What stays from v0.1

1. Aster is enabled per deployment, not globally.
2. Rollback is still `execution_plane=local` plus disabling the remote-runner/Funrun knob and restarting the affected backend.
3. Convex remains the committer; Aster never writes deployment data directly.
4. Operators still watch cell saturation, read-trap rate, stale snapshot rate, and commit wait time.
5. Initial rollout should start with one staging deployment and one query, one mutation, and one action smoke test.

## What changes in v0.2

### 1. Capsule seal key becomes an operator secret

Aster now needs a broker-side `ASTER_CAPSULE_SEAL_KEY` (32 bytes, hex/base64 in production packaging). Synapse should treat it like `SYNAPSE_STORAGE_KEY`: generate once, store in the install `.env`, back it up, and never log it.

Runbook:

- Generate during Aster install.
- Mount only into `aster-broker`, not cells.
- Rotate by draining cells, accepting old+new seals during a short overlap, then killing all old-seal cells.
- Include in backup documentation; losing it invalidates in-flight capsules but not Convex data.

### 2. Cells have explicit identities

Seals bind to `cell_id` and lease epoch. The supervisor must assign stable cell IDs for the cell lifetime and include the ID in broker hydrate requests.

Runbook:

- Cell IDs should be unique per deployment epoch, e.g. `<deployment>/<epoch>/<ordinal>`.
- If a cell is restarted, increment its incarnation/epoch or issue a new lease context.
- Broker rejects hydrate requests whose seal cell ID does not match the caller identity.

### 3. V8 continuation support is Promise-based

Aster v0.2 demonstrates real V8 read traps, but only at async host API boundaries. Synapse rollout should track Convex versions because the adapter depends on the function-runner syscall shape.

Runbook:

- Pin a tested `convex-backend` image/commit for Aster-enabled deployments.
- Before upgrading Convex, run the compatibility fixture once it exists: query + mutation + action through Aster.
- If compatibility fails, keep that deployment on local execution or the previous Convex image.

### 4. New health checks

Add operator-visible checks:

- `aster_broker_seal_verify_failures_total{reason}`
- `aster_v8_traps_total{deployment}`
- `aster_v8_pending_without_trap_total`
- `aster_capsule_wrong_cell_total`
- `aster_cell_reincarnations_total{reason}` (future supervisor work)

Synapse dashboard should surface seal failures as security alerts, not ordinary 500s.

## Suggested docker-compose primitives

Names are illustrative; the prototype does not ship production compose files yet.

```env
ASTER_ENABLED=true
ASTER_CAPSULE_SEAL_KEY=<32-byte random secret>
ASTER_BROKER_UDS=/run/aster/broker.sock
ASTER_CELL_SUPERVISOR_UDS=/run/aster/supervisor.sock
ASTER_MAX_TRAPS_PER_INVOCATION=64
ASTER_CAPSULE_MAX_BYTES=1048576
ASTER_CONVEX_COMPAT_COMMIT=<pinned convex-backend commit>
```

Cells should not receive database URLs or the seal key. They receive only their cell identity, deployment identity, trap budget, and broker socket path.

## Rollout sequence v0.2

1. Install Aster binaries but leave all deployments on local execution.
2. Generate and persist `ASTER_CAPSULE_SEAL_KEY`.
3. Start broker and supervisor. Confirm broker can build and seal a capsule for a staging deployment.
4. Start one V8 cell pinned to staging. Confirm a synthetic `Aster.read` V8 smoke test passes.
5. Enable Aster for one staging Convex deployment with a pinned backend version.
6. Run query/mutation/action smoke tests and confirm:
   - read traps resolve,
   - mutation commits still happen in Convex,
   - action effects produce fences/ledger rows,
   - wrong-cell seal replay is rejected in audit logs.
7. Increase to two cells. Verify scheduler/failover behavior.
8. Enable one low-risk production deployment.
9. Keep rollback command documented next to the feature flag.

## Emergency rollback

1. Set the deployment execution plane to `local` in Synapse.
2. Disable Convex remote-runner/Funrun environment variables for the deployment.
3. Restart the Convex backend container.
4. Leave Aster broker/cells running until in-flight requests drain, or kill them if seal failures suggest compromise.
5. Audit `aster_broker_seal_verify_failures_total` and Synapse audit logs before re-enabling.

No data repair is expected because Convex remained the only writer.

## Backup/restore additions

- Back up Aster config and seal keys with Synapse secrets.
- Do not back up cell caches; they are disposable.
- Broker LRU capsule caches are disposable.
- Effect ledger (future production action support) is durable and must be included in backups if action egress is enabled.

## Operator warnings

- Aster v0.2 prototype proves the V8 mechanism but is not a production sandbox. Do not expose it to hostile tenants without OS process isolation, cgroups, seccomp/gVisor, and a real broker process boundary.
- Seal failures may indicate a bug, stale cell, replay attempt, or compromise. Treat repeated failures as security incidents.
- If trap rate is high, adding cells may make latency worse by increasing broker pressure. Fix prewarm/scheduler locality first.
