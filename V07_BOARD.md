# Aster v0.7 — board de execução

Write path + provas reais + paper. Fonte de verdade entre iterações do loop
(o contexto da sessão pode resetar; este arquivo não). Atualizar a cada fatia
merged.

## Fatias

| # | Fatia | Status | Onde |
|---|---|---|---|
| S1 | Seal v2 direct-MAC + comparação constant-time (mata assumption A3/K-PREHASH) | ✅ | `22e872f` |
| S2 | Codec canônico W-CANON — decode estrito com rejeição adversarial | ✅ | `a467272` |
| S3 | Range certificates + conflict windows phantom-safe (R-RANGE, F2) | ✅ | `24c9665` |
| S3b | Certified prefix scan fim-a-fim (MvccStore live-scan → broker `hydrate_prefix` → verbo IPC; store-postgres com scan SQL real) | ✅ | `d96961c` |
| S4 | Session/channel binding C-CHANNEL (seal v3 `aster-blake3-keyed-v3` + tabela de sessões no brokerd, getrandom) | ✅ | `d55fd06` |
| S5 | Lease authority — épocas estritamente crescentes, nunca reusadas (F1) | ✅ | `c615bd9` (write_plane) |
| S6 | Commit fence transacional (1 tx Postgres: lease FOR UPDATE + tip + conflict scan + append) | ✅ | `c615bd9` + 13 testes `write_plane_it` |
| S7 | Variante B runtime honesto: consumption tracking (`V8ExecutionResult.consumed_reads`) + warm-hit no `1.0/get` (bench bug b, F7) | ✅ | `b40b4e5` |
| S8 | Broker não se auto-esgota (lifetime budget removido — bench bug a) | ✅ | `b8fce68` |
| S9 | Commit verb IPC + mutation syscalls — loop cell→broker→fence FECHADO. S9a: verbos Commit/Abort (Variante B: declared ⊆ capsule, B-SUBSET), CommitFence trait (MemoryFence twin + teste de paridade com WritePlane), sessão fecha no commit/abort, **época da lease authority no boot em modo Postgres** (obrigação C-CHANNEL #2 morta; ASTER_LEASE_EPOCH só stand-in de memory mode), consumed_reads no envelope. S9b: insert/patch/replace/delete no V8 com write set nascendo na célula, read-your-own-writes sem trap, existência upstream-parity, e2e JS→fence real (pg_v8_mutation_write_set_commits_and_interleaved_conflict_aborts) | ✅ | merge `6a00150` (85ab52f, bc59933, b82da6d) |
| S10 | Bench do write path — TUDO MEDIDO (merge `7ae878c`): B1 read pós-warm-hit (same-key ×200 = 1 trap, era 200; marginal warm ~5µs); EQ2 reseal linear/trap (0,030ms + 0,66µs/entry; quadrático cumulativo confirmado 366,9ms medido vs 361 previsto @ n=1000); B3 fence isolado (blind 3,51ms p50 / 280,5 commits/s; validação de pontos FLAT em p=1..200; windows 4,6→6,6ms w=1..50; abort 1,75ms; 7 SQL round-trips/fence); B4 e2e write path 6,49ms p50 (exec 2,44 + commit 4,03) / 152,9 tx/s serial — **perna de commit == fence isolado: aparato de capability não adiciona nada mensurável**. Harness `bench/run-v07.sh` + raw logs versionados; reprodutibilidade provada de clone limpo. Paper: [PENDING] substituídos, 0,34ms virou baseline v0.6 rotulado, contagens re-stamp 18,4k LOC / 254 testes | ✅ | merge `7ae878c` (8cb09c7, c51ca50, 4c8dd7c) |
| S11 | TLA+ do fence/GC/failover com TLC rodado de verdade (F8 — junta mais fraca): positivo 14,8M estados / 6 invariantes OK; negativos F1-reuse→I3 e no-pin→I2 violam como previsto | ✅ | `656bc91` + addendum `885ef9b` |
| S12 | Paper "Authenticate the Reads, Not the Code" — Ian Lucas Beé, draft 9.588 palavras + CLAIMS.md | ✅ draft (reconciliação pós-merge no review) | `cb8c630` |

## Round 1 ultracode — CONCLUÍDO (16/07 ~07:40)

5 agentes paralelos → 4 merges limpos (`5614d2b`..`e609625`) + 2 achados
corrigidos na integração: flake SIGSEGV de isolates V8 em threads paralelas
do harness (guard serial, `42950cc`) e wedge de liveness do
`advance_retention` com watermark acima do tip (clamp + regressão + guard no
modelo TLA e TLC re-rodado, `885ef9b`). Suítes: workspace 0 falhas, v8cell
8/8 rodadas verdes, postgres-it 21+11 verdes. Worktrees e branches de fatia
removidos.

**Round 2 CONCLUÍDO — review adversarial (wf_8f0f1dfc-538):** 29 achados
brutos de 5 lentes → 24 verificados por 3 céticos cada → **16 confirmados**
(2 por mutation testing real). Código: C2 fence sem
idle_in_transaction_session_timeout/keepalive = wedge indefinido; C3
scan/read_point sem check do retention floor do Convex
(min_document_snapshot_ts) = evidência selada FALSA possível; C4 brokerd
morre com 1 conexão hostil (EPIPE/FrameTooLarge) e perde a session table.
Testes: C6 session-no-MAC sem teste semântico (mutação passa!); C7 metade
fence do pin Lemma R sem teste; C8 hydrate-at-capsule.ts sem pin (mutação
passa!). Paper: cluster seal v2→v3 (C1/C9), cluster C-CHANNEL overclaim
(C0/C5/C10/C11 — sessão é broker-minted mas cid/epoch self-asserted no
mint; epoch do read path vem de env), números stale (C12/C14/C15), CLAIMS.md
dessincronizado (C13). 8 sem verificação (limite de sessão): R0/R2/R5/R7
plausíveis → verificar-e-corrigir no round 3; R1/R3/R6 duplicatas de
confirmados; R4 = escopo S9 aceito. Overflow U2/U3/U4 = paper minors.

**Round 3 CONCLUÍDO (16/07 ~09:30, merges `b10d415`+`cca2ebe`+`311564a`+
re-stamp `558eaa5`):** TODOS os 16 confirmados aplicados e os 4 não-verificados
(R0/R2/R5/R7) se provaram REAIS e foram corrigidos — zero refutados.
Código: write plane ganhou `idle_in_transaction_session_timeout` (knob no
config) + TCP keepalives (C2), guard do retention floor upstream
(`min_document_snapshot_ts` → `StoreError::Stale`, checado PÓS-query —
floor monotônico — pra não correr com vacuum concorrente) (C3), conversões
i64 checadas (R0), prefixo normalizado lowercase no certificate (R2);
brokerd contém erro por-conexão (EPIPE/FrameTooLarge → log/erro estruturado
`response_too_large`, broker sobrevive) (C4). Testes novos matam as mutações
que o review provou passarem: session-bytes-no-MAC (C6), scan-at-capsule.ts
(C8), pin Lemma R bidirecional (C7), tombstone-write-vs-read (R5), wedge de
idle resolvendo failover (C2), Send/Sync real (R7). Paper: seal v1→v2→v3
história honesta, C-CHANNEL rebaixado pra mecanismo real + obrigações
nomeadas, contagens re-medidas e re-carimbadas pós-merge (14,4k LOC / 217
testes / 14 provas do fence), CLAIMS.md reconciliado + linha do addendum
TLC. Suítes no HEAD: workspace 23 suítes ok, v8cell ok, postgres-it
33+25+14 ok.

**Round 4 CONCLUÍDO (16/07 ~13h, commit `ac3ac50`) — review final do delta
S9+S10, aplicado INLINE (freio de custo do Ian — sem mais tribunal de
agentes):** 17 achados de 3 lentes + 16 vereditos de céticos herdados do
workflow parado (wf_af32f1dc). Código: brokerd timeouts anti-wedge no accept
loop (B2), Commit fecha a sessão mesmo em rejeição do gate (C6),
`duplicate_write_key` estruturado (C4b), log alto de stale epoch (B1);
v8cell write gate em query — paridade upstream (C3), held-absence warm no
legacy read (C7), `traps` = broker round trips com budget de pump separado
(C5), `_raw` malformado erra em vez de base vazia (C2 sentinela); write
plane recusa leitura abaixo do retention floor — Stale, nunca ausência
falsa (B4). Zombie-broker REFUTADO 2-1 pelos céticos (fail-closed por
design). Honestidade dos benchs A0–A6: drift por métrica (+26% nas caudas
small-N declarado), window sweep rotulado upper bound (população cresce
1,0k→1,6k), convenções de mediana, clone overhead do reseal, warmup por
fase, GUCs capturados em machine.log + run-v07.sh. Seams B3 (snapshot
boot-pinned → retry = relançar broker) e C1 (aliasing IDv6/wire form,
confinado a callers nativos) documentados no paper §8 + docs de código.
+6 testes sentinela. Re-stamp: **18.801 LOC / 260 testes / 16 provas do
fence**. Suítes: workspace 23 ok, v8cell 18, postgres-it 33+25+16, ipc
gated 31.

**v0.7 COMPLETO — 13/13 fatias + 4 rounds de review adversarial.** Próximo
passo é DO IAN: colar `paper/rereferee-v07-prompt.md` no GPT Pro (re-referee
externo da implementação + paper — o xeque-mate combinado). Depois do
veredito: aplicar achados → LaTeX/BibTeX (venue/ano das citações ainda por
confirmar, ver CLAIMS) → arXiv. Obrigação C-CHANNEL #1 (launch token no
mint) segue future work documentado.

