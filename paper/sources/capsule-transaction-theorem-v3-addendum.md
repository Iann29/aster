# The Capsule Transaction Theorem — Seal-v3 and Session Protocol Addendum

## Direct-MAC framing, bearer-session binding, exact protocol leakage, retention well-formedness, and the current implementation ledger

**Issued:** 28 July 2026  
**Protocol revision:** Aster v0.7, capsule domain v3, seal algorithm v3

**Normative status.** This addendum is part of *The Capsule Transaction Theorem — a repaired proof for Aster v0.7*. It closes re-referee finding F9 for the shipped v3 protocol. It supersedes only the report text listed in §7 below. Every definition, counterexample, lemma, and proof not expressly replaced remains in force.

For the shipped wire protocol, the retired prehash seal is historical only. The governing seal is v3, the governing channel object is a one-attempt bearer session, and the governing T1a statement is issuance authenticity for that bearer session and its broker-registered label—not physical-cell attribution. The addendum also records the exact whole-protocol leakage, adds retention invariant R0 without changing Lemma R or T2, and re-stamps the implementation ledger.

---

## 1. Exact shipped protocol

### 1.1 Canonical capsule encoding v3

Let `lp(x) = le64(|x|) ∥ x`, where `le64` is the unique eight-byte little-endian encoding of an unsigned 64-bit integer. Let

- `ALG3 = "aster-blake3-keyed-v3"`, encoded as its exact ASCII bytes;
- `CAPDOM3 = "aster-capsule-v3\0"`, including the terminal zero byte.

For a well-formed capsule

\[
C = (tenant, deployment, s, docs, ranges),
\]

define the canonical encoding

\[
E_3(C)= CAPDOM3 \parallel lp(tenant) \parallel lp(deployment) \parallel le64(s)
        \parallel EncDocs(docs) \parallel EncRanges(ranges).
\]

`EncDocs` carries a count, strictly ordered unique keys, and canonical versioned documents. `EncRanges` carries an ordered range-certificate sequence. Each sum type has a fixed tag; each list has an explicit count; strings and byte strings are length-prefixed; integers are fixed width; and canonical decoding rejects duplicates, noncanonical order, malformed range cross-references, trailing bytes, and any invalid structure. The original report's Lemma 3.1 therefore applies unchanged after replacing `E` by `E3` and the capsule domain by `CAPDOM3`.

The ordered range-certificate sequence is part of the authenticated content. In particular, interval endpoints, direction, limit, returned keys, and `Exhausted` versus `Boundary` are all inside `E3(C)`.

### 1.2 Session registry and trusted reconstruction

A wire broker maintains a trusted in-process partial map

\[
Reg : SessionId \rightharpoonup (cid, e, expiry),
\]

where `SessionId = {0,1}^{256}`. Formally, a *session instance* is a mint event `μ` together with its 32-byte wire value `q_μ`. The live registry is keyed by `q_μ`. Let `NR` be the event that no two mint events in the execution receive the same wire value. For at most `Q` mints from an ideal 256-bit source,

\[
Pr[\neg NR] \le \frac{Q(Q-1)}{2^{257}}.
\]

All statements below are conditioned on `NR`; its failure is absorbed into the displayed negligible term. We write `q` for the session instance and its wire value when the distinction is immaterial. On `InitialCapsule`, the broker:

1. rejects a request that already contains a session binding;
2. obtains the current authority epoch `e_B` from the broker's lease authority;
3. takes the requested `cid_req` as the label to register, subject to any separate launch-authentication mechanism covered by A11;
4. samples a fresh 32-byte session `q` from operating-system entropy, retrying on the negligible event of an in-table collision;
5. registers `Reg[q] = (cid_req, e_B, expiry)`; and
6. seals the initial capsule under the bound context

\[
ctx_q = (cid_q, e_q, q), \qquad (cid_q,e_q)=Reg[q].
\]

The broker's tenant and deployment are broker configuration, not session-table claims. They are checked against the capsule header after seal verification. The snapshot is part of the capsule and is checked against the broker's snapshot mode and authoritative history.

For every later capsule verb, the presented session is a lookup key into `Reg`. The broker reconstructs `ctx_q` from the table entry. Any serialized context supplied by the requester is only a consistency claim: its `cid` and epoch must equal the table entry, and its optional session must be absent or equal to `q`. Cryptographic verification is performed against the reconstructed context, never against a context chosen solely by the requester.

