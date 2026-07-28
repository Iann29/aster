# Re-referee round 2 — hostile referee report (GPT Pro, 2026-07-16)

Recebido via colagem do Ian em 16/07/2026. Input: `paper/rereferee-v07-prompt.md`
+ anexos (paper draft, CLAIMS, write_plane.rs, aster_brokerd.rs, fence.rs,
AsterFence.tla + RESULTS, bench RESULTS, ctt.txt). Veredito: **REJECT** do
paper-como-sistema; teorema intacto; kit de reparo F1–F18 com replacement
sentences. Preservado verbatim abaixo.

---

## Verdict: **REJECT**

The repaired Capsule Transaction Theorem remains a credible conditional result, and several individual mechanisms are well designed: direct-MAC seal v3, downgrade rejection, the Postgres lease/retention locking discipline, the F2 Boundary/Exhausted semantics, and the post-query floor-check argument under the right storage contract.

That is not enough for this paper.

The shipped pipeline does **not** instantiate the theorem's central premise that reads and conflict validation range over one authoritative history. The read plane observes Convex tables; the fence validates and appends against an independent `aster.log`. The benchmark numerically aligns the two timestamp spaces by seeding the Aster tip, but it does not make the histories identical or make writes on one plane visible to the other. Thus the paper's "full theorem loop," "end-to-end," "strictly serializable mutation," and 6.5 ms headline substitute a plumbing experiment for the security experiment the theorem requires. This is a fatal flaw in the **system-paper claim**, although not in the abstract theorem.

There are additional acceptance blockers: the implemented policy is effectively coarse allow-all within a deployment; session binding is bearer-capability binding rather than trusted cell identity; the actual protocol leaks substantially more than the stated T1b transcript; shipped "Variant B" is Variant B for points but Variant A for ranges; logical document identity and key ordering are not canonical end-to-end; the formal companion report does not prove the shipped v3 protocol; the TLA+ model assumes the serialization property it is described as checking; and the evaluation makes several causal claims that the measurements do not isolate.

---

# 1. Theorem-to-code fidelity

Legend: **Yes** means the theorem survives as stated; **Qualified** means only after narrowing the statement; **No** means the shipped composition violates a required premise.

| Instantiation delta | T1a | T1b | T2 | Ruling |
| --- | ---: | ---: | ---: | --- |
| 1. Random session table; self-asserted `cid` at mint; authoritative epoch | **Qualified** | **Qualified** | **Yes, locally** | T1a authenticates issuance to a broker-minted **bearer session**, not to a physical cell identity. T1b survives only if `cid` is not used for authority. T2 does not inherently depend on trusted `cid`. |
| 2. Seal v1→v2→v3, direct MAC, constant-time compare, v3-only verification | **Yes** | **Yes** | **Yes** | Cryptographically stronger than the theorem's old prehash formulation. The problem is documentary: the governing technical report is stale. |
| 3. Variant B subset; duplicates rejected; committer derives windows | **Yes** | **Yes** | **Qualified** | T2 survives for the **actual** set validated. The implementation is B for point reads but validates every range certificate, so it is not the exact Variant B described in the paper. |
| 4. Exhausted/Boundary rule and negative Boundary case | **Yes** | **Yes** | **Qualified** | The rule is sound for first-ℓ semantics, assuming the storage scan order and Rust window order are identical and A6 holds. |
| 5. Retention watermark clamped to tip | Unaffected | Unaffected | **Yes** | This is not part of T2 safety. Add a separate retention well-formedness/liveness proposition; do not contaminate T2 or Lemma R. |
| 6. One session = one attempt; structured Commit/Abort closes it | **Qualified** | **Yes** | **Yes, currently** | This is stateful anti-replay at the session layer, not an extension of cryptographic T1a. It works because the broker currently handles requests serially. Parallel handling would require atomic consume-before-commit. |
| 7. Post-query floor guards on both read planes | **Qualified** | **Yes** | **Qualified** | Correct on the Aster plane because floor publication and deletion are transactional. On the Convex plane, monotonicity alone is insufficient: the artifact must establish the vacuum/floor publication contract. |
| 8(i). Boot-pinned Convex reads plus independent Aster writes | **Per-plane only** | **Per-plane only** | **No** | The composite pipeline does not satisfy the shared-history premise of T2. |
| 8(ii). IDv6/raw aliases accepted below a string-keyed protocol | **Bytes only** | **Qualified** | **No over logical documents** | T1a authenticates one spelling. It does not prove that two strings are not the same logical document. Alias mixing can defeat RYOW and conflict projection. |

