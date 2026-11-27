#!/bin/bash
# The Linux packages for a build (after scripts/package.sh TAG, whose guide they carry):
#   installer/linux/deb/build.sh TAG
# Builds in a Debian 12 container (Dockerfile.builder; docker or podman), with BtbN's
# FFmpeg 8.1 LGPL build for Linux (pinned, checked), into ROOT/Installers/Media and
# ROOT/Installers/Industrial:
# idn-media_VER~tag_amd64.deb and idn-industrial_VER~tag_amd64.deb. Then test.sh installs
# them on Debian 12, Ubuntu 24.04 and Ubuntu 26.04.
# The cargo cache and the build's target folder stay in ROOT/build-kit/deb (on disk).
set -euo pipefail
TAG="${1:?usage: installer/linux/deb/build.sh TAG}"
# media, industrial or both (scripts/package.sh sets it).
PRODUCT="${IDN_PRODUCT:-both}"
HERE="$(cd "$(dirname "$0")" && pwd)"
IMPL="$(cd "$HERE/../../.." && pwd)"
ROOT="$(cd "$IMPL/.." && pwd)"
VER="$(sed -n 's/^version = "\(.*\)"/\1/p' "$IMPL/core/idn-open-protocol/Cargo.toml" | head -1)"
# Debian's order: 0.2.0~alpha25 comes before 0.2.0.
VERSION="$VER~$(echo "$TAG" | tr '[:upper:]' '[:lower:]')"
GUIDE="$ROOT/handoff/$TAG/guide"
[ "$PRODUCT" = industrial ] || [ -d "$GUIDE" ] || { echo "no $GUIDE: run scripts/package.sh $TAG first" >&2; exit 1; }
DOCKER=$(command -v docker || command -v podman)
K="$ROOT/build-kit/deb"
mkdir -p "$K/target" "$K/cargo-home"
# FFmpeg 8.1, the same build the Windows installer takes (BtbN's), for Linux.
FF_NAME=ffmpeg-n8.1.3-9-g29e619e767-linux64-lgpl-shared-8.1
FF_URL="https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-09-30-13-08/$FF_NAME.tar.xz"
FF_SHA256=d4d7b6936f492c0b866b1cb4a29a1bbae09d6a064b4ec4eb90c95bed7969535a
FD="$ROOT/build-kit/linux-ffmpeg"
mkdir -p "$FD"
if [ ! -d "$FD/$FF_NAME" ]; then
  [ -f "$FD/$FF_NAME.tar.xz" ] || curl -sSfL -o "$FD/$FF_NAME.tar.xz" "$FF_URL"
  echo "$FF_SHA256  $FD/$FF_NAME.tar.xz" | sha256sum -c --quiet
  tar -xJf "$FD/$FF_NAME.tar.xz" -C "$FD"
fi
VK_INC="$ROOT/build-kit/vulkan-include"
[ "$PRODUCT" = industrial ] || [ -f "$VK_INC/vulkan/vulkan.hpp" ] || { echo "no $VK_INC (scripts/package.sh makes it)" >&2; exit 1; }
"$DOCKER" image inspect idn-deb-builder:bookworm >/dev/null 2>&1 \
  || "$DOCKER" build -t idn-deb-builder:bookworm -f "$HERE/Dockerfile.builder" "$HERE"
"$DOCKER" run --rm --memory 12g --user "$(id -u):$(id -g)" \
  -e IDN_BUILD="$TAG" -e IDN_BUILD_DATE="${IDN_BUILD_DATE:-$(date '+%Y-%m-%d %H:%M')}" \
  -e IDN_PRODUCT="$PRODUCT" -e CARGO_HOME=/cargo-home -e CARGO_BUILD_JOBS=8 -e HOME=/tmp \
  -v "$IMPL:/src:ro" -v "$K/target:/target" -v "$K/cargo-home:/cargo-home" \
  -v "$FD/$FF_NAME:/ffmpeg:ro" -v "$(mkdir -p "$VK_INC" && echo "$VK_INC"):/vulkan-include:ro" \
  -v "$HERE/in-container.sh:/in-container.sh:ro" \
  idn-deb-builder:bookworm /in-container.sh
# Each product's packages in its own folder: IDN Media's in
# Installers/Media, IDN Industrial's in Installers/Industrial; made together, then moved.
INST="$ROOT/Installers"
OUT="$INST/.debs-$TAG"
rm -rf "$OUT"; mkdir -p "$OUT" "$INST/Media" "$INST/Industrial"
DEBS=()
[ "$PRODUCT" = industrial ] || DEBS+=("Media:idn-media_${VERSION}_amd64.deb")
[ "$PRODUCT" = industrial ] || DEBS+=("Media:idn-site-manager_${VERSION}_amd64.deb")
[ "$PRODUCT" = media ] || DEBS+=("Industrial:idn-industrial_${VERSION}_amd64.deb")
[ "$PRODUCT" = media ] || DEBS+=("Industrial:idn-site-manager-industrial_${VERSION}_amd64.deb")
IDN_PRODUCT="$PRODUCT" "$HERE/make-debs.sh" "$VERSION" "$K/target/release" "$FD/$FF_NAME" "$GUIDE" "$OUT"
for e in "${DEBS[@]}"; do
  dir=${e%%:*} d=${e#*:}
  rm -f "$INST/$dir/${d%%_*}"_*"~$(echo "$TAG" | tr '[:upper:]' '[:lower:]')"_amd64.deb
  mv "$OUT/$d" "$INST/$dir/$d"
  (cd "$INST/$dir" && { grep -v "  $d\$" "SHA256SUMS-$TAG.txt" 2>/dev/null; sha256sum "$d"; } > "SHA256SUMS-$TAG.txt.new" && mv "SHA256SUMS-$TAG.txt.new" "SHA256SUMS-$TAG.txt" && ls -l --block-size=K "$d")
done
rmdir "$OUT" 2>/dev/null || rm -rf "$OUT"
