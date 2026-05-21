#!/usr/bin/env bash
set -euo pipefail

# GUI RSS baseline: measures lixun-gui RSS across idle/active/teardown phases.
# Usage: scripts/gui-rss-baseline.sh [--compare-with=OLD.json]

BINARY_NAME="${LIXUN_GUI_BINARY:-lixun-gui}"
WTYPE_CMD="${WTYPE_CMD:-wtype}"
XDOTOOL_CMD="${XDOTOOL_CMD:-xdotool}"

# Session / tool detection

detect_session_type() {
    if [ -n "${WAYLAND_DISPLAY:-}" ]; then
        printf 'wayland\n'
    elif [ -n "${DISPLAY:-}" ]; then
        printf 'x11\n'
    else
        printf 'none\n'
    fi
}

get_input_tool() {
    local session
    session=$(detect_session_type)

    case "$session" in
        wayland)
            if [ -x "$WTYPE_CMD" ] || command -v "$WTYPE_CMD" >/dev/null 2>&1; then
                printf 'wtype\n'
            else
                printf 'wtype (not installed)\n' >&2
            fi
            ;;
        x11)
            if [ -x "$XDOTOOL_CMD" ] || command -v "$XDOTOOL_CMD" >/dev/null 2>&1; then
                printf 'xdotool\n'
            else
                printf 'xdotool (not installed)\n' >&2
            fi
            ;;
        *)
            printf 'error: Neither WAYLAND_DISPLAY nor DISPLAY is set.\n' >&2
            printf 'Cannot determine display session type.\n' >&2
            exit 1
            ;;
    esac
}

# Binary discovery

find_binary() {
    local candidates=(
        "./target/release/${BINARY_NAME}"
        "./target/debug/${BINARY_NAME}"
    )
    for c in "${candidates[@]}"; do
        if [ -x "$c" ]; then
            realpath "$c"
            return 0
        fi
    done
    if command -v "$BINARY_NAME" >/dev/null 2>&1; then
        command -v "$BINARY_NAME"
        return 0
    fi
    printf 'error: GUI binary "%s" not found.\n' "$BINARY_NAME" >&2
    printf 'Build it with: cargo build -p lixun-gui --release\n' >&2
    exit 1
}

# Process management

get_running_pid() {
    local pids
    pids=$(pgrep -fx "$BINARY_NAME" 2>/dev/null || true)
    if [ -z "$pids" ]; then
        pids=$(pidof "$BINARY_NAME" 2>/dev/null || true)
    fi
    printf '%s\n' "$pids"
}

wait_for_exit() {
    local pid=$1
    local max_wait=${2:-10}
    local waited=0
    while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt "$max_wait" ]; do
        sleep 1
        waited=$((waited + 1))
    done
}

terminate_existing() {
    local pid
    pid=$(get_running_pid)
    if [ -n "$pid" ]; then
        printf 'Terminating existing %s instance (PID %s)...\n' "$BINARY_NAME" "$pid"
        kill -TERM "$pid" 2>/dev/null || true
        wait_for_exit "$pid" 5
        if kill -0 "$pid" 2>/dev/null; then
            kill -KILL "$pid" 2>/dev/null || true
            wait_for_exit "$pid" 3
        fi
    fi
}

# Sampling helpers

sample_proc_status() {
    local pid=$1
    local state=$2
    local t_ms=$3
    local status_file="/proc/${pid}/status"

    if [ ! -f "$status_file" ]; then
        printf 'error: %s does not exist.\n' "$status_file" >&2
        return 1
    fi

    local vm_rss vm_size rss_anon rss_file rss_shmem
    vm_rss=$(grep -m1 '^VmRSS:' "$status_file" | awk '{print $2}')
    vm_size=$(grep -m1 '^VmSize:' "$status_file" | awk '{print $2}')
    rss_anon=$(grep -m1 '^RssAnon:' "$status_file" | awk '{print $2}')
    rss_file=$(grep -m1 '^RssFile:' "$status_file" | awk '{print $2}')
    rss_shmem=$(grep -m1 '^RssShmem:' "$status_file" | awk '{print $2}')

    vm_rss="${vm_rss:-0}"
    vm_size="${vm_size:-0}"
    rss_anon="${rss_anon:-0}"
    rss_file="${rss_file:-0}"
    rss_shmem="${rss_shmem:-0}"

    jq -n \
        --arg state "$state" \
        --argjson t_ms "$t_ms" \
        --argjson vm_rss_kb "$vm_rss" \
        --argjson vm_size_kb "$vm_size" \
        --argjson rss_anon_kb "$rss_anon" \
        --argjson rss_file_kb "$rss_file" \
        --argjson rss_shmem_kb "$rss_shmem" \
        '{
            state: $state,
            t_ms: $t_ms,
            vm_rss_kb: $vm_rss_kb,
            vm_size_kb: $vm_size_kb,
            rss_anon_kb: $rss_anon_kb,
            rss_file_kb: $rss_file_kb,
            rss_shmem_kb: $rss_shmem_kb
        }'
}

