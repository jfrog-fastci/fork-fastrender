//! Integration tests for the text shaping pipeline.
//!
//! These tests verify the complete text shaping pipeline including:
//! - Bidi analysis
//! - Script itemization
//! - Font matching
//! - Text shaping
//! - Mixed script handling
//!
//! Note: Tests prefer bundled fixture fonts for deterministic results.

use fastrender::style::types::TextOrientation;
use fastrender::style::types::UnicodeBidi;
use fastrender::style::types::WritingMode;
use fastrender::text::pipeline::atomic_shaping_clusters;
use fastrender::text::pipeline::itemize_text;
use fastrender::text::pipeline::BidiAnalysis;
use fastrender::text::pipeline::Direction;
use fastrender::text::pipeline::ExplicitBidiContext;
use fastrender::text::pipeline::ItemizedRun;
use fastrender::text::pipeline::RunRotation;
use fastrender::text::pipeline::Script;
use fastrender::text::pipeline::ShapingPipeline;
use fastrender::ComputedStyle;
use fastrender::FontConfig;
use fastrender::FontContext;
use unicode_bidi::Level;

fn bundled_font_context() -> FontContext {
  FontContext::with_config(FontConfig::bundled_only())
}

/// Helper macro to unwrap shaping results from the bundled font context.
macro_rules! require_fonts {
  ($result:expr) => {
    match $result {
      Ok(v) => v,
      Err(e) => panic!("Unexpected shaping error: {e}"),
    }
  };
}

// ============================================================================
// Direction Tests
// ============================================================================

#[test]
fn test_direction_default() {
  assert_eq!(Direction::default(), Direction::LeftToRight);
}

#[test]
fn test_direction_is_ltr() {
  assert!(Direction::LeftToRight.is_ltr());
  assert!(!Direction::RightToLeft.is_ltr());
}

#[test]
fn test_direction_is_rtl() {
  assert!(Direction::RightToLeft.is_rtl());
  assert!(!Direction::LeftToRight.is_rtl());
}

// ============================================================================
// Writing Mode Tests
// ============================================================================

#[test]
fn sideways_writing_mode_shapes_with_rotation() {
  let pipeline = ShapingPipeline::new();
  let font_ctx = bundled_font_context();
  let mut style = ComputedStyle::default();
  style.writing_mode = WritingMode::SidewaysLr;

  let runs =
    require_fonts!(pipeline.shape_with_direction("abc", &style, &font_ctx, Direction::LeftToRight));
  assert!(!runs.is_empty());
  for run in runs {
    assert_eq!(run.rotation, RunRotation::Cw90);
  }
}

#[test]
fn vertical_mixed_orientation_splits_runs() {
  let _guard = super::text_diagnostics_guard();
  let pipeline = ShapingPipeline::new();
  let font_ctx = bundled_font_context();
  let mut style = ComputedStyle::default();
  style.writing_mode = WritingMode::VerticalRl;
  style.text_orientation = TextOrientation::Mixed;

  let runs =
    require_fonts!(pipeline.shape_with_direction("漢A", &style, &font_ctx, Direction::LeftToRight));
  assert!(
    runs.len() >= 2,
    "expected separate runs for upright CJK and rotated Latin"
  );

  let mut saw_upright = false;
  let mut saw_rotated = false;
  for run in runs {
    if run.text.contains('漢') {
      assert_eq!(run.rotation, RunRotation::None);
      saw_upright = true;
    }
    if run.text.contains('A') {
      assert_eq!(run.rotation, RunRotation::Cw90);
      saw_rotated = true;
    }
  }

  assert!(
    saw_upright && saw_rotated,
    "mixed orientation should produce upright and rotated runs"
  );
}

#[test]
fn vertical_sideways_orientation_rotates_all() {
  let pipeline = ShapingPipeline::new();
  let font_ctx = bundled_font_context();
  let mut style = ComputedStyle::default();
  style.writing_mode = WritingMode::VerticalRl;
  style.text_orientation = TextOrientation::Sideways;

  let runs =
    require_fonts!(pipeline.shape_with_direction("AB", &style, &font_ctx, Direction::LeftToRight));
  assert!(!runs.is_empty());
  for run in runs {
    assert_eq!(run.rotation, RunRotation::Cw90);
  }
}

