#!/usr/bin/env bash
# Run with built binaries on PATH. Create a fresh bus after isolating all XDG paths.
set -euo pipefail
fixture="$PWD/tests/fixtures/preview/pdf/sample-text.pdf"
if [[ ${1:-} != --session ]]; then
    profile=$(mktemp -d)
    runtime=$(mktemp -d)
    export HOME="$profile/home" XDG_RUNTIME_DIR="$runtime" XDG_CACHE_HOME="$profile/cache"
    export XDG_STATE_HOME="$profile/state" XDG_DATA_HOME="$profile/data" XDG_CONFIG_HOME="$profile/config"
    mkdir -m 700 "$HOME"
    cleanup_profile() {
        # The bus runtime is a separate sibling. Never recursively remove it:
        # a document-portal mount can appear even as desktop helpers exit.
        rmdir "$runtime" 2>/dev/null || true
        if ! awk -v root="$profile/" 'index($5,root)==1 {found=1} END {exit !found}' /proc/self/mountinfo; then
            rm -rf --one-file-system -- "$profile"
        fi
    }
    trap cleanup_profile EXIT
    unset DBUS_SESSION_BUS_ADDRESS WAYLAND_DISPLAY SWAYSOCK DISPLAY
    dbus-run-session -- bash "$0" --session "$profile"
    exit
fi
profile=$2
export WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman GDK_BACKEND=wayland
export XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=sway LIBGL_ALWAYS_SOFTWARE=1 GSK_RENDERER=cairo
mkdir -p "$HOME" "$XDG_CONFIG_HOME/lixun" "$profile/docs"
cp "$fixture" "$profile/docs/display-fixture.pdf"
printf 'Layout fixture\n' > "$profile/docs/привет.txt"
cat > "$XDG_CONFIG_HOME/lixun/config.toml" <<EOF
[core]
roots = ["$profile/docs"]
[semantic]
enabled = false
EOF
cat > "$profile/sway.conf" <<EOF
output HEADLESS-1 resolution 1440x900
seat seat0 fallback true
xwayland disable
EOF
sway -c "$profile/sway.conf" > "$profile/sway.log" 2>&1 &
compositor=$!
daemon=
cleanup() {
    if [[ -n "$daemon" ]]; then
        kill "$daemon" 2>/dev/null || true
        wait "$daemon" 2>/dev/null || true
    fi
    kill "$compositor" 2>/dev/null || true
    wait "$compositor" 2>/dev/null || true
}
trap cleanup EXIT
trap '[[ ! -f "$profile/daemon.log" ]] || tail -40 "$profile/daemon.log" >&2' ERR
for ((i=0; i<100; i++)); do
    for display in "$XDG_RUNTIME_DIR"/wayland-*; do
        if [[ -S "$display" ]]; then export WAYLAND_DISPLAY="${display##*/}"; break 2; fi
    done
    sleep .1
done
if [[ -z "${WAYLAND_DISPLAY:-}" ]]; then
    cat "$profile/sway.log" >&2
    exit 1
fi
dbus-update-activation-environment XDG_CURRENT_DESKTOP XDG_CONFIG_HOME XDG_RUNTIME_DIR WAYLAND_DISPLAY GDK_BACKEND
RUST_LOG=lixun=debug lixund > "$profile/daemon.log" 2>&1 &
daemon=$!
for ((i=0; i<150; i++)); do
    if lixun-cli search display-fixture 2>/dev/null | grep -q 'display-fixture.pdf'; then break; fi
    sleep .1
done
lixun-cli show &
first_show=$!
lixun-cli show &
second_show=$!
wait "$first_show"
wait "$second_show"
[[ "$(grep -c 'gui_control: spawned lixun-gui pid=' "$profile/daemon.log")" -eq 1 ]]
# One virtual keyboard maintains its seat for the whole interaction.
wtype -s 600 -d 35 display-fixture -s 1000 -M ctrl -k c -m ctrl -s 700 -M ctrl -k space -m ctrl -s 1000 -k Page_Down -s 300 -k Escape
[[ "$(timeout 3 wl-paste -n)" == "$profile/docs/display-fixture.pdf" ]]
for ((i=0; i<100; i++)); do
    if grep -q 'preview shown' "$profile/daemon.log"; then break; fi
    sleep .1
done
grep -q 'preview shown' "$profile/daemon.log"
lixun-cli hide
lixun-cli show
wtype -s 400 -M ctrl -k a -m ctrl -d 30 'privateguifixture814' -s 400 -k Escape
if grep -Fq 'privateguifixture814' "$profile/daemon.log"; then exit 1; fi
# Input and copied content should never appear in GUI or IPC diagnostics.
if grep -E 'entry changed, text=|sending search request query=|Copied to clipboard:|final hit\[|key_controller fired key=' "$profile/daemon.log"; then exit 1; fi

# Accept the keyboard-layout retry, then save the query with a single virtual keyboard.
lixun-cli show
(sleep 2.3; gapplication action app.lixun.gui saved-searches) &
opener=$!
wtype -s 350 -M ctrl -k a -m ctrl -d 30 ghbdtn -s 500 -k Return -s 650 -M ctrl -k c -m ctrl -s 1350 -d 25 'Layout fixture' -k Return -s 350
wait "$opener"
[[ "$(timeout 3 wl-paste -n)" == "привет" ]]
python - "$XDG_STATE_HOME/lixun/saved-searches" <<'PYTHON'
import json, pathlib, sys
records = [json.loads(f.read_text()) for f in pathlib.Path(sys.argv[1]).glob("*.json")]
assert any(r["name"] == "Layout fixture" and r["query"] == "привет" for r in records), records
PYTHON
# Reopening the definition should query the new filename in the live index.
mv "$profile/docs/привет.txt" "$profile/docs/привет-new.txt"
for ((i=0; i<100; i++)); do
    if lixun-cli search привет-new 2>/dev/null | grep -q 'привет-new.txt'; then break; fi
    sleep .1
done
lixun-cli show
(sleep 1.5; gapplication action app.lixun.gui saved-searches) &
opener=$!
wtype -s 300 -M ctrl -k a -m ctrl -d 20 display-fixture -s 1350 -k Tab -k Tab -k Return -s 550 -k End -M ctrl -k c -m ctrl -s 300
wait "$opener"
[[ "$(timeout 3 wl-paste -n)" == "$profile/docs/привет-new.txt" ]]
