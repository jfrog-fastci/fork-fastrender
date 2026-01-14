//! Integration tests for the Canvas module
//!
//! These tests verify the Canvas wrapper for tiny-skia works correctly
//! for real-world rendering scenarios.

use crate::error::{Error, RenderError, RenderStage};
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::image_compare::{compare_images, compare_png, decode_png, encode_png, CompareConfig};
use crate::paint::display_list::BorderRadius;
use crate::paint::display_list::FontVariation;
use crate::paint::display_list::GlyphInstance;
use crate::render_control::{with_deadline, CancelCallback, RenderDeadline};
use crate::style::types::FontPalette;
use crate::text::color_fonts::ColorFontRenderer;
use crate::text::font_db::FontDatabase;
use crate::text::font_instance::FontInstance;
use crate::BlendMode;
use crate::BorderRadii;
use crate::Canvas;
use crate::ComputedStyle;
use crate::FontContext;
use crate::Rgba;
use crate::ShapedRun;
use crate::ShapingPipeline;
use image::RgbaImage;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tiny_skia::Pixmap;

fn to_glyph_instances(run: &ShapedRun) -> Vec<GlyphInstance> {
  run
    .glyphs
    .iter()
    .map(|g| GlyphInstance {
      glyph_id: g.glyph_id,
      cluster: g.cluster,
      x_offset: g.x_offset,
      y_offset: -g.y_offset,
      x_advance: g.x_advance,
      y_advance: g.y_advance,
    })
    .collect()
}

fn pixmap_to_rgba_image(pixmap: &Pixmap) -> RgbaImage {
  let width = pixmap.width();
  let height = pixmap.height();
  let mut rgba = RgbaImage::new(width, height);

  for (dst, src) in rgba
    .as_mut()
    .chunks_exact_mut(4)
    .zip(pixmap.data().chunks_exact(4))
  {
    let b = src[0];
    let g = src[1];
    let r = src[2];
    let a = src[3];

    if a == 0 {
      dst.copy_from_slice(&[0, 0, 0, 0]);
      continue;
    }

    let alpha = a as f32 / 255.0;
    dst[0] = ((r as f32 / alpha).min(255.0)) as u8;
    dst[1] = ((g as f32 / alpha).min(255.0)) as u8;
    dst[2] = ((b as f32 / alpha).min(255.0)) as u8;
    dst[3] = a;
  }

  rgba
}

fn source_over_skia(dst: Rgba, src: Rgba) -> (u8, u8, u8, u8) {
  #[inline]
  fn mul_div_255_round_u16(a: u16, b: u16) -> u16 {
    let prod = (a as u32) * (b as u32);
    (((prod + 128) * 257) >> 16) as u16
  }

  let sa = (src.a * 255.0).round().clamp(0.0, 255.0) as u16;
  let inv_sa = 255u16 - sa;

  // Skia/Chrome premultiply source colors with *rounding*.
  let sr = mul_div_255_round_u16(src.r as u16, sa);
  let sg = mul_div_255_round_u16(src.g as u16, sa);
  let sb = mul_div_255_round_u16(src.b as u16, sa);

  let dr = dst.r as u16;
  let dg = dst.g as u16;
  let db = dst.b as u16;
  let da = dst.alpha_u8() as u16;

  let out_a = sa + (da * inv_sa) / 255u16;
  let out_r = sr + (dr * inv_sa) / 255u16;
  let out_g = sg + (dg * inv_sa) / 255u16;
  let out_b = sb + (db * inv_sa) / 255u16;

  // Clamp channels to the resulting alpha to preserve premultiplied invariants.
  let out_a_u8 = out_a.min(255) as u8;
  let clamp = out_a_u8 as u16;
  (
    out_r.min(clamp).min(255) as u8,
    out_g.min(clamp).min(255) as u8,
    out_b.min(clamp).min(255) as u8,
    out_a_u8,
  )
}

// ============================================================================
// Canvas Creation Tests
// ============================================================================

#[test]
fn test_canvas_creation_various_sizes() {
  // Small canvas
  let canvas = Canvas::new(1, 1, Rgba::WHITE);
  assert!(canvas.is_ok());

  // Medium canvas
  let canvas = Canvas::new(800, 600, Rgba::WHITE);
  assert!(canvas.is_ok());

  // Large canvas
  let canvas = Canvas::new(4096, 4096, Rgba::WHITE);
  assert!(canvas.is_ok());
}

#[test]
fn test_canvas_creation_with_colors() {
  // White background
  let canvas = Canvas::new(10, 10, Rgba::WHITE).unwrap();
  let data = canvas.pixmap().data();
  assert_eq!(data[0], 255);
  assert_eq!(data[1], 255);
  assert_eq!(data[2], 255);
  assert_eq!(data[3], 255);

  // Black background
  let canvas = Canvas::new(10, 10, Rgba::BLACK).unwrap();
  let data = canvas.pixmap().data();
  assert_eq!(data[0], 0);
  assert_eq!(data[1], 0);
  assert_eq!(data[2], 0);
  assert_eq!(data[3], 255);

  // Transparent background
  let canvas = Canvas::new(10, 10, Rgba::TRANSPARENT).unwrap();
  let data = canvas.pixmap().data();
  assert_eq!(data[0], 0);
  assert_eq!(data[1], 0);
  assert_eq!(data[2], 0);
  assert_eq!(data[3], 0);
}

// ============================================================================
// Rectangle Drawing Tests
// ============================================================================

#[test]
fn test_draw_rect_basic() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  // Draw a red rectangle
  let rect = Rect::from_xywh(10.0, 10.0, 30.0, 20.0);
  canvas.draw_rect(rect, Rgba::rgb(255, 0, 0));

  // Verify the canvas was modified (just checking it doesn't crash)
  let pixmap = canvas.into_pixmap();
  assert_eq!(pixmap.width(), 100);
  assert_eq!(pixmap.height(), 100);
}

#[test]
fn test_draw_rect_at_origin() {
  let mut canvas = Canvas::new(50, 50, Rgba::WHITE).unwrap();

  let rect = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  canvas.draw_rect(rect, Rgba::rgb(0, 255, 0));

  let data = canvas.pixmap().data();
  // First pixel should be green
  assert_eq!(data[0], 0); // R
  assert_eq!(data[1], 255); // G
  assert_eq!(data[2], 0); // B
  assert_eq!(data[3], 255); // A
}

