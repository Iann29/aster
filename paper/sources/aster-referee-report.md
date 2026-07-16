# Referee report — The Capsule Transaction Theorem (Aster v0.7)

**Documento:** `The_Capsule_Transaction_Theorem_Aster_v0.7.pdf` (27 pp., julho/2026)
**SHA-256:** `73053ae57bdc5ef5dc59005c98c743da32c48099e7c4b7910204eafb9885608e` ✓ (verificado)
**Referee:** revisão adversarial completa (modelo, reduções, indução de serialização, os 13 ataques, ledger de assumptions) + checagem de fidelidade modelo↔código contra o repo `Iann29/aster`.

---

## VEREDITO: APROVADO COM RESSALVAS — nenhuma falha fatal encontrada

O documento sobrevive à revisão adversarial. As três contramedidas centrais (rollback,
prehash, conflict-bit) são **corretas e necessárias** — não são frouxidão, são o preço
real do protocolo stateless. As reduções são padrão e bem estruturadas; a indução de
serialização é o argumento clássico de backward validation feito com cuidado; os
escopos (T1a ≠ "latest", T1b com leakage de controle nomeado, Q-SCOPE) são honestos.
O documento serve como especificação do v0.7.

## 1. Fidelidade modelo↔código (verificada por mim contra `seal.rs`)

| Claim do documento | Código real | Veredito |
|---|---|---|
| Layout do MAC: `alg ∥ lp(cid) ∥ le64(e) ∥ lp(d) ∥ lp(tenant) ∥ lp(deployment) ∥ le64(s)` | `capsule_mac()` — idêntico, campo a campo | ✓ fiel |
| Seal implementado é prehash (MAC cobre `d = H(E(Cap))`, não os bytes) | `put_bytes(&mut hasher, digest)` | ✓ — **A3 (collision resistance) é genuinamente necessária** |
| Comparação de tag não é constant-time | `if mac != self.seal.mac` (`!=` comum em `[u8;32]`) | ✓ obrigação real |
| Reality ledger (epoch auto-declarado, capsule só com point entries, snapshot pinado no boot) | confere com o dossiê de 16/07 | ✓ ledger honesto |

Nota: no código atual, `verify()` só confere `cell_id`+`lease_epoch` contra o contexto;
tenant/deployment/ts vêm do próprio capsule (pinning no broker). O modelo v0.7 exige
igualdade header↔contexto completa — coberto pelo reality ledger como "proposed", ok.

## 2. Spot-checks das provas (todos passaram)

- **Counterexample 2.1 (rollback)** — correto e inevitável sem estado anti-rollback.
  O reescopo de T1a ("issued", não "latest") é o mínimo honesto.
- **Counterexample 2.2 (prehash)** — pigeonhole trivialmente correto; o countermodel
  (keyed-PRF seguro + unkeyed constante) é válido como separação formal de assumptions.
- **Counterexample 2.3 (conflict bit)** — correto; tratar como leakage explícito no
  ideal functionality é a prática padrão (leakage profile).
- **Redução T1a (duas vias: forgery fresco ∨ colisão via Lemas 3.1/3.2)** — sólida;
  a estrutura multi-contexto cobre conluio de graça.
- **Lema 3.7 (range stability)** — verifiquei os dois casos; o exhaustion gap está
  certo, e a janela `I≤km` no caso boundary é o análogo correto do next-key locking.
- **Counterexample 3.9 (write-skew sem fence)** — clássico e correto; a necessidade
  do commit fence é real, não excesso de zelo.
- **Lema 3.11 (epoch fencing)** — sólido, **condicionado a F1 abaixo**.
- **Indução T2** — backward validation clássica; o ponto de linearização (append
  atômico) fecha o real-time para mutations.
