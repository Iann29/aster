# Authenticate the Reads, Not the Code: Isolating Untrusted Database Executors with Capsule Transactions

**Ian Lucas Beé**

*Draft — July 2026. Markdown working draft for internal review; Figures 1–4 are ASCII/pseudocode placeholders for the LaTeX pass. Companion artifacts: the technical report "The Capsule Transaction Theorem" (27 pp., July 2026) and the Aster repository. Claim-by-claim evidence status is tracked in `paper/CLAIMS.md`.*

---

## Abstract

Platforms increasingly run code they do not trust — AI-generated application logic, third-party plugins, multi-tenant functions — next to a database they do. Our threat model makes that distrust total: the executor is Byzantine from its first instruction, arbitrary probabilistic-polynomial-time code holding no database credential, no network, and no filesystem, whose only channel is a Unix-domain socket to a trusted broker that owns storage, the MAC key, and a single-writer lease. The prevailing defenses isolate the *compute* (microVMs, enclaves) and broker the *outbound credential*, but the code still holds some path to data — and, as recent incidents show, isolation is one SSRF away from credential exfiltration. We take the opposite cut: the executor never touches storage at all. The broker serves each untrusted invocation cryptographically **sealed, per-invocation data capsules**; at commit, it validates the executor's **MAC-authenticated read-set** with classical optimistic backward validation before appending. We prove the *Capsule Transaction Theorem*: committed mutations are strictly serializable over the declared, authenticated read/write sets — snapshot reads may be stale — and a compromised executor is exactly as powerful as a malicious-but-authorized application: it cannot fabricate a read, cross a policy, forge a conflict, or append through a stale lease epoch. A dependency the executor omits demotes its transaction to an authorized blind write, never a cross-authority isolation failure. The construction's freedom from BFT replication, re-execution, and trusted hardware is inherited from the trusted-broker model rather than contributed by it. We implement Aster in roughly fourteen thousand lines of Rust; it runs unmodified `npx convex deploy` bundles end-to-end, and the entire authentication apparatus — V8 trap, IPC, seal verification, snapshot point read, reseal — adds about 0.34 ms per read: a near-zero security tax on the read path.

---

## 1 Introduction

Untrusted code next to a real database is now the normal case, not the exception. AI app builders execute model-generated queries and mutations against production data. Agent platforms wire language models to tools that read and write live records. Multi-tenant platforms co-locate customer functions on shared infrastructure. In every one of these settings the security question an auditor asks is the same: *what stops that code from touching data it shouldn't?*

The prevailing answers isolate the compute and guard the credential. Both halves fail in practice, and they fail for the same reason: **the code still holds ambient authority over everything its credential unlocks.** Harden the sandbox all you want; if an environment variable, IAM role, or connection string is reachable from inside it, the isolation boundary is only as strong as the weakest request the code can emit. In February 2026, researchers at Sonrai and Unit 42 reported driving the AWS AgentCore code interpreter — marketed as "completely isolated" — into exfiltrating IAM credentials through the instance metadata service. In 2025, a Supabase MCP server running with the `service_role` credential was prompt-injected into dumping an `integration_tokens` table — the case that popularized the "lethal trifecta" framing (private data, untrusted input, and an exfiltration path in one agent). By 2026 the industry frontier already brokers the *outbound* credential — Anthropic Managed Agents, Cloudflare Code Mode, and Vercel Sandbox all keep long-lived secrets out of the sandboxed code — so credential-free execution is table stakes, not a contribution. What nobody does is the *inbound* half: authenticate the data the code acted on, and prove at commit time that the code's transaction depended only on real, current rows.

This paper takes that cut. We move the trust boundary off the executor entirely:

- The **executor** (a V8 isolate in its own OS process, one per invocation) is Byzantine from instruction zero. It holds no database credential, no network, no filesystem. Its only channel is a Unix-domain socket to the broker.
- The **broker** is trusted. It owns the Postgres handle, the 32-byte MAC key κ, and the deployment's single-writer lease. It serves reads as **sealed capsules**: keyed-BLAKE3-MACed, canonically encoded bundles binding cell identity, lease epoch, tenant, deployment, snapshot timestamp, the document map, and any range-scan certificates.
- At commit, the executor submits its sealed capsule, a declared dependency set, and a write set. The broker performs classical **OCC backward validation** — but over a read-set whose every entry carries broker-minted provenance. Validation, lease-epoch check, retention check, write policy, and append execute inside one storage-visible **commit fence**.

The key idea in one line: **for isolation you do not need to verify the computation — you need to authenticate the data flow.** Optimistic concurrency control does not need to trust the executor if the read-set is authenticated. FoundationDB's resolver already performs exactly this backward validation over client-declared conflict ranges — while trusting the client entirely; a client that under-declares its reads commits what OCC should have aborted [1]. Aster makes the resolver's input trustworthy: every read-set entry is a serve-time broker MAC, so the executor leaves the trusted computing base.

The result is stated as the **Capsule Transaction Theorem** (Section 4, full proof in the companion technical report): under a fully Byzantine executor, read-sets are unforgeable (T1a), the executor's view is confined to its authorized grant transcript (T1b), committed mutations are strictly serializable over the declared, authenticated read/write sets (T2), and — the honesty boundary — every Byzantine commit is reproducible by an authorized, protocol-following application client with the same grants (T3). A compromised executor is exactly as powerful as a malicious-but-authorized application. It can write garbage *inside its policy envelope*. It cannot fabricate a snapshot observation, read outside policy, evade a conflict on a declared observation, or append through a stale lease epoch.

One sentence survives the proof and organizes the paper:

> Aster makes every submitted OCC observation provenance-authentic without trusting or re-executing the application executor; strict serializability then follows from classical backward validation, while any omitted dependency is demoted to an authorized blind write rather than a cross-authority isolation failure.

### 1.1 Contributions

- **C1 — mechanism.** Serve-time keyed-MAC capsules that make each read self-certifying, turning classical OCC backward validation into an *online admission gate* against a Byzantine executor. The commit fence couples seal verification, lease epoch, retention coverage, conflict validation, write policy, and append at one horizon (Section 3.4).
- **C2 — theory.** The Capsule Transaction Theorem: read-set unforgeability (T1a), confinement with named leakage (T1b), Byzantine strict serializability (T2), Byzantine equivalence (T3), retention safety (Lemma R), and read-plane scale-out (Corollary C1). The proof is conditional on an explicit assumptions ledger (Table 3) and was hardened by an adversarial review round; it is a protocol specification, not a substitute for code verification (Section 4).
- **C3 — principle.** "Authenticate data flow, don't verify computation," with its precise limit stated as a theorem rather than hidden: under-declaration and capsule rollback demote a transaction to an *authorized blind write* (T3). The guarantee is about **authority, not arithmetic integrity**.
- **C4 — system.** Aster: roughly fourteen thousand lines of Rust across eight crates, running unmodified `npx convex deploy` bundles end-to-end against the same Postgres schema the upstream Convex backend writes. Measured on the real pipeline, the full authentication apparatus costs ~0.34 ms per read (Section 6).

### 1.2 What we do not claim

Stating the non-claims up front is part of the contribution's hygiene.

- *"Serializable transactions on untrusted infrastructure without BFT" is not ours.* Fides claimed exactly that in 2020 [5]. Aster's freedom from BFT replication and re-execution is a *consequence* of placing one trusted broker in the online path, not a novelty. Our delta against Fides is the guarantee class: online **prevention** at a trusted commit gate versus post-hoc **detection** by an offline auditor (Section 7).
- *"The code never holds a credential" is not ours either.* Industry credential brokers made that table stakes by 2026. Ours is the data-plane half: sealed inbound reads and commit-time validation of their authenticity.
- *T2 is not execution correctness.* It is strict serializability over the **declared, authenticated** read/write sets. A Byzantine cell may compute nonsense and lie in its application-level return value; only the broker-served grant transcript and the committed history are covered.
- *Reads may be stale.* The headline consistency guarantee is **strict serializability for mutations; snapshot reads are serializable but possibly stale** — mirroring FoundationDB's own snapshot-read caveat [1].

