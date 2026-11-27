#!/usr/bin/env python3
"""Draws the IDN icon: three linked nodes on a rounded square.

Writes, next to the IDN Node crate (idn-node/assets/):
  idn.ico     16, 20, 24, 32, 48 and 64 px as 32-bit bitmaps, 256 px as PNG: the programs'
              icon (embedded in idn-node.exe and idn-session.exe), the installer's and
              the Start menu's.

Standard library only. Run it again after changing the drawing; the outputs are committed.
"""
import math
import os
import struct
import zlib

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "idn-node", "assets")
BLUE = (0x25, 0x63, 0xEB)
WHITE = (0xFF, 0xFF, 0xFF)

# In units of the icon's width, y down.
CORNER = 0.22
NODES = [(0.50, 0.29), (0.27, 0.70), (0.73, 0.70)]
NODE_R = 0.125
LINE_W = 0.085


def in_background(x, y):
    # Rounded square over the whole icon.
    cx = min(max(x, CORNER), 1 - CORNER)
    cy = min(max(y, CORNER), 1 - CORNER)
    return (x - cx) ** 2 + (y - cy) ** 2 <= CORNER**2


def dist_to_segment(px, py, a, b):
    ax, ay = a
    bx, by = b
    dx, dy = bx - ax, by - ay
    t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
    return math.hypot(px - (ax + t * dx), py - (ay + t * dy))


def in_symbol(x, y):
    for n in NODES:
        if math.hypot(x - n[0], y - n[1]) <= NODE_R:
            return True
    for i in range(3):
        if dist_to_segment(x, y, NODES[i], NODES[(i + 1) % 3]) <= LINE_W / 2:
            return True
    return False


def coverage(size):
    """(background, symbol) coverage of each pixel, 0..255, rows top to bottom."""
    ss = 8 if size <= 64 else 3
    out = []
    for py in range(size):
        row = []
        for px in range(size):
            bg = sym = 0
            for sy in range(ss):
                for sx in range(ss):
                    x = (px + (sx + 0.5) / ss) / size
                    y = (py + (sy + 0.5) / ss) / size
                    if in_background(x, y):
                        bg += 1
                        if in_symbol(x, y):
                            sym += 1
            n = ss * ss
            row.append((round(255 * bg / n), round(255 * sym / max(bg, 1)) if bg else 0))
        out.append(row)
    return out


def rgba(size, colour=BLUE):
    rows = []
    for row in coverage(size):
        r = []
        for bg, sym in row:
            t = sym / 255
            c = tuple(round(colour[i] * (1 - t) + WHITE[i] * t) for i in range(3))
            r.append(c + (bg,))
        rows.append(r)
    return rows


def bmp_entry(size):
    rows = rgba(size)
    hdr = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    pixels = b"".join(
        bytes((b, g, r, a)) for row in reversed(rows) for (r, g, b, a) in row
    )
    mask_row = ((size + 31) // 32) * 4
    return hdr + pixels + bytes(mask_row * size)


def png_entry(size):
    rows = rgba(size)
    raw = b"".join(b"\0" + b"".join(bytes(p) for p in row) for row in rows)

    def chunk(kind, data):
        c = kind + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def write_ico(path):
    sizes = [16, 20, 24, 32, 48, 64, 256]
    images = [png_entry(s) if s == 256 else bmp_entry(s) for s in sizes]
    offset = 6 + 16 * len(sizes)
    head = struct.pack("<HHH", 0, 1, len(sizes))
    dirs = b""
    for s, img in zip(sizes, images):
        dim = 0 if s == 256 else s
        dirs += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(img), offset)
        offset += len(img)
    with open(path, "wb") as f:
        f.write(head + dirs + b"".join(images))


if __name__ == "__main__":
    os.makedirs(OUT, exist_ok=True)
    write_ico(os.path.join(OUT, "idn.ico"))
    print("wrote", os.path.normpath(OUT))