- **Testemunha T3** — argumento existencial válido; a franqueza ("external-trace
  theorem, não integridade de computação") é a postura certa.
- **Simulador T1b** — hybrid PRF→random function padrão; a mesa lazy compartilhada
  entre contextos em conluio está certa (espelha a chave única real).

Tentativas de quebra que NÃO furaram: cross-variant confusion (sem poder novo);
range certs sobrepostos inconsistentes (impossível via issuance honesta + T1a);
snapshot velho + h distante (só liveness, coberto pelo retry do Lemma R);
transplant via coalizão (modelado corretamente como exercício de capability).

## 3. Achados do referee (nenhum fatal)

| # | Severidade | Achado |
|---|---|---|
| F1 | **média — corrigir na spec** | A7 precisa declarar explicitamente que épocas são **estritamente crescentes e nunca reusadas**. Num failback A→B→A com reuso de época, a análise exaustiva do Lema 3.11 quebra. Uma linha no ledger resolve. |
| F2 | **média — API/paper** | Caso boundary (m = ℓ): footgun de completude pro app. Quem pede `limit ℓ`, recebe exatamente ℓ e trata como "conjunto completo" NÃO é protegido contra insert após kℓ (corretamente — a observação é "first-ℓ"). Regra pro app: asserção de completude exige certificado **Exhausted** (pedir ℓ+1 ou checar o stop bit). Documentar na API e no paper. |
| F3 | média — deployment | C1 (read brokers escalados): "read time" da política é por-broker; revogação de read policy tem skew de propagação. Bound e documentar a latência de revogação. |
| F4 | recomendação | Implementar o **seal v3 direct-MAC** no v0.7 em vez de carregar A3: BLAKE3 é streaming, custo ~zero, remove uma assumption inteira do paper. A "future repair" do documento deve ser o default. |
| F5 | produto | A12 exige células sem canal alternativo: os containers precisam de **egress bloqueado** (rede negada), senão exfiltração de dado autorizado é trivial. Não quebra teorema; quebra o pitch. |
| F6 | paper | Headline de consistência: "strict serializability **para mutations**; snapshot reads serializáveis possivelmente stale" — espelho do caveat de snapshot-read do próprio FDB. Nunca vender o headline sem o caveat. |
| F7 | implementação (liga no bench) | O runtime honesto da Variante B precisa rastrear **consumo** (cache hits), não só traps. Hoje `Convex.asyncSyscall` sempre trapa e o warm-check só existe no toy path (bug (b) do bench de 16/07) — o auto-populate de S aterrissa exatamente aí. |
| F8 | o próprio doc nomeia | A6 (conflict projection) é a junta mais fraca: writes em index-space devem ser derivados pelo **committer confiável**, nunca pela célula. Merece model checking (TLA+) do fence + GC pin + failover — o documento pede verificação independente e eu concordo. |

## 4. Concordância com o veredito de variantes

**Variante B, confirmada.** A vantagem bizantina da Variante A é ilusória sob rollback
stateless (o doc prova isso via CE 2.1) e o custo dela (false conflicts sob prewarm)
é real. `S` = "declared dependency set" é o nome certo. F7 é o pré-requisito de
implementação.

## 5. O que isto significa

- A prova é **condicional ao ledger** — normal e honesto; é uma especificação, não
  substituto de verificação de código (o doc diz isso na capa).
- O `CommitFence` mapeia naturalmente numa **única transação Postgres** (lease row
  `FOR UPDATE` + tip read + conflict scan + policy version + append) — o mesmo padrão
  do lease do próprio Convex (`crates/postgres/src/lib.rs:1738-1799`). O v0.7 é
  construível sem inventar maquinaria nova.
- A tese que sobrevive (§7 do doc) está pronta pro paper: *"Aster makes every
  submitted OCC observation provenance-authentic without trusting or re-executing
  the application executor; strict serializability then follows from classical
  backward validation, while any omitted dependency is demoted to an authorized
  blind application write rather than a cross-authority isolation failure."*

## 6. Próximos passos (ordem recomendada)

1. **Blueprint v0.7** direto do doc + este report: seal v3 direct-MAC (F4), channel
   binding (C-CHANNEL), época da autoridade de lease (não auto-declarada), commit
   fence em Postgres (transação única), range certificates no wire, decoder canônico
   (W-CANON), Variante B com consumption tracking (F7) — e de carona os 2 bugs do
   bench (MAX_CONNECTIONS como orçamento de vida; warm-check no path real).
2. **Model checking TLA+** do fence + GC pin + epoch failover (F8) — o doc pede,
   eu subscrevo; é a única parte que cryptografia nenhuma salva.
3. **Paper skeleton**: tese pronta, related work 5/5 pronto (aster-related-work.md),
   eval semeada com o bench real de 16/07. Preprint no arXiv após o v0.7 provar a
   spec em código.

## 7. Confirmação independente (Opus 4.8) + F9

Segunda passada adversarial independente (Opus 4.8, contexto separado, leitura das 27
páginas) **CONVERGE** com este report: mesmo veredito (condicionalmente sólido, nenhuma
falha fatal) e re-derivou de forma independente F4 (seal v3 direct-MAC), F5/A12 (side
channels — a Opus alarga pra timing/cache, não só egress), F6 (headline stale-reads),
F7 (consumption tracking) e F8 (A6 = junta mais fraca). F1 (reuso de época no failback
A→B→A) e F2 (regra ℓ+1 pra exhaustion) são achados em que **este report é mais afiado** —
a passada Opus não os isolou. Convergência de dois referees independentes = sinal forte.

**F9 (passada Opus) — decisão arquitetural do committer. Severidade ALTA pro blueprint.**
A §5.2 diz que o CommitFence mapeia numa transação Postgres usando o lease — verdade, mas
isso ESCONDE a decisão de fundo. Hoje o `convex-backend` REAL segura o lease single-writer
e commita pelo próprio caminho. Fazer mutations no Aster (v0.7) exige que o broker do Aster
segure esse lease — e não pode haver DOIS writers (A7). Logo o blueprint TEM que escolher:
(a) o broker do Aster **vira** o committer daquele deployment (o convex-backend deixa de
commitar direto — mudança profunda no caminho de escrita), ou (b) o Aster valida e
**encaminha** o write autenticado pro committer do convex-backend (Aster = gate na frente
do commit). O read plane roda ao lado sem tocar nisso (Corolário C1); o write plane não
pode ser sidecar. Essa é a maior pergunta em aberto do v0.7 — e é justamente a "maquinaria
central", não "sem maquinaria nova".