### 1.3 Results preview

On the real pipeline — one-shot containerized V8 cell, UDS to a long-lived broker, Postgres 16 behind it — the marginal cost of one authenticated read trap is **0.34 ms** at the median (0.3–0.5 ms across configurations): a full seal verification of the accumulated capsule, a snapshot point read, a capsule merge, and a complete canonical re-encode and re-MAC, per trap. That is the same order as a warm-connection Postgres point read by itself; the cryptography and process isolation add roughly nothing (Section 6). The cold one-shot invocation floor is ~390 ms, attributable to container spawn and V8 boot, not to the capsule machinery. Write-path throughput numbers do not exist yet; the cell-facing commit verb and its benchmark are the next implementation slices, and we say so plainly rather than projecting (Section 6.3).

---

## 2 Background and threat model

### 2.1 Target data model

Aster targets the storage model of Convex, an open-source reactive backend: an MVCC document store over a totally ordered commit log, with a **single-writer lease**. Reads execute against an immutable snapshot σ_s at timestamp *s*; all commits flow through one writer holding the lease; lease **epochs** rise on failover and stale-epoch writers are fenced out at the storage layer (the upstream backend implements this as a lease check inside the Postgres commit path; `crates/postgres/src/lib.rs:1738–1799` in the `convex-backend` repository). This lease is a design constraint we inherit and exploit: active-passive failover per deployment is possible; active-active is not, and Aster never needs it.

### 2.2 OCC backward validation in one paragraph

A transaction reads at snapshot *s*, accumulating a read-set R, and submits R with a write set W. The validator picks a horizon *h* (the log tip), checks that no committed write in the interval (s, h] intersects R, and if so appends W at a fresh timestamp c > h. The classical soundness argument is an induction over the commit order: if every observation in R still evaluates to its snapshot value at the append point, the serial history that orders transactions by commit timestamp is legal. Everything in this paper preserves that classical core; what changes is that **R's entries are no longer taken on faith**.

### 2.3 Threat model

**Trusted:** the storage system (assumed to implement the MVCC and lease contracts — a Byzantine database server is out of model); the broker/committer role; every process holding the capsule key κ. The TCB is broker + storage.

**Untrusted:** the executor cells — arbitrary PPT Byzantine code from instruction zero. A cell's only channel is the Unix-domain socket to the broker (assumption A12: no database credentials, no network, no filesystem, no alternate storage channel). In deployment this means cells run with egress blocked; without that, exfiltration of *authorized* data is trivial and the confinement pitch collapses even though the theorem survives.

**Collusion:** cells may share out of band anything each was authorized to read; no protocol prevents authorized principals from pooling their authority. What the protocol does prevent is *relabeling*: channel binding stops one cell from replaying another live context's capsule on its own channel (Section 3.2).

**Out of scope, stated up front:** liveness against Byzantine cells (a cell can always self-abort or spam retries), denial of service and resource exhaustion, timing/cache/speculation side channels, exactly-once execution (a replayed request that revalidates is two serial transactions), and compromise of a key-holding broker.

**Goal.** Strict serializability of committed effects plus confinement of the executor to its authorized policy envelope — *online*, at the commit gate, with no trusted hardware and no replication.

---

## 3 Design

### 3.1 Architecture

```
Figure 1 — architecture. The broker owns κ + storage + lease; the cell
gets a UDS and nothing else.

                 HTTP request
                      │
              ┌───────▼────────┐  owns: Postgres handle,
              │   aster-broker │  MAC key κ, single-writer
              │  (long-lived,  │  lease (epoch e)
              │   per deploy)  │
              └───┬───────▲────┘
     spawn, ctx = │       │ sealed capsules /
 (tenant, deploy, │       │ commit verdicts
  cid, e, s)      │       │       ▲
              ┌───▼───────┴────┐  │ UDS only. No DB creds,
              │  V8 cell       │──┘ no network, no fs.
              │ (one-shot, per │
              │  invocation)   │
              └────────────────┘
```

A long-lived broker runs per deployment; it owns κ, the storage handle, and the lease. Each invocation gets a fresh one-shot cell: a V8 isolate in its own OS process, connected to the broker by a Unix-domain socket (`crates/ipc/src/bin/aster_brokerd.rs`, `crates/ipc/src/bin/aster_v8cell.rs`). The broker spawns the cell with a context

  ctx = (tenant, deployment, cid, e, s)

where *cid* is the cell identity, *e* the lease epoch, and *s* the per-invocation snapshot timestamp. After the initial grant, the context is **not** cell-asserted: every capsule verb resolves its expected context exclusively from the broker's own session table (repair C-CHANNEL, Section 3.2), so a request-supplied context is either omitted or must equal the registered one. One prototype caveat belongs here rather than in fine print: at session-mint time the broker still takes *cid* and *e* from the request payload; deriving them from trusted launch metadata is a named remaining obligation (Sections 3.2, 5). Every datum the cell ever sees arrives as (part of) a sealed capsule; the cell never holds a credential of any kind.

### 3.2 Capsules and the seal

A capsule is

  Cap = (tenant, deployment, s, docs, ranges)

where `docs` is a finite map from document keys to versioned results — including **explicit absence**: a versioned "no such document" and a versioned tombstone are first-class observations, because a mutation that depended on absence must conflict when the document later appears — and `ranges` is an ordered sequence of range certificates (Section 3.5).

**Canonical encoding.** The capsule has exactly one wire representation: length-prefixed strings, fixed-width little-endian integers, maps in strict key order, tagged sum types, and a fixed domain string (`aster-capsule-v3\0`) at offset zero (`crates/capsule/src/canon.rs`). Injectivity of this encoding is proved by exhibiting a deterministic decoder (Lemma 3.1 of the technical report). Just as important, the production decoder is a **canonical decoder, not a permissive deserializer** (repair W-CANON): it rejects duplicate keys, out-of-order keys, invalid tags, non-minimal forms, truncated input, oversized declared lengths, and trailing bytes — accepting exactly the byte strings the encoder can produce. A permissive "last duplicate wins" parser would let attacker-controlled bytes carry two meanings across parsing and MAC recomputation; the codec's test suite drives 14 adversarial-decode rejections through exactly those corners (18 codec tests in all — the other four pin positive round-trips and canonical ordering). On the JSON IPC path, the same structural validation is enforced at the seal-verification chokepoint (`SnapshotCapsule::validate_structure`, called from `SealedCapsule::verify`), where every capsule must pass.

**The seal, and an honest version history.** The first production seal (algorithm string `aster-blake3-keyed-v1`) MACed a BLAKE3 *digest* of the canonical bytes — a prehash. Counterexample 2.2 of the technical report shows why that construction cannot be reduced to keyed-MAC security alone: literal injectivity of a 256-bit digest over unbounded capsules is impossible by counting, and there is a proof-theoretic countermodel in which the keyed mode is a perfect PRF while the unkeyed hash maps every capsule to one constant — a tag for one capsule would then verify for every capsule sharing its context. Not a practical attack on BLAKE3; a demonstration that the v1 proof needs an *extra* assumption, collision resistance of unkeyed BLAKE3 (ledger entry A3). The v0.7 cycle therefore moved to direct MACing in two steps: `aster-blake3-keyed-v2` MACed the full framed canonical bytes, retiring A3 (Remark 3.4 of the report); the shipped seal, `aster-blake3-keyed-v3`, keeps the direct-MAC construction and additionally binds the broker session into the MAC input (the channel-binding repair below), superseding v2 within the same cycle. The v3 MAC input is

  alg ∥ lp(cid) ∥ le64(e) ∥ SB ∥ lp(E(Cap)),  SB = 0x00 (unbound) | 0x01 ∥ session[32] (bound)

