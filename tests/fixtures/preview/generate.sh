#!/usr/bin/env bash
# Idempotent generator for preview QA fixtures.
#
# Layout: see ./README.md (and the plan at
# .workspace-local/plans/preview-rich-quicklook.md §0.1).
#
# Reruns produce size-stable output. Tools required are checked up
# front; a missing tool aborts before any partial work hits disk.
# Large fixtures live under git-ignored paths (see ./.gitignore).

set -Eeuo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PDF="$HERE/pdf"
OFFICE="$HERE/office"
IMAGE="$HERE/image"
CODE="$HERE/code"
NICHE="$HERE/niche"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

log()  { printf '[fixtures] %s\n' "$*" >&2; }
fail() { printf '[fixtures] ERROR: %s\n' "$*" >&2; exit 1; }

require() {
    local tool="$1" pkg="${2:-}"
    if ! command -v "$tool" >/dev/null 2>&1; then
        if [[ -n "$pkg" ]]; then
            fail "missing tool: $tool (install: $pkg)"
        else
            fail "missing tool: $tool"
        fi
    fi
}

# ---- preflight -------------------------------------------------------------
require pdflatex  "texlive-latex-base"
require qpdf      "qpdf"
require soffice   "libreoffice-core"
require convert   "imagemagick"
require sqlite3   "sqlite3"
require djvumake  "djvulibre-bin"
require pandoc    "pandoc"
require gs        "ghostscript"
require python3

# Fingerprint helper: deterministic file from a tag string. Used to
# stabilise file mtimes for idempotency rather than rebuilding on every
# run.
needs_rebuild() {
    local out="$1" stamp="$2"
    [[ ! -f "$out" ]] || [[ ! -f "$out.stamp" ]] || [[ "$(cat "$out.stamp")" != "$stamp" ]]
}
mark_built() { printf '%s' "$2" > "$1.stamp"; }

# ---- pdf -------------------------------------------------------------------

build_sample_text_pdf() {
    local out="$PDF/sample-text.pdf"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out"
    local tex="$WORK/sample-text.tex"
    cat >"$tex" <<'TEX'
\documentclass{article}
\usepackage[utf8]{inputenc}
\usepackage{lipsum}
\title{Lixun Preview Sample}
\author{Lixun QA}
\date{}
\begin{document}
\maketitle
\section{Introduction}
\lipsum[1-2]
\section{Selection target}
The phrase NEEDLE-A occurs here.
\lipsum[3]
\newpage
\section{Second page}
\lipsum[4-5]
The phrase NEEDLE-B spans this page.
\newpage
\section{Third page}
\lipsum[6-7]
\end{document}
TEX
    ( cd "$WORK" && pdflatex -interaction=batchmode -halt-on-error sample-text.tex >/dev/null )
    cp "$WORK/sample-text.pdf" "$out"
    mark_built "$out" "$stamp"
}

build_sample_scan_pdf() {
    local out="$PDF/sample-scan.pdf"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out"
    # Rasterise sample-text.pdf at 150 DPI then wrap pages as JPEG-only PDF.
    gs -dQUIET -dNOPAUSE -dBATCH -sDEVICE=jpeg -r150 \
        -sOutputFile="$WORK/scan-%03d.jpg" "$PDF/sample-text.pdf" >/dev/null
    convert "$WORK"/scan-*.jpg "$out"
    mark_built "$out" "$stamp"
}

build_sample_encrypted_pdf() {
    local out="$PDF/sample-encrypted.pdf"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out"
    qpdf --encrypt secret secret 256 -- \
        "$PDF/sample-text.pdf" "$out"
    mark_built "$out" "$stamp"
}

build_sample_broken_pdf() {
    local out="$PDF/sample-broken.pdf"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out"
    head -c 8192 "$PDF/sample-text.pdf" > "$out"
    mark_built "$out" "$stamp"
}

build_sample_huge_pdf() {
    # Git-ignored, large.
    local out="$PDF/sample-huge.pdf"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out (large, gitignored)"
    # Replicate sample-text.pdf 50 times via qpdf --pages.
    local args=()
    for _ in $(seq 1 50); do args+=("$PDF/sample-text.pdf"); done
    qpdf --empty --pages "${args[@]}" -- "$out"
    mark_built "$out" "$stamp"
}

# ---- office ----------------------------------------------------------------

soffice_convert() {
    local infile="$1" out_ext="$2" outfile="$3"
    soffice --headless --convert-to "$out_ext" \
        --outdir "$WORK" "$infile" >/dev/null
    local base
    base="$(basename "$infile")"
    local produced="$WORK/${base%.*}.$out_ext"
    [[ -f "$produced" ]] || fail "soffice produced no $produced from $infile"
    cp "$produced" "$outfile"
}

