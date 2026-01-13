#!/usr/bin/env python3
"""Generate a COLRv1 font that exercises variable transform paints.

The resulting font is released into the public domain (CC0) and checked in under
`tests/fixtures/fonts/colrv1-var-transform-test.ttf`.
"""

from __future__ import annotations

from pathlib import Path
from typing import Dict, List, Tuple

from fontTools.colorLib.builder import buildCOLR, buildCPAL
from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib.tables.otTables import PaintFormat
from fontTools.varLib import builder as varBuilder

UNITS_PER_EM = 1000
ASCENT = 850
DESCENT = -200


def rect_glyph(x0: int, y0: int, x1: int, y1: int):
    pen = TTGlyphPen(None)
    pen.moveTo((x0, y0))
    pen.lineTo((x1, y0))
    pen.lineTo((x1, y1))
    pen.lineTo((x0, y1))
    pen.closePath()
    return pen.glyph()


def setup_basic_tables(fb: FontBuilder, glyphs: Dict[str, object]) -> None:
    metrics = {name: (UNITS_PER_EM, 0) for name in glyphs}
    fb.setupGlyf(glyphs)
    fb.setupHorizontalMetrics(metrics)
    fb.setupHorizontalHeader(ascent=ASCENT, descent=DESCENT)
    fb.setupOS2(
        sTypoAscender=ASCENT,
        sTypoDescender=DESCENT,
        usWinAscent=ASCENT,
        usWinDescent=abs(DESCENT),
        usWeightClass=400,
    )
    fb.setupPost()
    fb.setupMaxp()
    fb.setupHead()
    fb.font["head"].created = 0
    fb.font["head"].modified = 0


def f2dot14(value: float) -> int:
    return int(round(value * (1 << 14)))


def fixed_16_16(value: float) -> int:
    return int(round(value * (1 << 16)))


def paint_glyph_solid(glyph: str, palette_index: int, alpha: float = 1.0) -> dict:
    return {
        "Format": PaintFormat.PaintGlyph,
        "Glyph": glyph,
        "Paint": {
            "Format": PaintFormat.PaintSolid,
            "PaletteIndex": palette_index,
            "Alpha": alpha,
        },
    }


def paint_translate(paint: dict, dx: float, dy: float) -> dict:
    return {
        "Format": PaintFormat.PaintTranslate,
        "Paint": paint,
        "dx": dx,
        "dy": dy,
    }


