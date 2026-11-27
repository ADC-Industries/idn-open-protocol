#!/usr/bin/env python3
"""Moves whole sections (line ranges, items and all) of a Rust file into child modules.

sections.py FILE SPEC.json
SPEC: {"sub": {"doc": "//! ...", "from": FIRST_LINE, "to": LAST_LINE}, ...} (or "ranges":
[[FIRST, LAST], ...]; 1-based,
inclusive, ranges not overlapping). Top-level items that aren't pub become pub(super),
and so do the fields of structs that move; FILE gets `mod sub;` and `pub use sub::*;`
(or `use` when nothing in it is pub) after its last top-level `use`, so every path
stays as it was. A `// ----` banner opening a range is dropped (the module's doc says it).
"""
import json, re, sys, os

path, spec_path = sys.argv[1], sys.argv[2]
spec = json.load(open(spec_path))
lines = open(path).read().split('\n')
item = re.compile(r'^(async |unsafe |const fn |fn |struct |enum |trait |static |const |type |mod |impl)')
field = re.compile(r'^    ([a-z_][a-z_0-9]*): ')
taken, out_bodies, public, public_crate = set(), {}, set(), set()
for sub, s in spec.items():
    ranges = s.get('ranges') or [[s['from'], s['to']]]
    body = []
    for a, b in ranges:
        body += ([''] if body else []) + lines[a - 1:b]
        taken.update(range(a - 1, b))
    while body and (body[0].startswith('// ---') or body[0] == ''):
        if body[0].startswith('// ---'):
            body = body[1:]
            # the banner's title lines, up to the next banner line
            while body and body[0].startswith('//') and not body[0].startswith('///') and not body[0].startswith('// ---'):
                body = body[1:]
            if body and body[0].startswith('// ---'):
                body = body[1:]
        else:
            body = body[1:]
    in_struct = False
    for j, l in enumerate(body):
        if l.startswith('pub ') or l.startswith('pub('):
            if l.startswith('pub ') and not l.startswith('pub use'):
                public.add(sub)
            elif l.startswith('pub(crate) ') and public_crate is not None:
                public_crate.add(sub)
            in_struct = (' struct ' in l and l.rstrip().endswith('{'))
            continue
        if item.match(l) and not l.startswith('impl') and not l.startswith('mod '):
            body[j] = 'pub(super) ' + l
            in_struct = l.startswith('struct ') and l.rstrip().endswith('{')
            continue
        if l.startswith('mod '):
            body[j] = 'pub(super) ' + l
        if l.startswith('impl') and ' for ' not in l and l.rstrip().endswith('{'):
            k = j + 1
            while body[k] != '}':
                if re.match(r'^    (const |unsafe |async )*fn ', body[k]):
                    body[k] = '    pub(super) ' + body[k][4:]
                k += 1
        if in_struct:
            if l == '}':
                in_struct = False
            elif field.match(l):
                body[j] = '    pub(super) ' + l[4:]
    out_bodies[sub] = body

out = []
for j, l in enumerate(lines):
    if j in taken:
        continue
    if l == '' and out and out[-1] == '':
        continue
    out.append(l)
last_use = max(i for i, l in enumerate(out) if l.startswith('use ') or l.startswith('pub use '))
while not out[last_use].rstrip().endswith(';'):
    last_use += 1
decl = [f'mod {sub};' for sub in spec] + [('pub use' if sub in public else 'pub(crate) use' if sub in public_crate else 'use') + f' {sub}::*;' for sub in spec]
out = out[:last_use + 1] + [''] + decl + out[last_use + 1:]
open(path, 'w').write('\n'.join(out))
d = os.path.dirname(path) if os.path.basename(path) in ('mod.rs', 'lib.rs', 'main.rs') else os.path.splitext(path)[0]
os.makedirs(d, exist_ok=True)
for sub, body in out_bodies.items():
    p = os.path.join(d, sub + '.rs')
    assert not os.path.exists(p), p
    while body and body[-1] == '':
        body.pop()
    open(p, 'w').write(spec[sub]['doc'].rstrip('\n') + '\n\nuse super::*;\n\n' + '\n'.join(body) + '\n')
    print(p, len(body), 'lines')
