//! Parsing helpers for responsive image HTML attributes (`srcset` / `sizes`).
//!
//! These helpers are shared by the renderer (box generation / replaced elements)
//! and developer tooling (e.g. asset prefetch) so both paths interpret author
//! markup consistently.

use crate::tree::box_tree::{SizesEntry, SizesList, SrcsetCandidate, SrcsetDescriptor};
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

    let mut descriptor: Option<SrcsetDescriptor> = None;
    let mut valid = true;
    for desc in desc_str.split_whitespace() {
      if descriptor.is_some() {
        valid = false;
        break;
      }
      let d = desc.trim();
      if let Some(raw) = d.strip_suffix('x') {
        if let Ok(val) = raw.parse::<f32>() {
          descriptor = Some(SrcsetDescriptor::Density(val));
        }
      } else if let Some(raw) = d.strip_suffix("dppx") {
        if let Ok(val) = raw.parse::<f32>() {
          descriptor = Some(SrcsetDescriptor::Density(val));
        }
      } else if let Some(raw) = d.strip_suffix('w') {
        if let Ok(val) = raw.parse::<u32>() {
          descriptor = Some(SrcsetDescriptor::Width(val));
        }
      }
    }
    if valid {
      out.push(SrcsetCandidate {
        url: url.to_string(),
        descriptor: descriptor.unwrap_or(SrcsetDescriptor::Density(1.0)),
      });
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

fn parse_sizes_length(value: &str) -> Option<crate::style::values::Length> {
  use crate::css::properties::parse_calc_function_length;
  use crate::css::properties::parse_clamp_function_length;
  use crate::css::properties::parse_min_max_function_length;
  use crate::css::properties::MathFn;
  use crate::style::values::Length;
  use crate::style::values::LengthUnit;

  fn parse_strict(value: &str) -> Option<Length> {
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
      }
      Ok(Token::Function(ref name)) if name.eq_ignore_ascii_case("calc") => {
        parse_calc_function_length(&mut parser).ok()
      }
      Ok(Token::Function(ref name)) if name.eq_ignore_ascii_case("min") => {
        parse_min_max_function_length(&mut parser, MathFn::Min).ok()
      }
      Ok(Token::Function(ref name)) if name.eq_ignore_ascii_case("max") => {
        parse_min_max_function_length(&mut parser, MathFn::Max).ok()
      }
      Ok(Token::Function(ref name)) if name.eq_ignore_ascii_case("clamp") => {
        parse_clamp_function_length(&mut parser).ok()
      }
      Ok(Token::Percentage { unit_value, .. }) => Some(Length::percent(*unit_value * 100.0)),
      Ok(Token::Number { value, .. }) if *value == 0.0 => Some(Length::px(0.0)),
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

  fn extract_function_args<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    if !value.ends_with(')') {
      return None;
    }
    if value.len() <= name.len() + 2 {
      return None;
    }
    if !value
      .get(..name.len())
      .is_some_and(|prefix| prefix.eq_ignore_ascii_case(name))
    {
      return None;
    }
    if value.as_bytes()[name.len()] != b'(' {
      return None;
    }
    value.get(name.len() + 1..value.len() - 1)
  }

  fn parse_lenient_math_function(value: &str) -> Option<Length> {
    let value = value.trim();

    // When the core CSS parser cannot represent the math function (e.g., `min(100vw, 500px)` with
    // mixed units), fall back to parsing one argument so the `sizes` entry isn't discarded.
    for (func, preferred_index) in [("min", 0usize), ("max", 0usize), ("clamp", 1usize)] {
      let Some(args_str) = extract_function_args(value, func) else {
        continue;
      };

      let args = split_top_level_commas(args_str);
      let candidates = [preferred_index, 0, 1, 2];
      let mut tried = [false; 4];
      for idx in candidates {
        if idx >= tried.len() || tried[idx] {
          continue;
        }
        tried[idx] = true;
        let Some(arg) = args.get(idx) else {
          continue;
        };
        let arg = arg.trim();
        if arg.is_empty() {
          continue;
        }
        if let Some(len) = parse_sizes_length(arg) {
          return Some(len);
        }
      }

      return None;
    }

    None
  }

  let value = value.trim();
  if value.is_empty() {
    return None;
  }

  parse_strict(value).or_else(|| parse_lenient_math_function(value))
}

