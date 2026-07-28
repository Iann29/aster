# Builds the preprint with the same pdflatex toolchain arXiv runs.
# Requires Docker; image: pandoc/latex (TeX Live + pandoc).

IMAGE := pandoc/latex:latest
RUN   := docker run --rm -v $(CURDIR):/data -w /data \
         --user $(shell id -u):$(shell id -g) --entrypoint sh $(IMAGE)

pdf:
	$(RUN) -c "export HOME=/tmp && \
	  pdflatex -interaction=nonstopmode main.tex && \
	  bibtex main && \
	  pdflatex -interaction=nonstopmode main.tex && \
	  pdflatex -interaction=nonstopmode main.tex"

# Regenerate main.tex from the markdown source (paper/authenticate-the-reads.md).
# NOTE: overwrites hand-applied post-processing (cites, footnotes, unicode
# normalization) — only use to restart the conversion, then re-apply.
clean:
	rm -f main.aux main.bbl main.blg main.log main.out main.toc final.log bibtex.log

.PHONY: pdf clean
