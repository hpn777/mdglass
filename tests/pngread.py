#!/usr/bin/env python3
"""Minimal stdlib-only PNG reader for the screenshots GTK writes
(gdk_texture_save_to_png): non-interlaced, bitdepth 8, color types 2 (RGB)
and 6 (RGBA). Parses IHDR, concatenates every IDAT, zlib-decompresses and
undoes all five scanline filters. Exposes open(path) -> Image with
w, h, rgb(x, y) -> (r, g, b) and gray(x, y) (simple average luma)."""
import builtins, struct, zlib

SIGNATURE = b"\x89PNG\r\n\x1a\n"


def _paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    return b if pb <= pc else c


class Image:
    def __init__(self, w, h, channels, raw):
        self.w = w
        self.h = h
        self.nc = channels
        self._px = raw  # bytearray, w*h*channels, top-left origin

    def rgb(self, x, y):
        i = (y * self.w + x) * self.nc
        return tuple(self._px[i:i + 3])

    def gray(self, x, y):
        r, g, b = self.rgb(x, y)
        return (r + g + b) // 3

    def row(self, y):
        """all w pixels of scanline y as (r, g, b) tuples"""
        px, nc, w = self._px, self.nc, self.w
        i = y * w * nc
        return [(px[i + x * nc], px[i + x * nc + 1], px[i + x * nc + 2]) for x in range(w)]


def open(path):
    """Decode a PNG file into an Image; raises ValueError on anything the
    screenshots should not be (interlaced, odd bitdepth/colour type)."""
    with builtins.open(path, "rb") as f:  # module-level open shadows the builtin
        data = f.read()
    if data[:8] != SIGNATURE:
        raise ValueError(f"{path}: not a PNG")
    pos, idat, ihdr = 8, [], None
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        ctype = data[pos + 4:pos + 8]
        chunk = data[pos + 8:pos + 8 + length]
        if ctype == b"IHDR":
            ihdr = chunk
        elif ctype == b"IDAT":
            idat.append(chunk)
        elif ctype == b"IEND":
            break
        pos += 12 + length
    if ihdr is None:
        raise ValueError(f"{path}: no IHDR")
    w, h, depth, color, compression, filt, interlace = struct.unpack(">IIBBBBB", ihdr[:13])
    if interlace:
        raise ValueError(f"{path}: interlaced PNG not supported")
    if depth != 8 or color not in (2, 6):
        raise ValueError(f"{path}: unsupported PNG format depth={depth} color={color}")
    nc = 3 if color == 2 else 4
    stride = w * nc
    z = zlib.decompress(b"".join(idat))
    if len(z) < h * (stride + 1):
        raise ValueError(f"{path}: truncated pixel data")
    out = bytearray(h * stride)
    prev = bytearray(stride)
    for y in range(h):
        ft = z[y * (stride + 1)]
        line = bytearray(z[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        if ft == 0:
            pass
        elif ft == 1:  # Sub
            for i in range(nc, stride):
                line[i] = (line[i] + line[i - nc]) & 0xFF
        elif ft == 2:  # Up
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif ft == 3:  # Average
            for i in range(stride):
                a = line[i - nc] if i >= nc else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 0xFF
        elif ft == 4:  # Paeth
            for i in range(stride):
                a = line[i - nc] if i >= nc else 0
                c = prev[i - nc] if i >= nc else 0
                line[i] = (line[i] + _paeth(a, prev[i], c)) & 0xFF
        else:
            raise ValueError(f"{path}: bad filter type {ft} on row {y}")
        out[y * stride:(y + 1) * stride] = line
        prev = line
    return Image(w, h, nc, out)
