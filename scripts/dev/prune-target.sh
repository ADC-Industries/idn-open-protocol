#!/bin/bash
# Removes the build folder's leftovers: copies of crates from earlier builds that no
# current build uses (each change leaves the old copy behind; target/ grew to 427 GB).
# What is kept is what cargo itself lists: each build below is asked for its artifacts
# (`--message-format=json` lists every unit, rebuilt or fresh), and in each profile's
# deps/, build/ and .fingerprint/ only the files and folders of those units stay. A
# build that is out of date is brought up to date on the way (as the next build would).
#   prune-target.sh            what it would remove, by folder
#   prune-target.sh --delete   removes it
# The builds are package.sh's (Linux, the GPU programs, Windows with OBS's FFmpeg and
# with FFmpeg 8.1), the test builds (debug and release, the library with `gpu`) and the
# clippy checks (Linux, Windows): keep the list in step with them.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/common.sh"
set -u
delete=${1:-}
W=$(workdir prune)
keep=$W/keep.txt
: > "$keep"
cd "$ROOT"
KW=$K/windows
F8=$KW/ffmpeg8-dl/ffmpeg-n8.1.3-9-g29e619e767-win64-lgpl-shared-8.1
export Vulkan_INCLUDE_DIR=$K/vulkan-include
[ -z "${PCSC_LIB_DIR:-}" ] && [ -d "$K/pcsc" ] && export PCSC_LIB_DIR="$K/pcsc"
WIN=(--release --target x86_64-pc-windows-gnu)
# One build's artifacts into the keep list: every hash in its artifacts' paths and out
# dirs. The env and cargo arguments follow.
failed=0
units() {
  echo "listing: $*" >&2
  env "$@" --message-format=json 2>/dev/null > "$W/units.json" || { echo "  that build failed: nothing is removed" >&2; failed=1; }
  python3 -c '
import json, os, re, sys
# A program or example is listed as its copy beside deps/ (target/release/idn), with no
# hash: its hash is that of the hard-linked original, in deps/ (programs) or beside it
# (examples); without it the next build would link every program again.
inodes = {}
def linked(path):
    try:
        st = os.stat(path)
    except OSError:
        return []
    d = os.path.dirname(path)
    found = []
    for c in (os.path.join(d, "deps"), d):
        if c not in inodes:
            m = {}
            try:
                for e in os.scandir(c):
                    if e.is_file(follow_symlinks=False):
                        m.setdefault((e.stat().st_dev, e.stat().st_ino), []).append(e.name)
            except OSError:
                pass
            inodes[c] = m
        found += inodes[c].get((st.st_dev, st.st_ino), [])
    return found
for line in sys.stdin:
    if not line.startswith("{"):
        continue
    m = json.loads(line)
    paths = m.get("filenames") or []
    if m.get("executable"):
        paths.append(m["executable"])
    if m.get("out_dir"):
        paths.append(m["out_dir"])
    for p in paths:
        hs = re.findall(r"-([0-9a-f]{16})(?=[./]|$)", p)
        if not hs:
            hs = [h for n in linked(p) for h in re.findall(r"-([0-9a-f]{16})(?=[./]|$)", n)]
        for h in hs:
            print(h)
' < "$W/units.json" >> "$keep"
}
units cargo build --release --workspace --exclude idn-switcher --exclude idn-display --exclude idn-ffmpeg
# (package.sh's exact builds: with `sound` too, or every prune removed what it uses;
# bug round E, E7-12.)
units cargo build --release -p idn-switcher -p idn-display -p idn-ffmpeg --features idn-ffmpeg/gpu,idn-ffmpeg/sound --bins --example va_gpu_check
units cargo build --release -p idn-open-protocol --example avsync --example stream_log --example beep_offset --example audio_chain
units cargo build --release -p idn-open-protocol --features gpu --example gpu_recv
units cargo build --release -p idn-mqtt --example mqtt
units cargo build --release -p idn-ffmpeg --example decode_bench
units cargo build --release -p idn-switcher --example compose_bench
units cargo build --release -p idn-industrial --bins --example mqtt_watch --example modbus_sim
units cargo test --release --workspace --no-run
units cargo test --release --features gpu -p idn-open-protocol --no-run
units cargo test --workspace --no-run
units cargo clippy --release --workspace --all-targets
units FFMPEG_DIR=$KW/ffmpeg IDN_OBS_INCLUDE=$KW/include IDN_OBS_LIB=$KW/dll cargo build "${WIN[@]}" \
  -p idn-node -p idn-obs -p idn-obs-loader -p idn-open-protocol -p idn-ffmpeg -p idn-audio -p idn-mixer -p idn-plugin-host \
  -p idn-audio-plugins -p idn-cli -p idn-asio -p idn-camera-source -p idn-release -p idn-voice-app --bins --lib