#[test]
fn opaque_axis_aligned_rect_fills_are_pixel_snapped() {
  let mut canvas = Canvas::new(10, 60, Rgba::WHITE).unwrap();

  // This intentionally uses a fractional height similar to the Berkeley fixture header:
  // `padding: 0.3rem` on a 16px root font size yields box edges at 51.6px.
  //
  // Anti-aliased fills produce a 1px seam at the bottom edge when the box is immediately followed
  // by another background. Our canvas rect fill path snaps opaque, axis-aligned fills to device
  // pixels to avoid this.
  canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 10.0, 51.6), Rgba::rgb(0, 38, 118));
  // Simulate the adjacent content section background that starts at the same fractional boundary.
  canvas.draw_rect(Rect::from_xywh(0.0, 51.6, 10.0, 8.4), Rgba::WHITE);

  let p51 = canvas.pixmap().pixel(0, 51).unwrap();
  assert_eq!(
    (p51.red(), p51.green(), p51.blue(), p51.alpha()),
    (0, 38, 118, 255),
    "expected the last covered row to be fully filled (no blended seam)"
  );

  let p52 = canvas.pixmap().pixel(0, 52).unwrap();
  assert_eq!(
    (p52.red(), p52.green(), p52.blue(), p52.alpha()),
    (255, 255, 255, 255),
    "expected the fill not to extend past the snapped bounds"
  );
}

#[test]
fn opaque_axis_aligned_rect_fills_do_not_cover_partial_max_edge_scanline() {
  // Chrome/Skia's non-AA rect fill uses a pixel-center rule: a scanline is covered only if its
  // pixel centers are inside the rect.
  //
  // For a `0..10.4px` height rect, the y=9 scanline (center 9.5) is covered but the y=10 scanline
  // (center 10.5) is not.
  let mut canvas = Canvas::new(20, 20, Rgba::WHITE).unwrap();
  let fill = Rgba::rgb(7, 28, 46);
  canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 10.4, 10.4), fill);

  let p9 = canvas.pixmap().pixel(0, 9).unwrap();
  assert_eq!(
    (p9.red(), p9.green(), p9.blue(), p9.alpha()),
    (7, 28, 46, 255),
    "expected the last fully covered scanline to be filled"
  );

  let p10 = canvas.pixmap().pixel(0, 10).unwrap();
  assert_eq!(
    (p10.red(), p10.green(), p10.blue(), p10.alpha()),
    (255, 255, 255, 255),
    "expected the partially covered scanline to remain unfilled"
  );

  let px9 = canvas.pixmap().pixel(9, 0).unwrap();
  assert_eq!(
    (px9.red(), px9.green(), px9.blue(), px9.alpha()),
    (7, 28, 46, 255),
    "expected the last fully covered column to be filled"
  );

  let px10 = canvas.pixmap().pixel(10, 0).unwrap();
  assert_eq!(
    (px10.red(), px10.green(), px10.blue(), px10.alpha()),
    (255, 255, 255, 255),
    "expected the partially covered column to remain unfilled"
  );
}

#[test]
fn opaque_axis_aligned_rect_fills_do_not_overpaint_adjacent_fractional_strips() {
  let mut canvas = Canvas::new(10, 200, Rgba::WHITE).unwrap();

  // Regression test for python.org: the top bar ends at a fractional device pixel and has a 1px
  // bottom border. The next section background starts at the same fractional edge and must not
  // "bleed" upward and overwrite the border scanline.
  //
  // Coordinates taken from the python.org display list dump:
  // - border strip: y=119.944534 h=1
  // - next background: y=120.944534
  let border_color = Rgba::rgb(31, 59, 71); // #1f3b47
  let bg_color = Rgba::rgb(43, 91, 132); // #2b5b84

  canvas.draw_rect(
    Rect::from_xywh(0.0, 119.94453430175781, 10.0, 1.0),
    border_color,
  );
  canvas.draw_rect(
    Rect::from_xywh(0.0, 120.94453430175781, 10.0, 10.0),
    bg_color,
  );

  let p120 = canvas.pixmap().pixel(0, 120).unwrap();
  assert_eq!(
    (p120.red(), p120.green(), p120.blue(), p120.alpha()),
    (31, 59, 71, 255),
    "expected the border scanline to remain visible"
  );
}

#[test]
fn opaque_axis_aligned_rect_fills_snap_half_pixel_edges_like_chrome() {
  // Regression: many layouts yield `.5` device pixel boundaries (e.g. `0.75rem` padding at an 18px
  // root font size -> 13.5px). Chrome/Skia's opaque axis-aligned fills treat pixel centers on the
  // min edge as outside and pixel centers on the max edge as inside (`open min / closed max`).
  //
  // This matters for crisp 1px borders/background edges: a rect ending at `2.5px` should include
  // pixel `2` (center at `2.5`), while a rect starting at `2.5px` should start at pixel `3` (center
  // at `3.5`).
  let bg = Rgba::rgb(255, 102, 0);
  let fill = Rgba::rgb(0, 255, 0);
  let mut canvas = Canvas::new(6, 4, bg).unwrap();
  canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 2.5, 2.0), fill);
  canvas.draw_rect(Rect::from_xywh(2.5, 2.0, 2.5, 2.0), fill);

  let p2_top = canvas.pixmap().pixel(2, 1).unwrap();
  assert_eq!(
    (p2_top.red(), p2_top.green(), p2_top.blue(), p2_top.alpha()),
    (0, 255, 0, 255),
    "expected max edge at 2.5px to include pixel 2"
  );
  let p3_top = canvas.pixmap().pixel(3, 1).unwrap();
  assert_eq!(
    (p3_top.red(), p3_top.green(), p3_top.blue(), p3_top.alpha()),
    (255, 102, 0, 255),
    "expected max edge at 2.5px to exclude pixel 3"
  );

  let p2_bottom = canvas.pixmap().pixel(2, 3).unwrap();
  assert_eq!(
    (
      p2_bottom.red(),
      p2_bottom.green(),
      p2_bottom.blue(),
      p2_bottom.alpha()
    ),
    (255, 102, 0, 255),
    "expected min edge at 2.5px to exclude pixel 2"
  );
  let p3_bottom = canvas.pixmap().pixel(3, 3).unwrap();
  assert_eq!(
    (
      p3_bottom.red(),
      p3_bottom.green(),
      p3_bottom.blue(),
      p3_bottom.alpha()
    ),
    (0, 255, 0, 255),
    "expected min edge at 2.5px to include pixel 3"
  );
}

#[test]
fn semi_transparent_axis_aligned_rect_fills_snap_half_pixel_edges_like_chrome() {
  // Chrome/Skia rasterize axis-aligned rect fills without anti-aliasing and decide coverage based
  // on pixel centers ("open min / closed max"), even when the fill is semi-transparent.
  //
  // This matters for hairlines like `height: 0.5px`, which should cover a full device pixel row
  // (not a half-coverage blended row).
  let bg = Rgba::WHITE;
  let src = Rgba::BLACK.with_alpha(0.4);
  let mut canvas = Canvas::new(10, 2, bg).unwrap();
  canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 10.0, 0.5), src);

  let p0 = canvas.pixmap().pixel(0, 0).unwrap();
  assert_eq!(
    (p0.red(), p0.green(), p0.blue(), p0.alpha()),
    (153, 153, 153, 255),
    "expected 0.5px-high rect to cover the first scanline at full alpha (0.4 over white)"
  );

  let p1 = canvas.pixmap().pixel(0, 1).unwrap();
  assert_eq!(
    (p1.red(), p1.green(), p1.blue(), p1.alpha()),
    (255, 255, 255, 255),
    "expected rect not to bleed into the next scanline"
  );
}