The implementation is therefore not one uniform instantiation. It contains sound components, but the theorem does not compose across the actual read/write seam.

---

# 2. Findings

## F1 — **Critical** — The "end-to-end" system has two unrelated transactional histories

The paper acknowledges that Convex reads and Aster commits use different tables and that the write plane never updates Convex. It then immediately says "The loop is closed" and calls the associated test and benchmark end-to-end. Disclosure does not cure contradiction.

In normal Postgres broker startup, the capsule snapshot is obtained from the Convex read store, while the newly created Aster log may have tip zero. No production bridge is created. For a nonzero Convex snapshot, the Aster fence can return `SnapshotBeyondHorizon`. The benchmark special-cases this by seeding the Aster log tip to the fixture snapshot. That aligns two integers; it does not align histories.

A concrete T2 counterexample is immediate:

1. The broker seals a read of Convex key `x` at snapshot `s`.
2. The live Convex writer changes `x` after `s`.
3. No corresponding event enters `aster.log`.
4. The cell declares `x` and writes `y`.
5. The Aster fence finds no write to `x` in its own suffix and commits.

The accepted read is stale at the append point. The backward-validation induction cannot start because its conflict history is not the history that produced the read.

**Required implementation repair:** either become the Convex committer or route validated writes through the authoritative Convex commit path, including complete index conflict projection. The read snapshot, validation suffix, append, retention floor, and epoch must all refer to that same history.

**Required replacement sentence:**

> "v0.7 exercises the cell→broker→fence plumbing across a Convex-schema read adapter and an independent Aster commit log. Because those planes do not share an authoritative history, this experiment does not instantiate T2 end-to-end; T2 applies to the standalone Aster plane and conditionally to a future live-committer integration."

Delete "The loop is closed," "full theorem loop," and every description of the 6.5 ms result as a strictly serializable end-to-end database mutation.

---

## F2 — **Major** — The implemented authority policy is effectively allow-all within one deployment

The abstract says the executor cannot "cross a policy"; the design says `P.read` is enforced before storage access and `P.write` is linearized with append. The attached broker and write plane contain no such checks. They pin one configured tenant and deployment, but any key in that namespace can be read, prewarmed, module-loaded, or written. The fence receives a supposedly "policy-authorized" write set from its caller without enforcing or versioning a policy.

A fixed immutable policy saying "all keys in this broker's deployment are allowed" is a legitimate trivial instantiation. But then:

* T1b means only that the cell cannot read outside the broker's whole deployment.
* T3's malicious-but-authorized equivalence is correspondingly weak.
* The claims about per-key policies, revocation, policy TOCTOU, and disjoint tenant authority are unimplemented.
* The performance numbers omit the policy mechanism included in the claimed "capability apparatus."

T2 itself does not require fine-grained policy, so history safety can survive this instantiation. T1b/T3's advertised security consequence does not survive without being narrowed.

**Required replacement sentence:**

> "The v0.7 prototype instantiates policy as fixed deployment-wide authority: a connected session may read and write any supported key in that broker's configured tenant and deployment. Fine-grained `P.read`, `P.write`, policy versioning, and revocation linearization remain unimplemented."

Until real policy is built, delete "cannot cross a policy" from the abstract and conclusion.

---

## F3 — **Major** — C-CHANNEL is bearer-session binding, not trusted cell or channel binding

The random 32-byte session is sound as an unguessable capability. It prevents a capsule issued under session A from verifying under session B. It does not establish that the requester is the physical cell named by `cid`.

At mint:

* `cid` comes from the hostile request.
* The session is returned to that requester.
* Subsequent requests use new Unix connections.
* The server verifies possession of the bearer token, not socket continuity or trusted launch identity.
* The attached code does not obtain `SO_PEERCRED`, verify a launch token, or establish socket ACLs itself.

Consequently, a policy keyed by `cid` is immediately bypassed: a hostile cell asks to be any `cid` and receives a fresh valid session for that label. T1a survives only in this narrower form:

> An accepted capsule was issued for the presented broker-minted bearer session and its registered self-asserted label.

That is not "wrong-cell rejection."

The unauthenticated `Shutdown` verb further demonstrates that the UDS is not itself being treated as an authenticated security channel. Denial of service may be out of theorem scope, but it is evidence against the paper's channel language.

**Required replacement sentence:**

> "v0.7 implements per-attempt bearer-capability binding: possession of a broker-minted random session is required for subsequent verbs. It does not yet bind that session to a trusted physical-cell identity; `cid`-dependent authority remains conditional on a launch token or verified peer credential."