def build_variable_transform_font(path: Path) -> None:
    fb = FontBuilder(UNITS_PER_EM, isTTF=True)
    glyph_order = [".notdef", "color", "shape", "bounds"]
    fb.setupGlyphOrder(glyph_order)
    fb.setupCharacterMap({ord("A"): "color"})

    # `shape` is centered on (0, 0) so rotate/scale/skew formats are easy to reason about.
    glyphs = {
        ".notdef": rect_glyph(120, 0, 880, 780),
        "color": rect_glyph(0, 0, 1000, 800),
        "shape": rect_glyph(-110, -80, 110, 80),
        # A transparent bounds layer ensures the raster bounds remain stable even when variable
        # transforms move other layers around.
        "bounds": rect_glyph(-200, 0, 1200, 700),
    }
    setup_basic_tables(fb, glyphs)
    fb.setupNameTable(
        {
            "familyName": "COLRv1 Var Transform Test",
            "styleName": "Regular",
            "fullName": "COLRv1 Var Transform Test",
            "uniqueFontIdentifier": "COLRv1 Var Transform Test",
            "psName": "COLRv1VarTransformTest-Regular",
            "version": "Version 1.0",
            "licenseDescription": "Public Domain / CC0",
            "licenseInfoURL": "https://creativecommons.org/publicdomain/zero/1.0/",
        }
    )
    fb.setupDummyDSIG()
    fb.setupFvar(
        axes=[("wght", 0.0, 0.0, 1.0, "Weight")],
        instances=[
            {"stylename": "Regular", "location": {"wght": 0.0}},
            {"stylename": "Bold", "location": {"wght": 1.0}},
        ],
    )

    palette = [
        (0.0, 0.0, 0.0, 1.0),  # bounds (alpha is set to 0 in the paint)
        (0.9, 0.2, 0.2, 1.0),
        (0.2, 0.75, 0.3, 1.0),
        (0.15, 0.4, 0.9, 1.0),
        (0.95, 0.85, 0.3, 1.0),
        (0.65, 0.3, 0.85, 1.0),
        (0.15, 0.75, 0.8, 1.0),
        (0.85, 0.55, 0.2, 1.0),
        (0.35, 0.35, 0.35, 1.0),
        (0.1, 0.55, 0.95, 1.0),
        (0.95, 0.35, 0.65, 1.0),
    ]
    fb.font["CPAL"] = buildCPAL([palette], paletteTypes=[0])

    axis_tags = ["wght"]
    supports = [{"wght": (0.0, 1.0, 1.0)}]
    var_region_list = varBuilder.buildVarRegionList(supports, axis_tags)

    # Deltas are stored in the same units as their corresponding table fields:
    # - Translate/center fields: design-space FWORD (integer design units).
    # - Scale/angle/skew fields: F2Dot14 (signed 2.14 fixed point). Angles are half-turn units.
    # - VarTransform matrix: Fixed 16.16.
    deltas: List[List[int]] = []

    var_translate_base = len(deltas)
    deltas += [
        [120],  # dx
        [-70],  # dy
    ]

    var_scale_base = len(deltas)
    deltas += [
        [f2dot14(0.25)],  # scaleX
        [f2dot14(-0.12)],  # scaleY
    ]

    var_scale_center_base = len(deltas)
    deltas += [
        [f2dot14(0.18)],  # scaleX
        [f2dot14(0.06)],  # scaleY
        [45],  # centerX
        [-30],  # centerY
    ]

    var_scale_uniform_base = len(deltas)
    deltas += [
        [f2dot14(0.22)],  # scale
    ]

    var_scale_uniform_center_base = len(deltas)
    deltas += [
        [f2dot14(-0.14)],  # scale
        [-55],  # centerX
        [35],  # centerY
    ]

    var_rotate_base = len(deltas)
    deltas += [
        [f2dot14(0.10)],  # angle (+18°) in half-turn units
    ]

    var_rotate_center_base = len(deltas)
    deltas += [
        [f2dot14(-0.14)],  # angle (-25.2°)
        [70],  # centerX
        [-40],  # centerY
    ]

    var_skew_base = len(deltas)
    deltas += [
        [f2dot14(0.07)],  # xSkewAngle (+12.6°)
        [f2dot14(-0.05)],  # ySkewAngle (-9°)
    ]

    var_skew_center_base = len(deltas)
    deltas += [
        [f2dot14(-0.08)],  # xSkewAngle (-14.4°)
        [f2dot14(0.04)],  # ySkewAngle (+7.2°)
        [-35],  # centerX
        [55],  # centerY
    ]

    var_transform_base = len(deltas)
    deltas += [
        [fixed_16_16(0.2)],  # xx
        [fixed_16_16(0.12)],  # yx
        [fixed_16_16(-0.08)],  # xy
        [fixed_16_16(0.15)],  # yy
        [fixed_16_16(75.0)],  # dx
        [fixed_16_16(-55.0)],  # dy
    ]

    var_data = [varBuilder.buildVarData([0], deltas, optimize=False)]
    var_store = varBuilder.buildVarStore(var_region_list, var_data)
    var_index_map = varBuilder.buildDeltaSetIndexMap(range(len(deltas)))

    positions: List[Tuple[float, float]] = [
        (150.0, 520.0),
        (380.0, 520.0),
        (610.0, 520.0),
        (840.0, 520.0),
        (1070.0, 520.0),
        (150.0, 200.0),
        (380.0, 200.0),
        (610.0, 200.0),
        (840.0, 200.0),
        (1070.0, 200.0),
    ]

    bounds_layer = paint_glyph_solid("bounds", 0, alpha=0.0)

    layers: List[dict] = [bounds_layer]

    # 1) PaintVarTranslate (FWORD deltas)
    paint = {
        "Format": PaintFormat.PaintVarTranslate,
        "Paint": paint_glyph_solid("shape", 1, alpha=0.92),
        "dx": 0.0,
        "dy": 0.0,
        "VarIndexBase": var_translate_base,
    }
    layers.append(paint_translate(paint, *positions[0]))

    # 2) PaintVarScale (F2Dot14 deltas)
    paint = {
        "Format": PaintFormat.PaintVarScale,
        "Paint": paint_glyph_solid("shape", 2, alpha=0.92),
        "scaleX": 1.0,
        "scaleY": 1.0,
        "VarIndexBase": var_scale_base,
    }
    layers.append(paint_translate(paint, *positions[1]))

    # 3) PaintVarScaleAroundCenter (mix of F2Dot14 + FWORD deltas)
    paint = {
        "Format": PaintFormat.PaintVarScaleAroundCenter,
        "Paint": paint_glyph_solid("shape", 3, alpha=0.92),
        "scaleX": 1.0,
        "scaleY": 1.0,
        "centerX": 0.0,
        "centerY": 0.0,
        "VarIndexBase": var_scale_center_base,
    }
    layers.append(paint_translate(paint, *positions[2]))

    # 4) PaintVarScaleUniform
    paint = {
        "Format": PaintFormat.PaintVarScaleUniform,
        "Paint": paint_glyph_solid("shape", 4, alpha=0.92),
        "scale": 1.0,
        "VarIndexBase": var_scale_uniform_base,
    }
    layers.append(paint_translate(paint, *positions[3]))

    # 5) PaintVarScaleUniformAroundCenter
    paint = {
        "Format": PaintFormat.PaintVarScaleUniformAroundCenter,
        "Paint": paint_glyph_solid("shape", 5, alpha=0.92),
        "scale": 1.0,
        "centerX": 0.0,
        "centerY": 0.0,
        "VarIndexBase": var_scale_uniform_center_base,
    }
    layers.append(paint_translate(paint, *positions[4]))

    # 6) PaintVarRotate (angle delta)
    paint = {
        "Format": PaintFormat.PaintVarRotate,
        "Paint": paint_glyph_solid("shape", 6, alpha=0.92),
        "angle": 0.0,
        "VarIndexBase": var_rotate_base,
    }
    layers.append(paint_translate(paint, *positions[5]))

    # 7) PaintVarRotateAroundCenter (angle + center deltas)
    paint = {
        "Format": PaintFormat.PaintVarRotateAroundCenter,
        "Paint": paint_glyph_solid("shape", 7, alpha=0.92),
        "angle": 0.0,
        "centerX": 0.0,
        "centerY": 0.0,
        "VarIndexBase": var_rotate_center_base,
    }
    layers.append(paint_translate(paint, *positions[6]))

    # 8) PaintVarSkew (x/y angle deltas)
    paint = {
        "Format": PaintFormat.PaintVarSkew,
        "Paint": paint_glyph_solid("shape", 8, alpha=0.92),
        "xSkewAngle": 0.0,
        "ySkewAngle": 0.0,
        "VarIndexBase": var_skew_base,
    }
    layers.append(paint_translate(paint, *positions[7]))

    # 9) PaintVarSkewAroundCenter
    paint = {
        "Format": PaintFormat.PaintVarSkewAroundCenter,
        "Paint": paint_glyph_solid("shape", 9, alpha=0.92),
        "xSkewAngle": 0.0,
        "ySkewAngle": 0.0,
        "centerX": 0.0,
        "centerY": 0.0,
        "VarIndexBase": var_skew_center_base,
    }
    layers.append(paint_translate(paint, *positions[8]))

    # 10) PaintVarTransform (Fixed 16.16 deltas)
    paint = {
        "Format": PaintFormat.PaintVarTransform,
        "Paint": paint_glyph_solid("shape", 10, alpha=0.92),
        "Transform": {
            "xx": 1.0,
            "yx": 0.0,
            "xy": 0.0,
            "yy": 1.0,
            "dx": 0.0,
            "dy": 0.0,
            "VarIndexBase": var_transform_base,
        },
    }
    layers.append(paint_translate(paint, *positions[9]))

    fb.font["COLR"] = buildCOLR(
        {"color": {"Format": PaintFormat.PaintColrLayers, "Layers": layers}},
        version=1,
        glyphMap=fb.font.getReverseGlyphMap(),
        varStore=var_store,
        varIndexMap=var_index_map,
    )

    path.parent.mkdir(parents=True, exist_ok=True)
    fb.save(path)


def main() -> None:
    out_path = Path(__file__).with_name("colrv1-var-transform-test.ttf")
    build_variable_transform_font(out_path)
    print(f"Wrote {out_path}")


if __name__ == "__main__":
    main()
