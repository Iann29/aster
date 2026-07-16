# CLAIMS.md — the paper's honesty backbone

Every load-bearing claim in `paper/authenticate-the-reads.md`, with its evidence and its
current status. A claim not in this ledger should not be in the paper; a status that
degrades must degrade the paper text with it. Update this file in the same commit as any
paper edit that touches a claim.

**Status legend**

| Status | Meaning |
|---|---|
| `MEASURED` | A real number from a real run, written down in a source doc with method |
| `PROVED-COND` | Proved in the technical report, conditional on the assumptions ledger (A1–A13) |
| `IMPLEMENTED` | In the tree at the branch this paper describes, with tests; code path cited |
| `LANDING-v0.7` | Being built in a parallel v0.7 slice this round; paper writes it as "v0.7 ships X". **Verify merged + tested before submission, or demote the paper text** |
| `PENDING` | Does not exist. The paper must say so (planned methodology only, no numbers) |
| `RULE` | A standing guardrail about what we may never claim |

---

## 1 Headline & theorem claims

| Claim (as the paper states it) | Evidence | Status |
|---|---|---|
| Committed **mutations** are strictly serializable over the declared, authenticated read/write sets; commit-timestamp order; append is the linearization point (T2) | `paper/sources/ctt.txt` §2.3 + §3.8 (induction over the commit log; stability lemmas 3.6/3.7; fence lemmas 3.10/3.11); referee report: verdict "APROVADO COM RESSALVAS", T2 induction spot-checked | PROVED-COND |
| **Snapshot reads are serializable but possibly stale**; the headline is never stated without this caveat (referee F6) | ctt.txt Executive verdict item 6 + §2.3; mirrors FDB's own snapshot-read caveat | PROVED-COND + RULE |
| Read-set unforgeability: an accepted capsule was issued for exactly that channel-bound context (T1a); scope is "an issued capsule", NOT "the latest" — rollback/replay of earlier issued capsules is permitted by design (CE 2.1) | ctt.txt §2.1 + §3.2 (two-case reduction: MAC forgery ∨ hash collision; direct-MAC seal removes the collision case) | PROVED-COND |
| Confinement: executor's view simulatable from its authorized grant transcript; whole protocol additionally leaks named control bits, esp. the **conflict bit** (T1b, CE 2.3) | ctt.txt §2.2 + §3.3 | PROVED-COND |
| **Byzantine equivalence / honesty boundary**: every Byzantine commit is reproducible by an authorized protocol-following client with the same grants; omission demotes to an authorized blind write (T3, Variant B) | ctt.txt §2.4 + §3.9; attack appendix items 4 & 8 | PROVED-COND |
| Retention safety: validation sound iff consulted log covers (s, h]; exact condition g ≤ s under a pin; stale-read counterexample makes it necessary (Lemma R) | ctt.txt §2.5 + §3.5 | PROVED-COND |
| Read-plane scale-out: any κ-holder can serve+reseal; only the committer needs the lease (C1); revocation propagates with bounded skew across read brokers (F3 caveat) | ctt.txt §2.6 + §3.10; referee F3 | PROVED-COND |
| The proof is **conditional** on ledger A1–A13 (+P1/P2 proved); it is a protocol specification, not code verification — the paper says this verbatim | ctt.txt Status box + §5; referee §5 | PROVED-COND + RULE |
| Thirteen attack obligations discharged (replay, transplant, epoch race, splice/rollback/fork, phantoms/exhaustion gap, absence flips, encoding ambiguity, under/over-declaration, GC race, duplicates, policy TOCTOU, whole-capsule replay) | ctt.txt §4 (items 1–13) | PROVED-COND |
| Weakest joint is **A6 (complete conflict projection)** — defeats T2 with no cryptography broken; named plainly in §8 | ctt.txt §9 Confidence report; referee F8 | PROVED-COND + RULE |
| The theorem survived an independent adversarial review round with **no fatal findings** (two independent passes converged) | `paper/sources/aster-referee-report.md` (verdict + §7 Opus convergence) | Evidence on disk |

## 2 Novelty / positioning claims (what is ours)