The code repair is to mint the session only in response to broker-owned launch metadata and bind it to a socket peer credential or unforgeable launch capability.

---

## F4 — **Major** — The session repair and the claimed read-plane scale-out do not compose

Corollary C1 says any trusted key holder able to read the snapshot may verify, grow, and reseal a capsule. The shipped session mechanism requires an entry in an **in-process** session table. A capsule minted by broker A cannot be hydrated by broker B, despite both holding the MAC key, because B lacks A's table entry. At best, the implementation permits sticky-session scale-out, not the stateless any-broker scale-out stated by C1.

The shipped Postgres broker also always acquires the writer lease at boot. Launching another supposedly read-only broker advances the writer epoch and renders sessions from existing brokers stale. There is no read-only broker mode. Thus adding read capacity perturbs write authority—the opposite of C1's operational claim.

A second latent composition bug is session consumption. Commit performs lookup, executes the fence, and removes the session afterward. The serial accept loop makes double consumption impossible today. If the broker is parallelized—the obvious route toward the advertised scale—two requests can resolve the same session before either removes it and can both reach the fence. T2 still orders two successful commits, but "one session = one attempt" stops being true.

**Required replacement sentence:**

> "Corollary C1 is an architectural result, not a v0.7 implementation claim. The current broker requires sticky routing to its in-process session table and always acquires the writer lease in Postgres mode; read-only scale-out and atomic session consumption remain future work."

The code needs a read-only mode plus either self-verifying launch tokens or a shared authoritative session service. Commit must atomically consume the session before fence execution.

---

## F5 — **Major** — The implementation leaks more than T1b's stated transcript

The theorem and paper repeatedly emphasize a "conflict bit." The wire outcome exposes:

* the exact conflicting key;
* the current lease epoch;
* the exact horizon;
* the exact retention low watermark;
* the successful commit timestamp.

Returning an exact conflict key is not one bit. For a range certificate, it can disclose the name of a post-snapshot inserted key that was never returned in the grant. That may be inside an authorized range, but the theorem still has to name it. Exact epoch and watermark values similarly provide a richer activity oracle than a rejection class.

There is also an entirely separate broker-to-cell data channel: `LoadModuleBundle` verifies a capsule, accepts a cell-chosen path, reads module bytes from the broker's source, and returns them unsealed. The paper's sentence "Every datum the cell ever sees arrives as (part of) a sealed capsule" is literally false. Whether module code is public, confidential, or trusted launch input, it must be represented in the ideal functionality.

**Required replacement sentences:**

> "The whole-protocol transcript reveals the exact conflicting key and the returned epoch, horizon, retention-floor, and commit-timestamp values; these are explicit protocol leakages, not side channels."

> "Module bundles are broker-provided launch inputs outside the capsule read theorem. Their authorization and confidentiality are not covered by T1b."

A cleaner implementation would return opaque conflict/retry classes and resolve modules from a broker-pinned manifest rather than a hostile path.

---

## F6 — **Major** — "Variant B as shipped" is false for range observations

The technical report defines Variant B as an exact subset of both point and range atoms. The wire request contains only `Vec<DocumentId>` declarations. At commit, the broker validates declared point keys but unconditionally converts **every** sealed range certificate into a conflict window.

The actual rule is:

R = S_points ∪ AllRanges(Cap).

This is safe and conservative. T2 survives. But it is a hybrid of Variant B for points and Variant A for ranges. It also invalidates the paper's ergonomics argument: an unused prewarmed or speculative scan still creates abort amplification.

**Required replacement sentence:**

> "v0.7 implements Variant B for point observations and conservative whole-capsule validation for range certificates. Exact declared range subsets, and the corresponding no-false-conflict prewarm claim for ranges, are not yet implemented."

Alternatively, add stable range-certificate identifiers to `declared_reads` and enforce exact membership.

---

## F7 — **Major** — Logical key identity and ordering are not canonical across the proof boundary

The paper admits that IDv6 and raw-wire spellings can refer to the same Convex row while every protocol layer above the store treats them as distinct strings. It then dismisses the problem as confined to hand-rolled native callers. That dismissal contradicts the threat model. A Byzantine executor that can speak the UDS protocol **is** a hand-rolled native caller. It is not restricted to the honest JS shim.

This can defeat:

* read-your-own-writes;
* duplicate-write detection;
* point conflict detection;
* write policy keyed by string;
* A6 complete conflict projection.

T1a authenticates the spelling, not the logical document identity. T2 over the intended Convex document model therefore fails.

