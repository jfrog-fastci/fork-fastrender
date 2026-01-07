//! Parsing helpers for responsive image HTML attributes (`srcset` / `sizes`).
//!
//! These helpers are shared by the renderer (box generation / replaced elements)
//! and developer tooling (e.g. asset prefetch) so both paths interpret author
//! markup consistently.

use crate::tree::box_tree::{SizesEntry, SizesLength, SizesList, SrcsetCandidate, SrcsetDescriptor};
use cssparser::{Parser, ParserInput, Token};

const MAX_SRCSET_COMMA_CONTEXT_BYTES: usize = 256;

/// Parse an HTML `srcset` attribute into candidate URLs with descriptors.
///
/// This is a small, allocation-minimal parser intended to match the renderer's
/// internal behavior. Invalid candidate strings are ignored.
pub fn parse_srcset(attr: &str) -> Vec<SrcsetCandidate> {
  parse_srcset_with_limit(attr, usize::MAX)
}

/// Parse an HTML `srcset` attribute into candidate URLs with descriptors,
/// returning at most `max_candidates` valid entries.
///
/// This is primarily used by developer tooling (e.g. regex-based HTML asset
/// discovery) to keep memory bounded when encountering pathological attributes.
pub fn parse_srcset_with_limit(attr: &str, max_candidates: usize) -> Vec<SrcsetCandidate> {
  fn is_data_url(bytes: &[u8], start: usize) -> bool {
    if start + 5 > bytes.len() {
      return false;
    }
    let matches =
      |offset: usize, expected: u8| bytes[start + offset].to_ascii_lowercase() == expected;
    matches(0, b'd')
      && matches(1, b'a')
      && matches(2, b't')
      && matches(3, b'a')
      && bytes[start + 4] == b':'
  }

  fn is_likely_comma_in_cdn_transform_url(
    bytes: &[u8],
    url_start: usize,
    comma_idx: usize,
  ) -> bool {
    // Heuristic for malformed-but-common production markup:
    //
    // Many CDNs (including Condé Nast properties like wired.com) embed transform parameters
    // directly in the URL path using unescaped commas, e.g.:
    //   https://img.example/master/w_2560,c_limit/foo.jpg
    //
    // In valid `srcset`, commas are candidate separators, so such URLs should have escaped the
    // comma. But in practice, treating every comma as a separator produces bogus relative URLs
    // like `/c_limit/foo.jpg`.
    //
    // We treat a comma as part of the URL when it appears between two underscore-separated
    // transform params (`w_2560,c_limit`) that are followed by another path segment or transform.

    // Find the transform param before the comma (from the last `/`, `?`, or `#`).
    let scan_start = url_start.max(comma_idx.saturating_sub(MAX_SRCSET_COMMA_CONTEXT_BYTES));
    let mut segment_start = scan_start;
    for i in (scan_start..comma_idx).rev() {
      match bytes[i] {
        b'/' | b'?' | b'#' => {
          segment_start = i + 1;
          break;
        }
        _ => {}
      }
    }
    if segment_start >= comma_idx {
      return false;
    }
    let before = &bytes[segment_start..comma_idx];
    if !before.contains(&b'_') || before.contains(&b'.') {
      return false;
    }

    // Find the transform param after the comma (up to the next delimiter).
    let mut after_end = comma_idx + 1;
    while after_end < bytes.len() {
      let b = bytes[after_end];
      if b.is_ascii_whitespace() || matches!(b, b'/' | b'?' | b'#' | b',') {
        break;
      }
      after_end += 1;
    }
    if comma_idx + 1 >= after_end {
      return false;
    }
    let after = &bytes[comma_idx + 1..after_end];
    if !after.contains(&b'_') || after.contains(&b'.') {
      return false;
    }

    // Finally, ensure the URL continues after this transform list with another path segment
    // (common patterns include `/foo.jpg`).
    let mut idx = comma_idx + 1;
    while idx < bytes.len() {
      let b = bytes[idx];
      if b.is_ascii_whitespace() {
        break;
      }
      if b == b'/' {
        return true;
      }
      idx += 1;
    }

    false
  }

  fn is_likely_comma_in_query_numeric_list(
    bytes: &[u8],
    url_start: usize,
    comma_idx: usize,
  ) -> bool {
    // Some sites (notably WordPress, including nasa.gov fixtures) embed comma-separated numeric
    // values in query parameters, e.g. `?resize=300,163`.
    //
    // Like the CDN transform case above, these commas are invalid per spec but common in
    // production `srcset` values, so we treat them as part of the URL.
    let scan_start = url_start.max(comma_idx.saturating_sub(MAX_SRCSET_COMMA_CONTEXT_BYTES));
    let query_start = bytes[scan_start..comma_idx]
      .iter()
      .rposition(|&b| b == b'?')
      .map(|pos| scan_start + pos);
    let Some(query_start) = query_start else {
      return false;
    };

    // Find the start of the current query parameter (after the last '&' following '?').
    let mut param_start = query_start + 1;
    for i in (param_start..comma_idx).rev() {
      if bytes[i] == b'&' {
        param_start = i + 1;
        break;
      }
    }

    // Find the '=' within the parameter.
    let eq_pos = bytes[param_start..comma_idx]
      .iter()
      .rposition(|&b| b == b'=')
      .map(|pos| param_start + pos);
    let Some(eq_pos) = eq_pos else {
      return false;
    };
    if eq_pos + 1 >= comma_idx {
      return false;
    }

    fn is_numeric_list_char(b: u8) -> bool {
      b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b',')
    }

    // Verify numeric-list characters on both sides of the comma.
    let before = &bytes[eq_pos + 1..comma_idx];
    if before.is_empty() || !before.iter().all(|&b| is_numeric_list_char(b)) {
      return false;
    }
    if !before.iter().any(|b| b.is_ascii_digit()) {
      return false;
    }

    let mut after_end = comma_idx + 1;
    while after_end < bytes.len() {
      let b = bytes[after_end];
      if b.is_ascii_whitespace() || matches!(b, b'&' | b'#' | b',') {
        break;
      }
      after_end += 1;
    }
    if comma_idx + 1 >= after_end {
      return false;
    }
    let after = &bytes[comma_idx + 1..after_end];
    if after.is_empty() || !after.iter().all(|&b| is_numeric_list_char(b)) {
      return false;
    }
    if !after.iter().any(|b| b.is_ascii_digit()) {
      return false;
    }

    true
  }

  fn is_likely_comma_in_path_crop_rect(bytes: &[u8], url_start: usize, comma_idx: usize) -> bool {
    // Another common `srcset` compatibility issue:
    //
    // Amazon-hosted images (including IMDb) embed crop rectangles directly in the filename using
    // unescaped commas, e.g.:
    //   https://m.media-amazon.com/images/..._UX414_CR0,0,414,612_AL_.jpg
    //
    // Here the `CR` token is followed by a comma-separated numeric list. Treating those commas as
    // candidate separators produces bogus relative URLs like `/414_.jpg` on imdb.com.
    //
    // We treat a comma as part of the URL when it appears inside the numeric list following a
    // `_CR` marker in the current path segment.

    // Find the current path segment start (after the last `/`, `?`, or `#`).
    let scan_start = url_start.max(comma_idx.saturating_sub(MAX_SRCSET_COMMA_CONTEXT_BYTES));
    let mut segment_start = scan_start;
    for i in (scan_start..comma_idx).rev() {
      match bytes[i] {
        b'/' | b'?' | b'#' => {
          segment_start = i + 1;
          break;
        }
        _ => {}
      }
    }

    // Find the last `_CR` marker before this comma.
    let mut cr_start = None;
    if segment_start + 3 <= comma_idx {
      for i in (segment_start..=comma_idx - 3).rev() {
        if bytes[i] == b'_'
          && bytes[i + 1].to_ascii_uppercase() == b'C'
          && bytes[i + 2].to_ascii_uppercase() == b'R'
        {
          cr_start = Some(i);
          break;
        }
      }
    }
    let Some(cr_start) = cr_start else {
      return false;
    };

    let list_start = cr_start + 3;
    if list_start >= comma_idx {
      return false;
    }

    fn is_numeric_list_char(b: u8) -> bool {
      b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b',')
    }

    let before = &bytes[list_start..comma_idx];
    if before.is_empty() || !before.iter().all(|&b| is_numeric_list_char(b)) {
      return false;
    }
    if !before.iter().any(|b| b.is_ascii_digit()) {
      return false;
    }

    let mut after_end = comma_idx + 1;
    while after_end < bytes.len() {
      let b = bytes[after_end];
      if b.is_ascii_whitespace() || matches!(b, b'_' | b'.' | b'/' | b'?' | b'#' | b',') {
        break;
      }
      after_end += 1;
    }
    if comma_idx + 1 >= after_end {
      return false;
    }
    let after = &bytes[comma_idx + 1..after_end];
    if after.is_empty() || !after.iter().all(|&b| is_numeric_list_char(b)) {
      return false;
    }
    if !after.iter().any(|b| b.is_ascii_digit()) {
      return false;
    }

    true
  }

  fn is_likely_comma_in_percent_encoded_space_filename(
    bytes: &[u8],
    _url_start: usize,
    comma_idx: usize,
  ) -> bool {
    // Another production `srcset` compatibility issue:
    //
    // Some sites (notably Condé Nast properties like newyorker.com) include commas inside the
    // filename itself, followed by a percent-encoded space (`,%20`) and the rest of the filename:
    //
    //   https://media.example/.../Artist%20-%20Title,%202023.jpg 120w, ...
    //
    // In valid `srcset`, commas separate candidates, but treating this comma as a separator
    // produces a bogus relative URL like `%202023.jpg`.
    //
    // Heuristic: treat the comma as part of the URL when it is immediately followed by `%20` and
    // the remainder looks like a filename with a common image extension.
    if comma_idx + 3 >= bytes.len() {
      return false;
    }
    if bytes[comma_idx + 1] != b'%' {
      return false;
    }
    if bytes[comma_idx + 2].to_ascii_lowercase() != b'2' || bytes[comma_idx + 3] != b'0' {
      return false;
    }

    // Scan forward to the end of the URL token (whitespace separates URL and descriptors).
    let mut end = comma_idx + 4;
    while end < bytes.len() {
      let b = bytes[end];
      if b.is_ascii_whitespace() {
        break;
      }
      // Stop at another comma so we don't scan into the next candidate if we're wrong.
      if b == b',' {
        break;
      }
      end += 1;
    }
    if comma_idx + 1 >= end {
      return false;
    }

    let tail = &bytes[comma_idx + 1..end];
    let tail_end = tail
      .iter()
      .position(|&b| matches!(b, b'?' | b'#'))
      .unwrap_or(tail.len());
    let tail = &tail[..tail_end];

    // Look for a likely image extension at the end of the tail.
    let Some(dot_pos) = tail.iter().rposition(|&b| b == b'.') else {
      return false;
    };
    let ext = &tail[dot_pos + 1..];
    if ext.is_empty() || ext.len() > 5 {
      return false;
    }
    if !ext.iter().all(|b| b.is_ascii_alphabetic()) {
      return false;
    }
    let ext = ext
      .iter()
      .map(|b| b.to_ascii_lowercase())
      .collect::<Vec<_>>();
    // A small allowlist keeps the heuristic tight; expand if new cases appear.
    let ext = ext.as_slice();
    matches!(
      ext,
      b"jpg" | b"jpeg" | b"png" | b"gif" | b"webp" | b"avif" | b"svg"
    )
  }

  let bytes = attr.as_bytes();
  let mut out = Vec::new();
  let mut idx = 0;

  while idx < bytes.len() {
    if out.len() >= max_candidates {
      break;
    }
    while idx < bytes.len() && (bytes[idx].is_ascii_whitespace() || bytes[idx] == b',') {
      idx += 1;
    }
    if idx >= bytes.len() {
      break;
    }

    let url_start = idx;
    let data_url = is_data_url(bytes, url_start);

    while idx < bytes.len() {
      let b = bytes[idx];
      if b.is_ascii_whitespace() {
        break;
      }
      if b == b',' {
        if data_url {
          // `data:` URLs may contain commas in the payload; do not treat them as srcset separators.
          idx += 1;
          continue;
        }
        if idx + 1 < bytes.len() && !bytes[idx + 1].is_ascii_whitespace() {
          if is_likely_comma_in_cdn_transform_url(bytes, url_start, idx)
            || is_likely_comma_in_query_numeric_list(bytes, url_start, idx)
            || is_likely_comma_in_path_crop_rect(bytes, url_start, idx)
            || is_likely_comma_in_percent_encoded_space_filename(bytes, url_start, idx)
          {
            idx += 1;
            continue;
          }
        }
        // Candidate separator (no descriptors).
        break;
      }
      idx += 1;
    }

    let url = attr[url_start..idx].trim();
    if url.is_empty() {
      while idx < bytes.len() && bytes[idx] != b',' {
        idx += 1;
      }
      continue;
    }

    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
      idx += 1;
    }

    let desc_start = idx;
    while idx < bytes.len() && bytes[idx] != b',' {
      idx += 1;
    }
    let desc_str = attr[desc_start..idx].trim();

    let mut density: Option<f32> = None;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut unknown = false;
    let mut valid = true;
    for desc in desc_str.split_whitespace() {
      let d = desc.trim();
      if d.is_empty() {
        continue;
      }

      if let Some(raw) = d.strip_suffix("dppx") {
        match raw.parse::<f32>() {
          Ok(val) if val.is_finite() && val > 0.0 => {
            if density.is_some() {
              valid = false;
              break;
            }
            density = Some(val);
          }
          _ => {
            valid = false;
            break;
          }
        }
      } else if let Some(raw) = d.strip_suffix('x') {
        match raw.parse::<f32>() {
          Ok(val) if val.is_finite() && val > 0.0 => {
            if density.is_some() {
              valid = false;
              break;
            }
            density = Some(val);
          }
          _ => {
            valid = false;
            break;
          }
        }
      } else if let Some(raw) = d.strip_suffix('w') {
        match raw.parse::<u32>() {
          Ok(val) if val > 0 => {
            if width.is_some() {
              valid = false;
              break;
            }
            width = Some(val);
          }
          _ => {
            valid = false;
            break;
          }
        }
      } else if let Some(raw) = d.strip_suffix('h') {
        match raw.parse::<u32>() {
          Ok(val) if val > 0 => {
            if height.is_some() {
              valid = false;
              break;
            }
            height = Some(val);
          }
          _ => {
            valid = false;
            break;
          }
        }
      } else {
        unknown = true;
      }
    }

    if valid {
      let descriptor = match (density, width, height) {
        (Some(d), None, None) if !unknown => Some(SrcsetDescriptor::Density(d)),
        (None, Some(w), None) if !unknown => Some(SrcsetDescriptor::Width(w)),
        (None, Some(w), Some(h)) if !unknown => Some(SrcsetDescriptor::WidthHeight {
          width: w,
          height: h,
        }),
        // Height descriptors without a width descriptor are invalid.
        (None, None, Some(_)) => None,
        // Any unknown tokens or invalid descriptor combinations make the candidate invalid.
        _ if density.is_some() || width.is_some() || height.is_some() => None,
        // No valid descriptors -> default to 1x (unknown tokens ignored).
        _ => Some(SrcsetDescriptor::Density(1.0)),
      };

      if let Some(descriptor) = descriptor {
        out.push(SrcsetCandidate {
          url: url.to_string(),
          descriptor,
        });
      }
    }

    if idx < bytes.len() && bytes[idx] == b',' {
      idx += 1;
    }
  }

  out
}

