#!/bin/bash
# Builds a lab build, all named "Build TAG (date)": the Linux handoff (ROOT/handoff/TAG)
# and the downloads, IDN Media's in ROOT/Installers/Media and IDN Industrial's in
# ROOT/Installers/Industrial (overwritten in place):
# (names carry VER-TAG: the crate version then the build tag, e.g. idn-media-setup-0.2.0-rv5-…)
#   idn-media-setup-VER-TAG-x64.exe             IDN Media's installer (programs without symbols;
#                                               idn-setup-… before 2026-11-01)
#   idn-industrial-setup-VER-TAG-x64.exe        IDN Node | Industrial's installer
#   idn-media_VER~tag_amd64.deb, idn-industrial_VER~tag_amd64.deb   the Linux packages
# (No zips, no Docker image, no debug symbols; the lab uses the installers.)
#
#   scripts/package.sh TAG                 both products
#   scripts/package.sh --media TAG         IDN Media only (its installer, .deb, handoff)
#   scripts/package.sh --industrial TAG    IDN Industrial only (Node | Industrial's
#                                          installer and .deb, the Site Manager | Industrial)
#   scripts/package.sh --lab TAG           a lab build: IDN Media's Linux handoff alone
#                                          (no Windows builds, installers, .debs or their
#                                          checks), for a round on the lab's host (not
#                                          every round needs packaging)
# (The two can be built and sent on their own. A product built alone takes its own tag,
# so a Media AlphaN and an Industrial IndN never share a handoff.)
#
# ROOT is the folder above this repository. The Windows kit (IDN_KIT, default
# ROOT/build-kit/windows; build-kit/windows/setup-kit.sh makes it) holds what is never
# in the repository: OBS's headers and DLLs, FFmpeg 7.1 (OBS 32's), OBS 33's FFmpeg 8.1 and 8.1 (BtbN's LGPL
# build), the H.264 test
# clips, and node/, lab/ and setup/ where the downloads are put together. Builds run through IDN_CAP (default
# impl/scripts/dev/capped.sh: a memory cap without swap, NAME MAXMEM JOBS cargo-args);
# with IDN_CAP=none, plain cargo.
set -euo pipefail
# One heavy build on this host at a time (shared lock with the other projects, as
# scripts/dev/test-all.sh).
exec 9>/tmp/heavy-build.lock
if ! flock -n 9; then
  echo "package: waiting for another heavy build (/tmp/heavy-build.lock)" >&2
  flock 9
fi
MEDIA=1 INDUSTRIAL=1 LAB=
case "${1:-}" in
  --media) INDUSTRIAL=; shift ;;
  --lab) INDUSTRIAL=; LAB=1; shift ;;
  --industrial) MEDIA=; shift ;;
esac
TAG="${1:?usage: scripts/package.sh [--media|--industrial|--lab] TAG}"
# Letters and digits only: a '-' split the hash lists' names, a '_' broke dpkg-deb (bug
# round E, E7-10).
case "$TAG" in *[!A-Za-z0-9]*|"") echo "TAG: letters and digits only (Alpha67, Ind13)" >&2; exit 1 ;; esac
PRODUCT=both; [ -z "$INDUSTRIAL" ] && PRODUCT=media; [ -z "$MEDIA" ] && PRODUCT=industrial
export IDN_PRODUCT=$PRODUCT
IMPL="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="$(cd "$IMPL/.." && pwd)"
# The crate version (Cargo.toml) and the build tag both go in every file name, as
# <name>-<VER>-<TAG>-…: the version for the release, the tag to tell builds apart.
VER="$(sed -n 's/^version = "\(.*\)"/\1/p' "$IMPL/core/idn-open-protocol/Cargo.toml" | head -1)"
VT="$VER-$TAG"
K="${IDN_KIT:-$ROOT/build-kit/windows}"
CAP="${IDN_CAP:-$IMPL/scripts/dev/capped.sh}"
if [ "$CAP" = none ]; then
  CAP=cargo_plain
  cargo_plain() { shift 3; cargo "$@"; local rc=$?; echo "cargo exit $rc"; return $rc; }