There is a second identity problem: range correctness assumes one total order (K). The SQL scan uses PostgreSQL `text`, `ORDER BY key`, and `LIKE`, while window membership is evaluated with Rust string ordering. No binary-collation contract is stated or enforced. Under a locale collation that differs from Rust's ordering, a Boundary certificate can protect the wrong prefix. This can produce a missed phantom without forging a capsule.

**Required replacement sentence:**

> "Before policy, sealing, RYOW, declaration, conflict projection, or append, every document identifier is canonicalized to one wire representation; noncanonical aliases are rejected. The protocol order is bytewise and is enforced identically in PostgreSQL and Rust."

The implementation should use canonical IDv6 only, reject raw aliases at the broker, and use `bytea` or an explicitly indexed `COLLATE "C"` representation whose prefix and order semantics are tested against Rust.

---

## F8 — **Major** — Monotone floor publication is not sufficient to justify the Convex post-read guard

The post-query check is directionally correct. If the floor is authoritative and monotone, observing `floor ≤ s` after the query proves it was also ≤ s earlier.

But monotonicity alone does not establish that history was present while the query ran. The required storage contract is stronger:

1. no revision covered by snapshot (s) is removed while the published floor is ≤ (s); and
2. deletion and floor advancement are atomic, or the floor is advanced conservatively before deletion becomes visible.

The Aster write plane meets this because it updates the floor and deletes rows in one transaction. The attached materials do not establish the corresponding publication contract for Convex's `min_document_snapshot_ts`. If Convex vacuum can delete before atomically publishing the new floor, the broker may seal false absence evidence. T1a would then faithfully authenticate a lie from a "trusted" broker, and T2's stability lemma would not apply.

**Required replacement sentence:**

> "The post-read floor check is sound provided the store guarantees that floor advancement and removal are atomically ordered: no history needed by snapshot (s) becomes invisible while the published floor is at or below (s). The Aster plane enforces this transactionally; the Convex vacuum contract remains to be verified."

A real race test must run against the actual Convex vacuum implementation, not merely a mock floor.

---

## F9 — **Major** — The governing formal report does not prove the shipped v3 protocol

The paper says:

* v3 direct-MAC with session frame is shipped;
* A3 is retired;
* only v3 verifies;
* where paper and report differ, "the report governs."

The report instead formalizes the prehash seal, includes unkeyed-hash collision resistance in T1a, treats direct MAC as a future alternative, lacks the session frame in the formal message, and describes much of the end-to-end write path as proposed.

The v3 proof is probably straightforward, but "probably straightforward" is not a proof artifact. The outer-message injectivity lemma must include the session frame, the T1a reduction must remove the collision branch, and the reality ledger must match the code. The current appendix pointer turns harmless version skew into a formal contradiction.

**Required replacement sentence until the report is updated:**

> "The companion report proves the repaired abstract protocol and the retired prehash seal. Seal v3's direct-MAC/session framing is an implementation refinement whose corresponding formal reduction is included in this paper; where the artifacts differ, this paper governs v3."

The preferable repair is to update and reissue the report, then retain "the report governs."

---

## F10 — **Major** — The TLA+ model assumes atomic fencing instead of checking its implementation

The model has a single `inflight` fence, and `FenceBegin` requires `inflight = None`. Lease acquisition also requires no in-flight fence. This encodes the global row-lock serialization that the paper says the model checks. The model then verifies consequences of that serialization, including no write skew.

The negative configurations have useful teeth for:

* epoch reuse;
* loss of the retention pin.

They do not include the central A-ATOMIC mutant: two fences validate against the same horizon and both append. Therefore the sentence that TLC model-checks "validation/append atomicity" is too strong. TLC checks the abstract serialized-fence design; SQL tests provide evidence that Postgres realizes its guards.

The model's own abstraction-gap section correctly admits that row locks and transaction atomicity are assumed. The main paper erases that qualification.

**Required replacement sentence:**

> "TLC checks the safety consequences of an abstract fence whose lease-row serialization is assumed, together with epoch-reuse and no-retention-pin mutants. It does not verify that the Rust/Postgres implementation establishes those guards."

Add a no-fence model with two simultaneously validated transactions; it should produce Counterexample 3.9. A stronger model would also distinguish lock acquisition, statement failure, rollback completion, and session return.

---

## F11 — **Moderate** — The retention clamp belongs in a separate liveness/well-formedness result

The clamp does not strengthen T2 or Lemma R. T2 and Lemma R are safety statements: if a transaction commits after complete retained-suffix validation, its observations are legal. A floor above the tip prevents commits; it does not create an illegal successful commit.

