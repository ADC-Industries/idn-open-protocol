#!/bin/bash
# The release of a build (G10, spec/extensions/idn-node.md §N.23), after
# scripts/package.sh TAG: one file the Site Manager takes, with
#   linux-x86_64    a .tar.gz of the IDN programs (unpacked over a machine's programs folder)
#   windows-x86_64  the installer (run with /S /UPDATE)
# signed with ADC Industries' release key, which is on a YubiKey:
#
#   scripts/release.sh TAG ["What is new"]
#     makes ROOT/Installers/Media/idn-release-VER-TAG/ (Industrial/ for --industrial): the two packages, idn-release.exe and
#     sign.cmd. On the Windows PC: run sign.cmd with the YubiKey plugged in (its PIN, then
#     touch it), or drag the key file onto it (its passphrase): idn-media-VER-TAG.idnrelease.
#     (make-key.cmd makes a key on a YubiKey, once.)
#   scripts/release.sh TAG --key KEYFILE ["What is new"]
#     signs here with a key in a file (tests).
#   scripts/release.sh --industrial TAG [--key KEYFILE] ["What is new"]
#     IDN Industrial's (after scripts/package.sh --industrial TAG): its installer and its
#     Linux program, `--product industrial` (only IDN Industrial's machines take it):
#     idn-industrial-VER-TAG.idnrelease, for the Site Manager | Industrial's Updates page.
#
# Machines install a release only if its key's public half is in release-keys.txt of the
# build they run.
set -euo pipefail
PRODUCT=media
if [ "${1:-}" = "--industrial" ]; then PRODUCT=industrial; shift; fi
TAG="${1:?usage: scripts/release.sh [--industrial] TAG [--key KEYFILE] [NOTES]}"
shift
KEY=""
if [ "${1:-}" = "--key" ]; then KEY="${2:?--key KEYFILE}"; shift 2; fi
NOTES="${1:-}"
IMPL="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="$(cd "$IMPL/.." && pwd)"
VER="$(sed -n 's/^version = "\(.*\)"/\1/p' "$IMPL/core/idn-open-protocol/Cargo.toml" | head -1)"
VT="$VER-$TAG"
H="$ROOT/handoff/$TAG"
# Each product's downloads in its own folder.
if [ "$PRODUCT" = industrial ]; then OUT="$ROOT/Installers/Industrial"; else OUT="$ROOT/Installers/Media"; fi
if [ "$PRODUCT" = industrial ]; then
  NAME=idn-industrial WIN="$OUT/idn-industrial-setup-$VT-x64.exe"
else
  NAME=idn-media WIN="$OUT/idn-media-setup-$VT-x64.exe"
fi
[ -d "$H" ] || { echo "no handoff for $TAG: run scripts/package.sh $TAG first" >&2; exit 1; }
[ -f "$WIN" ] || { echo "no installer $WIN: run scripts/package.sh $TAG first" >&2; exit 1; }
# The programs a Linux machine runs (the lab's tools and clips stay out).
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/programs"
# The tools' old names are links to `idn` (kept as links).
if [ "$PRODUCT" = industrial ]; then
  # The Node's program (a machine installed from the Linux package updates with apt).
  cp "$H/idn-industrial" "$H/BUILD.txt" "$STAGE/programs/"
else
# (Every program package.sh puts in the handoff: one left out stays at its old version
# after an update, speaking a request this Node no longer has.)
for b in idn idn-node idn-channel idn-discovery-server idn-doctor idn-dsctl idn-find idn-publish idn-recv idn-send idn-audio idn-codec idn-switcher idn-mixer idn-plugin-host idn-session idn-display idn-site-manager BUILD.txt; do
  [ -e "$H/$b" ] && cp -P "$H/$b" "$STAGE/programs/"
done
cp "$H"/*-README.md "$STAGE/programs/" 2>/dev/null || true
fi
LIN="$STAGE/$NAME-$VT-linux-x86_64.tar.gz"
(cd "$STAGE/programs" && tar -czf "$LIN" .)
if [ -n "$KEY" ]; then
  "$IMPL/target/release/idn-release" make --key "$KEY" --product "$PRODUCT" --version "$VT" --notes "$NOTES" \
    --package "linux-x86_64=$LIN" --package "windows-x86_64=$WIN" "$OUT/$NAME-$VT.idnrelease"
  exit
fi
# For the Windows PC with the YubiKey: the packages, the tool and what to double-click.
REL="$OUT/$NAME-release-$VT"
rm -rf "$REL"; mkdir -p "$REL"
cp "$LIN" "$WIN" "$REL/"
EXE="$IMPL/target/x86_64-pc-windows-gnu/release/idn-release.exe"
[ -f "$EXE" ] || { echo "no $EXE: cargo build --release --target x86_64-pc-windows-gnu -p idn-release" >&2; exit 1; }
cp "$EXE" "$REL/"
x86_64-w64-mingw32-strip --strip-debug --strip-unneeded "$REL/idn-release.exe"
# The notes inside cmd's quotes: no `"` (made `'`), `%` doubled (cmd expanded "50% to
# 70%" to "50", and that was signed), no line breaks (they split the command; bug review 3).
NOTES_CMD="${NOTES//\"/\'}"
NOTES_CMD="${NOTES_CMD//%/%%}"
NOTES_CMD="${NOTES_CMD//$'\r'/ }"
NOTES_CMD="${NOTES_CMD//$'\n'/ }"
printf '%s\r\n' '@echo off' \
  'rem Signs this release: with the YubiKey plugged in (its PIN, then touch it), or drag' \
  'rem the key file (from the USB stick) onto this (its passphrase).' \
  'cd /d "%~dp0"' \
  'if "%~1"=="" (set SIGNWITH=--card) else (set SIGNWITH=--key "%~1")' \
  "idn-release.exe make %SIGNWITH% --product $PRODUCT --version $VT --notes \"$NOTES_CMD\" --package linux-x86_64=$(basename "$LIN") --package windows-x86_64=$(basename "$WIN") $NAME-$VT.idnrelease" \
  'if errorlevel 1 (echo Not signed.) else (echo Signed: '"$NAME-$VT"'.idnrelease - upload it on the Updates page of the Site Manager.)' \
  'pause' > "$REL/sign.cmd"
printf '%s\r\n' '@echo off' \
  'rem Once per YubiKey: change its factory PINs, then make its signing key.' \
  'cd /d "%~dp0"' \
  'idn-release.exe card-status' \
  'if errorlevel 1 goto end' \
  'echo.' \
  'choice /M "Change the YubiKey PINs first (yes if they are still the factory 123456 and 12345678)"' \
  'if errorlevel 2 goto key' \
  'idn-release.exe card-pins' \
  'if errorlevel 1 goto end' \
  ':key' \
  'idn-release.exe card-keygen' \
  'echo.' \
  'echo Send the public key above (64 characters) to be put in release-keys.txt.' \
  ':end' \
  'pause' > "$REL/make-key.cmd"
echo "$REL: copy it to the Windows PC with the YubiKey and run sign.cmd"