| Claim | Evidence | Status |
|---|---|---|
| Serve-time MAC-sealed read-sets as an **online admission gate for OCC validation** against Byzantine executors — no system found does this (FDB trusts declarations; Fabric's rwsets come from re-executing endorsers; Fides authenticates the log post-hoc) | `paper/sources/aster-related-work.md` claim 1 (three sweeps, five neighbors full-text cleared) | Claimed; related-work dossier is the basis. Re-verify Basil full-text before submission (dossier follow-up) |
| Untrusted **executor** locus (compute layer), vs. the literature's untrusted storage (Fides, SUNDR, Depot, TransEdge) or whole replica set (Basil) | related-work claim 2 + positioning table | Claimed per dossier |
| Online **prevention** of serializability violations at a trusted commit gate, vs. detection/audit (Fides, Cobra) | related-work claim 3 | Claimed per dossier |
| "A trusted broker makes TEEs unnecessary for **executor** isolation" — stated nowhere found in the TEE line | related-work claim 4 | Claimed per dossier |
| Principle: "for isolation, authenticate data flow — don't verify computation", with its limit stated as T3 | related-work claim 5; ctt.txt §7 thesis sentence | Claimed per dossier + PROVED-COND for the limit |

## 3 Implementation claims (code paths a reviewer can check)

| Claim | Evidence | Status |
|---|---|---|
| Direct-MAC seal `aster-blake3-keyed-v2`: MAC over `alg ∥ lp(cid) ∥ le64(e) ∥ lp(E(Cap))`, full framed canonical bytes; digest carried but never a MAC input; constant-time tag compare; pinned wire test vector | `crates/capsule/src/seal.rs` (`seal_mac`, `ct_eq`, `seal_test_vector_is_stable`) | IMPLEMENTED |
| Honest seal history: v1 (`aster-blake3-keyed-v1`) MACed a prehash digest and needed A3 (unkeyed-BLAKE3 collision resistance); v2 retires A3 per the theorem's Remark 3.4 | seal.rs module docs; ctt.txt CE 2.2 + Remark 3.4. **Numbering note:** the theorem text calls the prehash construction "v2" and direct-MAC "v3" (capsule-domain numbering); the CODE's algorithm strings are v1 (prehash) → v2 (direct-MAC), and the capsule DOMAIN is `aster-capsule-v3`. The paper uses the code's algorithm-string naming. Do not "fix" one to match the other | IMPLEMENTED |
| Canonical codec with adversarial decode (W-CANON): rejects duplicate keys, out-of-order keys, invalid tags, truncated input, oversized lengths, trailing bytes; 18 rejection tests | `crates/capsule/src/canon.rs` (18 `#[test]`s); structural validation also enforced at the seal-verify chokepoint for the JSON IPC path: `crates/capsule/src/lib.rs::validate_structure` via `seal.rs::verify` | IMPLEMENTED |
| Sealed range certificates: interval + endpoint inclusivity + direction + limit + ordered keys + Exhausted/Boundary stop; observed-window computation; F2 rule (completeness needs an Exhausted certificate — ask ℓ+1) documented | `crates/capsule/src/lib.rs` (`RangeCertificate`, `ScanStop`, `ObservedWindow`, `window()`); ctt.txt Def 1.1 + Lemma 3.7; referee F2 | IMPLEMENTED |
| Write plane: lease authority with **strictly increasing, never-reused epochs** (F1) + commit fence as ONE Postgres transaction (lease row `FOR UPDATE` → V2 epoch equality for committer AND capsule context → stable horizon → retention coverage g ≤ s with row-lock pin → V4 point+window conflict scan → append at c = h+1); GC serialized on the same retention lock | `crates/store-postgres/src/write_plane.rs` (`acquire_lease`, `commit`, `advance_retention`); module docs carry the theorem↔code map | IMPLEMENTED |
| **Ten** integration proofs against real Postgres: epochs never reuse; CE 3.9 write skew impossible sequentially AND under real concurrency (exactly one of two racing fences commits); stale epoch can't append after failover; GC blocks on in-flight fence pin + coverage enforced; replay commits as a second transaction; phantom insert conflicts with Exhausted window but NOT past a Boundary window; absence/tombstone reads conflict on later writes; MVCC point/prefix semantics | `crates/store-postgres/tests/write_plane_it.rs` (10 `#[test]`s, listed by name in the file). **Count note:** the S12 briefing said "13 integration proofs"; the actual count on the base branch is 10 (commit c615bd9's message also says 10). The paper says ten | IMPLEMENTED |
| Runs unmodified `npx convex deploy` bundles: ESM compile + `Convex.asyncSyscall("1.0/get")` trap loop, one trap per user-level read; Postgres adapter reads the upstream Convex schema (documents/_tables/_modules/_source_packages, ZIP unpack) | `crates/v8cell/src/lib.rs`; `crates/store-postgres/src/{lib,module_index,modules_storage,table_mapping}.rs`; README smoke transcript | IMPLEMENTED |
| ~12k lines of Rust, 8 crates, 167 tests | measured in-tree: `wc -l` over `crates/**/*.rs` = 11,756 (tests included); `grep -c '#[test]|#[tokio::test]'` = 167; crates: broker, capsule, convex-codec, host, ipc, runner, store-postgres, v8cell | MEASURED (2026-07-16, branch v0.7-s12 base) |
| Broker lifetime-budget bug (self-termination after 1024 traps) fixed in v0.7 | found by the 2026-07-16 bench (`paper/sources/aster-bench-notes.md` finding 1); fix commit b8fce68 "fix(brokerd): remove connection lifetime budget that killed busy brokers" | IMPLEMENTED |
| Convex upstream single-writer lease as design constraint | `convex-backend` repo `crates/postgres/src/lib.rs:1738–1799` (external pointer, cited in ctt referee §5) | External evidence |