#[test]
fn vertical_sideways_left_orientation_rotates_counter_clockwise() {
  let pipeline = ShapingPipeline::new();
  let font_ctx = bundled_font_context();
  let mut style = ComputedStyle::default();
  style.writing_mode = WritingMode::VerticalRl;
  style.text_orientation = TextOrientation::SidewaysLeft;

  let runs =
    require_fonts!(pipeline.shape_with_direction("AB", &style, &font_ctx, Direction::LeftToRight));
  assert!(!runs.is_empty());
  for run in runs {
    assert_eq!(run.rotation, RunRotation::Ccw90);
  }
}

#[test]
fn vertical_shaping_uses_vertical_advances() {
  let _guard = super::text_diagnostics_guard();
  let pipeline = ShapingPipeline::new();
  let font_ctx = bundled_font_context();

  let mut vertical_style = ComputedStyle::default();
  vertical_style.writing_mode = WritingMode::VerticalRl;
  vertical_style.text_orientation = TextOrientation::Upright;

  let vertical_runs = require_fonts!(pipeline.shape("日本語", &vertical_style, &font_ctx));
  let horizontal_runs = require_fonts!(pipeline.shape("日本語", &ComputedStyle::default(), &font_ctx));

  let vertical_y_advance: f32 = vertical_runs
    .iter()
    .flat_map(|r| r.glyphs.iter())
    .map(|g| g.y_advance.abs())
    .sum();
  let horizontal_y_advance: f32 = horizontal_runs
    .iter()
    .flat_map(|r| r.glyphs.iter())
    .map(|g| g.y_advance.abs())
    .sum();

  assert!(
    vertical_y_advance > horizontal_y_advance + 0.1,
    "vertical shaping should expose vertical advances for inline progression"
  );
}

#[test]
fn last_resort_font_fallback_shapes_missing_scripts() {
  let _guard = super::text_diagnostics_guard();
  let pipeline = ShapingPipeline::new();
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let style = ComputedStyle::default();

  // Pick a codepoint that none of the bundled fonts cover so the shaping pipeline
  // is forced into last-resort font selection and emits `.notdef` glyphs.
  let db = font_ctx.database();
  let missing = [
    '\u{10380}',  // UGARITIC LETTER ALPA
    '\u{103A0}',  // OLD PERSIAN SIGN A
    '\u{10400}',  // DESERET CAPITAL LETTER LONG I
    '\u{16A0}',   // RUNIC LETTER FEHU FEOH FE F
    '\u{10FFFF}', // highest Unicode scalar value (noncharacter)
  ]
  .into_iter()
  .find(|ch| !db.faces().any(|face| db.has_glyph_cached(face.id, *ch)))
  .expect("expected at least one missing codepoint");

  let text = format!("hello {missing}");
  let runs = pipeline
    .shape(&text, &style, &font_ctx)
    .expect("shaping should succeed with last-resort fallback");
  assert!(
    !runs.is_empty(),
    "missing coverage should still yield shaped runs"
  );
  let glyphs: usize = runs.iter().map(|run| run.glyphs.len()).sum();
  assert!(glyphs > 0, "fallback shaping should emit glyphs");
  assert!(
    runs
      .iter()
      .any(|run| run.glyphs.iter().any(|glyph| glyph.glyph_id == 0)),
    "fallback glyphs should use .notdef when coverage is missing"
  );
}

#[test]
fn bundled_only_sans_serif_prefers_additional_script_fonts() {
  let pipeline = ShapingPipeline::new();
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let mut style = ComputedStyle::default();
  style.font_family = vec!["sans-serif".to_string()].into();
  style.font_size = 32.0;

  for (label, sample, expected_family) in [
    ("Gurmukhi", 'ਗ', "Noto Sans Gurmukhi"),
    ("Gujarati", 'ગ', "Noto Sans Gujarati"),
    ("Oriya", 'ଓ', "Noto Sans Oriya"),
    ("Kannada", 'ಕ', "Noto Sans Kannada"),
    ("Malayalam", 'മ', "Noto Sans Malayalam"),
    ("Sinhala", 'ස', "Noto Sans Sinhala"),
    ("Armenian", 'Ա', "Noto Sans Armenian"),
    ("Georgian", 'ა', "Noto Sans Georgian"),
    ("Ethiopic", 'አ', "Noto Sans Ethiopic"),
    ("Lao", 'ກ', "Noto Sans Lao"),
    ("Tibetan", 'ཀ', "Noto Serif Tibetan"),
    ("Khmer", 'ក', "Noto Sans Khmer"),
    ("Cherokee", 'Ꭰ', "Noto Sans Cherokee"),
    ("CanadianAboriginal", 'ᐁ', "Noto Sans Canadian Aboriginal"),
    ("TaiLe", 'ᥐ', "Noto Sans Tai Le"),
    ("OlChiki", 'ᱚ', "Noto Sans Ol Chiki"),
    ("Glagolitic", 'Ⰰ', "Noto Sans Glagolitic"),
    ("Tifinagh", 'ⴰ', "Noto Sans Tifinagh"),
    ("SylotiNagri", 'ꠅ', "Noto Sans Syloti Nagri"),
    ("MeeteiMayek", 'ꯀ', "Noto Sans Meetei Mayek"),
    ("Gothic", '𐌰', "Noto Sans Gothic"),
  ] {
    let text = sample.to_string();
    let runs = pipeline
      .shape(&text, &style, &font_ctx)
      .unwrap_or_else(|_| panic!("shape bundled-only sample for {label}"));
    let run = runs
      .iter()
      .find(|run| run.text.contains(sample))
      .unwrap_or_else(|| panic!("no shaped run contained {label} sample"));
    assert_eq!(
      run.font.family, expected_family,
      "{label} sample should use bundled {expected_family} fallback font"
    );
    assert!(
      run.glyphs.iter().all(|glyph| glyph.glyph_id != 0),
      "{label} sample should not contain .notdef glyphs"
    );
  }
}

