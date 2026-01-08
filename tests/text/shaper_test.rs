//! Shaping pipeline integration tests (legacy shaper coverage replacement).

use fastrender::style::ComputedStyle;
use fastrender::text::pipeline::{Direction, ShapedRun, ShapingPipeline};
use fastrender::FontConfig;
use fastrender::FontContext;

fn shape(text: &str) -> Vec<ShapedRun> {
  let pipeline = ShapingPipeline::new();
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let style = ComputedStyle::default();
  pipeline.shape(text, &style, &font_ctx).expect("shape text")
}

#[test]
fn shaping_empty_string_returns_no_runs() {
  let runs = shape("");
  assert!(runs.is_empty());
}

#[test]
fn shaping_basic_latin_text_produces_glyphs() {
  let runs = shape("Hello");
  assert!(!runs.is_empty());
  assert_eq!(runs[0].text, "Hello");
  assert!(runs[0].glyphs.len() >= 1);
  assert!(runs[0].advance > 0.0);
  assert_eq!(runs[0].direction, Direction::LeftToRight);
}

#[test]
fn shaping_rtl_text_sets_direction() {
  let runs = shape("שלום");
  assert!(!runs.is_empty());
  assert_eq!(runs[0].direction, Direction::RightToLeft);
  assert!(runs[0].advance > 0.0);
}
