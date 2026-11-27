#!/usr/bin/env python3
"""Prints a Rust file's top-level items with their line ranges and sizes, and its
section comments (`// ----` banners), for choosing where to split it.

outline.py FILE [MIN_LINES]
"""
import re, sys

path = sys.argv[1]
least = int(sys.argv[2]) if len(sys.argv) > 2 else 15
lines = open(path).read().split('\n')
item = re.compile(r'^(pub(\([a-z]+\))? )?(async )?(unsafe )?(const fn|fn|impl|struct|enum|trait|mod|static|const|type|macro_rules!)\b')
i = 0
while i < len(lines):
    l = lines[i]
    if l.startswith('// ---') and i + 1 < len(lines):
        print(f'{i+1:5}       == {lines[i+1][3:80]}')
    if item.match(l):
        e = i
        if l.rstrip().endswith('{'):
            while e < len(lines) and lines[e] != '}':
                e += 1
        elif not l.rstrip().endswith(';'):
            # a signature over several lines: up to its body's closing brace
            while e < len(lines) and not lines[e].endswith('{') and not lines[e].endswith(';'):
                e += 1
            if lines[e].endswith('{'):
                while e < len(lines) and lines[e] != '}':
                    e += 1
        n = e - i + 1
        if n >= least:
            print(f'{i+1:5} {n:5} {l[:100]}')
        i = e
    i += 1

# With a third argument, the methods of the impl blocks starting with it (`impl Output {`).
if len(sys.argv) > 3:
    pre = sys.argv[3]
    meth = re.compile(r'^    (pub(\([a-z]+\))? )?(const )?(unsafe )?(async )?fn ([a-z_0-9]+)')
    for i, l in enumerate(lines):
        if l.startswith(pre):
            e = i
            while lines[e] != '}':
                e += 1
            j = i
            while j < e:
                m = meth.match(lines[j])
                if m:
                    k = j
                    while lines[k] != '    }' and not (k == j and lines[k].rstrip().endswith(';')):
                        k += 1
                    print(f'   {j+1:5} {k-j+1:5}   fn {m.group(6)}')
                    j = k
                j += 1