#[test]
fn bundled_only_indic_scripts_shape_conjunct_clusters() {
  let pipeline = ShapingPipeline::new();
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let mut style = ComputedStyle::default();
  style.font_family = vec!["sans-serif".to_string()].into();
  style.font_size = 32.0;

  for (label, text, expected_family) in [
    ("Gujarati", "ક્ષ", "Noto Sans Gujarati"),
    ("Oriya", "କ୍ଷ", "Noto Sans Oriya"),
    ("Kannada", "ಕ್ಷ", "Noto Sans Kannada"),
    ("Malayalam", "ക്ഷ", "Noto Sans Malayalam"),
    ("Sinhala", "ක්ෂ", "Noto Sans Sinhala"),
  ] {
    let runs = pipeline
      .shape(text, &style, &font_ctx)
      .unwrap_or_else(|_| panic!("shape {label} conjunct sample"));
    assert!(
      !runs.is_empty(),
      "{label} conjunct sample should produce runs"
    );

    let run = runs
      .iter()
      .find(|run| run.font.family == expected_family)
      .unwrap_or_else(|| panic!("{label} conjunct sample should use {expected_family}"));
    assert!(
      run.glyphs.iter().all(|glyph| glyph.glyph_id != 0),
      "{label} conjunct sample should not contain .notdef glyphs"
    );

    let char_count = run.text.chars().count();
    assert_ne!(
      run.glyphs.len(),
      char_count,
      "{label} conjunct shaping should not be 1:1 glyphs-per-character (got {} glyphs for {} chars)",
      run.glyphs.len(),
      char_count
    );
  }
}

#[test]
fn bundled_only_gurmukhi_shapes_prebase_matra() {
  let pipeline = ShapingPipeline::new();
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let mut style = ComputedStyle::default();
  style.font_family = vec!["sans-serif".to_string()].into();
  style.font_size = 32.0;

  // GURMUKHI LETTER KA + GURMUKHI VOWEL SIGN I (pre-base matra).
  let base_runs = pipeline
    .shape("ਕ", &style, &font_ctx)
    .expect("shape Gurmukhi base glyph");
  let base_run = base_runs
    .iter()
    .find(|run| run.text.contains('ਕ'))
    .expect("expected run containing base glyph");
  assert_eq!(
    base_run.font.family, "Noto Sans Gurmukhi",
    "Gurmukhi shaping should use Noto Sans Gurmukhi"
  );
  let base_glyph_id = base_run.glyphs[0].glyph_id;

  let runs = pipeline
    .shape("ਕਿ", &style, &font_ctx)
    .expect("shape Gurmukhi matra cluster");
  let run = runs
    .iter()
    .find(|run| run.text.contains('ਕ'))
    .expect("expected run containing matra cluster");
  assert_eq!(
    run.font.family, "Noto Sans Gurmukhi",
    "Gurmukhi matra cluster should use Noto Sans Gurmukhi"
  );
  assert!(
    run.glyphs.iter().all(|glyph| glyph.glyph_id != 0),
    "Gurmukhi matra cluster should not contain .notdef glyphs"
  );

  let base_index = run
    .glyphs
    .iter()
    .position(|glyph| glyph.glyph_id == base_glyph_id)
    .expect("expected matra cluster to contain base glyph");
  assert!(
    base_index > 0,
    "expected pre-base matra shaping to reorder glyphs (base glyph index {base_index})"
  );
}

