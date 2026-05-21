#!/usr/bin/env bash
# run-heaptrack-baseline.sh
#
# Wraps the memory-states harness with heaptrack, then prints a leak
# summary via heaptrack_print.
#
# Precondition: model cache is already populated so the harness does
# not measure download time / memory.
#
# Requires: heaptrack (apt install heaptrack)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

HEAPTRACK="${HEAPTRACK:-heaptrack}"
HEAPTRACK_PRINT="${HEAPTRACK_PRINT:-heaptrack_print}"

if ! command -v "$HEAPTRACK" >/dev/null 2>&1; then
    echo "Error: heaptrack not found. Install it with:"
    echo "  sudo apt install heaptrack"
    exit 1
fi

HARNESS="${PROJECT_ROOT}/target/release/memory-states-harness"

if [[ ! -x "$HARNESS" ]]; then
    echo "Building memory-states-harness (release)..."
    cargo build -p lixun-semantic-worker --bin memory-states-harness --release
fi

TIMESTAMP=$(date +%Y%m%dT%H%M%S)
EVIDENCE_DIR="${PROJECT_ROOT}/.workspace-local/evidence/baseline/memory_states"
mkdir -p "$EVIDENCE_DIR"

OUTPUT_JSONL="${EVIDENCE_DIR}/${TIMESTAMP}.jsonl"
HEAPTRACK_FILE="${EVIDENCE_DIR}/${TIMESTAMP}.zst"

echo "=== Running heaptrack --record-only ==="
"$HEAPTRACK" --record-only -o "$HEAPTRACK_FILE" "$HARNESS" --states-mode=all > "$OUTPUT_JSONL"

echo ""
echo "=== JSONL baseline written to ==="
echo "$OUTPUT_JSONL"

echo ""
echo "=== heaptrack_print --print-leaks summary ==="
"$HEAPTRACK_PRINT" --print-leaks "$HEAPTRACK_FILE" | tail -n 30

echo ""
echo "=== RSS summary per state ==="
jq -r '[.state, .vm_rss_kb, .rss_anon_kb, .rss_file_kb] | @tsv' "$OUTPUT_JSONL" | column -t -N STATE,VmRSS,RssAnon,RssFile
