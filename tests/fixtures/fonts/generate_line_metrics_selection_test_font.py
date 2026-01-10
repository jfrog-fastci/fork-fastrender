#!/usr/bin/env python3
"""Generate a tiny OpenType font fixture to test line metric selection.

The generated font intentionally sets different OS/2 typographic metrics vs hhea metrics, while
leaving the OS/2.fsSelection.USE_TYPO_METRICS bit *unset*.

This lets the Rust side assert that we follow FreeType/browser metric selection rules: prefer hhea
metrics unless USE_TYPO_METRICS is enabled.
"""

from __future__ import annotations

from pathlib import Path

from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen


UNITS_PER_EM = 1000

# OS/2 typographic metrics (sTypo*)
OS2_ASCENT = 800
OS2_DESCENT = -200
OS2_LINE_GAP = 0

# hhea metrics (used when USE_TYPO_METRICS is not set)
HHEA_ASCENT = 900
HHEA_DESCENT = -300
HHEA_LINE_GAP = 100


def rect(x0: int, y0: int, x1: int, y1: int):
  pen = TTGlyphPen(None)
  pen.moveTo((x0, y0))
  pen.lineTo((x1, y0))
  pen.lineTo((x1, y1))
  pen.lineTo((x0, y1))
  pen.closePath()
  return pen.glyph()


def empty_glyph():
  return TTGlyphPen(None).glyph()


def build_font(path: Path) -> None:
  fb = FontBuilder(UNITS_PER_EM, isTTF=True)
  glyph_order = [".notdef", "space", "A"]
  fb.setupGlyphOrder(glyph_order)
  fb.setupCharacterMap({ord(" "): "space", ord("A"): "A"})

  glyphs = {
    ".notdef": rect(100, 0, 900, 800),
    "space": empty_glyph(),
    "A": rect(160, 0, 840, 820),
  }
  fb.setupGlyf(glyphs)

  advance = 1000
  fb.setupHorizontalMetrics(
    {
      ".notdef": (advance, 0),
      "space": (advance // 2, 0),
      "A": (advance, 0),
    }
  )

  fb.setupHorizontalHeader(ascent=HHEA_ASCENT, descent=HHEA_DESCENT)
  fb.font["hhea"].lineGap = HHEA_LINE_GAP

  fb.setupOS2(
    sTypoAscender=OS2_ASCENT,
    sTypoDescender=OS2_DESCENT,
    sTypoLineGap=OS2_LINE_GAP,
    usWinAscent=max(OS2_ASCENT, HHEA_ASCENT),
    usWinDescent=max(abs(OS2_DESCENT), abs(HHEA_DESCENT)),
    usWeightClass=400,
  )
  # Ensure USE_TYPO_METRICS stays unset.
  fb.font["OS/2"].fsSelection = 0

  fb.setupPost()
  fb.setupMaxp()
  fb.setupHead()
  fb.font["head"].created = 0
  fb.font["head"].modified = 0

  fb.setupNameTable(
    {
      "familyName": "Line Metrics Selection Test",
      "styleName": "Regular",
      "fullName": "Line Metrics Selection Test Regular",
      "uniqueFontIdentifier": "Line Metrics Selection Test Regular",
      "psName": "LineMetricsSelectionTest-Regular",
      "version": "Version 1.0",
      "licenseDescription": "Public Domain / CC0",
      "licenseInfoURL": "https://creativecommons.org/publicdomain/zero/1.0/",
    }
  )

  fb.save(path)


def main() -> None:
  out = Path(__file__).resolve().parent / "line-metrics-selection-test.ttf"
  build_font(out)
  print(f"Wrote {out}")


if __name__ == "__main__":
  main()