— the **full framed canonical encoding**, with tenant, deployment, and snapshot bound through E(Cap), which frames them immediately after the domain string, and SB a domain-separated session frame whose tag byte alone determines the frame length, so a bound message can never collide with an unbound one (`crates/capsule/src/seal.rs::seal_mac`). BLAKE3 is a streaming hash, so MACing the full encoding costs no second materialization. Forging any accepted capsule reduces to a keyed-MAC forgery alone; assumption A3 stays retired. The canonical digest is still computed and carried in the seal, but only as an audit and tooling convenience — never a MAC input. Verification enforces the exact algorithm identifier — only v3 verifies; v1 and v2 seals are rejected (`seal.rs::sealed_capsule_rejects_legacy_v1_algorithm`, `sealed_capsule_rejects_legacy_v2_algorithm`) — exact 32-byte tag length, canonical structure, header/context equality, and constant-time tag comparison (`crates/capsule/src/seal.rs::ct_eq`). Two pinned test vectors guard the wire format, one per session state: any drift in the seal construction is a deliberate, versioned decision, never an accident (`seal.rs::seal_test_vector_is_stable`, `bound_seal_test_vector_is_stable`).

**Channel binding (seal v3).** Binding the MAC to *cid* means "wrong-cell rejection" only if the verifier knows which cell a request came from by a channel it controls. v0.7 implements C-CHANNEL as a broker-minted session: at `InitialCapsule` time the wire broker draws an unguessable 32-byte session id from OS entropy, registers it in its own session table, and returns it with the grant (`InitialCapsuleGrant`, `crates/ipc/src/bin/aster_brokerd.rs`). Every subsequent capsule verb must present that id, and the broker rebuilds the expected bound context **exclusively from its own table entry** — the request's serialized context is checked for equality against the record and then discarded, never used as authority. The enforcement point is the seal itself: the session enters the v3 MAC input under domain separation, so a capsule issued to one session and presented on another — even with identical *cid* and epoch, the re-spawned-cell case — fails seal verification (`capsule_from_another_session_fails_seal_verification`), and stripping the session field from a bound seal dies on the MAC's tag byte. A capsule plus its public context copied to another cell's channel therefore fails verification — while a *cooperating* holder of the other context can of course exercise its own authority, which is collusion the model explicitly permits. Two prototype caveats, stated rather than blurred: the session is per-invocation, not per-socket — the prototype wire protocol opens one connection per request, so the unguessable id *is* the channel; and at mint time the broker takes *cid* and lease epoch from the request payload, which stand in for trusted launch metadata (`SO_PEERCRED`, a broker-assigned launch token) — deriving them from a channel the cell cannot influence is a named remaining obligation (Section 5), which is why A11 stays an assumption in Table 3 rather than a discharged fact.

### 3.3 The read protocol: grow-and-reseal

```
Figure 2 — one read trap.

cell                         broker                        Postgres
 │  JS executes; db.get(id)     │                              │
 │  suspends on a trap          │                              │
 │──(ctx, sealed Cap_i, key)──▶ │                              │
 │                              │ verify seal (V1, channel ctx)│
 │                              │ check P.read(ctx, key)       │
 │                              │──── point read at σ_s ──────▶│
 │                              │◀──── (version, doc/absence) ─│
 │                              │ merge into Cap_{i+1}         │
 │                              │ reseal: canonical encode +   │
 │                              │ keyed MAC for ctx            │
 │ ◀──(sealed Cap_{i+1})────────│                              │
 │  promise resolves; JS resumes│                              │
```

Reads are pull-based **traps**. The cell starts with a sealed initial capsule (possibly prewarmed — every prewarmed item passes the same read-policy check a trap would, and enters the grant transcript; prewarm is an ideal read grant, not a policy bypass). When the JavaScript touches a key that is not in the capsule, the runtime suspends, sends the sealed capsule and the request over the UDS, and the broker: verifies the seal against the channel-bound context, checks read policy *before* any storage access, evaluates the point read or scan at the immutable snapshot σ_s, merges the exact result, and reseals the grown capsule. Denied requests return a denial code and nothing else.

Two properties matter. First, the broker is **stateless per invocation**: it remembers no "latest capsule" digest, no read list, no sequence number — only immutable launch metadata and global state (policy, lease, log, key). This is what lets read brokers scale out (Corollary C1) and it is also what makes the honesty boundary of Section 4 exactly what it is: because the broker accepts any *issued* capsule, a Byzantine cell can replay an earlier one and fork its own issuance tree. Second, **absence is authenticated**: a read of a missing or tombstoned key enters the capsule as a versioned atom, so "I saw nothing there" is as unforgeable — and as conflict-checked — as "I saw value v."

### 3.4 The commit protocol and the commit fence

At commit the cell submits (ctx, sealed Cap, S, W): its channel-bound context, a sealed capsule, a **declared dependency set** S (Variant B, Section 3.6), and a canonical write set W. The broker reduces this to the theorem's validated ingredients and executes the **commit fence**:

```
Figure 3 — CommitFence pseudocode (technical report §1.8).

CommitFence(ctx, Cap, R, W):
  enter a storage-visible commit/validation serialization fence
  require the committer's epoch e_now was acquired and fenced at storage
  read log tip h only after that epoch-acquisition fence
  read monotonic wall/log time Now and retained low-watermark g
  require ctx.e == e_now                      // V2  (stale-epoch fencing)
  require ctx.s <= h                          //     (snapshot is a real prefix)
  require ctx.s >= Now - Delta                // V3  (product max-age rule)
  require g <= ctx.s                          //     (actual coverage rule)
  pin the retained interval so g cannot advance past ctx.s
  evaluate P.write(ctx, W) against policy version p      // V5
  require no committed write in (ctx.s, h] intersects
      any point/window in R                   // V4  (backward validation)
  atomically append W at fresh c > h iff:
      the storage lease still accepts ctx.e, and
      the policy decision/version p is still current
  release the fence and retention pin
```

The load-bearing property is that **validation and append share one stable horizon** (repair A-ATOMIC). The classical write-skew pair shows why a check-then-append implementation is simply wrong, not merely racy: T1 reads y=0 and writes x=1; T2 reads x=0 and writes y=1; both validate against the same tip h=s, both see no conflict, both append — and no serial order realizes both reads (Counterexample 3.9 of the report). Serializing each validation with its append forces whichever transaction reaches the fence second to see the first writer in (s, h] and abort.

Our implementation maps the fence onto **one Postgres transaction** (`crates/store-postgres/src/write_plane.rs::commit`): the first statement locks the deployment's lease row `FOR UPDATE` — every commit takes that lock, so validation and append serialize; V2 requires *both* the committer's acquired epoch and the capsule context's epoch to equal the live lease epoch (a current committer still rejects a stale-epoch capsule); the horizon h is read only after the epoch fence; the retention row is locked `FOR UPDATE` from the coverage check g ≤ s through append, so the GC sweeper — which takes the same lock — cannot remove an event in (s, h] mid-validation (Lemma R's pin); the conflict scan checks declared point keys and range windows against committed writes in (s, h]; and the append at c = h+1 carries the epoch inside the same transaction. A concurrent lease acquisition blocks on the same row lock until an in-flight fence commits, which is precisely the failover ordering Lemma 3.11 requires: an old-epoch append linearizes before the new epoch's first fence, and every later old-epoch fence fails the epoch equality check. Lease epochs are **strictly increasing and never reused** — acquisition always bumps the epoch under the row lock, even across an A→B→A failback; epoch reuse would break the exhaustive case analysis of the epoch-fencing lemma. This is the same lease-in-the-commit-path pattern the upstream Convex backend uses for its single writer; the fence adds validation, retention, and policy to the decision that storage already serializes. Coverage (g ≤ s) is the exact admission condition implemented in the fence; the wall-clock max-age rule (s ≥ Now − Δ) is a product admission policy layered above it.

