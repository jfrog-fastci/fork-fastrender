use crate::style::ComputedStyle;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextareaVisualLine {
  pub start_char: usize,
  pub end_char: usize,
  pub start_byte: usize,
  pub end_byte: usize,
}

impl TextareaVisualLine {
  pub fn len_chars(&self) -> usize {
    self.end_char.saturating_sub(self.start_char)
  }

  pub fn text<'a>(&self, value: &'a str) -> &'a str {
    value.get(self.start_byte..self.end_byte).unwrap_or("")
  }
}

#[derive(Debug, Clone)]
pub(crate) struct TextareaVisualLines {
  pub lines: Vec<TextareaVisualLine>,
  /// Byte offsets for each character boundary in the original string.
  ///
  /// This always includes a final entry at `value.len()`, so its length is `value.chars().count() + 1`.
  pub char_boundary_bytes: Vec<usize>,
}

fn char_boundary_bytes(text: &str) -> Vec<usize> {
  let mut out: Vec<usize> = text.char_indices().map(|(idx, _)| idx).collect();
  out.push(text.len());
  out
}

fn char_idx_at_byte(boundaries: &[usize], byte_idx: usize) -> usize {
  match boundaries.binary_search(&byte_idx) {
    Ok(idx) => idx,
    Err(idx) => idx,
  }
}

fn grapheme_cluster_boundaries_char_idx(text: &str, boundary_bytes: &[usize]) -> Vec<usize> {
  if text.is_empty() {
    return vec![0];
  }
  let mut out = Vec::with_capacity(boundary_bytes.len());
  for (byte_idx, _) in text.grapheme_indices(true) {
    out.push(char_idx_at_byte(boundary_bytes, byte_idx));
  }
  out.push(boundary_bytes.len().saturating_sub(1));
  out
}

pub(crate) fn textarea_chars_per_line(style: &ComputedStyle, available_width: f32) -> usize {
  let width = if available_width.is_finite() {
    available_width.max(0.0)
  } else {
    0.0
  };
  // Best-effort: approximate a single character's width. This matches other interaction fallbacks
  // that estimate advance as `font_size * 0.6` per character.
  let font_size = if style.font_size.is_finite() {
    style.font_size.max(0.0)
  } else {
    0.0
  };
  let char_advance = (font_size * 0.6).max(f32::EPSILON);
  let per_line = (width / char_advance).floor();
  if per_line.is_finite() {
    (per_line as usize).max(1)
  } else {
    1
  }
}

pub(crate) fn build_textarea_visual_lines(value: &str, chars_per_line: usize) -> TextareaVisualLines {
  let chars_per_line = chars_per_line.max(1);
  let boundaries = char_boundary_bytes(value);
  let grapheme_boundaries = grapheme_cluster_boundaries_char_idx(value, &boundaries);
  let total_chars = boundaries.len().saturating_sub(1);

  let mut lines: Vec<TextareaVisualLine> = Vec::new();

  let mut logical_start = 0usize;
  let mut idx = 0usize;
  for ch in value.chars() {
    if ch == '\n' {
      push_wrapped_lines(
        &mut lines,
        &boundaries,
        &grapheme_boundaries,
        logical_start,
        idx,
        chars_per_line,
      );
      logical_start = idx.saturating_add(1);
    }
    idx = idx.saturating_add(1);
  }
  push_wrapped_lines(
    &mut lines,
    &boundaries,
    &grapheme_boundaries,
    logical_start,
    idx,
    chars_per_line,
  );

  // Ensure at least one visual line exists so callers can clamp indices without special-casing
  // empty textareas.
  if lines.is_empty() {
    lines.push(TextareaVisualLine {
      start_char: total_chars,
      end_char: total_chars,
      start_byte: value.len(),
      end_byte: value.len(),
    });
  }

  TextareaVisualLines {
    lines,
    char_boundary_bytes: boundaries,
  }
}

fn push_wrapped_lines(
  out: &mut Vec<TextareaVisualLine>,
  boundaries: &[usize],
  grapheme_boundaries: &[usize],
  start_char: usize,
  end_char: usize,
  chars_per_line: usize,
) {
  let total_chars = boundaries.len().saturating_sub(1);
  let start_char = start_char.min(total_chars);
  let end_char = end_char.min(total_chars);

  if start_char >= end_char {
    // Empty logical line (e.g. consecutive newlines).
    let byte = *boundaries.get(start_char).unwrap_or(&0);
    out.push(TextareaVisualLine {
      start_char,
      end_char: start_char,
      start_byte: byte,
      end_byte: byte,
    });
    return;
  }

  // Wrap by grapheme clusters so visual line boundaries never split a single user-perceived
  // character (e.g. ZWJ emoji sequences).
  let start_boundary_idx = grapheme_boundaries
    .partition_point(|&b| b <= start_char)
    .saturating_sub(1);
  let end_boundary_idx = grapheme_boundaries.partition_point(|&b| b < end_char);
  let end_boundary_idx = end_boundary_idx.min(grapheme_boundaries.len().saturating_sub(1));

  let mut boundary_idx = start_boundary_idx;
  while boundary_idx < end_boundary_idx {
    let next_boundary_idx = (boundary_idx + chars_per_line).min(end_boundary_idx);
    let seg_start = *grapheme_boundaries.get(boundary_idx).unwrap_or(&start_char);
    let seg_end = *grapheme_boundaries.get(next_boundary_idx).unwrap_or(&end_char);
    let start_byte = *boundaries.get(seg_start).unwrap_or(&0);
    let end_byte = *boundaries.get(seg_end).unwrap_or(&start_byte);
    out.push(TextareaVisualLine {
      start_char: seg_start,
      end_char: seg_end,
      start_byte,
      end_byte,
    });
    boundary_idx = next_boundary_idx;
  }
}

pub(crate) fn textarea_char_at(value: &str, boundaries: &[usize], char_idx: usize) -> Option<char> {
  let byte = *boundaries.get(char_idx)?;
  value.get(byte..)?.chars().next()
}

pub(crate) fn textarea_visual_line_index_for_caret(
  value: &str,
  layout: &TextareaVisualLines,
  caret: usize,
) -> usize {
  let lines = &layout.lines;
  if lines.is_empty() {
    return 0;
  }

  let total_chars = layout.char_boundary_bytes.len().saturating_sub(1);
  let caret = caret.min(total_chars);
  let caret_is_before_newline = (caret < total_chars)
    .then(|| textarea_char_at(value, &layout.char_boundary_bytes, caret) == Some('\n'))
    .unwrap_or(false);

  if caret_is_before_newline {
    let mut found = None;
    for (idx, line) in lines.iter().enumerate() {
      if line.end_char == caret {
        found = Some(idx);
      }
    }
    if let Some(idx) = found {
      return idx;
    }
  }

  for (idx, line) in lines.iter().enumerate() {
    if line.start_char == line.end_char {
      if caret == line.start_char {
        return idx;
      }
    } else if caret >= line.start_char && caret < line.end_char {
      return idx;
    }
  }

  lines.len().saturating_sub(1)
}