However, the paper already makes the operational statement that an expired transaction "must retry from a fresh snapshot." That statement silently assumes a fresh snapshot is admissible. The missing invariant is:

R0: 0 ≤ g ≤ tip(L).

Under R0, a snapshot at the current tip passes retention coverage. The clamp is the implementation mechanism preserving R0. The TLC report itself says no liveness is checked.

There is also an upgrade edge: `current.max(requested.min(tip))` prevents a new above-tip floor but does not repair a preexisting `current > tip` state left by an older binary. The paper says the bug was found before deployment, which may make this irrelevant operationally, but the invariant should be checked at startup.

**Required theorem addition:**

> "Invariant R0 requires 0 ≤ g ≤ tip(L). Under R0, a current epoch, stable policy, retained snapshot s = tip(L), and no intersecting write, an honest attempt is obstruction-free. `advance_retention`'s tip clamp is the implementation discharge of R0."

Do not add liveness language to T2 itself.

---

## F12 — **Major** — The artifact does not instantiate the threat model used by the abstract and conclusion

The paper's abstract and architecture present these as facts:

* no network;
* no filesystem;
* only the UDS;
* a production-secret MAC key;
* trusted launch identity.

The implementation section later lists all of them as remaining obligations:

* egress-blocked containers are not built;
* trusted launch binding is not built;
* the broker uses a test-fixture key derivation helper;
* the cell still receives epoch data through its environment;
* explicit resource caps are absent.

This is not harmless deployment detail. The conclusion's "a V8 escape stops being a database incident" relies on OS process isolation against native code. A V8 escape in a same-UID, non-hardened process can potentially access host files, process metadata, other Unix sockets, or network egress. The paper itself concedes that without egress blocking the confinement pitch collapses.

The theorem can retain A12 as an assumption. The **system paper** may not write A12 in the indicative mood until the artifact establishes it.

**Required replacement sentence:**

> "Under a deployment that enforces A12 with OS-level egress, filesystem, process, and socket isolation—and that provisions κ from a production secret store—a V8 compromise adds no database authority beyond the broker protocol. The v0.7 artifact does not yet provide that deployment profile."

Delete "a V8 escape stops being a database incident" and "the attacker holds an empty isolate" from the unconditional conclusion.

---

## F13 — **Major** — "Runs unmodified Convex bundles" overstates compatibility

The implementation supports a selected syscall subset. The claim ledger itself says that `insert` requires an explicit `_id` and rejects "mint me an ID," while ordinary Convex inserts generate document IDs. The supported query and mutation surface is also far narrower than the full Convex runtime API.

Demonstrating that a selected bundle produced by `npx convex deploy` can be parsed and executed without source editing does not establish general compatibility with unmodified Convex applications.

This matters because "unmodified bundles end-to-end" is an abstract contribution, not a footnote.

**Required replacement sentence:**

> "Aster executes selected unmodified `npx convex deploy` bundles that use the implemented syscall subset; v0.7 does not claim compatibility with the complete Convex query, mutation, scheduling, generated-ID, or reactive runtime semantics."

The artifact needs a compatibility matrix and a corpus of independently selected applications, not only the benchmark bundle.

---

## F14 — **Major** — The 6.5 ms result is a plumbing benchmark, not the cost of the theorem

The measured path contains real V8 execution, UDS traffic, capsule verification, and a real Postgres fence. What it does not contain is the security composition advertised in the abstract:

* no shared authoritative read/write history;
* no live Convex committer integration;
* no complete A6 index projection;
* no `P.read` or `P.write`;
* no policy versioning;
* no trusted launch binding;
* no OS-level A12 deployment;
* no fresh-snapshot retry path;
* no production key provisioning.

The benchmark uses one long-lived broker process and amortizes V8 process initialization; it creates a fresh isolate, not the one-OS-process-per-invocation architecture shown in Figure 1. It reads one point and writes one key.

Therefore, the number supports:

> 6.49 ms median for the tested capsule/session/fence plumbing in a warm process under a serial one-read/one-write workload.

It does not support:

> 6.49 ms for an authenticated, strictly serializable live-backend mutation under the stated threat model.

**Required abstract sentence:**

> "In a warm-process plumbing benchmark with a fresh V8 isolate, one Convex-fixture point read and one write to an independent Aster log complete in 6.49 ms median; this benchmark does not include live Convex committer integration or instantiate T2 across the two planes."

That sentence is less attractive because it is what was measured.

---

## F15 — **Major** — "Statistically the same," "nothing measurable," and "WAL dominated" are not supported