/// Parse an HTML `sizes` attribute into a `SizesList`.
///
/// Returns `None` if no valid size entries are found.
pub fn parse_sizes(attr: &str) -> Option<SizesList> {
  use crate::style::media::MediaQuery;

  fn split_top_level_commas(input: &str) -> Vec<&str> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;

    for (idx, &b) in bytes.iter().enumerate() {
      match b {
        b'(' => depth += 1,
        b')' => {
          if depth > 0 {
            depth -= 1;
          }
        }
        b',' if depth == 0 => {
          out.push(&input[start..idx]);
          start = idx + 1;
        }
        _ => {}
      }
    }

    out.push(&input[start..]);
    out
  }

  fn split_media_and_length(entry: &str) -> (Option<&str>, &str) {
    // `sizes` values are `<media-condition>? <length>`, where the `<length>` can itself contain
    // whitespace (e.g. `calc(100vw - 20px)`) and commas (e.g. `clamp(10px, 20vw, 30px)`).
    //
    // Split on the last *top-level* whitespace (outside any ()/[]/{}) so we don't tear apart
    // calc/min/max/clamp expressions.
    let mut paren_depth = 0i32;
    let mut bracket_depth = 0i32;
    let mut brace_depth = 0i32;
    let mut in_string: Option<char> = None;
    let mut last_ws: Option<usize> = None;
    let mut chars = entry.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
      if let Some(quote) = in_string {
        if ch == '\\' {
          // Skip escaped character.
          let _ = chars.next();
          continue;
        }
        if ch == quote {
          in_string = None;
        }
        continue;
      }

      if ch == '\\' {
        // Skip escaped character.
        let _ = chars.next();
        continue;
      }

      match ch {
        '(' => paren_depth += 1,
        ')' => paren_depth = (paren_depth - 1).max(0),
        '[' => bracket_depth += 1,
        ']' => bracket_depth = (bracket_depth - 1).max(0),
        '{' => brace_depth += 1,
        '}' => brace_depth = (brace_depth - 1).max(0),
        '"' | '\'' => in_string = Some(ch),
        ch
          if ch.is_ascii_whitespace()
            && paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0 =>
        {
          last_ws = Some(idx);
        }
        _ => {}
      }
    }

    let Some(ws_idx) = last_ws else {
      return (None, entry.trim());
    };

    let (head, tail) = entry.split_at(ws_idx);
    let media = head.trim();
    let length = tail.trim();

    if media.is_empty() {
      (None, length)
    } else {
      (Some(media), length)
    }
  }

  let mut entries = Vec::new();
  for item in split_top_level_commas(attr) {
    let item = item.trim();
    if item.is_empty() {
      continue;
    }

    let (media_part, length_part) = split_media_and_length(item);
    let length = match parse_sizes_length(length_part) {
      Some(l) => l,
      None => continue,
    };

    let media = match media_part {
      Some(cond) if !cond.is_empty() => MediaQuery::parse_list(cond).ok(),
      _ => None,
    };

    entries.push(SizesEntry { media, length });
  }

  if entries.is_empty() {
    None
  } else {
    Some(SizesList { entries })
  }
}