#[test]
fn near_opaque_axis_aligned_rect_fills_are_pixel_snapped() {
  let mut canvas = Canvas::new(10, 60, Rgba::WHITE).unwrap();

  // `Canvas::draw_rect` treats fills as "opaque" based on the quantized 8-bit alpha channel.
  // In practice, computed opacity values can be extremely close to 1.0 while still rounding to 255
  // when mapped via `round(alpha * 255)`. Those should be considered opaque for the purposes of
  // pixel snapping; otherwise we end up with anti-aliased seams at fractional boundaries even
  // though the fill is effectively fully opaque.
  canvas.set_opacity(0.999);
  canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 10.0, 51.6), Rgba::rgb(0, 38, 118));
  canvas.draw_rect(Rect::from_xywh(0.0, 51.6, 10.0, 8.4), Rgba::WHITE);

  let p51 = canvas.pixmap().pixel(0, 51).unwrap();
  assert_eq!(
    (p51.red(), p51.green(), p51.blue(), p51.alpha()),
    (0, 38, 118, 255),
    "expected the last covered row to be fully filled (no blended seam)"
  );

  let p52 = canvas.pixmap().pixel(0, 52).unwrap();
  assert_eq!(
    (p52.red(), p52.green(), p52.blue(), p52.alpha()),
    (255, 255, 255, 255),
    "expected the fill not to extend past the snapped bounds"
  );
}

#[test]
fn opaque_axis_aligned_rect_fills_inside_clips_are_pixel_snapped() {
  let mut canvas = Canvas::new(10, 60, Rgba::WHITE).unwrap();
  canvas
    .set_clip(Rect::from_xywh(0.0, 0.6, 10.0, 59.0))
    .unwrap();

  // When an opaque rect is fully inside the clip bounds, we still want to pixel-snap its edges.
  // This matches how Chrome avoids fractional-edge seams for UI elements inside scrolling/clip
  // containers (e.g. the Berkeley hero video controls, which are clipped to the hero media box).
  canvas.draw_rect(Rect::from_xywh(2.0, 10.6, 6.0, 5.0), Rgba::rgb(0, 38, 118));

  let p10 = canvas.pixmap().pixel(2, 10).unwrap();
  assert_eq!(
    (p10.red(), p10.green(), p10.blue(), p10.alpha()),
    (255, 255, 255, 255)
  );

  let p11 = canvas.pixmap().pixel(2, 11).unwrap();
  assert_eq!(
    (p11.red(), p11.green(), p11.blue(), p11.alpha()),
    (0, 38, 118, 255)
  );
}

#[test]
fn source_over_trunc_rect_fill_near_integer_bounds_matches_integer() {
  let overlay = Rgba::new(0, 0, 0, 0.3);

  // Use a non-white background so we exercise `mul/255` rounding differences between the
  // truncating fast-path and tiny-skia's default blending.
  let mut exact = Canvas::new(10, 10, Rgba::rgb(200, 200, 200)).unwrap();
  exact.draw_rect(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), overlay);
  let exact = exact.into_pixmap();

  // Chrome/Skia-style truncating `mul/255` compositing (0.3 over 200) yields 139.
  let expected = exact.pixel(5, 5).unwrap();
  assert_eq!(
    (
      expected.red(),
      expected.green(),
      expected.blue(),
      expected.alpha()
    ),
    (139, 139, 139, 255)
  );

  // A "should-be-integer" edge can land slightly below an integer due to float noise.
  // The renderer should treat it as pixel-aligned and produce identical output.
  let mut near = Canvas::new(10, 10, Rgba::rgb(200, 200, 200)).unwrap();
  near.draw_rect(Rect::from_xywh(0.0, 0.0, 9.9996, 9.9996), overlay);
  let near = near.into_pixmap();

  assert_eq!(
    near.data(),
    exact.data(),
    "near-integer device bounds should use the truncating source-over fast path"
  );
}

#[test]
fn source_over_trunc_rect_fill_near_integer_translation_matches_integer() {
  let overlay = Rgba::new(0, 0, 0, 0.3);

  let mut exact = Canvas::new(10, 10, Rgba::rgb(200, 200, 200)).unwrap();
  exact.draw_rect(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), overlay);
  let exact = exact.into_pixmap();

  // Start at a near-integer translation that comes from float noise (e.g. layout math).
  let mut near = Canvas::new(10, 10, Rgba::rgb(200, 200, 200)).unwrap();
  near.translate(0.0004, 0.0004);
  near.draw_rect(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), overlay);
  let near = near.into_pixmap();

  assert_eq!(
    near.data(),
    exact.data(),
    "near-integer translation should use the truncating source-over fast path"
  );
}

#[test]
fn source_over_trunc_rounded_rect_fill_near_integer_translation_uses_truncation() {
  let overlay = Rgba::new(0, 0, 0, 0.3);

  let mut canvas = Canvas::new(10, 10, Rgba::rgb(200, 200, 200)).unwrap();
  canvas.translate(0.0004, 0.0004);
  canvas.draw_rounded_rect(
    Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    BorderRadii::uniform(2.0),
    overlay,
  );

  // A fully covered pixel should use truncating `mul/255` compositing (0.3 over 200 => 139).
  let p = canvas.pixmap().pixel(5, 5).unwrap();
  assert_eq!(
    (p.red(), p.green(), p.blue(), p.alpha()),
    (139, 139, 139, 255)
  );
}

#[test]
fn semi_transparent_source_over_rect_fills_match_chrome_blending() {
  // Regression for large translucent overlays (e.g. `tests/pages/fixtures/figma.com`), where a
  // ±1 LSB mismatch in `source-over` compositing can flip the entire viewport in pixel diffs.
  let mut canvas = Canvas::new(4, 4, Rgba::WHITE).unwrap();
  canvas.draw_rect(
    Rect::from_xywh(0.0, 0.0, 4.0, 4.0),
    Rgba::BLACK.with_alpha(0.3),
  );

  let p = canvas.pixmap().pixel(0, 0).unwrap();
  assert_eq!(
    (p.red(), p.green(), p.blue(), p.alpha()),
    (178, 178, 178, 255)
  );
}

