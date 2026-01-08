use std::sync::Arc;

use fastrender::style::types::FontVariantCaps;
use fastrender::text::font_db::FontDatabase;
use fastrender::text::font_loader::FontContext;
use fastrender::text::pipeline::ShapingPipeline;
use fastrender::ComputedStyle;

const NOTO_SANS_FONT: &[u8] = include_bytes!("../fixtures/fonts/NotoSans-subset.ttf");

#[test]
fn all_small_caps_uses_native_features_when_available() {
  let mut db = FontDatabase::empty();
  db
    .load_font_data(NOTO_SANS_FONT.to_vec())
    .expect("fixture font should load");
  db.refresh_generic_fallbacks();
  let ctx = FontContext::with_database(Arc::new(db));

  let mut style = ComputedStyle::default();
  style.font_family = vec!["Noto Sans".to_string()].into();
  style.font_size = 20.0;
  style.font_variant_caps = FontVariantCaps::AllSmallCaps;
  style.font_synthesis.small_caps = true;

  let pipeline = ShapingPipeline::new();
  let runs = pipeline.shape("Abc", &style, &ctx).expect("shape text");
  assert_eq!(runs.len(), 1, "native small-caps support should not split runs");
  let run = &runs[0];
  assert_eq!(
    run.text, "Abc",
    "native small-caps support should not rewrite the shaped text"
  );
  assert!(
    (run.font_size - style.font_size).abs() < 0.05,
    "native small-caps support should keep the original font size"
  );
}

