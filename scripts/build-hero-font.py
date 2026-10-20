#!/usr/bin/env python3
"""Build Duplex Dots from bundled OFL Doto; only the zero outline is changed.

Requires fontTools (python3-fonttools). Runtime and ordinary Rust builds use the
committed TTF and do not require Python or fontTools.
"""
from pathlib import Path
from fontTools.ttLib import TTFont
from fontTools.ttLib.tables._g_l_y_f import Glyph, GlyphCoordinates

ROOT = Path(__file__).resolve().parents[1]
FONTS = ROOT / "crates/duplex-gui/assets/fonts"
font = TTFont(FONTS / "Doto-Bold.ttf", recalcTimestamp=False)
glyf = font["glyf"]
zero_name = font.getBestCmap()[ord("0")]
original = glyf[zero_name]
coordinates, ends, flags = original.getCoordinates(glyf)
# Reuse the original top dot's exact contour, including on/off-curve points.
dot = coordinates[:ends[0] + 1]
dot_flags = flags[:ends[0] + 1]
zero = Glyph()
zero.numberOfContours = 0
zero.coordinates = GlyphCoordinates()
zero.endPtsOfContours = []
zero.flags = type(flags)()
for row, pattern in enumerate(("01110", "10001", "10001", "10001", "10001", "10001", "01110")):
    for column, filled in enumerate(pattern):
        if filled == "0":
            continue
        zero.coordinates.extend((x + (column - 2) * 100, y - row * 100) for x, y in dot)
        zero.flags.extend(dot_flags)
        zero.endPtsOfContours.append(len(zero.coordinates) - 1)
        zero.numberOfContours += 1
zero.program = original.program
glyf[zero_name] = zero
zero.recalcBounds(glyf)
# Keep the original advance, side bearings, cap height and every other glyph.
assert font["hmtx"][zero_name] == (600, 13)
assert (zero.xMin, zero.yMin, zero.xMax, zero.yMax) == (13, 13, 488, 688)
for record in font["name"].names:
    names = {
        1: "Duplex Dots", 2: "Bold", 3: "Duplex Dots Bold 1.000",
        4: "Duplex Dots Bold", 6: "DuplexDots-Bold",
        16: "Duplex Dots", 17: "Bold",
    }
    if record.nameID in names:
        record.string = names[record.nameID].encode(record.getEncoding())
font.save(FONTS / "DuplexDots-Bold.ttf")