## 4 In flight THIS round — paper says "v0.7 ships X"; verify before submission

| Claim in the paper | Where the paper says it | What must be true at submission |
|---|---|---|
| Certified prefix scans served **over the wire** (certificate sealed into the capsule the cell holds) | §3.5 last paragraph | The IPC prefix path returns sealed range certificates, not just point entries; test exists. If the slice didn't land: rewrite to "certificates are sealed at the store/committer layer; wire serving is next" |
| Channel binding / C-CHANNEL (seal v3 session binding; context from trusted launch metadata, never request-asserted) | §3.1, §3.2 "Channel binding", §5 delta list | Broker verifies against per-socket immutable launch context; request-supplied context must equal it. If not landed: demote to "specified (C-CHANNEL); prototype binds cid+epoch, full channel binding landing" |
| Lease epoch obtained from the storage lease authority (not self-asserted env) | §5 delta list | Broker context epoch flows from `acquire_lease`/lease authority. Partially true today on the write plane (`write_plane.rs`); the brokerd read path must stop trusting `ASTER_LEASE_EPOCH`-style env before the claim is fully true |
| Variant B **consumption tracking** in the V8 runtime (S auto-populated from actually-consumed observations incl. warm hits; prewarm stays out until consumed); also closes bench finding 2 (no warm-capsule short-circuit on the real syscall path) | §3.6, §5 delta list + engineering findings | Runtime tracks consumption on the production `Convex.asyncSyscall` path; warm hits don't re-trap; consumed set is what commit declares. If not landed: paper must say the warm-path bug is open and S is trap-populated only |
| TLA+ model checking of the fence (validation/append atomicity, GC pin, epoch failover) | §5 write-path paragraph, §8 limitations | The spec + model-check run exist in-repo. If not landed: change to "we are model-checking" or move to future work — do NOT leave the past tense |

## 5 Not built — the paper must never present these as existing

| Item | Paper treatment |
|---|---|
| Cell-facing IPC **commit verb** (S9) | §5 "Not yet wired" says mutations reach the fence through the committer API, not from inside a cell — keep until S9 lands |
| **Write-path benchmark** (S10): commit throughput, fence latency decomposition, abort-rate-under-contention, failover blip; comparison baselines | §6.3 + §6.5 are `[PENDING: S10 write-path bench]` with methodology only. NO projected numbers anywhere. §6.2 growing-capsule reseal curve equally unmeasured (its own PENDING tag) |
| Warm-pool serving latency | §6.4 names the 390 ms floor as spawn+boot (measured) and defers warm numbers — never project |
| Live convex-backend committer integration (F9 option (a) or (b)) | §5 states plainly: v0.7 builds NEITHER; write plane owns its own `aster`-schema log; integration is a scoped-out product decision |
| Incremental / Merkle-ized sealing | §6.2 future remedy, contingent on the unmeasured curve |

## 6 Evaluation numbers — every number in §6 traces here

Source: `paper/sources/aster-bench-notes.md` (2026-07-16, first real end-to-end bench; harness `paper/sources/aster-bench.sh`; N=12, K=200, exit 0).

