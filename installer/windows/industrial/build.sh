#!/bin/sh
# Builds the IDN Node | Industrial installer with NSIS 3:
#   installer/windows/industrial/build.sh DIR OUT.exe
# DIR holds idn-industrial.exe; the icon, read-me and licence text are added here.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
impl="$here/../../.."
dir="$(cd "$1" && pwd)"
out="$2"
case "$out" in /*) ;; *) out="$(pwd)/$out" ;; esac
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$impl/industrial/idn-industrial/Cargo.toml" | head -1)"
cp "$here/idn-industrial.ico" "$dir/idn-industrial.ico"
sed 's/$/\r/' "$here/README-industrial.txt" > "$dir/README-industrial.txt"
{
    printf 'IDN Node | Industrial, by ADC Industries, under the MIT License or the Apache License 2.0, at your choice.\n'
    printf '\n\n======================================== MIT License\n\n'
    cat "$impl/LICENSE-MIT"
    printf '\n\n======================================== Apache License 2.0\n\n'
    cat "$impl/LICENSE-APACHE"
} | sed 's/$/\r/' > "$dir/LICENSE.txt"
# Bundle FFmpeg when its DLLs are staged (dir/ffmpeg-dll); else the installer downloads it.
bundle=""
[ -f "$dir/ffmpeg-dll/avcodec-62.dll" ] && bundle="-DBUNDLE_FFMPEG"
makensis -V2 -DSRC="$dir" -DBUILD="${IDN_BUILD:-dev}" -DVERSION="$version" -DOUT="$out" $bundle \
    "$here/idn-industrial-setup.nsi"