build_office_text_master() {
    local master="$OFFICE/sample.odt"
    local stamp="v1"
    if ! needs_rebuild "$master" "$stamp"; then return; fi
    log "building $master and word-processor family"
    local md="$WORK/sample.md"
    cat >"$md" <<'MD'
# Lixun Office Sample

This is a multi-page office sample used by preview QA.

The phrase NEEDLE-A occurs in the first paragraph.

\newpage

Second page content. The phrase NEEDLE-B spans this page.

\newpage

Third page with a small table:

| key | value |
|-----|-------|
| a   | 1     |
| b   | 2     |
MD
    pandoc -o "$master" "$md"
    mark_built "$master" "$stamp"

    local fmt
    for fmt in docx doc rtf; do
        local sibling="$OFFICE/sample.$fmt"
        local sibling_stamp="from-odt-v1"
        if ! needs_rebuild "$sibling" "$sibling_stamp"; then continue; fi
        log "building $sibling"
        soffice_convert "$master" "$fmt" "$sibling"
        mark_built "$sibling" "$sibling_stamp"
    done
}

build_office_sheet_master() {
    local master="$OFFICE/sample.ods"
    local stamp="v1"
    if ! needs_rebuild "$master" "$stamp"; then return; fi
    log "building $master and spreadsheet family"
    local csv="$WORK/sample.csv"
    cat >"$csv" <<'CSV'
key,value,note
NEEDLE-A,1,first row contains the search target
b,2,second row
c,3,third row
d,4,fourth row
e,5,fifth row
CSV
    soffice_convert "$csv" "ods" "$master"
    mark_built "$master" "$stamp"

    local fmt
    for fmt in xlsx xls; do
        local sibling="$OFFICE/sample.$fmt"
        local sibling_stamp="from-ods-v1"
        if ! needs_rebuild "$sibling" "$sibling_stamp"; then continue; fi
        log "building $sibling"
        soffice_convert "$master" "$fmt" "$sibling"
        mark_built "$sibling" "$sibling_stamp"
    done
}

build_office_slides_master() {
    local master="$OFFICE/sample.odp"
    local stamp="v1"
    if ! needs_rebuild "$master" "$stamp"; then return; fi
    log "building $master and presentation family"
    local fodp="$WORK/sample.fodp"
    python3 - "$fodp" <<'PY'
import sys, pathlib
out = pathlib.Path(sys.argv[1])
out.write_text("""<?xml version="1.0" encoding="UTF-8"?>
<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
                 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
                 xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"
                 xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0"
                 office:version="1.2" office:mimetype="application/vnd.oasis.opendocument.presentation">
<office:body><office:presentation>
  <draw:page draw:name="p1"><draw:frame><draw:text-box>
    <text:p>Lixun slide one — NEEDLE-A</text:p>
  </draw:text-box></draw:frame></draw:page>
  <draw:page draw:name="p2"><draw:frame><draw:text-box>
    <text:p>Lixun slide two — NEEDLE-B</text:p>
  </draw:text-box></draw:frame></draw:page>
</office:presentation></office:body></office:document>
""", encoding="utf-8")
PY
    soffice_convert "$fodp" "odp" "$master"
    mark_built "$master" "$stamp"

    local fmt
    for fmt in pptx ppt; do
        local sibling="$OFFICE/sample.$fmt"
        local sibling_stamp="from-odp-v1"
        if ! needs_rebuild "$sibling" "$sibling_stamp"; then continue; fi
        log "building $sibling"
        soffice_convert "$master" "$fmt" "$sibling"
        mark_built "$sibling" "$sibling_stamp"
    done
}

build_office_encrypted() {
    local out="$OFFICE/sample-encrypted.docx"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out"
    # soffice supports --convert-to with a password filter via a
    # dedicated FilterOptions string; --infilter is for the input side.
    # We use the macro-free variant: convert to docx first, then
    # re-save with password via a python uno script — kept simple by
    # delegating to soffice's filter syntax.
    rm -f "$WORK/sample.docx"
    soffice --headless \
        --convert-to "docx:MS Word 2007 XML:Password=secret" \
        --outdir "$WORK" "$OFFICE/sample.odt" >/dev/null
    cp "$WORK/sample.docx" "$out"
    mark_built "$out" "$stamp"
}

# ---- image -----------------------------------------------------------------

build_sample_jpg() {
    local out="$IMAGE/sample.jpg"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out"
    convert -size 1280x800 gradient:navy-white \
        -font DejaVu-Sans -pointsize 48 -fill white -gravity center \
        -annotate 0 "Lixun JPG Sample" "$out"
    mark_built "$out" "$stamp"
}

build_sample_png_large() {
    local out="$IMAGE/sample-large.png"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out (large, gitignored)"
    convert -size 8000x6000 plasma: "$out"
    mark_built "$out" "$stamp"
}

build_sample_anim_gif() {
    local out="$IMAGE/sample-anim.gif"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out"
    local i
    for i in 0 1 2 3; do
        convert -size 320x240 "xc:hsl(${i}0,80%,60%)" \
            -font DejaVu-Sans -pointsize 36 -gravity center \
            -annotate 0 "frame $i" "$WORK/anim-$i.png"
    done
    convert -delay 25 -loop 0 "$WORK"/anim-*.png "$out"
    mark_built "$out" "$stamp"
}

