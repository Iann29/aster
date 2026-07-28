------------------------- MODULE AsterFenceNoAtomic -------------------------
(***************************************************************************)
(* NEGATIVE MODEL (re-referee F10): the A-ATOMIC mutant the positive       *)
(* model cannot express. AsterFence encodes the lease-row lock as "at      *)
(* most one in-flight fence"; TLC there checks the CONSEQUENCES of that    *)
(* serialization, not the serialization itself. This module removes the    *)
(* mutual exclusion: up to two fences may validate concurrently, each      *)
(* against the horizon it captured at BEGIN, and both may append. The      *)
(* log's primary key is modeled faithfully as (pos, key) — the real       *)
(* aster.log PK is (tenant, deployment, ts, key) — so a same-position      *)
(* same-key second append is refused (the statement would die on the PK),  *)
(* while same-position appends on DIFFERENT keys both land. That door is   *)
(* exactly Counterexample 3.9. Expected TLC result: I4_NoWriteSkew is      *)
(* VIOLATED (both-append write skew); I1_PrimaryKey holds (it is enforced  *)
(* structurally, kept as a sanity invariant).                              *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Committers, Keys, MaxPos, MaxEpoch

ASSUME /\ MaxPos \in Nat /\ MaxPos >= 1
       /\ MaxEpoch \in Nat /\ MaxEpoch >= 1

VARIABLES
    leaseEpoch,
    held,
    log,
    floor,
    inflight    \* a SET of fences — the mutant's whole point

vars == <<leaseEpoch, held, log, floor, inflight>>

Entry == [pos: 1..MaxPos, key: Keys, epoch: 1..MaxEpoch,
          snap: 0..MaxPos, reads: SUBSET Keys]

Fence == [cmtr: Committers, epoch: 1..MaxEpoch, snap: 0..MaxPos,
          reads: SUBSET Keys, wkey: Keys, h: 0..MaxPos]

Max(S) == CHOOSE x \in S : \A y \in S : y <= x

Tip == IF log = {} THEN 0 ELSE Max({e.pos : e \in log})

Init ==
    /\ leaseEpoch = 0
    /\ held = [c \in Committers |-> 0]
    /\ log = {}
    /\ floor = 0
    /\ inflight = {}

AcquireLease(c) ==
    /\ inflight = {}
    /\ leaseEpoch < MaxEpoch
    /\ leaseEpoch' = leaseEpoch + 1
    /\ held' = [held EXCEPT ![c] = leaseEpoch + 1]
    /\ UNCHANGED <<log, floor, inflight>>

(***************************************************************************)
(* MUTANT: no `inflight = None` guard. Two fences may hold the same        *)
(* captured horizon simultaneously — the unlocked check-then-append.       *)
(***************************************************************************)
FenceBegin(c, s, R, w) ==
    /\ Cardinality(inflight) < 2
    /\ Tip < MaxPos
    /\ held[c] = leaseEpoch
    /\ held[c] > 0
    /\ s <= Tip
    /\ floor <= s
    /\ inflight' = inflight \cup
           {[cmtr |-> c, epoch |-> held[c], snap |-> s,
             reads |-> R, wkey |-> w, h |-> Tip]}
    /\ UNCHANGED <<leaseEpoch, held, log, floor>>

WindowConflict(f) ==
    \E e \in log : e.key \in f.reads /\ f.snap < e.pos /\ e.pos <= f.h

(***************************************************************************)
(* Each fence validates against ITS OWN h — a concurrent sibling's append  *)
(* lands above h and is invisible to the scan, which is the race. The PK   *)
(* guard refuses a same-(pos, key) second append, exactly as Postgres      *)
(* would kill the INSERT; different keys at one position both land.        *)
(***************************************************************************)
FenceCommit ==
    \E f \in inflight :
        /\ ~WindowConflict(f)
        /\ ~\E e \in log : e.pos = f.h + 1 /\ e.key = f.wkey
        /\ log' = log \cup {[pos |-> f.h + 1, key |-> f.wkey,
                             epoch |-> f.epoch, snap |-> f.snap,
                             reads |-> f.reads]}
        /\ inflight' = inflight \ {f}
        /\ UNCHANGED <<leaseEpoch, held, floor>>

FenceAbort ==
    \E f \in inflight :
        /\ inflight' = inflight \ {f}
        /\ UNCHANGED <<leaseEpoch, held, log, floor>>

Next ==
    \/ \E c \in Committers : AcquireLease(c)
    \/ \E c \in Committers, s \in 0..MaxPos, R \in SUBSET Keys, w \in Keys :
           FenceBegin(c, s, R, w)
    \/ FenceCommit
    \/ FenceAbort

Spec == Init /\ [][Next]_vars

Symm == Permutations(Committers) \union Permutations(Keys)

I1_PrimaryKey ==
    \A e1, e2 \in log : (e1.pos = e2.pos /\ e1.key = e2.key) => e1 = e2

I4_NoWriteSkew ==
    \A t1, t2 \in log :
        ~(/\ t1 # t2
          /\ t1.key \in t2.reads
          /\ t2.key \in t1.reads
          /\ t1.snap < t2.pos
          /\ t2.snap < t1.pos)

================================================================================