### 3.5 Phantom-safe limited scans: range certificates

Point reads are not enough — `db.query(...).take(5)` must be phantom-safe. A limited scan's result is protected by a **sealed range certificate**:

  ρ = (I, direction, ℓ, ⟨k1 … km⟩, stop),  stop ∈ {Exhausted (m < ℓ), Boundary (m = ℓ)}

binding the normalized interval with endpoint inclusivity, the scan direction, the positive limit, the ordered returned keys, and — critically — whether the scan **stopped at the limit** or **exhausted the interval** (repair R-RANGE). Every returned key must cross-reference an exact entry in the capsule's document map. The certificate determines the **observed window** that commit-time validation defends:

```
Figure 4 — the two conflict windows of a limited ascending scan.

Exhausted (m < ℓ):        [============ whole interval I ============]
                           k1   k2   k3        (gap certified empty)
                           a later insert ANYWHERE in I conflicts.

Boundary (m = ℓ):         [==== prefix through km ====]· · · · · · ·]
                           k1   k2   ...  km            (unobserved)
                           a later insert ≤ km conflicts;
                           an insert strictly after km does not —
                           it cannot change the first-ℓ answer.
```

A full result protects the prefix through the last returned key; a short result certifies an **exhaustion gap** — no additional live key existed at *s* — and protects the whole interval. This is next-key locking's insight transplanted into OCC validation windows. Stability is proved as Lemma 3.7 of the report: if no committed write in (s, h] touches Win(ρ), the first-ℓ answer at h equals the answer at s, including versions and documents.

One API consequence deserves the ink (finding F2 of the adversarial review): when an application asks for `limit ℓ` and receives exactly ℓ results, it is protected as a *first-ℓ* observation — **not** as a completeness assertion. Correctly so: an insert after k_ℓ does not change the first-ℓ answer. An application that wants "this is the complete set" must obtain an Exhausted certificate — ask for ℓ+1, or check the stop bit. We document this in the API and repeat it here because it is exactly the kind of semantic footgun that survives type checking.

The certificate types, their window computation, and the window-containment predicate used by the fence live in `crates/capsule/src/lib.rs` (`RangeCertificate`, `ScanStop`, `ObservedWindow`); the canonical encoding includes the ordered certificate sequence (capsule domain `aster-capsule-v3`); v0.7 serves certified prefix scans over the wire protocol, so a scan's certificate is sealed into the capsule the cell holds, not reconstructed at commit.

### 3.6 Variant B: declared dependency sets

At commit, which observations does validation defend? Variant A validates the *entire* submitted capsule: R = Obs(Cap). Variant B validates an exact **declared subset**: R = S ⊆ Obs(Cap), where each declaration references a sealed atom precisely — a point declaration must match the sealed key and exact versioned result; a range declaration must identify an exact sealed certificate entry, never a narrowed or rewritten interval; duplicates and atoms not structurally present in the capsule are rejected (repair B-SUBSET).

v0.7 adopts **Variant B**, and the reason is worth being precise about, because A's apparent advantage is illusory. Variant A looks stronger — "the committer does not accept omission" — but it proves completeness only relative to *whichever issued capsule the adversary chooses to submit*. Under a stateless broker, a Byzantine cell can compute on a value from a later capsule and submit the still-valid seal on an earlier one (Counterexample 2.1); no stateless verifier can distinguish that from an application that stopped early and issued a blind write. So A buys no Byzantine completeness — while costing honest users real aborts: every unused prewarmed atom in the selected capsule participates in conflict checking, so prewarming converts prediction error into abort amplification. Variant B has the same T2/T3 theorem class, avoids false conflicts on unused prewarm, and names the object honestly: S is the *declared dependency set*, not "the executor's complete read set."

For honest code, the runtime does the declaring. The v0.7 V8 runtime tracks **consumption**, not just traps: every observation the JavaScript actually touches — including warm hits served from the capsule without a trap — lands in a deduped consumption ledger surfaced as `V8ExecutionResult::consumed_reads`, and prewarmed entries stay out of it until consumed. That ledger is S; submitting it over the wire rides the cell-facing commit verb, which is not yet wired (Section 5). Honest applications therefore get ordinary OCC behavior with no bookkeeping, and the declared set equals actual dependencies whenever the runtime is intact. When it is not — a compromised runtime can omit — T3 bounds the damage: the omission demotes the transaction to an authorized blind write.

---

## 4 Formal guarantees

This section condenses the companion technical report — "The Capsule Transaction Theorem" (27 pp.), whose proof survived an independent adversarial review round with no fatal findings — into the four theorems, the retention lemma, and the scale-out corollary, with an emphasis on what each statement does **not** claim (Table 2). The proof is **conditional** on the assumptions ledger reproduced as Table 3; it is a specification for v0.7, not a claim that any particular code revision is verified.

**T1a — read-set unforgeability.** Assume the keyed MAC is EUF-CMA-secure, the canonical encoding and outer framing are injective (proved as Lemmas 3.1/3.2, not assumed), verification performs the exact canonical and context checks, and all key holders are trusted. Then for every PPT coalition of cells, the probability that the verifier accepts a capsule **not issued for that exact channel-bound context** is bounded by the MAC-forgery advantage plus a negligible term. (For the retired v1 prehash seal, the bound carried an additional unkeyed-BLAKE3 collision-resistance term — the direct-MAC construction, v2 and the shipped session-bound v3, removes it.) Consequences: no cross-context transplant (different cid, epoch, tenant, deployment, or snapshot changes the MAC input), no splicing entries from two capsules into an unissued union, no payload substitution. **Honest scope:** T1a authenticates *an issued* capsule — not "the latest." Replay of an earlier issued capsule under the same context is deliberately outside the word "forgery"; preventing it would require stateful anti-rollback or trusted use-observation, both of which the stateless design refuses. This scope is not a proof convenience; it is load-bearing for the honesty boundary in T3.

**T1b — confinement, with named leakage.** The coalition's read-plane view is computationally indistinguishable from a simulation given only the public contexts and the ideal grant transcript: it learns no document payload beyond the adaptive transitive closure of its policy-authorized reads. The *whole-protocol* view (once commits are visible) additionally leaks explicit control predicates — epoch/retention/policy outcomes and, in particular, the **conflict bit**: whether a declared authorized observation window changed after the snapshot. Counterexample 2.3 shows the bit is real (two executions identical up to an unrelated post-snapshot write differ in commit outcome), so the ideal functionality names it rather than pretending it away. No unauthorized payload is revealed either way. Timing, cache, and scheduler channels are excluded by assumption, not hidden by the theorem.

