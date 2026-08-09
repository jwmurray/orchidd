#!/usr/bin/env python3
"""Generate orchidd's PWA icons.

Writes static/icon-192.png, icon-512.png and apple-touch-icon.png (180px).
Pure stdlib (zlib + struct) so there is no Pillow dependency; the icons are
committed, and this script exists so they can be regenerated or restyled.

Design: rounded square, warm-to-cool vertical gradient (the temperature red and
humidity blue used by the graph), with a white thermometer glyph.
"""

import math
import struct
import zlib
from pathlib import Path

STATIC = Path(__file__).resolve().parent.parent / "static"

TEMP = (216, 58, 47)  # --temp
HUM = (22, 104, 201)  # --hum


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def rounded_alpha(x, y, size, radius):
    """Coverage (0..1) of a rounded square, antialiased at the corners."""
    cx = min(max(x, radius), size - radius)
    cy = min(max(y, radius), size - radius)
    d = math.hypot(x - cx, y - cy)
    return max(0.0, min(1.0, radius - d + 0.5))


def thermometer_alpha(x, y, size):
    """Coverage of a white thermometer: round bulb + capsule stem."""
    s = size / 100.0  # work in a 100x100 design space
    px, py = x / s, y / s

    bulb_cx, bulb_cy, bulb_r = 50.0, 71.0, 13.0
    stem_cx, stem_top, stem_bot, stem_r = 50.0, 26.0, 68.0, 6.0

    d_bulb = math.hypot(px - bulb_cx, py - bulb_cy) - bulb_r
    cy = min(max(py, stem_top), stem_bot)
    d_stem = math.hypot(px - stem_cx, py - cy) - stem_r

    d = min(d_bulb, d_stem)
    return max(0.0, min(1.0, 0.5 - d * s))


def render(size):
    radius = size * 0.22
    rows = []
    for y in range(size):
        row = bytearray()
        row.append(0)  # PNG filter type 0
        base = lerp(TEMP, HUM, y / max(1, size - 1))
        for x in range(size):
            a = rounded_alpha(x + 0.5, y + 0.5, size, radius)
            r, g, b = base
            t = thermometer_alpha(x + 0.5, y + 0.5, size)
            if t > 0:
                r, g, b = lerp((r, g, b), (255, 255, 255), t)
            row += bytes((r, g, b, round(a * 255)))
        rows.append(bytes(row))
    return b"".join(rows)


def chunk(tag, data):
    return (
        struct.pack(">I", len(data))
        + tag
        + data
        + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    )


def write_png(path, size):
    raw = render(size)
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)  # 8-bit RGBA
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    path.write_bytes(png)
    print(f"wrote {path.relative_to(STATIC.parent)} ({size}x{size}, {len(png)} bytes)")


if __name__ == "__main__":
    STATIC.mkdir(exist_ok=True)
    write_png(STATIC / "icon-192.png", 192)
    write_png(STATIC / "icon-512.png", 512)
    write_png(STATIC / "apple-touch-icon.png", 180)
