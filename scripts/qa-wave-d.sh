#!/usr/bin/env bash
set -euo pipefail

# Anchor every step at the repo root so the script behaves identically
# whether invoked from CI, a packaging chroot, or a developer's subdir.
cd "$(git rev-parse --show-toplevel)"

echo '==> step 1: cargo build --workspace --release (default features)'
cargo build --workspace --release

echo '==> step 2: cargo build lixund --release --features semantic (hybrid daemon)'
cargo build -p lixun-daemon --bin lixund --release --features semantic

echo '==> step 3: cargo build lixund --release --features lixun-plugin-bundle/semantic (bundle linkability probe)'
cargo build -p lixun-daemon --bin lixund --release --features 'lixun-plugin-bundle/semantic'

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
    crates/lixun-fusion 2>/dev/null || true)
unauthorised=$(echo "$section1_matches" | grep -v '^crates/lixun-fusion/Cargo.toml:' || true)
if [ -n "$unauthorised" ]; then
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
if git grep -niE "$pattern" -- ':!docs/ocr.md' ':!scripts/qa-wave-d.sh' ':!.gitignore'; then
    echo 'AGENTS §2 violation in tracked files' >&2
    exit 1
fi

cargo fmt --all -- --check
cargo doc --no-deps --workspace

echo 'qa-wave-d: all gates green'