This is a **bearer-capability** channel. Possession of `q` authorizes presentation of that session. The registry proves no fact about the physical process presenting it. One session scopes one transaction attempt: the holder may make nonterminal hydrate, ID-mint, and module-input requests, followed by at most one terminal `Commit` or `Abort` while the session remains valid. At mint, `cid` is request-supplied unless A11 is separately discharged by a trusted launch token, verified peer credential, or equivalent launch authority.

### 1.3 Exact v3 direct-MAC frame

Define the session frame

\[
SF(\bot)=\mathtt{00}, \qquad SF(q)=\mathtt{01}\parallel q
\]

for a 32-byte bound session `q`. The shipped hostile-wire path uses only the bound form. The unbound form is retained solely for in-process callers and is cryptographically disjoint from every bound form.

The exact v3 MAC message is

\[
\begin{aligned}
M_3(ctx_q,C) ={}& ALG3
\parallel lp(cid_q)
\parallel le64(e_q)
\parallel SF(q)
\parallel lp(E_3(C)).
\end{aligned}
\]

Expanding `E3`, this is a domain-separated canonical framing of

\[
(cid_q,e_q,q,tenant,deployment,s,docs,ranges).
\]

The algorithm string is not length-prefixed in the implementation, but it is one exact accepted constant at a grammar-known offset and of a grammar-known length. The verifier rejects every other algorithm string. The session tag determines whether zero or exactly 32 session bytes follow. The final capsule encoding is length-prefixed and begins with the independent capsule domain `CAPDOM3`.

Let `F_κ` be keyed BLAKE3 with a 32-byte key and its full 32-byte output. The shipped tag is

\[
\tau = F_\kappa(M_3(ctx_q,C)).
\]

There is **no prehash in the MAC path**. The seal also carries

\[
d = H(E_3(C))
\]

as a recomputed audit/tooling digest. Verification checks that this field equals the unkeyed BLAKE3 digest of the capsule, but `d` is not an input to `M3`. Therefore collision resistance of `H` is not a premise of T1a-v3. A collision in the audit digest does not permit capsule substitution because the full distinct canonical capsule bytes still change the keyed-MAC message.

### 1.4 Verification relation

Given a session entry yielded by a live lookup, or by a successful consume for `Commit`, `Verify3` accepts a sealed capsule only if all of the following hold:

1. the algorithm is exactly `ALG3` and the tag has the exact full length;
2. the seal's public `cid`, epoch, and session claims equal the broker-reconstructed `ctx_q`;
3. the capsule satisfies structural validity and has one canonical semantic encoding `E3(C)`;
4. the carried audit digest equals `H(E3(C))`;
5. the constant-time tag comparison accepts `F_κ(M3(ctx_q,C))`;
6. the capsule tenant and deployment equal the broker's configured tenant and deployment; and
7. the capsule snapshot satisfies the broker's snapshot-mode checks.

The public seal fields are consistency claims and friendly-error surfaces. The enforcement point for session transplant is the session bytes inside `M3`.

---

## 2. Revised statements

### 2.1 Issuance relation for a bearer session

For each broker-minted session `q`, let `Label(q)=(cid_q,e_q)` be the immutable pair recorded at mint. Let `Issued3(q)` be the least set satisfying:

1. after registering `q`, the trusted broker may issue an initial well-formed capsule for its configured tenant/deployment and an admissible snapshot, sealed under `ctx_q`;
2. given any sealed capsule in `Issued3(q)`, the broker may verify it under `ctx_q`, perform one policy-authorized point or range read at that capsule's fixed snapshot, merge the exact result, and seal the resulting complete capsule under the same `ctx_q`; and
3. no other capsule enters `Issued3(q)`.

The relation is a branching tree. Replaying an earlier issued capsule during a live session may create another authorized branch. Session binding does not create a latest-capsule chain.

### 2.2 Theorem T1a-v3 — bearer-session issued-capsule authenticity

**Theorem T1a-v3.** Assume:

1. all holders of `κ` are trusted and invoke sealing only through `Issued3`;
2. keyed BLAKE3 with full 32-byte tags is EUF-CMA secure;
3. canonical capsule encoding is injective on well-formed capsules;
4. the v3 outer frame is injective as proved in Lemma 3.2-v3 below;
5. verification performs the exact checks of §1.4; and
6. the session registry and the broker's reconstruction of `ctx_q` are trusted.

