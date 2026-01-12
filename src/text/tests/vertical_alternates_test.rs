use std::sync::Arc;

use crate::style::types::TextOrientation;
use crate::style::types::WritingMode;
use crate::text::font_db::FontDatabase;
use crate::text::font_loader::FontContext;
use crate::text::pipeline::ShapingPipeline;
use crate::ComputedStyle;
use ttf_parser::Tag;

<<<<<<<< HEAD:src/text/tests/vertical_alternates_test.rs
const VERT_FEATURE_FONT: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fonts/vert-feature-test.ttf"));
========
const VERT_FEATURE_FONT: &[u8] = include_bytes!(concat!(
  env!("CARGO_MANIFEST_DIR"),
  "/tests/fixtures/fonts/vert-feature-test.ttf"
));
>>>>>>>> 69c448c8 (test(text): move text/font regression suites into lib tests):src/text/tests/text/vertical_alternates_test.rs

fn feature_enabled(run: &crate::text::pipeline::ShapedRun, tag: &[u8; 4]) -> bool {
  let tag = Tag::from_bytes(tag);
  run
    .features
    .iter()
    .any(|feature| feature.tag == tag && feature.value == 1)
}

#[test]
fn vertical_shaping_applies_opentype_vertical_alternates() {
  let mut db = FontDatabase::empty();
  db.load_font_data(VERT_FEATURE_FONT.to_vec())
    .expect("fixture font should load");
  db.refresh_generic_fallbacks();
  let ctx = FontContext::with_database(Arc::new(db));

  let pipeline = ShapingPipeline::new();

  let mut base_style = ComputedStyle::default();
  base_style.font_family = vec!["Vert Feature Test".to_string()].into();
  base_style.font_size = 20.0;

  let horizontal_runs = pipeline
    .shape("A", &base_style, &ctx)
    .expect("shape horizontal text");
  assert_eq!(horizontal_runs.len(), 1, "expected a single run for 'A'");
  let horizontal_run = &horizontal_runs[0];
  assert_eq!(
    horizontal_run.font.family, "Vert Feature Test",
    "expected fixture font to be selected"
  );
  assert_eq!(
    horizontal_run.glyphs.len(),
    1,
    "expected horizontal shaping to produce one glyph"
  );
  let horizontal_glyph_id = horizontal_run.glyphs[0].glyph_id;
  assert_eq!(
    horizontal_glyph_id, 1,
    "expected horizontal shaping to use base glyph id 1 (glyph order is stable)"
  );

  let mut vertical_style = base_style;
  vertical_style.writing_mode = WritingMode::VerticalRl;
  vertical_style.text_orientation = TextOrientation::Upright;

  let vertical_runs = pipeline
    .shape("A", &vertical_style, &ctx)
    .expect("shape vertical text");
  assert_eq!(
    vertical_runs.len(),
    1,
    "expected a single run for vertical 'A'"
  );
  let vertical_run = &vertical_runs[0];
  assert_eq!(
    vertical_run.glyphs.len(),
    1,
    "expected vertical shaping to produce one glyph"
  );
  let vertical_glyph_id = vertical_run.glyphs[0].glyph_id;

  assert_ne!(
    vertical_glyph_id, horizontal_glyph_id,
    "vertical shaping should apply a GSUB substitution for `vert`/`vrt2`"
  );
  assert_eq!(
    vertical_glyph_id, 2,
    "expected vertical shaping to substitute to A.vert (glyph id 2 in fixture glyph order)"
  );

  assert!(
    feature_enabled(vertical_run, b"vert"),
    "vertical shaping should enable the `vert` feature"
  );
  assert!(
    feature_enabled(vertical_run, b"vrt2"),
    "vertical shaping should enable the `vrt2` feature"
  );
}