| Number in the paper | Source line |
|---|---|
| T0 = 390 ms p50 (min 351, p95 398) | bench-notes results table |
| T1 = 390 ms p50 (min 362, p95 400) | bench-notes results table |
| TK = 458 ms p50 (min 422, p95 490) | bench-notes results table |
| Marginal per-trap = (TK−T1)/199 = **0.34 ms** | bench-notes results table |
| Band 0.3–0.5 ms; K=16 run = 0.47 ms/trap; signal 68 ms ≫ noise ±10 ms | bench-notes ¶ after table |
| First-trap cost below noise (< ±10 ms) | bench-notes results table |
| ~2,900 traps/s implied serial broker throughput | bench-notes results table |
| Cold floor = docker spawn + V8 boot (not capsule machinery) | bench-notes closing ¶ |
| Per-trap contents (UDS connect + framed JSON + full seal verify + point read `ts<=$ts ORDER BY ts DESC LIMIT 1` + merge + full re-encode/re-MAC + response + promise resolution) | bench-notes "What the per-trap number contains" |
| 1,000-read report query ≈ 0.4 s trap time + ~0.4 s cold spawn | bench-notes "Context vs. the RAM discussion" |
| Setup facts: postgres:16, Convex-schema fixtures, ASTER_SNAPSHOT_TS=200, `"traps":N` sanity assert, spawn cancels in subtraction | bench-notes Setup/Method |
| EQ1 caveat: capsule stayed tiny (same id ×200 = 1 entry) → reseal cost constant; O(capsule) per trap → O(n²) growing | bench-notes honesty caveat ¶ |

## 7 Standing guardrails (RULEs — violating any is a failed draft)

1. **Never** claim "no BFT / no re-execution" as a novelty. Fides (ICDCS 2020) owns the
   no-BFT precedent. In Aster it is a consequence of the trusted-broker model. The paper
   states this in §1.2 and §7 and must keep stating it.
2. **Never** lead with "code holds no credential" as the contribution — industry table
   stakes by 2026 (Anthropic Managed Agents, Cloudflare Code Mode, Vercel Sandbox). Ours
   is the data-plane half.
3. Headline is always "**strict serializability for mutations; snapshot reads possibly
   stale**". T2 is serializability over DECLARED+AUTHENTICATED sets — authority, not
   arithmetic integrity. Omitted dependencies demote to authorized blind writes.
4. Evaluation: measured numbers only (§6 of this ledger). Write-path subsections stay
   `[PENDING: S10 write-path bench]` until S10 produces data. No projected number may be
   formatted as a result.
5. Citations: only works in `paper/sources/aster-related-work.md` / ctt.txt references,
   with the venues/years those files give. Systems the dossier names without a venue
   (Ryoan, Opaque, EnclaveDB, VC3, SUNDR, Depot, IFDB, Qapla, CapTP, DataCapsule/GDP)
   stay in-text-only until the LaTeX pass adds verified BibTeX.
6. The paper never discusses authorship tooling.
7. Every design claim cites a repo path a reviewer can open.

## 8 Bookkeeping notes for the orchestrator

- **"13 integration proofs" (S12 briefing) vs 10 (reality):** write_plane_it.rs has 10
  tests; commit c615bd9 says 10. Paper and this ledger say ten. If sibling slices add
  fence tests, update both.
- **Seal version numbering mismatch** (theorem's v2/v3 vs code's v1/v2 algorithm strings
  + `aster-capsule-v3` domain): explained in §3 row above; the paper consistently uses
  the code's algorithm-string naming.
- **LOC:** README says "roughly 10k"; measured 11,756 incl. tests on this base. Paper
  says "roughly twelve thousand… (11.8k measured, tests included)". Re-measure at
  submission.
- **Section 4 in-flight items** are the reconciliation hot spots: if any slice slips out
  of the v0.7 merge, the paper text listed there MUST be demoted in the same commit.
- Incidents in §1 (AgentCore IAM exfil — Sonrai/Unit 42, Feb 2026; Supabase MCP
  `service_role` / "lethal trifecta", 2025) are cited as reported in the sources dossier;
  verify public URLs at LaTeX time.
- Tech report page count: 27 pp. (the skeleton once said 28 — 27 is correct per the PDF
  and referee report).
