#!/usr/bin/env python3
"""Moves methods of one `impl TYPE {` block of a Rust file into child module files.

split.py FILE TYPE SPEC.json
SPEC: {"submodule": {"doc": "//! ...", "methods": ["a", "b"]}, ...}
Each method (with its doc comments and attributes) leaves the impl block and goes into
FILE's directory/<stem>/<submodule>.rs inside `impl TYPE { ... }`, made `pub(super)`
unless it already says pub; `mod <submodule>;` lines go after FILE's last top `use`.
"""
import json, re, sys, os

path, ty, spec_path = sys.argv[1], sys.argv[2], sys.argv[3]
spec = json.load(open(spec_path))
lines = open(path).read().split('\n')
head = f'impl {ty} {{'
starts = [i for i, l in enumerate(lines) if l.startswith(head) or (l.startswith('impl<') and f' {ty}<' in l and l.endswith('{'))]
assert starts, 'impl block not found'
inside = set()
for st in starts:
    e = st
    while lines[e] != '}':
        e += 1
    inside.update(range(st, e + 1))
moved = {}
taken = set()
for sub, s in spec.items():
    for name in s['methods']:
        pat = re.compile(r'^    (pub(\([a-z]+\))? )?(const )?(unsafe )?(async )?fn ' + re.escape(name) + r'\b')
        idx = [i for i, l in enumerate(lines) if pat.match(l) and i not in taken and i in inside]
        assert len(idx) == 1, (name, len(idx))
        i = idx[0]
        # the docs and attributes above it
        a = i
        while a > 0 and (lines[a-1].startswith('    ///') or lines[a-1].startswith('    #[') or lines[a-1].startswith('    //')) and not lines[a-1].startswith('    // ----'):
            a -= 1
        # its end: the first line that is exactly "    }" (rustfmt) after the signature
        e = i
        while lines[e] != '    }':
            e += 1
        body = lines[a:e+1]
        # pub(super) unless pub already
        k = i - a
        if not re.match(r'^    pub', body[k]):
            body[k] = body[k].replace('    fn ', '    pub(super) fn ', 1).replace('    const fn ', '    pub(super) const fn ', 1).replace('    unsafe fn ', '    pub(super) unsafe fn ', 1).replace('    async fn ', '    pub(super) async fn ', 1)
        moved.setdefault(sub, []).append(body)
        for j in range(a, e+1):
            taken.add(j)
# drop the taken lines (and a blank line after each, if it leaves two blanks)
out = []
for j, l in enumerate(lines):
    if j in taken:
        continue
    if l == '' and out and out[-1] == '' :
        continue
    out.append(l)
# blank lines left just inside "impl X {" or before a closing "}" tidied
text = '\n'.join(out)
text = re.sub(r'(impl[^\n]*\{\n)\n+', r'\1', text)
text = re.sub(r'\n\n+(\}\n)', r'\n\1', text)
# mod lines after the last top-level use
tl = text.split('\n')
last_use = max(i for i, l in enumerate(tl) if l.startswith('use ') or (l.startswith('pub use ')))
# a use may span lines: go to its end
while not tl[last_use].rstrip().endswith(';'):
    last_use += 1
mods = [f'mod {sub};' for sub in spec]
tl = tl[:last_use+1] + [''] + mods + tl[last_use+1:]
open(path, 'w').write('\n'.join(tl))
stem = os.path.dirname(path) if os.path.basename(path) in ('mod.rs', 'lib.rs', 'main.rs') else os.path.splitext(path)[0]
os.makedirs(stem, exist_ok=True)
for sub, bodies in moved.items():
    with open(os.path.join(stem, sub + '.rs'), 'w') as f:
        f.write(spec[sub]['doc'].rstrip('\n') + '\n\n')
        f.write('use super::*;\n\n')
        f.write(f'impl {ty} {{\n')
        f.write('\n\n'.join('\n'.join(b) for b in bodies))
        f.write('\n}\n')
print('moved', {k: len(v) for k, v in moved.items()})
