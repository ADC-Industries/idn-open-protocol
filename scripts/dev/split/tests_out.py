#!/usr/bin/env python3
"""Moves a Rust file's top-level `#[cfg(test)] mod NAME { ... }` modules into
FILE_STEM/NAME.rs.

tests_out.py FILE...
Their lines are written unchanged (rustfmt re-indents the code afterwards; multi-line
string literals keep their content), and FILE ends with `#[cfg(test)]\nmod NAME;` for
each, in their order.
"""
import os, re, sys

head = re.compile(r'^mod ([a-z_0-9]+) \{$')

for path in sys.argv[1:]:
    lines = open(path).read().rstrip('\n').split('\n')
    mods = []  # (attr line, open line, close line, name)
    for o, l in enumerate(lines):
        m = head.match(l)
        if m and o > 0 and lines[o - 1] == '#[cfg(test)]':
            c = next(k for k in range(o + 1, len(lines)) if lines[k] == '}')
            mods.append((o - 1, o, c, m.group(1)))
    assert mods, (path, 'no test module')
    keep, last = [], 0
    d = os.path.dirname(path) if os.path.basename(path) in ('mod.rs', 'lib.rs', 'main.rs') else path[:-3]
    os.makedirs(d, exist_ok=True)
    for a, o, c, name in mods:
        keep += lines[last:a]
        while keep and keep[-1] == '':
            keep.pop()
        keep.append('')
        last = c + 1
        out = os.path.join(d, name + '.rs')
        assert not os.path.exists(out), out
        open(out, 'w').write('\n'.join(lines[o + 1:c]) + '\n')
        print(path, '->', out, c - o - 1, 'lines')
    keep += lines[last:]
    while keep and keep[-1] == '':
        keep.pop()
    for a, o, c, name in mods:
        keep += ['', '#[cfg(test)]', 'mod %s;' % name]
    open(path, 'w').write('\n'.join(keep) + '\n')
