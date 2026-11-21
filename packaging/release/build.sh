#!/usr/bin/env bash
# Runs as root in an ephemeral build container.
set -euo pipefail
export CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0
mkdir /build
tar -xzf /input/source.tar.gz -C /build
cd "/build/lixun-$GITHUB_SHA"

if [[ $TARGET == arch ]]; then
    pacman -Syu --noconfirm --needed rust clang cmake protobuf gtk4 gtk4-layer-shell \
        poppler-glib libheif openssl pkgconf tesseract tesseract-data-eng xdg-utils
    useradd --create-home builder
    sum=$(sha256sum /input/source.tar.gz | cut -d' ' -f1)
    for name in lixun lixun-semantic lixun-ocr; do
        dir="/home/builder/$name"
        install -d -o builder -g builder "$dir"
        # Keep standalone recipes pinned. The CI copy uses this exact checkout.
        sed -e "s/^pkgver=.*/pkgver=$VERSION/" -e "s/^_commit=.*/_commit=$GITHUB_SHA/" \
            -e "s/^sha256sums=.*/sha256sums=('$sum')/" \
            -e 's/^options=(/options=(!debug /' \
            "packaging/arch/$name/PKGBUILD" > "$dir/PKGBUILD"
        if [[ $name != lixun-ocr ]]; then
            cp /input/source.tar.gz "$dir/lixun-$VERSION.tar.gz"
        fi
        (cd "$dir" && runuser -u builder -- makepkg --clean --noconfirm)
        cp "$dir/$name-$VERSION-"*.pkg.tar.zst /out/
        if [[ $name == lixun ]]; then
            pacman -U --noconfirm "$dir/$name-$VERSION-"*.pkg.tar.zst
        fi
    done
else
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y --no-install-recommends build-essential debhelper ca-certificates \
        curl clang cmake protobuf-compiler libgtk-4-dev libgtk4-layer-shell-dev \
        libpoppler-glib-dev libheif-dev pkg-config libssl-dev libbz2-dev liblzma-dev zlib1g-dev
    curl --fail --silent --show-error --location --proto '=https' https://sh.rustup.rs -o /tmp/rustup-init.sh
    sh /tmp/rustup-init.sh -y --profile minimal --default-toolchain stable --no-modify-path
    export PATH="/root/.cargo/bin:$PATH"
    cp -r packaging/debian debian
    case "$TARGET" in
        debian13) revision=deb13; suite=trixie ;;
        ubuntu26.04) revision=ubuntu26.04; suite=resolute ;;
        *) exit 1 ;;
    esac
    cat > debian/changelog <<CHANGELOG
lixun ($VERSION-1+$revision) $suite; urgency=medium

  * Build the upstream release for $suite.

 -- Denis Kopin <deng@lixun.app>  $(date -Ru)
CHANGELOG
    dpkg-buildpackage -b -us -uc
    cp ../*.deb /out/
fi