#[cfg(test)]
mod tests {
  use super::{parse_sizes, parse_srcset};
  use crate::style::values::{Length, LengthUnit};
  use crate::tree::box_tree::SrcsetDescriptor;

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
  fn parse_srcset_parses_width_descriptors() {
    let parsed = parse_srcset("a.png 320w, b.png 640w");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].url, "a.png");
    assert!(matches!(parsed[0].descriptor, SrcsetDescriptor::Width(320)));
    assert!(matches!(parsed[1].descriptor, SrcsetDescriptor::Width(640)));
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
    assert_eq!(parsed.entries[0].length, Length::new(50.0, LengthUnit::Vw));
    assert!(parsed.entries[1].media.is_none());
    assert_eq!(parsed.entries[1].length, Length::new(100.0, LengthUnit::Vw));
  }

  #[test]
  fn parse_sizes_supports_modern_viewport_units() {
    let parsed = parse_sizes("50SVW, 100lvw").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 2);
    assert_eq!(parsed.entries[0].length, Length::new(50.0, LengthUnit::Vw));
    assert_eq!(parsed.entries[1].length, Length::new(100.0, LengthUnit::Vw));
  }

  #[test]
  fn parse_sizes_parses_calc_with_spaces() {
    let parsed = parse_sizes("calc(100vw - 20px)").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 1);
    assert!(parsed.entries[0].media.is_none());
    assert_eq!(parsed.entries[0].length.unit, LengthUnit::Calc);
    assert!(parsed.entries[0].length.calc.is_some());
  }

  #[test]
  fn parse_sizes_parses_min_max_clamp_with_commas() {
    let parsed = parse_sizes("min(100vw, 500px)").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 1);
    assert!(parsed.entries[0].media.is_none());

    let parsed = parse_sizes("clamp(10px, 50vw, 300px)").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 1);
    assert!(parsed.entries[0].media.is_none());
  }

  #[test]
  fn parse_sizes_parses_media_with_calc_length() {
    let parsed =
      parse_sizes("(max-width: 600px) calc(100vw - 20px), 100vw").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 2);
    assert!(parsed.entries[0].media.is_some());
    assert_eq!(parsed.entries[0].length.unit, LengthUnit::Calc);
    assert!(parsed.entries[0].length.calc.is_some());
    assert!(parsed.entries[1].media.is_none());
    assert_eq!(parsed.entries[1].length, Length::new(100.0, LengthUnit::Vw));
  }

  #[test]
  fn parse_sizes_supports_calc_with_internal_whitespace() {
    let parsed =
      parse_sizes("(max-width: 600px) calc(50vw - 20px), 100vw").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 2);
    assert!(parsed.entries[0].media.is_some());
    assert_eq!(parsed.entries[0].length.unit, LengthUnit::Calc);
    assert!(parsed.entries[0].length.calc.is_some());
    assert!(parsed.entries[1].media.is_none());
    assert_eq!(parsed.entries[1].length, Length::new(100.0, LengthUnit::Vw));
  }

  #[test]
  fn parse_sizes_supports_clamp_with_internal_commas() {
    let parsed =
      parse_sizes("clamp(10px, calc(50vw - 20px), 300px), 100vw").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 2);
    assert!(parsed.entries[0].media.is_none());
    assert_eq!(parsed.entries[0].length.unit, LengthUnit::Calc);
    assert!(parsed.entries[0].length.calc.is_some());
    assert!(parsed.entries[1].media.is_none());
    assert_eq!(parsed.entries[1].length, Length::new(100.0, LengthUnit::Vw));
  }

  #[test]
  fn parse_sizes_skips_invalid_lengths() {
    let parsed = parse_sizes("bad, 100vw").expect("sizes parsed");
    assert_eq!(parsed.entries.len(), 1);
    assert_eq!(parsed.entries[0].length, Length::new(100.0, LengthUnit::Vw));
  }
}
