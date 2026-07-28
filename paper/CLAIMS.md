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

## Post-paper v0.8 product-state addendum (2026-07-23)

The tables below remain the ledger for the v0.7 paper text. Mainline has since
closed several limitations that the paper correctly reports as absent. This
addendum does **not** silently upgrade the submitted/draft prose; a v0.8 paper
revision must reconcile every affected paragraph before using these facts.

**RECONCILED 2026-07-28 (same commit as this line):** the paper text now
distinguishes measured v0.7 campaigns from shipped v0.8 mechanisms in every
affected paragraph — abstract + §1.3 (A12 profile shipped; split-plane
measurement labeling), §2.3 (egress enforced by the profile), §3.1/§3.2
(launch-token mint identity), Table 3 A11 (narrowed) + A12 (enforced),
§5 (counts re-stamped; v0.8-closure block; F9 paragraph gains the
authoritative single history; concurrency finding updated; obligations
split discharged/remaining), §6 (dating note; "did not exist in the
measured build"), §8 seams (i) and (iii) updated, §9 conclusion. Every
v0.8 mechanism statement cites the rows above; every number remains from
the dated campaigns.

| Product delta | Evidence | Mainline status |
|---|---|---|
| One authoritative transaction history: snapshot selection, document reads, retention, conflict validation, and append all use tenant/deployment `aster.log` | `crates/store-postgres/src/authoritative.rs`; broker Postgres construction; `crates/ipc/tests/authoritative_postgres.rs` commit-to-fresh-cell proof | IMPLEMENTED |
| Deployment policy independently gates read/write/scan/module/insert plus bounded transaction/session limits; generated policy starts deny-all | `crates/ipc/src/policy.rs`; broker authorization gates and denial tests; `docker/policy.production.example.json` | IMPLEMENTED |
| Trusted launch binding uses one-use, expiring tokens tied to cell/tenant/deployment/authority epoch; broker reconstructs session context | `crates/ipc/src/launch.rs`; `aster_launch_token`; token/session tests | IMPLEMENTED |
| Operational A12 profile: rootless/read-only/capability-free cells, no cell network, broker-only DB network, kernel peer-UID gate, resource limits and V8 watchdog | `docker/compose.production.yml`; `docker/aster-invoke`; `crates/v8cell/tests/resource_limits.rs`; peer rejection + production Compose smokes | IMPLEMENTED, not independently audited |
| Bounded concurrent IPC contains silent peers; commit consumes a bearer session atomically before fence work | `aster_brokerd.rs`; `process_boundary.rs::silent_peer_does_not_head_of_line_block_other_cells`; `concurrent_commits_cannot_double_spend_one_session` | IMPLEMENTED |
| Real bundle mutations mint canonical IDv6, commit through the fence, and are visible to a fresh cell | `docker/smoke-bundle.sh`; `module_loader.rs`; `authoritative_postgres.rs` | IMPLEMENTED for get/insert/patch/replace/delete subset |

## Round-3 formal addendum (2026-07-28) — F9 closed

The companion report is amended by `paper/sources/capsule-transaction-theorem-v3-addendum.md`
(SHA-256 `924cc5d1042f830a813a69bf28c353fa12c2aeaf1f0382bf23f10e99f664f6bd`), issued by the
report's author to close re-referee F9. It proves the **shipped v3 protocol exactly**:
Lemma 3.2-v3 (outer-frame injectivity including the session frame), T1a-v3 as a one-branch
EUF-CMA reduction (A3 retired; P2 replaced by the proved lemma), the honest bearer-session
scope for T1a (physical-cell attribution stays conditional on A11), consume-first lifecycle
in CE 2.1 plus Lemma S1 (at most one fence attempt per session), T1b-v3 with the exact
transcript leakage (conflicting key, epoch, horizon, retention floor, commit timestamp) and
module bundles as a separate launch-input functionality, invariant R0 with obstruction
freedom (separate from T2/Lemma R), and a re-stamped reality ledger recording the shipped
hybrid `Rimpl = S_points ∪ AllRanges(C)`. The addendum closes **F9 only** — it explicitly
preserves the F1 two-plane disclosure. Code correspondence spot-verified in-tree on
2026-07-28: v3 frame byte layout and audit-digest-outside-MAC (`seal.rs::seal_mac`/`verify`),
consume-first atomic remove (`aster_brokerd.rs::consume_bound_context` +
`concurrent_commits_cannot_double_spend_one_session`), retention clamp + lease repair
(`write_plane.rs`), all-ranges window derivation (`aster_brokerd.rs` commit path). The
paper's appendix pointer now reads "the report, as amended by the addendum, governs".

---

## 1 Headline & theorem claims

| Claim (as the paper states it) | Evidence | Status |
|---|---|---|
| Committed **mutations** are strictly serializable over the declared, authenticated read/write sets; commit-timestamp order; append is the linearization point (T2) | `paper/sources/ctt.txt` §2.3 + §3.8 (induction over the commit log; stability lemmas 3.6/3.7; fence lemmas 3.10/3.11); referee report: verdict "APROVADO COM RESSALVAS", T2 induction spot-checked | PROVED-COND |
| **Snapshot reads are serializable but possibly stale**; the headline is never stated without this caveat (referee F6) | ctt.txt Executive verdict item 6 + §2.3; mirrors FDB's own snapshot-read caveat | PROVED-COND + RULE |
| Read-set unforgeability: an accepted capsule was issued for exactly that channel-bound context (T1a); scope is "an issued capsule", NOT "the latest" — rollback/replay of earlier issued capsules is permitted by design (CE 2.1) | ctt.txt §2.1 + §3.2, as amended by v3 addendum §2.2 + §3.2 (T1a-v3: one-branch EUF-CMA reduction, session in the outer frame, bearer-session scope; A3 retired) | PROVED-COND |
| Confinement: executor's view simulatable from its authorized grant transcript; whole protocol additionally leaks named control bits, esp. the **conflict bit** (T1b, CE 2.3) | ctt.txt §2.2 + §3.3, as amended by v3 addendum §2.6 + §2.8 + §3.4 (exact transcript leakage: conflicting key, epoch, horizon, retention floor, commit ts; module bundles via `Fmodule`) | PROVED-COND |
| **Byzantine equivalence / honesty boundary**: every Byzantine commit is reproducible by an authorized protocol-following client with the same grants; omission demotes to an authorized blind write (T3, Variant B) | ctt.txt §2.4 + §3.9; attack appendix items 4 & 8 | PROVED-COND |
| Retention safety: validation sound iff consulted log covers (s, h]; exact condition g ≤ s under a pin; stale-read counterexample makes it necessary (Lemma R) | ctt.txt §2.5 + §3.5 | PROVED-COND |
| Read-plane scale-out: any κ-holder can serve+reseal; only the committer needs the lease (C1); revocation propagates with bounded skew across read brokers (F3 caveat) | ctt.txt §2.6 + §3.10; referee F3 | PROVED-COND |
| The proof is **conditional** on ledger A1–A13 (+P1/P2 proved); it is a protocol specification, not code verification — the paper says this verbatim | ctt.txt Status box + §5; referee §5; v3 addendum §4 (A3 retired, P2 replaced by Lemma 3.2-v3, A11 narrowed, A11-S added) | PROVED-COND + RULE |
| Thirteen attack obligations discharged (replay, transplant, epoch race, splice/rollback/fork, phantoms/exhaustion gap, absence flips, encoding ambiguity, under/over-declaration, GC race, duplicates, policy TOCTOU, whole-capsule replay) | ctt.txt §4 (items 1–13) | PROVED-COND |
| Weakest joint is **A6 (complete conflict projection)** — defeats T2 with no cryptography broken; named plainly in §8 | ctt.txt §9 Confidence report; referee F8 | PROVED-COND + RULE |
| The theorem survived an independent adversarial review round with **no fatal findings** (two independent passes converged) | `paper/sources/aster-referee-report.md` (verdict + §7 Opus convergence) | Evidence on disk |

## 2 Novelty / positioning claims (what is ours)

| Claim | Evidence | Status |
|---|---|---|
| Serve-time MAC-sealed read-sets as an **online admission gate for OCC validation** against Byzantine executors — no system we surveyed does this (FDB trusts declarations; Fabric's rwsets come from re-executing endorsers; Fides authenticates the log post-hoc) | `paper/sources/aster-related-work.md` claim 1 (three sweeps, five neighbors full-text cleared) | Claimed; related-work dossier is the basis. Re-verify Basil full-text before submission (dossier follow-up) |
| Untrusted **executor** locus (compute layer), vs. the literature's untrusted storage (Fides, SUNDR, Depot, TransEdge) or whole replica set (Basil) | related-work claim 2 + positioning table | Claimed per dossier |
| Online **prevention** of serializability violations at a trusted commit gate, vs. detection/audit (Fides, Cobra) | related-work claim 3 | Claimed per dossier |
| "A trusted broker makes TEEs unnecessary for **executor** isolation" — stated nowhere found in the TEE line | related-work claim 4 | Claimed per dossier |
| Principle: "for isolation, authenticate data flow — don't verify computation", with its limit stated as T3 | related-work claim 5; ctt.txt §7 thesis sentence | Claimed per dossier + PROVED-COND for the limit |

## 3 Implementation claims (code paths a reviewer can check)

| Claim | Evidence | Status |
|---|---|---|
| Direct-MAC, session-bound seal `aster-blake3-keyed-v3`: MAC over `alg ∥ lp(cid) ∥ le64(e) ∥ SB ∥ lp(E(Cap))` where SB is a domain-separated session frame (tag `0x00` unbound; `0x01 ∥ session[32]` bound — the tag alone fixes the frame length, so bound/unbound can never collide); full framed canonical bytes; digest carried but never a MAC input; constant-time tag compare; verification accepts ONLY v3 (v1 and v2 rejected); TWO pinned wire vectors, one per session state | `crates/capsule/src/seal.rs` (`seal_mac`, `ct_eq`, `seal_test_vector_is_stable`, `bound_seal_test_vector_is_stable`, `sealed_capsule_rejects_legacy_v1_algorithm`, `sealed_capsule_rejects_legacy_v2_algorithm`) | IMPLEMENTED |
| Honest seal history: v1 (`aster-blake3-keyed-v1`) MACed a prehash digest and needed A3 (unkeyed-BLAKE3 collision resistance); v2 (`aster-blake3-keyed-v2`, commit 22e872f) direct-MACs the full framed bytes and retires A3 per the theorem's Remark 3.4; v3 (commit d55fd06) keeps direct-MAC and adds the domain-separated session binding (C-CHANNEL), superseding v2 **within the v0.7 cycle** — only v3 verifies | seal.rs module docs; ctt.txt CE 2.2 + Remark 3.4. **Numbering note:** the theorem text names v1 as the implemented prehash seal (§1.6) and sketches the direct-MAC alternative under the capsule-DOMAIN name (`aster-capsule-v3 ∥ …`, Remark 3.4; §9 calls it "a v3 direct-MAC format"); the CODE's algorithm strings are v1 (prehash) → v2 (direct-MAC) → v3 (direct-MAC + session), and the capsule DOMAIN is `aster-capsule-v3`. The paper uses the code's algorithm-string naming. Do not "fix" one to match the other | IMPLEMENTED |
| Canonical codec with adversarial decode (W-CANON): rejects duplicate keys, out-of-order keys, invalid tags, truncated input, oversized lengths, trailing bytes; 18 codec tests, 14 of them adversarial-decode rejections (the other 4 pin positive round-trips / canonical ordering) | `crates/capsule/src/canon.rs` (18 `#[test]`s: 14 `rejects_*`/`decode_rejects_*` + `round_trip_decode_of_encode`, `reencode_of_decode_is_byte_identical`, `decode_recomputes_root_hash`, `range_certificate_sequence_order_is_canonical`); structural validation also enforced at the seal-verify chokepoint for the JSON IPC path: `crates/capsule/src/lib.rs::validate_structure` via `seal.rs::verify` | IMPLEMENTED |
| Sealed range certificates: interval + endpoint inclusivity + direction + limit + ordered keys + Exhausted/Boundary stop; observed-window computation; F2 rule (completeness needs an Exhausted certificate — ask ℓ+1) documented | `crates/capsule/src/lib.rs` (`RangeCertificate`, `ScanStop`, `ObservedWindow`, `window()`); ctt.txt Def 1.1 + Lemma 3.7; referee F2 | IMPLEMENTED |
| Write plane: lease authority with **strictly increasing, never-reused epochs** (F1) + commit fence as ONE Postgres transaction (lease row `FOR UPDATE` → V2 epoch equality for committer AND capsule context → stable horizon → retention coverage g ≤ s with row-lock pin → V4 point+window conflict scan → append at c = h+1); GC serialized on the same retention lock | `crates/store-postgres/src/write_plane.rs` (`acquire_lease`, `commit`, `advance_retention`); module docs carry the theorem↔code map | IMPLEMENTED |
| **Cell-facing write path (S9)**: `Commit`/`Abort` IPC verbs — session gate → seal verify against the table-rebuilt bound context → B-SUBSET declared-set check (every declared key must be a sealed observation; duplicates rejected) → conflict windows derived from ALL sealed certificates (never a cell claim) → `CommitFence` seam; ANY structured commit/abort answer closes the session (one session = one transaction attempt); postgres-mode broker epoch from `acquire_lease` at boot, stamped into every minted session, mint refuses other epochs | `crates/ipc/src/lib.rs` (verbs + `UdsCapsuleBrokerClient::commit/abort`), `crates/ipc/src/bin/aster_brokerd.rs` (`handle_request` Commit/Abort arms, `ProcessBroker::commit`); gated e2e `pg_commit_e2e` incl. `pg_v8_mutation_write_set_commits_and_interleaved_conflict_aborts` | IMPLEMENTED |
| **Mutation syscalls (S9b)**: `1.0/insert` (explicit `_id`; mint-me rejected), `1.0/shallowMerge` (db.patch), `1.0/replace`, `1.0/remove` — write set born in the cell (`V8ExecutionResult::write_set`), read-your-own-writes without ledgering pending writes, absence observations consumed on failed patch/replace, no store authority granted | `crates/v8cell/src/lib.rs` (`syscall_insert/patch/replace/delete`, `V8CellState::writes` ordering rule) | IMPLEMENTED |
| **Eighteen** integration proofs against real Postgres: epochs never reuse; CE 3.9 write skew impossible sequentially AND under real concurrency (exactly one of two racing fences commits); stale epoch can't append after failover; the Lemma R pin holds in BOTH directions (GC blocks on in-flight fence; fence blocks on sweeper-side lock holder, then commits — `fence_blocks_on_retention_lock_holder_until_release`) + coverage enforced; a wedged idle lock holder is killed by the idle-in-transaction timeout so failover resumes (`idle_wedged_lock_holder_is_killed_so_failover_resumes`); replay commits as a second transaction; phantom insert conflicts with Exhausted window but NOT past a Boundary window; absence/tombstone reads conflict on later writes AND a tombstone write inside the window conflicts with a point read (`tombstone_write_in_window_conflicts_with_point_read`); MVCC point/prefix semantics; Send/Sync of fence input types statically asserted; retention watermark clamps to the log tip (commit-admission liveness regression); reads below the retention floor refuse as Stale (`reads_below_the_retention_floor_are_stale`); scan order pinned bytewise, SQL COLLATE "C" == Rust str order (`prefix_scan_order_is_bytewise`); a stranded above-tip floor is repaired at lease acquisition (`retention_floor_above_tip_is_repaired_at_lease_acquisition`, invariant R0); memory/Postgres fence parity on one identical scenario (`memory_fence_matches_write_plane_outcomes`) | `crates/store-postgres/tests/write_plane_it.rs` (18 `#[test]`s, listed by name in the file). **Count note:** base had 10 (c615bd9), 885ef9b added the clamp regression → 11, the adversarial-review fix round (C7/R5/R7 + C2) added three more → 14, S9 added the fence-parity proof → 15, the final-review round added the retention-floor read guard → 16, the round-2 repair kit added the bytewise-order and R0-repair proofs → 18. The paper says eighteen | IMPLEMENTED |
| Runs unmodified `npx convex deploy` bundles: ESM compile + `Convex.asyncSyscall("1.0/get")` trap loop, one trap per user-level read; Postgres adapter reads the upstream Convex schema (documents/_tables/_modules/_source_packages, ZIP unpack) | `crates/v8cell/src/lib.rs`; `crates/store-postgres/src/{lib,module_index,modules_storage,table_mapping}.rs`; README smoke transcript | IMPLEMENTED |
| ~22.1k lines of Rust, 8 crates, 287 tests | measured in-tree: `git ls-files 'crates/**/*.rs'` piped to `xargs wc -l` = 22,112 (tests + the S10 bench bin included; 21,398 without `crates/ipc/examples/`); grep-count `#[test]`+`#[tokio::test]` = 287; crates: broker, capsule, convex-codec, host, ipc, runner, store-postgres, v8cell | MEASURED (2026-07-28, main, post-v0.8-closure — re-measure at submission) |
| Broker lifetime-budget bug (self-termination after 1024 traps) fixed in v0.7 | found by the 2026-07-16 bench (`paper/sources/aster-bench-notes.md` finding 1); fix commit b8fce68 "fix(brokerd): remove connection lifetime budget that killed busy brokers" | IMPLEMENTED |
| Convex upstream single-writer lease as design constraint | `convex-backend` repo `crates/postgres/src/lib.rs:1738–1799` (external pointer, cited in ctt referee §5) | External evidence |

## 4 In flight THIS round — paper says "v0.7 ships X"; reconciled 2026-07-16 (truth-reconciliation pass; rows marked S10 re-reconciled after the S9 merge + S10 campaign, tree @ v0.7-s10)

| Claim in the paper | Where the paper says it | Reconciled status (2026-07-16, tree @ 75263cb unless marked) |
|---|---|---|
| Certified prefix scans served **over the wire** (certificate sealed into the capsule the cell holds) | §3.5 last paragraph | **VERIFIED**: brokerd `hydrate_prefix` scans at the capsule snapshot and seals the certificate via `capsule.hydrate_range` (`crates/ipc/src/bin/aster_brokerd.rs`); cross-process coverage in `crates/ipc/tests/process_boundary.rs` (drives `HydratePrefix` end-to-end) |
| Channel binding / C-CHANNEL | §3.1, §3.2 "Channel binding", §5 delta list | **DEMOTED to the implemented mechanism** (the pre-reconciliation text overclaimed "per-socket label from trusted launch metadata"): what ships is a broker-minted unguessable 32-byte session id (OS entropy, `SessionTable::mint`) returned in `InitialCapsuleGrant`; every capsule verb presents it; the broker rebuilds the expected bound context EXCLUSIVELY from its own session table (`resolve_bound_context`); enforcement is the seal-v3 domain-separated session frame — a capsule from another session fails MAC verification even with identical cell_id+epoch (`capsule_from_another_session_fails_seal_verification`). HONEST CAVEATS (re-reconciled 2026-07-28 for v0.8): connections are one-per-request, so nothing is per-socket — still true. The mint-time-identity caveat is CLOSED: the postgres-mode broker refuses `InitialCapsule` without a one-use launch token binding (cell, tenant, deployment, epoch), TTL-bounded and spent atomically (`crates/ipc/src/launch.rs`; `launch_token_required`), and the UDS accept path enforces `SO_PEERCRED` UID admission; the epoch claim is checked against the boot-acquired authority epoch and refused on mismatch (`stale_lease_epoch`). A11 stays an ASSUMPTION in Table 3, NARROWED to the token issuer + supervisor chain (matches the v3 formal addendum §4) |
| Lease epoch obtained from the storage lease authority (not self-asserted env) | §5 delta list + obligations | **S10 re-reconciled (post-S9)**: the postgres-mode brokerd now acquires its epoch from `acquire_lease` at BOOT, stamps it into every minted session, and refuses InitialCapsule contexts claiming any other epoch (`stale_lease_epoch`; `aster_brokerd.rs` run_broker + mint gate) — a wrong env value can no longer yield a usable session. What REMAINS env-delivered (re-reconciled 2026-07-28): the cell still receives the epoch via the supervisor's launch environment, now cross-checked at mint by the launch token's epoch binding; the postgres-mode broker ignores `ASTER_LEASE_EPOCH` outright (memory mode keeps the env stand-in). Paper §5 obligation reworded to exactly this residue |
| Variant B **consumption tracking** in the V8 runtime (consumption ledger from actually-consumed observations incl. warm hits; prewarm stays out until consumed); also closes bench finding 2 (no warm-capsule short-circuit on the real syscall path) | §3.6, §5 delta list + engineering findings | **S10 re-reconciled (post-S9)**: runtime tracks consumption on the production `Convex.asyncSyscall` path (`V8ExecutionResult::consumed_reads`), the ledger NOW crosses the wire as the Commit verb's `declared_reads` (B-SUBSET-checked broker-side), and the `aster_v8cell` envelope carries `consumed_reads` + `write_set` for harnesses. Warm-hit fix ASSERTED on the wire by the S10 bench (same-key ×200 = 1 trap, `bench/results/v07/b1-read-path.log`); §3.6 wire-boundary sentence updated accordingly |
| TLA+ model checking of the fence (validation/append atomicity, GC pin, epoch failover) | §5 write-path paragraph, §8 limitations | **VERIFIED**: spec + four configs + real TLC runs in-repo (`tla/AsterFence.tla`, `tla/*.cfg`, `tla/RESULTS.md` — TLC 2.19, invariants I1–I4a; positive model passes, epoch-reuse and no-pin negatives produce their designed I3/I2 violations) |
| TLA+ addendum 2026-07-16: retention-clamp re-verification after the liveness fix | §5 write-path paragraph (liveness find + re-run), §8 | **VERIFIED**: `tla/RESULTS.md` addendum — `advance_retention` clamp (`current.max(requested.min(tip))` under the retention row lock, `write_plane.rs:555`) + regression `retention_watermark_clamps_to_log_tip` + model guard `w <= Tip`; all four configs re-run: positive clean over 14,813,201 generated states, `AsterFenceReuse` still violates I3 and `AsterFenceNoPin` still violates I2 as designed |

## 5 Not built — the paper must never present these as existing

(S10 removed two rows from this table by building them: the cell-facing
commit verb — S9, landed before this campaign — and the write-path
benchmark itself. Their claims moved to §3 and §6.)

| Item | Paper treatment |
|---|---|
| **Abort-rate-under-contention sweep, prewarm on/off sweep, failover blip** | §6.3 names them unmeasured after the S10 tables ("Not yet measured, named so the reader can hold us to it"). Only the per-abort COST is measured |
| **Comparison baselines** (upstream trusted-executor Convex; TEE concept baseline) | §6.5 is `[PENDING: baseline campaign]` with methodology only — S10 scoped baselines out, and the section says so |
| Warm-pool serving latency | §6.4 names the floor as spawn+boot (measured at both packaging points: ~390 ms docker, ~6 ms host process) and defers warm-pool numbers — never project. (B4's long-lived-process e2e amortizes V8 *process* init and is labeled as such — it is not a warm-pool claim) |
| Live convex-backend committer integration (F9 option (a) or (b)) | §5 states plainly: NEITHER is built; write plane owns its own `aster`-schema log; integration is a scoped-out product decision. The S10 e2e bridges the two timestamp spaces by seeding the aster log tip to the fixture snapshot — stated in `bench/results/v07/RESULTS.md`, never presented as integration. v0.8 note: the STANDALONE plane now shares one authoritative history (`AuthoritativeCapsuleStore`) — that is not a live-Convex integration and the paper says so |
| Incremental / Merkle-ized sealing | §6.2 future remedy, now contingent on the MEASURED curve (linear per trap, quadratic cumulative; not the bottleneck at ≤10³ small entries) |

## 6 Evaluation numbers — every number in §6 traces here

### 6a — v0.6 baseline (kept in the paper as Table 4, labeled historical)

Source: `paper/sources/aster-bench-notes.md` (2026-07-16, first real end-to-end bench, PRE warm-hit fix and PRE retention-floor guard; harness `paper/sources/aster-bench.sh`, dockerized one-shot cells; N=12, K=200, exit 0).

| Number in the paper | Source line | Status |
|---|---|---|
| T0 = 390 ms p50 (min 351, p95 398); T1 = 390 (362/400); TK = 458 (422/490) | bench-notes results table | MEASURED (v0.6 conditions — historical baseline) |
| Marginal per-trap = (TK−T1)/199 = **0.34 ms**; band 0.3–0.5; K=16 = 0.47 | bench-notes results table + ¶ | MEASURED (v0.6 conditions; the always-trap path no longer exists — paper §6.1 says exactly this) |
| First-trap cost below noise (< ±10 ms); ~2,900 traps/s implied serial | bench-notes results table | MEASURED (v0.6 conditions) |
| Cold docker floor = container spawn + V8 boot | bench-notes closing ¶ | MEASURED (v0.6 conditions) |

### 6b — S10 v0.7 campaign (2026-07-16, branch v0.7-s10 @ 8cb09c7)

Source: `bench/results/v07/RESULTS.md` + raw logs `bench/results/v07/*.log` (canonical run), harness `bench/run-v07.sh` + `crates/ipc/examples/bench_v07.rs`. Machine: AMD Ryzen 7 5800H (16 threads), 31 GiB RAM, Arch Linux 7.0.9, rustc 1.94.1, postgres:16.14 container (stock `synchronous_commit=on`), docker dev stack idling alongside (load snapshot in `machine.log`). Reproduced from a clean clone of the branch same day (headline medians within single-digit drift; the small-N un-warmed tails moved up to +26% — detailed in RESULTS.md, which also records that shakedown/clone logs were not preserved). All release builds, warmup discarded where a warmup pass exists (see RESULTS.md), median/p95/min/max + raw per-sample series committed for the canonical run. TLC jar pinned by sha256 (tla/RESULTS.md). At submission: freeze ONE artifact commit and re-verify every count and number against it (round-2 referee F17).

| Number in the paper | Method | Status |
|---|---|---|
| **B1 re-run (Table 4b)**: T0=5 ms, T1=6 ms, TKsame=7 ms **with 1 trap** (harness-asserted), TKdistinct=191 ms with 200 traps; first-trap 1–2 ms (1 ms granularity, median-convention sensitive); warm-read marginal ≤0.005 ms (timer floor); cold-trap marginal 0.93 ms (capsule 1→200) | v0.6 subtraction methodology, host processes, N=12, K=200; `"traps"` asserted | MEASURED |
| Cold trap = TWO store queries (value + `min_document_snapshot_ts` retention-floor guard) | code path `store-postgres/src/lib.rs::read_point/check_retention_floor`; guard postdates the v0.6 bench | Code fact (paper §6.1 explains the delta) |
| **EQ2 curve (Table 5)**: per-trap 0.035 ms @ n=1 → 0.697 ms @ n=1000; linear fit 0.030 ms + 0.66 µs/entry (~13.5 µs/KB); cumulative climb quadratic (367 ms measured @ n=1000 vs 361 predicted); wire request 0.9→49.8 KB | UDS-direct constant-size sampling (grow to n, then 300 samples re-hydrating a held key; memory store isolates the apparatus) | MEASURED |
| **EQ3 fence (Table 6)**: blind 1-write commit 3.51 ms p50 / 3.87 p95, **280 commits/s sustained serial** (N=1500); points p∈{1,10,50,200} ≈ 4.3 ms flat; windows w∈{1,10,50} over a growing ~1.0k→1.6k-event (s,h] = 4.59/5.81/6.61 ms (upper bound on the w-effect — the sweep conflates w with log growth); conflict-abort **1.75 ms** (no WAL flush); 7 SQL round trips per blind fence (+1 points, +1 windows) | direct `WritePlane::commit`, fresh (tenant,deployment), single serial committer; round-trip count from code | MEASURED |
| **EQ3/EQ4 e2e (Table 7)**: 6.49 ms p50 / 7.10 p95 per transaction (exec 2.44 + commit 4.03), **153 tx/s serial**, N=500 after 30 warmup, `Committed` asserted per sample; commit leg ≈ isolated fence p=1 (no apparatus overhead ISOLATED by the comparison — no paired A/B control, per round-2 referee F15; not "zero") | full PLUMBING loop (read adapter and commit log are separate histories — F1) over real UDS: InitialCapsule → fresh V8 isolate JS (`1.0/get` + `1.0/insert`) → Commit verb → Postgres fence; read store = Postgres Convex adapter, fence = Postgres write plane | MEASURED |
| Host-process cold floor ~6 ms (T0) vs ~390 ms docker | B1 T0 both campaigns | MEASURED |
| 200 distinct authenticated reads end-to-end in 191 ms as host one-shot | B1 TK-distinct | MEASURED |

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
4. Evaluation: measured numbers only (§6 of this ledger). §6.5 baselines stay
   `[PENDING: baseline campaign]` until a baseline campaign produces data; the
   write-path items S10 did not measure (abort rate under contention, prewarm
   sweeps, failover blip) stay named as unmeasured in §6.3. No projected number
   may be formatted as a result, and v0.6-conditions numbers (0.34 ms/trap, 390 ms
   docker floor) are always labeled as the historical baseline, never as current.
5. Citations: only works in `paper/sources/aster-related-work.md` / ctt.txt references,
   with the venues/years those files give. Systems the dossier names without a venue
   (Ryoan, Opaque, EnclaveDB, VC3, SUNDR, Depot, IFDB, Qapla, CapTP, DataCapsule/GDP)
   stay in-text-only until the LaTeX pass adds verified BibTeX.
6. The paper never discusses authorship tooling.
7. Every design claim cites a repo path a reviewer can open.

## 8 Bookkeeping notes for the orchestrator

- **Integration-proof count:** 15 as of the S10 stamp (base 10 at c615bd9 → +1 clamp
  regression 885ef9b → +3 adversarial-review round → +1 S9 fence-parity proof).
  Paper and this ledger say eighteen. Re-count at any fence-touching merge.
- **Seal version numbering** (theorem describes prehash v1 in §1.6 and sketches
  direct-MAC under the `aster-capsule-v3` domain name in Remark 3.4/§9 vs the CODE's
  algorithm strings v1 → v2 → v3, capsule domain `aster-capsule-v3`): explained in §3
  row above; the paper consistently uses the code's algorithm-string naming. Only v3
  verifies; v2 lived and died inside the v0.7 cycle (22e872f → d55fd06).
- **LOC:** measured 22,112 incl. tests + the S10 bench bin (2026-07-28, main,
  post-v0.8-closure; 21,398 without `crates/ipc/examples/`). Paper says "roughly
  twenty-two thousand… (22.1k measured, tests and the S10 bench harness
  included)". Re-measure at submission.
- **Count-carrying lines** (re-stamped 2026-07-28 at the v0.8 reconciliation —
  LOC 22,112 / 287 tests / eighteen fence proofs / codec 18-total-14-rejection):
  paper abstract, §1.1 C4, §1.3 preview, §5 first ¶, §5 codec bullet, §3.2
  canonical-encoding ¶, §5 fence ¶, §6 tables 4b–7, §9 closing numbers, and this
  ledger's §3 + §6b rows. Re-measure all of them again at submission.
- **Section 4 in-flight items** are the reconciliation hot spots: if any slice slips out
  of the v0.7 merge, the paper text listed there MUST be demoted in the same commit.
- Incidents in §1 (AgentCore IAM exfil — Sonrai 2025-09-04 + Unit 42 2026-04-07, dates corrected per round-2 referee F18; Supabase MCP = demonstrated scenario, NOT a customer breach; Supabase MCP
  `service_role` / "lethal trifecta", 2025) are cited as reported in the sources dossier;
  verify public URLs at LaTeX time.
- Tech report page count: 27 pp. (the skeleton once said 28 — 27 is correct per the PDF
  and referee report).