#[test]
fn semi_transparent_source_over_rect_fills_round_src_premultiplication_like_chrome() {
  // Regression for iana.org: the home page background uses `rgba(223, 227, 230, 0.2)` over a white
  // canvas. Chrome produces `rgb(249, 249, 250)` while truncating premultiplication yields
  // `rgb(248, 249, 250)` and flips a large portion of the viewport in strict pixel diffs.
  let mut canvas = Canvas::new(1, 1, Rgba::WHITE).unwrap();
  canvas.draw_rect(
    Rect::from_xywh(0.0, 0.0, 1.0, 1.0),
    Rgba::new(223, 227, 230, 0.2),
  );

  let p = canvas.pixmap().pixel(0, 0).unwrap();
  assert_eq!((p.red(), p.green(), p.blue(), p.alpha()), (249, 249, 250, 255));
}

#[test]
fn semi_transparent_source_over_rect_fills_with_fractional_bounds_match_chrome_blending() {
  // Regression for large translucent overlays with fractional device bounds (e.g. `imdb.com` hero
  // panels), where tiny-skia's blending math produces a pervasive ±1 LSB mismatch even for
  // *fully covered* interior pixels.
  let mut canvas = Canvas::new(6, 6, Rgba::rgb(18, 18, 18)).unwrap();
  canvas.draw_rect(
    // Fractional left and right edges. Pixel (2,2) is fully covered.
    Rect::from_xywh(0.25, 0.0, 5.5, 6.0),
    Rgba::WHITE.with_alpha(0.1),
  );

  let p = canvas.pixmap().pixel(2, 2).unwrap();
  assert_eq!((p.red(), p.green(), p.blue(), p.alpha()), (42, 42, 42, 255));
}

#[test]
fn semi_transparent_source_over_rect_fills_inside_rounded_clips_match_chrome_blending() {
  // Like `semi_transparent_source_over_rect_fills_with_fractional_bounds_match_chrome_blending`,
  // but with an active rounded-rect clip mask. IMDb's hero overlays are drawn inside a rounded
  // clip (border-radius), and the same ±1 blending mismatch shows up across the clipped interior.
  let mut canvas = Canvas::new(40, 40, Rgba::rgb(18, 18, 18)).unwrap();
  canvas
    .set_clip_with_radii(
      Rect::from_xywh(0.0, 0.0, 40.0, 40.0),
      Some(BorderRadii::uniform(12.0)),
    )
    .unwrap();
  canvas.draw_rect(
    Rect::from_xywh(0.25, 0.0, 39.5, 40.0),
    Rgba::WHITE.with_alpha(0.1),
  );

  let p = canvas.pixmap().pixel(20, 20).unwrap();
  assert_eq!((p.red(), p.green(), p.blue(), p.alpha()), (42, 42, 42, 255));
}

#[test]
fn source_over_trunc_fast_path_accepts_near_integer_translation_and_bounds() {
  // Use a case where tiny-skia's `source-over` math differs from Skia/Chrome.
  let bg = Rgba::rgb(200, 200, 200);
  let src = Rgba::BLACK.with_alpha(0.3);
  let expected = source_over_skia(bg, src);

  let rect = Rect::from_xywh(0.0004, 0.0004, 5.0, 5.0);
  let (tx, ty) = (10.0004, 20.0004);

  // Near-integer translations + near-integer device bounds should still take the truncating
  // source-over fast path, matching Chrome/Skia arithmetic.
  let mut canvas = Canvas::new(64, 64, bg).unwrap();
  canvas.translate(tx, ty);
  canvas.draw_rect(rect, src);
  let p = canvas.pixmap().pixel(12, 22).unwrap();
  assert_eq!((p.red(), p.green(), p.blue(), p.alpha()), expected);
}

#[test]
fn source_over_trunc_fast_path_rejects_fractional_translation() {
  let bg = Rgba::rgb(200, 200, 200);
  let src = Rgba::BLACK.with_alpha(0.3);
  let expected_trunc = source_over_skia(bg, src);

  // Even though the *resulting* device bounds are integers here (rect.x compensates for tx),
  // a meaningfully fractional translation should not be quantized into the fast path.
  let rect = Rect::from_xywh(-0.25, -0.25, 5.0, 5.0);
  let (tx, ty) = (10.25, 20.25);

  let mut canvas = Canvas::new(64, 64, bg).unwrap();
  canvas.translate(tx, ty);
  canvas.draw_rect(rect, src);
  let p = canvas.pixmap().pixel(12, 22).unwrap();
  let out = (p.red(), p.green(), p.blue(), p.alpha());
  assert_ne!(
    out, expected_trunc,
    "expected tiny-skia output to differ from the truncating fast path"
  );
}

#[test]
fn opaque_axis_aligned_rounded_rect_fills_are_pixel_snapped() {
  // Like `opaque_axis_aligned_rect_fills_are_pixel_snapped`, but for rounded rectangles.
  //
  // YouTube's JS-off skeleton contains many `border-radius: 8px` placeholder blocks whose edges
  // land on fractional pixels (e.g. via `padding-top: 56.25%`). Without snapping, the straight
  // edges anti-alias into the next row/column and show up as blended seams in page-loop diffs.
  let mut canvas = Canvas::new(10, 60, Rgba::WHITE).unwrap();
  let radii = BorderRadii::uniform(2.0);
  canvas.draw_rounded_rect(
    Rect::from_xywh(0.0, 0.0, 10.0, 51.6),
    radii,
    Rgba::rgb(0, 38, 118),
  );
  canvas.draw_rect(Rect::from_xywh(0.0, 51.6, 10.0, 8.4), Rgba::WHITE);

  let p51 = canvas.pixmap().pixel(5, 51).unwrap();
  assert_eq!(
    (p51.red(), p51.green(), p51.blue(), p51.alpha()),
    (0, 38, 118, 255),
    "expected the last covered row to be fully filled (no blended seam)"
  );

  let p52 = canvas.pixmap().pixel(5, 52).unwrap();
  assert_eq!(
    (p52.red(), p52.green(), p52.blue(), p52.alpha()),
    (255, 255, 255, 255),
    "expected the fill not to extend past the snapped bounds"
  );
}

#[test]
fn test_draw_multiple_rects() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  // Draw overlapping rectangles
  canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 50.0, 50.0), Rgba::rgb(255, 0, 0));
  canvas.draw_rect(
    Rect::from_xywh(25.0, 25.0, 50.0, 50.0),
    Rgba::rgb(0, 255, 0),
  );
  canvas.draw_rect(
    Rect::from_xywh(50.0, 50.0, 50.0, 50.0),
    Rgba::rgb(0, 0, 255),
  );

  // Just verify it completes without crashing
  let _ = canvas.into_pixmap();
}