**T2 — Byzantine strict serializability.** Except with the negligible T1a failure probability, every execution — arbitrarily many Byzantine cells, arbitrary interleavings, retries, and lease failovers — yields a committed-mutation history that is strictly serializable over the **declared, authenticated** read/write summaries (R_T, W_T, s_T, c_T), in commit-timestamp order, with the atomic append as the linearization point; real time is respected between non-overlapping successful operations. The proof is the classical backward-validation induction, with the point/range stability lemmas as the semantic bridge and the fence lemmas (single horizon; epoch block order) closing concurrency and failover. Read-only invocations get a serializable snapshot that may be **stale**; their application-level return value is not authenticated at all — only the broker-served grant transcript is. *Versus FoundationDB:* the resolver performs the same backward validation over client-declared ranges but trusts the client; Aster's proven delta is **provenance authenticity of each submitted observation** — the executor cannot fabricate a value, version, or sealed window. The stronger sentence "the server now knows the complete actual read set" is false for the stateless protocol, and the report says so in exactly those words.

**T3 — Byzantine equivalence: the honesty boundary.** For every successful Byzantine-produced commit there exists an *authorized, protocol-following* client — honest about wire syntax, broker issuance, and policy, not about application semantics — with the same context, the same grant transcript, and the same commit-time write authorization, that submits the identical accepted summary. The witness may hold W as a literal constant and treat any grant as unused; rollback and Variant B omission are reproduced by the witness *by design*. If write policy confines each tenant to disjoint key authority, then compromising the executor yields **no database effect beyond a malicious-but-authorized application**: garbage writes, blind writes, retries, self-conflicts — all inside the policy envelope; no unauthorized reads, no out-of-policy writes, no forged observations, no serialization break. This is the security pitch, stated as a theorem. Its honest limit: omission can destroy the malicious tenant's *own* semantic invariants. It creates no cross-tenant or cross-policy power.

**Lemma R — retention safety.** Backward validation is sound iff the consulted log covers (s, h]: sufficiency is the stability lemmas; necessity is a concrete truncation counterexample (read k at s; another commit changes k; GC removes the event; the stale read validates). The exact admission condition is coverage — retained low-watermark g ≤ s, held under a retention pin so GC cannot advance past s mid-validation — with the age rule s ≥ Now − Δ as the product's uniform admission bound. Liveness corollary: an invocation whose snapshot ages past Δ has no commit guarantee and must retry from a fresh snapshot.

**Corollary C1 — read-plane scale-out.** Any trusted κ-holder that can read the fixed snapshot and enforce read policy can verify, grow, and reseal capsules; only the single committer runs the fence and appends. Read brokers therefore scale horizontally without changing T2 — the committer re-verifies every capsule against the authoritative history; a read broker with stale epoch information at worst produces a capsule whose commit later aborts. One deployment caveat (review finding F3): read policy is evaluated per-broker at read time, so **revocation propagates with bounded skew** across scaled read brokers; operators must bound and monitor that latency.

**Table 2 — theorem map: what each result does and does not claim.**

| Result | Guarantees | Explicitly does NOT claim |
|---|---|---|
| T1a | An accepted capsule was issued by a trusted broker for exactly this channel-bound context; no transplant, splice, or substitution | That the capsule is the *latest* issued; that it contains everything the cell actually used (rollback/fork permitted) |
| T1b | Read-plane view simulatable from the authorized grant transcript; whole protocol adds only named control bits (conflict bit, policy/epoch/retention outcomes) | Hiding of policy allow/deny outcomes; anything about timing/cache side channels |
| T2 | Committed mutations strictly serializable over declared, authenticated read/write sets, in commit order; append is the linearization point | Execution correctness; completeness of R w.r.t. actual reads; freshness of read-only snapshots |
| T3 | Every Byzantine commit reproducible by an authorized protocol-following client with the same grants — compromise ≡ malicious authorized app | Application-semantic honesty; protection of a tenant from its own authorized code |
| Lemma R | Validation sound iff retained log covers (s, h]; g ≤ s + pin is exact; Δ-age rule is the uniform product bound | Commit liveness for snapshots older than Δ |
| C1 | Read brokers scale without a new serialization point; committer re-verifies everything | Instantaneous policy-revocation propagation across read brokers (bounded skew) |

The report additionally discharges **thirteen enumerated attack obligations** — capsule replay across snapshots, cross-cell transplant, cross-epoch replay racing failover, splicing/rollback/fork, phantoms including the exhaustion gap, absence flips, encoding ambiguity, Variant B under-declaration, over-declaration/prewarm poisoning, GC races, duplicate/contradictory entries, policy TOCTOU, and whole-capsule replay — absorbing rollback and under-declaration into the theorem boundary rather than pretending to exclude them.

**Table 3 — assumptions ledger (condensed from the report; P-entries are proved, not assumed).**

| ID | Assumption / proved fact | Role |
|---|---|---|
| A1 | Trusted broker & key holders | Every κ-holder enforces policy, reads the requested snapshot, seals only truthful transitions; compromise defeats T1a/T1b |
| A2 | Keyed BLAKE3 is EUF-CMA / PRF | The MAC in T1a/T1b |
| A3 | Unkeyed BLAKE3 collision resistance | Required only by the retired v1 prehash seal; **retired in v0.7** by the direct-MAC seals (v2, superseded in the same cycle by the shipped session-bound v3) |
| A4 | Key secrecy & domain discipline | κ from a secret store, not public environment material; cross-protocol uses get disjoint tags or keys |
| P1 | Injective canonical encoding (**proved**, Lemma 3.1) | Requires the production decoder to reject noncanonical/duplicate forms |
| P2 | Injective outer framing (**proved**, Lemma 3.2) | Exact algorithm prefix; framed/fixed-width fields |
| A5 | Correct MVCC storage | Snapshot reads immutable; tombstones/absence as modeled; appends atomic & durable |
| A6 | Complete conflict projection | Every mutation that can change a point or key-interval scan surfaces as a write event on the corresponding key — **the weakest joint** (Section 8) |
| A7 | Single-writer lease; strictly increasing, never-reused epochs | Storage rejects stale epochs atomically; assigns globally increasing timestamps |
| A8 | Atomic commit fence | V2–V5 + append at one stable horizon; no interposing commit |
| A9 | Retention floor & pin | Committer knows g; GC cannot remove (s, h] during validation |
| A10 | Policy correctness & versioning | P.read/P.write express intended authority; write decisions linearized with append; tenant confinement needs disjoint key authority |
| A11 | Context/channel binding | cid, tenant, deployment, epoch, snapshot from trusted launch/channel metadata, never cell assertions — an **assumption** the prototype only partly discharges: post-mint verbs resolve context from the broker's session table, but mint-time cid/epoch still arrive in the request payload (Sections 3.2, 5) |
| A12 | Process isolation | Cells: no DB credentials, no network, no filesystem, no alternate channel; side/covert channels excluded |
| A13 | PPT adversary | All cryptographic conclusions computational |

---

## 5 Implementation

Aster is roughly fourteen thousand lines of Rust (14.4k measured by `wc -l` over `crates/`, tests included) across eight crates: `capsule` (canonical codec + seal), `broker` (cell-facing capability trait + store abstraction), `store-postgres` (Convex-schema read adapter + the Aster write plane), `v8cell` (V8 isolate, ESM module loader, Convex shims), `ipc` (UDS framing, the `aster-brokerd` daemon and `aster-v8cell` one-shot binaries), `convex-codec` (IDv6 + ConvexValue ports), `runner`, and `host` (test harnesses and benchmarks). The workspace carries 217 tests; the seal, codec, and range-window properties of Section 3 are each pinned by unit tests, and the fence's concurrency claims by the integration suite below.