fn consume_nested_tokens_for_slice<'i, 't>(
  input: &mut Parser<'i, 't>,
) -> Result<(), cssparser::ParseError<'i, ()>> {
  while !input.is_exhausted() {
    let token = input.next_including_whitespace_and_comments()?;
    match token {
      Token::CurlyBracketBlock
      | Token::ParenthesisBlock
      | Token::SquareBracketBlock
      | Token::Function(_) => {
        input.parse_nested_block(consume_nested_tokens_for_slice)?;
      }
      _ => {}
    }
  }
  Ok(())
}

fn parse_sizes_calc_sum(value: &str) -> Option<SizesLength> {
  use crate::style::values::Length;

  fn strip_wrapping_parentheses(mut value: &str) -> (&str, bool) {
    let mut stripped_any = false;
    loop {
      let trimmed = value.trim();
      if !(trimmed.starts_with('(') && trimmed.ends_with(')')) {
        return (trimmed, stripped_any);
      }

      let bytes = trimmed.as_bytes();
      let mut depth = 0usize;
      let mut closing_idx = None;
      for (idx, &b) in bytes.iter().enumerate() {
        match b {
          b'(' => depth += 1,
          b')' => {
            if depth > 0 {
              depth -= 1;
            }
            if depth == 0 {
              closing_idx = Some(idx);
              break;
            }
          }
          _ => {}
        }
      }

      if closing_idx == Some(trimmed.len() - 1) {
        stripped_any = true;
        value = &trimmed[1..trimmed.len() - 1];
        continue;
      }

      return (trimmed, stripped_any);
    }
  }

  fn parse_term(term: &str) -> Option<SizesLength> {
    let (term, stripped) = strip_wrapping_parentheses(term);
    if term.is_empty() {
      return None;
    }
    if stripped {
      return parse_sizes_calc_sum(term);
    }
    parse_sizes_length(term)
  }

  let (value, _) = strip_wrapping_parentheses(value);
  if value.is_empty() {
    return None;
  }

  let bytes = value.as_bytes();
  let mut depth = 0usize;
  let mut start = 0usize;
  let mut sign: i8 = 1;
  let mut terms: Vec<(i8, &str)> = Vec::new();

  for (idx, &b) in bytes.iter().enumerate() {
    match b {
      b'(' => depth += 1,
      b')' => {
        if depth > 0 {
          depth -= 1;
        }
      }
      b'*' | b'/' if depth == 0 => {
        // Fall back to the full CSS calc parser (which supports multiplication/division) when the
        // expression uses operators we don't support here.
        return None;
      }
      b'+' | b'-' if depth == 0 => {
        let before = &value[start..idx];
        let has_content = before.chars().any(|ch| !ch.is_ascii_whitespace());
        if has_content {
          terms.push((sign, before));
          sign = if b == b'-' { -1 } else { 1 };
          start = idx + 1;
        } else {
          // Unary sign.
          if b == b'-' {
            sign *= -1;
          }
          start = idx + 1;
        }
      }
      _ => {}
    }
  }

  let tail = &value[start..];
  if tail.chars().any(|ch| !ch.is_ascii_whitespace()) {
    terms.push((sign, tail));
  }
  if terms.is_empty() {
    return None;
  }

  let mut iter = terms.into_iter();
  let (first_sign, first) = iter.next()?;
  let mut out = parse_term(first)?;
  if first_sign < 0 {
    out = SizesLength::Sub(Box::new(Length::px(0.0).into()), Box::new(out));
  }

  for (sign, term) in iter {
    let rhs = parse_term(term)?;
    out = if sign < 0 {
      SizesLength::Sub(Box::new(out), Box::new(rhs))
    } else {
      SizesLength::Add(Box::new(out), Box::new(rhs))
    };
  }

  Some(out)
}

