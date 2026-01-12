#![no_main]

use arbitrary::Unstructured;
use fastrender::style::types::TextJustify;
use fastrender::text::justify::{
  calculate_line_advance, justify_line_with_text, mark_word_boundaries, GlyphPosition, InlineAxis,
  JustificationOptions,
};
use fastrender::text::line_break::{
  find_break_opportunities, find_interior_breaks, find_mandatory_breaks, has_break_at, BreakIterator,
};
use fastrender::{find_soft_hyphens, remove_soft_hyphens, Hyphenator};
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_LEN: usize = 64 * 1024;
const MAX_TEXT_BYTES: usize = 32 * 1024;
const MAX_GLYPHS: usize = 1024;

fn bounded_f32(raw: f32, min: f32, max: f32, default: f32) -> f32 {
  if raw.is_finite() {
    raw.clamp(min, max)
  } else {
    default
  }
}

fn truncate_string_to_bytes(mut value: String, max_bytes: usize) -> String {
  if value.len() <= max_bytes {
    return value;
  }
  let mut end = max_bytes;
  while end > 0 && !value.is_char_boundary(end) {
    end -= 1;
  }
  value.truncate(end);
  value
}

fn choose<T: Copy>(unstructured: &mut Unstructured, candidates: &[T], fallback: T) -> T {
  match unstructured.choose(candidates) {
    Ok(value) => *value,
    Err(_) => fallback,
  }
}

fn build_text(data: &[u8]) -> String {
  let source = if data.len() > MAX_TEXT_BYTES {
    &data[..MAX_TEXT_BYTES]
  } else {
    data
  };
  let text = String::from_utf8_lossy(source).into_owned();
  truncate_string_to_bytes(text, MAX_TEXT_BYTES)
}

fn fuzz_justify(unstructured: &mut Unstructured, text: &str) {
  if text.is_empty() {
    return;
  }

  // Build a synthetic glyph stream (1 glyph per char) with bounded advances.
  let mut glyphs: Vec<GlyphPosition> = Vec::new();
  glyphs.reserve(text.chars().take(MAX_GLYPHS).count());

  let mut x = 0.0f32;
  let mut y = 0.0f32;
  for (cluster, _ch) in text.chars().take(MAX_GLYPHS).enumerate() {
    let x_advance = bounded_f32(
      unstructured.arbitrary::<f32>().unwrap_or(1.0).abs(),
      0.0,
      64.0,
      1.0,
    );
    let y_advance = bounded_f32(
      unstructured.arbitrary::<f32>().unwrap_or(0.0).abs(),
      0.0,
      64.0,
      0.0,
    );
    let glyph_id = unstructured.arbitrary::<u16>().unwrap_or(0);
    glyphs.push(GlyphPosition::with_cluster(
      glyph_id,
      x,
      y,
      x_advance,
      y_advance,
      false,
      cluster,
    ));
    x += x_advance;
    y += y_advance;
  }

  mark_word_boundaries(&mut glyphs, text);

  let axis = if unstructured.arbitrary::<bool>().unwrap_or(false) {
    InlineAxis::Vertical
  } else {
    InlineAxis::Horizontal
  };
  let current_width = calculate_line_advance(&glyphs, axis);
  let extra = bounded_f32(
    unstructured.arbitrary::<f32>().unwrap_or(0.0).abs(),
    0.0,
    1024.0,
    0.0,
  );
  let target_width = current_width + extra;

  let text_justify = choose(
    unstructured,
    &[
      TextJustify::Auto,
      TextJustify::None,
      TextJustify::InterWord,
      TextJustify::InterCharacter,
      TextJustify::Distribute,
    ],
    TextJustify::Auto,
  );

  let options = JustificationOptions::default()
    .with_min_fill_ratio(bounded_f32(
      unstructured.arbitrary::<f32>().unwrap_or(0.75),
      0.0,
      1.0,
      0.75,
    ))
    .with_max_word_spacing_ratio(bounded_f32(
      unstructured.arbitrary::<f32>().unwrap_or(3.0).abs(),
      1.0,
      8.0,
      3.0,
    ))
    .with_letter_spacing_fallback(unstructured.arbitrary::<bool>().unwrap_or(true))
    .with_max_letter_spacing(bounded_f32(
      unstructured.arbitrary::<f32>().unwrap_or(2.0).abs(),
      0.0,
      16.0,
      2.0,
    ))
    .with_justify_last_line(unstructured.arbitrary::<bool>().unwrap_or(false))
    .with_text_justify(text_justify)
    .with_axis(axis);

  let _ = justify_line_with_text(&mut glyphs, target_width, current_width, &options, Some(text));
}

fuzz_target!(|data: &[u8]| {
  let bytes = if data.len() > MAX_INPUT_LEN {
    &data[..MAX_INPUT_LEN]
  } else {
    data
  };
  let mut unstructured = Unstructured::new(bytes);
  let text = build_text(bytes);

  let _ = find_break_opportunities(&text);
  let _ = find_mandatory_breaks(&text);
  let _ = find_interior_breaks(&text);

  if !text.is_empty() {
    let offset = unstructured
      .int_in_range::<usize>(0..=text.len())
      .unwrap_or(0);
    let _ = has_break_at(&text, offset);
    let _ = BreakIterator::new(&text).collect::<Vec<_>>();
  }

  let soft_hyphens = find_soft_hyphens(&text);
  let cleaned = remove_soft_hyphens(&text);
  let _ = soft_hyphens;

  if let Ok(hyphenator) = Hyphenator::new(choose(
    &mut unstructured,
    &[
      "en-US", "en-GB", "de", "fr", "es", "it", "pt-BR", "pt-PT", "nl", "pl", "ru", "sv", "nb",
      "da", "fi", "hu", "cs", "sk", "hr", "ca", "tr", "el", "uk", "la",
    ],
    "en-US",
  )) {
    let _ = hyphenator.hyphenate_text(&cleaned);
  }

  fuzz_justify(&mut unstructured, &cleaned);
});
