# Aster — paper skeleton

**Working title (primary):** *Authenticate the Reads, Not the Code: Isolating Untrusted Database Executors with Capsule Transactions*

**Alternates:**
- *Aster: Credential-Free, Serializable Execution for Untrusted Database Code*
- *The Capsule Transaction Theorem: Strict Serializability with Byzantine Executors, No Replication, No Enclaves*

**Target venue class:** systems-security. Primary: USENIX Security / CCS (the "untrusted / AI-generated code near a database" + lethal-trifecta framing lands hardest there). Secondary: OSDI / EuroSys (transaction protocol + prototype). The DB angle (OCC/serializability) is a section, not the frame.

**Spine sentence (survives the proof — every section serves this):**
> Aster makes every submitted OCC observation provenance-authentic without trusting or re-executing the application executor; strict serializability then follows from classical backward validation, while any omitted dependency is demoted to an authorized blind write rather than a cross-authority isolation failure.

**Claim guardrails (baked in — see aster-related-work.md):**
- ✗ Do NOT claim "serializable txns on untrusted infra without BFT" as novel — Fides (ICDCS'20) owns it. Frame no-BFT/no-re-execution as a *consequence* of the trusted-broker model.
- ✗ Do NOT lead with "code holds no credential" — table stakes by 2026 (Anthropic Managed Agents, Cloudflare Code Mode, Vercel Sandbox). Lead with the *data-plane* half.
- ✓ DO claim: serve-time MAC-sealed read-sets as an online OCC soundness gate; untrusted *executor* (not storage) locus; online *prevention* (not detection); "a trusted broker makes TEEs unnecessary for executor isolation."

---

## Abstract (draft, ~180 words)

Platforms increasingly run code they do not trust next to a database they do: AI-generated application logic, third-party plugins, multi-tenant functions. The prevailing defenses isolate the *compute* (microVMs, enclaves) and broker the *outbound credential*, but the code still holds some path to data — and, as recent incidents show, isolation is one SSRF away from credential exfiltration. We take the opposite cut: the executor never touches storage at all. A trusted broker owns the database and a single-writer lease, and serves each untrusted executor cryptographically **sealed, per-invocation data capsules**; at commit, the broker validates the executor's **MAC-authenticated read-set** with classical optimistic backward validation before appending. We prove the *Capsule Transaction Theorem*: under a fully Byzantine executor, the committed history is strictly serializable over the declared, authenticated read/write sets, and a compromised executor is exactly as powerful as a malicious-but-authorized application — it cannot fabricate a read, cross a policy, or forge a conflict. The construction needs no BFT replication, no re-execution, and no trusted hardware. We implement Aster in ~10k lines of Rust, run unmodified `npx convex deploy` bundles end-to-end, and show the authentication apparatus adds ~0.3 ms per read — a near-zero security tax on the read path.

---

## 1. Introduction

- **Hook (the pain):** untrusted/AI-generated code running against a real database is now normal (AI app builders, agent platforms, multi-tenant PaaS). The security question auditors and engineers ask: "what stops that code from touching data it shouldn't?"
- **Why the status quo fails (motivation, cite incidents):**
  - Ambient credentials are the universal weak point. Isolate the compute all you want; the code still holds an env var / IAM role / connection = ambient authority over everything that credential unlocks.
  - Concrete failures: AWS AgentCore "completely isolated" interpreter exfiltrating IAM creds via the metadata service (Sonrai / Unit 42, Feb 2026); Supabase MCP running with `service_role`, prompt-injected into dumping `integration_tokens` (the "lethal trifecta", 2025).
  - The 2026 frontier already brokers the *outbound credential* (Anthropic Managed Agents, Cloudflare Code Mode, Vercel Sandbox) — but nobody authenticates the *inbound data* or proves at commit that the code acted only on real, current rows.
- **Our cut:** move the trust boundary off the executor entirely. Executor is Byzantine; a trusted broker owns storage + lease and hands out sealed per-invocation capsules; commit-time OCC over authenticated read-sets.
- **The key idea in one line:** for isolation you do not need to verify the computation — you need to authenticate the data flow. OCC does not need to trust the executor if the read-set is authenticated.
- **Contributions:**
  - **C1 (mechanism):** serve-time keyed-MAC capsules that make each read self-certifying, turning classical OCC backward validation into a soundness gate against a Byzantine executor — no BFT, no re-execution, no TEE.
  - **C2 (theory):** the Capsule Transaction Theorem — read-set unforgeability (T1a), confinement (T1b), Byzantine strict serializability (T2), and a Byzantine-equivalence honesty boundary (T3), with retention (Lemma R) and read-plane scale-out (C1). Conditionally proved; 28-page proof in appendix / tech report.
  - **C3 (principle):** "authenticate data flow, don't verify computation," and its precise limit — under-declaration/rollback demote to *authorized blind writes*, not isolation failures (T3).
  - **C4 (system):** Aster, ~10k LOC Rust, runs unmodified Convex bundles end-to-end; eval shows ~0.3 ms/read security tax.
- **Results preview:** the eval headline number + the theorem headline (T3).

## 2. Background & threat model

- **Target data model:** MVCC document store + totally ordered commit log + single-writer lease with epochs (Convex's model; the lease is the design constraint — cite the backend lease). Reads at a snapshot; commits by a single writer; epochs rise on failover.
- **OCC / backward validation primer:** brief — read at snapshot s, validate no conflicting committed write in (s, h] at commit, append at c. (Set up T2 as "classical OCC, but the read-set is now authenticated.")
- **Threat model (crisp):**
  - *Trusted:* storage (MVCC + lease contracts), the broker/committer role, the MAC key κ. TCB = broker + storage.
  - *Untrusted:* the executor cells — arbitrary PPT Byzantine code from instruction zero. Only channel is the UDS to the broker. No DB creds, no network, no FS (A12).
  - *Collusion:* cells may share out-of-band anything they were authorized to read; the protocol prevents relabeling one cell's channel as another's context.
  - *Out of scope (state up front):* liveness against Byzantine cells, DoS/resource exhaustion, timing/cache side channels, exactly-once execution, compromise of the key-holding broker.
- **Goal statement:** strict serializability of committed effects + confinement of the executor to its authorized policy envelope, online, with no trusted hardware and no replication.

## 3. Design

- **3.1 Architecture (Fig 1):** long-lived broker per deployment (owns κ, storage handle, lease); one-shot cell per invocation (V8 isolate, UDS to broker). HTTP → broker spawns cell with context ctx = (tenant, deployment, cid, epoch, snapshot). Read → trap → broker seals → resume. The cell never holds a credential; every datum arrives as a sealed capsule.
- **3.2 Capsules and the seal (Fig 2):** capsule = (tenant, deployment, s, docs, ranges). Seal = keyed-BLAKE3 MAC binding cid + epoch + tenant + deployment + snapshot + canonical content. Canonical injective encoding (length-prefixed, tagged, ordered — Lemma 3.1). **v3 direct-MAC** (MAC the canonical bytes, no prehash) — present this as the design, note the implemented v2 prehash needs an extra collision-resistance assumption (referee F4).
- **3.3 Read protocol:** grow-and-reseal. Broker verifies seal → checks P.read → reads at σ_s → merges → reseals. Broker keeps NO per-cell state (statelessness is a design goal; enables read-plane scale-out, §Guarantees C1). Absence is first-class (versioned tombstones in the read-set).
- **3.4 Commit protocol + the commit fence (Fig 3):** cell submits (ctx, sealed capsule, declared set S, write set W). Broker validates in one storage-visible fence: seal (V1) → epoch (V2) → retention (V3) → OCC backward validation over authenticated R (V4) → write policy (V5) → atomic append at fresh c. Show the CommitFence pseudocode (proof p8). Emphasize: validation + append share one horizon (write-skew counterexample if not — referee-confirmed).
- **3.5 Phantom-safe range reads (Fig 4):** sealed range certificates binding interval, direction, limit, ordered returned keys, and exhausted-vs-boundary. Full result protects the prefix through the last key; short result protects the whole interval including the exhaustion gap. (The next-key problem, transplanted to OCC validation.) **API note (referee F2):** asserting completeness requires an Exhausted certificate — ask for ℓ+1.
- **3.6 Variant B — declared dependency sets:** S ⊆ Obs(capsule), each declaration an exact reference to a sealed atom. Honest V8 runtime auto-populates S from actually-consumed traps (ties to consumption tracking, referee F7). Why B over A: A's completeness advantage is illusory under stateless rollback; B avoids false conflicts under prewarm.

## 4. Formal guarantees

*(Condense the tech-report proof; full proof in appendix. State theorems + what they do NOT claim — Table 2.)*

- **T1a (read-set unforgeability):** no PPT coalition gets the broker to accept a capsule not issued for that exact channel-bound context, except negligibly. Reduction to MAC EUF-CMA + (for the v2 prehash) unkeyed-BLAKE3 collision resistance. **Honest scope:** authenticates *an issued* capsule, not "the latest" — replay/rollback of an earlier valid capsule is permitted (stateless broker).
- **T1b (confinement / non-interference):** the executor's view is simulatable from its authorized grant transcript alone; it learns nothing beyond the adaptive transitive closure of its policy-allowed reads. The whole-protocol view additionally leaks the *conflict bit* over authorized windows (named explicit leakage).
- **T2 (Byzantine strict serializability):** committed mutations form a strictly serializable history over the *declared, authenticated* read/write sets, in commit-timestamp order; linearization point = atomic append. **Remark vs FoundationDB** (the thesis remark): FDB does the same backward validation but trusts the client's declared ranges; Aster's delta is provenance-authenticity of each declared observation.
- **T3 (Byzantine equivalence — the honesty boundary / headline):** every Byzantine commit is reproducible by an authorized protocol-following client with the same grants ⇒ a compromised executor ≡ a malicious-but-authorized application. It can issue garbage/blind writes *inside its policy envelope* and nothing else. **This is the security pitch, stated as a theorem.**
- **Lemma R (retention safety):** validation is sound iff the consulted log covers (s, h]; the retention floor g ≤ s is necessary (stale-read counterexample). Liveness: long invocations bounded by Δ, must retry.
- **Corollary C1 (read-plane scale-out):** any κ-holder can serve+reseal reads; only commits need the lease ⇒ read brokers scale freely without changing T2.

## 5. Implementation

- Aster in Rust, ~10k LOC, 8 crates. V8 cells (isolate, module loader, Convex shims); broker over UDS; Postgres adapter reading the *same* Convex schema the upstream backend writes (documents / _tables / _modules / _source_packages); IDv6 codec; keyed-BLAKE3 seal.
- **Runs unmodified `npx convex deploy` bundles:** cell compiles the bundle as ESM, drives the `Convex.asyncSyscall("1.0/get")` trap loop, returns the document — same control shape as Convex's own runner, with exactly one trap per read.
- **The v0.7 delta (what this paper's protocol requires over the read-only prototype):** v3 direct-MAC seal; context/epoch from trusted launch + lease authority (not cell-asserted); the commit fence as one Postgres transaction (lease row FOR UPDATE + tip + conflict scan + policy version + append — the same pattern as Convex's lease); sealed range certificates on the wire; canonical-decoding verifier; Variant B consumption tracking. **The committer-integration decision (referee F9):** the write-plane broker must hold the lease — either it becomes the deployment's committer or forwards authenticated writes into the backend's committer (not a sidecar). State which we built.
- Implementation obligations honestly listed: constant-time MAC compare; key from a secret store; per-collection size caps.

## 6. Evaluation

*(Seeded by the real bench, 2026-07; expand for camera-ready.)*

- **EQ1 — read-path security tax.** Marginal per-trap cost = **~0.34 ms** (V8 trap → UDS → seal verify → Postgres point read → reseal → resume), measured against `postgres:16` on the real schema. Same order as a warm-connection Postgres point read alone ⇒ the crypto+IPC+broker apparatus adds ≈ nothing. **Headline: near-zero security tax on reads.** (Table 4.)
- **EQ2 — reseal scaling.** The seal re-encodes the whole capsule per trap: O(capsule) per read → O(n²) over a growing read-set. Measure the curve; motivate incremental/Merkle-ized sealing as future work if it bites. (Honest — flag it, don't hide it.)
- **EQ3 — commit throughput / latency (v0.7).** Once the commit fence lands: commits/s at the single committer, validation cost vs read-set size, abort rate under contention (Variant A vs B, prewarm on/off).
- **EQ4 — end-to-end.** Cold one-shot invocation floor ≈ 390 ms (docker spawn + V8 boot) — attribute to the warm-pool item, NOT the capsule machinery. With warm pooling, project the real serving latency.
- **EQ5 — comparison baseline.** vs a trusted-executor Convex function (same query, in-backend): quantify the isolation cost. vs a TEE baseline (concept): argue the broker replaces the enclave at the cost of one process.
- **Bugs surfaced by benching (report as engineering findings):** broker `MAX_CONNECTIONS` is a lifetime counter (self-terminates); the real syscall path never short-circuits a warm capsule (kills prewarm value until fixed). Both fixed in v0.7.

## 7. Related work

*(Drop in from aster-related-work.md — Table 1 positioning + prose.)*
- FoundationDB — same backward validation, trusts the client's declared ranges (the exact-delta anchor).
- Basil (SOSP'21) — Byzantine clients + serializability via 5f+1 BFT; Aster = same robustness, one trusted broker.
- Hyperledger Fabric — signed rwsets validated without re-execution at commit, but rooted in endorser re-execution; Aster removes the endorsers.
- Fides/TFCommit (ICDCS'20) — serializable txns on untrusted *storage* without BFT, but detection (offline audit) not prevention, no online trusted verifier, no per-read MAC. **(Cite for the no-BFT prior claim — guardrail.)**
- TransEdge (EDBT'23) — untrusted edge *storage* via BFT-SMaRt + Merkle proofs to trusted clients; inverse locus.
- Ryoan / Opaque / EnclaveDB / VC3 — confine untrusted execution with TEEs; Aster: trusted broker makes the enclave unnecessary.
- Cobra (OSDI'20) — validate serializability of a black-box DB offline; inverse trust direction, same "don't verify computation" spirit.
- Object-capabilities (CapTP / ocap) + IFC DBs (IFDB, Qapla) — unforgeable read tokens / confinement, but never wired into an OCC serializability gate.
- Name-collision preempt: Berkeley DataCapsule / GDP = durable signed logs (data at rest), unrelated.

## 8. Discussion, limitations, open problems

- **Limitations (from the referee, stated plainly):**
  - The weakest joint is A6 — complete conflict projection: range soundness assumes every index mutation surfaces as a key-space insert/delete visible to validation. Requires auditing the Convex index maintenance; recommend TLA+ model-checking the fence + GC + failover (F8). *(This is the honest "what a reviewer should attack" — own it.)*
  - Side channels (timing/cache) are out of scope; "nothing to exfiltrate" holds for *data*, not covert channels (F5/A12).
  - Snapshot reads may be stale (bounded by Δ); the strict-serializability headline is for *mutations* — mirror FDB's own snapshot-read caveat (F6).
  - Conditional on the assumptions ledger (A1–A13); it is a specification, not a code-verified system.
- **Open problems (from the proof §8):**
  - Reactive subscriptions over capsules — invalidation without a resident write log in the cell plane; the promising direction is a broker-side approximate matcher with one-sided error (false positives cost only re-execution).
  - Learned prewarming ("dream capsules") and its interaction with Variant B.
  - Zero-knowledge *compliance* transcripts (prove capability/protocol compliance, not JS correctness) as an optional audit layer.
  - Actual-dependency completeness without trusting V8 — the clean open question the repaired theorem exposes.

## 9. Conclusion

Restate the spine sentence. The cut that pays off: stop trying to make the executor trustworthy (hardware, replication, re-execution) and instead make its *data* unforgeable. A V8 escape becomes a buggy authorized app — the strongest isolation statement available without an enclave, proved.

---

## Planned figures & tables

- **Fig 1** — architecture (broker owns κ+storage+lease; cell = V8 + UDS only).
- **Fig 2** — read trap + grow-and-reseal sequence.
- **Fig 3** — CommitFence pseudocode (from proof p8).
- **Fig 4** — range certificate: exhausted vs boundary observed window (phantom protection).
- **Fig 5** — eval: per-trap cost breakdown; reseal-vs-capsule-size curve; commit throughput.
- **Table 1** — positioning: trust locus × mechanism × guarantee × cost (8 systems).
- **Table 2** — theorem map: statement + what it does NOT claim.
- **Table 3** — assumptions ledger (A1–A13, P1–P2).
- **Table 4** — eval headline numbers.

## Artifacts on hand (reuse directly)
- Proof / tech report: `The_Capsule_Transaction_Theorem_Aster_v0.7.pdf` (→ appendix + §4).
- Related work + guardrails + Table 1 + drop-in prose: `aster-related-work.md`.
- Eval seed + method + bugs: `aster-bench-notes.md` + `aster-bench.sh`.
- Referee findings (shape the limitations §8): `aster-referee-report.md` (F1–F9).
- Repo state / impl facts (§5): the dossier in the session transcript.
- Motivation incidents (§1): frontier report (AgentCore exfil, Supabase MCP).

## Honest to-do before submission (gaps the skeleton can't paper over)
1. A6 audit of convex-backend index maintenance + TLA+ of the fence (turns the weakest joint from "assumed" to "checked").
2. Build the v0.7 commit path so EQ3/EQ4 are *measured*, not projected (a systems venue needs a working write path, not just reads).
3. Resolve F9 (committer integration) — the paper must describe what was actually built.
4. Full-text Basil read for related work (referees will know it cold).
