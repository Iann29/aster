# Aster — related work dossier & novelty framing rules (2026-07-16)

Consolidated from three independent sweeps (industry frontier, academic prior art, two full-text due-diligence reads). Status: **all five flagged neighbors cleared — the composition is unpublished.** This file is the seed of the paper's Related Work section and the guardrails for every claim we make in public.

## The one-sentence thesis (vs. the nearest mechanism)

> FoundationDB's resolver performs exactly Aster's OCC backward validation over client-declared read/write conflict ranges — while trusting the client entirely; a client that under-declares reads commits what OCC should have aborted. Aster makes the resolver's input trustworthy: every read-set entry is a serve-time broker MAC, so the executor leaves the TCB.

## Claim guardrails — what is ours and what is NOT

**Claim (novel, defensible):**
1. **Serve-time MAC-sealed read-sets as an ONLINE soundness gate for OCC** against fully Byzantine executors — per-read authentication feeding backward validation at a trusted committer. No system found does this (FDB trusts declarations; Fabric's signed rwsets come from endorsers who re-executed; Fides authenticates the log retroactively for audit).
2. **Untrusted EXECUTOR locus** — Byzantine compute against a trusted storage-owning broker. The literature's untrusted party is nearly always storage (Fides, SUNDR, Depot, outsourced-DB/ADS, TransEdge) or the whole replica set (Basil, Byzantium, Augustus).
3. **Online PREVENTION of strict-serializability violations** (a live safety property at the commit gate), where the no-trusted-party line (Fides) can only offer post-hoc DETECTION/auditability.
4. **"A trusted broker makes TEEs unnecessary for executor isolation"** — the TEE line (Ryoan, Opaque, VC3, EnclaveDB) spends attestation hardware to make the executor trustworthy; Aster's claim is that executor trustworthiness is *irrelevant to isolation* when data flow is authenticated. Stated explicitly nowhere we could find.
5. **"For isolation, authenticate data flow — don't verify computation"** (the Absurd-Idea-2 thesis). Nearest expressions: Cobra (validates serializability of a black-box DB — inverse direction), Opaque (verifies dataflow between enclave-run operators — still TEE-backed). Not stated as a general principle anywhere found.

**Do NOT claim (prior art owns these):**
- ✗ "First serializable transactions on untrusted infrastructure **without BFT replication**" — **Fides (ICDCS 2020) claimed exactly this in 2020.** Our no-BFT/no-re-execution property is inherited from the trusted-broker model, not novel in itself. Frame it as a *consequence*, never as the contribution.
- ✗ "Code that never holds a credential" as a stand-alone novelty — industry table stakes by 2026 (Anthropic Managed Agents, Cloudflare Code Mode, Vercel Sandbox credential brokering). Ours is the *data-plane* half: sealed inbound reads + commit-time validation.
- ✗ "Byzantine clients + serializability" as such — Basil (SOSP 2021) does it (with 5f+1 BFT replication); our delta is the cost model (one trusted broker), not the goal.

## Positioning map (trust locus × guarantee)

| System | Untrusted party | Mechanism | Guarantee | Cost |
|---|---|---|---|---|
| FoundationDB | nobody (client in TCB) | declared conflict ranges + OCC resolver | serializability *if clients honest* | — |
| Basil (SOSP'21) | clients + replicas | BFT quorums, 5f+1, commit certificates | Byzantine-tolerant serializability | replication ×5+ |
| Hyperledger Fabric | peers/clients | endorser re-execution + signed rwsets, version validation | endorsement-policy integrity | re-execution ×endorsers |
| Fides/TFCommit (ICDCS'20) | storage + coordinator + clients (no online trusted party) | CoSi-signed hash-chained log + Merkle state; offline auditor | **detection** (auditability), post-hoc | audit infrastructure |
| TransEdge (EDBT'23) | edge storage replicas | BFT-SMaRt per partition + Merkle proofs + f+1 quorum sigs to trusted clients | serializable reads vs untrusted storage | replication 3f+1 |
| Ryoan/Opaque/EnclaveDB | executor/operator hosts | SGX enclaves + attestation (+ Opaque: dataflow self-verification) | confidentiality/integrity via hardware | TEE + attestation pipeline |
| Cobra (OSDI'20) | the database | offline SMT serializability checking by trusted clients | detection, post-hoc | audit compute |
| **Aster** | **the executor (arbitrary app code)** | **serve-time keyed-MAC capsules + OCC backward validation at the lease-holding broker** | **online prevention: strict serializability over declared, authenticated sets** | **one broker process** |

## Drop-in paragraphs (academic tone, agent-verified full-text reads)

### TransEdge

> Closest in vocabulary is TransEdge [Singh et al., EDBT 2023], a Byzantine transaction-processing system for untrusted edge environments that, like Aster, targets serializable transactions with authenticated reads and no trusted hardware. TransEdge, however, places the trust boundary in the opposite location: its untrusted parties are the edge *storage* replicas, which it defends with classical Byzantine state-machine replication — each partition is held by a cluster of 3f+1 nodes running BFT-SMaRt, and reads are made verifiable to a *trusted client* via Merkle-tree proofs carrying f+1 replica signatures together with a dependency-tracking (LCE/CD-vector) scheme that yields single-round cross-partition snapshots. Aster instead assumes a *single trusted broker* that owns storage and a single-writer lease, and treats the transaction *executor* — arbitrary application code — as fully Byzantine; each served read is sealed with a keyed-BLAKE3 MAC binding cell identity, lease epoch, and snapshot timestamp, so the broker can perform FoundationDB-style OCC backward-validation of the executor's self-certifying read-set at commit. Consequently TransEdge relies on replication and consensus (and re-executes read-write commits across the quorum), whereas Aster deliberately avoids BFT replication, re-execution, and TEEs, deriving strict serializability from a single authenticated validation step. The two systems therefore address complementary threat models — untrusted *storage* under replication (TransEdge) versus an untrusted *executor* under a trusted broker (Aster) — and their read-authentication primitives (quorum signatures over Merkle roots vs. single-issuer symmetric MACs) are not interchangeable.

### Fides / TFCommit

> Closest in spirit is Fides [Maiyya et al., ICDCS 2020], whose TFCommit protocol commits serializable transactions across multiple *untrusted* storage servers without Byzantine replication, using a hash-chained, collectively-signed (CoSi) tamper-evident log over Merkle-authenticated datastores. Fides and Aster share the goal of transactional integrity on untrusted infrastructure while avoiding BFT state-machine replication, but they invert the trust model and, with it, the guarantee. Fides trusts no online party — storage servers, the commit coordinator, and clients may all be Byzantine — and therefore offers *auditability*: incorrect executions are not prevented but are made irrefutable and detected after the fact by an external, offline auditor that reconstructs a serialization graph from the signed log. Aster instead places a single trusted, lease-holding broker in the online data path and treats only the executors as Byzantine, allowing it to *prevent* rather than merely detect violations: every served read is sealed at serve time with a keyed MAC binding it to (cell, lease epoch, snapshot), and the broker validates the authenticated read-set through FoundationDB-style optimistic backward-validation before any commit is admitted. This concentration of trust also changes the cryptography — Fides must adopt publicly verifiable collective signatures precisely because it has no trusted verifier, whereas Aster's symmetric MAC is sound only because the broker both seals and checks it. Consequently Fides authenticates the *log* for post-hoc dispute resolution, whereas Aster authenticates *per-read read-sets* to render optimistic concurrency control sound against Byzantine executors online — a mechanism, and a strict-serializability safety guarantee, that TFCommit's detection-based design does not provide.

## Follow-ups still open

- Basil deserves its own careful full-text read at paper-writing time (the first sweep covered it well, but it's the closest on "Byzantine clients issuing validated transactions" and referees will know it).
- Name collision note: Berkeley "DataCapsule"/Global Data Plane = signed durable append-only logs (data at rest); unrelated to Aster's transient per-read seals. Preempt the confusion in the paper.
- Maiyya's UCSB thesis (extended Fides) — skim before submission for same-group follow-ups.
- The industry incidents for the motivation section: AWS AgentCore IAM-credential exfiltration via metadata service (Sonrai/Unit42, Feb 2026); Supabase MCP `service_role` leak (the "lethal trifecta" case, 2025).
