#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Converts Rnote notes (.rnote, the current zstd format and the older gzip one) into Cera notes.

    tools/rnote2cera.py note.rnote [more.rnote ...]     # writes note.cera next to each
    tools/rnote2cera.py book.rnote --pdf book.pdf -o book.cera

Pen and highlighter strokes, lines and rectangles become Cera strokes; text, other shapes and
vector images are skipped and counted. Pages keep Rnote's page size and paper pattern.

Rnote keeps an imported PDF only as a 96 dpi bitmap per page. Without --pdf those bitmaps (and
any other pictures) are packed into a PDF that becomes the pages' background. With --pdf the
original PDF is embedded instead and the ink is mapped onto its pages: sharper and smaller. The
k-th page picture from the top must be page k of the PDF. --pdf needs pdfinfo (poppler-utils).

Strokes are simplified as Cera simplifies stylus input. Needs Python 3.14 (compression.zstd), or
the zstandard module on older versions.
"""

import argparse
import base64
import gzip
import json
import math
import mmap
import os
import re
import struct
import subprocess
import sys
import time
import zlib
from array import array
from collections import Counter
from pathlib import Path

try:
    from compression.zstd import decompress as unzstd
except ImportError:
    import zstandard

    def unzstd(data):
        return zstandard.ZstdDecompressor().decompressobj().decompress(data)


RNOTE_MAGIC = b"RNOTE-\xce\xa6\xce\x9b"
RNOTE_HEADER_LEN_AT = 40

# The .cera layout, from crates/core/src/format.rs and codec.rs.
FORMAT_VERSION = 1
MANIFEST_VERSION = 1
INK_ENCODING = 1
ASSET_ALIGN = 4096
CHUNK_PAGE_INK = 1
CHUNK_MANIFEST = 3
UNITS_PER_PT = 64
MAX_UNITS = (1 << 24) - 1
PEN, HIGHLIGHTER = 0, 1
BLANK, RULED, GRID, DOTS = 0, 1, 2, 3
WHITE = (255, 255, 255, 255)
PAPER_BLANK = (BLANK, 0.0, (0, 0, 0, 0), WHITE)

# Cera's tolerances for stylus input (crates/app/src/canvas/input.rs), in points.
SIMPLIFY_EPS = 0.05
SIMPLIFY_PRESSURE_EPS = 0.03
# Cera's pen is this fraction of its full width at zero pressure (crates/render/src/ink.rs).
MIN_RADIUS = 0.25
BEZIER_STEPS = 8

PATTERNS = {
    "none": BLANK,
    "lines": RULED,
    "grid": GRID,
    "dots": DOTS,
    "isometric_grid": GRID,
    "isometric_dots": DOTS,
}
# Rnote scales the stroke width by curve(pressure).
CURVES = {
    "const": lambda p: 1.0,
    "linear": lambda p: p,
    "sqrt": math.sqrt,
    "cbrt": lambda p: p ** (1 / 3),
    "quadr": lambda p: p * p,
    "cubic": lambda p: p * p * p,
}


def fail(msg):
    raise SystemExit(f"rnote2cera: {msg}")


# --- Reading Rnote -------------------------------------------------------------------------


def read_rnote(path):
    """The document (config and bounds) and an iterator over (stroke, chrono) pairs."""
    with open(path, "rb") as f:
        data = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
    if data[:2] == b"\x1f\x8b":
        # The older format: one gzip-compressed JSON with strokes and their order side by side.
        snapshot = json.loads(gzip.decompress(data))["data"]["engine_snapshot"]
        pairs = zip(snapshot["stroke_components"], snapshot["chrono_components"])
        return snapshot["document"], ((s["value"], c["value"] or {}) for s, c in pairs if s["value"])
    if data[: len(RNOTE_MAGIC)] != RNOTE_MAGIC:
        fail(f"{path}: not an Rnote file")
    (header_len,) = struct.unpack_from("<I", data, RNOTE_HEADER_LEN_AT)
    pos = RNOTE_HEADER_LEN_AT + 4
    header = json.loads(data[pos : pos + header_len])
    pos += header_len
    method = header["compression_method"]
    if isinstance(method, dict) and "zstd" in method:
        unpack = unzstd
    elif method == "none":
        unpack = bytes
    else:
        fail(f"{path}: unknown compression {method}")
    sections = []
    for info in [header["core_info"], *header["chunk_info_vec"]]:
        sections.append((pos, info["c_size"]))
        pos += info["c_size"]
    if pos > len(data):
        fail(f"{path}: truncated")
    start, size = sections[0]
    document = json.loads(unpack(data[start : start + size]))["document"]

    def items():
        for start, size in sections[1:]:
            yield from json.loads(unpack(data[start : start + size]))

    return document, items()


def rgba8(c):
    return tuple(round(min(max(c[k], 0.0), 1.0) * 255) for k in "rgba")


def layer_rank(layer):
    """Rnote's draw order: document (pictures), then highlighters, then user layers."""
    if layer == "document":
        return 0
    if layer == "highlighter":
        return 1
    return 2 + layer.get("user_layer", 0)


