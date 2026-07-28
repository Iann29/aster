# TLC model-checking results — AsterFence

Model of the write plane's commit fence / GC / epoch failover interplay
(`crates/store-postgres/src/write_plane.rs`), the joint that referee finding
F8 (`paper/sources/aster-referee-report.md`) singles out as the one part of
the Capsule Transaction Theorem cryptography cannot cover. Spec:
`tla/AsterFence.tla`. All runs below are real TLC executions on the configs
committed next to it.

## Environment

- TLC2 Version 2.19 of 08 August 2024 (rev: 5a47802), `tla2tools.jar` from
  the tlaplus/tlaplus latest release (jar is gitignored, not committed)
- OpenJDK 21.0.11, Linux x86_64, 16 workers on 16 cores, 7099MB heap
- Bounds in every config: 2 committers, 2 keys, `MaxPos = 4`, `MaxEpoch = 3`,
  symmetry reduction on committers and keys, safety only (`-deadlock`
  disables the deadlock check — the bounded model intentionally runs out of
  enabled actions once the log and epoch bounds fill up)

Command shape (per config):

```
java -cp tla2tools.jar tlc2.TLC AsterFence -config <CONFIG>.cfg -workers auto -deadlock
```

## Invariants

| Name | Meaning | Theorem anchor |
|---|---|---|
| `I1_UniquePosition` | at most one append per (deployment, position) | A7 (globally increasing timestamps) |
| `I2_NoValidationAgainstPruned` | while a fence validates, no event of its window (s, h] has been pruned | Lemma R / Repair G-RETENTION |
| `I3_EpochBlockOrder` | epochs are non-decreasing along log positions — the observable content of "a stale-epoch holder never appends" | Lemma 3.11 / A7 / referee F1 |
| `I4_NoWriteSkew` | two transactions whose declared windows cover each other's writes, snapshots mutually older than the other's commit, never both commit | Counterexample 3.9 |
| `I4a_NoStaleValidatedRead` | no committed transaction has a committed write on a declared read inside (s, pos) — the general form I4 instantiates | Lemma 3.8 / A8 |

## Run 1 — positive (`AsterFence.cfg`): implemented semantics, all invariants PASS

`EpochReuse = FALSE`, `RetentionPinned = TRUE` — fresh epochs on every
acquisition, GC serialized with in-flight fences (the retention row lock).

```
Model checking completed. No error has been found.
  Estimates of the probability that TLC did not check all reachable states
  because two distinct states had the same fingerprint:
  calculated (optimistic):  val = 2.9E-6
  based on the actual fingerprints:  val = 7.5E-6
14948951 states generated, 5986803 distinct states found, 0 states left on queue.
The depth of the complete state graph search is 13.
The average outdegree of the complete state graph is 1 (minimum is 0, the maximum 31 and the 95th percentile is 4).
Finished in 01min 19s at (2026-07-16 06:55:00)
```

14,948,951 states generated / 5,986,803 distinct / depth 13. All six checked
invariants (`TypeOK`, I1, I2, I3, I4, I4a) hold.

## Run 2 — negative (`AsterFenceReuse.cfg`): epoch REUSE, the F1 violation — TLC finds it

`EpochReuse = TRUE` enables `AcquireLeaseReuse`: on failback the authority
reinstalls the returning committer's old epoch instead of bumping (per-holder
epoch bookkeeping, or lease state restored from a backup). The fence's V2
equality check still passes — the authority itself went back — and TLC finds
the Lemma 3.11 break:

```
Error: Invariant I3_EpochBlockOrder is violated.
```

Trace summary (9 states): `c1` acquires epoch 1, `c2` acquires epoch 2, `c1`
acquires epoch 3 (failover), `c1` commits log position 1 carrying epoch 3;
`AcquireLeaseReuse(c2)` reinstalls epoch 2 as the live lease epoch (failback
with reuse); `c2` passes the V2 check and commits position 2 carrying
epoch 2. The log now reads epoch 3 at position 1, epoch 2 at position 2 — a
stale-epoch commit ordered after a newer-epoch commit, exactly the case
Lemma 3.11's analysis excludes only when A7 says "strictly increasing, never
reused". Final two states of the TLC trace:

```
State 8: <FenceBegin line 154, col 5 to line 162, col 57 of module AsterFence>
/\ inflight = [epoch |-> 2, snap |-> 0, reads |-> {}, cmtr |-> c2, wkey |-> k1, h |-> 1]
/\ leaseEpoch = 2
...
State 9: <FenceCommit line 183, col 5 to line 189, col 52 of module AsterFence>
/\ log = { [pos |-> 1, key |-> k1, epoch |-> 3, snap |-> 0, reads |-> {}],
  [pos |-> 2, key |-> k1, epoch |-> 2, snap |-> 0, reads |-> {}] }

4758 states generated, 3138 distinct states found, 2196 states left on queue.
The depth of the complete state graph search is 10.
Finished in 00s at (2026-07-16 06:55:28)
```

The implementation forbids this by construction: `acquire_lease` bumps
`aster.lease.epoch` under the row lock (`epoch = aster.lease.epoch + 1`),
covered by `write_plane_it.rs::lease_epochs_strictly_increase_and_never_reuse`.

## Run 3 — negative (`AsterFenceNoPin.cfg`): retention pin dropped — TLC finds the Lemma R race

`RetentionPinned = FALSE` lets `AdvanceRetention` run while a fence is
between its coverage check and its append (the check-then-use race of Repair
G-RETENTION; the implementation forbids it by holding the `aster.retention`
row lock, proven live by
`write_plane_it.rs::gc_blocks_on_inflight_fence_and_enforces_coverage`).

```
Error: Invariant I2_NoValidationAgainstPruned is violated.
```

Trace summary (10 states): two commits write key `k1` at positions 1 and 2
(position 1 now shadowed); a fence begins at snapshot 0 with horizon h = 2,
passing the coverage check `floor <= snap`; GC then advances the floor to 2
mid-fence and prunes position 1 — an event inside the in-flight window
(0, 2] is gone before validation completes. Final state and stats:

```
State 10: <AdvanceRetention line 220, col 5 to line 227, col 47 of module AsterFence>
/\ inflight = [epoch |-> 3, snap |-> 0, reads |-> {}, cmtr |-> c2, wkey |-> k1, h |-> 2]
/\ pruned = {[pos |-> 1, key |-> k1, epoch |-> 3, snap |-> 0, reads |-> {}]}
/\ floor = 2
/\ log = {[pos |-> 2, key |-> k1, epoch |-> 3, snap |-> 0, reads |-> {}]}

17117 states generated, 11609 distinct states found, 9104 states left on queue.
The depth of the complete state graph search is 10.
Finished in 00s at (2026-07-16 06:55:30)
```

## Run 4 — finding (`AsterFenceNoPinSound.cfg`): shadowed-only compaction is defense in depth

Same unpinned model as run 3, with I2 removed to ask: does the pin failure
reach an actual serializability break (I4/I4a) in this implementation?

```
Model checking completed. No error has been found.
  Estimates of the probability that TLC did not check all reachable states
  because two distinct states had the same fingerprint:
  calculated (optimistic):  val = 1.1E-5
  based on the actual fingerprints:  val = 3.4E-6
29791231 states generated, 9627643 distinct states found, 0 states left on queue.
The depth of the complete state graph search is 13.
Finished in 01min 44s at (2026-07-16 06:59:09)
```

No violation in 29,791,231 states: I1, I3, I4, I4a all hold even with GC
racing mid-fence. Reason (visible in the model, true of the code):
`advance_retention` compacts only revisions **shadowed by a newer revision of
the same key at or below the watermark** — the newest revision of every key
survives any watermark. If a key has any write event in (s, h], its newest
event is also in (s, h] and is never pruned, so the V4 conflict scan always
keeps a live witness. Two consequences, stated honestly:

- Within this model, the current sweeper cannot cause a missed conflict even
  unpinned — the shadowed-only rule is an independent safety layer under the
  theorem's Lemma R argument, and it is also what keeps the log tip from
  regressing (I1: positions are never reused because the global newest entry
  is never compacted).
- The pin is still the property the theorem's proof consumes: Lemma R is
  stated for the general retention contract where ANY event at or below the
  floor may disappear (e.g. a future sweeper that truncates whole prefixes,
  or moves history to cold storage). Under that contract, run 3 is the
  counterexample and the pin is load-bearing. Dropping the pin because of
  run 4 would silently couple serializability to the compaction strategy.

## Verdict

- Positive model of the implemented semantics: **all invariants pass**
  (run 1).
- The model has teeth: switching on epoch reuse (referee F1) produces the
  Lemma 3.11 violation with a 9-state counterexample (run 2); dropping the
  Lemma R pin produces the coverage violation with a 10-state counterexample
  (run 3).

## Abstraction gap (what the model does NOT prove about the code)

Model checking here is against the DESIGN abstracted from
`write_plane.rs`; the following is assumed, not verified:

1. **Postgres primitives.** Row locks (`FOR UPDATE` on `aster.lease` and
   `aster.retention`) and transaction atomicity/rollback are modeled as
   action guards and as the begin/commit/abort split with at most one fence
   in flight. That Postgres actually delivers mutual exclusion, that the
   whole fence transaction is atomic (no partial append), and that
   `statement_timeout` aborts rather than wedges, is trusted (A5). Under
   those guards the model's FenceBegin+FenceCommit collapse to one atomic
   validate-then-append, which is the theorem's A8; the split exists only so
   the NoPin knob can re-enable the race the locks eliminate.
2. **One deployment.** All fence state is keyed by (tenant, deployment) in
   the schema; cross-deployment non-interference is by construction and is
   not modeled (A10 tenant confinement is out of scope here).
3. **Capsule authenticity is assumed, not modeled.** The adversary submits
   any (snapshot <= tip, read set, write key) — i.e. any capsule a broker
   could ever have issued, including rollback/replay of old ones. That a
   forged or spliced capsule cannot reach the fence is T1a's cryptographic
   reduction, deliberately outside this model. Document values do not exist
   in the model; observations are key-level, which is exactly the
   granularity V4 checks.
4. **Windows as key sets.** Point reads and sealed range windows both reduce
   to key membership (`ObservedWindow::contains`) over the bounded key
   space; an arbitrary `reads` subset subsumes every containment predicate
   two keys admit. Range-certificate semantics (boundary vs exhausted,
   next-key windows — referee F2) are validated by the Rust integration
   tests, not here.
5. **Single-key write sets.** The implementation appends N rows sharing
   `ts = h + 1`; the model commits one key per position, so I1 reads "one
   commit per position". The PK `(tenant, deployment, ts, key)` uniqueness
   within a multi-key commit is schema-enforced and not modeled.
6. **Committer epoch vs capsule context epoch.** V2 requires both to equal
   the live lease epoch; the model carries one value per fence because a
   capsule sealed under any other epoch fails the same equality — the
   collapse loses no behavior relevant to I1–I4a.
7. **Bounded instance.** 2 committers, 2 keys, 4 log positions, 3 epochs,
   with symmetry on committers and keys. TLC verifies these bounds
   exhaustively; the unbounded claim is the theorem's, not TLC's.
8. **No liveness.** Refusals (StaleEpoch, SnapshotBeyondHorizon,
   RetentionViolated) are modeled as disabled guards and aborts are always
   permitted; that an honest committer eventually commits is the theorem's
   obstruction-freedom remark and is not checked. One liveness observation
   from building the model: `advance_retention` accepts a watermark above
   the log tip, after which every snapshot fails coverage until new commits
   land — which cannot happen if all capsules are refused. The API relies on
   the sweeper passing sensible watermarks; worth a guard upstream someday.

## Reproducing