The 4.03 ms commit-leg median and the 4.29 ms isolated-fence median come from different benchmark paths and populations. There is no paired A/B control, confidence interval for their difference, hypothesis test, or apparatus-disabled run. Calling them "statistically the same" is unjustified. Calling the apparatus overhead zero is stronger still.

Likewise, `synchronous_commit=on` and the cheaper abort are consistent with WAL-flush dominance, but no `synchronous_commit=off` control, WAL statistics, storage trace, or device-latency measurement isolates that cause. "fsync-per-commit ceiling" is an inference, not a measurement.

The abort measurement has another problem. Conflict returns from `WritePlane::commit` without an explicit awaited `rollback()`. `tokio-postgres` implicitly rolls a dropped transaction back; its explicit async rollback is described as equivalent but reports completion or error. The measured call may therefore stop before server-confirmed rollback and row-lock release.

Finally, the paper says group commit is the standard lever and "nothing in the fence forbids it." The global lease-row lock is held until COMMIT completes. A second Aster transaction cannot reach its own commit phase concurrently, so ordinary cross-transaction PostgreSQL group commit is effectively precluded. Batching several logical transactions would require a different fence implementation.

**Required replacement sentence:**

> "The results are consistent with synchronous-commit latency dominating this configuration. The 4.03 ms and 4.29 ms medians are close, but this campaign does not statistically isolate capability overhead or identify its lower bound."

For the abort figure:

> "Conflict handling returns in 1.75 ms before an explicit awaited rollback; this is decision latency, not yet a measurement of completed rollback and lock release."

---

## F16 — **Major** — The evaluation misses the implementation's actual scaling variables

The point-validation experiment tests p=1…200 over an empty `(s,h]` suffix. It establishes that serializing an `ANY` parameter over an empty result is cheap. It does not establish point-validation cost under a large suffix or with matching/nonmatching events.

The window implementation executes:

1. a query returning **all distinct changed keys** in `(s,h]`;
2. Rust-side matching of those keys against every observed window.

Its natural variables are suffix cardinality, distinct-key count, window count, interval shape, and hit position. The reported sweep varies window count while also growing the suffix, so none of those effects is isolated. The paper declares this particular confounding, but it does not address the larger algorithmic issue: validation work can scale roughly with changed keys × windows.

Missing measurements that matter to the claimed design include:

* suffix length at fixed p and fixed w;
* w at a reset fixed suffix;
* early conflict versus no conflict;
* one versus many writes;
* large documents;
* an end-to-end range-certified mutation;
* absence/tombstone workloads;
* queued latency under concurrent callers;
* abort rate and useful throughput under contention;
* conflict followed by actual fresh-snapshot retry;
* failover recovery;
* read-only scale-out.

The last retry experiment cannot currently succeed without broker relaunch because the snapshot is boot-pinned.

**Required replacement sentence:**

> "Point-set cost is flat only for the tested empty suffix. Window results conflate window count with suffix growth, and no claim is made about multiwrite scaling, contention, queued latency, fresh-snapshot retry throughput, or range-heavy workloads."

A venue-quality evaluation needs a factorial sweep over p, w, suffix population, writes per transaction, and concurrency.

---

## F17 — **Moderate** — The artifact's own honesty ledger is internally inconsistent

`CLAIMS.md` says there are sixteen Postgres integration tests. Its bookkeeping section says the count is fifteen and explicitly notes that the paper and ledger say sixteen. That is precisely the kind of inconsistency an artifact committee will interpret as evidence that the claimed revision and evaluated revision are not frozen.

Other reproducibility weaknesses:

* clean-clone and shakedown raw logs were not preserved;
* the clone's conflict-abort median moved by 26%;
* the TLC reproduction command downloads the "latest" JAR rather than a pinned hash;
* the artifact needs one immutable source commit corresponding to the paper, claims ledger, tests, model, and measurements;
* container images should be pinned by digest;
* benchmark phase order should be randomized or reset to prevent temporal/log-growth bias.

The canonical raw logs may be adequate to reproduce the headline medians, but the claims about cross-run stability are not independently auditable without the omitted runs.

**Required replacement sentence:**

> "All paper numbers are generated from an immutable artifact commit; all canonical, clean-clone, and repeat-run raw logs are retained; Postgres and TLC dependencies are pinned by digest; and CI verifies test, LOC, and proof-count claims before paper generation."

An artifact committee could reasonably fail Table 7 on semantic validity even if it can reproduce 6.49 ms exactly.

---

## F18 — **Major** — Related-work discipline and motivating facts are not submission-ready

### Incident accuracy