build_sample_svg() {
    local out="$IMAGE/sample.svg"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out"
    cat >"$out" <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" width="400" height="300" viewBox="0 0 400 300">
  <rect width="100%" height="100%" fill="#1a1a2e"/>
  <circle cx="200" cy="150" r="80" fill="#e94560"/>
  <text x="200" y="160" font-family="sans-serif" font-size="24" fill="white" text-anchor="middle">Lixun SVG</text>
</svg>
SVG
    mark_built "$out" "$stamp"
}

# ---- code ------------------------------------------------------------------

build_sample_md() {
    local out="$CODE/sample.md"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out"
    cat >"$out" <<'MD'
# Sample markdown

Paragraph with **bold** and `code`.

```rust
fn main() {
    println!("hello");
}
```

- bullet one
- bullet two

NEEDLE-A appears here for search tests.
MD
    mark_built "$out" "$stamp"
}

build_sample_rs() {
    local out="$CODE/sample.rs"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out"
    cat >"$out" <<'RS'
//! Sample Rust file for code-preview QA.
//! NEEDLE-A appears in this comment for search tests.

use std::collections::HashMap;

pub fn fizzbuzz(n: u32) -> Vec<String> {
    (1..=n)
        .map(|i| match (i % 3, i % 5) {
            (0, 0) => "fizzbuzz".into(),
            (0, _) => "fizz".into(),
            (_, 0) => "buzz".into(),
            _ => i.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_15() {
        let v = fizzbuzz(15);
        assert_eq!(v.last().unwrap(), "fizzbuzz");
    }
}

#[allow(dead_code)]
fn _padding() -> HashMap<&'static str, u32> {
    HashMap::new()
}
RS
    mark_built "$out" "$stamp"
}

# ---- niche -----------------------------------------------------------------

build_sample_djvu() {
    local out="$NICHE/sample.djvu"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out"
    convert -size 600x800 xc:white \
        -font DejaVu-Sans -pointsize 28 -gravity center \
        -annotate 0 "Lixun DjVu sample" "$WORK/djvu-page.pbm"
    djvumake "$out" \
        INFO=600,800,300 \
        Sjbz="$WORK/djvu-page.pbm" >/dev/null 2>&1 || {
        # Fallback: cjb2 path if djvumake is too strict on this distro.
        cjb2 "$WORK/djvu-page.pbm" "$out"
    }
    mark_built "$out" "$stamp"
}

build_sample_epub() {
    local out="$NICHE/sample.epub"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out"
    pandoc -o "$out" --metadata title="Lixun EPUB Sample" "$CODE/sample.md"
    mark_built "$out" "$stamp"
}

build_sample_ipynb() {
    local out="$NICHE/sample.ipynb"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out"
    cat >"$out" <<'JSON'
{
 "cells": [
  {
   "cell_type": "markdown",
   "metadata": {},
   "source": ["# Lixun Notebook Sample\n", "\n", "NEEDLE-A appears here.\n"]
  },
  {
   "cell_type": "code",
   "execution_count": 1,
   "metadata": {},
   "outputs": [{"output_type": "stream", "name": "stdout", "text": ["hello\n"]}],
   "source": ["print('hello')\n"]
  }
 ],
 "metadata": {
  "kernelspec": {"display_name": "Python 3", "language": "python", "name": "python3"},
  "language_info": {"name": "python", "version": "3.11"}
 },
 "nbformat": 4,
 "nbformat_minor": 5
}
JSON
    mark_built "$out" "$stamp"
}

build_sample_sqlite() {
    local out="$NICHE/sample.sqlite"
    local stamp="v1"
    if ! needs_rebuild "$out" "$stamp"; then return; fi
    log "building $out"
    rm -f "$out"
    sqlite3 "$out" <<'SQL'
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, email TEXT);
CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT, body TEXT);
CREATE TABLE tags  (id INTEGER PRIMARY KEY, post_id INTEGER, tag TEXT);
WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<100)
INSERT INTO users(id,name,email) SELECT i, 'user'||i, 'u'||i||'@example.invalid' FROM c;
WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<100)
INSERT INTO posts(id,user_id,title,body) SELECT i, ((i-1)%100)+1, 'title '||i, 'body '||i FROM c;
WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<100)
INSERT INTO tags(id,post_id,tag) SELECT i, ((i-1)%100)+1, 'tag'||(i%7) FROM c;
SQL
    mark_built "$out" "$stamp"
}

# ---- run -------------------------------------------------------------------

build_sample_text_pdf
build_sample_scan_pdf
build_sample_encrypted_pdf
build_sample_broken_pdf
build_sample_huge_pdf

build_office_text_master
build_office_sheet_master
build_office_slides_master
build_office_encrypted

build_sample_jpg
build_sample_png_large
build_sample_anim_gif
build_sample_svg

build_sample_md
build_sample_rs

build_sample_djvu
build_sample_epub
build_sample_ipynb
build_sample_sqlite

log "all fixtures built"