**Runs unmodified Convex bundles.** The cell compiles a real `npx convex deploy` bundle as an ES module and drives the upstream wire shape — `Convex.asyncSyscall("1.0/get", argsJson)` — through the trap loop (`crates/v8cell/src/lib.rs`): one trap per read the user's query actually makes. The Postgres adapter reads the *same* schema the upstream Convex backend writes (`documents`, `_tables`, `_modules`, `_source_packages`), including the `_modules` × `_source_packages` join and ZIP unpack that resolves module source (`crates/store-postgres/src/{lib,module_index,modules_storage,table_mapping}.rs`). No application rewrite: the same bundle that deploys to a Convex backend runs in an Aster cell.

**The v0.7 write path.** What this paper's protocol required over the read-only prototype, and what shipped:

- the direct-MAC, session-bound seal (`aster-blake3-keyed-v3` — the session-less direct-MAC v2 was superseded within the cycle and is rejected alongside v1) with constant-time comparison and two pinned wire vectors (`crates/capsule/src/seal.rs`);
- the canonical wire codec with adversarial decode (`crates/capsule/src/canon.rs`, 14 rejection tests among its 18);
- sealed range certificates with exhausted/boundary windows, served over the wire (`crates/capsule/src/lib.rs`);
- session-bound capsule verbs (C-CHANNEL): a broker-minted unguessable session id per invocation, resolved against the broker's own session table and enforced in the seal MAC (Section 3.2) — mint-time identity from trusted launch metadata, and read-path lease epochs from the lease authority rather than the launch environment, remain named obligations below;
- Variant B consumption tracking in the V8 runtime (Section 3.6) — which also closed a real bug the first benchmark exposed: the production syscall path never short-circuited a warm capsule, so every read trapped even when the document was already sealed into the capsule;
- the **write plane**: lease authority and commit fence as one Postgres transaction over Aster-owned tables (`crates/store-postgres/src/write_plane.rs`), plus GC (`advance_retention`) serialized against in-flight fences by the retention row lock.

The fence and its concurrency claims are exercised by fourteen integration proofs against real Postgres (`crates/store-postgres/tests/write_plane_it.rs`): lease epochs strictly increase and never reuse across failback; the CE 3.9 write-skew pair is impossible sequentially **and under real concurrency** (two racing fences, exactly one commits); a stale epoch cannot append after failover; the Lemma R retention pin holds in **both directions** — the GC sweeper blocks on an in-flight fence and, conversely, a fence blocks while the sweeper side holds the retention lock, releasing into a successful commit; a wedged idle lock-holder is killed by the configured idle-in-transaction timeout so failover resumes instead of hanging forever; a replayed request commits as a second serial transaction; a phantom insert conflicts with an exhausted window but not past a boundary window (the F2 negative case); absence and tombstone reads conflict on later writes, and a tombstone **write** landing inside the window conflicts with a point read of that key; MVCC point/prefix reads follow snapshot semantics; and the retention watermark clamps to the log tip (`retention_watermark_clamps_to_log_tip` — the regression for the liveness find below). Beyond testing, the fence's interleavings — validation/append atomicity, the GC pin, and epoch failover — are model-checked in TLA+ (`tla/AsterFence.tla`, runs recorded in `tla/RESULTS.md`), because the write-skew and epoch races are exactly the class of bug that survives example-based tests. That investment has already paid for itself: building the model exposed a real commit-admission **liveness** bug before any deployment — `advance_retention` accepted a watermark above the log tip, and since the retention floor never lowers, every subsequent snapshot would fail coverage forever, permanently wedging admission. The fix clamps the applied watermark to the tip under the same retention row lock, with the regression test above, a matching model guard (`w <= Tip`), and all four TLC configs re-run — the positive model clean, both negative models still producing their designed violations.

**The committer-integration decision (F9).** A write plane cannot be a sidecar: the single-writer lease (A7) forbids two writers, so for a live Convex deployment the Aster broker must either (a) *become* that deployment's committer — taking over the lease and the write path — or (b) validate and *forward* authenticated writes through the backend's own committer, acting as a gate in front of it. **v0.7 builds neither takeover.** The shipped write plane owns its own commit log, lease, and retention tables (schema `aster`) and proves the fence against them; it never writes Convex's tables. The read plane needs no such decision (Corollary C1) and runs against the backend's schema today. We consider the integration choice a product decision with deep operational consequences on the backend's write path, and we scope it out of v0.7 explicitly rather than blur it.

**Not yet wired.** The cell-facing *commit verb* on the UDS protocol is the next slice: today a V8 cell reads end-to-end, but mutations reach the fence through the committer API (as the integration suite does), not from inside a cell. This is also why no write-path benchmark exists yet (Section 6.3).

**Engineering findings from the first real benchmark.** (1) The broker's connection budget (`ASTER_MAX_CONNECTIONS`) was enforced as a *lifetime* counter — the daemon counted total connections since boot and exited when crossed; with one connection per trap, a default broker self-terminated after 1024 traps. Found because the K=200 benchmark crossed it on invocation four; fixed in v0.7 — the budget is simply gone. The prototype broker serves one connection per request, serially; there is no concurrency cap, and concurrency control belongs at the accept/queue layer if the broker ever goes parallel. (2) The warm-capsule short-circuit existed only on a legacy test path, never on the production syscall path — 200 reads of the same document cost 200 traps. Fixed by the consumption-tracking work above. Both are reported here because they are exactly the kind of defect only a real end-to-end pipeline surfaces.

**Remaining implementation obligations** (recorded, not proved): production κ from a secret store or KMS — the test-fixture derivation helper is not the A4 key model; explicit size/count caps on capsule collections, strings, range results, and write sets (DoS control, not cryptography); egress-blocked cell containers (A12 is an assumption the deployment must make true); mint-time session contexts derived from trusted launch metadata (`SO_PEERCRED` or a broker-assigned launch token) instead of the request payload — the A11 half the session mechanism does not yet discharge (Section 3.2); read-path lease epochs wired from the storage lease authority instead of the launch environment — today the cell binary reads `ASTER_LEASE_EPOCH` from its environment, while the **write plane** already takes its epochs from `acquire_lease` and the fence rejects mismatched capsule epochs, so the lease-authority claim is scoped there; and coordination of the retention low-watermark with GC across committer failover.

---

## 6 Evaluation

Numbers in this section are measured; where a number does not exist yet we say so and give the planned methodology instead. Nothing here is projected.

**Setup.** Host: a developer workstation (Arch Linux, Docker). Images built from the repository (`docker/Dockerfile`, targets `runtime-broker` / `runtime-v8cell`). Store: `postgres:16` seeded with the repository's Convex-schema fixtures, snapshot pinned (`ASTER_SNAPSHOT_TS=200`). Method: three JavaScript workloads — T0 (no syscall), T1 (one `db.get`), TK (K sequential gets of the same document id) — each run N=12 times as a fresh one-shot cell container against a long-lived broker, K=200. Container-spawn overhead cancels in the subtractions; trap counts are sanity-asserted from the cell's own `"traps":N` output.

### 6.1 EQ1 — the read-path security tax

**Table 4 — read-path headline numbers (p50 unless noted, N=12, K=200).**

| Measurement | Value |
|---|---|
| Cold one-shot invocation, no reads (T0) | 390 ms (min 351, p95 398) |
| Cold one-shot invocation, 1 read (T1) | 390 ms (min 362, p95 400) |
| Cold one-shot invocation, 200 reads (TK) | 458 ms (min 422, p95 490) |
| **Marginal cost per trap — (TK − T1)/199** | **0.34 ms** |
| First-trap cost (T1 − T0) | below measurement noise (< ±10 ms) |
| Implied serial broker throughput | ~2,900 traps/s (single-threaded, one connection per trap) |

A K=16 configuration measured 0.47 ms/trap; K=200 tightens the estimate to 0.34 ms (signal 68 ms ≫ noise ±10 ms). We report the band as **0.3–0.5 ms per authenticated read**.

