# Preview QA fixtures

Corpus used by the per-phase QA matrix in
`.workspace-local/plans/preview-rich-quicklook.md`. Run `./generate.sh` to
materialise everything.

## Layout

```
pdf/
  sample-text.pdf        — multi-page text PDF, contains NEEDLE-A and NEEDLE-B
  sample-scan.pdf        — image-only (scanned) PDF, no text layer
  sample-encrypted.pdf   — sample-text.pdf encrypted with password "secret"
  sample-broken.pdf      — first 8 KiB of sample-text.pdf, intentionally truncated
  sample-huge.pdf        — sample-text.pdf tiled 50× (gitignored, large)

office/
  sample.{odt,docx,xlsx,pptx,ods,odp,doc,xls,ppt,rtf}
                         — derived from a single odt master via soffice
  sample-encrypted.docx  — password "secret"

image/
  sample.jpg             — 1280×800 gradient with a label
  sample.svg             — small declarative SVG
  sample-anim.gif        — 4-frame animation
  sample-large.png       — 8000×6000 plasma noise (gitignored, large)

code/
  sample.md              — markdown with code fences and bullets
  sample.rs              — Rust source

niche/
  sample.djvu            — single-page DjVu
  sample.epub            — EPUB built from sample.md
  sample.ipynb           — minimal Jupyter notebook (hand-written JSON)
  sample.sqlite          — three tables, 100 rows each
```

## NEEDLE-A and NEEDLE-B

Search and selection QA rows look for these literal strings. They
appear at known positions inside `pdf/sample-text.pdf`,
`office/sample.odt` (and its siblings), `code/sample.md`,
`code/sample.rs`, and `niche/sample.ipynb`.

NEEDLE-A is a single-page hit. NEEDLE-B spans a page break in PDF and
office fixtures so cross-page selection rows have a real target.

## Generator

`generate.sh` is the source of truth. It is idempotent: a stamp file
next to each output records the build version, and reruns are no-ops
when the stamp matches. Tool availability is checked before any work
is done, so a missing dependency aborts cleanly without leaving
partial fixtures behind.

Required tools: `pdflatex`, `qpdf`, `soffice`, `convert`
(ImageMagick), `sqlite3`, `djvumake` (or `cjb2`), `pandoc`, `gs`,
`python3`.

Large fixtures (`pdf/sample-huge.pdf`, `image/sample-large.png`) are
gitignored. QA rows that depend on them mark themselves SKIPPED if
the file is absent.