For every probabilistic polynomial-time coalition of hostile cells, consider any capsule operation for which the session gate yields the registered entry for `q`—by a live lookup for a nonterminal verb, or by a successful atomic consume for `Commit`. The probability that `Verify3` accepts `(q,C,τ)` and

\[
C \notin Issued3(q)
\]

is at most

\[
Adv^{euf-cma}_{F}(\mathcal B)
+ \frac{Q(Q-1)}{2^{257}}
+ negl(\lambda),
\]

where `Q` bounds session mints in the execution. The middle term is only the probability of a repeated random session identifier; it is not a hash-collision assumption.

Consequently, except with negligible probability, an accepted capsule was actually issued by a trusted broker for the **session instance whose broker-minted bearer value was presented**, its registered **self-asserted label** `Label(q)`, and the broker-configured tenant/deployment.

This theorem does **not** prove that the presenter is the physical cell denoted by `cid_q`. Without A11, `cid_q` is the request-supplied label registered at mint. Under A11, a separate launch-authenticity corollary may interpret that label as a trusted launch identity; T1a-v3 itself remains the narrower bearer-session statement.

EUF-CMA, rather than strong unforgeability, is sufficient. A new tag on a message that was already issued would still authenticate an issued capsule and would not satisfy the T1a-v3 winning condition. Keyed BLAKE3 is deterministic in this use in any event.

### 2.3 Counterexample CE2.1-v3 — replay survives, attempts do not multiply

Let `C0` be the initial capsule issued under live session `q`, and let `C1` be a later capsule obtained by hydrating `C0`. Both exact byte objects remain valid under `q`. The hostile executor may compute from `C1` and submit `C0`; verification cannot infer that `C1` was later or causally used. Thus the original counterexample to “latest capsule” and “complete actual dependency set” remains intact.

The shipped session lifecycle changes only the number of commit attempts:

- `Commit(q, ...)` atomically removes `q` from `Reg` **before** context validation, seal verification, policy checks, or fence execution. Every structured outcome after a successful removal—context mismatch, malformed capsule, policy denial, conflict, stale epoch, retention rejection, backend error, or success—leaves `q` closed.
- `Abort(q)` atomically removes `q` and performs no commit. It is the no-commit close.
- expiry removes or invalidates abandoned sessions.

Therefore replay of issued capsule bytes is inevitable **within the live bearer attempt**, but at most one commit request can consume that session and proceed beyond the session gate. A capsule issued under `q` cannot be transplanted to a fresh session `q'` because `q` is inside the MAC message. After `q` is closed, replay dies as `unknown_session` before fence work.

This is stateful at-most-one-attempt enforcement at the session layer, not cryptographic freshness and not durable exactly-once execution. A lost response may leave the client unable to distinguish a committed attempt from a closed failed attempt. A hostile application may also open a fresh session and submit a new authorized transaction. Neither behavior contradicts T1a, T2, or T3.

### 2.4 Lemma S1 — atomic consume-first

**Lemma S1.** Suppose `Reg.remove(q)` is one atomic operation. Among any number of concurrent `Commit` and `Abort` requests presenting the same live session `q`, at most one request obtains the table entry. At most one `Commit` request can therefore reach the commit fence under `q`.

The result is independent of the commit outcome. It also covers a race between `Abort` and `Commit`: exactly one may consume the capability; every loser observes an unknown or closed session.

### 2.5 Ideal read and session functionality

Define `Fgrant-v3` as the original `Fgrant` extended with bearer-session lifecycle. On a successful initial request it samples a fresh random session `q`, records `Label(q)`, fixes the capsule snapshot, policy-checks every prewarm grant, and returns `q` together with the initial sealed capsule. On a later point or range request, it requires a live `q`, rebuilds the registered context, applies the read policy, and returns the exact point/range result at the fixed snapshot or a denial. Its explicit transcript includes:

- the session identifier delivered to its holder;
- the registered public label, broker tenant/deployment, snapshot, request identities, framing, lengths, and policy outcomes;
- exact authorized point and range grant payloads; and
- session-gate and context-consistency error classes.

It includes no timing, cache, scheduling, speculative-execution, or other excluded side channel.

### 2.6 Ideal full functionality and exact protocol leakage

Define `FAster-v3` to contain `Fgrant-v3`, the repaired atomic commit fence, and the consume-first session state machine. Its ideal commit result is the exact structured wire result. At minimum, the result algebra contains branches of the following forms (plus syntax, policy, and backend errors already public in the wire protocol):