```
cd tla
curl -L -o tla2tools.jar https://github.com/tlaplus/tlaplus/releases/latest/download/tla2tools.jar
# Pinned toolchain (re-referee F17): verify before running —
#   sha256sum tla2tools.jar
#   936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88
java -cp tla2tools.jar tlc2.TLC AsterFence -config AsterFence.cfg -workers auto -deadlock            # PASS, ~80s
java -cp tla2tools.jar tlc2.TLC AsterFence -config AsterFenceReuse.cfg -workers auto -deadlock       # I3 violation, <1s
java -cp tla2tools.jar tlc2.TLC AsterFence -config AsterFenceNoPin.cfg -workers auto -deadlock       # I2 violation, <1s
java -cp tla2tools.jar tlc2.TLC AsterFence -config AsterFenceNoPinSound.cfg -workers auto -deadlock  # PASS, ~104s
java -cp tla2tools.jar tlc2.TLC AsterFenceNoAtomic -workers auto -deadlock                           # I4 violation, <1s
```

---

## Addendum 2026-07-16: retention clamp (liveness fix) re-verification

The liveness observation from the first modeling pass (an above-tip watermark
permanently wedges commit admission — safety unaffected) was fixed in
`write_plane.rs::advance_retention`: the applied watermark now clamps to the
log tip (`current.max(requested.min(tip))`, tip read under the same retention
row lock), with regression `write_plane_it.rs::retention_watermark_clamps_to_log_tip`.
`AdvanceRetention(w)` gained the matching guard `w <= Tip`, modeling the
post-clamp effect. All four configs re-run (TLC 2.19, Java 21, -workers auto
-deadlock, same commands as above):

| Config | Result | States generated / distinct |
|---|---|---|
| AsterFence (positive) | **No error** — all 6 invariants hold | 14,813,201 / 5,956,127 |
| AsterFenceReuse (F1 negative) | **Invariant I3_EpochBlockOrder violated** (depth 9) | 3,349 / 2,350 |
| AsterFenceNoPin (Lemma R negative) | **Invariant I2_NoValidationAgainstPruned violated** (depth 10) | 17,851 / 15,206 |
| AsterFenceNoPinSound (finding) | **No error** — shadowed-only compaction still sound unpinned | 22,175,377 / 7,333,215 |

State counts shrank versus the first pass (e.g. positive 14.9M→14.8M
generated, NoPinSound 29.8M→22.2M) exactly as expected: the Tip guard removes
the above-tip GC branches from the reachable space. Both negatives still
produce their violations, so the model kept its teeth after the change.

---

## Addendum 2026-07-17: the A-ATOMIC mutant + honest model scope (re-referee F10)

**Scope, stated exactly.** `AsterFence` encodes the lease-row lock as "at
most one in-flight fence" (`FenceBegin` requires `inflight = None`). TLC on
the positive model therefore checks the safety CONSEQUENCES of that
serialization — it does not verify that Rust/Postgres establish the
serialization itself; the SQL integration tests
(`write_plane_it.rs`) carry that weight. Any paper sentence claiming TLC
"checks validation/append atomicity" of the implementation is too strong
and has been corrected.

**The missing mutant, now present.** `AsterFenceNoAtomic.tla` removes the
mutual exclusion: up to two fences validate concurrently, each against the
horizon it captured at BEGIN, with the aster.log primary key `(pos, key)`
modeled faithfully (a same-position same-key second append is refused, as
the real INSERT would die on the PK; different keys at one position both
land). Run (TLC 2.19, same jar, `-workers auto -deadlock`):

| Config | Result | States generated / distinct |
|---|---|---|
| AsterFenceNoAtomic (A-ATOMIC negative) | **Invariant I4_NoWriteSkew violated** — the Counterexample 3.9 write skew, exactly as required | 1,484 / 1,231 |

Trace shape: both fences begin at the same tip with crossing declared
reads, the first append lands above the second fence's captured `h`, so the
second conflict scan cannot see it, and both commit. This is the anomaly
the lease-row lock exists to kill; the model now demonstrates the kill is
load-bearing rather than assuming it.