The paper says Sonrai and Unit 42 reported the AgentCore issue "in February 2026." Sonrai's article is dated September 4, 2025 and was updated March 25, 2026; Unit 42's separate report was published April 7, 2026.

The Supabase paragraph is written as an incident in which an MCP server was prompt-injected into dumping a table. The cited 2025 discussion describes a possible constructed attack scenario. Supabase states that there had been no reported customer MCP data-leak incident.

**Required replacement sentences:**

> "Sonrai reported an AgentCore credential-exfiltration path on September 4, 2025; Unit 42 separately published network-isolation and metadata-service findings on April 7, 2026."

> "A 2025 demonstration described how a misconfigured high-privilege Supabase MCP deployment could be prompt-injected into exposing database contents; it was a demonstrated scenario, not a reported Supabase customer breach."

### Fides

The Fides abstract claims a novel atomic-commit protocol over untrusted servers without expensive Byzantine replication and an auditable system that detects malicious failures. That supports the paper's no-BFT precedent and prevention-versus-detection comparison. It does not, by itself, support the exact sentence "Fides claimed exactly serializable transactions on untrusted infrastructure without BFT." Cite the specific Fides theorem that establishes that formulation or narrow it.

**Required replacement sentence:**

> "Fides established auditable transaction management, including atomic commitment over untrusted servers without expensive Byzantine replication; Aster's distinction is online admission at a trusted broker rather than post-hoc detection."

### FoundationDB

The mechanism comparison is good: FDB clients submit read/write ranges and resolvers compare them against recently modified ranges. But the freshness comparison is sloppy. FDB's paper says a transaction's read version is at least as new as commits known when it starts, while snapshot reads omit selected conflicts. Aster's boot-pinned read-only snapshot may predate operations completed before invocation. That is a stronger freshness relaxation than ordinary FDB snapshot reads.

**Required replacement sentence:**

> "Like FDB snapshot reads, Aster can omit conflict protection for selected reads; unlike ordinary FDB transactions, the v0.7 prototype may also use a boot-pinned snapshot that is stale at invocation time."

### Novelty discipline

The explicit statements in §1.2 and §7 that no-BFT/no-reexecution/no-TEE follow from the trusted-broker model are appropriately disciplined. Fabric's endorsement model, for example, does rely on peers executing and endorsing transaction results, and the paper's trust-locus distinction is broadly fair.

The discipline fails elsewhere:

* "What nobody does" is an unsupported universal novelty claim.
* "The strongest isolation statement available without trusted hardware" is an unsupported optimality claim.
* "One broker process" in the comparison table is not a comparable cost metric: Aster also relies on trusted storage, OS isolation, key management, and per-invocation execution.
* "A trusted broker makes the enclave unnecessary" must be scoped to executor-to-database authority under Aster's assumptions, not confidentiality, host compromise, or side channels.

**Required replacement sentence:**

> "We are not aware of prior work that combines serve-time broker MACs with commit-time OCC validation specifically to remove a Byzantine application executor from the read-provenance TCB; the no-BFT, no-reexecution, and no-TEE properties follow from assuming a trusted broker and storage system."

Delete "what nobody does" and "strongest isolation statement available."

---

# 3. Direct audit of the abstract and §6 claims

Yes: several sentences claim more than the measurements support.

| Claim | Referee ruling |
| --- | --- |
| "It runs unmodified `npx convex deploy` bundles end-to-end." | **Overclaim.** Only selected bundles using the implemented syscall subset are shown. |
| "One fenced mutation … completes in 6.5 ms end-to-end." | **Overclaim.** It is warm-process plumbing across disconnected read/write histories. |
| "At realistic read-set sizes, the security tax on both paths is a fraction of storage cost." | **Unsupported.** No realistic read-set distribution or upstream baseline is measured; at 1,000 tiny entries, resealing is already comparable to a storage leg. |
| "≈5 µs per warm read inside V8." | **Overclaim.** The experiment establishes only that 199 warm hits add no resolvable whole-process time beyond a 1 ms timer quantum; ≤5 µs is a derived resolution bound, not a per-read measurement. |
| "The quadratic is not yet the bottleneck at realistic read-set sizes." | **Unsupported.** "Realistic" is not characterized, and entries are synthetic ~49-byte objects. |
| "The fence is durability-bound" / "dominated by WAL flush." | **Causal inference, not measured attribution.** Needs a durability-off control or storage instrumentation. |
| "280 commits/s is the fsync-per-commit ceiling." | **Unsupported as a ceiling.** It is the observed throughput of one implementation/configuration. |
| "The commit leg is statistically the isolated fence." | **False methodology language.** No statistical equivalence test or paired experiment is reported. |
| "The capability apparatus adds nothing measurable." | **Unsupported.** The correct claim is that no overhead was isolated by this comparison; policy and launch-security costs are absent entirely. |
| "Both already inside interactive budgets." | **Unmeasured product assertion.** No workload, SLO, or user-facing baseline defines such a budget. |
| "Zero resident-memory pressure on the reactive backend." | **Unmeasured and absolute.** No memory experiment is reported. |
| "Group commit is the standard lever and nothing in the fence forbids it." | **Architecturally misleading.** The global row lock serializes commit completion; batching would require a modified fence. |

