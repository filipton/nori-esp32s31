#!/usr/bin/env python3
"""Renders nori's wordmark (nori's docs/brand/nori-wordmark.svg, in its dark colors) to
assets/logo.rgb565: raw RGB565, little-endian, row by row, for the round screen.

    tools/logo.py [path to nori-wordmark.svg]
"""
import io, re, subprocess, sys
from pathlib import Path
from PIL import Image

WIDTH = 184
svg = Path(sys.argv[1] if len(sys.argv) > 1 else Path.home() / "projects/nori/docs/brand/nori-wordmark.svg").read_text()
svg = re.sub(r"<style>.*?</style>", "<style>.nori-ink{fill:#ecebe6}.nori-rice{fill:#000000}</style>", svg, flags=re.S)
png = subprocess.run(["rsvg-convert", "-w", str(WIDTH), "-b", "black"], input=svg.encode(), capture_output=True, check=True).stdout
img = Image.open(io.BytesIO(png)).convert("RGB")
img = img.crop(img.getbbox())
out = bytearray()
raw = img.tobytes()
for i in range(0, len(raw), 3):
    r, g, b = raw[i : i + 3]
    v = (r >> 3) << 11 | (g >> 2) << 5 | b >> 3
    out += v.to_bytes(2, "little")
dest = Path(__file__).resolve().parent.parent / "assets/logo.rgb565"
dest.write_bytes(out)
print(f"{dest}: {img.width}x{img.height}")