\[
\begin{aligned}
CommitResult ::= {}& Committed(c) \\
 &\mid Conflict(k^*) \\
 &\mid StaleEpoch(e_{now}) \\
 &\mid SnapshotBeyondHorizon(h) \\
 &\mid SnapshotBelowRetention(g) \\
 &\mid Rejected(class, public\ metadata).
\end{aligned}
\]

The constructors above are mathematical names; the simulator emits the implementation's exact serialized response and payload. Thus the full transcript exposes the exact wire-visible result, not merely a conflict bit. In particular, where the corresponding branch is reached, it reveals:

- the **exact conflicting key** `k*` selected and returned by conflict validation;
- the returned current **lease epoch** `e_now`;
- the returned validation **horizon/log tip** `h`;
- the returned **retention floor** `g`;
- the exact successful **commit timestamp** `c`;
- the rejection class and any other concrete public metadata deliberately carried by the structured response; and
- the fact that the session has been consumed or aborted.

These values are explicit protocol leakage, not side channels. In particular, an exact conflict key found inside an authorized range may name a post-snapshot key that was not part of the original grant. T1b permits that disclosure because it is named in the ideal transcript; it must not be described as one bit.

`FAster-v3` never releases a database document value except through an authorized read grant. Its conflict result may release an authorized key name, but not the document stored at that key.

### 2.7 Module bundles are launch inputs outside the capsule theorem

Module bundles are broker-provided launch inputs. A cell may request an authorized module path and receive the corresponding bundle bytes from the broker's module source. Those bytes do not arrive inside a capsule and are not authenticated by T1a as capsule observations.

For whole-protocol simulation, define a separate ideal launch-input functionality `Fmodule` whose transcript contains the authorized path, bundle identity/hash metadata, and the exact bundle bytes returned. T1b-v3 is parameterized by this explicit launch-input transcript. The capsule theorem makes no claim that module bytes are database grants, and no claim about their confidentiality or provenance beyond the separate module-authorization and bundle-verification mechanism.

### 2.8 Theorem T1b-v3 — honest transcript statement

**Theorem T1b-v3.** Under the report's trusted-broker/storage assumptions, A2's PRF clause, A4's key/domain discipline, the session-registry premise, A12's process-isolation boundary, and the exact ideal transcripts above, for every PPT coalition controlling sessions `J`:

1. its untimed read-plane view is computationally indistinguishable from a simulation given the broker's public configuration and the joint transcript of `Fgrant-v3`;
2. its untimed whole-protocol view is computationally indistinguishable from a simulation given the joint transcripts of `FAster-v3` and `Fmodule`.

Accordingly, except with the PRF/T1a loss, the session-repeat term above, and an off-coalition guessing term at most `Q_G Q_H / 2^256` for `Q_G` guesses against `Q_H` honest live sessions, the coalition learns no database document payload beyond the adaptive transitive closure of its policy-authorized grants. It additionally learns every exact control value and module byte string named above. These are part of the ideal functionality and are not hidden by the theorem.

The statement is intentionally honest about the bearer model: colluding principals may share sessions, capsule bytes, and module bytes out of band. A session accepted by the broker authenticates possession of the capability, not the physical origin of the request.

### 2.9 Shipped validated set and preservation of T2

The shipped commit request declares point keys only. Let `S_points` be the duplicate-free declared subset of capsule document keys. The implementation derives a conflict window from **every** sealed range certificate. Therefore the actual authenticated observation set is

\[
R_{impl}(C,S_{points}) =
\{Point(k,C.docs[k]) : k\in S_{points}\}
\cup
\{Range(j,C.ranges[j]) : 0\le j<|C.ranges|\}.
\]

This is Variant B for points and whole-capsule/Variant A for ranges. Exact range under-declaration is not shipped. The T2 induction remains unchanged after substituting `Rimpl` for the abstract `R`: every successful commit is strictly serializable for the exact point atoms selected by `S_points` and all sealed range atoms carried by the accepted capsule.

Session consume-first does not strengthen or weaken the serialization proof. It removes attempts from consideration before the fence. Failed or replayed requests do not enter the successful history; the at-most-one fence attempt per session is an independent lifecycle property.

T2 still requires one authoritative history for snapshot reads, conflict projection, retention, and append. This addendum closes F9 only. The current tree contains a shipped Aster-owned `WritePlane` whose local read/validate/append path can satisfy that premise, but the system-level composition remains outside T2 wherever live transaction reads come from Convex while validation and append use `aster.log`. Numerical alignment of the two timestamp spaces is not history identity. The reality ledger therefore preserves F1 explicitly: the standalone Aster plane may instantiate T2; the split Convex-read/Aster-log pipeline does not.

