#!/usr/bin/env python3
"""Post-processes pandoc's LaTeX output (main.tex) into the preprint.

Pipeline: pandoc (see Makefile `regen`) -> this script -> pdflatex+bibtex.
Idempotent-ish: run exactly once on fresh pandoc output.
"""
import re, sys

src = open('main.tex').read()

def sub(old, new, count=1):
    global src
    assert old in src, "postprocess anchor missing: " + old[:80]
    src = src.replace(old, new, count)

# 1. real title block: drop the duplicated in-body heading + author line
src = re.sub(r"\\section\{Authenticate the Reads[^}]*\}\\label\{[^}]*\}\n\n\\textbf\{Ian Lucas Beé\}\n\n", "", src)

# 2. abstract environment
sub("\\section{Abstract}\\label{abstract}" if "\\section{Abstract}" in src else "\\subsection{Abstract}\\label{abstract}",
    "\\begin{abstract}")
src = re.sub(r"(security tax on both paths is a fraction of the storage cost it authenticates\.)\n\n\\begin\{center\}\\rule\{0.5\\linewidth\}\{0.5pt\}\\end\{center\}",
             r"\1\n\\end{abstract}", src)

# 3. promote heading levels (md title was consumed by pandoc's \section)
src = src.replace('\\subsubsection{', '\\XSUBX{').replace('\\subsection{', '\\section{').replace('\\XSUBX{', '\\subsection{')

# 4. horizontal rules out
src = src.replace("\\begin{center}\\rule{0.5\\linewidth}{0.5pt}\\end{center}\n\n", "")

# 5. numbered citation markers
for n, key in {1:'fdb2021',2:'fabricdocs',3:'basil2021',4:'transedge2023',5:'fides2020',6:'blake3-2020'}.items():
    src = src.replace('{[}%d{]}' % n, '\\cite{%s}' % key)

# 6. markdown References section out; BibTeX in
src = re.sub(r"\\section\{References\}\\label\{references\}.*?\\section\{Appendix pointer\}\\label\{appendix-pointer\}",
             r"\\section*{Appendix pointer}", src, flags=re.S)
sub("\\end{document}", "\\bibliographystyle{plain}\n\\bibliography{references}\n\n\\end{document}")

# 7. Times font (newtx; compile with the full texlive/texlive image)
sub("\\usepackage{lmodern}", "\\usepackage{newtxtext}\\usepackage{newtxmath}")

# 8. unicode -> pdflatex-safe (verbatim-aware)
verb_map = {'─':'-','│':'|','┌':'+','┐':'+','└':'+','┘':'+','┬':'+','┴':'+',
            '▼':'v','▲':'^','▶':'>','◀':'<','σ':'sigma','ℓ':'l','·':'.',
            '≤':'<=','≥':'>=','→':'->','—':'--','×':'x','−':'-','µ':'u',
            'Δ':'Delta','κ':'kappa','ρ':'rho','∥':'||','§':'sec.'}
prose_map = {'σ\\_s':'$\\sigma_s$','σ':'$\\sigma$','ℓ':'$\\ell$','κ':'$\\kappa$',
             '×':'$\\times$','≤':'$\\le$','≥':'$\\ge$','−':'$-$','·':'$\\cdot$',
             '→':'$\\rightarrow$','µ':'$\\mu$','—':'---','–':'--','∥':'$\\parallel$',
             'Δ':'$\\Delta$','ρ':'$\\rho$','≈':'$\\approx$','§':'\\S','⟨':'$\\langle$',
             '⟩':'$\\rangle$','∈':'$\\in$','⊆':'$\\subseteq$','≡':'$\\equiv$',
             '±':'$\\pm$','Σ':'$\\Sigma$','³':'$^{3}$'}
out, inverb = [], False
for line in src.split('\n'):
    if '\\begin{verbatim}' in line: inverb = True
    m = verb_map if inverb else prose_map
    for k, v in m.items(): line = line.replace(k, v)
    if '\\end{verbatim}' in line: inverb = False
    out.append(line)
src = '\n'.join(out)

# 9. incident + repo footnotes
sub("into exfiltrating IAM credentials through the instance metadata service",
    "into exfiltrating IAM credentials through the instance metadata service\\footnote{\\url{https://sonraisecurity.com/blog/sandboxed-to-compromised-new-research-exposes-credential-exfiltration-paths-in-aws-code-interpreters/}}")
sub("Unit 42 separately published network-isolation and metadata-service findings in April 2026.",
    "Unit 42 separately published network-isolation and metadata-service findings in April 2026.\\footnote{\\url{https://unit42.paloaltonetworks.com/bypass-of-aws-sandbox-network-isolation-mode/}}")