**Cron:** o v0.7 está completo e o próximo passo é manual (Ian → GPT Pro).
Iterações de cron: NADA a construir — reportar status em uma linha e
encerrar o turno imediatamente. O cron do loop (15min, id `5ce96300`) já
pode ser desligado.

## Operacional

- Branch de trabalho: `v0.7-write-path`. **NUNCA push sem ok explícito do Ian.**
- Suíte: `cargo test --workspace --exclude aster-v8cell` (v8cell à parte: `cargo test -p aster-v8cell`).
- Write plane (Postgres real): container `aster-pg-dev` na `:5433` —
  `ASTER_DB_URL=postgres://aster:aster@127.0.0.1:5433/aster cargo test -p aster-store-postgres --features postgres-it --test write_plane_it -- --test-threads=1`
- Lane gated do brokerd (fence e2e via handle_request):
  `ASTER_DB_URL=postgres://aster:aster@127.0.0.1:5433/aster cargo test -p aster-ipc --features postgres-it -- --test-threads=1`
- Worktrees compartilham cache: `export CARGO_TARGET_DIR=/home/ian/Documents/amage/aster/target`.
- Cron do loop: 15min, id `5ce96300`.
- Fontes do paper/teorema: `paper/sources/` (teorema completo em `ctt.txt`,
  referee F1-F9, related-work com regras de claim, bench notes com os únicos
  números reais: ~0,34ms/trap warm, cold ~390ms, ~2.900 traps/s serial).
