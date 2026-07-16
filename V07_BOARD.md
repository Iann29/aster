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
| S9 | Commit verb IPC: capsule + consumo declarado + write set → fence. Fecha o loop cell→commit. Inclui época REAL vinda da lease authority no lugar da auto-declarada | ⬜ próxima (depois do merge do round) | |
| S10 | Bench do write path — EQ3/EQ4 MEDIDOS (gate do paper de sistemas) | ⬜ depende de S9 | `paper/sources/aster-bench.sh` como base |
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

**Round 3 EM ANDAMENTO (wf_fd6697e3-e31):** 3 agentes em worktrees —
A) store-postgres: C2+C3+C7+R0+R2+R5+R7 (único usando aster-pg-dev);
B) ipc/seal/broker: C4+C6+C8; C) paper: reconciliação completa
(C0/C1/C5/C9-C15 + U2/U3/U4). Merge: A,B,C (paths disjuntos) + re-stamp
dos counts do paper pós-merge (agentes A/B adicionam testes). Depois:
S9 → S10 → re-referee externo (GPT) com errata.

**Se uma iteração de cron pegar este arquivo com round em andamento:** NÃO
iniciar fatia nova; checar task notifications; se workflow ainda roda, só
reportar status e encerrar o turno.

## Operacional

- Branch de trabalho: `v0.7-write-path`. **NUNCA push sem ok explícito do Ian.**
- Suíte: `cargo test --workspace --exclude aster-v8cell` (v8cell à parte: `cargo test -p aster-v8cell`).
- Write plane (Postgres real): container `aster-pg-dev` na `:5433` —
  `ASTER_DB_URL=postgres://aster:aster@127.0.0.1:5433/aster cargo test -p aster-store-postgres --features postgres-it --test write_plane_it -- --test-threads=1`
- Worktrees compartilham cache: `export CARGO_TARGET_DIR=/home/ian/Documents/amage/aster/target`.
- Cron do loop: 15min, id `5ce96300`.
- Fontes do paper/teorema: `paper/sources/` (teorema completo em `ctt.txt`,
  referee F1-F9, related-work com regras de claim, bench notes com os únicos
  números reais: ~0,34ms/trap warm, cold ~390ms, ~2.900 traps/s serial).