What that number *contains* is the point: a fresh UDS connect, a u32-framed JSON request, a broker-side **full seal verification** of the accumulated capsule, a Postgres point read at the pinned snapshot (`ts <= $ts ORDER BY ts DESC LIMIT 1`), the capsule merge, a **complete canonical re-encode and re-MAC** (reseal), the JSON response, and V8 promise resolution. At 0.34 ms, the entire capability apparatus — cryptography, IPC, broker hop — costs approximately what a warm-connection Postgres point read costs by itself. **The security tax on the read path is near zero.**

### 6.2 EQ2 — reseal scaling [PENDING: growing-capsule bench]

An honesty caveat on EQ1: in the TK workload the capsule stayed tiny (200 reads of the *same* id occupy one entry), so the reseal cost per trap was constant. The reseal re-encodes the **whole** capsule — O(capsule size) per trap, hence O(n²) total over a read-set that grows by one entry per trap. The growing-capsule curve has not been measured yet; the planned method is the TK harness with K *distinct* keys, sweeping K, reporting per-trap cost against capsule size. If the quadratic bites at realistic read-set sizes, the obvious remedy is incremental or Merkle-ized sealing, which we deliberately did not build ahead of the measurement.

### 6.3 EQ3 — commit throughput and abort behavior [PENDING: S10 write-path bench]