fi
[ -d "$K/ffmpeg8-dl" ] || { echo "no Windows kit at $K (build-kit/windows/setup-kit.sh)" >&2; exit 1; }
# The OBS plugin's two builds (OBS 32's FFmpeg 7, OBS 33's FFmpeg 8): checked up front,
# not by a link error later (bug run H).
if [ -n "$MEDIA" ]; then
  for d in ffmpeg/lib ffmpeg-obs33/lib obs33-dll; do
    [ -d "$K/$d" ] || { echo "the Windows kit has no $d: run scripts/setup-windows-kit.sh" >&2; exit 1; }
  done
fi
export IDN_BUILD="$TAG"
# idn-release (YubiKeys over PC/SC) links libpcsclite: build-kit/pcsc has the .so link
# when the system has only libpcsclite.so.1.
[ -z "${PCSC_LIB_DIR:-}" ] && [ -d "$ROOT/build-kit/pcsc" ] && export PCSC_LIB_DIR="$ROOT/build-kit/pcsc"
# Captions on the GPU (idn-captions' gpu feature) build against the Vulkan headers and
# glslc (Ubuntu: glslc, libvulkan-dev). The headers alone, in a folder of their own: the
# MinGW build must not see /usr/include.
VK_INC="$ROOT/build-kit/vulkan-include"
if [ -n "$MEDIA" ] && [ ! -f "$VK_INC/vulkan/vulkan.hpp" ]; then
  command -v glslc >/dev/null && [ -f /usr/include/vulkan/vulkan.hpp ] || {
    echo "no Vulkan headers or glslc (apt install glslc libvulkan-dev)" >&2; exit 1; }
  mkdir -p "$VK_INC" && cp -r /usr/include/vulkan /usr/include/vk_video "$VK_INC/"
fi
export Vulkan_INCLUDE_DIR="$VK_INC"
export IDN_BUILD_DATE="$(date '+%Y-%m-%d %H:%M')"
LINE="Build $TAG ($IDN_BUILD_DATE)"
case $PRODUCT in media) LINE="IDN Media $LINE" ;; industrial) LINE="IDN Industrial $LINE" ;; esac
cd "$IMPL"
# The errors and cargo's exit, shown; fails unless cargo said "cargo exit 0" (a failed
# build packaged the old binaries under the new tag, and head could cut grep off under
# pipefail: review 2026-11-01 F13).
quiet() {
  local out
  out="$(grep -E "^error|cargo exit" || true)"
  printf '%s\n' "$out" | sed -n '1,3p'
  grep -q "cargo exit 0" <<<"$out"
}
# The programs that draw on the GPU (the switcher, idn-display) turn on the library's `gpu`
# feature (§E.27). Built in a cargo run of their own: in one run, cargo gives every
# program the library with it, and each grew by wgpu's 2 MB for nothing (Alpha24).
# idn-codec too: its screen capture makes pictures planar on the GPU while receivers take
# them there (efficiency #4, 2026-11-02).
GPU_PROGRAMS=(-p idn-switcher -p idn-display -p idn-ffmpeg --features idn-ffmpeg/gpu,idn-ffmpeg/sound)
F8=$K/ffmpeg8-dl/ffmpeg-n8.1.3-9-g29e619e767-win64-lgpl-shared-8.1
if [ -n "$MEDIA" ]; then
$CAP pkg-linux 12G 8 build --release --workspace --exclude idn-switcher --exclude idn-display --exclude idn-ffmpeg 2>&1 | quiet
# With va_gpu_check: VA-API pictures kept on the GPU against downloaded ones (lab 321 (4)).
$CAP pkg-linux-gpu 12G 8 build --release "${GPU_PROGRAMS[@]}" --bins --example va_gpu_check 2>&1 | quiet
if [ -z "$LAB" ]; then
# Windows: the plugin binds to OBS 32's FFmpeg 7.1 (and below, OBS 33's 8.1); the tools build first with 7.1 too.
FFMPEG_DIR=$K/ffmpeg IDN_OBS_INCLUDE=$K/include IDN_OBS_LIB=$K/dll \
  $CAP pkg-win 12G 8 build --release --target x86_64-pc-windows-gnu \
  -p idn-node -p idn-obs -p idn-obs-loader -p idn-open-protocol -p idn-ffmpeg -p idn-audio -p idn-mixer -p idn-plugin-host -p idn-audio-plugins -p idn-cli -p idn-asio -p idn-camera-source -p idn-release -p idn-aoip --bins --lib 2>&1 \
  | quiet
