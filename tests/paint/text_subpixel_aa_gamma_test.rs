use fastrender::debug::runtime::{set_runtime_toggles, RuntimeToggles};
use fastrender::text::font_db::{FontConfig, FontDatabase};
use fastrender::{ComputedStyle, FontContext, Rgba, ShapingPipeline, TextRasterizer};
use std::collections::HashMap;
use std::sync::Arc;
use tiny_skia::Pixmap;

fn sum_rgb(pixmap: &Pixmap) -> u64 {
  pixmap
    .data()
    .chunks_exact(4)
    .map(|px| u64::from(px[0]) + u64::from(px[1]) + u64::from(px[2]))
    .sum()
}

fn render_text(gamma: Option<f32>) -> Pixmap {
  let mut raw = HashMap::new();
  raw.insert("FASTR_TEXT_SUBPIXEL_AA".to_string(), "1".to_string());
  if let Some(gamma) = gamma {
    raw.insert("FASTR_TEXT_SUBPIXEL_AA_GAMMA".to_string(), gamma.to_string());
  }
  let _guard = set_runtime_toggles(Arc::new(RuntimeToggles::from_map(raw)));

  let db = FontDatabase::with_config(&FontConfig::bundled_only());
  let font_ctx = FontContext::with_database(Arc::new(db));
  let pipeline = ShapingPipeline::new();
  let mut style = ComputedStyle::default();
  style.font_size = 32.0;

  let runs = pipeline
    .shape("A", &style, &font_ctx)
    .expect("shape glyph for subpixel AA gamma test");
  let run = runs.first().expect("expected at least one shaped run");

  let mut pixmap = Pixmap::new(96, 96).expect("pixmap");
  pixmap.fill(tiny_skia::Color::WHITE);

  // Use a fractional origin so the glyph edges land between device pixels.
  let mut rasterizer = TextRasterizer::new();
  rasterizer
    .render_shaped_run(run, 10.3, 64.0, Rgba::BLACK, &mut pixmap)
    .expect("render shaped run");
  pixmap
}

#[test]
fn text_subpixel_aa_gamma_darkens_edges() {
  let baseline = render_text(None);
  let baseline_sum = sum_rgb(&baseline);

  let gamma_corrected = render_text(Some(1.4));
  let gamma_sum = sum_rgb(&gamma_corrected);

  assert!(
    gamma_sum < baseline_sum,
    "expected gamma-corrected LCD AA to darken edges: baseline={baseline_sum} gamma={gamma_sum}"
  );
}