### 2.10 Retention well-formedness invariant R0

Add the independent invariant

\[
\mathbf{R0}: \qquad 0 \le g \le tip(L).
\]

R0 is not a premise needed to prove the safety of a commit that already passed Lemma R's complete retained-suffix validation. A floor above the tip blocks admissible snapshots; it does not create an illegal successful commit. R0 is therefore a storage well-formedness and obstruction-freedom invariant, not part of T2 and not part of Lemma R.

**Proposition R0-OF (fresh-attempt obstruction freedom).** Assume R0, ordinary storage availability, a current lease epoch, stable authorizing policy, a fresh snapshot `s = tip(L)` that also satisfies the product max-age rule, and no committed write intersecting the attempt's authenticated observations. Then `g ≤ s`; hence retention does not obstruct the attempt, and the repaired fence reaches append absent another independently named rejection condition or a concurrent event that invalidates one of these premises.

The implementation discharges R0 in two places:

1. `advance_retention` clamps a requested floor to the current tip before monotonic advancement, so an R0 state remains an R0 state; and
2. lease acquisition repairs or refuses any legacy preexisting `g > tip(L)` state before a new writer epoch becomes operational.

Lemma R remains exactly the report's safety statement: a successful validator must have complete immutable coverage of `(s,h]`, represented by `g ≤ s` under a retention pin. No liveness sentence is added to T2 or Lemma R.

---

## 3. Replacement proofs

### 3.1 Lemma 3.2-v3 — outer-message injectivity including session

**Lemma 3.2-v3.** For well-formed capsules `C,C'` and contexts whose session modes are valid, if

\[
M_3(ctx,C)=M_3(ctx',C'),
\]

then the algorithm version, `cid`, lease epoch, session mode, session identifier, tenant, deployment, snapshot, document map, and ordered range-certificate sequence are pairwise equal. In particular, `ctx=ctx'` and `C=C'`.

**Proof.** The verifier accepts one exact `ALG3`, so remove the common prefix of known length. The next eight bytes are the unique length of `cid`; equality gives the same length and the same following bytes. The next eight bytes give the same epoch.

The next byte is the session-mode tag. Equality gives the same tag. If it is `00`, both contexts are unbound and consume no session bytes. If it is `01`, both consume exactly the following 32 bytes, so the session identifiers are equal. No other tag is valid.

The next field is `lp(E3(C))`. Equality gives the same encoded length and the same exact capsule byte string. By canonical-encoding injectivity, `E3(C)=E3(C')` implies `C=C'`. Since tenant, deployment, snapshot, documents, and ranges are components of `C`, each is equal. End-of-input after the length-delimited capsule rules out suffix ambiguity. ∎

**Corollary 3.2.1.** A bound capsule issued under session `q` cannot have the same MAC message under a distinct session `q'`, even when `cid`, epoch, tenant, deployment, snapshot, and capsule content are otherwise identical.

### 3.2 Proof of T1a-v3

Model every trusted seal issuance, over all coalition-controlled sessions and branches, as a chosen-message query to one MAC oracle for `Fκ`. The reduction records the exact pair `(q,C)` and exact message `M3(ctx_q,C)` for every oracle query. This grants the adversary at least the real system's adaptive sealing power.

Suppose the adversary submits session value `q*`, capsule `C*`, and tag `τ*`; the session gate returns the registered entry for that mint instance (by lookup or consume), and all structural, algorithm, context, digest, tenant/deployment, snapshot, and tag checks pass, yet `C* ∉ Issued3(q*)`. Condition on `NR`. Let

\[
M^* = M_3(ctx_{q^*},C^*).
\]

There are two syntactic possibilities, but only one can satisfy the winning condition:

1. **`M*` was never queried.** Acceptance means `τ*` is a valid full tag for a fresh MAC message. The reduction outputs `(M*,τ*)` as an EUF-CMA forgery.
2. **`M*` was queried.** Let the recorded query be generated by `(q',C')`. Lemma 3.2-v3 gives `q'=q*`, equal registered label/context fields, and `C'=C*`. Hence `C*` was issued under `q*`, contradicting the winning condition.

