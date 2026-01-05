#!/usr/bin/env python3
"""
Generate `FastRenderEmoji.ttf` deterministically.

This is a tiny COLRv0 emoji font (CC0) used for hermetic bundled-font runs.
It intentionally covers only the emoji sequences we observe in pageset runs,
keeping the binary small while avoiding slow system-font fallback paths.
"""

from __future__ import annotations

import pathlib

try:
  from fontTools.colorLib.builder import buildCOLR, buildCPAL
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


FIXTURES = pathlib.Path(__file__).parent
OUTPUT = FIXTURES / "FastRenderEmoji.ttf"


def rect(pen: TTGlyphPen, x0: int, y0: int, x1: int, y1: int) -> None:
  pen.moveTo((x0, y0))
  pen.lineTo((x1, y0))
  pen.lineTo((x1, y1))
  pen.lineTo((x0, y1))
  pen.closePath()


def rect_glyph(x0: int, y0: int, x1: int, y1: int):
  pen = TTGlyphPen(None)
  rect(pen, x0, y0, x1, y1)
  return pen.glyph()


def grin_features_glyph():
  pen = TTGlyphPen(None)
  # Eyes.
  rect(pen, 300, 520, 380, 600)
  rect(pen, 620, 520, 700, 600)
  # Mouth.
  rect(pen, 360, 300, 640, 360)
  return pen.glyph()


def thumb_outline_glyph():
  pen = TTGlyphPen(None)
  # Simple border/crease.
  rect(pen, 250, 120, 760, 720)
  rect(pen, 300, 480, 720, 540)
  return pen.glyph()


def regional_indicator_u_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 220, 120, 780, 720)  # background box
  # U shape cut-out is omitted; keep it simple but distinct.
  rect(pen, 320, 220, 420, 620)
  rect(pen, 580, 220, 680, 620)
  rect(pen, 420, 220, 580, 320)
  return pen.glyph()


def regional_indicator_s_glyph():
  pen = TTGlyphPen(None)
  rect(pen, 220, 120, 780, 720)  # background box
  # A blocky S.
  rect(pen, 320, 540, 680, 620)
  rect(pen, 320, 420, 420, 540)
  rect(pen, 320, 320, 680, 400)
  rect(pen, 580, 200, 680, 320)
  rect(pen, 320, 120, 680, 200)
  return pen.glyph()


def regional_indicator_generic_mark_glyph():
  pen = TTGlyphPen(None)
  # A simple plus sign to indicate "some flag/regional indicator" without trying to
  # perfectly render every flag sequence.
  rect(pen, 460, 260, 540, 580)
  rect(pen, 340, 380, 660, 460)
  return pen.glyph()


def flag_stripes_glyph():
  pen = TTGlyphPen(None)
  # Three red stripes (leave implicit white stripes via the background layer).
  rect(pen, 150, 630, 850, 720)
  rect(pen, 150, 420, 850, 510)
  rect(pen, 150, 210, 850, 300)
  return pen.glyph()


def keycap_base_glyph():
  # A simple "1"-like vertical bar used for all keycap base characters.
  pen = TTGlyphPen(None)
  rect(pen, 460, 220, 540, 620)
  return pen.glyph()


def england_cross_glyph():
  # A simple red cross for the England subdivision tag sequence flag.
  pen = TTGlyphPen(None)
  rect(pen, 470, 180, 530, 720)  # vertical bar
  rect(pen, 150, 420, 850, 480)  # horizontal bar
  return pen.glyph()