# The plugin again for OBS 33, which ships FFmpeg 8.1 (avcodec-62.dll): its headers and
# import libraries made from OBS 33's own DLLs (the kit's ffmpeg-obs33). idn_obs.dll is
# then media/idn-obs-loader, which loads the build for the OBS it runs in (2026-11-12:
# the FFmpeg 7 build alone didn't load in OBS 33).
CARGO_TARGET_DIR=target/win-obs-ff8 FFMPEG_DIR=$K/ffmpeg-obs33 IDN_OBS_INCLUDE=$K/include IDN_OBS_LIB=$K/dll \
  $CAP pkg-win-obs8 12G 8 build --release --target x86_64-pc-windows-gnu -p idn-obs 2>&1 | quiet
# Each build against the OBS it is for: every OBS and FFmpeg function it imports there.
scripts/dev/obs-imports-check.py target/x86_64-pc-windows-gnu/release/idn_obs.dll $K/dll $K/ffmpeg/dll \
  || { echo "the OBS plugin's FFmpeg 7 build doesn't fit OBS 32" >&2; exit 1; }
scripts/dev/obs-imports-check.py target/win-obs-ff8/x86_64-pc-windows-gnu/release/idn_obs.dll $K/obs33-dll \
  || { echo "the OBS plugin's FFmpeg 8 build doesn't fit OBS 33" >&2; exit 1; }
$CAP pkg-win-gpu 12G 8 build --release --target x86_64-pc-windows-gnu -p idn-display --bins 2>&1 | quiet
# idn-codec binds to FFmpeg 8.1, which the installer downloads (BtbN's LGPL build, pinned).
CARGO_TARGET_DIR=target/win-ffmpeg8 FFMPEG_DIR=$F8 \
  $CAP pkg-win8 12G 8 build --release --target x86_64-pc-windows-gnu -p idn-ffmpeg --features gpu,sound --bins --examples 2>&1 | quiet
CARGO_TARGET_DIR=target/win-ffmpeg8 FFMPEG_DIR=$F8 \
  $CAP pkg-win8-gpu 12G 8 build --release --target x86_64-pc-windows-gnu -p idn-switcher --bins --examples 2>&1 | quiet
fi # (not a lab build)
fi
if [ -n "$INDUSTRIAL" ]; then
# IDN Node | Industrial for Windows, with the `capture` feature (remote-support screen
# capture + H.264), so it binds to FFmpeg 8.1 like idn-codec. The installer downloads it.
# The session helper is built with it too: it captures the interactive desktop in-session
# (the service in session 0 can't), so it needs FFmpeg as well.
# Only the session helper captures (the service, in session 0, can't): the Node and the
# Site Manager are built without FFmpeg, so they run without it (a data-only install).
CARGO_TARGET_DIR=target/win-ffmpeg8 FFMPEG_DIR=$F8 \
  $CAP pkg-win8-ind 12G 8 build --profile industrial --target x86_64-pc-windows-gnu -p idn-industrial --features capture --bin idn-helper-industrial 2>&1 | quiet