fn parse_sizes_length(value: &str) -> Option<SizesLength> {
  use crate::css::properties::parse_length;
  use crate::style::values::{Length, LengthUnit};
  fn contains_calc_math_functions(input: &str) -> bool {
    fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
      let needle_bytes = needle.as_bytes();
      haystack
        .as_bytes()
        .windows(needle_bytes.len())
        .any(|window| window.eq_ignore_ascii_case(needle_bytes))
    }
    contains_ignore_ascii_case(input, "min(")
      || contains_ignore_ascii_case(input, "max(")
      || contains_ignore_ascii_case(input, "clamp(")
  }

  fn split_top_level_commas(input: &str) -> Vec<&str> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;

    for (idx, &b) in bytes.iter().enumerate() {
      match b {
        b'(' => depth += 1,
        b')' => {
          if depth > 0 {
            depth -= 1;
          }
        }
        b',' if depth == 0 => {
          out.push(&input[start..idx]);
          start = idx + 1;
        }
        _ => {}
      }
    }

    out.push(&input[start..]);
    out
  }

  let mut input = ParserInput::new(value);
  let mut parser = Parser::new(&mut input);

  let parsed = match parser.next() {
    Ok(Token::Dimension {
      value, ref unit, ..
    }) => {
      let unit = unit.as_ref();
      if unit.eq_ignore_ascii_case("px") {
        Some(Length::px(*value))
      } else if unit.eq_ignore_ascii_case("em") {
        Some(Length::em(*value))
      } else if unit.eq_ignore_ascii_case("rem") {
        Some(Length::rem(*value))
      } else if unit.eq_ignore_ascii_case("ex") {
        Some(Length::ex(*value))
      } else if unit.eq_ignore_ascii_case("ch") {
        Some(Length::ch(*value))
      } else if unit.eq_ignore_ascii_case("pt") {
        Some(Length::pt(*value))
      } else if unit.eq_ignore_ascii_case("pc") {
        Some(Length::pc(*value))
      } else if unit.eq_ignore_ascii_case("in") {
        Some(Length::inches(*value))
      } else if unit.eq_ignore_ascii_case("cm") {
        Some(Length::cm(*value))
      } else if unit.eq_ignore_ascii_case("mm") {
        Some(Length::mm(*value))
      } else if unit.eq_ignore_ascii_case("q") {
        Some(Length::q(*value))
      } else if unit.eq_ignore_ascii_case("vw") {
        Some(Length::new(*value, LengthUnit::Vw))
      } else if unit.eq_ignore_ascii_case("vh") {
        Some(Length::new(*value, LengthUnit::Vh))
      } else if unit.eq_ignore_ascii_case("vmin") {
        Some(Length::new(*value, LengthUnit::Vmin))
      } else if unit.eq_ignore_ascii_case("vmax") {
        Some(Length::new(*value, LengthUnit::Vmax))
      } else if unit.eq_ignore_ascii_case("svw") {
        Some(Length::new(*value, LengthUnit::Vw))
      } else if unit.eq_ignore_ascii_case("svh") {
        Some(Length::new(*value, LengthUnit::Vh))
      } else if unit.eq_ignore_ascii_case("svmin") {
        Some(Length::new(*value, LengthUnit::Vmin))
      } else if unit.eq_ignore_ascii_case("svmax") {
        Some(Length::new(*value, LengthUnit::Vmax))
      } else if unit.eq_ignore_ascii_case("lvw") {
        Some(Length::new(*value, LengthUnit::Vw))
      } else if unit.eq_ignore_ascii_case("lvh") {
        Some(Length::new(*value, LengthUnit::Vh))
      } else if unit.eq_ignore_ascii_case("lvmin") {
        Some(Length::new(*value, LengthUnit::Vmin))
      } else if unit.eq_ignore_ascii_case("lvmax") {
        Some(Length::new(*value, LengthUnit::Vmax))
      } else if unit.eq_ignore_ascii_case("dvw") {
        Some(Length::new(*value, LengthUnit::Dvw))
      } else if unit.eq_ignore_ascii_case("dvh") {
        Some(Length::new(*value, LengthUnit::Dvh))
      } else if unit.eq_ignore_ascii_case("dvmin") {
        Some(Length::new(*value, LengthUnit::Dvmin))
      } else if unit.eq_ignore_ascii_case("dvmax") {
        Some(Length::new(*value, LengthUnit::Dvmax))
      } else {
        None
      }
      .map(Into::into)
    }
    Ok(Token::Function(ref name)) if name.eq_ignore_ascii_case("calc") => {
      parser
        .parse_nested_block(|block| {
          let start = block.position();
          consume_nested_tokens_for_slice(block)?;
          Ok(block.slice_from(start))
        })
        .ok()
        .and_then(|inner| {
          if contains_calc_math_functions(inner) {
            parse_sizes_calc_sum(inner).or_else(|| parse_length(value).map(Into::into))
          } else {
            parse_length(value).map(Into::into)
          }
        })
    }
    Ok(Token::Function(ref name)) if name.eq_ignore_ascii_case("min") => parser
      .parse_nested_block(|block| {
        let start = block.position();
        consume_nested_tokens_for_slice(block)?;
        Ok(block.slice_from(start))
      })
      .ok()
      .and_then(|inner| {
        let args = split_top_level_commas(inner);
        if args.is_empty() {
          return None;
        }
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
          values.push(parse_sizes_math_arg(arg)?);
        }
        Some(SizesLength::Min(values))
      }),
    Ok(Token::Function(ref name)) if name.eq_ignore_ascii_case("max") => parser
      .parse_nested_block(|block| {
        let start = block.position();
        consume_nested_tokens_for_slice(block)?;
        Ok(block.slice_from(start))
      })
      .ok()
      .and_then(|inner| {
        let args = split_top_level_commas(inner);
        if args.is_empty() {
          return None;
        }
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
          values.push(parse_sizes_math_arg(arg)?);
        }
        Some(SizesLength::Max(values))
      }),
    Ok(Token::Function(ref name)) if name.eq_ignore_ascii_case("clamp") => parser
      .parse_nested_block(|block| {
        let start = block.position();
        consume_nested_tokens_for_slice(block)?;
        Ok(block.slice_from(start))
      })
      .ok()
      .and_then(|inner| {
        let args = split_top_level_commas(inner);
        if args.len() != 3 {
          return None;
        }
        let min = parse_sizes_math_arg(args[0])?;
        let preferred = parse_sizes_math_arg(args[1])?;
        let max = parse_sizes_math_arg(args[2])?;
        Some(SizesLength::Clamp {
          min: Box::new(min),
          preferred: Box::new(preferred),
          max: Box::new(max),
        })
      }),
    Ok(Token::Percentage { unit_value, .. }) => Some(Length::percent(*unit_value * 100.0).into()),
    Ok(Token::Number { value, .. }) if *value == 0.0 => Some(Length::px(0.0).into()),
    Err(_) => None,
    _ => None,
  }?;

  parser.skip_whitespace();
  if parser.is_exhausted() {
    Some(parsed)
  } else {
    None
  }
}