sub("the case that popularized the ``lethal trifecta'' framing (private data, untrusted input, and an exfiltration path in one agent).",
    "the case that popularized the ``lethal trifecta'' framing (private data, untrusted input, and an exfiltration path in one agent).\\footnote{\\url{https://generalanalysis.com/blog/supabase-mcp-blog}; \\url{https://simonwillison.net/2025/Jul/6/supabase-mcp-lethal-trifecta/}}")
sub("in the \\texttt{convex-backend} repository)",
    "in the \\texttt{convex-backend} repository\\footnote{\\url{https://github.com/get-convex/convex-backend}})")

# 10. in-text cites
sub("BLAKE3 is a streaming hash, so MACing", "BLAKE3 \\cite{blake3-2020} is a streaming hash, so MACing")
sub("Ryoan / Opaque / EnclaveDB & executor/operator hosts",
    "Ryoan / Opaque / EnclaveDB \\cite{ryoan2016,opaque2017,enclavedb2018} & executor/operator hosts")
sub("Cobra (OSDI '20) & the database", "Cobra \\cite{cobra2020} & the database")
sub("(Ryoan, Opaque, EnclaveDB, VC3) confines",
    "(Ryoan \\cite{ryoan2016}, Opaque \\cite{opaque2017}, EnclaveDB \\cite{enclavedb2018}, VC3 \\cite{vc3-2015}) confines")
sub("offline on behalf of trusted clients (OSDI '20)", "offline on behalf of trusted clients \\cite{cobra2020}")
sub("(CapTP and the ocap tradition) supply unforgeable references,",
    "(CapTP and the ocap tradition \\cite{captp2006}) supply unforgeable references,")
sub("(IFDB, Qapla) confine queries by policy;",
    "(IFDB \\cite{ifdb2013}, Qapla \\cite{qapla2017}) confine queries by policy;")
sub("Global Data Plane names durable signed append-only logs",
    "Global Data Plane \\cite{gdp2019} names durable signed append-only logs")
sub("are in the companion technical report:", "are in the companion technical report \\cite{ctt2026}:")


# 11. figure floats: swap the ASCII figure blocks for the generated images
#     (canonical PNGs live in ../figures/; Figure 3 stays typeset pseudocode
#     inside a float so it gets a real caption and number)
import os
sub("\\usepackage{longtable,booktabs,array}", "\\usepackage{longtable,booktabs,array}\n\\usepackage{graphicx}\n\\graphicspath{{../figures/}}")

def swap_figure(marker, replacement):
    global src
    pat = re.compile(r"\\begin\{verbatim\}\n" + marker + r".*?\\end\{verbatim\}", re.S)
    assert pat.search(src), "figure block missing: " + marker
    src = pat.sub(replacement.replace('\\', '\\\\'), src, count=1)

fig_tpl = """\\begin{figure}[t]
\\centering
\\includegraphics[width=%s\\linewidth]{%s}
\\caption{%s}
\\label{%s}
\\end{figure}"""

if os.path.exists('../figures/fig1-architecture.png'):
    swap_figure("Figure 1: architecture",
        fig_tpl % ('0.72', 'fig1-architecture.png',
                   'Architecture. The broker owns $\\kappa$, the storage handle, and the single-writer lease; the cell gets a UDS and nothing else.',
                   'fig:architecture'))
if os.path.exists('../figures/fig2-read-trap.png'):
    swap_figure("Figure 2: one read trap",
        fig_tpl % ('0.85', 'fig2-read-trap.png', 'One read trap.', 'fig:readtrap'))
if os.path.exists('../figures/fig4-conflict-windows.png'):
    swap_figure("Figure 4: the two conflict windows",
        fig_tpl % ('0.85', 'fig4-conflict-windows.png',
                   'The two conflict windows of a limited ascending scan.',
                   'fig:windows'))

# Figure 3: keep the pseudocode but give it a numbered float + caption
pat3 = re.compile(r"\\begin\{verbatim\}\nFigure 3: CommitFence pseudocode[^\n]*\n\n(.*?)\\end\{verbatim\}", re.S)
m = pat3.search(src)
assert m, "figure 3 block missing"
body = m.group(1)
src = pat3.sub(lambda _: ("\\begin{figure}[t]\n\\begin{verbatim}\n" + body +
                          "\\end{verbatim}\n\\caption{CommitFence pseudocode (technical report \\S1.8).}\n"
                          "\\label{fig:fence}\n\\end{figure}"), src, count=1)

open('main.tex','w').write(src)
print("postprocess ok, cites:", src.count('\\cite{'))
