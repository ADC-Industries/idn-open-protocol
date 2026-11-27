#!/bin/bash
# Inside the builder (installer/linux/deb/build.sh starts it): the programs for the Linux
# packages, against Debian 12's C library and the bundled FFmpeg (/ffmpeg), into
# /target. The programs look for FFmpeg beside them first ($ORIGIN/../lib).
set -euo pipefail
cd /src
export FFMPEG_DIR=/ffmpeg Vulkan_INCLUDE_DIR=/vulkan-include
export CARGO_TARGET_DIR=/target IDN_BUILD="$IDN_BUILD" IDN_BUILD_DATE="$IDN_BUILD_DATE"
export RUSTFLAGS="-C link-arg=-Wl,-rpath,\$ORIGIN/../lib"
# As package.sh: the GPU programs in a cargo run of their own (wgpu stays out of the rest).
# Not here: the release tool (YubiKeys, a desk-side tool) and the OBS plugin (it links
# each system's own OBS and FFmpeg).
# (idn-codec too: its screen capture converts on the GPU, efficiency #4.)
PRODUCT="${IDN_PRODUCT:-both}"
if [ "$PRODUCT" = industrial ]; then
  cargo build --locked --release -p idn-industrial --bin idn-industrial --bin idn-site-manager-industrial
else
  cargo build --locked --release --workspace --exclude idn-switcher --exclude idn-display \
    --exclude idn-ffmpeg --exclude idn-release --exclude idn-obs
  cargo build --locked --release -p idn-switcher -p idn-display -p idn-ffmpeg --features idn-ffmpeg/gpu,idn-ffmpeg/sound
fi
# What the packages' programs need of the system, by Debian's names (Ubuntu's renamed
# libraries provide them): dpkg-shlibdeps over our programs. Not over FFmpeg's libraries:
# BtbN's need only glibc 2.28, but call a few of its private resolver functions, for which
# dpkg would pin glibc to exactly this version (test.sh checks they load on each system).
mkdir -p /target/work/debian && cd /target/work
printf 'Source: idn-media\n\nPackage: idn-media\nArchitecture: amd64\n' > debian/control
printf '%s\n' 'libavcodec 62 idn-media' 'libavformat 62 idn-media' 'libavutil 60 idn-media' \
  'libswscale 9 idn-media' 'libswresample 6 idn-media' > debian/shlibs.local
R=/target/release
deps() { # OUT PROGRAM...
  local out=$1; shift
  # (Its warnings, about the run path among others, in a file; errors shown.)
  dpkg-shlibdeps -O -xidn-media -l/ffmpeg/lib "$@" 2>/target/work/shlibdeps.log \
    | sed 's/^shlibs:Depends=//' > "$out" || { cat /target/work/shlibdeps.log; exit 1; }
  echo "$(basename "$out"): $(cat "$out")"
}
[ "$PRODUCT" = industrial ] || deps $R/DEPENDS $R/idn $R/idn-node $R/idn-codec $R/idn-switcher $R/idn-plugin-host \
  $R/idn-session $R/idn-display $R/idn-aoip $R/libidn_audio_plugins.so $R/libidn_asio.so
[ "$PRODUCT" = industrial ] || deps $R/DEPENDS-site-manager $R/idn-site-manager
[ "$PRODUCT" = media ] || deps $R/DEPENDS-industrial $R/idn-industrial
[ "$PRODUCT" = media ] || deps $R/DEPENDS-site-manager-industrial $R/idn-site-manager-industrial
