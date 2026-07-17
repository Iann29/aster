# Aster v0.7 — S10 measurement campaign (2026-07-16)

The write-path benchmark the paper's §6 was gated on, plus the post-fix
re-run of the read path and the EQ2 reseal curve. Every number here is
from the canonical run recorded in the sibling raw logs (`b1-*.log` …
`b4-*.log`, `machine.log`), produced by `bench/run-v07.sh` at commit
`8cb09c7` on branch `v0.7-s10` (worktree of `v0.7-write-path`). Nothing
is projected.

## Environment

- **Machine**: AMD Ryzen 7 5800H (16 hw threads), 31.2 GiB RAM, Linux
  7.0.9-arch2-1 (Arch), developer workstation.
- **Toolchain**: rustc 1.94.1, cargo 1.94.1, all binaries `--release`.
- **Postgres**: `postgres:16.14` (Debian) in container `aster-pg-dev`,
  port-mapped to `127.0.0.1:5433`; stock durability config
  (`synchronous_commit=on`, `fsync=on`, `shared_buffers=128MB` — asserted
  at campaign time, verified live and appended to `machine.log` on
  2026-07-16 post-review; `run-v07.sh` now records the GUCs itself). A fresh
  `aster_bench` database is dropped and recreated per run: Convex-schema
  fixtures (`crates/store-postgres/tests/fixtures/{schema,seed}.sql`) +
  1,000 extra bench documents at ts=150, and the `aster` write-plane
  schema created by `ensure_schema`.
- **Honesty note on load**: a docker dev stack (synapse + jumpy, 8
  containers) runs alongside; the snapshot in `machine.log` shows it
  near-idle (all containers ≤3% CPU, load average 2.19 on 16 threads).
  Expect single-digit-percent noise, not systematic bias. One w=50
  window sample caught a 66 ms hiccup (max column); medians and p95 are
  the load-bearing statistics.
- **Timing**: `Instant` (Rust) around the client-side call — a sample
  includes request serialization and UDS connect, i.e. what a cell pays.
  B1 uses `date +%s%N` around whole one-shot processes with 1 ms
  granularity (the derived marginals divide by K−1=199, giving ~5 µs
  resolution). Warmup samples are discarded where a warmup pass exists
  (blind fence 100, point-validation 30 per case, e2e 30); the
  window-validation and conflict-abort phases run un-warmed — after hot
  earlier phases in the same process, but their cold first samples show
  in the max column — and B1's one-shot processes have no warmup by
  design. Reported stats are median / p95 / min / max over the stated N;
  full chronological per-sample series are in the `RAW` lines of each log.

Reproduce: `bash bench/run-v07.sh` (optionally `--only=b1,b2,b3,b4`),
with a postgres:16 container named `aster-pg-dev` on :5433.

## B1 — read path re-run (post warm-hit fix)

Same subtraction methodology as the 2026-07-16 first bench
(`paper/sources/aster-bench-notes.md`): workloads T0 (no syscall), T1
(one `1.0/get`), TK (K=200 gets), each run N=12 times as a one-shot
`aster_v8cell` process against a long-lived `aster_brokerd`
(`ASTER_STORE=postgres`, fixture DB, snapshot pinned at 200); spawn cost
cancels in the subtractions. Two deliberate deltas from the old run,
both stated: (1) cells are **host processes**, not docker one-shot
containers — the old ~390 ms floor was container spawn, and removing it
tightens every subtraction; (2) the store adapter now runs **two** SQL
queries per trap (the C3 retention-floor guard landed after the old
bench: value read + `min_document_snapshot_ts` check on the same
connection). TK now comes in two shapes because the S7 warm-hit fix
changed the semantics the old TK measured:

