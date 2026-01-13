#!/usr/bin/env python3
"""Generate tiny OpenType font fixtures to test line metric selection.

The generated font intentionally sets different OS/2 typographic metrics vs hhea metrics, while
allowing the OS/2.fsSelection.USE_TYPO_METRICS bit to be toggled.

This lets the Rust side assert that we follow FreeType/browser metric selection rules: prefer hhea
metrics unless USE_TYPO_METRICS is enabled, in which case prefer OS/2 typographic metrics.
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


def build_font(path: Path, *, use_typo_metrics: bool) -> None:
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
  # The USE_TYPO_METRICS bit is specified in OS/2 table version 4+. Set the version explicitly so
  # toggling it is well-defined.
  fb.font["OS/2"].version = 4
  # Toggle OS/2.fsSelection.USE_TYPO_METRICS (bit 7).
  fb.font["OS/2"].fsSelection = 0x80 if use_typo_metrics else 0

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
  out_dir = Path(__file__).resolve().parent
  out_unset = out_dir / "line-metrics-selection-test.ttf"
  out_set = out_dir / "line-metrics-selection-test-use-typo.ttf"
  build_font(out_unset, use_typo_metrics=False)
  build_font(out_set, use_typo_metrics=True)
  print(f"Wrote {out_unset}")
  print(f"Wrote {out_set}")


if __name__ == "__main__":
  main()
