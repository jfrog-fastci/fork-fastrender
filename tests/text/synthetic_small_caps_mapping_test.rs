use std::sync::Arc;

use fastrender::style::types::FontVariantCaps;
use fastrender::text::font_db::FontStyle;
use fastrender::text::font_db::FontWeight;
use fastrender::text::font_db::FontDatabase;
use fastrender::text::font_loader::FontContext;
use fastrender::text::pipeline::ShapingPipeline;
use fastrender::ComputedStyle;

const DEJAVU_SANS_FONT: &[u8] = include_bytes!("../fixtures/fonts/DejaVuSans-subset.ttf");
const NOTO_SANS_FONT: &[u8] = include_bytes!("../fixtures/fonts/NotoSans-subset.ttf");

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

#[test]
fn synthetic_small_caps_uppercase_expansion_keeps_runs_mapped_to_original_text() {
  let mut db = FontDatabase::empty();
  db
    .load_font_data(DEJAVU_SANS_FONT.to_vec())
    .expect("fixture font should load");
  db.load_font_data(NOTO_SANS_FONT.to_vec())
    .expect("fixture font should load");
  db.refresh_generic_fallbacks();

  let primary_id = db
    .query("DejaVu Sans", FontWeight::NORMAL, FontStyle::Normal)
    .expect("DejaVu Sans fixture should be queryable");
  let fallback_id = db
    .query("Noto Sans", FontWeight::NORMAL, FontStyle::Normal)
    .expect("Noto Sans fixture should be queryable");
  let primary_font = db.load_font(primary_id).expect("load primary font");
  let primary_face = primary_font.as_ttf_face().expect("parse primary font");
  let fallback_font = db.load_font(fallback_id).expect("load fallback font");
  let fallback_face = fallback_font.as_ttf_face().expect("parse fallback font");

  let ctx = FontContext::with_database(Arc::new(db));

  let mut style = ComputedStyle::default();
  style.font_family = vec!["DejaVu Sans".to_string()].into();
  style.font_size = 20.0;
  style.font_variant_caps = FontVariantCaps::SmallCaps;
  style.font_synthesis.small_caps = true;

  let pipeline = ShapingPipeline::new();

  // LATIN SMALL LIGATURE FFI (uppercases to "FFI").
  let text = "\u{FB03}";
  let uppercase: String = text.chars().next().unwrap().to_uppercase().collect();
  assert_eq!(uppercase, "FFI");
  assert!(
    primary_face.glyph_index('F').is_some(),
    "fixture expectation: DejaVu Sans should cover basic Latin 'F'"
  );
  assert!(
    primary_face.glyph_index('I').is_none(),
    "fixture expectation: DejaVu Sans should not cover basic Latin 'I'"
  );
  assert!(
    fallback_face.glyph_index('I').is_some(),
    "fixture expectation: Noto Sans should cover basic Latin 'I'"
  );

  let runs = pipeline.shape(text, &style, &ctx).expect("shape text");
  assert!(
    runs.len() >= 2,
    "expected uppercase expansion {uppercase:?} to split into multiple font runs"
  );

  for run in &runs {
    assert_eq!(run.start, 0, "run should map to start of original text");
    assert_eq!(run.end, text.len(), "run should map to end of original text");
    assert_eq!(
      run.text, text,
      "synthetic small-caps should preserve the original text slice"
    );
    assert!(
      run.glyphs
        .iter()
        .all(|glyph| (glyph.cluster as usize) < run.text.len()),
      "glyph clusters should remain within the run text"
    );
  }
}
