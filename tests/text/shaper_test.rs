//! Shaping pipeline integration tests (legacy shaper coverage replacement).

use fastrender::style::ComputedStyle;
use fastrender::text::pipeline::{ClusterMap, Direction, ShapedRun, ShapingPipeline};
use fastrender::FontConfig;
use fastrender::FontContext;

fn shape(text: &str) -> Vec<ShapedRun> {
  let pipeline = ShapingPipeline::new();
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let style = ComputedStyle::default();
  pipeline.shape(text, &style, &font_ctx).expect("shape text")
}

fn shape_single_run(text: &str, style: &ComputedStyle, font_ctx: &FontContext) -> ShapedRun {
  let pipeline = ShapingPipeline::new();
  let runs = pipeline.shape(text, style, font_ctx).expect("shape text");
  assert_eq!(
    runs.len(),
    1,
    "expected a single shaped run for {text:?}, got {}",
    runs.len()
  );
  runs.into_iter().next().expect("shaping returned a run")
}

fn bundled_sans_style() -> ComputedStyle {
  let mut style = ComputedStyle::default();
  style.font_family = vec!["sans-serif".to_string()].into();
  style.font_size = 32.0;
  style
}

fn glyph_signature(run: &ShapedRun) -> Vec<(u32, u32)> {
  run.glyphs.iter().map(|g| (g.glyph_id, g.cluster)).collect()
}

fn assert_clusters_in_bounds(run: &ShapedRun) {
  for glyph in &run.glyphs {
    let cluster = glyph.cluster as usize;
    assert!(
      cluster <= run.text.len(),
      "glyph cluster {cluster} out of bounds for text len {} in {:?}",
      run.text.len(),
      run.text
    );
    assert!(
      run.text.is_char_boundary(cluster),
      "glyph cluster {cluster} is not a UTF-8 boundary in {:?}",
      run.text
    );
  }
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

#[test]
fn bidi_format_chars_do_not_break_fi_ligature() {
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let style = bundled_sans_style();

  let baseline = shape_single_run("fi", &style, &font_ctx);
  let baseline_family = baseline.font.family.clone();
  assert_eq!(
    baseline.glyphs.len(),
    1,
    "expected bundled sans-serif fallback ({}) to form the default \"fi\" ligature",
    baseline_family
  );
  assert_ne!(baseline.glyphs[0].glyph_id, 0, "ligature glyph must not be .notdef");

  let baseline_sig = glyph_signature(&baseline);

  for (label, text) in [
    ("LRM", "f\u{200E}i"),
    ("RLM", "f\u{200F}i"),
    ("ALM", "f\u{061C}i"),
    // LRI begins an isolate sequence; PDI terminates it. Both are default-ignorable.
    ("LRI/PDI", "f\u{2066}i\u{2069}"),
  ] {
    let shaped = shape_single_run(text, &style, &font_ctx);
    assert_eq!(
      shaped.font.family, baseline_family,
      "{label}: shaping should stay on the same bundled font"
    );
    assert_eq!(
      glyph_signature(&shaped),
      baseline_sig,
      "{label}: bidi format characters must not disrupt adjacency-sensitive shaping"
    );
    assert_clusters_in_bounds(&shaped);
  }
}

#[test]
fn bidi_format_chars_remap_clusters_to_original_text() {
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let style = bundled_sans_style();

  let baseline = shape_single_run("ab", &style, &font_ctx);
  let baseline_family = baseline.font.family.clone();
  assert_eq!(baseline.glyphs.len(), 2, "expected one glyph per character for \"ab\"");
  let baseline_ids: Vec<u32> = baseline.glyphs.iter().map(|g| g.glyph_id).collect();

  for (label, text) in [
    ("LRM", "a\u{200E}b"),
    ("RLM", "a\u{200F}b"),
    ("ALM", "a\u{061C}b"),
    ("LRI/PDI", "a\u{2066}b\u{2069}"),
  ] {
    let shaped = shape_single_run(text, &style, &font_ctx);
    assert_eq!(
      shaped.font.family, baseline_family,
      "{label}: shaping should stay on the same bundled font"
    );
    assert_eq!(
      shaped.glyphs.len(),
      2,
      "{label}: bidi format characters should not create extra glyphs"
    );
    let shaped_ids: Vec<u32> = shaped.glyphs.iter().map(|g| g.glyph_id).collect();
    assert_eq!(
      shaped_ids, baseline_ids,
      "{label}: glyph ids should match baseline when controls are ignored"
    );

    let b_byte_idx = text
      .char_indices()
      .find(|(_, ch)| *ch == 'b')
      .map(|(idx, _)| idx)
      .expect("test string must contain 'b'");
    assert_eq!(
      shaped.glyphs[1].cluster as usize, b_byte_idx,
      "{label}: cluster for 'b' must map back to its byte offset in the original text"
    );
    assert_clusters_in_bounds(&shaped);

    let map = ClusterMap::from_shaped_run(&shaped);
    let b_char_idx = text
      .chars()
      .position(|ch| ch == 'b')
      .expect("test string must contain 'b'");
    assert_eq!(
      map.glyph_for_char(b_char_idx),
      Some(1),
      "{label}: ClusterMap should map 'b' to the second glyph"
    );
    assert_eq!(
      map.char_for_glyph(1),
      Some(b_char_idx),
      "{label}: ClusterMap should map the second glyph back to 'b'"
    );
  }
}
