# Round 3 — prompt pro GPT Pro: reemitir o report formal pro seal v3 (F9)

**Como usar (Ian):** cola tudo a partir de "THE PROMPT" no GPT Pro, anexando:

1. `paper/sources/ctt.txt` — o report atual (o que ele vai revisar/reemitir)
2. `crates/capsule/src/seal.rs` — o seal v3 shipped (direct MAC + session frame)
3. `crates/capsule/src/canon.rs` — o codec canônico (injetividade)
4. `crates/ipc/src/bin/aster_brokerd.rs` — sessões + verbos (o C-CHANNEL real)
5. `paper/sources/rereferee-round2-report.md` — o veredito round 2 (contexto F9)

Resultado esperado: um addendum formal (ou report v2) que prove o protocolo
v3 SHIPPED. Quando chegar, salvamos em `paper/sources/` e o pointer do paper
volta a ser "the report governs".

---

## THE PROMPT

You authored *The Capsule Transaction Theorem — a repaired proof for Aster
v0.7* and later refereed the implementation (round 2, verdict REJECT with
findings F1–F18). Your finding **F9** was documentary: the report formalizes
the retired **prehash** seal (v2) and predates the **session frame**, so it
does not prove the shipped v3 protocol. Your task now is to close F9 by
producing a formal **addendum** (or a revised report section) that proves the
shipped protocol exactly as implemented:

1. **Seal v3, direct MAC**: `tag = MAC_κ(canonical_frame)` where the frame
   is the domain-separated canonical encoding (algorithm string
   `aster-blake3-keyed-v3`, capsule domain `aster-capsule-v3`) of
   (cid, lease_epoch, **session**, tenant, deployment, ts, canonical
   content). No prehash: the MAC covers the full framed message. Update the
   outer-message injectivity lemma to include the session frame, and remove
   the unkeyed-collision branch (assumption A3) from the T1a reduction —
   direct MAC needs only EUF-CMA (SUF-CMA for BLAKE3 keyed as you prefer).
2. **Session binding as shipped (bearer-capability)**: the broker mints an
   unguessable 32-byte session id, registers (session → cid, epoch) in its
   own table, and rebuilds the expected context exclusively from that table;
   the session enters the MAC input. State T1a in the honest narrowed form
   your round-2 F3 required: an accepted capsule was issued for the
   presented broker-minted bearer session and its registered (self-asserted)
   label — NOT physical wrong-cell rejection. `cid` at mint is
   request-supplied; keep A11 (trusted launch identity) as an assumption.
3. **Session lifecycle**: one session = one transaction attempt; Commit
   spends the session BEFORE the fence runs (atomic consume-first); Abort is
   the no-commit close. Fold this into the CE2.1 discussion (replay of
   issued capsule bytes remains inevitable; session closure is what bounds
   the attempt count).
4. **T1b transcript, honest**: the whole-protocol transcript reveals the
   exact conflicting key and the returned epoch / horizon / retention-floor
   / commit-timestamp values (explicit protocol leakage, not side channels),
   and module bundles are broker-provided launch inputs outside the capsule
   theorem. Name these in the ideal functionality.
5. **Retention well-formedness (your round-2 F11)**: add the separate
   invariant R0 (0 ≤ g ≤ tip(L)) with the obstruction-freedom statement you
   drafted, discharged in code by the advance_retention tip clamp + the
   lease-acquisition repair. Do NOT fold liveness into T2 or Lemma R.
6. **Reality ledger**: re-stamp the report's implemented-vs-proposed ledger
   to the current tree (write plane + fence + sessions shipped; policy
   deployment-wide only; two-plane history split per F1; Variant B hybrid —
   B for points, whole-capsule for ranges — per F6).

Keep the abstract protocol and repaired proofs intact where they already
hold; the deliverable is the v3 delta, formally closed, so the paper's
appendix pointer can safely say "the report governs" again.
