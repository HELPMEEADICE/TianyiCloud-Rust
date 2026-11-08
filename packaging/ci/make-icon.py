#!/usr/bin/env python3
"""生成打包用的占位图标（纯标准库实现，不依赖 Pillow / ImageMagick）。

仓库里目前没有品牌图标资源，而三端打包都需要一个图标文件：

* AppImage —— AppDir 里的 ``.DirIcon`` / hicolor 图标
* macOS    —— ``TianyiCloud.app/Contents/Resources/AppIcon.icns``
* Windows  —— MSI 的 ``ARPPRODUCTICON``（``.ico``）

所以这里用固定算法画一张 512x512 的图标（圆角渐变底 + 白色云朵），并导出
png / ico / icns 三种格式。将来要换正式图标，替换生成的同名文件即可。

用法::

    python packaging/ci/make-icon.py <输出目录>
"""

from __future__ import annotations

import pathlib
import struct
import sys
import zlib

# Windows 上 stdout 默认是 cp1252，直接打印非 ASCII 会 UnicodeEncodeError
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

SIZE = 512
CORNER = 96
BG_TOP = (43, 99, 235)
BG_BOTTOM = (18, 184, 166)
CLOUD = (255, 255, 255)

# 云朵由三个圆 + 一个圆角矩形合成，坐标相对画布中心
CLOUD_CIRCLES = ((-84.0, 24.0, 72.0), (0.0, -12.0, 94.0), (86.0, 24.0, 66.0))
CLOUD_BAR = (0.0, 30.0, 108.0, 66.0, 40.0)  # cx, cy, half_w, half_h, radius


def _clamp01(value: float) -> float:
    if value <= 0.0:
        return 0.0
    if value >= 1.0:
        return 1.0
    return value


def _sd_round_box(px: float, py: float, half_w: float, half_h: float, radius: float) -> float:
    """圆角矩形的有符号距离场（内部为负）。"""
    qx = abs(px) - max(half_w - radius, 0.0)
    qy = abs(py) - max(half_h - radius, 0.0)
    ax = qx if qx > 0.0 else 0.0
    ay = qy if qy > 0.0 else 0.0
    outside = (ax * ax + ay * ay) ** 0.5
    inside = min(max(qx, qy), 0.0)
    return outside + inside - radius


def _coverage(sdf: float) -> float:
    """有符号距离 -> 覆盖率（保留 1px 抗锯齿）。"""
    return _clamp01(0.5 - sdf)


def _cloud_coverage(px: float, py: float) -> float:
    best = float("inf")
    for cx, cy, cr in CLOUD_CIRCLES:
        dx = px - cx
        dy = py - cy
        best = min(best, (dx * dx + dy * dy) ** 0.5 - cr)
    cx, cy, half_w, half_h, radius = CLOUD_BAR
    best = min(best, _sd_round_box(px - cx, py - cy, half_w, half_h, radius))
    return _coverage(best)


def render_rows(size: int) -> list[bytes]:
    """渲染 RGBA 行数据。"""
    half = size / 2.0
    rows: list[bytes] = []
    for y in range(size):
        py = y + 0.5 - half
        row = bytearray()
        for x in range(size):
            px = x + 0.5 - half
            bg = _coverage(_sd_round_box(px, py, half, half, CORNER * size / SIZE))
            if bg <= 0.0:
                row += b"\x00\x00\x00\x00"
                continue
            t = (x + y) / (2.0 * (size - 1))
            r = BG_TOP[0] + (BG_BOTTOM[0] - BG_TOP[0]) * t
            g = BG_TOP[1] + (BG_BOTTOM[1] - BG_TOP[1]) * t
            b = BG_TOP[2] + (BG_BOTTOM[2] - BG_TOP[2]) * t
            scale = size / SIZE
            cloud = _cloud_coverage(px / scale * 1.0, py / scale * 1.0)
            r = r + (CLOUD[0] - r) * cloud
            g = g + (CLOUD[1] - g) * cloud
            b = b + (CLOUD[2] - b) * cloud
            row += bytes((int(r + 0.5), int(g + 0.5), int(b + 0.5), int(bg * 255 + 0.5)))
        rows.append(bytes(row))
    return rows


def encode_png(size: int, rows: list[bytes]) -> bytes:
    raw = b"".join(b"\x00" + row for row in rows)

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def downsample(rows: list[bytes], size: int) -> list[bytes]:
    """2x2 盒式采样缩小一半。"""
    half = size // 2
    out: list[bytes] = []
    for y in range(0, size, 2):
        top, bottom = rows[y], rows[y + 1]
        row = bytearray(half * 4)
        for x in range(0, size, 2):
            i0 = x * 4
            i1 = i0 + 4
            o = (x // 2) * 4
            for c in range(4):
                row[o + c] = (top[i0 + c] + top[i1 + c] + bottom[i0 + c] + bottom[i1 + c] + 2) // 4
        out.append(bytes(row))
    return out


def encode_ico(png_256: bytes) -> bytes:
    """单条目 ICO（256x256，PNG 载荷，Windows Vista+ 支持）。"""
    header = struct.pack("<HHH", 0, 1, 1)
    entry = struct.pack("<BBBBHHII", 0, 0, 0, 0, 1, 32, len(png_256), 6 + 16)
    return header + entry + png_256


def encode_icns(png_512: bytes, png_256: bytes) -> bytes:
    """ICNS：ic09 = 512x512，ic08 = 256x256（均为 PNG 载荷）。"""
    body = b""
    for tag, payload in ((b"ic09", png_512), (b"ic08", png_256)):
        body += tag + struct.pack(">I", len(payload) + 8) + payload
    return b"icns" + struct.pack(">I", len(body) + 8) + body


def main(argv: list[str]) -> int:
    out_dir = pathlib.Path(argv[1] if len(argv) > 1 else "dist/icons")
    out_dir.mkdir(parents=True, exist_ok=True)

    rows = render_rows(SIZE)
    png_512 = encode_png(SIZE, rows)
    rows_256 = downsample(rows, SIZE)
    png_256 = encode_png(SIZE // 2, rows_256)

    files = {
        "icon.png": png_512,
        "icon-256.png": png_256,
        "icon.ico": encode_ico(png_256),
        "icon.icns": encode_icns(png_512, png_256),
    }
    for name, data in files.items():
        path = out_dir / name
        path.write_bytes(data)
        print(f"[icon] {path} ({len(data)} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
