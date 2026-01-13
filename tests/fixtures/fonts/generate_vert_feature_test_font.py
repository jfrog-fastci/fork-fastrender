#!/usr/bin/env python3
"""Generate a tiny OpenType font fixture to test vertical alternates substitution.

The shaping pipeline auto-injects `vert`/`vrt2` OpenType features for vertical runs
(`src/text/pipeline.rs::shape_font_run`). This fixture provides a deterministic GSUB
substitution so tests can assert that vertical shaping actually performs the
substitution (not just that the features are present).
"""

from __future__ import annotations

from pathlib import Path

try:
  from fontTools.feaLib.builder import addOpenTypeFeaturesFromString
  from fontTools.fontBuilder import FontBuilder
  from fontTools.pens.ttGlyphPen import TTGlyphPen
except ImportError as exc:
  raise SystemExit(
    "Missing dependency: fontTools.\n\n"
    "Regenerate font fixtures by installing the pinned Python deps.\n"
    "From the repository root:\n"
    "  python3 -m venv .venv && . .venv/bin/activate\n"
    "  pip install -r tests/fixtures/fonts/requirements.txt\n"
  ) from exc


UNITS_PER_EM = 1000
ASCENT = 800
DESCENT = -200
ADVANCE = 1000


def rect_glyph(x0: int, y0: int, x1: int, y1: int):
  pen = TTGlyphPen(None)
  pen.moveTo((x0, y0))
  pen.lineTo((x1, y0))
  pen.lineTo((x1, y1))
  pen.lineTo((x0, y1))
  pen.closePath()
  return pen.glyph()


def build_font(path: Path) -> None:
  fb = FontBuilder(UNITS_PER_EM, isTTF=True)

  # Keep the glyph order minimal and stable so glyph IDs are deterministic:
  #   0: .notdef
  #   1: A
  #   2: A.vert
  glyph_order = [".notdef", "A", "A.vert"]
  fb.setupGlyphOrder(glyph_order)
  fb.setupCharacterMap({ord("A"): "A"})

  glyphs = {
    ".notdef": rect_glyph(100, 0, 900, 800),
    # Base glyph for U+0041.
    "A": rect_glyph(150, 0, 850, 700),
    # Alternate glyph substituted via `vert`/`vrt2`. Make it distinct in outline.
    "A.vert": rect_glyph(250, 0, 750, 900),
  }
  fb.setupGlyf(glyphs)

  fb.setupHorizontalMetrics({name: (ADVANCE, 0) for name in glyph_order})
  fb.setupHorizontalHeader(ascent=ASCENT, descent=DESCENT)
  fb.setupOS2(
    sTypoAscender=ASCENT,
    sTypoDescender=DESCENT,
    usWinAscent=ASCENT,
    usWinDescent=-DESCENT,
    usWeightClass=400,
  )

  fb.setupNameTable(
    {
      "familyName": "Vert Feature Test",
      "styleName": "Regular",
      "fullName": "Vert Feature Test Regular",
      "uniqueFontIdentifier": "Vert Feature Test Regular",
      "psName": "VertFeatureTest-Regular",
      "version": "Version 1.0",
      "licenseDescription": "Public Domain / CC0",
      "licenseInfoURL": "https://creativecommons.org/publicdomain/zero/1.0/",
    }
  )
  fb.setupPost()
  fb.setupMaxp()
  fb.setupHead()

  # Build a minimal GSUB table. Both `vert` and `vrt2` map A -> A.vert so the
  # test can cover either feature tag being applied.
  addOpenTypeFeaturesFromString(
    fb.font,
    """
languagesystem DFLT dflt;

feature vert {
  sub A by A.vert;
} vert;

feature vrt2 {
  sub A by A.vert;
} vrt2;
""",
  )

  # Deterministic timestamps (seconds since 1904-01-01).
  fb.font["head"].created = 0
  fb.font["head"].modified = 0

  fb.save(path)


def main() -> None:
  out = Path(__file__).resolve().parent / "vert-feature-test.ttf"
  build_font(out)
  print(f"Wrote {out}")


if __name__ == "__main__":
  main()