// ============================================================================
// Cluster Segmentation Tests
// ============================================================================

#[test]
fn atomic_clusters_split_ascii_by_byte() {
  let text = "Hello";
  let clusters = atomic_shaping_clusters(text);
  let expected: Vec<(usize, usize)> = (0..text.len()).map(|idx| (idx, idx + 1)).collect();
  assert_eq!(clusters, expected);
}

#[test]
fn atomic_clusters_split_trivial_unicode_by_scalar() {
  let text = "Hello—“world”";
  let clusters = atomic_shaping_clusters(text);
  let expected: Vec<(usize, usize)> = text
    .char_indices()
    .map(|(idx, ch)| (idx, idx + ch.len_utf8()))
    .collect();
  assert_eq!(clusters, expected);
}

#[test]
fn atomic_clusters_do_not_split_combining_marks() {
  let text = "a\u{0301}";
  let clusters = atomic_shaping_clusters(text);
  assert_eq!(clusters, vec![(0, text.len())]);
}

#[test]
fn atomic_clusters_do_not_split_zwj_sequences() {
  let text = "👨\u{200d}👩";
  let clusters = atomic_shaping_clusters(text);
  assert_eq!(clusters, vec![(0, text.len())]);
}

#[test]
fn atomic_clusters_do_not_split_flag_sequences() {
  let text = "🇺🇸";
  let clusters = atomic_shaping_clusters(text);
  assert_eq!(clusters, vec![(0, text.len())]);
}

#[test]
fn atomic_clusters_do_not_split_emoji_modifier_sequences() {
  let text = "👋🏽";
  let clusters = atomic_shaping_clusters(text);
  assert_eq!(clusters, vec![(0, text.len())]);
}

#[test]
fn atomic_clusters_do_not_split_halfwidth_katakana_voicing_marks() {
  // Halfwidth KA + halfwidth voiced sound mark (ｶﾞ). Unicode grapheme segmentation treats this as a
  // single extended grapheme cluster even though U+FF9E is General_Category=Lm.
  let text = "\u{FF76}\u{FF9E}";
  let clusters = atomic_shaping_clusters(text);
  assert_eq!(clusters, vec![(0, text.len())]);
}

#[test]
fn atomic_clusters_preserve_emoji_variation_sequences() {
  let text = "❤️";
  let clusters = atomic_shaping_clusters(text);
  assert_eq!(clusters, vec![(0, text.len())]);
}

#[test]
fn atomic_clusters_preserve_crlf() {
  let text = "\r\n";
  let clusters = atomic_shaping_clusters(text);
  assert_eq!(clusters, vec![(0, text.len())]);
}

// ============================================================================
// Script Detection Tests
// ============================================================================

#[test]
fn test_script_detect_ascii() {
  // ASCII letters should be Latin
  assert_eq!(Script::detect('A'), Script::Latin);
  assert_eq!(Script::detect('Z'), Script::Latin);
  assert_eq!(Script::detect('a'), Script::Latin);
  assert_eq!(Script::detect('z'), Script::Latin);
}

#[test]
fn test_script_detect_latin_extended() {
  // Extended Latin characters
  assert_eq!(Script::detect('é'), Script::Latin);
  assert_eq!(Script::detect('ñ'), Script::Latin);
  assert_eq!(Script::detect('ü'), Script::Latin);
  assert_eq!(Script::detect('ø'), Script::Latin);
}

#[test]
fn test_script_detect_common() {
  // Numbers and punctuation should be Common
  assert_eq!(Script::detect('0'), Script::Common);
  assert_eq!(Script::detect('9'), Script::Common);
  assert_eq!(Script::detect(' '), Script::Common);
  assert_eq!(Script::detect('.'), Script::Common);
  assert_eq!(Script::detect(','), Script::Common);
  assert_eq!(Script::detect('!'), Script::Common);
}

#[test]
fn test_script_detect_arabic() {
  // Arabic characters
  assert_eq!(Script::detect('م'), Script::Arabic);
  assert_eq!(Script::detect('ر'), Script::Arabic);
  assert_eq!(Script::detect('ح'), Script::Arabic);
  assert_eq!(Script::detect('ب'), Script::Arabic);
  assert_eq!(Script::detect('ا'), Script::Arabic);
}

#[test]
fn test_script_detect_hebrew() {
  // Hebrew characters
  assert_eq!(Script::detect('ש'), Script::Hebrew);
  assert_eq!(Script::detect('ל'), Script::Hebrew);
  assert_eq!(Script::detect('ו'), Script::Hebrew);
  assert_eq!(Script::detect('ם'), Script::Hebrew);
}

