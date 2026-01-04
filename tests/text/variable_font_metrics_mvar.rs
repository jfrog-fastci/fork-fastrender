use fastrender::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
use fastrender::text::font_loader::FontContext;
use rustybuzz::Variation;
use std::sync::Arc;
use ttf_parser::Tag;

const VAR_FONT: &[u8] = include_bytes!("../fixtures/fonts/mvar-metrics-test.ttf");

fn loaded_font() -> LoadedFont {
  LoadedFont {
    id: None,
    data: Arc::new(VAR_FONT.to_vec()),
    index: 0,
    family: "MVAR Metrics Test".to_string(),
    weight: FontWeight::NORMAL,
    style: FontStyle::Normal,
    stretch: FontStretch::Normal,
    face_metrics_overrides: Default::default(),
  }
}

#[test]
fn variable_font_metrics_apply_mvar_variations() {
  let font = loaded_font();
  let ctx = FontContext::empty();

  let font_size = 48.0;
  let light = vec![Variation {
    tag: Tag::from_bytes(b"wght"),
    value: 100.0,
  }];
  let heavy = vec![Variation {
    tag: Tag::from_bytes(b"wght"),
    value: 900.0,
  }];

  let a = ctx
    .get_scaled_metrics_with_variations(&font, font_size, &light)
    .expect("scaled metrics for wght=100");
  let b = ctx
    .get_scaled_metrics_with_variations(&font, font_size, &heavy)
    .expect("scaled metrics for wght=900");

  let coords_a: Vec<_> = light.iter().map(|v| (v.tag, v.value)).collect();
  let coords_b: Vec<_> = heavy.iter().map(|v| (v.tag, v.value)).collect();
  let raw_a = font
    .metrics_with_variations(&coords_a)
    .expect("raw metrics for first coordinate set");
  let raw_b = font
    .metrics_with_variations(&coords_b)
    .expect("raw metrics for second coordinate set");

  // The fixture font encodes MVAR deltas for typographic metrics (ascender/descender/line-gap) and
  // underline metrics. Both scaled and raw values should reflect those deltas when `wght` changes.
  assert_eq!(raw_a.ascent, 720, "raw A mismatch: {raw_a:?}");
  assert_eq!(raw_a.descent, -180, "raw A mismatch: {raw_a:?}");
  assert_eq!(raw_a.line_gap, 0, "raw A mismatch: {raw_a:?}");
  assert_eq!(raw_a.line_height, 900, "raw A mismatch: {raw_a:?}");
  assert_eq!(raw_a.x_height, Some(460), "raw A mismatch: {raw_a:?}");
  assert_eq!(raw_a.cap_height, Some(640), "raw A mismatch: {raw_a:?}");
  assert_eq!(raw_a.underline_position, -60, "raw A mismatch: {raw_a:?}");
  assert_eq!(raw_a.underline_thickness, 30, "raw A mismatch: {raw_a:?}");

  assert_eq!(raw_b.ascent, 920, "raw B mismatch: {raw_b:?}");
  assert_eq!(raw_b.descent, -260, "raw B mismatch: {raw_b:?}");
  assert_eq!(raw_b.line_gap, 300, "raw B mismatch: {raw_b:?}");
  assert_eq!(raw_b.line_height, 1480, "raw B mismatch: {raw_b:?}");
  assert_eq!(raw_b.x_height, Some(540), "raw B mismatch: {raw_b:?}");
  assert_eq!(raw_b.cap_height, Some(760), "raw B mismatch: {raw_b:?}");
  assert_eq!(raw_b.underline_position, -140, "raw B mismatch: {raw_b:?}");
  assert_eq!(raw_b.underline_thickness, 70, "raw B mismatch: {raw_b:?}");

  let expect = |label: &str, actual: f32, expected: f32| {
    assert!(
      (actual - expected).abs() < 0.05,
      "{label}: expected {expected}, got {actual}\nA={a:?}\nB={b:?}\nraw A={raw_a:?}\nraw B={raw_b:?}"
    );
  };

  expect("scaled A line_height", a.line_height, 43.2);
  expect("scaled B line_height", b.line_height, 71.04);
  expect("scaled A underline_thickness", a.underline_thickness, 1.44);
  expect("scaled B underline_thickness", b.underline_thickness, 3.36);

  assert!(
    b.line_height > a.line_height,
    "expected heavier instance to have a larger line height.\nA={a:?}\nB={b:?}\nraw A={raw_a:?}\nraw B={raw_b:?}"
  );
}
