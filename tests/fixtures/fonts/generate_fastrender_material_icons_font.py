#!/usr/bin/env python3
"""
Generate a tiny deterministic "Material Icons" fixture font.

The britannica.com fixture (and other pageset pages) rely on Google's Material Icons
ligature font. Offline fixtures commonly miss the upstream font files, causing icon
names like "menu" to render as plain text.

This generator creates a minimal TrueType font that:
  - Exposes family name "Material Icons"
  - Defines a handful of `liga` substitutions for icon names observed in fixtures
  - Draws simple geometric placeholder glyphs for those icons

It is intentionally not a full Material Icons implementation; it only aims to
make offline fixture renders deterministic and recognizably "icon-like".
"""

from __future__ import annotations

import pathlib

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


REPO_ROOT = pathlib.Path(__file__).resolve().parents[3]
OUTPUT = (
  REPO_ROOT
  / "tests/pages/fixtures/britannica.com/assets/FastRenderMaterialIcons.ttf"
)


def rect(pen: TTGlyphPen, x0: int, y0: int, x1: int, y1: int) -> None:
  pen.moveTo((x0, y0))
  pen.lineTo((x1, y0))
  pen.lineTo((x1, y1))
  pen.lineTo((x0, y1))
  pen.closePath()


def empty_glyph():
  pen = TTGlyphPen(None)
  return pen.glyph()


def notdef_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 100, 0, 900, 800)
  rect(pen, 200, 100, 800, 700)
  return pen.glyph()


def menu_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 180, 680, 820, 760)
  rect(pen, 180, 460, 820, 540)
  rect(pen, 180, 240, 820, 320)
  return pen.glyph()


def toc_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 180, 680, 820, 760)
  rect(pen, 180, 460, 820, 540)
  rect(pen, 180, 240, 820, 320)
  rect(pen, 120, 690, 160, 750)
  rect(pen, 120, 470, 160, 530)
  rect(pen, 120, 250, 160, 310)
  return pen.glyph()


def search_glyph():
  pen = TTGlyphPen(None)
  # Lens (boxy circle).
  rect(pen, 200, 420, 600, 820)
  rect(pen, 260, 480, 540, 760)
  # Handle.
  rect(pen, 580, 260, 820, 500)
  return pen.glyph()


def close_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 220, 640, 780, 720)
  rect(pen, 220, 280, 780, 360)
  rect(pen, 220, 460, 780, 540)
  return pen.glyph()


def play_arrow_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 320, 220, 420, 780)
  rect(pen, 420, 320, 720, 680)
  return pen.glyph()


def play_circle_outline_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 140, 140, 860, 860)
  rect(pen, 220, 220, 780, 780)
  # Inner play triangle (approx).
  rect(pen, 420, 360, 520, 640)
  rect(pen, 520, 420, 680, 580)
  return pen.glyph()


def arrow_up_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 300, 560, 700, 660)
  rect(pen, 420, 340, 580, 560)
  rect(pen, 420, 660, 580, 860)
  return pen.glyph()


def arrow_down_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 300, 340, 700, 440)
  rect(pen, 420, 440, 580, 660)
  rect(pen, 420, 140, 580, 340)
  return pen.glyph()


def arrow_left_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 340, 420, 440, 580)
  rect(pen, 440, 480, 660, 520)
  rect(pen, 140, 420, 340, 580)
  return pen.glyph()


def arrow_right_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 560, 420, 660, 580)
  rect(pen, 340, 480, 560, 520)
  rect(pen, 660, 420, 860, 580)
  return pen.glyph()


def expand_less_glyph():
  # Reuse arrow-up-like shape.
  return arrow_up_glyph()


def brightness_low_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 380, 380, 620, 620)
  rect(pen, 460, 120, 540, 300)
  rect(pen, 460, 700, 540, 880)
  rect(pen, 120, 460, 300, 540)
  rect(pen, 700, 460, 880, 540)
  return pen.glyph()


def account_circle_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 140, 140, 860, 860)
  rect(pen, 220, 220, 780, 780)
  rect(pen, 420, 520, 580, 680)  # head
  rect(pen, 340, 320, 660, 460)  # shoulders
  return pen.glyph()


def auto_awesome_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 460, 120, 540, 880)
  rect(pen, 120, 460, 880, 540)
  rect(pen, 300, 300, 700, 700)
  return pen.glyph()


def image_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 140, 220, 860, 780)
  rect(pen, 220, 300, 780, 700)
  rect(pen, 300, 360, 500, 560)
  rect(pen, 520, 360, 700, 500)
  return pen.glyph()