#[test]
fn test_script_detect_greek() {
  // Greek characters
  assert_eq!(Script::detect('α'), Script::Greek);
  assert_eq!(Script::detect('β'), Script::Greek);
  assert_eq!(Script::detect('Ω'), Script::Greek);
  assert_eq!(Script::detect('Δ'), Script::Greek);
}

#[test]
fn test_script_detect_cyrillic() {
  // Cyrillic characters
  assert_eq!(Script::detect('А'), Script::Cyrillic);
  assert_eq!(Script::detect('Б'), Script::Cyrillic);
  assert_eq!(Script::detect('я'), Script::Cyrillic);
  assert_eq!(Script::detect('ж'), Script::Cyrillic);
}

#[test]
fn test_script_detect_devanagari() {
  // Devanagari characters (Hindi)
  assert_eq!(Script::detect('न'), Script::Devanagari);
  assert_eq!(Script::detect('म'), Script::Devanagari);
  assert_eq!(Script::detect('स'), Script::Devanagari);
}

#[test]
fn test_script_detect_additional_bundled_scripts() {
  // Indic scripts
  assert_eq!(Script::detect('ਗ'), Script::Gurmukhi);
  assert_eq!(Script::detect('ગ'), Script::Gujarati);
  assert_eq!(Script::detect('ଓ'), Script::Oriya);
  assert_eq!(Script::detect('ಕ'), Script::Kannada);
  assert_eq!(Script::detect('മ'), Script::Malayalam);
  assert_eq!(Script::detect('ස'), Script::Sinhala);

  // Other bundled scripts
  assert_eq!(Script::detect('Ա'), Script::Armenian);
  assert_eq!(Script::detect('ა'), Script::Georgian);
  assert_eq!(Script::detect('አ'), Script::Ethiopic);
  assert_eq!(Script::detect('ກ'), Script::Lao);
  assert_eq!(Script::detect('ཀ'), Script::Tibetan);
  assert_eq!(Script::detect('ក'), Script::Khmer);
  assert_eq!(Script::detect('Ꭰ'), Script::Cherokee);
  assert_eq!(Script::detect('ᐁ'), Script::CanadianAboriginal);
  assert_eq!(Script::detect('ᥐ'), Script::TaiLe);
  assert_eq!(Script::detect('ᱚ'), Script::OlChiki);
  assert_eq!(Script::detect('Ⰰ'), Script::Glagolitic);
  assert_eq!(Script::detect('ⴰ'), Script::Tifinagh);
  assert_eq!(Script::detect('ꠅ'), Script::SylotiNagri);
  assert_eq!(Script::detect('ꯀ'), Script::MeeteiMayek);
  assert_eq!(Script::detect('𐌰'), Script::Gothic);
}

#[test]
fn test_script_detect_cjk() {
  // Chinese characters (Han)
  assert_eq!(Script::detect('中'), Script::Han);
  assert_eq!(Script::detect('国'), Script::Han);
  assert_eq!(Script::detect('人'), Script::Han);

  // Japanese Hiragana
  assert_eq!(Script::detect('あ'), Script::Hiragana);
  assert_eq!(Script::detect('い'), Script::Hiragana);

  // Japanese Katakana
  assert_eq!(Script::detect('ア'), Script::Katakana);
  assert_eq!(Script::detect('イ'), Script::Katakana);

  // Korean Hangul
  assert_eq!(Script::detect('한'), Script::Hangul);
  assert_eq!(Script::detect('글'), Script::Hangul);
}

#[test]
fn test_script_detect_thai() {
  // Thai characters
  assert_eq!(Script::detect('ก'), Script::Thai);
  assert_eq!(Script::detect('ข'), Script::Thai);
}

#[test]
fn test_script_is_neutral() {
  assert!(Script::Common.is_neutral());
  assert!(Script::Inherited.is_neutral());
  assert!(Script::Unknown.is_neutral());
  assert!(!Script::Latin.is_neutral());
  assert!(!Script::Arabic.is_neutral());
  assert!(!Script::Hebrew.is_neutral());
}

#[test]
fn test_script_to_harfbuzz() {
  // Specific scripts should return Some (non-None)
  assert!(Script::Latin.to_harfbuzz().is_some());
  assert!(Script::Arabic.to_harfbuzz().is_some());
  assert!(Script::Hebrew.to_harfbuzz().is_some());
  assert!(Script::Greek.to_harfbuzz().is_some());
  assert!(Script::Cyrillic.to_harfbuzz().is_some());
  assert!(Script::Gujarati.to_harfbuzz().is_some());
  // Common/neutral scripts should return None (auto-detect)
  assert!(Script::Common.to_harfbuzz().is_none());
}