# (The `industrial` profile: link-time optimised for size, 22 MB → 6.5 MB a program.)
$CAP pkg-win-ind 12G 8 build --profile industrial --target x86_64-pc-windows-gnu -p idn-industrial --bin idn-industrial --bin idn-site-manager-industrial 2>&1 | quiet
# IDN Voice's desktop app (§V.11), for the plant's PCs too.
$CAP pkg-win-ind 12G 8 build --profile industrial --target x86_64-pc-windows-gnu -p idn-voice-app 2>&1 | quiet
$CAP pkg-ind 12G 8 build --release -p idn-industrial --bins --example mqtt_watch --example modbus_sim 2>&1 | quiet
fi
if [ -n "$MEDIA" ]; then
# The lab tools' old builds gone first: one that fails to build isn't shipped under the
# new tag from an earlier one (bug round E, E7-9; they are built `|| true`).
rm -f target/release/examples/{decode_bench,compose_bench,avsync,stream_log,beep_offset,audio_chain,gpu_recv,mqtt}
[ -n "$LAB" ] || $CAP pkg-audio 12G 8 build --release --target x86_64-pc-windows-gnu -p idn-audio --examples 2>&1 | quiet || true
$CAP pkg-bench 12G 8 build --release -p idn-ffmpeg --example decode_bench 2>&1 | quiet || true
$CAP pkg-bench 12G 8 build --release -p idn-switcher --example compose_bench 2>&1 | quiet || true
# Timing tools for the lab: sound against pictures (avsync), a stream's blocks and steps
# (stream_log), what a bridge adds (beep_offset), the whole sound chain on one machine (audio_chain).
$CAP pkg-tools 12G 8 build --release -p idn-open-protocol --example avsync --example stream_log --example beep_offset --example audio_chain 2>&1 | quiet || true
# Pictures GPU to GPU, measured (lab 317).
$CAP pkg-tools 12G 8 build --release -p idn-open-protocol --features gpu --example gpu_recv 2>&1 | quiet || true
# An MQTT client for the lab (sub and pub: the site's broker, §N.34; the switcher's, §S.33).
$CAP pkg-tools 12G 8 build --release -p idn-mqtt --example mqtt 2>&1 | quiet || true
fi

# Linux handoff
H="$ROOT/handoff/$TAG"
mkdir -p "$H"
if [ -n "$MEDIA" ]; then
for b in idn idn-node idn-codec idn-switcher idn-plugin-host idn-session idn-display idn-site-manager; do
  [ -f target/release/$b ] && cp target/release/$b "$H/"
done
# The tools' old names: `idn` under that name, which runs that command (R.17).
for b in idn-channel idn-discovery-server idn-doctor idn-dsctl idn-find idn-publish idn-recv idn-send idn-audio idn-mixer; do ln -sfn idn "$H/$b"; done
cp target/release/libidn_obs.so "$H/"
# The audio editors' plugins (N8): one library, as IDN.clap and inside IDN.vst3.
cp target/release/libidn_audio_plugins.so "$H/IDN.clap"
rm -rf "$H/IDN.vst3" && mkdir -p "$H/IDN.vst3/Contents/x86_64-linux"
cp target/release/libidn_audio_plugins.so "$H/IDN.vst3/Contents/x86_64-linux/IDN.so"
for x in decode_bench compose_bench avsync stream_log beep_offset audio_chain gpu_recv; do
  if [ -f "target/release/examples/$x" ]; then cp "target/release/examples/$x" "$H/"; else echo "package.sh: $x didn't build (left out)" >&2; fi
done
[ -f target/release/examples/mqtt ] && cp target/release/examples/mqtt "$H/"
[ -f target/release/examples/va_gpu_check ] && cp target/release/examples/va_gpu_check "$H/"
cp $K/h264/uhd.h264 $K/h264/uhd_1slice.h264 $K/h264/uhd.mp4 "$H/"
cp media/idn-node/README.md "$H/idn-node-README.md"; cp media/idn-obs/README.md "$H/idn-obs-README.md"; cp media/idn-audio/README.md "$H/idn-audio-README.md"
cp media/idn-switcher/README.md "$H/idn-switcher-README.md"; cp media/idn-mixer/README.md "$H/idn-mixer-README.md"
# The network audio device (IDN's own engine, MIT OR Apache-2.0).
cp target/release/idn-aoip "$H/"
# The user guide as pages (G7): guide/index.html.
rm -rf "$H/guide"
$CAP pkg-guide 4G 4 run -q --release -p idn-guide -- docs/guide "$H/guide"
fi
cp notes/LAB-TESTS.md "$H/"
# IDN Industrial: the two programs, the MQTT watch tool, the systemd unit, the Docker
# files and the image itself.
if [ -n "$INDUSTRIAL" ]; then
cp target/release/idn-site-manager-industrial target/release/idn-industrial target/release/examples/mqtt_watch target/release/examples/modbus_sim "$H/"
cp industrial/idn-industrial/README.md "$H/idn-industrial-README.md"
cp installer/linux/idn-industrial.service "$H/"
fi
echo "$LINE" > "$H/BUILD.txt"
# Every file, the guide's pages too (guide/…).
(cd "$H" && find . -type f ! -name SHA256SUMS | sed 's|^\./||' | LC_ALL=C sort | xargs -d '\n' sha256sum > SHA256SUMS)
# A lab build: the handoff is all.
if [ -n "$LAB" ]; then
  echo "lab build $TAG: $H ($(wc -l < "$H/SHA256SUMS") files)"
  exit 0