The supported evaluation claims are narrower:

* the same-key workload generated one trap;
* the constant-size forced-trap apparatus curve is linear over the tested synthetic capsules;
* cumulative whole-capsule resealing is quadratic;
* the isolated one-write fence measured 3.51 ms median;
* the tested plumbing loop measured 6.49 ms median;
* the Boundary negative case and conflict tests exercise intended local semantics.

Those are useful results. They are not the claims currently built around them.

---

# 4. Threat-model ruling

The threat model is mathematically honest in its assumptions ledger but rhetorically dishonest in the abstract and conclusion.

A12 side channels are not the main problem. The implementation exposes exact conflict keys and metadata through an ordinary protocol channel; this must be included in T1b, not dismissed as a side channel. The larger issue is that the deployed preconditions—egress denial, filesystem/process isolation, trusted launch identity, production key provisioning, socket access control—are still obligations. The paper repeatedly switches from "assume" to "the executor has" and from "future deployment must" to "a V8 escape stops being an incident."

The egress obligation is particularly damaging to the pitch. The paper correctly says that authorized data is trivially exfiltrated without it. Therefore "confinement" is only database-authority confinement, not data-loss prevention. That distinction must appear in the abstract, introduction, and conclusion every time the isolation claim is made.

---

# 5. What survives hostile review

The following pieces are worth preserving:

* **Seal v3:** direct MAC over framed canonical content plus session binding is cleaner than the theorem's original prehash seal. Exact version dispatch and constant-time comparison are the right implementation choices.
* **Commit fence locally:** the lease-row lock followed by stable-horizon read, retention-row pin, conflict scan, and append in one Postgres transaction is a reasonable implementation of A7–A9 for the Aster-owned log.
* **Epoch monotonicity:** increment-under-row-lock matches the repaired failover lemma.
* **F2 scan semantics:** Exhausted protects the full interval; Boundary protects only the observed prefix. The negative "phantom past Boundary does not conflict" case is correct for first-ℓ semantics.
* **Retention post-check on the Aster plane:** with transactional floor advancement/deletion, checking after the read is the correct race direction.
* **Variant B's theorem boundary:** omission remains an authorized blind or less-dependent write; the theorem correctly refuses to call that execution correctness.
* **Session closure under the current serial broker:** it bounds simple replay after a structured outcome, although it is not yet safe under parallel request handling.

These components support a strong technical report or a substantially narrower prototype paper. They do not support the current system-level abstract.

---

# 6. Final acceptance bar

A resubmission at USENIX Security / CCS bar must, at minimum:

1. Put reads, conflict projection, retention, and append on one authoritative history, then rerun the end-to-end tests and evaluation against real interposed writes from that history.
2. Implement or explicitly demote policy, trusted launch identity, key provisioning, and A12 deployment isolation.
3. Canonicalize logical document IDs and storage ordering before claiming T2 over Convex documents.
4. Reconcile the formal report with seal v3 and model the no-atomic-fence mutant rather than assuming it away.
5. Redo the performance claims with paired baselines, suffix/window factorial sweeps, completed rollback timing, multiwrite/range workloads, contention, fresh-snapshot retry, and preserved reproducibility evidence.

**Current disposition: REJECT.** The theorem is not the reason. The paper repeatedly presents a set of individually tested components as an implemented theorem instance when their composition lacks the theorem's shared-history, policy, identity, and deployment premises. A top systems-security venue will not treat those as prototype polish.

---

References cited by the referee:

- [1] https://docs.rs/tokio-postgres/latest/tokio_postgres/struct.Transaction.html
- [2] https://sonraisecurity.com/blog/sandboxed-to-compromised-new-research-exposes-credential-exfiltration-paths-in-aws-code-interpreters/
- [3] https://simonwillison.net/2025/Jul/6/supabase-mcp-lethal-trifecta/
- [4] https://arxiv.org/abs/2001.06933
- [5] https://hyperledger-fabric.readthedocs.io/pt/latest/endorsement-policies.html
