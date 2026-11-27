#!/bin/bash
# Makes the Windows cross-build kit: everything the IDN Windows build (scripts/package.sh)
# needs that isn't in the repository, in IDN_KIT (default: the folder above this repository,
# build-kit/windows). Safe to run again; it skips what is there.
#   - OBS 32.1.0: headers (from its source) and obs.dll / obs-frontend-api.dll (from the
#     official Windows zip), for idn-obs (IDN_OBS_INCLUDE, IDN_OBS_LIB)
#   - FFmpeg 7.1.1 headers + MinGW import libraries made from OBS's own FFmpeg DLLs: the
#     plugin binds to the FFmpeg that OBS ships (FFMPEG_DIR for the plugin build)
#   - OBS 33.0.1's obs.dll, obs-frontend-api.dll and FFmpeg 8.1 DLLs (obs33-dll), with
#     FFmpeg 8.1.2 headers + import libraries made from those DLLs (ffmpeg-obs33): the
#     plugin's second build, for OBS 33 (media/idn-obs-loader picks one at run time)
#   - FFmpeg 8.1.3 LGPL (BtbN, pinned month-end build, checked): idn-codec's FFmpeg, the
#     one the installer downloads (FFMPEG_DIR for the tools build)
#   - the 4K test files from handoff/setup1 (for the lab's handoff)
#   - out/, where the zip is put together
# Needs: curl, unzip, gendef + x86_64-w64-mingw32-dlltool (mingw-w64-tools).
set -euo pipefail
IMPL="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="$(cd "$IMPL/.." && pwd)"
K="${IDN_KIT:-$ROOT/build-kit/windows}"
mkdir -p "$K/out"
cd "$K"
OBS=32.1.0
FF7=n7.1.1
OBS33=33.0.1
FF8_OBS33=n8.1.2
FF8_FOLDER=ffmpeg-n8.1.3-9-g29e619e767-win64-lgpl-shared-8.1
FF8_URL=https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-09-30-13-08/$FF8_FOLDER.zip
FF8_SHA256=3e47bda1607740550141e37c0e49d1e5182b34699f15adfd137ee266d346811a

fetch() { [ -s "$2" ] || curl -sSLf -o "$2.part" "$1" && { [ -s "$2" ] || mv "$2.part" "$2"; }; }
# A download's SHA-256 against scripts/windows-kit.sha256 (F2-8): a changed file stops here.
pinned() { grep -F "  $1" "$IMPL/scripts/windows-kit.sha256" | sha256sum -c --quiet || { echo "$1 isn't the pinned file (scripts/windows-kit.sha256)" >&2; exit 1; }; }

# OBS
fetch "https://github.com/obsproject/obs-studio/releases/download/$OBS/OBS-Studio-$OBS-Windows-x64.zip" obs-win.zip
fetch "https://github.com/obsproject/obs-studio/archive/refs/tags/$OBS.tar.gz" obs-src.tar.gz
pinned obs-win.zip
pinned obs-src.tar.gz
if [ ! -f include/obs/obs-module.h ]; then
  rm -rf include && mkdir -p include
  tar -xzf obs-src.tar.gz --strip-components=1 -C include --wildcards \
    "obs-studio-$OBS/libobs/*.h" "obs-studio-$OBS/libobs/*.hpp" "obs-studio-$OBS/libobs/util/*" \
    "obs-studio-$OBS/libobs/graphics/*" "obs-studio-$OBS/libobs/media-io/*" \
    "obs-studio-$OBS/libobs/callback/*" "obs-studio-$OBS/frontend/api/obs-frontend-api.h"
  mv include/libobs include/obs
  cp include/frontend/api/obs-frontend-api.h include/obs/
  cat > include/obs/obsconfig.h <<'EOF'
/* What OBS's CMake generates for a Windows build, as far as the public headers need. */
#pragma once

#define OBS_DATA_PATH "../../data"
#define OBS_PLUGIN_PATH "../../obs-plugins/64bit"
#define OBS_PLUGIN_DESTINATION "obs-plugins/64bit"

#define OBS_RELEASE_CANDIDATE 0
#define OBS_BETA 0
EOF
fi
mkdir -p dll
[ -f dll/obs.dll ] || unzip -o -j -q obs-win.zip "bin/64bit/obs.dll" "bin/64bit/obs-frontend-api.dll" -d dll

