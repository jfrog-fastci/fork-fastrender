use crate::image_compare::{compare_images, decode_png, CompareConfig};
use crate::paint::text_rasterize::{render_glyph, TextRasterizer};
use crate::style::color::Rgba;
use crate::testing::pixmap_to_rgba_image;
use crate::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
use crate::text::pipeline::GlyphPosition;
use std::sync::Arc;
use tiny_skia::Pixmap;

fn load_svg_color_font() -> LoadedFont {
  let path = crate::testing::fixtures_dir().join("fonts/svg-color-glyph-test.ttf");
  let data = std::fs::read(&path).expect("test font missing");
  LoadedFont {
    id: None,
    data: Arc::new(data),
    index: 0,
    family: "SVG Color Glyph Test".to_string(),
    weight: FontWeight::NORMAL,
    style: FontStyle::Normal,
    stretch: FontStretch::Normal,
    face_metrics_overrides: Default::default(),
    face_settings: Default::default(),
  }
}

fn painted_bounds(pixmap: &Pixmap) -> Option<(u32, u32, u32, u32)> {
  let mut min_x = u32::MAX;
  let mut min_y = u32::MAX;
  let mut max_x = 0;
  let mut max_y = 0;

  for (idx, chunk) in pixmap.data().chunks(4).enumerate() {
    // Alpha channel is last in tiny-skia's RGBA layout.
    if chunk[3] == 0 {
      continue;
    }
    let x = (idx as u32) % pixmap.width();
    let y = (idx as u32) / pixmap.width();
    min_x = min_x.min(x);
    min_y = min_y.min(y);
    max_x = max_x.max(x);
    max_y = max_y.max(y);
  }

  if min_x == u32::MAX {
    None
  } else {
    Some((min_x, max_x, min_y, max_y))
  }
}

#[test]
fn svg_glyph_resolves_current_color() {
  let font = load_svg_color_font();
  let face = font.as_ttf_face().unwrap();
  let glyph_id = face.glyph_index('A').map(|gid| gid.0 as u32).unwrap();

  let glyphs = [GlyphPosition {
    glyph_id,
    cluster: 0,
    x_offset: 0.0,
    y_offset: 0.0,
    x_advance: 64.0,
    y_advance: 0.0,
  }];

  let mut rasterizer = TextRasterizer::new();
  let mut red_pixmap = Pixmap::new(120, 120).unwrap();
  rasterizer
    .render_glyphs(
      &glyphs,
      &font,
      64.0,
      10.0,
      90.0,
      Rgba::new(200, 30, 30, 0.8),
      &mut red_pixmap,
    )
    .unwrap();

  let mut blue_pixmap = Pixmap::new(120, 120).unwrap();
  rasterizer
    .render_glyphs(
      &glyphs,
      &font,
      64.0,
      10.0,
      90.0,
      Rgba::new(30, 60, 200, 0.6),
      &mut blue_pixmap,
    )
    .unwrap();

  assert_ne!(
    red_pixmap.data(),
    blue_pixmap.data(),
    "text color should affect SVG glyphs"
  );

  let golden_dir = crate::testing::fixtures_dir().join("golden");

  if std::env::var("UPDATE_GOLDEN").is_ok() {
    pixmap_to_rgba_image(&red_pixmap)
      .save(golden_dir.join("svg_color_glyph_red.png"))
      .expect("failed to save red golden");
    pixmap_to_rgba_image(&blue_pixmap)
      .save(golden_dir.join("svg_color_glyph_blue.png"))
      .expect("failed to save blue golden");
  }

  let expected_red = decode_png(
    &std::fs::read(golden_dir.join("svg_color_glyph_red.png"))
      .expect("missing red golden"),
  )
  .expect("failed to decode red golden");
  let expected_blue = decode_png(
    &std::fs::read(golden_dir.join("svg_color_glyph_blue.png"))
      .expect("missing blue golden"),
  )
  .expect("failed to decode blue golden");

  let strict = CompareConfig::strict();
  let red_diff = compare_images(&pixmap_to_rgba_image(&red_pixmap), &expected_red, &strict);
  assert!(red_diff.is_match(), "{}", red_diff.summary());

  let blue_diff = compare_images(&pixmap_to_rgba_image(&blue_pixmap), &expected_blue, &strict);
  assert!(blue_diff.is_match(), "{}", blue_diff.summary());
}

#[test]
fn svg_glyph_aligns_with_outline_baseline() {
  let font = load_svg_color_font();
  let face = font.as_ttf_face().unwrap();
  let glyph_id = face.glyph_index('A').map(|gid| gid.0 as u32).unwrap();

  let mut rasterizer = TextRasterizer::new();
  let glyphs = [GlyphPosition {
    glyph_id,
    cluster: 0,
    x_offset: 0.0,
    y_offset: 0.0,
    x_advance: 64.0,
    y_advance: 0.0,
  }];

  let mut svg_pixmap = Pixmap::new(140, 140).unwrap();
  rasterizer
    .render_glyphs(
      &glyphs,
      &font,
      64.0,
      20.0,
      100.0,
      Rgba::BLACK,
      &mut svg_pixmap,
    )
    .unwrap();

  let mut outline_pixmap = Pixmap::new(140, 140).unwrap();
  render_glyph(
    &font,
    glyph_id,
    64.0,
    20.0,
    100.0,
    Rgba::BLACK,
    &mut outline_pixmap,
  )
  .unwrap();

  let svg_bounds = painted_bounds(&svg_pixmap).expect("svg glyph should paint");
  let outline_bounds = painted_bounds(&outline_pixmap).expect("outline glyph should paint");

  let dx_min = (svg_bounds.0 as i32 - outline_bounds.0 as i32).abs();
  let dy_min = (svg_bounds.2 as i32 - outline_bounds.2 as i32).abs();
  let dx_max = (svg_bounds.1 as i32 - outline_bounds.1 as i32).abs();
  let dy_max = (svg_bounds.3 as i32 - outline_bounds.3 as i32).abs();

  assert!(
    dx_min <= 1 && dy_min <= 1 && dx_max <= 1 && dy_max <= 1,
    "SVG glyph bounds {svg_bounds:?} should align with outline {outline_bounds:?}"
  );
}