No write-path performance numbers exist, because the cell-facing commit verb is not wired yet (Section 5). The planned methodology, so the reader can hold us to it: commits/s through the single committer as a function of read-set size (points and range windows sweep separately, since the fence's window scan is a distinct-key scan over (s, h]); fence latency decomposition (lease lock wait, horizon read, conflict scan, append); abort rate under contention with Variant B declared sets, prewarm on/off; and the failover blip — commit availability across a lease takeover. All against the same `postgres:16` setup as EQ1.

### 6.4 EQ4 — end-to-end invocation cost

The cold one-shot floor is ~390 ms (Table 4), and the T0/T1 identity shows it is **entirely container spawn plus V8 boot** — the capsule machinery contributes below noise at one read. This floor is a warm-pool roadmap item (cell reincarnation), not a protocol cost; warm serving latency will be reported when a warm pool exists, not projected here. For one-shot workloads the floor is already acceptable: a 1,000-read report query costs roughly 0.4 s of trap time on top of the 0.4 s spawn — with zero resident-memory pressure on the reactive backend it offloads.

### 6.5 EQ5 — comparison baselines [PENDING: S10 write-path bench]

Two baselines are planned. (a) The same query as a trusted-executor Convex function in the upstream backend, quantifying what the isolation actually costs end-to-end. (b) A TEE-based deployment as a concept baseline: the argument of Section 7 is that a trusted broker makes the enclave unnecessary for *executor* isolation; the measurable form of that argument is one broker process versus an attestation pipeline. Neither number exists yet.

---

## 7 Related work

**Table 1 — positioning: trust locus × mechanism × guarantee × cost.**

| System | Untrusted party | Mechanism | Guarantee | Cost |
|---|---|---|---|---|
| FoundationDB [1] | nobody (client in TCB) | declared conflict ranges + OCC resolver | serializability *if clients honest* | — |
| Basil [3] | clients + replicas | BFT quorums, 5f+1, commit certificates | Byzantine-tolerant serializability | replication ×5+ |
| Hyperledger Fabric [2] | peers/clients | endorser re-execution + signed rwsets, version validation | endorsement-policy integrity | re-execution ×endorsers |
| Fides/TFCommit [5] | storage + coordinator + clients (no online trusted party) | CoSi-signed hash-chained log + Merkle state; offline auditor | **detection** (auditability), post-hoc | audit infrastructure |
| TransEdge [4] | edge storage replicas | BFT-SMaRt per partition + Merkle proofs + f+1 quorum signatures to trusted clients | serializable reads vs untrusted storage | replication 3f+1 |
| Ryoan / Opaque / EnclaveDB | executor/operator hosts | SGX enclaves + attestation (Opaque: dataflow self-verification) | confidentiality/integrity via hardware | TEE + attestation pipeline |
| Cobra (OSDI '20) | the database | offline SMT serializability checking by trusted clients | detection, post-hoc | audit compute |
| **Aster** | **the executor (arbitrary app code)** | **serve-time keyed-MAC capsules + OCC backward validation at the lease-holding broker** | **online prevention: strict serializability over declared, authenticated sets** | **one broker process** |

**FoundationDB** is the mechanism anchor. Its resolver performs exactly the backward validation of T2 — clients obtain a read version, read, and submit read/write conflict ranges; resolvers check submitted read ranges against recently modified ranges; snapshot reads are omitted from conflict sets [1]. The client, however, is in the TCB: an executor that under-declares commits what OCC should have aborted. Aster's exact delta is provenance-authenticity of each submitted observation — the executor cannot fabricate the resolver's evidence — while semantic completeness remains governed by the variant and rollback boundary (Section 4, T2 remark).

**Fides / TFCommit** is closest in spirit, and owns the no-BFT precedent. TFCommit commits serializable transactions across multiple *untrusted* storage servers without Byzantine replication, using a hash-chained, collectively signed (CoSi) tamper-evident log over Merkle-authenticated datastores [5]. Fides and Aster share the goal of transactional integrity on untrusted infrastructure while avoiding BFT state-machine replication, but they invert the trust model and, with it, the guarantee. Fides trusts no online party — storage, coordinator, and clients may all be Byzantine — and therefore offers *auditability*: incorrect executions are not prevented but are made irrefutable and detected after the fact by an external, offline auditor reconstructing a serialization graph from the signed log. Aster places a single trusted, lease-holding broker in the online data path and treats only the executors as Byzantine, allowing it to *prevent* rather than detect: every served read is sealed at serve time with a keyed MAC binding it to (cell, lease epoch, snapshot), and the broker validates the authenticated read-set through backward validation before any commit is admitted. The concentration of trust also changes the cryptography — Fides needs publicly verifiable collective signatures precisely because it has no trusted verifier; Aster's symmetric MAC is sound only because the broker both seals and checks. Fides authenticates the *log* for post-hoc dispute resolution; Aster authenticates *per-read read-sets* to make OCC sound against Byzantine executors online — a mechanism, and a strict-serializability safety guarantee, that a detection-based design does not provide.

**Basil** shows Byzantine *clients* plus serializability is achievable with BFT machinery: 5f+1 replicas per shard and commit certificates yield Byzantine-tolerant serializability [3]. Aster targets the same robustness against the application principal at a different cost point — one trusted broker process — because its threat model concedes a trusted storage authority. The goals overlap; the cost models do not, and Aster is *not* a one-node replacement for Byzantine storage replication.

**Hyperledger Fabric** validates signed read/write sets at commit time without re-executing there — but those rwsets originate from *endorsers who executed the chaincode* [2]; integrity is rooted in re-execution by a quorum of trusted-enough peers. Aster removes the endorsers: nothing re-executes the tenant computation; the trusted broker signs *data flow* at serve time, and the write set is treated as adversary-chosen but policy-gated.

**TransEdge** is closest in vocabulary — Byzantine transaction processing, authenticated reads, no trusted hardware — with the trust boundary in the opposite location: its untrusted parties are the edge *storage* replicas, defended with classical BFT state-machine replication (3f+1 nodes per partition under BFT-SMaRt), and reads are made verifiable to a *trusted client* via Merkle proofs carrying f+1 replica signatures with dependency-tracking for single-round cross-partition snapshots [4]. Aster inverts the locus: a single trusted broker owns storage and the lease; the *executor* is Byzantine; each served read is sealed with a single-issuer symmetric MAC so the broker can backward-validate the executor's self-certifying read-set at commit. Untrusted storage under replication versus untrusted executor under a trusted broker: complementary threat models whose read-authentication primitives (quorum signatures over Merkle roots vs. single-issuer MACs) are not interchangeable.

**The TEE line** — Ryoan, Opaque, EnclaveDB, VC3 — confines untrusted or exposed execution with enclaves and attestation. Aster's position is that for *executor isolation against a database*, the enclave is unnecessary when a trusted broker authenticates the data flow: executor trustworthiness becomes irrelevant to isolation, which is the property T3 states. The trade is explicit: Aster spends a trusted online process where the TEE line spends attestation hardware, and Aster protects database authority, not the confidentiality of the executor's own computation.

**Cobra** validates serializability of a black-box *database* offline on behalf of trusted clients (OSDI '20) — the inverse trust direction, sharing the "don't verify the computation" spirit but as post-hoc detection. **Object-capability systems** (CapTP and the ocap tradition) supply unforgeable references, and **IFC databases** (IFDB, Qapla) confine queries by policy; neither wires unforgeable *read provenance* into an OCC serializability gate at commit time, which is precisely the composition here. Finally, a name-collision preempt: Berkeley's **DataCapsule** / Global Data Plane names durable signed append-only logs — data at rest; unrelated to Aster's transient per-invocation read seals.

Against this map, Aster's contribution reduces to a specific, checkable composition: (i) serve-time MACs as the *online admission gate* of OCC validation, (ii) the *executor* — not storage — as the untrusted locus, and (iii) *prevention* at a trusted commit gate rather than post-hoc detection, with (iv) the no-TEE/no-BFT cost profile arriving as a consequence of the trusted-broker model rather than a claim.

---

## 8 Discussion, limitations, open problems

**The weakest joint is A6, and we name it.** Range soundness assumes *complete conflict projection*: every mutation that can change a point read or a key-interval scan surfaces as a write event on the corresponding primary/index key, visible to validation. A missing index-entry mutation, an off-by-one endpoint, or a GC/interposition race defeats T2 **without breaking any cryptography**. This is where a reviewer should attack, and where our own effort goes: index-space writes must be derived by the trusted committer (never the cell), the fence/GC/failover interleavings are model-checked in TLA+, and integrating with Convex's real index maintenance requires an audit of that code path before the write plane touches a live deployment. Until then A6 is an assumption we implement against our own log, not a verified property of an integrated system.

**Side channels are excluded, not solved.** "Nothing to exfiltrate" holds for *data the cell was never granted*; it does not extend to covert channels. Timing, cache, and scheduler channels are outside the model (A12), and the deployment obligation is real: cells must run with egress blocked, or exfiltration of authorized data is trivial. The conflict bit of T1b is a named, authorized leakage — one bit about whether an authorized window changed — and a data-dependent read policy leaks its allow/deny outcomes by construction.

**Staleness is the headline's fine print.** Strict serializability is for *mutations*; a read-only invocation sees one consistent, possibly stale snapshot bounded by the retention rule. FoundationDB carries the same caveat for snapshot reads [1]; we repeat it every time the headline appears because omitting it would oversell the guarantee.

**The proof is conditional and the system is not verified.** The theorem is a specification with an explicit assumptions ledger (Table 3); the implementation discharges specific obligations with specific tests (Section 5), and the fence is model-checked in TLA+ — an investment that already caught a real commit-admission liveness bug before any deployment (Section 5) — but no end-to-end code-level verification is claimed. The reality-ledger discipline of the technical report — separating "implemented and tested today" from "proposed and covered conditionally" — is carried into this paper's CLAIMS ledger.

**Operational limits.** Policy revocation across scaled read brokers propagates with bounded skew (F3). Long invocations lose commit liveness past the retention window and must retry (Lemma R). Exactly-once semantics require an application-level idempotency key persisted in the append transaction; capsule freshness is not deduplication (attack obligation 13).

**Open problems.** (1) *Reactive subscriptions over capsules:* invalidation without a resident write log in the cell plane; the promising shape is a broker-side approximate matcher with one-sided error — false positives cost only re-execution; false negatives are forbidden. The range-certificate language is a natural matcher input, but retention, compaction, and epoch handoff need their own proofs. (2) *Learned prewarming ("dream capsules"):* Variant B makes unused predictions free of conflict cost, but a learned selector must be policy-checked per item, must not encode another tenant's secrets in its choices, and capsule size itself may become a side channel or DoS vector. (3) *Zero-knowledge compliance transcripts:* prove capability/protocol compliance without revealing documents — useful as an audit layer, never to be marketed as proof of JavaScript correctness. (4) *Actual-dependency completeness without trusting V8* — the clean open question the repaired theorem exposes: can a system keep prewarm and stateless read serving while proving that every value causally consumed by a Byzantine executor appears in R? A MAC cannot attest to *use*; every known direction (one-time finalization state, a trusted monitor on cache hits, hardware attestation, proof-carrying execution) spends a resource this design deliberately avoids.

---

## 9 Conclusion

Aster makes every submitted OCC observation provenance-authentic without trusting or re-executing the application executor; strict serializability of committed mutations then follows from classical backward validation — snapshot reads stay serializable but possibly stale — while any omitted dependency is demoted to an authorized blind write rather than a cross-authority isolation failure. The cut that pays off is refusing to make the executor trustworthy — no enclave, no replication, no re-execution — and making its *data* unforgeable instead. Under that cut, a V8 escape stops being a database incident: the attacker holds an empty isolate, a socket to a broker that authenticates every read it ever served, and a commit gate that validates authority, not intentions. A compromised executor collapses to a buggy authorized application — the strongest isolation statement available without trusted hardware, and we prove it. The read path costs ~0.34 ms per authenticated read; the write path is built on its own log and fence, proved against real Postgres, with the live-backend integration and its benchmark named as the honest remaining work.

---

## References

[1] Jingyu Zhou et al. "FoundationDB: A Distributed Unbundled Transactional Key Value Store." *Proceedings of SIGMOD*, 2021.

[2] Hyperledger Fabric documentation. "Architecture Explained: Endorsing peer simulation and read/write sets."

[3] Florian Suri-Payer, Matthew Burke, Zheng Wang, Yunhao Zhang, Lorenzo Alvisi, and Natacha Crooks. "Basil: Breaking up BFT with ACID (transactions)." *SOSP*, 2021.

[4] Abhishek A. Singh, Aasim Khan, Sharad Mehrotra, and Faisal Nawab. "TransEdge: Supporting Efficient Read Queries Across Untrusted Edge Nodes." *EDBT*, 2023 (arXiv:2302.08019).

[5] Sujaya Maiyya, Danny Hyun Bum Cho, Divyakant Agrawal, and Amr El Abbadi. "Fides: Managing Data on Untrusted Infrastructure." *ICDCS*, 2020 (arXiv:2001.06933).

[6] Jack O'Connor et al. "BLAKE3: one function, fast everywhere." Specification and design paper.

*In-text systems named without full citations in this draft (to be completed at LaTeX conversion from the related-work dossier): Ryoan, Opaque, EnclaveDB, VC3, Cobra (OSDI '20), SUNDR, Depot, IFDB, Qapla, CapTP, Berkeley DataCapsule / Global Data Plane.*

---

## Appendix pointer

The full model, repairs, proofs, thirteen-attack appendix, assumptions ledger, and variant verdict are in the companion technical report: *The Capsule Transaction Theorem — a repaired proof for Aster v0.7* (27 pp., July 2026). Section 4 of this paper is a faithful condensation; where wording differs, the report governs.