| Measurement (p50, N=12) | 2026-07-16 v0.6 run (docker) | this run (host) |
|---|---|---|
| T0 — no reads | 390 ms (min 351, p95 398) | 5 ms (min 5, p95 6) |
| T1 — 1 read | 390 ms (min 362, p95 400) | 6 ms (min 6, p95 7) |
| TK same key ×200 | 458 ms, **200 traps** | 7 ms, **1 trap** |
| TK distinct keys ×200 | not measured | 191 ms, 200 traps (min 183, p95 208) |
| First-trap cost (T1−T0) | below ±10 ms noise | 1–2 ms (1 ms timer granularity) |
| Same-key marginal | 0.34 ms/**trap** | ≤0.005 ms/**warm read** (timer floor) |
| Distinct-key marginal ((TK−T1)/199) | — | 0.93 ms/trap (capsule grows 1→200) |

**The warm-hit fix is proven on the production path**: 200 reads of one
key now cost one broker trap; the other 199 resolve from the sealed
capsule inside V8 at ≈5 µs per read (timer-resolution bound — the true
cost is a JS loop iteration plus a capsule lookup). Under the old
behavior this exact workload cost 200 traps ≈ 68 ms of trap time.
Asserted by the harness (`"traps":1`), not just timed.

**Median-convention note (review A3):** B1's per-process stats are
computed bash-side with the lower-of-two-middles median over N=12 at
1 ms granularity, while the Rust benches take the upper middle; T1's
series straddles 6/7 ms, so B1's *derived* numbers carry ±1 ms — hence
"1–2 ms" and "≤" above. The Rust-side tables are unaffected (large N,
sub-millisecond spreads).

**The 0.93 ms/trap distinct-key marginal is NOT comparable to the old
0.34 ms** headline: the old number was same-key (constant 1-entry
capsule, one SQL query per trap); this one reads 200 *different*
documents (each a cold key for the capsule), reseals a capsule growing
1→200 (≈0.10 ms apparatus at the mean size, per B2), and pays the
second guard query. The old 0.34 ms remains the honest v0.6-conditions
number; the v0.7 apparatus at those capsule sizes is *cheaper* than it
was (B2 n=1: 0.035 ms), and the trap cost is now dominated by the two
Postgres round trips.

psql baseline in the log (10 sequential `psql -c` point reads: 0.25 s)
is dominated by psql process spawn, kept only for continuity with the
old notes.

## B2 — reseal curve (EQ2)

`bench_v07 reseal` against a memory-store brokerd over the real UDS
socket — isolates the capability apparatus (connect + u32-framed JSON +
full seal verify of n entries + store hit + merge + full canonical
re-encode + re-MAC + response) from Postgres. Per size n: grow the
capsule with n distinct hydrates (timed = the cumulative climb), then
300 samples re-hydrating an already-present key — capsule size stays
exactly n, so each sample is one verify(n)+reseal(n) round trip.
Fixture docs are small (~49 B/entry on the wire); cost scales with
capsule *bytes*, so multiply accordingly for fatter documents.

| n (capsule entries) | request bytes | per-trap median | p95 | climb to n (cumulative) |
|---|---|---|---|---|
| 1 | 904 | 0.035 ms | 0.050 | 0.1 ms |
| 10 | 1,326 | 0.040 ms | 0.051 | 0.5 ms |
| 50 | 3,287 | 0.062 ms | 0.072 | 2.4 ms |
| 100 | 5,750 | 0.096 ms | 0.114 | 6.5 ms |
| 200 | 10,632 | 0.157 ms | 0.191 | 19.2 ms |
| 500 | 25,356 | 0.354 ms | 0.413 | 103.9 ms |
| 1000 | 49,832 | 0.697 ms | 0.809 | 366.9 ms |

**Shape**: cleanly linear per trap. Least-squares over the seven
medians: per-trap(n) ≈ **0.030 ms + 0.66 µs·n** (≈ 13.5 µs per KB of
capsule; fit predicts 0.692 ms at n=1000 vs 0.697 measured). Cumulative
over a read-set built one trap at a time is therefore quadratic:
Σ(a+b·i) ≈ a·n + b·n²/2 predicts 361 ms for n=1000 vs 366.9 measured.
At n=1000 the whole-capsule reseal still costs *less per trap than one
Postgres point read leg* (compare B1), so the quadratic is real but not
yet the bottleneck at realistic read-set sizes; incremental/Merkle-ized
sealing remains the known remedy if capsules grow past ~10³ entries or
entries get fat. One harness honesty note (review A4): the timed closure
clones the capsule client-side before each call — an O(n) `BTreeMap`
copy a real cell doesn't pay per trap — so the 0.66 µs/entry slope is a
slight upper bound on the apparatus itself (order of tens of µs inside
the 0.697 ms n=1000 sample).

## B3 — commit fence isolated (EQ3)

`bench_v07 fence`: direct `WritePlane::commit` calls against real
Postgres (no broker, no V8), fresh `(tenant, deployment)` namespace,
single serial committer (the lease design mandates one). Snapshot rides
the previous commit's ts, so the conflict window `(s, h]` is empty in
(a)/(b) and the sample isolates the fence machinery itself.

SQL round trips per fence, from the code: pool checkout stamps the two
session GUCs (1) + BEGIN (1) + lease `FOR UPDATE` (1) + horizon
`MAX(ts)` (1) + retention `FOR UPDATE` (1) + one INSERT per write (1
here) + COMMIT (1) = **7 round trips for a blind 1-write commit**; a
non-empty point set adds one `ANY($5)` query (8); windows add one
DISTINCT-key scan (8–9). The conflict path answers after the point scan
and rolls back on drop — ~6 round trips, **no WAL flush**.

| Case | median | p95 | notes |
|---|---|---|---|
| (a) blind 1-write commit | 3.51 ms | 3.87 | N=1500; **280.5 commits/s sustained** serial |
| (b) +1 point validation | 4.29 ms | 4.70 | N=200 each |
| (b) +10 points | 4.29 ms | 4.78 | |
| (b) +50 points | 4.28 ms | 4.61 | |
| (b) +200 points | 3.90 ms | 4.18 | |
| (c) +1 window | 4.59 ms | 4.91 | `(s,h]` population ~1,000→1,200 events |
| (c) +10 windows | 5.81 ms | 6.39 | population ~1,200→1,400 (each committed sample appends) |
| (c) +50 windows | 6.61 ms | 7.21 | population ~1,400→1,600; one 66 ms outlier in max |
| (d) conflict-abort | **1.75 ms** | 1.98 | N=100; no append, no COMMIT flush; decision latency — measured before the fence gained its awaited rollback (re-referee F15), re-measure next campaign |

**Interpretation.** The fence is durability-bound, not validation-bound:
the blind commit's ~3.5 ms is dominated by the synchronous WAL flush at
COMMIT (stock `synchronous_commit=on`), which is why an *abort* is twice
as cheap as a commit — it does the same validation reads but skips the
flush. Point validation is one extra round trip whose cost is flat in p
(the `= ANY` probe with an empty window barely moves from p=1 to p=200;
the p=200 median landing *below* p=1 is run-to-run noise, ~0.4 ms).
Window validation pays one DISTINCT-key scan over the `(s, h]`
population (~1.1 ms at ~1k events) plus Rust-side interval matching.
One caveat the sweep itself creates (review A2): the three w-phases
share one growing log — each committed sample appends into `(s, h]` —
so w=50 also scans a ~40% larger population than w=1, and the +2 ms
from w=1→w=50 is an upper bound that conflates window count with log
growth rather than isolating interval matching. Sustained
280 commits/s is the observed throughput of this implementation and
configuration, consistent with an fsync-per-commit regime — no ceiling
claimed, and no durability-off control was run to isolate the cause.
Cross-transaction group commit is NOT a drop-in lever here (re-referee
F15): the lease-row lock is held until COMMIT completes, so batching
several logical transactions would require a modified fence.

## B4 — write path end-to-end (EQ3/EQ4)

`bench_v07 e2e`: the full plumbing loop per transaction (the read adapter
and the commit log are separate histories — the paper's Section 5 states
how to read this), serial —
`InitialCapsule` (session mint) → fresh V8 isolate compiles + runs real
JS (`Convex.asyncSyscall`: one `1.0/get` that traps and hydrates the
capsule from the Convex-schema fixture via Postgres, one `1.0/insert`
that builds the write set in-cell) → `Commit` verb over UDS with
(sealed capsule, consumed_reads, write_set) → `WritePlane` fence in
Postgres. Both sides are real: the read store is the Postgres Convex
adapter, the fence is the Postgres write plane (same DB, the two
timestamp spaces bridged by seeding the log tip to the fixture snapshot
— the F9 scope cut the paper states). Cell asserts `traps == 1` and
`Committed` per transaction. N=500 after 30 warmup; one long-lived
brokerd process; fresh V8 isolate per transaction (V8 *process* init
amortized — the honest analog of a warm cell pool, stated as such).

| Leg | median | p95 |
|---|---|---|
| cell exec (InitialCapsule + V8 boot/compile/run + 1 hydrate trap) | 2.44 ms | 2.78 |
| commit (UDS Commit verb + seal verify + fence + WAL flush) | 4.03 ms | 4.42 |
| **total per transaction** | **6.49 ms** | **7.10** |

**Sustained: 152.9 tx/s serial** (500 committed transactions in 3.27 s).

**Interpretation.** The commit leg (4.03 ms) lands close to the isolated
fence with one point validation (B3: 4.29 ms). This campaign has no
paired A/B control (re-referee F15), so the supportable claim is that no
write-path apparatus overhead (UDS round trip, session gate, seal
verification, B-SUBSET declaration check) was isolated by this
comparison — not that it is zero — consistent with B2's 0.035 ms
apparatus floor at small capsules; policy and launch-security mechanisms
contribute no cost because they do not exist yet. The exec leg (2.44 ms)
buys a fresh isolate, ESM compile, one authenticated read (two fixture
queries + reseal), and the in-cell write set. End to end, a fenced
mutation from inside an untrusted V8 cell costs ~6.5 ms warm-process on
stock Postgres against Aster's own log — about 1.9× a blind fence
append, consistent with a synchronous-commit-dominated regime.

## Cross-run stability and clean-checkout reproducibility

The four benches were also exercised in three earlier same-day shakedown
runs (same machine, same configs) with medians within run-to-run noise
of the canonical numbers above (blind commit 3.50 vs 3.51 ms; e2e total
6.57 vs 6.49 ms; reseal n=1000 0.721 vs 0.697 ms; B1 distinct marginal
0.930 vs 0.930 ms/trap). Shakedown logs were not preserved — these are
summary medians only; the committed raw logs are the canonical run's.

Additionally, the whole campaign was re-run from a **clean `git clone`**
of branch `v0.7-s10` into a scratch directory with its own empty
`CARGO_TARGET_DIR` (cold build + all four benches, exit 0) — proving the
harness runs end-to-end from a fresh checkout. Its medians, canonical
first / clone second: warm-hit assertion 1 trap / 1 trap; B1 distinct
marginal 0.93 / 1.03 ms; reseal n=1000 0.697 / 0.674 ms; blind commit
3.51 / 3.70 ms (280 / 266 commits/s); conflict-abort 1.75 / 2.21 ms;
e2e total 6.49 / 6.91 ms (153 / 143 tx/s). Honest read of that drift
(review A0): the headline medians moved low single digits (blind +5%,
e2e +6.5%, reseal −3%), but the small-N un-warmed phases moved more —
conflict-abort +26% (N=100, no warmup pass) and B1's distinct marginal
+11% (N=12 at 1 ms granularity) — under the clone's warmer background
load right after its cold compile. Same shapes, same conclusions;
"within run-to-run noise" holds for the headline metrics, not
uniformly. The clone run's logs were likewise not preserved: its
evidence is this summary plus the exit-0 transcript.