#[test]
fn test_stroke_rect() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  let rect = Rect::from_xywh(20.0, 20.0, 60.0, 40.0);
  canvas.stroke_rect(rect, Rgba::BLACK, 2.0);

  let _ = canvas.into_pixmap();
}

// ============================================================================
// Rounded Rectangle Tests
// ============================================================================

#[test]
fn test_draw_rounded_rect_uniform() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  let rect = Rect::from_xywh(10.0, 10.0, 80.0, 60.0);
  let radii = BorderRadii::uniform(10.0);
  canvas.draw_rounded_rect(rect, radii, Rgba::rgb(100, 150, 200));

  let _ = canvas.into_pixmap();
}

#[test]
fn test_draw_rounded_rect_different_radii() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  let rect = Rect::from_xywh(10.0, 10.0, 80.0, 60.0);
  let radii = BorderRadii::new(
    BorderRadius::uniform(5.0),
    BorderRadius::uniform(10.0),
    BorderRadius::uniform(15.0),
    BorderRadius::uniform(20.0),
  );
  canvas.draw_rounded_rect(rect, radii, Rgba::rgb(200, 100, 50));

  let _ = canvas.into_pixmap();
}

#[test]
fn test_draw_rounded_rect_large_radius() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  // Radius larger than half the height - should be clamped
  let rect = Rect::from_xywh(10.0, 10.0, 80.0, 30.0);
  let radii = BorderRadii::uniform(50.0); // Will be clamped to 15
  canvas.draw_rounded_rect(rect, radii, Rgba::rgb(50, 100, 200));

  let _ = canvas.into_pixmap();
}

#[test]
fn test_stroke_rounded_rect() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  let rect = Rect::from_xywh(15.0, 15.0, 70.0, 50.0);
  let radii = BorderRadii::uniform(8.0);
  canvas.stroke_rounded_rect(rect, radii, Rgba::BLACK, 3.0);

  let _ = canvas.into_pixmap();
}

// ============================================================================
// Circle Drawing Tests
// ============================================================================

#[test]
fn test_draw_circle() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  canvas.draw_circle(Point::new(50.0, 50.0), 30.0, Rgba::rgb(255, 128, 0));

  let _ = canvas.into_pixmap();
}

#[test]
fn test_stroke_circle() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  canvas.stroke_circle(Point::new(50.0, 50.0), 40.0, Rgba::BLACK, 2.0);

  let _ = canvas.into_pixmap();
}

// ============================================================================
// Line Drawing Tests
// ============================================================================

#[test]
fn test_draw_line() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  canvas.draw_line(
    Point::new(10.0, 10.0),
    Point::new(90.0, 90.0),
    Rgba::BLACK,
    1.0,
  );

  let _ = canvas.into_pixmap();
}

#[test]
fn test_draw_line_thick() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  canvas.draw_line(
    Point::new(0.0, 50.0),
    Point::new(100.0, 50.0),
    Rgba::rgb(128, 0, 255),
    5.0,
  );

  let _ = canvas.into_pixmap();
}

// ============================================================================
// State Management Tests
// ============================================================================

#[test]
fn test_save_restore_opacity() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  assert_eq!(canvas.opacity(), 1.0);

  canvas.save();
  canvas.set_opacity(0.5);
  assert_eq!(canvas.opacity(), 0.5);

  canvas.save();
  canvas.set_opacity(0.25);
  assert_eq!(canvas.opacity(), 0.25);

  canvas.restore();
  assert_eq!(canvas.opacity(), 0.5);

  canvas.restore();
  assert_eq!(canvas.opacity(), 1.0);
}

#[test]
fn test_save_restore_transform() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  canvas.save();
  canvas.translate(10.0, 20.0);

  let transform = canvas.transform();
  assert!((transform.tx - 10.0).abs() < 0.001);
  assert!((transform.ty - 20.0).abs() < 0.001);

  canvas.restore();

  let transform = canvas.transform();
  assert!((transform.tx).abs() < 0.001);
  assert!((transform.ty).abs() < 0.001);
}

#[test]
fn test_nested_transforms() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  canvas.save();
  canvas.translate(10.0, 0.0);

  canvas.save();
  canvas.translate(20.0, 0.0);

  // Combined translation should be 30
  let transform = canvas.transform();
  assert!((transform.tx - 30.0).abs() < 0.001);

  canvas.restore();
  canvas.restore();
}

#[test]
fn test_scale_transform() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  canvas.scale(2.0, 2.0);

  // Draw a 10x10 rect - should appear as 20x20
  canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), Rgba::rgb(255, 0, 0));

  let _ = canvas.into_pixmap();
}

// ============================================================================
// Opacity Tests
// ============================================================================

#[test]
fn test_draw_with_opacity() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  canvas.set_opacity(0.5);
  canvas.draw_rect(
    Rect::from_xywh(10.0, 10.0, 80.0, 80.0),
    Rgba::rgb(255, 0, 0),
  );

  let _ = canvas.into_pixmap();
}

#[test]
fn test_opacity_clamping() {
  let mut canvas = Canvas::new(10, 10, Rgba::WHITE).unwrap();

  canvas.set_opacity(1.5);
  assert_eq!(canvas.opacity(), 1.0);

  canvas.set_opacity(-0.5);
  assert_eq!(canvas.opacity(), 0.0);
}

// ============================================================================
// Clipping Tests
// ============================================================================

#[test]
fn test_clip_rect() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  // Set clip to center region
  canvas
    .set_clip(Rect::from_xywh(25.0, 25.0, 50.0, 50.0))
    .unwrap();

  // Draw rectangle that extends beyond clip
  canvas.draw_rect(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    Rgba::rgb(255, 0, 0),
  );

  // Clear clip
  canvas.clear_clip();

  // Draw another rect - should not be clipped
  canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), Rgba::rgb(0, 0, 255));

  let _ = canvas.into_pixmap();
}

#[test]
fn fractional_rect_clips_do_not_overdraw_adjacent_border_pixels() {
  // Regression for `news.ycombinator.com` (Hacker News): the clipped logo image over-drew the 1px
  // white border when the clip rect started at a fractional x (e.g. x=99.8).
  let mut canvas = Canvas::new(4, 4, Rgba::rgb(255, 102, 0)).unwrap();

  // White "border" stripe at x=1.
  canvas.draw_rect(Rect::from_xywh(1.0, 0.0, 1.0, 4.0), Rgba::WHITE);

  // Clip begins inside that border pixel by 0.2px. The center of pixel x=1 is at 1.5, which is
  // outside the clip, so drawing inside the clip must not affect the border pixel.
  canvas
    .set_clip(Rect::from_xywh(1.8, 0.0, 2.0, 4.0))
    .unwrap();

  // Semi-transparent fill avoids the opaque-rect snapping path and ensures we detect any clip
  // expansion into the border pixel.
  canvas.draw_rect(
    Rect::from_xywh(0.0, 0.0, 4.0, 4.0),
    Rgba::GREEN.with_alpha(0.5),
  );

  let p = canvas.pixmap().pixel(1, 2).unwrap();
  assert_eq!(
    (p.red(), p.green(), p.blue(), p.alpha()),
    (255, 255, 255, 255),
    "expected the border pixel to remain untouched by the clipped fill"
  );
}

