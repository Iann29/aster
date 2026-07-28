# Re-referee round 2 — pacote pro GPT Pro (v0.7 completo)

**Como usar (Ian):** cola no GPT Pro tudo a partir de "THE PROMPT" abaixo,
anexando estes arquivos do repo (nesta ordem de prioridade — se houver limite
de anexos, corta do fim):

1. `paper/authenticate-the-reads.md` — o paper draft (o alvo principal)
2. `paper/CLAIMS.md` — o ledger de claims (honestidade auditável)
3. `crates/store-postgres/src/write_plane.rs` — lease authority + commit fence
4. `crates/ipc/src/bin/aster_brokerd.rs` — sessões, verbos, o gate do Commit
5. `crates/broker/src/fence.rs` — o trait CommitFence + MemoryFence twin
6. `tla/AsterFence.tla` + `tla/RESULTS.md` — o modelo TLC e as 4 rodadas
7. `bench/results/v07/RESULTS.md` — a campanha de medição S10
8. `paper/sources/ctt.txt` — o teorema (ele já conhece; referência)

O que esperar de volta: veredito no formato do round 1 (aprovado/reprovado +
achados F-numerados com severidade). Achados novos viram um round de fix
antes do LaTeX/arXiv.

---

## THE PROMPT

You are the referee who reviewed the **Capsule Transaction Theorem** (verdict:
approved with findings F1–F8, no fatal flaw, Variant B confirmed as the sound
formulation). Since then the theorem was implemented end-to-end. Your role now
**reverses**: you attack. Review the implementation and the attached paper
draft as a hostile referee for a top systems-security venue (USENIX
Security / CCS bar). The authors' own adversarial review ran four rounds
(multi-lens finders, 3 independent skeptics per finding, mutation testing,
TLC model checking) and everything it confirmed was fixed — your value is
finding what *that process* is structurally blind to.

### What the implementation decided that the theorem left abstract (errata / instantiation deltas)

Judge each: **do T1a / T1b / T2 survive this instantiation?** Name gaps as
F-numbered findings.

1. **C-CHANNEL** is instantiated as broker-minted 32-byte random sessions in
   an in-process table; the seal (v3, `aster-blake3-keyed-v3`) MACs the
   session into every capsule; every capsule verb resolves the presented
   session id against the table and rejects context mismatches.
   **Obligation #1 remains open and is declared in the paper:** the cell's
   `cid` and epoch are self-asserted on the FIRST request (mint); binding
   them to trusted launch metadata (launch token) is named future work.
   Obligation #2 is discharged: in Postgres mode the broker's epoch comes
   from `acquire_lease` at boot, and mint refuses contexts claiming any
   other epoch.
2. **Seal lineage v1→v2→v3**: v2 replaced hash-then-MAC with direct MAC +
   constant-time compare (killed assumption K-PREHASH); v3 added the session
   frame. Only v3 verifies; downgrade attempts are tested.
3. **Variant B as shipped**: `declared_reads ⊆ capsule` with duplicate
   rejection (B-SUBSET); an undeclared write key demotes to an *authorized
   blind write*; conflict windows are derived by the committer from the
   sealed range certificates (A6), never taken from the cell.
4. **F2 rule as shipped**: completeness requires an `Exhausted` certificate
   (ask ℓ+1); `Boundary` windows do not extend past the last returned key; a
   phantom past a Boundary window does NOT conflict (negative case tested).
5. **Retention liveness (not in the theorem)**: `advance_retention` clamps
   the watermark to the log tip — found by the TLA+ model, not by tests, and
   outside Lemma R's scope (which covers the pin's safety, not this liveness
   edge). **Direct question: should T2 / Lemma R gain a liveness
   companion, or is the clamp correctly framed as an implementation
   obligation?**
6. **Session lifecycle** (CE2.1): one session = one transaction attempt;
   Commit closes the session on every structured answer — including gate
   rejections (context mismatch) — and Abort is the no-commit close. Replay
   of issued capsule bytes is bounded by session closure.
7. **Retention-floor guards on BOTH read planes**: the Convex-schema capsule
   store checks `min_document_snapshot_ts`, the write plane checks its own
   `low_watermark` — both AFTER the query, justified by floor monotonicity
   (floor ≤ ts observed post-query proves it held during the query; checking
   before would race a concurrent sweep). Snapshots below the floor refuse
   as `Stale` instead of risking false absence evidence.
8. **Prototype seams declared in paper §8** (attack the framing, not just
   the facts): (i) brokerd pins snapshot + epoch at boot, and the demo read
   plane never observes `aster.log` commits — so "retry from a fresh
   snapshot" is dischargeable only by relaunching the broker (the F9
   integration decision is explicitly scoped out); (ii) document-id
   aliasing (Convex IDv6 vs raw wire form) is confined to hand-rolled
   native callers — a JS bundle only ever holds IDv6 strings.

### The measured claims (attack the eval)

All numbers are medians from the committed campaign (`bench/results/v07/`),
stock Postgres 16 (`synchronous_commit=on`, `fsync=on`), single serial
committer by design (upstream Convex is single-writer per deployment too):

- **Read path**: warm read ≤5 µs (timer floor), 200 same-key reads = 1 trap
  (harness-asserted); cold trap ~0.93 ms/trap at growing capsule (two SQL
  round trips dominate).
- **Reseal (EQ2)**: linear per trap, 0.030 ms + 0.66 µs/entry (slight upper
  bound — harness clones the capsule client-side); cumulative quadratic
  confirmed (predicted 361 ms vs 366.9 measured at n=1000).
- **Fence isolated (EQ3)**: blind commit 3.51 ms p50 / 280 commits/s;
  point validation FLAT p=1→200; window sweep 4.59→6.61 ms is an upper
  bound that conflates w with log growth (~1.0k→1.6k events — declared);
  conflict-abort 1.75 ms (no WAL flush); 7 SQL round trips per fence.
- **End-to-end (EQ3/EQ4)**: 6.49 ms p50 per authenticated fenced mutation
  (exec 2.44 + commit 4.03), 152.9 tx/s serial; **the commit leg equals the
  isolated fence** — the capability apparatus adds nothing measurable.
- Reproducibility: clean-clone re-run same day; headline medians drifted
  low single digits, small-N un-warmed tails up to +26% (declared per
  metric); shakedown/clone raw logs not preserved (declared).

**Question: does any sentence in §6 or the abstract claim more than these
numbers support?** The paper's own honesty rules: no projected numbers, no
"within noise" without per-metric drift, pending items marked as such.

### Your tasks

1. **Theorem→code fidelity**: for each delta above, does the proof survive?
   Where the code is *stronger* than the theorem (e.g., the liveness clamp),
   say whether the theorem should absorb it.
2. **Hostile paper review**: soundness of claims vs evidence, overclaim
   hunting (especially §1.2 "what we do not claim" vs what §3–6 imply),
   related-work positioning (FDB / Basil / Fabric / TransEdge / Fides — the
   no-BFT/no-reexecution/no-TEE combination is claimed as inherited from the
   trusted-broker model, NOT as novelty; check that discipline holds
   everywhere), threat-model honesty (A12 side channels, egress obligation).
3. **Eval attack**: methodology holes the declared caveats do NOT already
   cover; any figure a venue artifact-evaluation committee could fail.
4. **Verdict**: same format as round 1 — approve/reject + F-numbered
   findings with severity and the sentence-level fix each requires.

Do not soften. A finding the authors can refute is cheaper than one a venue
referee finds first.
