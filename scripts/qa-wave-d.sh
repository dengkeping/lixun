#!/usr/bin/env bash
set -euo pipefail

# Anchor every step at the repo root so the script behaves identically
# whether invoked from CI, a packaging chroot, or a developer's subdir.
cd "$(git rev-parse --show-toplevel)"

echo '==> step 1: cargo build --workspace --release (default features)'
cargo build --locked --workspace --release

echo '==> Test both IPC formats and the release worker'
cargo test --locked -p lixun-ipc --no-default-features
cargo test --locked -p lixun-ipc --features legacy-json-ipc
cargo test --locked -p lixun-daemon --test semantic_supervisor --test semantic_backfill_e2e
LIXUN_TEST_REAL_TESSERACT=1 cargo test --locked -p lixun-extract real_tesseract_reads_hello_fixture -- --ignored
xvfb-run -a env DBUS_SESSION_BUS_ADDRESS=unix:path=/dev/null GDK_BACKEND=x11 GSK_RENDERER=cairo cargo test --locked -p lixun-preview-pdf -- --ignored --test-threads=1

socket_tests=$(cargo test --locked -p lixun-daemon --test cancellation --no-run --message-format=json |
    python -c 'import json,sys; print(next(row["executable"] for row in map(json.loads,sys.stdin) if row.get("executable") and row["target"]["name"]=="cancellation"))')
python tests/daemon-regressions.py --binaries target/release --socket-tests "$socket_tests"

echo '==> step 4: AGENTS §1 host-crate grep'
# §1: plugin-domain strings must not leak into host or trait crates.
# The only authorised matches are optional ML dep declarations in
# crates/lixun-fusion/Cargo.toml (T0); everything else is a violation.
section1_matches=$(git grep -nE 'fastembed|lancedb|bge-|clip-vit' -- \
    crates/lixun-daemon \
    crates/lixun-gui \
    crates/lixun-cli \
    crates/lixun-preview-bin \
    crates/lixun-core \
    crates/lixun-ipc \
    crates/lixun-sources \
    crates/lixun-indexer \
    crates/lixun-index \
    crates/lixun-preview \
    crates/lixun-mutation \
    crates/lixun-fusion ':!*/tests/*' 2>/dev/null || true)
unauthorised=$(echo "$section1_matches" | grep -vE '^crates/lixun-fusion/Cargo.toml:|:[0-9]+:[[:space:]]*//'  || true)
if [[ -n "$unauthorised" ]]; then
    echo 'AGENTS §1 violation:' >&2
    echo "$unauthorised" >&2
    exit 1
fi

echo '==> step 5: AGENTS §2 grep + fmt + rustdoc'
# §2: no AI-agent names anywhere in the working tree or in commits since main.
# 'cursor' is omitted from the regex because of two pre-existing
# false-positives: the Rust std Cursor type in lixun-ipc and the
# SQLite cursor reference in T7's commit body.
pattern='sisyphus|claude|gpt|copilot|gemini|aider|codex|opencode|assistant|anthropic|openai'
if git log --format='%an <%ae>|%cn <%ce>|%s%n%b' main..HEAD | grep -iE "$pattern"; then
    echo 'AGENTS §2 violation in commit log' >&2
    exit 1
fi
if git grep -niE "\b($pattern)" -- ':!docs/ocr.md' ':!scripts/qa-wave-d.sh' ':!.gitignore'; then
    echo 'AGENTS §2 violation in tracked files' >&2
    exit 1
fi

cargo fmt --all -- --check
RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -D warnings" cargo doc --locked --no-deps --workspace

echo 'qa-wave-d: all gates green'
