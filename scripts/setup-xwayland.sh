#!/usr/bin/env bash
# Build a CPU-only, direct-EI Xwayland without replacing system libraries.
# Debian 12 prerequisites (install separately): build-essential curl xz-utils
# bzip2 meson ninja-build pkg-config python3-jinja2 python3-attr libwayland-dev
# libx11-dev libxkbfile-dev libxfont-dev libxcvt-dev libpixman-1-dev
# libxkbcommon-dev libdrm-dev libssl-dev libtirpc-dev xtrans-dev x11proto-dev
# libxshmfence-dev x11-xkb-utils xkb-data.
set -euo pipefail

prefix="${WAYWIRE_XWAYLAND_PREFIX:-$HOME/.local/share/waywire-xwayland}"
if [[ -x "$prefix/bin/Xwayland" ]] && "$prefix/bin/Xwayland" -version 2>&1 | grep -Fq 'Xwayland Version 24.1.13'; then
    printf 'CPU-only Xwayland is already installed in %s\n' "$prefix"
    exit 0
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# Debian's meson must find Debian's python modules, not an unrelated venv.
export PATH=/usr/bin:/bin
export PKG_CONFIG_PATH="$prefix/lib/pkgconfig:$prefix/share/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

fetch() {
    local url=$1 archive=$2 checksum=$3
    curl --fail --location --retry 3 "$url" -o "$work/$archive"
    echo "$checksum  $work/$archive" | sha256sum --check --status
    tar -xf "$work/$archive" -C "$work"
}

fetch https://deb.debian.org/debian/pool/main/libe/libei/libei_1.5.0.orig.tar.bz2 libei.tar.bz2 da1fba92daccd0667bc46c3ee952d4ae8cfc6bdb4c0bb4d34df26528fb240618
fetch https://deb.debian.org/debian/pool/main/w/wayland-protocols/wayland-protocols_1.47.orig.tar.xz protocols.tar.xz 5fd4349bcbc9bab9a46f8cf77d1f434296a7a052c87440a094f63fcf62a58e20
fetch https://www.x.org/releases/individual/proto/xorgproto-2024.1.tar.xz xorgproto.tar.xz 372225fd40815b8423547f5d890c5debc72e88b91088fbfb13158c20495ccb59
fetch https://www.x.org/releases/individual/xserver/xwayland-24.1.13.tar.xz xwayland.tar.xz 173aea3d6f79609164c04528e1c8e4c9b60fcd59391c3c9dad4667297d727fb6

build() {
    local source=$1
    shift
    meson setup "$work/$source-build" "$work/$source" --prefix="$prefix" --libdir=lib "$@"
    ninja -C "$work/$source-build" -j "${WAYWIRE_BUILD_JOBS:-4}" install
}

build libei-1.5.0 -Dtests=disabled -Dliboeffis=disabled -Dlibeis=disabled -Ddocumentation=[]
build wayland-protocols-1.47 -Dtests=false
build xorgproto-2024.1
# Xwayland includes xf86drm.h even in the software-only build. No DRM device
# or GL library is needed at runtime. Rpath survives Smithay's env_clear().
export LDFLAGS="-Wl,-rpath,$prefix/lib"
build xwayland-24.1.13 -Dglamor=false -Dglx=false -Ddrm=false -Ddri3=false \
    -Dxwayland_ei=socket -Dxvfb=false -Dlibdecor=false -Ddocs=false \
    -Ddevel-docs=false -Dxselinux=false -Dc_args=-I/usr/include/libdrm
"$prefix/bin/Xwayland" -version
printf 'Installed CPU-only Xwayland in %s/bin\n' "$prefix"