Thus, conditioned on `NR`, every winning execution induces a fresh-message MAC forgery. Removing the conditioning adds only the explicit 256-bit session-repeat bound. There is no digest-collision case: `H(E3(C))` is not in the keyed-MAC message, while the full `E3(C)` is. The old Proposition 3.3 and the collision branch of the old proof are retired for v3. Exact algorithm dispatch removes downgrade ambiguity, and the session frame removes cross-session message equality. This proves the stated bound. ∎

### 3.3 Proof of Lemma S1

`Reg.remove(q)` is linearizable under one mutex-protected map operation. Exactly one concurrent caller can observe and remove the live entry. After that linearization point, `q` is absent. Any other `Commit` or `Abort` presenting `q` fails before context reconstruction or fence execution. Because `Commit` performs this removal before invoking the fence, no two commit requests under one session can both reach the fence. ∎

### 3.4 Proof of T1b-v3

For the read plane, the simulator samples sessions with the same distribution as the broker and maintains the same ideal registry, expiry, and branch structure. From the ordered grant transcript it reconstructs every canonical capsule state: each transition is the deterministic merge of a prior well-formed capsule and the exact authorized point/range answer. It computes `E3`, the public audit digest, and the v3 message frame including the sampled session.

Replace real keyed BLAKE3 by a random function under A2's PRF clause. The simulator maintains one lazy random-function table keyed by `M3`; repeated messages receive the same 256-bit tag. Relative to the report's original hybrid, the table key is the full direct-MAC message `M3`, not the retired prehash message, and the T1a failure term contains no unkeyed-collision term. A real verifier accepting an unissued capsule is bounded by T1a-v3.

For the full protocol, the simulator additionally maintains consume-first session state. It emits the exact ideal commit response supplied by `FAster-v3`, including the conflicting key and the concrete epoch, horizon, retention-floor, and commit-timestamp values. It emits module bundles exactly as supplied by `Fmodule`. Every database document payload in the real view comes from an allowed grant; every other state-dependent wire value is already named in the ideal transcript. Excluded timing and covert channels remain excluded. This proves T1b-v3. ∎

### 3.5 Preservation of T2 and the existing stability lemmas

Lemmas 3.6 and 3.7 continue to establish point and limited-range stability. Lemma 3.8 now invokes T1a-v3 to conclude that each atom in `Rimpl` came from a broker-issued capsule under the consumed bearer session. The commit fence validates declared point keys and all derived range windows against one stable horizon, and atomic append remains the linearization point.

The induction in the original proof of T2 is otherwise unchanged. Session lifecycle only changes which attempts reach the fence. Direct MAC strengthens the cryptographic premise by removing a separate collision assumption. The hybrid point/range declaration rule changes the accepted set `R`, not the proof schema. No claim is made across a two-history F1 seam.

### 3.6 Preservation and repair of R0

Suppose R0 holds before `advance_retention`. Let the requested floor be `r` and the current tip be `t`. The clamped candidate is `min(r,t)`, and monotonic advancement chooses

\[
g' = \max(g,\min(r,t)).
\]

Because `g≤t` and `min(r,t)≤t`, it follows that `g'≤t`. Nonnegativity is immediate. Since the log tip is monotone, a later tip remains at least `t`. Thus the clamp preserves R0.

If an older binary left `g>tip(L)`, preservation alone cannot repair the state. The lease-acquisition repair restores `g≤tip(L)` or prevents the new epoch from serving requests. Once repaired, the clamp preserves the invariant. Under R0, choosing `s=tip(L)` gives `g≤s`, which is the retention part of Proposition R0-OF. This argument is separate from Lemma R's safety proof. ∎

---

## 4. Updated assumptions ledger

Only the following ledger entries change; all others remain as in the report.

| ID | Status after this addendum | Exact role |
|---|---|---|
| A2 | **Active.** Keyed BLAKE3 full-output EUF-CMA security; PRF security may still be used for the report's random-function presentation of T1b. | EUF-CMA is the sole cryptographic hardness premise in T1a-v3. No strong-unforgeability premise is required. |
| A3 | **Retired; not an assumption of v3.** | Unkeyed BLAKE3 collision resistance was required only by the retired prehash construction. The audit digest is not a MAC input. No theorem bound contains an A3 term. |
| P2 | **Replaced by Lemma 3.2-v3.** | The exact outer frame is injective in algorithm version, `cid`, epoch, bound/unbound mode, session, and the complete canonical capsule. |
| A4 | **Active, clarified.** | `κ` is secret and the exact v3 algorithm/capsule domains are disjoint from other uses. Test-derived keys are not the production-key model. |
| A11 | **Active environmental assumption, narrowed.** | A trusted launch token, verified peer credential, or equivalent authority is required to interpret the request-supplied `cid` registered at mint as a physical launch identity or to base `cid`-specific authority on it. Tenant/deployment are broker configuration and the epoch is authority-derived. T1a-v3 without A11 authenticates only the bearer session and its registered self-asserted label. |
| A11-S | **Active protocol/implementation premise.** | Session IDs are sampled uniformly from OS entropy; repeats and off-coalition guesses are negligible; the registry is trusted; lookup and consume are linearizable; the broker reconstructs expected context from the registry; bearer leakage or voluntary sharing is outside origin attribution. |