fn parse_sizes_math_arg(value: &str) -> Option<SizesLength> {
  let trimmed = value.trim();
  if trimmed.is_empty() {
    return None;
  }

  if let Some(parsed) = parse_sizes_length(trimmed) {
    return Some(parsed);
  }

  // `min()`/`max()`/`clamp()` arguments follow the `<calc-sum>` grammar which allows arithmetic
  // without an explicit `calc()` wrapper (e.g. `2vw + 1rem`). Our base parser requires the
  // `calc()` function for such expressions, so fall back by wrapping the argument.
  let wrapped = format!("calc({trimmed})");
  parse_sizes_length(&wrapped)
}

#[cfg(test)]
mod tests {
  use super::{parse_sizes, parse_srcset};
  use crate::style::values::{Length, LengthUnit};
  use crate::tree::box_tree::{SizesLength, SrcsetDescriptor};

  #[test]
  fn parse_srcset_parses_density_descriptors() {
    let parsed = parse_srcset("a.png 1x, b.png 2x, c.png 1.5x");
    assert_eq!(parsed.len(), 3);
    assert_eq!(parsed[0].url, "a.png");
    assert!(matches!(parsed[0].descriptor, SrcsetDescriptor::Density(d) if d == 1.0));
    assert!(matches!(parsed[1].descriptor, SrcsetDescriptor::Density(d) if d == 2.0));
    assert!(
      matches!(parsed[2].descriptor, SrcsetDescriptor::Density(d) if (d - 1.5).abs() < f32::EPSILON)
    );
  }