fi

# Windows: the Node's programs (what the installer installs), the lab tools, the installer.
W=target/x86_64-pc-windows-gnu/release
W8=target/win-ffmpeg8/x86_64-pc-windows-gnu/release
INST="$ROOT/Installers"
OM="$INST/Media" OI="$INST/Industrial"
mkdir -p "$OM" "$OI"
# Drop every earlier file for THIS tag in the product's folder (any version), so one build
# per tag replaces itself when the version bumps; other tags stay side by side.
[ -z "$MEDIA" ] || rm -f "$OM"/idn-*-"$TAG"-*.exe
[ -z "$INDUSTRIAL" ] || rm -f "$OI"/idn-*-"$TAG"-*.exe
if [ -n "$MEDIA" ]; then
NODE=$K/node SETUP=$K/setup
rm -rf "$NODE" "$K/lab" "$SETUP"
mkdir -p "$NODE" "$SETUP"
# The kit's third-party files (DXC, VB-CABLE, the virtual drivers) as pinned (F2-8).
(cd "$K" && grep -v '^#' "$IMPL/scripts/windows-kit.sha256" | sha256sum -c --quiet) \
  || { echo "a kit file isn't the pinned one (scripts/windows-kit.sha256)" >&2; exit 1; }
cp $W/idn-node.exe $W/idn-session.exe $W/idn.exe $W/idn-plugin-host.exe $W/idn-display.exe $W/idn_asio.dll $W/idn_camera.dll "$NODE/"
# The network audio device: IDN's own engine (MIT OR Apache-2.0) as idn-aoip.exe, the
# name `idn aoip` and the Node start.
cp $W/idn-aoip.exe "$NODE/idn-aoip.exe"
# The OBS plugin: the loader as idn_obs.dll, and the two builds it chooses from (the
# installer puts them in OBS's data\obs-plugins\idn_obs\bin).
cp $W/idn_obs_loader.dll "$NODE/idn_obs.dll"
cp $W/idn_obs.dll "$NODE/idn_obs_ffmpeg7.dll"
cp target/win-obs-ff8/x86_64-pc-windows-gnu/release/idn_obs.dll "$NODE/idn_obs_ffmpeg8.dll"
# The audio editors' plugins (N8): the installer puts this library in the CLAP and VST3
# folders as IDN.clap and IDN.vst3.
cp $W/idn_audio_plugins.dll "$NODE/"
cp $W8/idn-codec.exe $W8/idn-switcher.exe "$NODE/"
# Direct3D 12's shader compiler: Microsoft's DXC (dxcompiler.dll, LLVM/NCSA licence;
# release v1.9.2609 in build-kit/windows/dxc), loaded by wgpu from beside the programs.
# Without it wgpu takes the old FXC, which couldn't compile the SpeedHQ encoder (and the
# switcher composited on the CPU). Not dxil.dll: wgpu's DXC doesn't need it.
DXC="$K/dxc/x"
cp "$DXC/bin/x64/dxcompiler.dll" "$NODE/"
cp "$DXC/LICENSE-LLVM.txt" "$NODE/dxcompiler-LICENSE.txt"
# VB-Audio's VB-CABLE (for the IDN Microphone on Windows), its package AS IS (VB-Audio's
# terms: credited, donationware), in build-kit/windows/vbcable; the installer asks to run
# its setup, the Node's panel offers it later.
VBC="$K/vbcable/x"
[ -f "$VBC/VBCABLE_Setup_x64.exe" ] || { echo "no VB-CABLE package in $VBC (build-kit/windows/vbcable)" >&2; exit 1; }
cp -r "$VBC" "$NODE/vbcable"
# VirtualDrivers' Virtual Display Driver and Virtual Audio Driver, MIT, signed by the
# SignPath Foundation, as released (build-kit/windows/vdd: 25.7.23; vad: 25.7.14): the
# Node installs them on demand (idn-node.md §N.36, §N.37).
VDD="$K/vdd/x"; VAD="$K/vad/x"
[ -f "$VDD/VirtualDisplayDriver/MttVDD.inf" ] || { echo "no Virtual Display Driver in $VDD" >&2; exit 1; }
[ -f "$VAD/Virtual Audio Driver/VirtualAudioDriver.inf" ] || { echo "no Virtual Audio Driver in $VAD" >&2; exit 1; }
mkdir -p "$NODE/drivers/display" "$NODE/drivers/audio"
cp "$VDD/VirtualDisplayDriver/"* "$NODE/drivers/display/"
cp "$VDD/LICENSE.txt" "$NODE/drivers/display/LICENSE.txt"
cp "$VAD/Virtual Audio Driver/"* "$NODE/drivers/audio/"
cp "$VAD/LICENSE.txt" "$NODE/drivers/audio/LICENSE.txt"
cp installer/windows/README-windows.txt "$NODE/README-windows.txt"
cp media/idn-node/README.md "$NODE/idn-node-README.md"
cp media/idn-obs/README.md "$NODE/idn-obs-README.md"
cp media/idn-audio/README.md "$NODE/idn-audio-README.md"
cp media/idn-mixer/README.md "$NODE/idn-mixer-README.md"
cp media/idn-switcher/README.md "$NODE/idn-switcher-README.md"
cp -r "$H/guide" "$NODE/guide"
echo "$LINE" > "$NODE/BUILD.txt"
# The read-me's first line is the build line (replacing an earlier one).
python3 - "$NODE/README-windows.txt" "$LINE" <<'PY'
import sys
p, line = sys.argv[1], sys.argv[2]
lines = open(p).read().split("\n")
if lines and lines[0].startswith("Build "):
    lines = lines[1:]
