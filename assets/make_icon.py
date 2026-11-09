"""Generate PadMix's icon: the 5x5 block "P" (same as the About logo) on a dark tile.

Writes:
  padmix.ico        multi-size Windows icon (16..256, PNG-compressed entries)
  icon_256.rgba     raw 256x256 RGBA for the window/taskbar icon (eframe)
Standard library only: python -I make_icon.py <assets dir>
"""
import struct
import sys
import zlib
from pathlib import Path

P = ["XXXX.", "X...X", "XXXX.", "X....", "X...."]
COLORS = [(124, 92, 255), (34, 211, 166), (245, 165, 36), (239, 68, 68)]  # accent, good, warn, bad
BG = (22, 26, 34)
SS = 4  # supersampling factor for smooth edges


def inside_round_rect(x, y, x0, y0, x1, y1, r):
    if x < x0 or x >= x1 or y < y0 or y >= y1:
        return False
    cx = min(max(x, x0 + r), x1 - r)
    cy = min(max(y, y0 + r), y1 - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def render(n):
    big = n * SS
    px = [[(0, 0, 0, 0)] * big for _ in range(big)]
    # background tile
    r_tile = big * 0.22
    for y in range(big):
        row = px[y]
        for x in range(big):
            if inside_round_rect(x + 0.5, y + 0.5, 0, 0, big, big, r_tile):
                row[x] = BG + (255,)
    # blocks
    area = big * 0.62
    b = area / (5 + 4 * 0.25)
    g = b * 0.25
    ox = (big - area) / 2
    oy = (big - area) / 2
    placed = 0
    for j, line in enumerate(P):
        for i, cell in enumerate(line):
            if cell != "X":
                continue
            col = COLORS[placed % len(COLORS)]
            placed += 1
            x0 = ox + i * (b + g)
            y0 = oy + j * (b + g)
            for y in range(int(y0), int(y0 + b) + 1):
                for x in range(int(x0), int(x0 + b) + 1):
                    if 0 <= x < big and 0 <= y < big and inside_round_rect(x + 0.5, y + 0.5, x0, y0, x0 + b, y0 + b, b * 0.22):
                        px[y][x] = col + (255,)
    # downsample
    out = bytearray()
    for y in range(n):
        for x in range(n):
            acc = [0, 0, 0, 0]
            for sy in range(SS):
                for sx in range(SS):
                    r, g_, b_, a = px[y * SS + sy][x * SS + sx]
                    acc[0] += r * a
                    acc[1] += g_ * a
                    acc[2] += b_ * a
                    acc[3] += a
            a = acc[3]
            if a:
                out += bytes((acc[0] // a, acc[1] // a, acc[2] // a, a // (SS * SS)))
            else:
                out += b"\0\0\0\0"
    return bytes(out)


def png(n, rgba):
    raw = b"".join(b"\0" + rgba[y * n * 4:(y + 1) * n * 4] for y in range(n))

    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)

    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", n, n, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def main():
    out_dir = Path(sys.argv[1] if len(sys.argv) > 1 else ".")
    sizes = [16, 24, 32, 48, 64, 128, 256]
    images = {n: render(n) for n in sizes}
    pngs = [png(n, images[n]) for n in sizes]
    header = struct.pack("<HHH", 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    entries = b""
    for n, data in zip(sizes, pngs):
        entries += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    (out_dir / "padmix.ico").write_bytes(header + entries + b"".join(pngs))
    (out_dir / "icon_256.rgba").write_bytes(images[256])
    (out_dir / "icon_256.png").write_bytes(pngs[-1])
    print("wrote padmix.ico, icon_256.rgba, icon_256.png")


if __name__ == "__main__":
    main()