// ============================================================================
// Blend Mode Tests
// ============================================================================

#[test]
fn test_blend_mode_multiply() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  // Draw background
  canvas.draw_rect(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    Rgba::rgb(200, 200, 200),
  );

  // Set multiply blend
  canvas.set_blend_mode(BlendMode::Multiply);

  // Draw overlapping rect
  canvas.draw_rect(
    Rect::from_xywh(25.0, 25.0, 50.0, 50.0),
    Rgba::rgb(255, 100, 100),
  );

  let _ = canvas.into_pixmap();
}

#[test]
fn test_blend_mode_screen() {
  let mut canvas = Canvas::new(100, 100, Rgba::BLACK).unwrap();

  canvas.set_blend_mode(BlendMode::Screen);
  canvas.draw_rect(
    Rect::from_xywh(20.0, 20.0, 60.0, 60.0),
    Rgba::rgb(100, 100, 200),
  );

  let _ = canvas.into_pixmap();
}

// ============================================================================
// Border Radii Tests
// ============================================================================

#[test]
fn test_border_radii_constructors() {
  let zero = BorderRadii::ZERO;
  assert!(!zero.has_radius());

  let uniform = BorderRadii::uniform(10.0);
  assert!(uniform.has_radius());
  assert!(uniform.is_uniform());

  let different = BorderRadii::new(
    BorderRadius::uniform(1.0),
    BorderRadius::uniform(2.0),
    BorderRadius::uniform(3.0),
    BorderRadius::uniform(4.0),
  );
  assert!(different.has_radius());
  assert!(!different.is_uniform());
}

#[test]
fn test_border_radii_max_radius() {
  let radii = BorderRadii::new(
    BorderRadius::uniform(5.0),
    BorderRadius::uniform(10.0),
    BorderRadius::uniform(15.0),
    BorderRadius::uniform(20.0),
  );
  assert_eq!(radii.max_radius(), 20.0);
}

// ============================================================================
// Text Drawing Tests
// ============================================================================

#[test]
fn test_draw_text_empty() {
  let mut canvas = Canvas::new(200, 100, Rgba::WHITE).unwrap();

  // Get a font
  let font_ctx = FontContext::new();

  // Skip if no fonts available
  if font_ctx.get_sans_serif().is_none() {
    return;
  }

  let font = font_ctx.get_sans_serif().unwrap();

  // Draw empty glyphs - should not crash
  canvas
    .draw_text(
      Point::new(10.0, 50.0),
      &[],
      &font,
      16.0,
      Rgba::BLACK,
      0.0,
      0.0,
      0,
      &[],
      0,
      &[],
    )
    .unwrap();

  let _ = canvas.into_pixmap();
}

#[test]
fn canvas_draw_text_respects_deadline_timeout() {
  let font = FontContext::new()
    .get_sans_serif()
    .expect("expected bundled sans-serif font for tests");
  let face = font.as_ttf_face().expect("parse test font");
  let glyph_id = face
    .glyph_index(' ')
    .or_else(|| face.glyph_index('A'))
    .expect("resolve glyph for deadline timeout test")
    .0 as u32;

  let glyphs = [GlyphInstance {
    glyph_id,
    cluster: 0,
    x_offset: 0.0,
    y_offset: 0.0,
    x_advance: 0.0,
    y_advance: 0.0,
  }];

  let checks = Arc::new(AtomicUsize::new(0));
  let checks_for_cb = Arc::clone(&checks);
  let cancel: Arc<CancelCallback> = Arc::new(move || {
    let prev = checks_for_cb.fetch_add(1, Ordering::SeqCst);
    prev >= 1
  });
  let deadline = RenderDeadline::new(None, Some(cancel));

  let mut canvas = Canvas::new(16, 16, Rgba::WHITE).unwrap();
  let result = with_deadline(Some(&deadline), || {
    canvas.draw_text(
      Point::new(0.0, 12.0),
      &glyphs,
      &font,
      16.0,
      Rgba::BLACK,
      0.0,
      0.0,
      0,
      &[],
      0,
      &[],
    )
  });

  assert!(
    checks.load(Ordering::SeqCst) >= 2,
    "expected entry + final deadline checks, got {}",
    checks.load(Ordering::SeqCst)
  );
  assert!(
    matches!(
      result,
      Err(Error::Render(RenderError::Timeout {
        stage: RenderStage::Paint,
        ..
      }))
    ),
    "expected paint-stage timeout, got {result:?}"
  );
}

#[test]
fn test_draw_text_with_glyphs() {
  let mut canvas = Canvas::new(200, 50, Rgba::WHITE).unwrap();

  // Get a font
  let font_ctx = FontContext::new();

  if !font_ctx.has_fonts() {
    return;
  }

  let pipeline = ShapingPipeline::new();
  let style = ComputedStyle::default();
  let shaped = match pipeline.shape("Hello, World!", &style, &font_ctx) {
    Ok(runs) => runs,
    Err(_) => return,
  };
  let Some(run) = shaped.first() else {
    return;
  };
  let glyphs = to_glyph_instances(run);
  let variations: Vec<_> = run
    .variations
    .iter()
    .copied()
    .map(FontVariation::from)
    .collect();

  // Draw the glyphs
  canvas
    .draw_text(
      Point::new(10.0, 30.0),
      &glyphs,
      &run.font,
      run.font_size,
      Rgba::BLACK,
      run.synthetic_bold,
      run.synthetic_oblique,
      run.palette_index,
      run.palette_overrides.as_slice(),
      run.palette_override_hash,
      &variations,
    )
    .unwrap();

  let pixmap = canvas.into_pixmap();
  assert!(pixmap.data().iter().any(|&b| b != 255)); // Some non-white pixels
}

#[test]
fn test_draw_text_colored() {
  let mut canvas = Canvas::new(200, 50, Rgba::WHITE).unwrap();

  let font_ctx = FontContext::new();
  if !font_ctx.has_fonts() {
    return;
  }

  let pipeline = ShapingPipeline::new();
  let style = ComputedStyle::default();
  let shaped = match pipeline.shape("Red Text", &style, &font_ctx) {
    Ok(runs) => runs,
    Err(_) => return,
  };
  let Some(run) = shaped.first() else {
    return;
  };
  let glyphs = to_glyph_instances(run);
  let variations: Vec<_> = run
    .variations
    .iter()
    .copied()
    .map(FontVariation::from)
    .collect();
  canvas
    .draw_text(
      Point::new(10.0, 35.0),
      &glyphs,
      &run.font,
      run.font_size,
      Rgba::rgb(255, 0, 0),
      run.synthetic_bold,
      run.synthetic_oblique,
      run.palette_index,
      run.palette_overrides.as_slice(),
      run.palette_override_hash,
      &variations,
    )
    .unwrap();

  let _ = canvas.into_pixmap();
}