open(p, "w").write("\n".join([line] + lines))
PY
# The installer from the same programs without their symbols (Microsoft's signed loader
# as it is).
cp -r "$NODE"/* "$SETUP/"
for f in "$SETUP"/*.exe "$SETUP"/*.dll; do
  case "$(basename "$f")" in WebView2Loader.dll|dxcompiler.dll) continue ;; esac
  x86_64-w64-mingw32-strip --strip-debug --strip-unneeded "$f"
done
# FFmpeg 8 bundled into the installer (offline install); build.sh turns on -DBUNDLE_FFMPEG
# when it sees these. (Not stripped — they are BtbN's prebuilt LGPL DLLs.)
mkdir -p "$SETUP/ffmpeg-dll"
cp "$F8"/bin/*.dll "$SETUP/ffmpeg-dll/"
# The ffmpeg and ffprobe programs too (IDN Data's downloads and library, idn-data.md).
cp "$F8"/bin/ffmpeg.exe "$F8"/bin/ffprobe.exe "$SETUP/ffmpeg-dll/"
cp "$F8/LICENSE.txt" "$SETUP/ffmpeg-dll/"
installer/windows/build.sh "$SETUP" "$SETUP/idn-media-setup-x64.exe"
cp "$SETUP/idn-media-setup-x64.exe" "$OM/idn-media-setup-$VT-x64.exe"
fi
# IDN Node | Industrial: its program (built with capture against FFmpeg 8), stripped,
# and its own installer.
if [ -n "$INDUSTRIAL" ]; then
IND=$K/industrial
rm -rf "$IND"; mkdir -p "$IND"
WI=target/x86_64-pc-windows-gnu/industrial
WI8=target/win-ffmpeg8/x86_64-pc-windows-gnu/industrial
cp $WI/idn-industrial.exe $WI8/idn-helper-industrial.exe $WI/idn-site-manager-industrial.exe "$IND/"
cp $WI/idn-voice.exe "$IND/"
cp "$(ls -t $WI/build/webview2-com-sys-*/out/x64/WebView2Loader.dll | head -1)" "$IND/"
x86_64-w64-mingw32-strip --strip-debug --strip-unneeded "$IND/idn-industrial.exe" "$IND/idn-helper-industrial.exe" "$IND/idn-site-manager-industrial.exe" "$IND/idn-voice.exe"
# FFmpeg 8 bundled into the industrial installer too (plant PCs are often offline).
# Only the libraries the helper loads (avcodec, avformat, avutil, swresample, swscale):
# not avfilter and avdevice (36 MB).
mkdir -p "$IND/ffmpeg-dll"
cp "$F8"/bin/avcodec-*.dll "$F8"/bin/avformat-*.dll "$F8"/bin/avutil-*.dll \
  "$F8"/bin/swresample-*.dll "$F8"/bin/swscale-*.dll "$IND/ffmpeg-dll/"
