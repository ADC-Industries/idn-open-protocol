#!/bin/bash
# Prints the speech models' pins for media/idn-captions/src/lib.rs (MODELS, MODEL_PINS):
# whisper.cpp's latest commit on Hugging Face, and each model's size and SHA-256 as
# Hugging Face records them. Run it when adding a model or moving to a newer commit, then
# copy the values by hand (and check the commit's change log first).
#   scripts/dev/model-hashes.sh [MODEL...]   (default: the five offered)
set -euo pipefail
repo=ggerganov/whisper.cpp
models=("$@")
[ ${#models[@]} -eq 0 ] && models=(tiny.en base.en small.en medium.en large-v3-turbo)
commit=$(curl -fsS "https://huggingface.co/api/models/$repo" | python3 -I -c 'import json,sys; d=json.load(sys.stdin); print(d["sha"], d.get("lastModified",""))')
echo "commit: $commit"
curl -fsS "https://huggingface.co/api/models/$repo/tree/${commit%% *}" | python3 -I -c '
import json, sys
want = sys.argv[1:]
files = {f["path"]: f for f in json.load(sys.stdin)}
for m in want:
    f = files.get(f"ggml-{m}.bin")
    if not f or not f.get("lfs"):
        print(f"{m}: not found"); continue
    size, oid = f["size"], f["lfs"]["oid"]
    print("    (\"%s\", %s, \"%s\")," % (m, format(size, "_"), oid))
' "${models[@]}"
