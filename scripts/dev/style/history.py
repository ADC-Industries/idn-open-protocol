#!/usr/bin/env python3
"""Takes history tags out of Rust comments, keeping what the comment explains.

history.py [--write] FILES...  (without --write: prints a sample of changes and counts)
Only comment lines (//, ///, //!) are touched; the code is not.
"""
import re, sys, random

D = r'20\d\d-\d\d-\d\d'
CODE = r'(?: [A-Z]{1,3}[0-9]*(?:-[0-9]+)?)?'          # review finding codes: S1-7, B2, D10, C8
LAB = r'lab(?: items?)? [0-9]{2,3}(?:\s*(?:–|-|,|and)\s*[0-9]{2,3})*(?:,? finding [A-Z])?'
PASS = r'(?:(?:the )?desk(?: and switcher)? pass|switcher pass|pass)(?: [A-Z][0-9])?(?:,? ' + D + r')?'
REV = r'review ' + D + CODE
PROF = r'profil(?:e|ing)(?: round)?(?: ' + D + r')?'
TAG = r'(?:' + '|'.join([LAB, REV, PROF + r'(?<=\d)', PASS, r'lab finding,? ' + D, D, r'CR-D[0-9]{3}']) + r')'
rules = [
    # a whole parenthesis that is only a tag (or tags)
    (re.compile(r'\s*\(' + TAG + r'(?:[,;] ' + TAG + r')*\)'), ''),
    # a tag ending a parenthesis: "(a 4K60 file, lab item 255)" -> "(a 4K60 file)"
    (re.compile(r'[,;:]\s*' + TAG + r'(?:[,;] ' + TAG + r')*(?=\))'), ''),
    # a tag opening a parenthesis or sentence: "(review 2026-11-01 A3: the ...)" -> "(the ...)"
    (re.compile(r'\(' + TAG + r':\s*'), '('),
    (re.compile(r'(?<=// )' + TAG + r':\s*'), ''),
    # a tag after a colon or comma ending the sentence: "... nothing: review 2026-11-02 S1-5.)"
    (re.compile(r'[,:;]\s*' + TAG + r'(?=[.)])'), ''),
]
tidy = [
    (re.compile(r'\(\s*\)'), ''),
    (re.compile(r' +([.,;:])'), r'\1'),
    (re.compile(r'\(\s+'), '('),
    (re.compile(r'(\S)  +'), r'\1 '),
]

def fix_comment(text):
    t = text
    for r, s in rules:
        t = r.sub(s, t)
    if t != text:
        for r, s in tidy:
            t = r.sub(s, t)
    return t

write = '--write' in sys.argv
files = [f for f in sys.argv[1:] if f != '--write']
changes = []
for p in files:
    lines = open(p).read().split('\n')
    out = []
    for i, l in enumerate(lines):
        m = re.match(r'^(\s*//[/!]?)(.*)$', l)
        if m:
            body = fix_comment(m.group(2))
            new = m.group(1) + body
            if new != l:
                changes.append((p, i + 1, l.strip(), new.strip()))
                # Nothing left but punctuation: the line goes, and the comment before it
                # ends where it did.
                if re.fullmatch(r'[\s.,;:()]*', body):
                    if out and re.match(r'^\s*//', out[-1]) and not re.search(r'[.!?:)]$', out[-1].rstrip()):
                        out[-1] = out[-1].rstrip() + '.'
                    continue
                # A tag that opened the line leaves its punctuation behind: it goes back to
                # the end of the line before.
                lead = re.match(r'([;,:])\s*(.*)$', body.lstrip())
                if lead and out and re.match(r'^\s*//', out[-1]):
                    out[-1] = out[-1].rstrip() + lead.group(1)
                    new = m.group(1) + ' ' + lead.group(2)
                l = new.rstrip()
        out.append(l)
    if write:
        open(p, 'w').write('\n'.join(out))
print(len(changes), 'comment lines changed')
if not write:
    random.seed(1)
    for c in random.sample(changes, min(25, len(changes))):
        print(f'{c[0]}:{c[1]}\n  - {c[2]}\n  + {c[3]}')
