# Aster Runner v0.3 absurd-ideas register

These are intentionally not roadmap commitments. They are ideas that sound implausible at first read but are concrete enough to falsify.

## Absurd idea 1: Execute most queries with zero database access by shipping probabilistic “dream capsules”

**Hypothesis:** For a large fraction of hot Convex queries, the next read set is predictable enough that the broker can pre-ship a capsule generated from a learned model—readset fingerprints, Bloom filters, and key PageRank—so the runner never traps and the broker often does no synchronous read work on the invocation path.

**Argument:** Many app queries are template-shaped: current user, project, membership rows, latest N messages, feature flags. v0.2's benchmark shows warm capsules are materially cheaper than cold trap capsules. If learned prewarm has high precision, the system buys latency and reduces read-pool bursts. Incorrect predictions are safe: extra capsule bytes cost bandwidth/memory; missing reads trap and hydrate normally.

**Falsification:** Record `(function, args fingerprint, read set)` for real Synapse/Convex workloads. Train a simple predictor with no neural net first: LFU by function, args-keyed cache, Bloom filter per function, then compare against a learned model. If p95 capsule bytes explode or trap reduction is weak on held-out traffic, dream capsules are false.

**If true, consequences:** The broker becomes partly a readset prediction engine. Scheduler routing should use predicted readset fingerprints. Operators need metrics for wasted capsule bytes versus traps avoided.

**If false, consequences:** Use conservative last-readset prewarming only; correctness is unchanged because traps remain the fallback.

**Status:** speculative.

## Absurd idea 2: Use zk proofs only for capability compliance, not for JavaScript correctness

**Hypothesis:** We do not need to prove “this JS program computed the right answer.” We might prove the narrower property “this result's declared read set is exactly the set of capsule keys touched by the host database API, and no undeclared database capability was used.” That compliance proof could be tractable where full V8 zk execution is not.

**Argument:** Aster's high-risk boundary is authority, not arithmetic integrity. The committer already checks OCC; JS bugs are tenant bugs. A proof over a small syscall transcript—capsule seal, read trap sequence, read observations, write set derivation labels—may be orders of magnitude smaller than proving V8 execution. The cell would emit a transcript; a verifier checks that every database-originating value came from a sealed capsule or broker delta.

**Falsification:** Build a transcript VM for the `Aster.read` subset and use a 2025 proving stack (e.g. RISC Zero/SP1/Halo2-style circuits) to prove 1,000 reads under 100 ms prover time or under an operator-acceptable async audit budget. If proof time or transcript size is absurd, drop it.

**If true, consequences:** High-risk tenants can run in “verifiable capability mode” where results are accepted only with a compliance proof or sampled proof. This could reduce trust in cell code after a sandbox escape.

**If false, consequences:** Keep seals, OS sandboxing, and audit logs. The architecture does not depend on zk.

**Status:** speculative.

## Absurd idea 3: Cells should be killed on a schedule even when healthy, because caches are authority residue

**Hypothesis:** Long-lived warm cells improve latency but accumulate authority residue: module code, capsule fragments, timing side channels, and allocator state. A production multi-tenant runner should intentionally kill and reincarnate cells based on a risk score, not only on crashes or deploys.

**Argument:** Browser processes and workerd isolates both treat reuse as a performance/security trade. Aster cells are tenant-pinned, so killing a cell loses only one tenant/deployment cache. If capsule seals and module snapshots make restart cheap enough, periodic reincarnation can bound the lifetime of post-exploit access and flush microarchitectural residue.

**Falsification:** Measure p95 latency and CPU cost with cell TTLs of 30s, 5m, 30m, and deploy-only across real workloads. Run a side-channel/leak harness that tries to recover prior capsule values across invocations. If TTLs hurt latency badly and do not reduce measurable risk, reincarnation is unjustified.

**If true, consequences:** The supervisor gains risk-class policies: public-hostile tenants get short TTLs and gVisor; trusted internal tenants get long-lived hot cells. Synapse exposes this as “cell reincarnation interval.”

**If false, consequences:** Keep kill-on-deploy/delete/key-rotation and rely on cache hygiene plus OS sandboxing.

**Status:** speculative.

## Absurd idea 4: Run the broker as a database read “diode” with no request parser in the cell trust domain

**Hypothesis:** The safest broker/cell interface might not be request/response RPC. It could be a one-way shared-memory or pipe protocol where the cell can only append fixed-size trap descriptors, and the broker writes capsule deltas to a separate sealed channel. The cell never sends protobuf/JSON to the broker.

**Argument:** Parser bugs in privileged brokers are a classic escape path. Aster traps are structurally small: point key, prefix key, limit, prior digest. A ring buffer with fixed records and kernel-enforced directionality would reduce attack surface. This sounds excessive for a VPS, but it directly targets the most security-sensitive boundary. v0.3 makes the motivation concrete: `aster_brokerd` now has a JSON parser on the privileged side of a UDS boundary, with a frame cap but no deep parser hardening.

**Falsification:** Implement a UDS/protobuf broker and a ring-buffer broker, fuzz both, and measure throughput/latency under 1M traps. If protobuf+strict validation is simpler and no slower enough to matter, the diode is overengineering.

**If true, consequences:** Production cells use a tiny trap ABI and the broker's parser surface shrinks dramatically.

**If false, consequences:** Use tonic/UDS with strict message size caps and fuzzing.

**Status:** speculative.

## Absurd idea 5: Give every cell a private broker socket that self-destructs after one invocation

**Hypothesis:** Instead of authenticating cell identity on a shared broker socket, the supervisor could create a fresh socketpair or private UDS path per invocation, pass one endpoint to the cell, and have the broker destroy the channel after the final capsule seal. The socket itself becomes a short-lived capability.

**Argument:** v0.3's shared socket needs peer credentials, cell IDs, lease epochs, and replay protection. A one-shot channel could collapse several checks into the kernel object lifetime and supervisor handoff. It sounds operationally clumsy, but function invocations already have lifecycle boundaries and trap budgets.

**Falsification:** Implement shared-socket and one-shot-socket modes, then compare p50/p99 setup overhead, leaked socket cleanup after crashes, and security bugs found by replay tests. If setup overhead dominates or cleanup is unreliable, keep the shared broker socket.

**If true, consequences:** The production supervisor becomes more important, but broker authentication gets simpler: the accepted connection already names one invocation and one cell lease.

**If false, consequences:** Keep the shared UDS broker and harden it with `SO_PEERCRED`, sequence numbers, nonces, and per-cell rate limits.

**Status:** speculative.