The report's active assumption set must no longer list unkeyed collision resistance as load-bearing. Cross-references to “MAC forgery or hash collision” in the T1a, splice, cross-context, encoding, and attack-appendix discussions become “MAC forgery,” except where the text is explicitly recounting the retired v2 construction.

---

## 5. Current implementation reality ledger

This ledger distinguishes theorem components that are shipped from system-level compositions that remain conditional.

| Surface | Current status | Formal consequence |
|---|---|---|
| Canonical capsule v3 | **Shipped.** Capsule domain `aster-capsule-v3\0`; ordered range certificates are canonical content. | Original canonical-encoding and range-stability proofs apply. |
| Seal v3 | **Shipped.** Exact algorithm `aster-blake3-keyed-v3`; direct keyed MAC over the full framed capsule; v1/v2 rejected; constant-time comparisons; audit digest retained but not MACed. | T1a-v3 is a pure EUF-CMA reduction. A3 is retired. |
| Bearer sessions | **Shipped.** Broker-minted 32-byte IDs, in-process `session → (cid,epoch)` table, table-rebuilt context, TTL/capacity, session inside the MAC. | T1a means issuance for the presented session instance and its registered self-asserted label. Physical-cell attribution remains conditional on A11. Sticky routing is required by the in-process table. |
| Session end-of-life | **Shipped.** Commit atomically consumes before the fence; Abort is no-commit close; all post-consume outcomes close the session. | At most one fence attempt per session. Issued-byte replay and non-latest branch selection remain possible before close. |
| Write plane and fence | **Shipped for the standalone Aster-owned transaction plane.** Lease, horizon, retention, conflict validation, and append are implemented against `aster.log`. | The local T2 proof applies only when capsule reads are also issued from this same history. |
| F1 two-plane boundary | **Still present in the system-level Convex-read/Aster-log composition and any evaluation using it.** Convex transaction reads and Aster-log validation/appends are different histories; matching timestamp integers does not merge them. The tree's standalone Aster read path is a separate candidate T2 instance, not a repair by implication of the split path. Module tables are a separate launch-input plane, not transaction history. | This addendum closes F9, not F1. End-to-end T2 must not be claimed for the split pipeline, and the “report governs” pointer does not erase this limitation. |
| Module bundles | **Shipped as broker-provided inputs outside capsules.** | Included in `Fmodule`, not in T1a's observation authenticity or T1b's database-grant closure. |
| Policy | **Deployment-wide only.** One broker policy envelope applies to every session in the configured tenant/deployment; it may constrain deployment prefixes and limits, but it is not physical-cell- or `cid`-specific. Dynamic revocation/versioning is outside this ledger unless separately implemented. | T1b/T3 confinement is only as fine-grained as the common deployment policy. No finer identity-derived authority follows from the session bearer. |
| Variant selection | **Hybrid.** Variant B for point keys; every sealed range certificate is validated. | `Rimpl = S_points ∪ AllRanges(C)`. T2 survives for `Rimpl`; exact range subsets and their no-false-conflict ergonomics are not shipped. |
| Retention R0 | **Shipped by tip-clamped advancement plus lease-acquisition repair.** | Adds fresh-attempt obstruction freedom; does not alter T2 or Lemma R. |
| Read-broker scale-out C1 | **Architectural theorem, not a uniform current implementation property.** The in-process session table requires sticky ownership unless a shared/self-verifying session mechanism is added. | C1 remains valid for the abstract protocol but must not be advertised as any-broker session mobility in this implementation. |

---

## 6. Consequences for the attack appendix

The following concise replacements keep the original appendix structure.

### Cross-session transplant

