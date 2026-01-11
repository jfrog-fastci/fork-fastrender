use fastrender::debug::runtime::{set_runtime_toggles, RuntimeToggles};
use fastrender::text::font_db::{FontConfig, FontDatabase};
use fastrender::{ComputedStyle, FontContext, Rgba, ShapingPipeline, TextRasterizer};
use std::collections::HashMap;
use std::sync::Arc;
use tiny_skia::Pixmap;

fn count_colored_pixels(pixmap: &Pixmap) -> usize {
  pixmap
    .data()
    .chunks_exact(4)
    .filter(|px| {
      let r = px[0];
      let g = px[1];
      let b = px[2];
      let a = px[3];
      a != 0 && (r != g || g != b)
    })
    .count()
}

fn render_text_with_subpixel_aa(enabled: bool) -> Pixmap {
  let mut raw = HashMap::new();
  raw.insert(
    "FASTR_TEXT_SUBPIXEL_AA".to_string(),
    if enabled { "1" } else { "0" }.to_string(),
  );
  let _guard = set_runtime_toggles(Arc::new(RuntimeToggles::from_map(raw)));

  let db = FontDatabase::with_config(&FontConfig::bundled_only());
  let font_ctx = FontContext::with_database(Arc::new(db));
  let pipeline = ShapingPipeline::new();
  let mut style = ComputedStyle::default();
  style.font_size = 32.0;

  let runs = pipeline
    .shape("A", &style, &font_ctx)
    .expect("shape glyph for subpixel AA test");
  let run = runs.first().expect("expected at least one shaped run");

  let mut pixmap = Pixmap::new(96, 96).expect("pixmap");
  pixmap.fill(tiny_skia::Color::WHITE);

  // Use a fractional x origin so the glyph edges land between device pixels, producing distinct
  // subpixel coverages.
  let mut rasterizer = TextRasterizer::new();
  rasterizer
    .render_shaped_run(run, 10.3, 64.0, Rgba::BLACK, &mut pixmap)
    .expect("render shaped run");
  pixmap
}

#[test]
fn text_subpixel_aa_produces_colored_edge_pixels() {
  let grayscale = render_text_with_subpixel_aa(false);
  assert_eq!(
    count_colored_pixels(&grayscale),
    0,
    "expected grayscale AA to produce only gray pixels for black text"
  );

  let subpixel = render_text_with_subpixel_aa(true);
  assert!(
    count_colored_pixels(&subpixel) > 0,
    "expected subpixel AA to produce tinted edge pixels (r!=g!=b) for black text"
  );
}