#[test]
fn test_draw_text_with_opacity() {
  let mut canvas = Canvas::new(200, 50, Rgba::WHITE).unwrap();

  let font_ctx = FontContext::new();
  if !font_ctx.has_fonts() {
    return;
  }

  let pipeline = ShapingPipeline::new();
  let style = ComputedStyle::default();

  canvas.set_opacity(0.5);

  let shaped = match pipeline.shape("Faded", &style, &font_ctx) {
    Ok(runs) => runs,
    Err(_) => return,
  };
  let Some(run) = shaped.first() else {
    return;
  };
  let glyphs = to_glyph_instances(run);
  let variations: Vec<_> = run
    .variations
    .iter()
    .copied()
    .map(FontVariation::from)
    .collect();
  canvas
    .draw_text(
      Point::new(10.0, 35.0),
      &glyphs,
      &run.font,
      run.font_size,
      Rgba::BLACK,
      run.synthetic_bold,
      run.synthetic_oblique,
      run.palette_index,
      run.palette_overrides.as_slice(),
      run.palette_override_hash,
      &variations,
    )
    .unwrap();

  let _ = canvas.into_pixmap();
}

#[test]
fn canvas_renders_color_fonts() {
  let font_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fonts/ColorTestCOLR.ttf");
  let font_bytes = std::fs::read(&font_path).expect("read color font fixture");
  let mut db = FontDatabase::empty();
  db.load_font_data(font_bytes)
    .expect("load color font into database");
  let font_ctx = FontContext::with_database(Arc::new(db));

  let mut style = ComputedStyle::default();
  style.font_family = vec!["ColorTestCOLR".to_string()].into();
  style.font_size = 32.0;

  let pipeline = ShapingPipeline::new();
  let shaped = pipeline
    .shape("A", &style, &font_ctx)
    .expect("shape color glyph");
  let run = shaped.first().expect("color run");
  let glyphs = to_glyph_instances(run);
  let variations: Vec<_> = run
    .variations
    .iter()
    .copied()
    .map(FontVariation::from)
    .collect();

  let mut canvas = Canvas::new(64, 64, Rgba::WHITE).unwrap();
  canvas
    .draw_text(
      Point::new(12.0, 48.0),
      &glyphs,
      &run.font,
      run.font_size * run.scale,
      Rgba::BLACK,
      run.synthetic_bold,
      run.synthetic_oblique,
      run.palette_index,
      run.palette_overrides.as_slice(),
      run.palette_override_hash,
      &variations,
    )
    .unwrap();
  let pixmap = canvas.into_pixmap();
  let actual_image = pixmap_to_rgba_image(&pixmap);
  let actual_png = encode_png(&actual_image).expect("encode actual png");

  let golden_path =
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden/canvas_color_font.png");
  if std::env::var("UPDATE_GOLDEN").is_ok() {
    std::fs::write(&golden_path, &actual_png).expect("write golden");
  }
  let expected_png = std::fs::read(&golden_path).expect("read golden");
  let expected_image = decode_png(&expected_png).expect("decode golden");

  let diff = compare_images(&actual_image, &expected_image, &CompareConfig::lenient());
  assert!(
    diff.is_match(),
    "color font raster mismatch: {}",
    diff.summary()
  );

  let unique_colors: HashSet<(u8, u8, u8)> = pixmap
    .data()
    .chunks_exact(4)
    .map(|c| (c[2], c[1], c[0]))
    .collect();
  assert!(
    unique_colors.len() > 2,
    "expected multiple colors in color glyph rendering"
  );
}

#[test]
fn canvas_respects_font_palette() {
  let font_path =
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts/PaletteTestCOLRv1.ttf");
  let font_bytes = std::fs::read(&font_path).expect("read palette font fixture");
  let mut db = FontDatabase::empty();
  db.load_font_data(font_bytes)
    .expect("load palette font into database");
  let font_ctx = FontContext::with_database(Arc::new(db));

  let mut style = ComputedStyle::default();
  style.font_family = vec!["PaletteTestCOLRv1".to_string()].into();
  // Keep this test's output in sync with `text::tests::font_palette`, which uses 72px
  // goldens for PaletteTestCOLRv1.
  style.font_size = 72.0;
  style.root_font_size = style.font_size;
  style.font_palette = FontPalette::Dark;

  let pipeline = ShapingPipeline::new();
  let runs = pipeline
    .shape("A", &style, &font_ctx)
    .expect("shape palette font glyph");
  let palette_run = runs.first().expect("palette run");
  let palette_glyph = palette_run.glyphs.first().expect("palette glyph");
  let palette_instance =
    FontInstance::new(&palette_run.font, &palette_run.variations).expect("palette instance");
  let palette_raster = ColorFontRenderer::new()
    .render(
      &palette_run.font,
      &palette_instance,
      palette_glyph.glyph_id,
      palette_run.font_size,
      palette_run.palette_index,
      palette_run.palette_overrides.as_ref(),
      0,
      Rgba::BLACK,
      palette_run.synthetic_oblique,
      &palette_run.variations,
      None,
    )
    .expect("color glyph raster");

  let origin = Point::new(
    -(palette_glyph.x_offset + palette_raster.left as f32),
    -(palette_glyph.y_offset + palette_raster.top as f32),
  );
  let mut canvas = Canvas::new(
    palette_raster.image.width(),
    palette_raster.image.height(),
    Rgba::TRANSPARENT,
  )
  .unwrap();
  canvas
    .draw_shaped_run(palette_run, origin, Rgba::BLACK)
    .unwrap();
  let palette_pixmap = canvas.into_pixmap();
  let palette_png = palette_pixmap.encode_png().expect("encode palette png");

  let golden_path =
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden/font_palette_dark.png");
  if std::env::var("UPDATE_GOLDEN").is_ok() {
    std::fs::write(&golden_path, &palette_png).expect("write palette golden");
  }
  let expected_png = std::fs::read(&golden_path).expect("read palette golden");
  let diff = compare_png(&palette_png, &expected_png, &CompareConfig::lenient()).expect("compare");
  assert!(
    diff.is_match(),
    "palette color glyph raster mismatch: {}",
    diff.summary()
  );

  let mut normal_style = style.clone();
  normal_style.font_palette = FontPalette::Normal;
  let normal_runs = pipeline
    .shape("A", &normal_style, &font_ctx)
    .expect("shape normal palette glyph");
  let normal_run = normal_runs.first().expect("normal palette run");
  let normal_glyph = normal_run.glyphs.first().expect("normal glyph");
  let normal_instance =
    FontInstance::new(&normal_run.font, &normal_run.variations).expect("normal instance");
  let normal_raster = ColorFontRenderer::new()
    .render(
      &normal_run.font,
      &normal_instance,
      normal_glyph.glyph_id,
      normal_run.font_size,
      normal_run.palette_index,
      normal_run.palette_overrides.as_ref(),
      0,
      Rgba::BLACK,
      normal_run.synthetic_oblique,
      &normal_run.variations,
      None,
    )
    .expect("normal palette raster");
  let normal_origin = Point::new(
    -(normal_glyph.x_offset + normal_raster.left as f32),
    -(normal_glyph.y_offset + normal_raster.top as f32),
  );
  let mut normal_canvas = Canvas::new(
    normal_raster.image.width(),
    normal_raster.image.height(),
    Rgba::TRANSPARENT,
  )
  .unwrap();
  normal_canvas
    .draw_shaped_run(normal_run, normal_origin, Rgba::BLACK)
    .unwrap();
  let normal_png = normal_canvas
    .into_pixmap()
    .encode_png()
    .expect("encode normal png");

  assert_ne!(
    normal_png, palette_png,
    "different palettes should render different canvas outputs"
  );
}

