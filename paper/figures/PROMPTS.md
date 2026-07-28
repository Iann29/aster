# Prompts de geração das figuras do paper (GPT image)

Regras de uso: gerar em alta resolução, paisagem, PNG. Conferir cada rótulo
letra a letra (regenerar em caso de typo). Fig 3 permanece pseudocódigo
tipografado; só 1, 2 e 4 viram imagem. Ao chegar os PNGs, salvar como
`paper/figures/fig1-architecture.png`, `fig2-read-trap.png`,
`fig4-conflict-windows.png` — o postprocess troca os blocos verbatim por
`\includegraphics` quando os arquivos existem.

## Bloco de estilo (prefixo comum de todos os prompts)

> Flat 2D vector-style technical diagram for an academic systems paper
> (USENIX/ACM aesthetic). Pure white background. Thin dark-gray strokes
> (uniform 1.5px weight), sharp or barely-rounded rectangles, simple arrows
> with small solid heads. No gradients, no shadows, no 3D, no icons, no
> clip-art, no decorative elements, no texture. Exactly one muted accent
> color: desaturated steel blue (#3B6491), used only where specified; all
> other elements are black, dark gray, or white. Labels in a clean
> sans-serif (Helvetica-like), small and consistent; code-like identifiers
> in monospace. Generous whitespace, high resolution, landscape 3:2.
> Render all label text EXACTLY as specified, with no extra words.

## Figura 1 — arquitetura

> [bloco de estilo]
>
> Content: a system architecture diagram with three components in a
> vertical flow. At the top, a small plain arrow labeled "HTTP request"
> points down into a rectangle labeled "aster-broker" with the sublabel
> "long-lived, per deployment". Attached to the right of that rectangle, a
> borderless annotation in smaller gray text reads: "owns: Postgres handle,
> MAC key κ, single-writer lease (epoch e)". Below it, a second rectangle
> labeled "V8 cell" with the sublabel "one-shot, per invocation". Between
> the two rectangles, two parallel vertical arrows: one pointing down
> labeled "spawn, ctx = (tenant, deploy, cid, e, s)", one pointing up-down
> (double-headed) labeled "sealed capsules / commit verdicts" — draw the
> sealed-capsules arrow in the accent blue. Next to the V8 cell rectangle,
> a borderless gray annotation reads: "UDS only. No DB creds, no network,
> no fs." To the right of the broker, a cylinder shape labeled "Postgres"
> connected to the broker by a plain horizontal line. Nothing else.

## Figura 2 — um read trap (diagrama de sequência)

> [bloco de estilo]
>
> Content: a message-sequence diagram with three vertical lifelines,
> labeled at the top, left to right: "cell" (monospace), "broker"
> (monospace), "Postgres". Time flows downward; lifelines are thin dashed
> vertical lines. Messages, top to bottom:
> 1. On the cell lifeline, a small left-aligned note: "JS executes;
>    db.get(id) suspends on a trap".
> 2. Solid arrow from cell to broker labeled "(ctx, sealed Cap_i, key)" —
>    accent blue.
> 3. On the broker lifeline, three stacked small notes: "verify seal",
>    "check read policy", then
> 4. Solid arrow from broker to Postgres labeled "point read at snapshot
>    σ_s", and a return dashed arrow labeled "(version, doc/absence)".
> 5. On the broker lifeline, one note: "merge into Cap_i+1, reseal
>    (canonical encode + keyed MAC)".
> 6. Solid arrow from broker back to cell labeled "sealed Cap_i+1" —
>    accent blue.
> 7. On the cell lifeline, a final note: "promise resolves; JS resumes".
> Nothing else.

## Figura 4 — as duas janelas de conflito

> [bloco de estilo]
>
> Content: two horizontal interval diagrams stacked vertically, each an
> interval drawn as a long thin horizontal bracket with square endpoints.
> Top diagram, titled on the left "Exhausted (m < ℓ)": the ENTIRE interval
> is filled with a light accent-blue tint; three small tick marks near the
> left labeled "k1", "k2", "k3" in monospace; a gray annotation under the
> empty right part reads "gap certified empty"; below the whole bar, one
> caption line: "a later insert anywhere in I conflicts".
> Bottom diagram, titled on the left "Boundary (m = ℓ)": only the LEFT
> PORTION of the interval, from the start through a tick labeled "km", is
> filled with the light accent-blue tint; ticks labeled "k1", "k2", "…",
> "km"; the right portion is unfilled with a gray annotation "unobserved";
> below the bar, one caption line: "an insert ≤ km conflicts; an insert
> after km does not".
> Nothing else.