A capsule issued under `q` and presented under `q'≠q` changes the session frame and therefore the direct-MAC message. Rewriting the attacker-visible seal session claim to `q'` may pass the friendly equality precheck, but the tag still verifies against a message containing `q'` rather than the issued `q` and fails unless the adversary forges the MAC. This is bearer-session separation, not proof of physical wrong-cell origin.

### Capsule splicing and substitution

Splicing, field replacement, range-certificate editing, or any other change to a well-formed capsule changes `E3(C)` and therefore changes `M3`. If the resulting complete capsule was not issued under the presented session, acceptance yields a fresh-message MAC forgery. No unkeyed-hash collision branch exists.

### Rollback, fork, and whole-capsule replay

An earlier complete capsule issued under the same live session remains valid and may be selected. The broker does not track a latest digest. Commit's atomic consume-first step bounds that session to one commit attempt; Abort closes it without commit. Replaying the same bytes after close fails at the session gate. Opening another session creates another bearer attempt, not a replay under the closed one.

### Exact conflict leakage

Conflict responses reveal the exact conflicting key, including a key newly written inside an authorized range. Epoch, horizon, retention-floor, and successful commit-timestamp values are likewise explicit. These outputs are represented in `FAster-v3`; they are not side channels and are not reduced to one bit.

---

## 7. Supersession map and safe appendix pointer

This addendum replaces or amends the following report material:

1. **Status and executive verdict:** replace every statement that the implemented seal is a prehash or needs unkeyed BLAKE3 collision resistance. The digest remains only an audit field.
2. **§1.1 C-CHANNEL:** replace physical wrong-cell language with bearer-session language; retain A11 solely as the condition for trusted launch identity.
3. **§1.2 Reality ledger:** replace with §5 of this addendum.
4. **§1.6 Seal construction:** replace in full with §§1.1–1.4.
5. **§1.7 Issuance relation:** index issuance by broker-minted session as in §2.1.
6. **§1.8/§1.10 and CE2.1:** add consume-first session lifecycle and the at-most-one-attempt qualification from §§2.3–2.4.
7. **Theorem 2.4 and §3.2:** replace with T1a-v3 and its one-branch EUF-CMA proof.
8. **Definitions 2.5–2.6, Theorem 2.7, and §3.3:** replace the whole-protocol transcript with §§2.5–2.8 and §3.4.
9. **Lemma 3.2, Proposition 3.3, and Remark 3.4:** replace with Lemma 3.2-v3. Proposition 3.3 is retired; direct MAC is no longer a future alternative.
10. **Lemma R discussion:** leave Lemma R unchanged and add R0/Proposition R0-OF from §2.10.
11. **Assumptions ledger:** apply §4; A3 is retired and A11 is narrowed.
12. **Variant verdict:** record the shipped hybrid `S_points ∪ AllRanges(C)` rather than calling the implementation full Variant B.
13. **Attack appendix:** apply §6.

The paper may now safely use the following pointer:

> **Formal-governance sentence.** “The companion report *The Capsule Transaction Theorem — a repaired proof for Aster v0.7*, as amended by its Seal-v3 and Session Protocol Addendum, governs the formal protocol claims. The report proves the shipped v3 direct-MAC/bearer-session protocol and the Aster-owned single-history fence; it does not erase the separately disclosed F1 two-plane limitation.”

With that qualification, re-referee finding **F9 is closed**: the governing formal artifact now contains the exact v3 frame, session-bearing outer-message injectivity, a pure EUF-CMA T1a reduction, the honest bearer-session scope, consume-first lifecycle, exact T1b leakage, R0, and the current implementation/variant ledger.

---

## 8. Implementation correspondence for artifact review

The proof obligations above correspond to the attached implementation as follows:

- `seal.rs`: v3-only algorithm dispatch; full-capsule direct MAC; audit digest outside the MAC; bound/unbound session domain tag; constant-time comparisons; stable bound and unbound test vectors.
- `canon.rs`: `aster-capsule-v3\0`; strict canonical document and ordered range-certificate encoding.
- `aster_brokerd.rs`: OS-random session minting; `session → (cid,epoch)` registry; broker-rebuilt contexts; consume-first Commit; Abort close; concurrent double-spend test; exact point-subset plus all-range-window fence input; module bundles as a separate source.
- round-2 referee report: F3 supplies the narrowed bearer-session T1a wording; F5 supplies the exact transcript leakage and module-input qualification; F6 supplies the hybrid Variant-B ledger; F9 identifies the stale proof artifact closed here; F11 supplies R0 and its separation from T2/Lemma R.