  #[test]
  fn parse_srcset_parses_dppx_descriptors() {
    let parsed = parse_srcset("a.png 1dppx, b.png 2dppx");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].url, "a.png");
    assert!(matches!(parsed[0].descriptor, SrcsetDescriptor::Density(d) if d == 1.0));
    assert_eq!(parsed[1].url, "b.png");
    assert!(matches!(parsed[1].descriptor, SrcsetDescriptor::Density(d) if d == 2.0));
  }

  #[test]
  fn parse_srcset_parses_width_descriptors() {
    let parsed = parse_srcset("a.png 320w, b.png 640w");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].url, "a.png");
    assert!(matches!(parsed[0].descriptor, SrcsetDescriptor::Width(320)));
    assert!(matches!(parsed[1].descriptor, SrcsetDescriptor::Width(640)));
  }

  #[test]
  fn parse_srcset_parses_width_height_descriptors() {
    let parsed = parse_srcset("a.png 100w 50h, b.png 200w 100h");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].url, "a.png");
    assert!(matches!(
      parsed[0].descriptor,
      SrcsetDescriptor::WidthHeight {
        width: 100,
        height: 50
      }
    ));
    assert_eq!(parsed[1].url, "b.png");
    assert!(matches!(
      parsed[1].descriptor,
      SrcsetDescriptor::WidthHeight {
        width: 200,
        height: 100
      }
    ));
  }

  #[test]
  fn parse_srcset_ignores_invalid_descriptor_tokens() {
    // Unknown descriptor tokens should be ignored, producing the default 1x descriptor.
    let parsed = parse_srcset("a.png foo, b.png 2x bar, c.png 2x");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].url, "a.png");
    assert!(matches!(parsed[0].descriptor, SrcsetDescriptor::Density(d) if d == 1.0));
    assert_eq!(parsed[1].url, "c.png");
    assert!(matches!(parsed[1].descriptor, SrcsetDescriptor::Density(d) if d == 2.0));
  }

  #[test]
  fn parse_srcset_skips_invalid_descriptor_combinations() {
    let parsed = parse_srcset(
      "a.png 100w 50h 2x, b.png 100w 0h, c.png 2x 100w, d.png 1x 2x, e.png 100h, ok.png 100w 50h",
    );
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].url, "ok.png");
    assert!(matches!(
      parsed[0].descriptor,
      SrcsetDescriptor::WidthHeight {
        width: 100,
        height: 50
      }
    ));
  }

  #[test]
  fn parse_srcset_allows_commas_inside_urls() {
    let parsed = parse_srcset(
      "https://img.example/master/w_2560,c_limit/foo.jpg 1x,https://img.example/bar.jpg 2x",
    );
    assert_eq!(parsed.len(), 2);
    assert_eq!(
      parsed[0].url,
      "https://img.example/master/w_2560,c_limit/foo.jpg"
    );
    assert!(matches!(parsed[0].descriptor, SrcsetDescriptor::Density(d) if d == 1.0));
    assert_eq!(parsed[1].url, "https://img.example/bar.jpg");
    assert!(matches!(parsed[1].descriptor, SrcsetDescriptor::Density(d) if d == 2.0));
  }

  #[test]
  fn parse_srcset_allows_commas_inside_urls_with_width_descriptors() {
    let parsed = parse_srcset(
      "https://example.com/w_2560,c_limit/image.jpg 2560w, https://example.com/w_1280,c_limit/image.jpg 1280w",
    );
    assert_eq!(parsed.len(), 2);
    assert_eq!(
      parsed[0].url,
      "https://example.com/w_2560,c_limit/image.jpg"
    );
    assert!(matches!(
      parsed[0].descriptor,
      SrcsetDescriptor::Width(2560)
    ));
    assert_eq!(
      parsed[1].url,
      "https://example.com/w_1280,c_limit/image.jpg"
    );
    assert!(matches!(
      parsed[1].descriptor,
      SrcsetDescriptor::Width(1280)
    ));
  }

  #[test]
  fn parse_srcset_parses_urls_without_descriptors() {
    let parsed = parse_srcset("a.png, b.png");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].url, "a.png");
    assert!(matches!(parsed[0].descriptor, SrcsetDescriptor::Density(d) if d == 1.0));
    assert_eq!(parsed[1].url, "b.png");
    assert!(matches!(parsed[1].descriptor, SrcsetDescriptor::Density(d) if d == 1.0));
  }

  #[test]
  fn parse_srcset_parses_urls_without_descriptors_without_whitespace() {
    let parsed = parse_srcset("a.png,b.png");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].url, "a.png");
    assert!(matches!(parsed[0].descriptor, SrcsetDescriptor::Density(d) if d == 1.0));
    assert_eq!(parsed[1].url, "b.png");
    assert!(matches!(parsed[1].descriptor, SrcsetDescriptor::Density(d) if d == 1.0));
  }

  #[test]
  fn parse_srcset_parses_data_urls() {
    let parsed = parse_srcset("data:image/png;base64,abcd 1x, b.png 2x");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].url, "data:image/png;base64,abcd");
    assert!(matches!(parsed[0].descriptor, SrcsetDescriptor::Density(d) if d == 1.0));
    assert_eq!(parsed[1].url, "b.png");
    assert!(matches!(parsed[1].descriptor, SrcsetDescriptor::Density(d) if d == 2.0));
  }

  #[test]
  fn parse_srcset_allows_commas_in_query_params() {
    let parsed = parse_srcset(
      "https://img.example/foo.jpg?resize=300,163 300w, https://img.example/bar.jpg 600w",
    );
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].url, "https://img.example/foo.jpg?resize=300,163");
    assert!(matches!(parsed[0].descriptor, SrcsetDescriptor::Width(300)));
    assert_eq!(parsed[1].url, "https://img.example/bar.jpg");
    assert!(matches!(parsed[1].descriptor, SrcsetDescriptor::Width(600)));
  }

  #[test]
  fn parse_srcset_allows_multiple_commas_in_query_numeric_lists() {
    let parsed = parse_srcset(
      "https://img.example/foo.jpg?rect=0,0,100,100 1x, https://img.example/bar.jpg 2x",
    );
    assert_eq!(parsed.len(), 2);
    assert_eq!(
      parsed[0].url,
      "https://img.example/foo.jpg?rect=0,0,100,100"
    );
    assert!(matches!(parsed[0].descriptor, SrcsetDescriptor::Density(d) if d == 1.0));
    assert_eq!(parsed[1].url, "https://img.example/bar.jpg");
    assert!(matches!(parsed[1].descriptor, SrcsetDescriptor::Density(d) if d == 2.0));
  }

  #[test]
  fn parse_srcset_allows_commas_in_amazon_crop_rect_paths() {
    let parsed = parse_srcset(
      "https://img.example/foo_UX414_CR0,0,414,612_AL_.jpg 1x,https://img.example/bar.jpg 2x",
    );
    assert_eq!(parsed.len(), 2);
    assert_eq!(
      parsed[0].url,
      "https://img.example/foo_UX414_CR0,0,414,612_AL_.jpg"
    );
    assert!(matches!(parsed[0].descriptor, SrcsetDescriptor::Density(d) if d == 1.0));
    assert_eq!(parsed[1].url, "https://img.example/bar.jpg");
    assert!(matches!(parsed[1].descriptor, SrcsetDescriptor::Density(d) if d == 2.0));
  }

  #[test]
  fn parse_srcset_allows_commas_in_filenames_with_percent_encoded_space() {
    let parsed = parse_srcset(
      "https://media.example/Artist%20-%20Title,%202023.jpg 120w, https://media.example/other.jpg 240w",
    );
    assert_eq!(parsed.len(), 2);
    assert_eq!(
      parsed[0].url,
      "https://media.example/Artist%20-%20Title,%202023.jpg"
    );
    assert!(matches!(parsed[0].descriptor, SrcsetDescriptor::Width(120)));
    assert_eq!(parsed[1].url, "https://media.example/other.jpg");
    assert!(matches!(parsed[1].descriptor, SrcsetDescriptor::Width(240)));
  }

  #[test]
  fn parse_sizes_parses_lengths_and_media_conditions() {
    let parsed = parse_sizes("(max-width: 600px) 50vw, 100vw").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 2);
    assert!(parsed.entries[0].media.is_some());
    assert_eq!(parsed.entries[0].length, Length::new(50.0, LengthUnit::Vw).into());
    assert!(parsed.entries[1].media.is_none());
    assert_eq!(
      parsed.entries[1].length,
      Length::new(100.0, LengthUnit::Vw).into()
    );
  }

  #[test]
  fn parse_sizes_supports_modern_viewport_units() {
    let parsed = parse_sizes("50SVW, 100lvw").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 2);
    assert_eq!(parsed.entries[0].length, Length::new(50.0, LengthUnit::Vw).into());
    assert_eq!(
      parsed.entries[1].length,
      Length::new(100.0, LengthUnit::Vw).into()
    );
  }

  #[test]
  fn parse_sizes_parses_calc_with_spaces() {
    let parsed = parse_sizes("calc(100vw - 20px)").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 1);
    assert!(parsed.entries[0].media.is_none());
    let SizesLength::Length(len) = &parsed.entries[0].length else {
      panic!("expected calc() to parse as a length");
    };
    assert_eq!(len.unit, LengthUnit::Calc);
    assert!(len.calc.is_some());
  }

  #[test]
  fn parse_sizes_supports_calc_with_nested_min_function() {
    let parsed = parse_sizes("calc(min(100vw, 80px) - 20px), 100vw").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 2);
    assert_eq!(
      parsed.entries[0].length,
      SizesLength::Sub(
        Box::new(SizesLength::Min(vec![
          Length::new(100.0, LengthUnit::Vw).into(),
          Length::px(80.0).into(),
        ])),
        Box::new(Length::px(20.0).into()),
      )
    );
    assert_eq!(
      parsed.entries[1].length,
      Length::new(100.0, LengthUnit::Vw).into()
    );
  }

  #[test]
  fn parse_sizes_parses_min_max_clamp_with_commas() {
    let parsed = parse_sizes("min(100vw, 500px)").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 1);
    assert!(parsed.entries[0].media.is_none());
    assert_eq!(
      parsed.entries[0].length,
      SizesLength::Min(vec![
        Length::new(100.0, LengthUnit::Vw).into(),
        Length::px(500.0).into(),
      ])
    );

    let parsed = parse_sizes("clamp(10px, 50vw, 300px)").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 1);
    assert!(parsed.entries[0].media.is_none());
    let SizesLength::Clamp {
      min,
      preferred,
      max,
    } = &parsed.entries[0].length
    else {
      panic!("expected clamp() to parse");
    };
    assert!(matches!(
      min.as_ref(),
      SizesLength::Length(len) if len.unit == LengthUnit::Px && (len.value - 10.0).abs() < f32::EPSILON
    ));
    assert!(matches!(
      preferred.as_ref(),
      SizesLength::Length(len) if len.unit == LengthUnit::Vw && (len.value - 50.0).abs() < f32::EPSILON
    ));
    assert!(matches!(
      max.as_ref(),
      SizesLength::Length(len) if len.unit == LengthUnit::Px && (len.value - 300.0).abs() < f32::EPSILON
    ));
  }

  #[test]
  fn parse_sizes_parses_media_with_calc_length() {
    let parsed =
      parse_sizes("(max-width: 600px) calc(100vw - 20px), 100vw").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 2);
    assert!(parsed.entries[0].media.is_some());
    let SizesLength::Length(len) = &parsed.entries[0].length else {
      panic!("expected calc() to parse as a length");
    };
    assert_eq!(len.unit, LengthUnit::Calc);
    assert!(len.calc.is_some());
    assert!(parsed.entries[1].media.is_none());
    assert_eq!(
      parsed.entries[1].length,
      Length::new(100.0, LengthUnit::Vw).into()
    );
  }

  #[test]
  fn parse_sizes_supports_calc_with_internal_whitespace() {
    let parsed =
      parse_sizes("(max-width: 600px) calc(50vw - 20px), 100vw").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 2);
    assert!(parsed.entries[0].media.is_some());
    let SizesLength::Length(len) = &parsed.entries[0].length else {
      panic!("expected calc() to parse as a length");
    };
    assert_eq!(len.unit, LengthUnit::Calc);
    assert!(len.calc.is_some());
    assert!(parsed.entries[1].media.is_none());
    assert_eq!(
      parsed.entries[1].length,
      Length::new(100.0, LengthUnit::Vw).into()
    );
  }

  #[test]
  fn parse_sizes_supports_clamp_with_internal_commas() {
    let parsed =
      parse_sizes("clamp(10px, calc(50vw - 20px), 300px), 100vw").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 2);
    assert!(parsed.entries[0].media.is_none());
    let SizesLength::Clamp {
      min,
      preferred,
      max,
    } = &parsed.entries[0].length
    else {
      panic!("expected clamp() to parse");
    };
    assert!(matches!(
      min.as_ref(),
      SizesLength::Length(len) if len.unit == LengthUnit::Px && (len.value - 10.0).abs() < f32::EPSILON
    ));
    assert!(matches!(
      max.as_ref(),
      SizesLength::Length(len) if len.unit == LengthUnit::Px && (len.value - 300.0).abs() < f32::EPSILON
    ));
    match preferred.as_ref() {
      SizesLength::Length(len) => {
        assert_eq!(len.unit, LengthUnit::Calc);
        assert!(len.calc.is_some());
      }
      _ => panic!("expected clamp() preferred value to be a length"),
    }
    assert!(parsed.entries[1].media.is_none());
    assert_eq!(
      parsed.entries[1].length,
      Length::new(100.0, LengthUnit::Vw).into()
    );
  }

  #[test]
  fn parse_sizes_supports_min_with_mixed_units() {
    let parsed = parse_sizes("min(100vw, 80px), 100vw").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 2);
    let SizesLength::Min(values) = &parsed.entries[0].length else {
      panic!("expected min() to parse");
    };
    assert_eq!(
      values.as_slice(),
      &[
        Length::new(100.0, LengthUnit::Vw).into(),
        Length::px(80.0).into(),
      ]
    );
    assert_eq!(
      parsed.entries[1].length,
      Length::new(100.0, LengthUnit::Vw).into()
    );
  }

  #[test]
  fn parse_sizes_skips_invalid_lengths() {
    let parsed = parse_sizes("bad, 100vw").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 1);
    assert_eq!(
      parsed.entries[0].length,
      Length::new(100.0, LengthUnit::Vw).into()
    );
  }

  #[test]
  fn parse_sizes_splits_media_and_calc_length_like_browsers() {
    let parsed =
      parse_sizes("(max-width: 600px) calc(50vw - 10px), 100vw").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 2);
    assert!(parsed.entries[0].media.is_some());
    let SizesLength::Length(len) = &parsed.entries[0].length else {
      panic!("expected calc() to parse as a length");
    };
    assert_eq!(len.unit, LengthUnit::Calc);
    assert!(len.calc.is_some());
    assert_eq!(
      parsed.entries[1].length,
      Length::new(100.0, LengthUnit::Vw).into()
    );
  }

  #[test]
  fn parse_sizes_does_not_split_commas_inside_clamp() {
    let parsed = parse_sizes("clamp(200px, 50vw, 400px)").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 1);
    let SizesLength::Clamp {
      min,
      preferred,
      max,
    } = &parsed.entries[0].length
    else {
      panic!("expected clamp() to parse");
    };
    assert!(matches!(
      min.as_ref(),
      SizesLength::Length(len) if len.unit == LengthUnit::Px && (len.value - 200.0).abs() < f32::EPSILON
    ));
    assert!(matches!(
      preferred.as_ref(),
      SizesLength::Length(len) if len.unit == LengthUnit::Vw && (len.value - 50.0).abs() < f32::EPSILON
    ));
    assert!(matches!(
      max.as_ref(),
      SizesLength::Length(len) if len.unit == LengthUnit::Px && (len.value - 400.0).abs() < f32::EPSILON
    ));
  }
}
