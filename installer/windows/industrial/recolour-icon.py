#!/usr/bin/env python3
"""The IDN Node | Industrial icon: the IDN icon with its blue (#2563EB) turned to the
industrial orange (#B4531A), so the editions' icons can't be mistaken (spec
idn-industrial.md I.2). Standard library only:

    recolour-icon.py idn.ico idn-industrial.ico
"""
import struct, sys, zlib

BLUE = (37, 99, 235)
NEW = (180, 83, 26)


def recolour(r, g, b):
    # The icon is the blue blended with white: how much blue, then that much orange.
    t = max(0.0, min(1.0, (255 - r) / (255 - BLUE[0])))
    return tuple(round(255 * (1 - t) + n * t) for n in NEW)


def bmp(data):
    hdr = struct.unpack('<IiiHHIIiiII', data[:40])
    w, h2, bpp = hdr[1], hdr[2], hdr[4]
    assert bpp == 32
    out = bytearray(data)
    n = w * (h2 // 2)
    for i in range(n):
        o = 40 + 4 * i
        b, g, r, a = out[o:o + 4]
        out[o:o + 3] = bytes(reversed(recolour(r, g, b)))
    return bytes(out)


def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    return a if pa <= pb and pa <= pc else (b if pb <= pc else c)


def png(data):
    pos, chunks = 8, []
    while pos < len(data):
        n, = struct.unpack('>I', data[pos:pos + 4])
        kind = data[pos + 4:pos + 8]
        chunks.append((kind, data[pos + 8:pos + 8 + n]))
        pos += 12 + n
    ihdr = dict(chunks)[b'IHDR']
    w, h, depth, ctype = struct.unpack('>IIBB', ihdr[:10])
    assert depth == 8 and ctype == 6, 'RGBA 8-bit only'
    raw = zlib.decompress(b''.join(c for k, c in chunks if k == b'IDAT'))
    stride, bpp = w * 4, 4
    rows, prev, pos = [], bytearray(stride), 0
    for _ in range(h):
        f = raw[pos]
        line = bytearray(raw[pos + 1:pos + 1 + stride])
        pos += 1 + stride
        for i in range(stride):
            a = line[i - bpp] if i >= bpp else 0
            b = prev[i]
            c = prev[i - bpp] if i >= bpp else 0
            line[i] = (line[i] + (0, a, b, (a + b) // 2, paeth(a, b, c))[f]) & 255
        rows.append(line)
        prev = line
    out = bytearray()
    for line in rows:
        for i in range(0, stride, 4):
            line[i:i + 3] = bytes(recolour(*line[i:i + 3]))
        out += b'\0' + line
    def chunk(kind, body):
        return struct.pack('>I', len(body)) + kind + body + struct.pack('>I', zlib.crc32(kind + body))
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', ihdr) + chunk(b'IDAT', zlib.compress(bytes(out), 9)) + chunk(b'IEND', b'')


src = open(sys.argv[1], 'rb').read()
n, = struct.unpack('<H', src[4:6])
entries, images = [], []
for i in range(n):
    e = list(struct.unpack('<BBBBHHII', src[6 + 16 * i:22 + 16 * i]))
    img = src[e[7]:e[7] + e[6]]
    img = png(img) if img[:4] == b'\x89PNG' else bmp(img)
    entries.append(e)
    images.append(img)
off = 6 + 16 * n
out = bytearray(src[:6])
for e, img in zip(entries, images):
    e[6], e[7] = len(img), off
    out += struct.pack('<BBBBHHII', *e)
    off += len(img)
for img in images:
    out += img
open(sys.argv[2], 'wb').write(out)
