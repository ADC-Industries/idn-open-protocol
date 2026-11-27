#!/bin/sh
# Builds the Windows installer with NSIS 3 (apt install nsis):
#   installer/windows/build.sh DIR OUT.exe
# DIR holds the Windows programs and read-mes as the Windows zip has them; the icon and the
# licence text are added here. IDN_BUILD names the build (as for the programs themselves).
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
impl="$here/../.."
dir="$(cd "$1" && pwd)"
out="$2"
case "$out" in /*) ;; *) out="$(pwd)/$out" ;; esac
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$impl/media/idn-node/Cargo.toml" | head -1)"
cp "$impl/media/idn-node/assets/idn.ico" "$dir/idn.ico"
{
    cat "$here/LICENSE-intro.txt"
    printf '\n\n======================================== MIT License\n\n'
    cat "$impl/LICENSE-MIT"
    printf '\n\n======================================== Apache License 2.0\n\n'
    cat "$impl/LICENSE-APACHE"
    printf '\n\n======================================== Open Media Transport (libomtnet, libvmx), MIT License\n\n'
    cat "$impl/vendor/libvmx/LICENSE.txt"
    printf '\n\n======================================== cwASIO (the ASIO interface definitions), MIT License\n\n'
    sed -n '/^MIT License/,$p' "$impl/core/idn-asio-abi/NOTICE-cwASIO.txt"
} | sed 's/$/\r/' > "$dir/LICENSE.txt"
# Bundle FFmpeg into the installer when its DLLs are staged (dir/ffmpeg-dll); otherwise the
# installer downloads it at install time.
bundle=""
[ -f "$dir/ffmpeg-dll/avcodec-62.dll" ] && bundle="-DBUNDLE_FFMPEG"
makensis -V2 -DSRC="$dir" -DBUILD="${IDN_BUILD:-dev}" -DVERSION="$version" -DOUT="$out" $bundle \
    "$here/idn-setup.nsi"
