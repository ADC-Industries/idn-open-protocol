#!/usr/bin/env python3
"""Moves free functions and whole impl blocks of a Rust file into child module files.

split2.py FILE SPEC.json
SPEC: {"sub": {"doc": "//! ...", "fns": ["name"], "impls": ["impl Name {", ...]}}
Free functions (with docs/attributes) are made pub(super) unless pub; in a moved impl
block every method not pub becomes pub(super). Structs stay in FILE (their fields'
privacy is the parent's). `mod sub;` lines go after FILE's last top-level `use`.
"""
import json, re, sys, os

path, spec_path = sys.argv[1], sys.argv[2]
spec = json.load(open(spec_path))
lines = open(path).read().split('\n')
taken = set()
moved = {}
public = set()

def with_docs(i):
    a = i
    while a > 0 and (lines[a-1].startswith('///') or lines[a-1].startswith('#[') or lines[a-1].startswith('#![')):
        a -= 1
    return a

def end_of(i):
    e = i
    while lines[e] != '}':
        e += 1
    return e

for sub, s in spec.items():
    for name in s.get('fns', []):
        pat = re.compile(r'^(pub(\([a-z]+\))? )?(const )?(async )?(unsafe )?fn ' + re.escape(name) + r'\b')
        idx = [i for i, l in enumerate(lines) if pat.match(l) and i not in taken]
        assert len(idx) == 1, (name, len(idx))
        i = idx[0]; a = with_docs(i); e = end_of(i)
        body = lines[a:e+1]
        k = i - a
        if not body[k].startswith('pub'):
            body[k] = 'pub(super) ' + body[k]
        elif not body[k].startswith('pub(super)'):
            public.add(sub)
        moved.setdefault(sub, []).append(body)
        taken.update(range(a, e+1))
    for head in s.get('impls', []):
        idx = [i for i, l in enumerate(lines) if l.startswith(head) and i not in taken]
        assert len(idx) >= 1, (head, len(idx))
        for i in idx:
            a = with_docs(i); e = end_of(i)
            body = lines[a:e+1]
            trait_impl = ' for ' in lines[i]
            for j, l in enumerate(body):
                m = re.match(r'^    ((const |unsafe |async )*)fn ', l)
                if m and not trait_impl:
                    body[j] = '    pub(super) ' + l[4:]
            moved.setdefault(sub, []).append(body)
            taken.update(range(a, e+1))

out = []
for j, l in enumerate(lines):
    if j in taken:
        continue
    if l == '' and out and out[-1] == '':
        continue
    out.append(l)
tl = out
last_use = max(i for i, l in enumerate(tl) if l.startswith('use ') or l.startswith('pub use '))
while not tl[last_use].rstrip().endswith(';'):
    last_use += 1
tl = tl[:last_use+1] + [''] + [f'mod {sub};' for sub in spec] + [('pub use' if sub in public else 'use') + f' {sub}::*;' for sub in spec if spec[sub].get('fns')] + tl[last_use+1:]
open(path, 'w').write('\n'.join(tl))
stem = os.path.dirname(path) if os.path.basename(path) in ('mod.rs', 'lib.rs', 'main.rs') else os.path.splitext(path)[0]
os.makedirs(stem, exist_ok=True)
for sub, bodies in moved.items():
    p = os.path.join(stem, sub + '.rs')
    with open(p, 'w') as f:
        f.write(spec[sub]['doc'].rstrip('\n') + '\n\n')
        f.write('use super::*;\n\n')
        f.write('\n\n'.join('\n'.join(b) for b in bodies))
        f.write('\n')
print('moved', {k: len(v) for k, v in moved.items()})