# The OBS plugin's FFmpeg 8 build (OBS 33), in its own folder as package.sh makes it.
units CARGO_TARGET_DIR=target/win-obs-ff8 FFMPEG_DIR=$KW/ffmpeg-obs33 IDN_OBS_INCLUDE=$KW/include IDN_OBS_LIB=$KW/dll \
  cargo build "${WIN[@]}" -p idn-obs
units FFMPEG_DIR=$KW/ffmpeg IDN_OBS_INCLUDE=$KW/include IDN_OBS_LIB=$KW/dll cargo build "${WIN[@]}" -p idn-display --bins
units FFMPEG_DIR=$KW/ffmpeg cargo build "${WIN[@]}" -p idn-audio --examples
for c in "-p idn-ffmpeg --features gpu,sound --bins --examples" "-p idn-switcher --bins --examples" \
  "-p idn-display --bins"; do
  # shellcheck disable=SC2086
  units CARGO_TARGET_DIR=target/win-ffmpeg8 FFMPEG_DIR=$F8 cargo build "${WIN[@]}" $c
done
units CARGO_TARGET_DIR=target/win-ffmpeg8 FFMPEG_DIR=$F8 IDN_OBS_INCLUDE=$KW/include IDN_OBS_LIB=$KW/dll \
  cargo clippy "${WIN[@]}" --workspace --all-targets
# The Industrial builds (their own profile).
units CARGO_TARGET_DIR=target/win-ffmpeg8 FFMPEG_DIR=$F8 cargo build --target x86_64-pc-windows-gnu --profile industrial \
  -p idn-industrial --features capture --bin idn-helper-industrial
units cargo build --target x86_64-pc-windows-gnu --profile industrial -p idn-industrial --bin idn-industrial --bin idn-site-manager-industrial
units cargo build --target x86_64-pc-windows-gnu --profile industrial -p idn-voice-app
sort -u -o "$keep" "$keep"
# A build that failed lists only part of what it uses: then nothing is removed.
[ "$failed" = 0 ] || delete=
echo "units kept: $(wc -l < "$keep")" >&2
# The leftovers: entries of deps/, build/ and .fingerprint/ named with a hash not kept.
python3 - "$keep" "$delete" <<'EOF'
import os, re, shutil, sys
keep = set(open(sys.argv[1]).read().split())
delete = sys.argv[2] == "--delete"
profiles = []
for root, dirs, _ in os.walk("target"):
    if os.path.basename(root) in ("debug", "release", "industrial") and os.path.isdir(os.path.join(root, "deps")):
        profiles.append(root)
        dirs[:] = []
total = 0
for p in sorted(profiles):
    size = 0
    for sub in ("deps", "build", ".fingerprint"):
        d = os.path.join(p, sub)
        if not os.path.isdir(d):
            continue
        for name in os.listdir(d):
            m = re.search(r"-([0-9a-f]{16})(?:\.|$)", name)
            if not m or m.group(1) in keep:
                continue
            path = os.path.join(d, name)
            if os.path.isdir(path) and not os.path.islink(path):
                n = sum(os.path.getsize(os.path.join(a, f)) for a, _, fs in os.walk(path) for f in fs
                        if not os.path.islink(os.path.join(a, f)))
                if delete:
                    shutil.rmtree(path)
            else:
                n = os.lstat(path).st_size
                if delete:
                    os.remove(path)
            size += n
    total += size
    print(f"{p}: {size / 1e9:.1f} GB {'removed' if delete else 'to remove'}")
print(f"all: {total / 1e9:.1f} GB {'removed' if delete else 'to remove (--delete removes it)'}")
EOF