// ============================================================================
// Bidi Analysis Tests
// ============================================================================

#[test]
fn test_bidi_analysis_empty() {
  let style = ComputedStyle::default();
  let bidi = BidiAnalysis::analyze("", &style);

  assert!(!bidi.needs_reordering());
  assert!(bidi.base_direction().is_ltr());
}

#[test]
fn test_bidi_analysis_simple_ltr() {
  let style = ComputedStyle::default();
  let bidi = BidiAnalysis::analyze("Hello, world!", &style);

  assert!(!bidi.needs_reordering());
  assert!(bidi.base_direction().is_ltr());
}

#[test]
fn test_bidi_analysis_simple_rtl() {
  let style = ComputedStyle::default();
  let bidi = BidiAnalysis::analyze("שלום", &style);

  assert!(bidi.needs_reordering());
}

#[test]
fn test_bidi_analysis_mixed_ltr_rtl() {
  let style = ComputedStyle::default();
  let bidi = BidiAnalysis::analyze("Hello שלום World", &style);

  assert!(bidi.needs_reordering());
}

#[test]
fn test_bidi_analysis_arabic_text() {
  let style = ComputedStyle::default();
  let bidi = BidiAnalysis::analyze("مرحبا بالعالم", &style);

  assert!(bidi.needs_reordering());
}

#[test]
fn test_bidi_analysis_direction_at() {
  let style = ComputedStyle::default();
  let text = "Hello שלום";
  let bidi = BidiAnalysis::analyze(text, &style);

  // First character 'H' should be LTR
  assert!(bidi.direction_at(0).is_ltr());
}

#[test]
fn bidi_plaintext_preserves_explicit_context_embedding_level() {
  let mut style = ComputedStyle::default();
  style.unicode_bidi = UnicodeBidi::Plaintext;

  let bidi = BidiAnalysis::analyze_with_base(
    "123",
    &style,
    Direction::RightToLeft,
    Some(ExplicitBidiContext {
      level: Level::new(3).unwrap(),
      override_all: false,
    }),
  );

  assert_eq!(bidi.base_level().number(), 3);
  assert!(bidi.base_direction().is_rtl());
}

#[test]
fn bidi_plaintext_without_explicit_context_uses_first_strong_base_direction() {
  let mut style = ComputedStyle::default();
  style.unicode_bidi = UnicodeBidi::Plaintext;

  let latin = BidiAnalysis::analyze_with_base("abc", &style, Direction::RightToLeft, None);
  assert!(latin.base_direction().is_ltr());

  let hebrew = BidiAnalysis::analyze_with_base("אבג", &style, Direction::LeftToRight, None);
  assert!(hebrew.base_direction().is_rtl());
}

// ============================================================================
// Script Itemization Tests
// ============================================================================

#[test]
fn test_itemize_empty() {
  let style = ComputedStyle::default();
  let bidi = BidiAnalysis::analyze("", &style);
  let runs = itemize_text("", &bidi);

  assert!(runs.is_empty());
}

#[test]
fn test_itemize_single_latin() {
  let style = ComputedStyle::default();
  let text = "Hello";
  let bidi = BidiAnalysis::analyze(text, &style);
  let runs = itemize_text(text, &bidi);

  assert_eq!(runs.len(), 1);
  assert_eq!(runs[0].text, "Hello");
  assert_eq!(runs[0].script, Script::Latin);
  assert!(runs[0].direction.is_ltr());
}

#[test]
fn bidi_override_does_not_cross_paragraph_boundary() {
  let mut style = ComputedStyle::default();
  style.direction = fastrender::style::types::Direction::Rtl;
  style.unicode_bidi = UnicodeBidi::BidiOverride;

  // Two paragraphs separated by a newline. Second paragraph has an embedded RLE.
  let text = "ABC\n\u{202B}DEF"; // ABC\nRLE DEF
  let bidi = BidiAnalysis::analyze(text, &style);

  assert_eq!(bidi.paragraphs().len(), 2);

  // Runs should stay within their paragraph and not reorder across the line break.
  let runs = itemize_text(text, &bidi);
  let directions: Vec<_> = runs.iter().map(|r| r.direction.is_rtl()).collect();
  assert!(
    directions.iter().all(|d| *d),
    "runs should remain RTL within paragraphs"
  );
}