cp "$F8/LICENSE.txt" "$IND/ffmpeg-dll/"
installer/windows/industrial/build.sh "$IND" "$IND/idn-industrial-setup-x64.exe"
cp "$IND/idn-industrial-setup-x64.exe" "$OI/idn-industrial-setup-$VT-x64.exe"
fi
# Keep only the five most recent builds of each product in Installers; older ones go
# with their Linux package. Each build writes one idn-media-setup-<ver>-<tag>-x64.exe
# (idn-setup-… before the rename) and/or one idn-industrial-setup-<ver>-<tag>-x64.exe, so
# those mark the builds. A tag's hash list goes when neither product has it any more.
prune() { # DIR PREFIX DEB-NAME...
  local OUT=$1 prefix=$2 exe vt deb; shift 2
  mapfile -t STALE < <(ls -t "$OUT"/"$prefix"-*-x64.exe 2>/dev/null | tail -n +6)
  for exe in "${STALE[@]:-}"; do
    [ -n "$exe" ] || continue
    vt=$(basename "$exe"); vt=${vt#"$prefix"-}; vt=${vt%-x64.exe}   # e.g. 0.2.0-rv5
    echo "pruning older $prefix $vt (keeping the latest 5)"
    rm -f "${exe:?}"
    # (The Linux packages' names have the version as Debian writes it: 0.2.0~rv5.)
    for deb in "$@"; do
      rm -f "${OUT:?}"/"$deb"_"${vt%-*}~$(echo "${vt##*-}" | tr '[:upper:]' '[:lower:]')"_amd64.deb
    done
    ls "$OUT"/idn-*-"$vt"-x64.exe >/dev/null 2>&1 || rm -f "${OUT:?}/SHA256SUMS-${vt##*-}.txt"
  done
}
[ -z "$MEDIA" ] || prune "$OM" idn-media-setup idn-media idn-site-manager
[ -z "$INDUSTRIAL" ] || prune "$OI" idn-industrial-setup idn-industrial idn-site-manager-industrial
# The single zip of earlier builds is replaced by these.
rm -f "$ROOT/idn-windows-x64.zip"
echo "$LINE"
echo "handoff: $H"
(cd "$H" && grep -E " idn$| idn-node$| idn-mixer$| idn-switcher$| idn-plugin-host$| idn-audio$|libidn_obs|industrial" SHA256SUMS | cut -c1-16,65-)
# The downloads' full hashes beside them (the lab checks against this list).
[ -z "$MEDIA" ] || (cd "$OM" && sha256sum "idn-media-setup-$VT-x64.exe" > "SHA256SUMS-$TAG.txt")
[ -z "$INDUSTRIAL" ] || (cd "$OI" && sha256sum "idn-industrial-setup-$VT-x64.exe" > "SHA256SUMS-$TAG.txt")
# The Linux packages (Debian 12, Ubuntu 24.04 and 26.04): built in a Debian 12 container
# and installed on each of the three to check them (N.12.3). IDN_NO_DEB=1 skips them.
if [ -z "${IDN_NO_DEB:-}" ]; then
  installer/linux/deb/build.sh "$TAG"
  installer/linux/deb/test.sh "$INST" "$VER~$(echo "$TAG" | tr '[:upper:]' '[:lower:]')" | grep -E "^==|^ok|not found|rror"
fi
for d in ${MEDIA:+"$OM"} ${INDUSTRIAL:+"$OI"}; do
  (cd "$d" && echo "$d:" && cut -c1-16,65- "SHA256SUMS-$TAG.txt" && ls -l --block-size=K ./*"$(echo "$TAG" | tr '[:upper:]' '[:lower:]')"* ./*"$TAG"* 2>/dev/null | sort -u)
done