# FFmpeg 7.1 (OBS's) for the plugin
fetch "https://github.com/FFmpeg/FFmpeg/archive/refs/tags/$FF7.tar.gz" ffmpeg-src.tar.gz
pinned ffmpeg-src.tar.gz
if [ ! -f ffmpeg/.complete ]; then
  rm -rf ffmpeg && mkdir -p ffmpeg/include ffmpeg/lib ffmpeg/dll
  tar -xzf ffmpeg-src.tar.gz --strip-components=1 -C ffmpeg/include --wildcards \
    "FFmpeg-$FF7/libavcodec/*.h" "FFmpeg-$FF7/libavformat/*.h" "FFmpeg-$FF7/libavutil/*.h" \
    "FFmpeg-$FF7/libswscale/*.h" "FFmpeg-$FF7/libswresample/*.h" \
    "FFmpeg-$FF7/libavdevice/*.h" "FFmpeg-$FF7/libavfilter/*.h"
  printf '#ifndef AVUTIL_AVCONFIG_H\n#define AVUTIL_AVCONFIG_H\n#define AV_HAVE_BIGENDIAN 0\n#define AV_HAVE_FAST_UNALIGNED 1\n#endif\n' \
    > ffmpeg/include/libavutil/avconfig.h
  printf '#define FFMPEG_VERSION "%s"\n' "$FF7" > ffmpeg/include/libavutil/ffversion.h
  unzip -o -j -q obs-win.zip "bin/64bit/avcodec-61.dll" "bin/64bit/avformat-61.dll" \
    "bin/64bit/avutil-59.dll" "bin/64bit/swscale-8.dll" "bin/64bit/swresample-5.dll" \
    "bin/64bit/avdevice-61.dll" "bin/64bit/avfilter-10.dll" -d ffmpeg/dll
  (set -e; cd ffmpeg/dll && for d in avcodec-61 avformat-61 avutil-59 swscale-8 swresample-5 avdevice-61 avfilter-10; do
    gendef "$d.dll" >/dev/null 2>&1
    x86_64-w64-mingw32-dlltool -d "$d.def" -D "$d.dll" -l "../lib/lib${d%-*}.dll.a"
  done)
  touch ffmpeg/.complete
fi

# OBS 33 (FFmpeg 8.1.2) for the plugin's second build
fetch "https://github.com/obsproject/obs-studio/releases/download/$OBS33/OBS-Studio-$OBS33-Windows-x64.zip" obs33-win.zip
fetch "https://github.com/FFmpeg/FFmpeg/archive/refs/tags/$FF8_OBS33.tar.gz" ffmpeg812-src.tar.gz
pinned obs33-win.zip
pinned ffmpeg812-src.tar.gz
# Done once all of it is made (a stamp written last: the first import library, the old
# test, was written in the loop's first pass, so a failed or stopped run left it half made
# and was skipped from then on: bug run H).
if [ ! -f ffmpeg-obs33/.complete ]; then
  rm -rf ffmpeg-obs33 obs33-dll && mkdir -p ffmpeg-obs33/include ffmpeg-obs33/lib ffmpeg-obs33/dll obs33-dll
  tar -xzf ffmpeg812-src.tar.gz --strip-components=1 -C ffmpeg-obs33/include --wildcards \
    "FFmpeg-$FF8_OBS33/libavcodec/*.h" "FFmpeg-$FF8_OBS33/libavformat/*.h" "FFmpeg-$FF8_OBS33/libavutil/*.h" \
    "FFmpeg-$FF8_OBS33/libswscale/*.h" "FFmpeg-$FF8_OBS33/libswresample/*.h" \
    "FFmpeg-$FF8_OBS33/libavdevice/*.h" "FFmpeg-$FF8_OBS33/libavfilter/*.h"
  printf '#ifndef AVUTIL_AVCONFIG_H\n#define AVUTIL_AVCONFIG_H\n#define AV_HAVE_BIGENDIAN 0\n#define AV_HAVE_FAST_UNALIGNED 1\n#endif\n' \
    > ffmpeg-obs33/include/libavutil/avconfig.h
  printf '#define FFMPEG_VERSION "%s"\n' "$FF8_OBS33" > ffmpeg-obs33/include/libavutil/ffversion.h
  unzip -o -j -q obs33-win.zip "bin/64bit/avcodec-62.dll" "bin/64bit/avformat-62.dll" \
    "bin/64bit/avutil-60.dll" "bin/64bit/swscale-9.dll" "bin/64bit/swresample-6.dll" \
    "bin/64bit/avdevice-62.dll" "bin/64bit/avfilter-11.dll" -d ffmpeg-obs33/dll
  (set -e; cd ffmpeg-obs33/dll && for d in avcodec-62 avformat-62 avutil-60 swscale-9 swresample-6 avdevice-62 avfilter-11; do
    gendef "$d.dll" >/dev/null 2>&1
    x86_64-w64-mingw32-dlltool -d "$d.def" -D "$d.dll" -l "../lib/lib${d%-*}.dll.a"
  done)
  # What package.sh checks the OBS 33 build's imports against.
  unzip -o -j -q obs33-win.zip "bin/64bit/obs.dll" "bin/64bit/obs-frontend-api.dll" -d obs33-dll
  cp ffmpeg-obs33/dll/*.dll obs33-dll/
  touch ffmpeg-obs33/.complete
fi

# FFmpeg 8.1 (installer's) for idn-codec
mkdir -p ffmpeg8-dl
if [ ! -d "ffmpeg8-dl/$FF8_FOLDER" ]; then
  fetch "$FF8_URL" ffmpeg8-dl/ff.zip
  echo "$FF8_SHA256  ffmpeg8-dl/ff.zip" | sha256sum -c --quiet
  unzip -o -q ffmpeg8-dl/ff.zip -d ffmpeg8-dl
fi

# (No ASIO SDK: IDN's ASIO is its own, core/idn-asio-abi, since 2026-11-25.)

# 4K test files
mkdir -p h264
for f in uhd.h264 uhd_1slice.h264 uhd.mp4; do
  [ -f "h264/$f" ] || cp "$ROOT/handoff/setup1/$f" h264/ 2>/dev/null || echo "note: no h264/$f (from handoff/setup1)"
done
echo "kit ready in $K"