#[test]
fn test_itemize_single_arabic() {
  let style = ComputedStyle::default();
  let text = "مرحبا";
  let bidi = BidiAnalysis::analyze(text, &style);
  let runs = itemize_text(text, &bidi);

  assert!(!runs.is_empty());
  assert_eq!(runs[0].script, Script::Arabic);
}

#[test]
fn test_itemize_single_hebrew() {
  let style = ComputedStyle::default();
  let text = "שלום";
  let bidi = BidiAnalysis::analyze(text, &style);
  let runs = itemize_text(text, &bidi);

  assert!(!runs.is_empty());
  assert_eq!(runs[0].script, Script::Hebrew);
}

#[test]
fn test_itemize_mixed_latin_hebrew() {
  let style = ComputedStyle::default();
  let text = "Hello שלום";
  let bidi = BidiAnalysis::analyze(text, &style);
  let runs = itemize_text(text, &bidi);

  // Should have at least 2 runs
  assert!(runs.len() >= 2);
}

#[test]
fn test_itemize_mixed_latin_arabic() {
  let style = ComputedStyle::default();
  let text = "Hello مرحبا World";
  let bidi = BidiAnalysis::analyze(text, &style);
  let runs = itemize_text(text, &bidi);

  // Should have at least 3 runs: Latin, Arabic, Latin
  assert!(runs.len() >= 2);
}

#[test]
fn test_itemize_mixed_scripts_cjk() {
  let style = ComputedStyle::default();
  let text = "Hello 你好 World";
  let bidi = BidiAnalysis::analyze(text, &style);
  let runs = itemize_text(text, &bidi);

  // Should split at script boundaries
  assert!(runs.len() >= 2);
}

#[test]
fn test_itemize_cyrillic() {
  let style = ComputedStyle::default();
  let text = "Привет";
  let bidi = BidiAnalysis::analyze(text, &style);
  let runs = itemize_text(text, &bidi);

  assert_eq!(runs.len(), 1);
  assert_eq!(runs[0].script, Script::Cyrillic);
}

#[test]
fn test_itemize_greek() {
  let style = ComputedStyle::default();
  let text = "Γειά";
  let bidi = BidiAnalysis::analyze(text, &style);
  let runs = itemize_text(text, &bidi);

  assert_eq!(runs.len(), 1);
  assert_eq!(runs[0].script, Script::Greek);
}

#[test]
fn test_itemized_run_properties() {
  let run = ItemizedRun {
    start: 0,
    end: 5,
    text: "Hello".to_string(),
    script: Script::Latin,
    direction: Direction::LeftToRight,
    level: 0,
  };

  assert_eq!(run.len(), 5);
  assert!(!run.is_empty());
  assert_eq!(run.start, 0);
  assert_eq!(run.end, 5);
}

#[test]
fn test_itemized_run_empty() {
  let run = ItemizedRun {
    start: 0,
    end: 0,
    text: "".to_string(),
    script: Script::Latin,
    direction: Direction::LeftToRight,
    level: 0,
  };

  assert_eq!(run.len(), 0);
  assert!(run.is_empty());
}

// ============================================================================
// Shaping Pipeline Tests
// ============================================================================

#[test]
fn test_pipeline_new() {
  let pipeline = ShapingPipeline::new();
  // Should not panic
  let _ = pipeline;
}

#[test]
fn test_pipeline_default() {
  let pipeline = ShapingPipeline::default();
  // Should not panic
  let _ = pipeline;
}

#[test]
fn test_pipeline_shape_empty() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();
  let style = ComputedStyle::default();

  let result = pipeline.shape("", &style, &font_context);
  assert!(result.is_ok());
  assert!(result.unwrap().is_empty());
}

#[test]
fn test_pipeline_shape_simple_ltr() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();
  let style = ComputedStyle::default();

  let result = pipeline.shape("Hello", &style, &font_context);
  let runs = require_fonts!(result);
  assert!(!runs.is_empty());

  // First run should be LTR
  assert!(runs[0].direction.is_ltr());
  // Should have glyphs
  assert!(!runs[0].glyphs.is_empty());
  // Advance should be positive
  assert!(runs[0].advance > 0.0);
}

#[test]
fn test_pipeline_shape_with_spaces() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();
  let style = ComputedStyle::default();

  let result = pipeline.shape("Hello World", &style, &font_context);
  let runs = require_fonts!(result);
  assert!(!runs.is_empty());
}