// ============================================================================
// Edge Cases
// ============================================================================

#[test]
fn test_draw_transparent() {
  let mut canvas = Canvas::new(50, 50, Rgba::WHITE).unwrap();

  // Draw with transparent color should be a no-op
  canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 50.0, 50.0), Rgba::TRANSPARENT);

  // Canvas should still be white
  let data = canvas.pixmap().data();
  assert_eq!(data[0], 255);
  assert_eq!(data[1], 255);
  assert_eq!(data[2], 255);
  assert_eq!(data[3], 255);
}

#[test]
fn test_draw_zero_size_rect() {
  let mut canvas = Canvas::new(50, 50, Rgba::WHITE).unwrap();

  // Zero-width rect
  canvas.draw_rect(Rect::from_xywh(10.0, 10.0, 0.0, 20.0), Rgba::rgb(255, 0, 0));

  // Zero-height rect
  canvas.draw_rect(Rect::from_xywh(10.0, 10.0, 20.0, 0.0), Rgba::rgb(255, 0, 0));

  let _ = canvas.into_pixmap();
}

#[test]
fn test_draw_outside_bounds() {
  let mut canvas = Canvas::new(50, 50, Rgba::WHITE).unwrap();

  // Rect completely outside canvas
  canvas.draw_rect(
    Rect::from_xywh(100.0, 100.0, 20.0, 20.0),
    Rgba::rgb(255, 0, 0),
  );

  // Rect partially outside
  canvas.draw_rect(
    Rect::from_xywh(-10.0, -10.0, 30.0, 30.0),
    Rgba::rgb(0, 0, 255),
  );

  let _ = canvas.into_pixmap();
}

#[test]
fn test_draw_negative_radius_circle() {
  let mut canvas = Canvas::new(50, 50, Rgba::WHITE).unwrap();

  // Should not crash with negative radius
  canvas.draw_circle(Point::new(25.0, 25.0), -10.0, Rgba::rgb(255, 0, 0));

  let _ = canvas.into_pixmap();
}

// ============================================================================
// Complex Rendering Tests
// ============================================================================

#[test]
fn test_complex_scene() {
  let mut canvas = Canvas::new(200, 200, Rgba::rgb(240, 240, 240)).unwrap();

  // Background rectangle
  canvas.draw_rect(Rect::from_xywh(10.0, 10.0, 180.0, 180.0), Rgba::WHITE);

  // Rounded header
  let radii = BorderRadii::new(
    BorderRadius::uniform(5.0),
    BorderRadius::uniform(5.0),
    BorderRadius::ZERO,
    BorderRadius::ZERO,
  );
  canvas.draw_rounded_rect(
    Rect::from_xywh(10.0, 10.0, 180.0, 40.0),
    radii,
    Rgba::rgb(51, 102, 204),
  );

  // Content area with border
  canvas.stroke_rect(
    Rect::from_xywh(20.0, 60.0, 160.0, 120.0),
    Rgba::rgb(200, 200, 200),
    1.0,
  );

  // Decorative circles
  canvas.draw_circle(Point::new(50.0, 100.0), 15.0, Rgba::rgb(255, 100, 100));
  canvas.draw_circle(Point::new(100.0, 100.0), 15.0, Rgba::rgb(100, 255, 100));
  canvas.draw_circle(Point::new(150.0, 100.0), 15.0, Rgba::rgb(100, 100, 255));

  // Lines
  canvas.draw_line(
    Point::new(30.0, 140.0),
    Point::new(170.0, 140.0),
    Rgba::rgb(150, 150, 150),
    1.0,
  );
  canvas.draw_line(
    Point::new(30.0, 160.0),
    Point::new(170.0, 160.0),
    Rgba::rgb(150, 150, 150),
    1.0,
  );

  let pixmap = canvas.into_pixmap();
  assert_eq!(pixmap.width(), 200);
  assert_eq!(pixmap.height(), 200);
}

#[test]
fn test_layered_opacity() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  // Layer 1: Base rectangle
  canvas.draw_rect(
    Rect::from_xywh(10.0, 10.0, 80.0, 80.0),
    Rgba::rgb(255, 0, 0),
  );

  // Layer 2: 50% opacity rectangle
  canvas.save();
  canvas.set_opacity(0.5);
  canvas.draw_rect(
    Rect::from_xywh(20.0, 20.0, 60.0, 60.0),
    Rgba::rgb(0, 255, 0),
  );
  canvas.restore();

  // Layer 3: Back to full opacity
  canvas.draw_rect(
    Rect::from_xywh(35.0, 35.0, 30.0, 30.0),
    Rgba::rgb(0, 0, 255),
  );

  let _ = canvas.into_pixmap();
}

#[test]
fn test_transformed_rendering() {
  let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

  // Draw with translation
  canvas.save();
  canvas.translate(50.0, 50.0);
  canvas.draw_rect(
    Rect::from_xywh(-10.0, -10.0, 20.0, 20.0),
    Rgba::rgb(255, 0, 0),
  );
  canvas.restore();

  // Draw with scale
  canvas.save();
  canvas.translate(25.0, 25.0);
  canvas.scale(0.5, 0.5);
  canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 50.0, 50.0), Rgba::rgb(0, 0, 255));
  canvas.restore();

  let _ = canvas.into_pixmap();
}
