//! Bidirectional text analysis using the shaping pipeline.

use crate::style::ComputedStyle;
use crate::style::types::UnicodeBidi;
use crate::text::pipeline::BidiAnalysis;
use crate::text::pipeline::BidiRun;
use crate::text::pipeline::Direction;
use crate::text::pipeline::ShapingPipeline;
use crate::text::pipeline::{debug_bidi_info_calls, debug_reset_bidi_info_calls};
use crate::FontConfig;
use crate::FontContext;
use std::sync::Arc;

fn analyze(text: &str, base: Direction) -> BidiAnalysis {
  let style = ComputedStyle::default();
  BidiAnalysis::analyze_with_base(text, &style, base, None)
}

fn run_texts<'a>(runs: &[BidiRun], text: &'a str) -> Vec<&'a str> {
  runs.iter().map(|r| r.text_slice(text)).collect()
}

#[test]
fn pure_ltr_has_single_run() {
  debug_reset_bidi_info_calls();
  let analysis = analyze("Hello world", Direction::LeftToRight);
  assert_eq!(
    debug_bidi_info_calls(),
    0,
    "ASCII-only LTR text should not invoke the full Unicode bidi algorithm"
  );
  assert!(!analysis.needs_reordering());

  let runs = analysis.logical_runs();
  assert_eq!(runs.len(), 1);
  assert_eq!(runs[0].direction, Direction::LeftToRight);
  assert_eq!(runs[0].text_slice(analysis.text()), "Hello world");
}

#[test]
fn pure_rtl_uses_slow_path() {
  debug_reset_bidi_info_calls();
  let analysis = analyze("שלום", Direction::LeftToRight);
  assert!(
    debug_bidi_info_calls() > 0,
    "RTL text requires the Unicode bidi algorithm"
  );
  assert!(analysis.needs_reordering());
}

#[test]
fn mixed_text_produces_rtl_and_ltr_runs() {
  debug_reset_bidi_info_calls();
  let analysis = analyze("Hello שלום world", Direction::LeftToRight);
  assert!(
    debug_bidi_info_calls() > 0,
    "mixed-direction text should require the Unicode bidi algorithm"
  );
  assert!(analysis.needs_reordering());

  let runs = analysis.logical_runs();
  assert!(runs.iter().any(|r| r.direction == Direction::RightToLeft));
  assert!(runs.iter().any(|r| r.direction == Direction::LeftToRight));
}

#[test]
fn visual_runs_reorder_mixed_content() {
  debug_reset_bidi_info_calls();
  let analysis = analyze("ABC שלום GHI", Direction::LeftToRight);
  assert!(debug_bidi_info_calls() > 0);
  let runs = analysis.visual_runs();

  assert!(runs.len() >= 3);
  let ordered: String = run_texts(&runs, analysis.text()).join("");
  assert!(ordered.contains("ABC"));
  assert!(ordered.contains("שלום"));
  assert!(ordered.contains("GHI"));
  assert!(runs.iter().any(|r| r.direction == Direction::RightToLeft));
}

#[test]
fn paragraph_boundaries_split_runs() {
  debug_reset_bidi_info_calls();
  let text = "\u{202E}ABC\u{202C}\n\u{202A}DEF\u{202C}";
  let analysis = analyze(text, Direction::LeftToRight);
  assert!(debug_bidi_info_calls() > 0);
  let runs = analysis.visual_runs();

  // Runs should stay within their paragraphs.
  assert!(runs.iter().any(|r| r.text_slice(text).contains("ABC")));
  assert!(runs.iter().any(|r| r.text_slice(text).contains("DEF")));
  assert!(runs
    .iter()
    .all(|r| !(r.text_slice(text).contains("ABC") && r.text_slice(text).contains("DEF"))));
}

#[test]
fn inline_bidi_runs_position_in_visual_order() {
  let pipeline = ShapingPipeline::new();
  let font_context = FontContext::with_config(FontConfig::bundled_only());
  let style = ComputedStyle::default();
  let text = "Hello שלום world";
  let analysis = analyze(text, Direction::LeftToRight);
  let runs = analysis.visual_runs();
  let mut cursor = 0.0f32;

  for run in runs {
    let shaped = pipeline
      .shape(run.text_slice(text), &style, &font_context)
      .expect("shape run");

    for shaped_run in shaped {
      for glyph in shaped_run.glyphs.iter() {
        assert!(glyph.x_offset + cursor >= 0.0);
      }
      cursor += shaped_run.advance;
    }
  }
}

#[test]
fn rtl_measure_width_is_physical_and_advances_are_sane() {
  let pipeline = ShapingPipeline::new();
  let font_context = FontContext::with_config(FontConfig::bundled_only());
  let style = ComputedStyle::default();

  let text = "שלום";

  let runs = pipeline
    .shape(text, &style, &font_context)
    .expect("shape RTL text");
  assert!(!runs.is_empty(), "expected shaped runs for RTL text");

  for run in &runs {
    assert!(
      run.advance.is_finite(),
      "run advance should be finite, got {}",
      run.advance
    );
  }

  let sum_advances: f32 = runs.iter().map(|r| r.advance).sum();
  let physical_sum_advances: f32 = runs.iter().map(|r| r.advance.abs()).sum();

  let measured = pipeline
    .measure_width(text, &style, &font_context)
    .expect("measure RTL width");
  assert!(
    measured.is_finite() && measured > 0.0,
    "expected a positive measured width for RTL text, got {}",
    measured
  );
  assert!(
    (measured - physical_sum_advances).abs() < 0.01,
    "measure_width should return physical width; expected {}, got {} (logical sum={})",
    physical_sum_advances,
    measured,
    sum_advances
  );

  let item = crate::layout::contexts::inline::line_builder::TextItem::new(
    runs,
    text.to_string(),
    crate::layout::contexts::inline::baseline::BaselineMetrics::new(0.0, 0.0, 0.0, 0.0),
    Vec::new(),
    Vec::new(),
    Arc::new(style.clone()),
    style.direction,
  );

  assert!(
    (item.advance - sum_advances).abs() < 0.01,
    "TextItem advance should equal the cumulative cluster advance; expected {}, got {}",
    sum_advances,
    item.advance
  );
}

#[test]
fn unicode_bidi_override_skips_bidi_info_new() {
  debug_reset_bidi_info_calls();
  let mut style = ComputedStyle::default();
  style.direction = crate::style::types::Direction::Rtl;
  style.unicode_bidi = UnicodeBidi::BidiOverride;

  let analysis = BidiAnalysis::analyze("abc", &style);
  assert_eq!(
    debug_bidi_info_calls(),
    0,
    "unicode-bidi:bidi-override should not invoke BidiInfo::new"
  );
  assert!(!analysis.needs_reordering());
}