#[test]
fn test_pipeline_shape_numbers() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();
  let style = ComputedStyle::default();

  let result = pipeline.shape("12345", &style, &font_context);
  let runs = require_fonts!(result);
  assert!(!runs.is_empty());
}

#[test]
fn test_pipeline_shape_punctuation() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();
  let style = ComputedStyle::default();

  let result = pipeline.shape("Hello, world!", &style, &font_context);
  let runs = require_fonts!(result);
  assert!(!runs.is_empty());
}

#[test]
fn test_pipeline_shape_unicode_latin() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();
  let style = ComputedStyle::default();

  // Latin extended characters
  let result = pipeline.shape("café résumé naïve", &style, &font_context);
  let runs = require_fonts!(result);
  assert!(!runs.is_empty());
}

#[test]
fn test_pipeline_measure_width() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();
  let style = ComputedStyle::default();

  let result = pipeline.measure_width("Hello", &style, &font_context);
  let width = require_fonts!(result);
  assert!(width > 0.0);
}

#[test]
fn test_pipeline_measure_width_empty() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();
  let style = ComputedStyle::default();

  let result = pipeline.measure_width("", &style, &font_context);
  assert!(result.is_ok());
  assert_eq!(result.unwrap(), 0.0);
}

#[test]
fn test_pipeline_measure_width_longer_text() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();
  let style = ComputedStyle::default();

  let short_width = require_fonts!(pipeline.measure_width("Hi", &style, &font_context));
  let long_width = require_fonts!(pipeline.measure_width("Hello, world!", &style, &font_context));

  assert!(long_width > short_width);
}

#[test]
fn zwj_sequences_stay_in_a_single_run() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();
  let style = ComputedStyle::default();

  let text = "👨\u{200d}👩\u{200d}👧";
  let runs = require_fonts!(pipeline.shape(text, &style, &font_context));
  if runs.is_empty() {
    return;
  }

  for boundary in runs.iter().flat_map(|r| [r.start, r.end]) {
    if boundary != 0 && boundary != text.len() {
      panic!(
        "run boundary at {} splits ZWJ sequence (len {})",
        boundary,
        text.len()
      );
    }
  }
}

// ============================================================================
// Glyph Position Tests
// ============================================================================

#[test]
fn test_glyph_positions_ordering() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();
  let style = ComputedStyle::default();

  let result = pipeline.shape("ABC", &style, &font_context);
  let runs = require_fonts!(result);
  if runs.is_empty() {
    return;
  }

  let run = &runs[0];

  // Check that x_offset increases monotonically for LTR text
  let mut prev_offset = -1.0_f32;
  for glyph in &run.glyphs {
    assert!(glyph.x_offset >= prev_offset);
    prev_offset = glyph.x_offset;
  }
}

#[test]
fn test_glyph_advances_positive() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();
  let style = ComputedStyle::default();

  let result = pipeline.shape("Hello", &style, &font_context);
  let runs = require_fonts!(result);
  for run in runs {
    for glyph in &run.glyphs {
      // Most characters should have non-negative advance
      assert!(glyph.x_advance >= 0.0);
    }
  }
}

// ============================================================================
// Shaped Run Tests
// ============================================================================

#[test]
fn test_shaped_run_glyph_count() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();
  let style = ComputedStyle::default();

  let result = pipeline.shape("Hello", &style, &font_context);
  let runs = require_fonts!(result);
  if !runs.is_empty() {
    // Simple ASCII text should produce roughly one glyph per character
    // (may be less due to ligatures)
    assert!(runs[0].glyph_count() > 0);
    assert!(runs[0].glyph_count() <= 5);
  }
}

#[test]
fn test_shaped_run_is_empty() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();
  let style = ComputedStyle::default();

  let result = pipeline.shape("A", &style, &font_context);
  let runs = require_fonts!(result);
  if !runs.is_empty() {
    assert!(!runs[0].is_empty());
  }
}

// ============================================================================
// Font Size Tests
// ============================================================================

#[test]
fn test_pipeline_respects_font_size() {
  let pipeline = ShapingPipeline::new();
  let font_context = bundled_font_context();

  let mut style_16 = ComputedStyle::default();
  style_16.font_size = 16.0;

  let mut style_32 = ComputedStyle::default();
  style_32.font_size = 32.0;

  let width_16 = require_fonts!(pipeline.measure_width("Hello", &style_16, &font_context));
  let width_32 = require_fonts!(pipeline.measure_width("Hello", &style_32, &font_context));

  // Double font size should roughly double width
  assert!(width_32 > width_16 * 1.8);
  assert!(width_32 < width_16 * 2.2);
}