ICON_LIGATURES = {
  # HTML `data-icon=` values.
  "account_circle": "icon_account_circle",
  "auto_awesome": "icon_auto_awesome",
  "close": "icon_close",
  "image": "icon_image",
  "keyboard_arrow_down": "icon_keyboard_arrow_down",
  "keyboard_arrow_left": "icon_keyboard_arrow_left",
  "keyboard_arrow_right": "icon_keyboard_arrow_right",
  "keyboard_arrow_up": "icon_keyboard_arrow_up",
  "menu": "icon_menu",
  "play_arrow": "icon_play_arrow",
  "play_circle_outline": "icon_play_circle_outline",
  "search": "icon_search",
  # CSS `content:"..."` values.
  "brightness_low": "icon_brightness_low",
  "expand_less": "icon_expand_less",
  "toc": "icon_toc",
}


def main() -> None:
  upem = 1000
  ascent = 800
  descent = -200

  glyph_order: list[str] = [".notdef", "space", "underscore"]
  glyph_order.extend([chr(cp) for cp in range(ord("a"), ord("z") + 1)])
  glyph_order.extend(sorted(set(ICON_LIGATURES.values())))

  fb = FontBuilder(upem, isTTF=True)
  fb.setupGlyphOrder(glyph_order)

  cmap = {0x0020: "space", 0x005F: "underscore"}
  for cp in range(ord("a"), ord("z") + 1):
    cmap[cp] = chr(cp)
  fb.setupCharacterMap(cmap)

  glyphs: dict[str, object] = {name: empty_glyph() for name in glyph_order}
  glyphs[".notdef"] = notdef_glyph()
  glyphs["space"] = empty_glyph()

  glyphs["icon_menu"] = menu_glyph()
  glyphs["icon_toc"] = toc_glyph()
  glyphs["icon_search"] = search_glyph()
  glyphs["icon_close"] = close_glyph()
  glyphs["icon_play_arrow"] = play_arrow_glyph()
  glyphs["icon_play_circle_outline"] = play_circle_outline_glyph()
  glyphs["icon_keyboard_arrow_up"] = arrow_up_glyph()
  glyphs["icon_keyboard_arrow_down"] = arrow_down_glyph()
  glyphs["icon_keyboard_arrow_left"] = arrow_left_glyph()
  glyphs["icon_keyboard_arrow_right"] = arrow_right_glyph()
  glyphs["icon_expand_less"] = expand_less_glyph()
  glyphs["icon_brightness_low"] = brightness_low_glyph()
  glyphs["icon_account_circle"] = account_circle_glyph()
  glyphs["icon_auto_awesome"] = auto_awesome_glyph()
  glyphs["icon_image"] = image_glyph()

  fb.setupGlyf(glyphs)

  metrics = {name: (0, 0) for name in glyph_order}
  metrics["space"] = (500, 0)
  for glyph in ICON_LIGATURES.values():
    metrics[glyph] = (upem, 0)
  fb.setupHorizontalMetrics(metrics)
  fb.setupHorizontalHeader(ascent=ascent, descent=descent)
  fb.setupOS2(
    sTypoAscender=ascent,
    sTypoDescender=descent,
    usWinAscent=ascent,
    usWinDescent=-descent,
  )
  fb.setupNameTable(
    {
      "familyName": "Material Icons",
      "styleName": "Regular",
      "fullName": "Material Icons Regular",
      "uniqueFontIdentifier": "Material Icons Regular",
      "psName": "MaterialIcons-Regular",
      "version": "Version 1.0",
      "licenseDescription": "Public Domain / CC0",
      "licenseInfoURL": "https://creativecommons.org/publicdomain/zero/1.0/",
    }
  )
  fb.setupPost()
  fb.setupMaxp()

  # Ligature substitutions for icon names.
  feature_lines = ["languagesystem DFLT dflt;", "", "feature liga {"]
  for name, glyph in ICON_LIGATURES.items():
    parts = []
    for ch in name:
      parts.append("underscore" if ch == "_" else ch)
    feature_lines.append(f"  sub {' '.join(parts)} by {glyph};")
  feature_lines.append("} liga;")
  feature = "\n".join(feature_lines) + "\n"
  addOpenTypeFeaturesFromString(fb.font, feature)

  # Deterministic timestamps (seconds since 1904-01-01).
  fb.font["head"].created = 0
  fb.font["head"].modified = 0

  OUTPUT.parent.mkdir(parents=True, exist_ok=True)
  fb.save(OUTPUT)


if __name__ == "__main__":
  main()
