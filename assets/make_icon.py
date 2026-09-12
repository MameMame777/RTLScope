"""Draws RTLScope's mark and writes the icon files the GUI and the installer use.

The mark is the same square wave `theme::brand_mark` paints in the window's
corner, so the taskbar, the Start menu and the toolbar all show one thing.

Written by hand rather than with an imaging library, because the mark is four
rectangles on a rounded square and pulling in Pillow to draw them would put a
build-time dependency in the way of a file that changes once a year. Everything
here is `zlib` and `struct`, which are in the standard library.

Run from anywhere:

    python assets/make_icon.py

and it rewrites `rtlscope.ico`, `rtlscope-256.png` and `rtlscope-64.rgba` beside
itself. The `.rgba` is raw pixels because the window icon is handed to egui as
raw pixels, and decoding a PNG at startup would mean an image crate for one
64×64 image.
"""

import pathlib
import struct
import zlib

HERE = pathlib.Path(__file__).resolve().parent

# The dark theme's ground and accent. The dark ground reads on both a light and
# a dark taskbar; a light one disappears into half of them.
GROUND = (0x18, 0x20, 0x28, 0xFF)
MARK = (0x3C, 0xC1, 0xD8, 0xFF)

# Supersampling. The mark is axis-aligned so only the rounded corners and the
# stroke ends need it, but four samples a side costs nothing at these sizes.
SS = 4


def rounded_square(px, py, size, radius):
    """Whether a point is inside a rounded square covering the whole canvas."""
    inset = size * 0.045
    lo, hi = inset, size - inset
    if not (lo <= px <= hi and lo <= py <= hi):
        return False
    for cx, cy in ((lo + radius, lo + radius), (hi - radius, lo + radius),
                   (lo + radius, hi - radius), (hi - radius, hi - radius)):
        # Only the quadrant outside the straight edges is curved.
        if (px < lo + radius or px > hi - radius) and (py < lo + radius or py > hi - radius):
            if abs(px - cx) <= radius and abs(py - cy) <= radius:
                return (px - cx) ** 2 + (py - cy) ** 2 <= radius * radius
    return True


def wave_bars(size):
    """The mark, as the rectangles its polyline covers.

    `brand_mark` walks (0, low) (0.2, low) (0.2, high) (0.55, high) (0.55, low)
    (0.9, low) (0.9, high) (1, high) with a round-capped stroke. Axis-aligned
    throughout, so every segment is a rectangle.
    """
    left, right = size * 0.16, size * 0.84
    top, bottom = size * 0.33, size * 0.67
    stroke = size * 0.075
    half = stroke / 2.0

    xs = [left + (right - left) * f for f in (0.0, 0.2, 0.55, 0.9, 1.0)]
    bars = []
    # The horizontal runs, low and high alternating.
    for (x0, x1, y) in ((xs[0], xs[1], bottom), (xs[1], xs[2], top),
                        (xs[2], xs[3], bottom), (xs[3], xs[4], top)):
        bars.append((x0 - half, y - half, x1 + half, y + half))
    # The edges between them.
    for x in (xs[1], xs[2], xs[3]):
        bars.append((x - half, top - half, x + half, bottom + half))
    return bars


def render(size):
    """One RGBA image of the mark, as `bytes`."""
    bars = wave_bars(size)
    radius = size * 0.20
    out = bytearray()
    step = 1.0 / SS
    for y in range(size):
        for x in range(size):
            ground = 0
            mark = 0
            for sy in range(SS):
                for sx in range(SS):
                    px = x + (sx + 0.5) * step
                    py = y + (sy + 0.5) * step
                    if not rounded_square(px, py, size, radius):
                        continue
                    ground += 1
                    if any(x0 <= px <= x1 and y0 <= py <= y1 for x0, y0, x1, y1 in bars):
                        mark += 1
            total = SS * SS
            if ground == 0:
                out += b"\x00\x00\x00\x00"
                continue
            # The mark over the ground, both weighted by how much of the pixel
            # each covers, then the whole thing faded by the ground's coverage
            # so the rounded corner is a clean edge rather than a stair.
            weight = mark / total
            colour = tuple(
                round(MARK[i] * weight + GROUND[i] * (1.0 - weight)) for i in range(3)
            )
            alpha = round(255 * ground / total)
            out += bytes(colour) + bytes((alpha,))
    return bytes(out)


def png(size, rgba):
    """A PNG of one RGBA image."""
    raw = bytearray()
    stride = size * 4
    for y in range(size):
        raw.append(0)  # no filter; these images are tiny and mostly flat
        raw += rgba[y * stride:(y + 1) * stride]

    def chunk(kind, body):
        return (struct.pack(">I", len(body)) + kind + body
                + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF))

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", header)
            + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
            + chunk(b"IEND", b""))


def dib(size, rgba):
    """A 32-bit bottom-up DIB, which is what an icon entry classically holds.

    Kept for the small sizes: the resource loader has understood these since
    Windows 3, whereas a PNG entry needs Vista. The mask is all-opaque because
    the alpha channel already carries the shape.
    """
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    pixels = bytearray()
    stride = size * 4
    for y in range(size - 1, -1, -1):
        row = rgba[y * stride:(y + 1) * stride]
        for x in range(0, stride, 4):
            r, g, b, a = row[x], row[x + 1], row[x + 2], row[x + 3]
            pixels += bytes((b, g, r, a))
    mask_stride = ((size + 31) // 32) * 4
    return header + bytes(pixels) + b"\x00" * (mask_stride * size)


def ico(images):
    """An .ico holding every size, DIB for the small ones and PNG for the rest."""
    entries = bytearray()
    body = bytearray()
    offset = 6 + 16 * len(images)
    for size, data in images:
        entries += struct.pack(
            "<BBBBHHII",
            size if size < 256 else 0,
            size if size < 256 else 0,
            0, 0, 1, 32, len(data), offset,
        )
        body += data
        offset += len(data)
    return struct.pack("<HHH", 0, 1, len(images)) + bytes(entries) + bytes(body)


def main():
    rendered = {size: render(size) for size in (16, 32, 48, 64, 128, 256)}

    parts = [(size, dib(size, rendered[size])) for size in (16, 32, 48)]
    parts += [(size, png(size, rendered[size])) for size in (64, 128, 256)]
    (HERE / "rtlscope.ico").write_bytes(ico(parts))

    (HERE / "rtlscope-256.png").write_bytes(png(256, rendered[256]))
    # The installer's own window draws its logo at 64 and does not scale.
    (HERE / "rtlscope-64.png").write_bytes(png(64, rendered[64]))
    # Raw pixels for the window icon, so the GUI needs no PNG decoder.
    (HERE / "rtlscope-64.rgba").write_bytes(rendered[64])

    for name in ("rtlscope.ico", "rtlscope-256.png", "rtlscope-64.png", "rtlscope-64.rgba"):
        print(f"{name}: {(HERE / name).stat().st_size} bytes")


if __name__ == "__main__":
    main()
