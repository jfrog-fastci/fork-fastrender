use std::sync::Arc;

use fastrender::style::types::FontVariantCaps;
use fastrender::text::font_db::FontDatabase;
use fastrender::text::font_loader::FontContext;
use fastrender::text::pipeline::ShapingPipeline;
use fastrender::ComputedStyle;

const DEJAVU_SANS_FONT: &[u8] = include_bytes!("../fixtures/fonts/DejaVuSans-subset.ttf");

#[test]
fn synthetic_small_caps_preserves_original_byte_ranges() {
  let mut db = FontDatabase::empty();
  db
    .load_font_data(DEJAVU_SANS_FONT.to_vec())
    .expect("fixture font should load");
  db.refresh_generic_fallbacks();
  let ctx = FontContext::with_database(Arc::new(db));

  let text = "\u{FB02}"; // LATIN SMALL LIGATURE FL (uppercases to "FL" with shorter UTF-8 byte length).

  let mut style = ComputedStyle::default();
  style.font_family = vec!["DejaVu Sans".to_string()].into();
  style.font_size = 20.0;
  style.font_variant_caps = FontVariantCaps::SmallCaps;
  style.font_synthesis.small_caps = true;

  let pipeline = ShapingPipeline::new();
  let runs = pipeline.shape(text, &style, &ctx).expect("shape text");
  assert_eq!(runs.len(), 1);

  let run = &runs[0];
  assert_eq!(run.start, 0);
  assert_eq!(run.end, text.len());
  assert_eq!(
    run.text, text,
    "synthetic small-caps should preserve the original text slice"
  );
  assert_eq!(
    run.text.len(),
    run.end - run.start,
    "text byte length should match run byte range"
  );
  assert!(
    run.glyphs
      .iter()
      .all(|glyph| (glyph.cluster as usize) < run.text.len()),
    "glyph clusters should remain within the run text"
  );
}