def main() -> None:
  upem = 1000
  ascent = 900
  descent = -200

  glyph_order = [
    ".notdef",
    "space",
    "grin",
    "grin.layer1",
    "grin.layer2",
    "heart",
    "heart.layer1",
    "thumb",
    "thumb.layer1",
    "thumb.layer2",
    "ri_generic",
    "ri_generic.layer1",
    "ri_generic.layer2",
    "ri_u",
    "ri_s",
    "flag_us",
    "flag_us.layer1",
    "flag_us.layer2",
    "flag_us.layer3",
    # Minimal ZWJ sequence components/ligature.
    "zwj",
    "man",
    "woman",
    "girl",
    "boy",
    "family",
    "family.layer1",
    "family.layer2",
    # Keycap sequences + tag sequences + additional ZWJ sequences.
    "keycap_base",
    "keycap_mark",
    "keycap",
    "black_flag",
    "tag",
    "tag_cancel",
    "flag_england",
    "flag_england.layer2",
    "emoji_modifier",
    "microscope",
    "medical",
    "woman_scientist",
    "woman_health_worker",
  ]

  fb = FontBuilder(upem, isTTF=True)
  fb.setupGlyphOrder(glyph_order)
  cmap = {
    0x0020: "space",
    # Keycap base characters.
    0x0023: "keycap_base",  # #
    0x002A: "keycap_base",  # *
    0x20E3: "keycap_mark",  # combining enclosing keycap
    0x200D: "zwj",
    0x1F600: "grin",
    0x2764: "heart",
    0x1F44D: "thumb",
    # Tag sequence components (England subdivision flag).
    0x1F3F4: "black_flag",
    0xE0067: "tag",  # tag g
    0xE0062: "tag",  # tag b
    0xE0065: "tag",  # tag e
    0xE006E: "tag",  # tag n
    # Extra tag codepoints used by other subdivision flags (e.g. Scotland = "gbsct", Wales = "gbwls").
    0xE0073: "tag",  # tag s
    0xE0063: "tag",  # tag c
    0xE0074: "tag",  # tag t
    0xE0077: "tag",  # tag w
    0xE006C: "tag",  # tag l
    0xE007F: "tag_cancel",
    # ZWJ sequences we want to keep on the emoji fallback path.
    0x1F52C: "microscope",  # 🔬
    0x2695: "medical",  # ⚕ (emoji variant ⚕️)
    # Regional indicator letters needed for pageset flags.
    0x1F1EB: "ri_generic",  # 🇫
    0x1F1EE: "ri_generic",  # 🇮
    0x1F1FA: "ri_u",
    0x1F1F8: "ri_s",
    # ZWJ sequence (family) codepoints.
    0x1F468: "man",  # 👨
    0x1F469: "woman",  # 👩
    0x1F467: "girl",  # 👧
    0x1F466: "boy",  # 👦
  }
  for codepoint in range(0x30, 0x3A):
    cmap[codepoint] = "keycap_base"
  # Pageset-derived emoji (from `bundled_font_coverage`) mapped onto the existing fixture glyphs
  # so bundled-font runs avoid missing-emoji tofu.
  for codepoint in [
    0x2602,  # ☂ (emoji variant ☂️)
    0x2636,  # ☶
    0x26A0,  # ⚠ (emoji variant ⚠️)
    0x25B6,  # ▶ (emoji variant ▶️)
    0x2705,  # ✅
    0x2714,  # ✔
    0x270D,  # ✍
    0x2726,  # ✦
    0x2728,  # ✨
    0x276E,  # ❮
    0x276F,  # ❯
    0x2756,  # ❖
    0x2B06,  # ⬆
    0x2B07,  # ⬇
    0x2B50,  # ⭐
    0x1F31F,  # 🌟
    0x1F30E,  # 🌎
    0x1F381,  # 🎁
    0x1F382,  # 🎂
    0x1F386,  # 🎆
    0x1F389,  # 🎉
    0x1F38A,  # 🎊
    0x1F3C6,  # 🏆
    0x1F3C8,  # 🏈
    0x1F3DF,  # 🏟
    0x1F3E0,  # 🏠
    0x1F410,  # 🐐
    0x1F414,  # 🐔
    0x1F41F,  # 🐟
    0x1F42E,  # 🐮
    0x1F437,  # 🐷
    0x1F440,  # 👀
    0x1F447,  # 👇
    0x1F44B,  # 👋
    0x1F45C,  # 👜
    0x1F4A5,  # 💥
    0x1F4C5,  # 📅
    0x1F4CC,  # 📌
    0x1F4CD,  # 📍
    0x1F58C,  # 🖌
    0x1F602,  # 😂
    0x1F621,  # 😡
    0x1F62C,  # 😬
    0x1F62D,  # 😭
    0x1F644,  # 🙄
    0x1F680,  # 🚀
    0x1F6A8,  # 🚨
    0x1F914,  # 🤔
    0x1F919,  # 🤙
    0x1F920,  # 🤠
    0x1F929,  # 🤩
    0x1F92F,  # 🤯
    0x1F52E,  # 🔮
    0x1F9C3,  # 🧃
    0x1F9E3,  # 🧣
  ]:
    cmap[codepoint] = "grin"
  # Pageset-derived icon/codepoint regressions (typically inserted via CSS `content:`) mapped onto
  # existing glyphs so bundled-font runs avoid last-resort tofu.
  for codepoint in [
    0x28FE,  # ⣾ (stripe.com)
    0xE021,  # private use (hbr.org)
    0xE022,  # private use (hbr.org)
    0xE031,  # private use (hbr.org)
    0xE083,  # private use (hbr.org)
    0xE085,  # private use (hbr.org)
    0xE909,  # private use (microsoft.com)
    0xF301,  # private use (developer.apple.com)
    0xF8FF,  # private use (developer.apple.com)
  ]:
    cmap[codepoint] = "grin"
  for codepoint in [
    0x1F4AA,  # 💪
    0x1F937,  # 🤷
  ]:
    cmap[codepoint] = "thumb"
  cmap[0x1F497] = "heart"  # 💗
  cmap[0x1F525] = "heart"  # 🔥
  for codepoint in range(0x1F3FB, 0x1F400):
    cmap[codepoint] = "emoji_modifier"
  fb.setupCharacterMap(cmap)

  glyphs = {
    ".notdef": rect_glyph(100, 0, 900, 800),
    "space": rect_glyph(0, 0, 0, 0),
    "zwj": rect_glyph(0, 0, 0, 0),
    # 😀
    "grin": rect_glyph(0, 0, 0, 0),
    "grin.layer1": rect_glyph(150, 150, 850, 850),
    "grin.layer2": grin_features_glyph(),
    # ❤
    "heart": rect_glyph(0, 0, 0, 0),
    "heart.layer1": rect_glyph(300, 250, 700, 650),
    # 👍
    "thumb": rect_glyph(0, 0, 0, 0),
    "thumb.layer1": rect_glyph(260, 140, 740, 720),
    "thumb.layer2": thumb_outline_glyph(),
    # Generic tile used for pageset flags outside 🇺🇸 (e.g. 🇫🇮).
    "ri_generic": rect_glyph(0, 0, 0, 0),
    "ri_generic.layer1": rect_glyph(220, 120, 780, 720),
    "ri_generic.layer2": regional_indicator_generic_mark_glyph(),
    # Regional indicators used for 🇺🇸.
    "ri_u": regional_indicator_u_glyph(),
    "ri_s": regional_indicator_s_glyph(),
    # Flag glyph reached via GSUB ligature.
    "flag_us": rect_glyph(0, 0, 0, 0),
    "flag_us.layer1": rect_glyph(150, 180, 850, 720),  # white background
    "flag_us.layer2": flag_stripes_glyph(),
    "flag_us.layer3": rect_glyph(150, 480, 500, 720),  # blue canton
    # ZWJ family sequence.
    "man": rect_glyph(0, 0, 0, 0),
    "woman": rect_glyph(0, 0, 0, 0),
    "girl": rect_glyph(0, 0, 0, 0),
    "boy": rect_glyph(0, 0, 0, 0),
    "family": rect_glyph(0, 0, 0, 0),
    "family.layer1": rect_glyph(150, 150, 850, 850),
    "family.layer2": regional_indicator_generic_mark_glyph(),
    # Keycap sequences.
    "keycap_base": keycap_base_glyph(),
    "keycap_mark": rect_glyph(0, 0, 0, 0),
    "keycap": rect_glyph(0, 0, 0, 0),
    # Tag sequence (England subdivision flag).
    "black_flag": rect_glyph(0, 0, 0, 0),
    "tag": rect_glyph(0, 0, 0, 0),
    "tag_cancel": rect_glyph(0, 0, 0, 0),
    "flag_england": rect_glyph(0, 0, 0, 0),
    "flag_england.layer2": england_cross_glyph(),
    # ZWJ sequences with emoji modifiers.
    "emoji_modifier": rect_glyph(0, 0, 0, 0),
    "microscope": rect_glyph(0, 0, 0, 0),
    "medical": rect_glyph(0, 0, 0, 0),
    "woman_scientist": rect_glyph(0, 0, 0, 0),
    "woman_health_worker": rect_glyph(0, 0, 0, 0),
  }

  advance = 1000
  metrics = {name: (advance, 0) for name in glyph_order}
  metrics["space"] = (500, 0)
  metrics["zwj"] = (0, 0)
  metrics["keycap_mark"] = (0, 0)
  metrics["emoji_modifier"] = (0, 0)
  metrics["tag"] = (0, 0)
  metrics["tag_cancel"] = (0, 0)

  fb.setupGlyf(glyphs)
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
      "familyName": "FastRender Emoji",
      "styleName": "Regular",
      "fullName": "FastRender Emoji Regular",
      "uniqueFontIdentifier": "FastRender Emoji Regular",
      "psName": "FastRenderEmoji-Regular",
      "version": "Version 1.0",
      "licenseDescription": "Public Domain / CC0",
      "licenseInfoURL": "https://creativecommons.org/publicdomain/zero/1.0/",
    }
  )
  fb.setupPost()
  fb.setupMaxp()

  # Palette indices:
  # 0 yellow, 1 black, 2 red, 3 skin, 4 white, 5 blue.
  palette = [
    (0.98, 0.86, 0.12, 1.0),
    (0.05, 0.05, 0.05, 1.0),
    (0.80, 0.12, 0.18, 1.0),
    (0.95, 0.78, 0.60, 1.0),
    (0.98, 0.98, 0.98, 1.0),
    (0.12, 0.25, 0.75, 1.0),
  ]
  fb.font["CPAL"] = buildCPAL([palette])
  fb.font["COLR"] = buildCOLR(
    {
      "grin": [("grin.layer1", 0), ("grin.layer2", 1)],
      "heart": [("heart.layer1", 2)],
      "thumb": [("thumb.layer1", 3), ("thumb.layer2", 1)],
      "man": [("grin.layer1", 0), ("grin.layer2", 1)],
      "woman": [("grin.layer1", 0), ("grin.layer2", 1)],
      "girl": [("grin.layer1", 0), ("grin.layer2", 1)],
      "boy": [("grin.layer1", 0), ("grin.layer2", 1)],
      "family": [("family.layer1", 3), ("family.layer2", 1)],
      "ri_generic": [("ri_generic.layer1", 4), ("ri_generic.layer2", 5)],
      "keycap": [("grin.layer1", 0), ("keycap_base", 1)],
      "black_flag": [("flag_us.layer1", 1)],
      "flag_us": [
        ("flag_us.layer1", 4),
        ("flag_us.layer2", 2),
        ("flag_us.layer3", 5),
      ],
      "flag_england": [("flag_us.layer1", 4), ("flag_england.layer2", 2)],
      "woman_scientist": [("grin.layer1", 0), ("family.layer2", 1)],
      "woman_health_worker": [("thumb.layer1", 3), ("family.layer2", 2)],
    },
    version=0,
    glyphMap=fb.font.getReverseGlyphMap(),
  )

  # Minimal ligature for 🇺🇸 so HarfBuzz can shape it as a single glyph.
  addOpenTypeFeaturesFromString(
    fb.font,
    """
languagesystem DFLT dflt;

feature ccmp {
  sub ri_u ri_s by flag_us;
  sub man zwj woman zwj girl zwj boy by family;
  sub keycap_base keycap_mark by keycap;
  sub black_flag tag tag tag tag tag tag_cancel by flag_england;
  sub woman zwj microscope by woman_scientist;
  sub woman emoji_modifier zwj microscope by woman_scientist;
  sub woman zwj medical by woman_health_worker;
  sub woman emoji_modifier zwj medical by woman_health_worker;
} ccmp;
""",
  )

  # Deterministic timestamps (seconds since 1904-01-01).
  fb.font["head"].created = 0
  fb.font["head"].modified = 0

  fb.save(OUTPUT)


if __name__ == "__main__":
  main()
