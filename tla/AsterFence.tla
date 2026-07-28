------------------------------ MODULE AsterFence ------------------------------
(***************************************************************************)
(* TLA+ model of Aster's write plane: the lease authority, the commit-log  *)
(* positions, the single-transaction commit fence, and retention/GC — the  *)
(* semantics implemented in crates/store-postgres/src/write_plane.rs and   *)
(* exercised by crates/store-postgres/tests/write_plane_it.rs.             *)
(*                                                                         *)
(* Why this model exists: referee finding F8 (paper/sources/               *)
(* aster-referee-report.md) names the fence + GC pin + epoch failover      *)
(* interplay as the theorem's weakest joint — the one part cryptography    *)
(* does not cover. This module model-checks that interplay against the     *)
(* DESIGN abstracted from the implementation. The abstraction gap is       *)
(* stated in tla/RESULTS.md.                                               *)
(*                                                                         *)
(* Implementation <-> model map (write_plane.rs):                          *)
(* - acquire_lease: INSERT..ON CONFLICT bumps aster.lease.epoch under the  *)
(*   row lock -> AcquireLease (epochs strictly increasing, never reused;   *)
(*   failover A->B->A takes a fresh epoch — theorem A7 / referee F1).      *)
(* - commit(): one Postgres transaction whose FIRST statement locks the    *)
(*   lease row FOR UPDATE, so fences are mutually exclusive and            *)
(*   acquire_lease blocks while a fence is in flight. Modeled as           *)
(*   FenceBegin (epoch check V2, horizon read, coverage check V3 — the     *)
(*   retention row lock is taken here) followed by FenceCommit/FenceAbort  *)
(*   (conflict scan V4 + append). The lease lock is modeled by allowing    *)
(*   at most one in-flight fence and disabling AcquireLease/GC while one   *)
(*   exists; under those guards Begin+Commit collapse to ONE atomic        *)
(*   action, which is exactly what the row locks buy the implementation    *)
(*   (Lemma 3.10). The split exists so the negative knob RetentionPinned   *)
(*   = FALSE can re-enable GC mid-fence and show WHY the pin is needed.    *)
(* - advance_retention: watermark never lowers; compaction deletes only    *)
(*   revisions at or below the watermark that are SHADOWED by a newer      *)
(*   revision of the same key at or below it -> AdvanceRetention.          *)
(*   Serialized with fences via the retention row lock (Lemma R pin).     *)
(*                                                                         *)
(* Rejection outcomes (StaleEpoch, SnapshotBeyondHorizon,                  *)
(* RetentionViolated) change no durable state, so they appear as disabled  *)
(* FenceBegin guards rather than as actions. FenceAbort covers both a V4   *)
(* conflict and any mid-transaction rollback (statement_timeout, crash):   *)
(* Postgres discards the whole transaction, so an aborted fence leaves no  *)
(* trace. Policy (V5) and document values are out of model — T1a           *)
(* authenticity is the theorem's cryptographic half; here the adversary    *)
(* simply picks any (snapshot, reads, write) a capsule could carry.        *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
    Committers,      \* symmetry set: committer/broker identities
    Keys,            \* symmetry set: document keys
    MaxPos,          \* bound on commit-log positions (aster.log ts)
    MaxEpoch,        \* bound on lease epochs
    None,            \* model value: no fence in flight
    EpochReuse,      \* BOOLEAN. FALSE = A7/F1 respected. TRUE = negative
                     \* model: failback reinstalls an old epoch.
    RetentionPinned  \* BOOLEAN. TRUE = fence holds the retention row lock
                     \* through append (Lemma R pin). FALSE = negative
                     \* model: GC may run mid-fence (check-then-use race).

ASSUME /\ MaxPos \in Nat /\ MaxPos >= 1
       /\ MaxEpoch \in Nat /\ MaxEpoch >= 1
       /\ EpochReuse \in BOOLEAN
       /\ RetentionPinned \in BOOLEAN

VARIABLES
    leaseEpoch, \* aster.lease.epoch at the authority (0 = never acquired)
    held,       \* [Committers -> 0..MaxEpoch]: epoch each committer last
                \* acquired (its e_now; 0 = never held)
    log,        \* set of live Entry records (aster.log)
    pruned,     \* entries removed by GC — history kept for the invariants
    floor,      \* aster.retention.low_watermark (g)
    inflight    \* None, or the fence between its BEGIN and COMMIT/abort

vars == <<leaseEpoch, held, log, pruned, floor, inflight>>

(***************************************************************************)
(* One log entry per commit: the implementation appends N rows sharing     *)
(* ts = h+1; the model's single-key write set makes position uniqueness    *)
(* (I1) mean "at most one commit per position". snap/reads are carried on  *)
(* the entry purely so the history invariants can see what the committed   *)
(* transaction declared (theorem Section 1.10).                            *)
(***************************************************************************)
Entry == [pos: 1..MaxPos, key: Keys, epoch: 1..MaxEpoch,
          snap: 0..MaxPos, reads: SUBSET Keys]

Fence == [cmtr: Committers, epoch: 1..MaxEpoch, snap: 0..MaxPos,
          reads: SUBSET Keys, wkey: Keys, h: 0..MaxPos]

\* Committed history: live rows plus everything GC compacted away.
All == log \cup pruned

Max(S) == CHOOSE x \in S : \A y \in S : y <= x

(***************************************************************************)
(* Log tip, derived from LIVE rows exactly like the implementation's       *)
(* SELECT COALESCE(MAX(ts), 0). GC never removes the newest revision of a  *)
(* key, so the tip cannot regress — I1 polices that this derivation and    *)
(* the h captured at FenceBegin agree.                                     *)
(***************************************************************************)
Tip == IF log = {} THEN 0 ELSE Max({e.pos : e \in log})

Init ==
    /\ leaseEpoch = 0
    /\ held = [c \in Committers |-> 0]
    /\ log = {}
    /\ pruned = {}
    /\ floor = 0
    /\ inflight = None

(***************************************************************************)
(* Lease acquisition (write_plane.rs::acquire_lease). Every acquisition    *)
(* bumps the epoch under the lease row lock: strictly increasing, never    *)
(* reused, even across failback A->B->A (theorem A7, referee F1). The      *)
(* UPDATE waits on an in-flight fence's row lock, which is exactly the     *)
(* failover ordering Lemma 3.11 requires — modeled as inflight = None.     *)
(***************************************************************************)
AcquireLease(c) ==
    /\ inflight = None
    /\ leaseEpoch < MaxEpoch
    /\ leaseEpoch' = leaseEpoch + 1
    /\ held' = [held EXCEPT ![c] = leaseEpoch + 1]
    /\ UNCHANGED <<log, pruned, floor, inflight>>

(***************************************************************************)
(* NEGATIVE MODEL ONLY (EpochReuse = TRUE): the F1 violation. A failback   *)
(* A->B->A where the authority reinstalls A's old epoch instead of         *)
(* bumping — e.g. a per-holder epoch table or lease state restored from    *)
(* backup. The committer's own epoch check (V2) still passes because the   *)
(* authority itself went back, which is precisely why A7 must say          *)
(* "strictly increasing, never reused" in the spec.                        *)
(***************************************************************************)
AcquireLeaseReuse(c) ==
    /\ EpochReuse
    /\ inflight = None
    /\ held[c] > 0
    /\ held[c] < leaseEpoch
    /\ leaseEpoch' = held[c]
    /\ UNCHANGED <<held, log, pruned, floor, inflight>>

(***************************************************************************)
(* Fence entry (write_plane.rs::commit, first half of the transaction):    *)
(* lease row locked FOR UPDATE (at most one fence in flight; acquire and   *)
(* GC-vs-pin guards elsewhere), V2 epoch equality, stable horizon read,    *)
(* V3 coverage against the retention watermark — locking the retention     *)
(* row, i.e. taking the Lemma R pin.                                       *)
(*                                                                         *)
(* The capsule is adversarial: any snapshot of a committed prefix          *)
(* (s <= Tip — positions are contiguous, so every such s was once the      *)
(* tip; S-SNAPSHOT rejections need no modeling), any declared read set,    *)
(* any authorized write key. committer_epoch and context_epoch must BOTH   *)
(* equal the live lease epoch in the implementation; the model's single    *)
(* guard held[c] = leaseEpoch is the same check because a capsule sealed   *)
(* under any other epoch fails it identically.                             *)
(***************************************************************************)
FenceBegin(c, s, R, w) ==
    /\ inflight = None
    /\ Tip < MaxPos                \* state bound: log has room to append
    /\ held[c] = leaseEpoch        \* V2 (else CommitOutcome::StaleEpoch)
    /\ held[c] > 0
    /\ s <= Tip                    \* else CommitOutcome::SnapshotBeyondHorizon
    /\ floor <= s                  \* V3 coverage g <= s (else RetentionViolated)
    /\ inflight' = [cmtr |-> c, epoch |-> held[c], snap |-> s,
                    reads |-> R, wkey |-> w, h |-> Tip]
    /\ UNCHANGED <<leaseEpoch, held, log, pruned, floor>>

(***************************************************************************)
(* V4: a committed write in (s, h] that intersects a declared observation  *)
(* forces CommitOutcome::Conflict. Point reads and range windows both      *)
(* reduce to key membership over the bounded key space (ObservedWindow::   *)
(* contains), so `reads` covers both forms.                                *)
(***************************************************************************)
WindowConflict(f) ==
    \E e \in log : e.key \in f.reads /\ f.snap < e.pos /\ e.pos <= f.h

(***************************************************************************)
(* Fence completion: conflict scan over the live log, then the atomic      *)
(* append at c = h + 1. Appending at inflight.h + 1 rather than Tip + 1    *)
(* is deliberate — the implementation reads h once, inside the             *)
(* transaction; the locks are what make the two equal, and I1 fails if a   *)
(* model change ever lets them diverge. No epoch re-check is needed at     *)
(* commit for the same reason: acquire_lease blocks on the lease row       *)
(* while a fence is in flight (Lemma 3.10/3.11).                           *)
(***************************************************************************)
FenceCommit ==
    /\ inflight # None
    /\ ~WindowConflict(inflight)
    /\ log' = log \cup {[pos |-> inflight.h + 1, key |-> inflight.wkey,
                         epoch |-> inflight.epoch, snap |-> inflight.snap,
                         reads |-> inflight.reads]}
    /\ inflight' = None
    /\ UNCHANGED <<leaseEpoch, held, pruned, floor>>

(***************************************************************************)
(* Any in-flight fence may abort: a V4 conflict, a statement_timeout, or   *)
(* a committer crash. Postgres rolls the whole transaction back, so an     *)
(* abort leaves no durable trace — allowing it unconditionally is the      *)
(* sound over-approximation.                                               *)
(***************************************************************************)
FenceAbort ==
    /\ inflight # None
    /\ inflight' = None
    /\ UNCHANGED <<leaseEpoch, held, log, pruned, floor>>

(***************************************************************************)
(* GC (write_plane.rs::advance_retention): raise the watermark (never      *)
(* lower) and compact revisions at or below it that are shadowed by a      *)
(* newer revision of the same key also at or below it. The newest          *)
(* revision of every key survives, so reads at covered snapshots and the   *)
(* tip derivation stay intact.                                             *)
(*                                                                         *)
(* The guard (RetentionPinned => inflight = None) is the retention row     *)
(* lock: an in-flight fence holds it from its coverage check through      *)
(* append, so the sweeper blocks (write_plane_it.rs::                      *)
(* gc_blocks_on_inflight_fence_and_enforces_coverage). With the pin        *)
(* disabled, GC interposes between validation-entry and append — the       *)
(* check-then-use race of theorem Repair G-RETENTION / Lemma R.            *)
(***************************************************************************)
Shadowed(e, w) ==
    \E n \in log : n.key = e.key /\ n.pos > e.pos /\ n.pos <= w

AdvanceRetention(w) ==
    /\ (RetentionPinned => inflight = None)
    /\ w > floor
    /\ w <= MaxPos
    \* Code clamp (write_plane.rs): the applied watermark never exceeds the
    \* log tip — an above-tip watermark would wedge commit admission forever
    \* (floor never lowers). Guarding w <= Tip models the post-clamp effect.
    /\ w <= Tip
    /\ floor' = w
    /\ LET dead == {e \in log : e.pos <= w /\ Shadowed(e, w)}
       IN /\ log' = log \ dead
          /\ pruned' = pruned \cup dead
    /\ UNCHANGED <<leaseEpoch, held, inflight>>

Next ==
    \/ \E c \in Committers : AcquireLease(c)
    \/ \E c \in Committers : AcquireLeaseReuse(c)
    \/ \E c \in Committers, s \in 0..MaxPos, R \in SUBSET Keys, w \in Keys :
           FenceBegin(c, s, R, w)
    \/ FenceCommit
    \/ FenceAbort
    \/ \E w \in 1..MaxPos : AdvanceRetention(w)

Spec == Init /\ [][Next]_vars

Symm == Permutations(Committers) \union Permutations(Keys)

--------------------------------------------------------------------------------
(***************************************************************************)
(* Invariants                                                              *)
(***************************************************************************)

TypeOK ==
    /\ leaseEpoch \in 0..MaxEpoch
    /\ held \in [Committers -> 0..MaxEpoch]
    /\ log \subseteq Entry
    /\ pruned \subseteq Entry
    /\ floor \in 0..MaxPos
    /\ inflight \in Fence \union {None}

(***************************************************************************)
(* I1 — at most one append per (deployment, position). The implementation  *)
(* leans on the aster.log primary key plus "fresh c = h + 1 under the      *)
(* lease lock"; here it checks that no interleaving of fences, failovers   *)
(* and GC ever computes a duplicate position (in particular, that GC can   *)
(* never make the tip regress — only shadowed revisions are compacted).    *)
(***************************************************************************)
I1_UniquePosition ==
    \A e1, e2 \in All : e1.pos = e2.pos => e1 = e2

(***************************************************************************)
(* I2 — Lemma R soundness: while a fence is validating, no event of its    *)
(* validation window (s, h] has been pruned. V4's decision is only sound   *)
(* if it sees a COMPLETE representation of (s, h] (theorem Lemma 2.11);    *)
(* monotone floor + prune-only-below-floor + the coverage check g <= s     *)
(* under the retention pin are what make this inductive.                   *)
(***************************************************************************)
I2_NoValidationAgainstPruned ==
    inflight # None =>
        \A e \in pruned : ~(inflight.snap < e.pos /\ e.pos <= inflight.h)

(***************************************************************************)
(* I3 — a stale-epoch holder never appends, stated as its observable       *)
(* history-level content (Lemma 3.11, epoch block order): epochs are       *)
(* non-decreasing along log positions. The guard-level form "append only   *)
(* with the live lease epoch" is unfalsifiable by construction (V2 is a    *)
(* fence guard) — but with epoch REUSE the guard still passes while THIS   *)
(* invariant breaks, which is exactly referee finding F1: A7 must demand   *)
(* strictly-increasing, never-reused epochs for Lemma 3.11's case          *)
(* analysis to be exhaustive.                                              *)
(***************************************************************************)
I3_EpochBlockOrder ==
    \A e1, e2 \in All : e1.pos < e2.pos => e1.epoch <= e2.epoch

(***************************************************************************)
(* I4 — no write-skew history (theorem Counterexample 3.9): two            *)
(* transactions whose declared windows cover each other's writes, both     *)
(* with snapshots older than the other's commit, cannot both land in the   *)
(* log. This is the anomaly an unlocked check-then-append would admit.     *)
(***************************************************************************)
I4_NoWriteSkew ==
    \A t1, t2 \in All :
        ~(/\ t1 # t2
          /\ t1.key \in t2.reads
          /\ t2.key \in t1.reads
          /\ t1.snap < t2.pos
          /\ t2.snap < t1.pos)

(***************************************************************************)
(* I4a — the general form I4 is an instance of (Lemma 3.8, the heart of    *)
(* the T2 induction): a committed transaction never validated a stale      *)
(* observation — no committed write on a declared read landed in           *)
(* (s, pos). Backward validation is sound exactly when this holds at      *)
(* every append point.                                                     *)
(***************************************************************************)
I4a_NoStaleValidatedRead ==
    \A t \in All, e \in All :
        ~(e.key \in t.reads /\ t.snap < e.pos /\ e.pos < t.pos)

================================================================================