def transform_of(r):
    """A rectangle's transform as (a, b, c, d, e, f); older files hold a column-major 3×3."""
    m = r["affine"] if "affine" in r else r["transform"]["affine"]
    return m if len(m) == 6 else [m[0], m[1], m[3], m[4], m[6], m[7]]


def affine(m, x, y):
    a, b, c, d, e, f = m
    return a * x + c * y + e, b * x + d * y + f


def bezier(ctrl, t):
    pts = list(ctrl)
    while len(pts) > 1:
        pts = [(p[0] + (q[0] - p[0]) * t, p[1] + (q[1] - p[1]) * t) for p, q in zip(pts, pts[1:])]
    return pts[0]


def brush_points(path, curve):
    """The path flattened to x, y, pressure triples, pressure already mapped for Cera's pen."""
    pts = array("d")

    def add(pos, pressure):
        width = curve(min(max(pressure, 0.0), 1.0))
        pts.extend((pos[0], pos[1], min(max((width - MIN_RADIUS) / (1 - MIN_RADIUS), 0.0), 1.0)))

    prev = path["start"]
    add(prev["pos"], prev["pressure"])
    for seg in path["segments"]:
        ((_, s),) = seg.items()
        end = s["end"]
        cps = [s[k] for k in sorted(s) if k.startswith("cp")]
        if cps:
            ctrl = [prev["pos"], *cps, end["pos"]]
            for i in range(1, BEZIER_STEPS):
                t = i / BEZIER_STEPS
                add(bezier(ctrl, t), prev["pressure"] + (end["pressure"] - prev["pressure"]) * t)
        add(end["pos"], end["pressure"])
        prev = end
    return pts


def shape_points(shape):
    """A line or rectangle as full-pressure points, or None for other shapes."""
    ((kind, s),) = shape.items()
    if kind == "line":
        corners = [s["start"], s["end"]]
    elif kind == "rect":
        hx, hy = s["cuboid"]["half_extents"]
        corners = [affine(transform_of(s), x, y) for x, y in [(-hx, -hy), (hx, -hy), (hx, hy), (-hx, hy), (-hx, -hy)]]
    else:
        return None
    pts = array("d")
    for x, y in corners:
        pts.extend((x, y, 1.0))
    return pts


class Rect:
    """An Rnote rectangle: a centred box under an affine transform, in document pixels."""

    def __init__(self, r):
        self.hx, self.hy = r["cuboid"]["half_extents"]
        self.m = transform_of(r)
        self.cx, self.cy = self.m[4], self.m[5]
        self.axis_aligned = abs(self.m[1]) < 1e-6 and abs(self.m[2]) < 1e-6

    def pdf_matrix(self, ox, oy, scale, page_h):
        """The PDF `cm` that draws an image over this rectangle on a page with origin (ox, oy)."""

        def to_pdf(x, y):
            px, py = affine(self.m, x, y)
            return (px - ox) * scale, page_h - (py - oy) * scale

        (x0, y0), (x1, y1), (x2, y2) = to_pdf(-self.hx, self.hy), to_pdf(self.hx, self.hy), to_pdf(-self.hx, -self.hy)
        return (x1 - x0, y1 - y0, x2 - x0, y2 - y0, x0, y0)