# Keystroke injection

trigger_keystrokes() {
    local text="abcdefghijklmnopqrst"
    local succeeded=0

    if [ -x "$WTYPE_CMD" ] || command -v "$WTYPE_CMD" >/dev/null 2>&1; then
        if "$WTYPE_CMD" "$text" >/dev/null 2>&1; then
            succeeded=1
        else
            printf 'warning: wtype failed (compositor may lack virtual-keyboard support).\n' >&2
        fi
    fi

    if [ "$succeeded" -eq 0 ] && { [ -x "$XDOTOOL_CMD" ] || command -v "$XDOTOOL_CMD" >/dev/null 2>&1; }; then
        for ((i = 0; i < ${#text}; i++)); do
            "$XDOTOOL_CMD" type "${text:$i:1}" >/dev/null 2>&1 || true
            sleep 0.05
        done
        succeeded=1
    fi

    if [ "$succeeded" -eq 0 ]; then
        printf 'warning: No working keystroke injector found (tried wtype, xdotool).\n' >&2
        printf '         Active-phase RSS may not differ from idle-phase RSS.\n' >&2
    fi
}

# Baseline run

run_baseline() {
    local binary input_tool
    binary=$(find_binary)
    input_tool=$(get_input_tool)

    printf 'Session type: %s\n' "$(detect_session_type)"
    printf 'Input tool:   %s\n' "$input_tool"
    printf 'Binary:       %s\n' "$binary"

    # Ensure no stale instance
    terminate_existing

    printf 'Launching %s...\n' "$BINARY_NAME"
    "$binary" &
    gui_pid=$!

    # shellcheck disable=SC2317
    cleanup() {
        if kill -0 "$gui_pid" 2>/dev/null; then
            kill -TERM "$gui_pid" 2>/dev/null || true
            wait_for_exit "$gui_pid" 5
            kill -KILL "$gui_pid" 2>/dev/null || true
        fi
    }
    trap cleanup EXIT

    printf 'Waiting 5 s for first frame...\n'
    sleep 5

    if ! kill -0 "$gui_pid" 2>/dev/null; then
        printf 'error: GUI process exited before baseline could start.\n' >&2
        exit 1
    fi

    local samples_jsonl start_time t_ms
    samples_jsonl=$(mktemp)
    start_time=$(date +%s%N)

    # ---- Idle phase: 12 samples every 5 s --------------------------------
    printf 'Collecting idle samples (12 x 5 s)...\n'
    for i in $(seq 0 11); do
        t_ms=$(( ($(date +%s%N) - start_time) / 1000000 ))
        sample_proc_status "$gui_pid" "idle" "$t_ms" >> "$samples_jsonl"
        if [ "$i" -lt 11 ]; then
            sleep 5
        fi
    done

    # ---- Active phase: keystrokes + 12 samples ---------------------------
    printf 'Injecting 20 synthetic keystrokes...\n'
    trigger_keystrokes

    printf 'Collecting active samples (12 x 5 s)...\n'
    for i in $(seq 0 11); do
        t_ms=$(( ($(date +%s%N) - start_time) / 1000000 ))
        sample_proc_status "$gui_pid" "active" "$t_ms" >> "$samples_jsonl"
        if [ "$i" -lt 11 ]; then
            sleep 5
        fi
    done

    # ---- Teardown: one last sample, then close ---------------------------
    t_ms=$(( ($(date +%s%N) - start_time) / 1000000 ))
    sample_proc_status "$gui_pid" "teardown" "$t_ms" >> "$samples_jsonl"

    printf 'Sending SIGTERM to GUI...\n'
    kill -TERM "$gui_pid" 2>/dev/null || true
    wait_for_exit "$gui_pid" 10
    if kill -0 "$gui_pid" 2>/dev/null; then
        printf 'warning: GUI did not exit gracefully; sending SIGKILL.\n' >&2
        kill -KILL "$gui_pid" 2>/dev/null || true
        wait_for_exit "$gui_pid" 5
    fi

    # ---- Assemble JSON output --------------------------------------------
    local out_dir out_file timestamp sample_count
    out_dir=".workspace-local/evidence/baseline/gui_rss"
    mkdir -p "$out_dir"
    timestamp=$(date +%Y%m%d_%H%M%S)
    out_file="${out_dir}/${timestamp}.json"

    jq -s '{samples: .}' < "$samples_jsonl" > "$out_file"
    rm -f "$samples_jsonl"

    sample_count=$(jq '.samples | length' "$out_file")
    printf 'Wrote %d samples to %s\n' "$sample_count" "$out_file"

    if [ "$sample_count" -ne 25 ]; then
        printf 'warning: Expected 25 samples, got %d.\n' "$sample_count" >&2
    fi

    printf '%s\n' "$out_file"
}

# Compare mode

compare_baselines() {
    local old_file=$1
    local new_file=$2

    if [ ! -f "$old_file" ]; then
        printf 'error: Old baseline not found: %s\n' "$old_file" >&2
        exit 1
    fi
    if [ ! -f "$new_file" ]; then
        printf 'error: New baseline not found: %s\n' "$new_file" >&2
        exit 1
    fi

    local states=(idle active teardown)
    local regressed=0

    for state in "${states[@]}"; do
        local old_mean new_mean
        old_mean=$(jq -r --arg state "$state" '
            [ .samples[] | select(.state == $state) | .vm_rss_kb ] |
            if length > 0 then add / length else 0 end
        ' "$old_file")
        new_mean=$(jq -r --arg state "$state" '
            [ .samples[] | select(.state == $state) | .vm_rss_kb ] |
            if length > 0 then add / length else 0 end
        ' "$new_file")

        if [ "$old_mean" = "null" ] || [ "$new_mean" = "null" ]; then
            printf 'Skipping %s (missing data).\n' "$state" >&2
            continue
        fi

        local delta_pct abs_delta
        delta_pct=$(awk "BEGIN { v = (($new_mean - $old_mean) / $old_mean) * 100; printf \"%.2f\", v }")
        abs_delta=$(awk "BEGIN { v = $delta_pct; if (v < 0) v = -v; printf \"%.2f\", v }")

        printf 'State: %s | old: %.1f kB | new: %.1f kB | delta: %s%%\n' \
            "$state" "$old_mean" "$new_mean" "$delta_pct"

        if awk "BEGIN { exit !($abs_delta > 5) }"; then
            printf 'REGRESSION: %s drifted by %s%% (threshold ±5%%)\n' \
                "$state" "$delta_pct" >&2
            regressed=1
        fi
    done

    if [ "$regressed" -eq 1 ]; then
        exit 1
    else
        printf 'All states within ±5%% threshold.\n'
        exit 0
    fi
}

# Entry point

main() {
    local compare_target=""

    for arg in "$@"; do
        case "$arg" in
            --compare-with=*)
                compare_target="${arg#--compare-with=}"
                ;;
            --help|-h)
                printf 'Usage: %s [--compare-with=OLD.json]\n' "$0"
                printf '  Runs a fresh RSS baseline and optionally compares it to OLD.json.\n'
                exit 0
                ;;
        esac
    done

    local out_file
    out_file=$(run_baseline | tail -1)

    if [ -n "$compare_target" ]; then
        printf '\n--- Comparing with %s ---\n' "$compare_target"
        compare_baselines "$compare_target" "$out_file"
    fi
}

main "$@"
