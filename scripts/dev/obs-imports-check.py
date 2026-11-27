#!/usr/bin/env python3
"""Every function a Windows DLL imports from the DLLs in the given folders, against what
those DLLs export (x86_64-w64-mingw32-objdump). package.sh runs it on the OBS plugin's
builds against the OBS release each is for: a function OBS's libobs or FFmpeg doesn't
have stops the build (Windows would refuse the plugin: error 127, or 126 for a missing
DLL, as OBS 33's FFmpeg 8 did to the FFmpeg 7 build, 2026-11-12).
    obs-imports-check.py PLUGIN.dll FOLDER [FOLDER...]"""
import os
import re
import subprocess
import sys


def objdump(path):
    return subprocess.run(["x86_64-w64-mingw32-objdump", "-p", path],
                          capture_output=True, text=True, check=True).stdout


plugin, folders = sys.argv[1], sys.argv[2:]
imports, cur = {}, None
for line in objdump(plugin).splitlines():
    m = re.match(r"\s*DLL Name: (\S+)", line)
    if m:
        cur = m.group(1).lower()
        continue
    m = re.match(r"\s+[0-9a-f]+\s+<none>\s+[0-9a-f]+\s+(\S+)|\s+[0-9a-f]+\s+[0-9a-f]+\s+(\S+)$", line)
    if cur and m:
        imports.setdefault(cur, set()).add(m.group(1) or m.group(2))
found, bad = set(), 0
for folder in folders:
    for f in sorted(os.listdir(folder)):
        if not f.lower().endswith(".dll") or f.lower() not in imports:
            continue
        found.add(f.lower())
        names = set(re.findall(r"^\s*\[\s*\d+\] \+base\[\s*\d+\]\s+[0-9a-f]+ (\S+)$",
                               objdump(os.path.join(folder, f)), re.M))
        missing = sorted(imports[f.lower()] - names)
        if missing:
            print(f"{os.path.basename(plugin)}: {f} lacks {', '.join(missing)}")
            bad += 1
# The OBS and FFmpeg DLLs it imports must all be in the folders (one missing: error 126).
for need in sorted(d for d in imports if re.match(r"(obs|obs-frontend-api|av\w+-\d+|sw\w+-\d+)\.dll$", d)):
    if need not in found:
        print(f"{os.path.basename(plugin)}: imports {need}, which this OBS doesn't have")
        bad += 1
sys.exit(1 if bad else 0)
