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
| S3b | Certified prefix scan fim-a-fim (MvccStore live-scan → broker `hydrate_prefix` → verbo IPC) | 🔄 round paralelo | branch `v0.7-s3b` |
| S4 | Session/channel binding C-CHANNEL (seal v3 + tabela de sessões no brokerd) | 🔄 round paralelo | branch `v0.7-s4` (base: s3b) |
| S5 | Lease authority — épocas estritamente crescentes, nunca reusadas (F1) | ✅ | `c615bd9` (write_plane) |
| S6 | Commit fence transacional (1 tx Postgres: lease FOR UPDATE + tip + conflict scan + append) | ✅ | `c615bd9` + 13 testes `write_plane_it` |
| S7 | Variante B runtime honesto: consumption tracking + warm-hit no `1.0/get` (bench bug b, F7) | 🔄 round paralelo | branch `v0.7-s7` |
| S8 | Broker não se auto-esgota (lifetime budget removido — bench bug a) | ✅ | `b8fce68` |
| S9 | Commit verb IPC: capsule + consumo declarado + write set → fence. Fecha o loop cell→commit. Inclui época REAL vinda da lease authority no lugar da auto-declarada | ⬜ próxima (depois do merge do round) | |
| S10 | Bench do write path — EQ3/EQ4 MEDIDOS (gate do paper de sistemas) | ⬜ depende de S9 | `paper/sources/aster-bench.sh` como base |
| S11 | TLA+ do fence/GC/failover com TLC rodado de verdade (F8 — junta mais fraca) | 🔄 round paralelo | branch `v0.7-s11`, dir `tla/` |
| S12 | Paper "Authenticate the Reads, Not the Code" — Ian Lucas Beé, draft completo | 🔄 round paralelo | branch `v0.7-s12`, dir `paper/` |

## Round ultracode em andamento (16/07 ~06:40)

5 agentes paralelos em worktrees (`/tmp/aster-wt-*`), cada um commita na sua
branch. Orquestrador (sessão principal) faz merge na ordem **s7 → s4 (traz
s3b junto) → s11 → s12**, rodando a suíte completa entre merges, e depois um
workflow de review adversarial no diff acumulado.

**Se uma iteração de cron pegar este arquivo com o round em andamento:** NÃO
iniciar fatia nova. Checar `git branch --list 'v0.7-*'` + task notifications;
se o workflow ainda roda, só reportar status e encerrar o turno.

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
