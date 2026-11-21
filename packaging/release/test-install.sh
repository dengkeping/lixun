#!/usr/bin/env bash
# Runs inside a new distribution container with only the packages mounted.
set -euo pipefail
cd /packages
if [[ $TARGET == arch ]]; then
    pacman -Syu --noconfirm
    pacman -U --noconfirm ./lixun-"$VERSION"-*.pkg.tar.zst
else
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y --no-install-recommends ./lixun_*.deb
fi

test -r /usr/lib/systemd/user/lixund.service
test -r /usr/share/applications/app.lixun.daemon.desktop
test -r /usr/share/lixun/matugen/lixun-colors.css.tmpl
for binary in lixund lixun-cli lixun-gui lixun-preview; do
    ldd -r "/usr/bin/$binary" > /tmp/link-check
    if grep -E 'not found|undefined symbol' /tmp/link-check; then exit 1; fi
done

useradd --create-home tester
runuser -u tester -- bash -eu -o pipefail <<'SMOKE'
export XDG_RUNTIME_DIR="$HOME/run"
mkdir -m700 "$XDG_RUNTIME_DIR" "$HOME/corpus" -p "$HOME/.config/lixun"
cat > "$HOME/.config/lixun/config.toml" <<CONFIG
[core]
roots = ["$HOME/corpus"]
[semantic]
enabled = false
[ocr]
enabled = false
CONFIG
touch "$HOME/corpus/packagefixture.txt"
lixund > "$HOME/daemon.log" 2>&1 &
pid=$!
trap 'kill "$pid" 2>/dev/null || true' EXIT
for attempt in {1..60}; do
    if ! kill -0 "$pid"; then cat "$HOME/daemon.log"; exit 1; fi
    if lixun-cli search packagefixture 2>/dev/null | grep -F 'packagefixture.txt'; then
        lixun-cli status
        exit 0
    fi
    sleep 1
done
cat "$HOME/daemon.log"
exit 1
SMOKE

if [[ $TARGET == arch ]]; then
    pacman -U --noconfirm ./lixun-semantic-*.pkg.tar.zst ./lixun-ocr-*.pkg.tar.zst
else
    apt-get install -y --no-install-recommends ./lixun-semantic_*.deb ./lixun-ocr_*.deb
fi
/usr/lib/lixun/lixun-semantic-worker --help
tesseract --list-langs | grep -Fx eng
