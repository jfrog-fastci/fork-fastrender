use std::sync::Arc;

use fastrender::style::types::FontVariantPosition;
use fastrender::text::font_db::FontDatabase;
use fastrender::text::font_loader::FontContext;
use fastrender::text::pipeline::{assign_fonts, Direction, ItemizedRun, Script};
use fastrender::ComputedStyle;
use rustybuzz::{Feature, Face as HbFace, UnicodeBuffer};
use ttf_parser::Tag;

const NOTO_SANS_FONT: &[u8] = include_bytes!("../fixtures/fonts/NotoSans-subset.ttf");

fn feature_affects_char(face: &HbFace<'_>, tag: [u8; 4], ch: char) -> bool {
  let mut utf8 = [0u8; 4];
  let encoded = ch.encode_utf8(&mut utf8);

  let mut base_buf = UnicodeBuffer::new();
  base_buf.push_str(encoded);
  base_buf.set_direction(rustybuzz::Direction::LeftToRight);
  let base = rustybuzz::shape(face, &[], base_buf);

  let mut feature_buf = UnicodeBuffer::new();
  feature_buf.push_str(encoded);
  feature_buf.set_direction(rustybuzz::Direction::LeftToRight);
  let feature = Feature {
    tag: Tag::from_bytes(&tag),
    value: 1,
    start: 0,
    end: u32::MAX,
  };
  let shaped = rustybuzz::shape(face, &[feature], feature_buf);

  let base_infos = base.glyph_infos();
  let feature_infos = shaped.glyph_infos();
  let base_positions = base.glyph_positions();
  let feature_positions = shaped.glyph_positions();

  if base_infos.len() != feature_infos.len() || base_positions.len() != feature_positions.len() {
    return true;
  }

  base_infos
    .iter()
    .zip(feature_infos)
    .any(|(base, feature)| base.glyph_id != feature.glyph_id)
    || base_positions.iter().zip(feature_positions).any(|(base, feature)| {
      base.x_advance != feature.x_advance
        || base.y_advance != feature.y_advance
        || base.x_offset != feature.x_offset
        || base.y_offset != feature.y_offset
    })
}

fn feature_value(run: &fastrender::text::pipeline::FontRun, tag: [u8; 4]) -> Option<u32> {
  run
    .features
    .iter()
    .find(|feature| feature.tag == Tag::from_bytes(&tag))
    .map(|feature| feature.value)
}

#[test]
fn font_variant_position_synthesis_probes_run_text() {
  let hb_face = HbFace::from_slice(NOTO_SANS_FONT, 0).expect("fixture font should parse");

  let digit = ('0'..='9')
    .find(|ch| feature_affects_char(&hb_face, *b"sups", *ch))
    .expect("fixture font should contain at least one superscript digit");
  assert!(
    !feature_affects_char(&hb_face, *b"sups", 'x'),
    "expected fixture font superscript feature to exclude 'x'"
  );

  let mut db = FontDatabase::empty();
  db.load_font_data(NOTO_SANS_FONT.to_vec())
    .expect("fixture font should load");
  db.refresh_generic_fallbacks();
  let ctx = FontContext::with_database(Arc::new(db));

  let mut style = ComputedStyle::default();
  style.font_family = vec!["Noto Sans".to_string()].into();
  style.font_size = 20.0;
  style.font_variant_position = FontVariantPosition::Super;
  style.font_synthesis.position = true;

  // The legacy font-variant-position synthesis probe used a hard-coded sample "x", which would
  // incorrectly decide that superscripts are unavailable for digit-only runs when the feature
  // exists for digits but not for letters.
  let preflight_runs = assign_fonts(
    &[ItemizedRun {
      start: 0,
      end: digit.len_utf8(),
      text: digit.to_string(),
      script: Script::Latin,
      direction: Direction::LeftToRight,
      level: 0,
    }],
    &style,
    &ctx,
  )
  .expect("assign fonts for digit");
  assert_eq!(preflight_runs.len(), 1);

  let digit_run = &preflight_runs[0];
  assert!(
    (digit_run.font_size - style.font_size).abs() < 0.05,
    "digit-only run should keep base font size when superscript glyphs exist"
  );
  assert!(
    digit_run.baseline_shift.abs() < 0.01,
    "digit-only run should not apply synthetic baseline shift when superscript glyphs exist"
  );
  assert_eq!(feature_value(digit_run, *b"sups"), Some(1));

  let mixed_text = format!("x{digit}");
  let mixed_runs = assign_fonts(
    &[ItemizedRun {
      start: 0,
      end: mixed_text.len(),
      text: mixed_text.clone(),
      script: Script::Latin,
      direction: Direction::LeftToRight,
      level: 0,
    }],
    &style,
    &ctx,
  )
  .expect("assign fonts for mixed run");
  assert_eq!(mixed_runs.len(), 1);
  let mixed_run = &mixed_runs[0];
  assert!(
    mixed_run.font_size < style.font_size * 0.95,
    "run containing unsupported characters should synthesize superscript scaling"
  );
  assert!(
    mixed_run.baseline_shift > 0.01,
    "synthetic superscript should raise the baseline"
  );
  assert_eq!(
    feature_value(mixed_run, *b"sups"),
    Some(0),
    "synthesized superscripts should disable partial OpenType superscript features"
  );
}