# --- Writing PDF -------------------------------------------------------------------------


class PdfBuilder:
    """Packs Rnote's pictures into a PDF, written straight into the note at the asset position."""

    def __init__(self, out):
        self.out = out
        self.start = None
        self.offsets = {}
        self.next_id = 3  # 1 is the catalog, 2 the page tree
        self.pages = {}  # rnote page index -> [(image object, cm matrix)]

    def _write(self, b):
        self.out.write(b)
        self.crc = zlib.crc32(b, self.crc)

    def _obj(self, body, stream=None, oid=None):
        if oid is None:
            oid, self.next_id = self.next_id, self.next_id + 1
        self.offsets[oid] = self.out.tell() - self.start
        if stream is None:
            self._write(f"{oid} 0 obj\n{body}\nendobj\n".encode())
        else:
            self._write(f"{oid} 0 obj\n<< {body} /Length {len(stream)} >>\nstream\n".encode())
            self._write(stream)
            self._write(b"\nendstream\nendobj\n")
        return oid

    def _image(self, w, h, colors, data):
        return f"/Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /{colors} /BitsPerComponent 8 /Filter /FlateDecode", zlib.compress(data, 6)

    def add(self, page, image, matrix):
        """Adds a bitmap image; returns False if its encoding isn't one we read."""
        w, h = image["pixel_width"], image["pixel_height"]
        fmt = image["memory_format"].lower()
        encoding = image.get("encoding")
        if encoding not in (None, "Zstd") or not fmt.startswith(("r8g8b8a8", "b8g8r8a8")):
            return False
        raw = base64.b64decode(image["data"])
        if encoding == "Zstd":
            raw = unzstd(raw)
        if len(raw) != w * h * 4:
            return False
        if self.start is None:
            self.out.write(bytes(-self.out.tell() % ASSET_ALIGN))
            self.start, self.crc = self.out.tell(), 0
            self._write(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
        rgb = bytearray(w * h * 3)
        for i, c in enumerate((0, 1, 2) if fmt.startswith("r") else (2, 1, 0)):
            rgb[i::3] = raw[c::4]
        alpha = raw[3::4]
        body, data = self._image(w, h, "DeviceRGB", rgb)
        del rgb
        if alpha.count(255) != w * h:
            # Rnote's colours are premultiplied; semi-transparent edges come out a little dark.
            mask = self._obj(*self._image(w, h, "DeviceGray", alpha))
            body += f" /SMask {mask} 0 R"
        self.pages.setdefault(page, []).append((self._obj(body, data), matrix))
        return True

    def finish(self, page_size):
        """Writes the pages; returns ((offset, len, crc), {rnote page: PDF page}) or None."""
        if self.start is None:
            return None
        w, h = page_size
        kids, index = [], {}
        for k, page in enumerate(sorted(self.pages)):
            index[page] = k
            images = self.pages[page]
            content = "".join(
                "q " + " ".join(f"{v:.4f}" for v in m) + f" cm /Im{j} Do Q\n" for j, (_, m) in enumerate(images)
            ).encode()
            xobjects = " ".join(f"/Im{j} {oid} 0 R" for j, (oid, _) in enumerate(images))
            contents = self._obj("", content)
            kids.append(
                self._obj(
                    f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w:.4f} {h:.4f}]"
                    f" /Resources << /XObject << {xobjects} >> >> /Contents {contents} 0 R >>"
                )
            )
        self._obj(f"<< /Type /Pages /Kids [{' '.join(f'{k} 0 R' for k in kids)}] /Count {len(kids)} >>", oid=2)
        self._obj("<< /Type /Catalog /Pages 2 0 R >>", oid=1)
        xref = self.out.tell() - self.start
        lines = [f"xref\n0 {self.next_id}\n", "0000000000 65535 f \n"]
        lines += [f"{self.offsets[i]:010d} 00000 n \n" for i in range(1, self.next_id)]
        lines.append(f"trailer\n<< /Size {self.next_id} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
        self._write("".join(lines).encode())
        return (self.start, self.out.tell() - self.start, self.crc), index


def pdf_page_sizes(pdf):
    """Page sizes in points, rotated as Poppler (and so Cera's import) reports them."""

    def pdfinfo(*args):
        try:
            return subprocess.run(["pdfinfo", *args, str(pdf)], capture_output=True, text=True, check=True).stdout
        except FileNotFoundError:
            fail("--pdf needs pdfinfo (poppler-utils)")
        except subprocess.CalledProcessError as e:
            fail(f"{pdf}: {e.stderr.strip()}")

    count = int(re.search(r"^Pages:\s+(\d+)", pdfinfo(), re.M).group(1))
    info = pdfinfo("-f", "1", "-l", str(count))
    sizes = {int(n): (float(w), float(h)) for n, w, h in re.findall(r"^Page\s+(\d+) size:\s+([\d.]+) x ([\d.]+)", info, re.M)}
    rots = {int(n): int(r) for n, r in re.findall(r"^Page\s+(\d+) rot:\s+(\d+)", info, re.M)}
    out = []
    for n in range(1, count + 1):
        w, h = sizes[n]
        out.append((h, w) if rots.get(n, 0) % 180 == 90 else (w, h))
    return out


# --- Writing Cera ------------------------------------------------------------------------


def varint(out, v):
    while v >= 0x80:
        out.append(v & 0x7F | 0x80)
        v >>= 7
    out.append(v)


def units(v):
    """1/64 pt, rounded half away from zero like Rust's `f32::round`."""
    u = min(math.floor(abs(v) * UNITS_PER_PT + 0.5), MAX_UNITS)
    return -u if v < 0 else u


def zigzag(v):
    return 2 * v if v >= 0 else -2 * v - 1


def simplify(pts, eps=SIMPLIFY_EPS, pressure_eps=SIMPLIFY_PRESSURE_EPS):
    """Ramer–Douglas–Peucker over (x, y, pressure), as crates/core/src/simplify.rs."""
    n = len(pts)
    if n <= 2:
        return pts
    keep = [False] * n
    keep[0] = keep[-1] = True
    spans = [(0, n - 1)]
    while spans:
        a, b = spans.pop()
        ax, ay, ap = pts[a]
        bx, by, bp = pts[b]
        dx, dy = bx - ax, by - ay
        len2 = dx * dx + dy * dy
        worst, at = 0.0, 0
        for i in range(a + 1, b):
            px, py, pp = pts[i]
            t = min(max(((px - ax) * dx + (py - ay) * dy) / len2, 0.0), 1.0) if len2 > 0 else 0.0
            excess = max(
                math.hypot(px - ax - t * dx, py - ay - t * dy) / eps,
                abs(pp - ap - t * (bp - ap)) / pressure_eps,
            )
            if excess > worst:
                worst, at = excess, i
        if worst > 1.0:
            keep[at] = True
            spans += [(a, at), (at, b)]
    return [p for p, k in zip(pts, keep) if k]


def page_ink(page_id, strokes):
    out = bytearray([INK_ENCODING])
    varint(out, page_id)
    varint(out, len(strokes))
    for tool, color, width, pts in strokes:
        out.append(tool)
        out += bytes(color)
        varint(out, max(units(width), 0))
        varint(out, len(pts))
        px = py = 0
        for x, y, p in pts:
            ux, uy = units(x), units(y)
            varint(out, zigzag(ux - px))
            varint(out, zigzag(uy - py))
            out.append(round(min(max(p, 0.0), 1.0) * 255))
            px, py = ux, uy
    return out


def chunk(kind, payload):
    head = struct.pack("<BI", kind, len(payload))
    return head + struct.pack("<I", zlib.crc32(payload, zlib.crc32(head))) + payload


def manifest(pages, asset):
    out = bytearray()
    varint(out, MANIFEST_VERSION)
    varint(out, len(pages))  # next_page_id
    varint(out, 1 if asset else 0)
    if asset:
        offset, length, crc = asset
        for v in (0, 0, offset, length, crc):  # id, kind Pdf, offset, len, crc32
            varint(out, v)
    varint(out, len(pages))
    for page_id, (size, paper, background, ink) in enumerate(pages):
        varint(out, page_id)
        out += struct.pack("<ff", *size)
        kind, spacing, line, fill = paper
        varint(out, kind)
        out += struct.pack("<f", spacing) + bytes(line) + bytes(fill)
        if background is None:
            varint(out, 0)
        else:
            varint(out, 1)
            varint(out, 0)  # asset id
            varint(out, background)
        if ink is None:
            out.append(0)
        else:
            out.append(1)
            varint(out, ink[0])
            varint(out, ink[1])
    return out


# --- Converting --------------------------------------------------------------------------


def convert(src, dst, pdf=None):
    started = time.monotonic()
    document, items = read_rnote(src)
    config = document["config"]
    fmt, bg = config["format"], config["background"]
    fw, fh, scale = fmt["width"], fmt["height"], 72 / fmt["dpi"]
    doc_x, doc_y = document["x"], document["y"]
    if config.get("layout") not in ("fixed_size", "continuous_vertical"):
        print(f"  {config.get('layout')} layout: ink left or right of the page column ends up off the page", file=sys.stderr)
    paper = (
        PATTERNS.get(bg["pattern"], BLANK),
        bg["pattern_size"][1] * scale,
        rgba8(bg["pattern_color"]),
        rgba8(bg["color"]),
    )
    pdf_sizes = pdf_page_sizes(pdf) if pdf else None

    def page_of(y):
        return math.floor((y - doc_y) / fh)

    tmp = dst.with_name(f".{dst.name}.tmp")
    out = open(tmp, "wb")
    try:
        header = b"CERANOTE" + struct.pack("<II", FORMAT_VERSION, 0) + bytes(44)
        out.write(header + struct.pack("<I", zlib.crc32(header)))
        asset = None
        if pdf:
            out.write(bytes(-out.tell() % ASSET_ALIGN))
            start, crc = out.tell(), 0
            with open(pdf, "rb") as f:
                while block := f.read(1 << 20):
                    out.write(block)
                    crc = zlib.crc32(block, crc)
            asset = (start, out.tell() - start, crc)
        builder = PdfBuilder(out)

        def page_origin(i):
            return doc_x, doc_y + i * fh

        strokes, pictures, skipped = [], [], Counter()
        for stroke, chrono in items:
            ((kind, s),) = stroke.items()
            order = (layer_rank(chrono.get("layer", "document")), chrono.get("t", 0))
            if kind in ("brushstroke", "shapestroke"):
                style = next(iter(s["style"].values()))
                if kind == "brushstroke":
                    pts = brush_points(s["path"], CURVES.get(style.get("pressure_curve"), CURVES["const"]))
                else:
                    pts = shape_points(s["shape"])
                    if pts is None:
                        skipped[f"{next(iter(s['shape']))} shape"] += 1
                        continue
                tool = HIGHLIGHTER if style.get("is_highlighter") else PEN
                strokes.append((order, tool, rgba8(style["stroke_color"]), style["stroke_width"], pts))
            elif kind in ("bitmapimage", "vectorimage"):
                rect = Rect(s["rectangle"])
                page = max(page_of(rect.cy), 0)
                if pdf:
                    pictures.append((page, rect))
                elif kind == "vectorimage":
                    skipped["vector image (try --pdf)"] += 1
                else:
                    ox, oy = page_origin(page)
                    if not builder.add(page, s["image"], rect.pdf_matrix(ox, oy, scale, fh * scale)):
                        skipped[f"{s['image'].get('encoding')} image"] += 1
            else:
                skipped[kind.removesuffix("stroke")] += 1

        built = builder.finish((fw * scale, fh * scale))
        if built:
            asset, built_index = built

        # Pages: Rnote's page grid, extended to cover all ink and pictures.
        used = [page_of(pts[i]) for *_, pts in strokes for i in range(1, len(pts), 3)]
        used += [p for p, _ in pictures]
        if built:
            used += list(built_index)
        if min(used, default=0) < 0:
            print("  ink above the first page is cut off", file=sys.stderr)
        n_pages = max(math.ceil((document["height"]) / fh - 0.01), max(used, default=0) + 1, 1)
        specs = [((fw * scale, fh * scale), paper, None) for _ in range(n_pages)]
        transforms = [(*page_origin(i), scale, scale) for i in range(n_pages)]
        if built:
            for page, k in built_index.items():
                specs[page] = (specs[page][0], paper, k)
        if pdf:
            pictures.sort(key=lambda p: p[1].cy)
            if len(pictures) != len(pdf_sizes):
                fail(f"{src} has {len(pictures)} page pictures but {pdf} has {len(pdf_sizes)} pages")
            for k, (page, rect) in enumerate(pictures):
                if specs[page][2] is not None or not rect.axis_aligned:
                    fail(f"{src}: page {page + 1} has two pictures or a rotated one; convert without --pdf")
                w, h = pdf_sizes[k]
                specs[page] = ((w, h), PAPER_BLANK, k)
                transforms[page] = (rect.cx - rect.hx, rect.cy - rect.hy, w / (2 * rect.hx), h / (2 * rect.hy))

        # Ink, split where a stroke crosses into another page and mapped to page points.
        per_page = [[] for _ in range(n_pages)]
        points_in = points_out = 0
        strokes.sort(key=lambda s: s[0])
        for _, tool, color, width, pts in strokes:
            n = len(pts) // 3
            points_in += n
            pages = [min(max(page_of(pts[3 * i + 1]), 0), n_pages - 1) for i in range(n)]
            start = 0
            for i in range(1, n + 1):
                if i < n and pages[i] == pages[start]:
                    continue
                page = pages[start]
                ox, oy, sx, sy = transforms[page]
                # One point past each end, so the stroke reaches the page edge.
                run = [
                    ((pts[3 * j] - ox) * sx, (pts[3 * j + 1] - oy) * sy, pts[3 * j + 2])
                    for j in range(max(start - 1, 0), min(i + 1, n))
                ]
                run = simplify(run)
                points_out += len(run)
                per_page[page].append((tool, color, width * (sx + sy) / 2, run))
                start = i

        pages = []
        for page_id, ((size, page_paper, background), ink) in enumerate(zip(specs, per_page)):
            ref = None
            if ink:
                data = chunk(CHUNK_PAGE_INK, page_ink(page_id, ink))
                ref = (out.tell(), len(data))
                out.write(data)
            pages.append((size, page_paper, background, ref))
        data = chunk(CHUNK_MANIFEST, manifest(pages, asset))
        manifest_ref = (out.tell(), len(data))
        out.write(data)
        out.flush()
        os.fsync(out.fileno())
        footer = b"CERAFOOT" + struct.pack("<QQI", 1, *manifest_ref)
        out.write(footer + struct.pack("<I", zlib.crc32(footer)))
        out.flush()
        os.fsync(out.fileno())
        out.close()
        os.replace(tmp, dst)
    except BaseException:
        out.close()
        tmp.unlink(missing_ok=True)
        raise

    backgrounds = sum(s[2] is not None for s in specs)
    print(
        f"  {n_pages} pages ({backgrounds} with a {'PDF' if pdf else 'picture'} background), {len(strokes)} strokes, "
        f"{points_in} → {points_out} points, {dst.stat().st_size / 1e6:.1f} MB in {time.monotonic() - started:.1f} s"
    )
    if skipped:
        print("  skipped: " + ", ".join(f"{n} {k}" for k, n in skipped.items()))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("notes", nargs="+", type=Path, metavar="NOTE.rnote")
    ap.add_argument("-o", "--output", type=Path, help="output file (one note only)")
    ap.add_argument("--pdf", type=Path, help="the PDF the note was made from (one note only)")
    ap.add_argument("-f", "--force", action="store_true", help="replace existing .cera files")
    args = ap.parse_args()
    if len(args.notes) > 1 and (args.output or args.pdf):
        ap.error("--output and --pdf take one note")
    for src in args.notes:
        dst = args.output or src.with_suffix(".cera")
        if dst.exists() and not args.force:
            fail(f"{dst} exists; pass --force to replace it")
        print(f"{src} → {dst}")
        convert(src, dst, args.pdf)


if __name__ == "__main__":
    main()
