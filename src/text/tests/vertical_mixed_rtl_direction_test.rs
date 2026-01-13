use crate::style::types::TextOrientation;
use crate::style::types::WritingMode;
use crate::text::pipeline::Direction;
use crate::text::pipeline::RunRotation;
use crate::text::pipeline::ShapingPipeline;
use crate::ComputedStyle;
use crate::FontConfig;
use crate::FontContext;

fn bundled_font_context() -> FontContext {
  FontContext::with_config(FontConfig::bundled_only())
}

#[test]
fn vertical_mixed_rtl_direction_preserves_direction_and_text_order() {
  let pipeline = ShapingPipeline::new();
  let font_ctx = bundled_font_context();

  let mut style = ComputedStyle::default();
  style.writing_mode = WritingMode::VerticalRl;
  style.text_orientation = TextOrientation::Mixed;

  // Hebrew letters + punctuation. "。" is upright under `text-orientation:mixed` and forces a
  // run split so we can regression-test that the split segments keep RTL direction.
  let text = "אב。ג!";
  let runs = pipeline
    .shape_with_direction(text, &style, &font_ctx, Direction::RightToLeft)
    .expect("shape vertical mixed RTL text");

  assert!(
    !runs.is_empty(),
    "expected shaping pipeline to produce at least one run"
  );

  assert!(
    runs
      .iter()
      .any(|run| run.direction == Direction::RightToLeft),
    "expected at least one shaped run to keep RTL direction under vertical mixed orientation"
  );

  let reconstructed: String = runs.iter().flat_map(|run| run.text.chars()).collect();
  assert_eq!(
    reconstructed, text,
    "vertical mixed RTL shaping must preserve source text order"
  );

  let mut saw_rotated = false;
  for run in &runs {
    if run.rotation != RunRotation::None {
      saw_rotated = true;
      assert_eq!(
        run.direction,
        Direction::RightToLeft,
        "rotated mixed-orientation segments must still shape RTL"
      );
    }
  }
  assert!(
    saw_rotated,
    "expected at least one rotated segment for {text:?} under vertical mixed orientation"
  );
}

