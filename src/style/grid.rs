//! CSS Grid Track Parsing
//!
//! This module handles parsing CSS Grid layout values including
//! track definitions, named lines, and grid placement.
//!
//! Reference: CSS Grid Layout Module Level 1
//! <https://www.w3.org/TR/css-grid-1/>

use crate::css::properties::parse_property_value;
use crate::css::types::PropertyValue;
use crate::style::types::GridTrack;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use cssparser::Parser;
use cssparser::ParserInput;
use cssparser::Token;
use std::collections::HashMap;
use std::hash::BuildHasher;

fn is_css_ascii_whitespace(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(is_css_ascii_whitespace)
}

fn strip_css_comments_to_whitespace(value: &str) -> std::borrow::Cow<'_, str> {
  if !value.contains("/*") {
    return std::borrow::Cow::Borrowed(value);
  }

  let mut out = String::with_capacity(value.len());
  let mut idx = 0usize;
  while let Some(rel_start) = value.get(idx..).and_then(|s| s.find("/*")) {
    let start = idx + rel_start;
    out.push_str(value.get(idx..start).unwrap_or(""));
    out.push(' ');
    let after_start = start + "/*".len();
    let Some(rel_end) = value.get(after_start..).and_then(|s| s.find("*/")) else {
      idx = value.len();
      break;
    };
    idx = after_start + rel_end + "*/".len();
  }
  out.push_str(value.get(idx..).unwrap_or(""));
  std::borrow::Cow::Owned(out)
}

fn split_ascii_whitespace(value: &str) -> impl Iterator<Item = &str> {
  value
    .split(is_css_ascii_whitespace)
    .filter(|part| !part.is_empty())
}

fn tokenize_ms_grid_track_list(value: &str) -> Vec<&str> {
  let bytes = value.as_bytes();
  let mut tokens: Vec<&str> = Vec::new();
  let mut token_start: Option<usize> = None;
  let mut paren = 0i32;
  let mut bracket = 0i32;
  let mut brace = 0i32;
  let mut in_string: Option<u8> = None;
  let mut escape = false;

  let mut idx = 0usize;
  while idx < bytes.len() {
    let b = bytes[idx];

    if escape {
      escape = false;
      idx += 1;
      continue;
    }

    if b == b'\\' {
      token_start.get_or_insert(idx);
      escape = true;
      idx += 1;
      continue;
    }

    if let Some(q) = in_string {
      token_start.get_or_insert(idx);
      if b == q {
        in_string = None;
      }
      idx += 1;
      continue;
    }

    // Treat CSS comments as whitespace at the top-level so tokens like `1fr/*x*/1fr` are not
    // concatenated.
    if b == b'/' && bytes.get(idx + 1) == Some(&b'*') {
      if paren == 0 && bracket == 0 && brace == 0 {
        if let Some(start) = token_start.take() {
          let slice = trim_ascii_whitespace(&value[start..idx]);
          if !slice.is_empty() {
            tokens.push(slice);
          }
        }
      } else {
        token_start.get_or_insert(idx);
      }

      idx += 2;
      while idx + 1 < bytes.len() && !(bytes[idx] == b'*' && bytes[idx + 1] == b'/') {
        idx += 1;
      }
      idx = idx.saturating_add(2).min(bytes.len());
      continue;
    }

    match b {
      b'"' | b'\'' => {
        token_start.get_or_insert(idx);
        in_string = Some(b);
      }
      b'(' => {
        token_start.get_or_insert(idx);
        paren += 1;
      }
      b')' => {
        token_start.get_or_insert(idx);
        paren -= 1;
      }
      b'[' => {
        token_start.get_or_insert(idx);
        bracket += 1;
      }
      b']' => {
        token_start.get_or_insert(idx);
        bracket -= 1;
      }
      b'{' => {
        token_start.get_or_insert(idx);
        brace += 1;
      }
      b'}' => {
        token_start.get_or_insert(idx);
        brace -= 1;
      }
      b if b.is_ascii_whitespace() && paren == 0 && bracket == 0 && brace == 0 => {
        if let Some(start) = token_start.take() {
          let slice = trim_ascii_whitespace(&value[start..idx]);
          if !slice.is_empty() {
            tokens.push(slice);
          }
        }
      }
      _ => {
        token_start.get_or_insert(idx);
      }
    }

    idx += 1;
  }

  if let Some(start) = token_start.take() {
    let slice = trim_ascii_whitespace(&value[start..]);
    if !slice.is_empty() {
      tokens.push(slice);
    }
  }

  tokens
}

fn try_parse_ms_grid_repeat_token(token: &str) -> Option<(String, usize)> {
  let token = trim_ascii_whitespace(token);
  if !token.starts_with('(') {
    return None;
  }

  // Find the matching `)` for the opening `(`, accounting for nested parentheses inside functions
  // like `calc()`/`minmax()`.
  let mut depth = 0i32;
  let mut in_string: Option<char> = None;
  let mut escape = false;
  let mut end_paren: Option<usize> = None;
  for (idx, ch) in token.char_indices() {
    if escape {
      escape = false;
      continue;
    }
    if ch == '\\' {
      escape = true;
      continue;
    }
    if let Some(q) = in_string {
      if ch == q {
        in_string = None;
      }
      continue;
    }
    if ch == '"' || ch == '\'' {
      in_string = Some(ch);
      continue;
    }

    match ch {
      '(' => depth += 1,
      ')' => {
        depth -= 1;
        if depth == 0 {
          end_paren = Some(idx);
          break;
        }
      }
      _ => {}
    }
  }

  let end_paren = end_paren?;
  let pattern = trim_ascii_whitespace(&token[1..end_paren]);
  if pattern.is_empty() {
    return None;
  }

  let rest = trim_ascii_whitespace(&token[end_paren + ')'.len_utf8()..]);
  if !(rest.starts_with('[') && rest.ends_with(']')) {
    return None;
  }
  let count_str = trim_ascii_whitespace(&rest[1..rest.len() - ']'.len_utf8()]);
  if count_str.is_empty() || !count_str.chars().all(|c| c.is_ascii_digit()) {
    return None;
  }
  let count: usize = count_str.parse().ok()?;
  if count == 0 {
    return None;
  }

  Some((pattern.to_string(), count))
}

/// Normalize legacy IE `-ms-grid-columns`/`-ms-grid-rows` track lists into a modern track list
/// syntax that FastRender can parse.
///
/// Autoprefixer (and some authoring tools) emit IE10/11 grid track repeats using the old
/// `(pattern)[count]` syntax (e.g. `(1fr)[2]`). CSS Grid Level 1 uses `repeat(count, pattern)`
/// instead. FastRender's track parser implements the modern syntax only, so we expand the legacy
/// repeat syntax into an equivalent explicit list.
pub(crate) fn normalize_ms_grid_track_list(value: &str) -> Option<String> {
  let value = trim_ascii_whitespace(value);
  if value.is_empty() {
    return None;
  }

  let tokens = tokenize_ms_grid_track_list(value);
  if tokens.is_empty() {
    return None;
  }

  let mut out_parts: Vec<String> = Vec::new();
  for token in tokens {
    if let Some((pattern, count)) = try_parse_ms_grid_repeat_token(token) {
      for _ in 0..count {
        out_parts.push(pattern.clone());
      }
    } else {
      out_parts.push(token.to_string());
    }
  }

  if out_parts.is_empty() {
    return None;
  }

  Some(out_parts.join(" "))
}

/// Parse grid-template-columns/rows into track list with named lines
///
/// Handles syntax like: `[text-start] 1fr [text-end sidebar-start] 300px [sidebar-end]`
pub fn parse_grid_tracks_with_names(
  tracks_str: &str,
) -> (
  Vec<GridTrack>,
  HashMap<String, Vec<usize>>,
  Vec<Vec<String>>,
) {
  let ParsedTracks {
    tracks,
    named_lines,
    line_names,
  } = parse_track_list(tracks_str);

  (tracks, named_lines, line_names)
}

/// Parse a single grid line reference (e.g., "text-start", "3", "auto")
pub fn parse_grid_line<S: BuildHasher>(
  value: &str,
  named_lines: &HashMap<String, Vec<usize>, S>,
) -> i32 {
  let value = trim_ascii_whitespace(value);

  // Try parsing as integer first
  if let Ok(n) = value.parse::<i32>() {
    return n;
  }

  // Check if it's "auto"
  if value.eq_ignore_ascii_case("auto") {
    return 0; // 0 means auto-placement in Taffy
  }

  // Try to resolve as named grid line
  if let Some(positions) = named_lines.get(value) {
    if let Some(&pos) = positions.first() {
      // Grid lines are 1-indexed in CSS (line 1 is before track 0)
      return (pos + 1) as i32;
    }
  }

  // Default to auto
  0
}

pub(crate) fn parse_grid_auto_flow_value(value: &str) -> Option<crate::style::types::GridAutoFlow> {
  let value = trim_ascii_whitespace(value);
  if value.is_empty() {
    return None;
  }

  let mut input = ParserInput::new(value);
  let mut parser = Parser::new(&mut input);

  let mut saw_primary = false;
  let mut has_column = false;
  let mut dense = false;

  while let Ok(token) = parser.next_including_whitespace_and_comments() {
    match token {
      Token::WhiteSpace(_) | Token::Comment(_) => continue,
      Token::Ident(ident) => {
        let ident = ident.as_ref();
        if ident.eq_ignore_ascii_case("dense") {
          if dense {
            return None;
          }
          dense = true;
          continue;
        }

        if ident.eq_ignore_ascii_case("row") {
          if saw_primary {
            return None;
          }
          saw_primary = true;
          has_column = false;
          continue;
        }

        if ident.eq_ignore_ascii_case("column") {
          if saw_primary {
            return None;
          }
          saw_primary = true;
          has_column = true;
          continue;
        }

        return None;
      }
      _ => return None,
    }
  }

  if !saw_primary && !dense {
    return None;
  }

  Some(match (has_column, dense) {
    (false, false) => crate::style::types::GridAutoFlow::Row,
    (false, true) => crate::style::types::GridAutoFlow::RowDense,
    (true, false) => crate::style::types::GridAutoFlow::Column,
    (true, true) => crate::style::types::GridAutoFlow::ColumnDense,
  })
}

/// Parse `grid-template-areas` into row/column names, validating rectangular areas.
///
/// Returns `None` on syntax errors or non-rectangular area definitions. The rows are stored with `None`
/// representing an empty cell (`.` in authored CSS).
pub fn parse_grid_template_areas(value: &str) -> Option<Vec<Vec<Option<String>>>> {
  let trimmed = trim_ascii_whitespace(value);
  if trimmed.eq_ignore_ascii_case("none") {
    return Some(Vec::new());
  }

  let mut input = ParserInput::new(trimmed);
  let mut parser = Parser::new(&mut input);
  let mut rows: Vec<Vec<Option<String>>> = Vec::new();

  while !parser.is_exhausted() {
    match parser.next_including_whitespace() {
      Ok(Token::WhiteSpace(_)) => continue,
      Ok(Token::QuotedString(s)) => {
        let cols: Vec<Option<String>> = s
          .split(is_css_ascii_whitespace)
          .filter(|part| !part.is_empty())
          .map(|name| {
            if name == "." {
              None
            } else {
              Some(name.to_string())
            }
          })
          .collect();
        if cols.is_empty() {
          return None;
        }
        if let Some(expected) = rows.first().map(|r| r.len()) {
          if expected != cols.len() {
            return None;
          }
        }
        rows.push(cols);
      }
      Ok(_) => return None,
      Err(_) => break,
    }
  }

  if rows.is_empty() {
    return None;
  }

  validate_area_rectangles(&rows).map(|_| rows)
}

/// Validate that each named area forms a rectangle and return area bounds per name.
///
/// Note: The returned [`HashMap`] has nondeterministic iteration order; callers must not rely on
/// it for stable output (sort the entries first if ordering matters).
pub fn validate_area_rectangles(
  rows: &[Vec<Option<String>>],
) -> Option<HashMap<String, (usize, usize, usize, usize)>> {
  let mut bounds: HashMap<String, (usize, usize, usize, usize)> = HashMap::new();

  for (row_idx, row) in rows.iter().enumerate() {
    for (col_idx, cell) in row.iter().enumerate() {
      let Some(name) = cell else { continue };
      let entry = bounds
        .entry(name.clone())
        .or_insert((row_idx, row_idx, col_idx, col_idx));
      let (top, bottom, left, right) = entry;
      *top = (*top).min(row_idx);
      *bottom = (*bottom).max(row_idx);
      *left = (*left).min(col_idx);
      *right = (*right).max(col_idx);
    }
  }

  for (name, (top, bottom, left, right)) in bounds.iter() {
    for r in *top..=*bottom {
      for c in *left..=*right {
        match rows
          .get(r)
          .and_then(|row| row.get(c))
          .and_then(|cell| cell.as_ref())
        {
          Some(cell_name) if cell_name == name => {}
          _ => return None,
        }
      }
    }
  }

  Some(bounds)
}

/// Parsed representation of the `grid-template` shorthand.
pub struct ParsedGridTemplate {
  pub areas: Option<Vec<Vec<Option<String>>>>,
  pub row_tracks: Option<(Vec<GridTrack>, Vec<Vec<String>>)>,
  pub column_tracks: Option<(Vec<GridTrack>, Vec<Vec<String>>)>,
  /// Whether the row axis uses `subgrid` instead of an explicit track list.
  pub row_is_subgrid: bool,
  /// Whether the column axis uses `subgrid` instead of an explicit track list.
  pub col_is_subgrid: bool,
  /// Optional author-provided line name lists that accompany `subgrid` for rows.
  pub row_subgrid_line_names: Option<Vec<Vec<String>>>,
  /// Optional author-provided line name lists that accompany `subgrid` for columns.
  pub col_subgrid_line_names: Option<Vec<Vec<String>>>,
}

/// Parsed representation of the `grid` shorthand.
pub struct ParsedGridShorthand {
  pub template: Option<ParsedGridTemplate>,
  pub auto_rows: Option<Vec<GridTrack>>,
  pub auto_columns: Option<Vec<GridTrack>>,
  pub auto_flow: Option<crate::style::types::GridAutoFlow>,
}

/// Parse the `grid-template` shorthand.
///
/// Supports two forms:
/// 1) `<track-list> / <track-list>` (explicit rows/columns)
/// 2) `<area-rows> [ / <col-tracks> ]`, where area rows are quoted strings with optional
///    per-row track sizes following each string.
pub fn parse_grid_template_shorthand(value: &str) -> Option<ParsedGridTemplate> {
  let value = trim_ascii_whitespace(value);
  if value.eq_ignore_ascii_case("none") {
    return Some(ParsedGridTemplate {
      areas: Some(Vec::new()),
      row_tracks: Some((Vec::new(), Vec::new())),
      column_tracks: Some((Vec::new(), Vec::new())),
      row_is_subgrid: false,
      col_is_subgrid: false,
      row_subgrid_line_names: None,
      col_subgrid_line_names: None,
    });
  }

  let (main, cols_part) = split_once_unquoted(value, '/');

  let main = trim_ascii_whitespace(main);
  let cols_part = cols_part.and_then(|s| {
    let trimmed = trim_ascii_whitespace(s);
    if trimmed.is_empty() {
      None
    } else {
      Some(trimmed)
    }
  });

  // If the shorthand does not start with a quoted area row, treat as the track-list form
  // (`<track-list> / <track-list>`).
  //
  // Note: `<string>` tokens in grid-template-areas may use either double quotes or single quotes.
  let main_starts_with_quote = matches!(main.as_bytes().first(), Some(b'"' | b'\''));
  if !main_starts_with_quote {
    // Per spec the track-list form requires both rows and columns separated by a slash.
    let cols_raw = cols_part?;
    let (row_tracks, row_line_names, row_is_subgrid, row_subgrid_line_names) =
      match parse_subgrid_line_names(main) {
        Some(line_names) => (None, None, true, Some(line_names)),
        None => {
          let ParsedTracks {
            tracks, line_names, ..
          } = parse_track_list(main);
          if tracks.is_empty() {
            return None;
          }
          (Some(tracks), Some(line_names), false, None)
        }
      };

    let (col_tracks, col_line_names, col_is_subgrid, col_subgrid_line_names) =
      match parse_subgrid_line_names(cols_raw) {
        Some(line_names) => (None, None, true, Some(line_names)),
        None => {
          let ParsedTracks {
            tracks, line_names, ..
          } = parse_track_list(cols_raw);
          if tracks.is_empty() {
            return None;
          }
          (Some(tracks), Some(line_names), false, None)
        }
      };

    return Some(ParsedGridTemplate {
      areas: None,
      row_tracks: row_tracks.zip(row_line_names),
      column_tracks: col_tracks.zip(col_line_names),
      row_is_subgrid,
      col_is_subgrid,
      row_subgrid_line_names,
      col_subgrid_line_names,
    });
  }

  // Area form: parse quoted rows with optional row sizes.
  let (area_rows, row_sizes_raw) = parse_area_rows_with_sizes(main)?;
  let areas = build_area_matrix(&area_rows)?;
  validate_area_rectangles(&areas)?;

  // Row tracks: if row sizes were provided, use them; otherwise default to auto.
  let row_tracks = if row_sizes_raw.iter().any(|s| s.is_some()) {
    let mut tracks = Vec::with_capacity(row_sizes_raw.len());
    for size in &row_sizes_raw {
      let Some(size_str) = size else {
        tracks.push(GridTrack::Auto);
        continue;
      };
      let track = parse_single_grid_track(&size_str)?;
      tracks.push(track);
    }
    Some((tracks, vec![Vec::new(); row_sizes_raw.len() + 1]))
  } else {
    Some((
      vec![GridTrack::Auto; areas.len()],
      vec![Vec::new(); areas.len() + 1],
    ))
  };

  // Column tracks: explicit slash wins; otherwise derive auto from area width.
  let (column_tracks, col_is_subgrid, col_subgrid_line_names) = if let Some(cols_raw) = cols_part {
    if let Some(names) = parse_subgrid_line_names(cols_raw) {
      (None, true, Some(names))
    } else {
      let ParsedTracks {
        tracks, line_names, ..
      } = parse_track_list(cols_raw);
      if tracks.is_empty() {
        return None;
      }
      (Some((tracks, line_names)), false, None)
    }
  } else {
    let cols = areas.first().map(|r| r.len()).unwrap_or(0);
    (
      Some((vec![GridTrack::Auto; cols], vec![Vec::new(); cols + 1])),
      false,
      None,
    )
  };

  Some(ParsedGridTemplate {
    areas: Some(areas),
    row_tracks,
    column_tracks,
    row_is_subgrid: false,
    col_is_subgrid,
    row_subgrid_line_names: None,
    col_subgrid_line_names,
  })
}

fn contains_grid_auto_flow_keyword(input: &str) -> bool {
  let bytes = input.as_bytes();
  let mut bracket_depth: usize = 0;
  let mut paren_depth: usize = 0;
  let mut in_string: Option<u8> = None;
  let mut escape = false;
  let mut i = 0usize;
  while i < bytes.len() {
    let byte = bytes[i];
    if let Some(quote) = in_string {
      if escape {
        escape = false;
        i += 1;
        continue;
      }
      if byte == b'\\' {
        escape = true;
        i += 1;
        continue;
      }
      if byte == quote {
        in_string = None;
      }
      i += 1;
      continue;
    }
    match byte {
      b'"' | b'\'' => in_string = Some(byte),
      b'(' => paren_depth += 1,
      b')' => paren_depth = paren_depth.saturating_sub(1),
      b'[' if paren_depth == 0 => bracket_depth += 1,
      b']' if paren_depth == 0 => bracket_depth = bracket_depth.saturating_sub(1),
      b if bracket_depth == 0
        && paren_depth == 0
        && (b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) =>
      {
        let start = i;
        i += 1;
        while i < bytes.len() {
          let next = bytes[i];
          if next.is_ascii_alphanumeric() || matches!(next, b'-' | b'_') {
            i += 1;
          } else {
            break;
          }
        }
        if input[start..i].eq_ignore_ascii_case("auto-flow") {
          return true;
        }
        continue;
      }
      _ => {}
    }
    i += 1;
  }
  false
}

/// Parse the `grid` shorthand (template or auto-flow forms).
pub fn parse_grid_shorthand(value: &str) -> Option<ParsedGridShorthand> {
  let value = trim_ascii_whitespace(value);
  if value.eq_ignore_ascii_case("none") {
    return Some(reset_grid_shorthand());
  }

  // Auto-flow form: either left or right of the slash contains auto-flow.
  let (left, right_opt) = split_once_unquoted(value, '/');
  let left = trim_ascii_whitespace(left);
  let right = right_opt.map(trim_ascii_whitespace);
  // If the author included a `/`, both sides must be present. This rejects values like
  // `grid: auto-flow /` and `grid: / auto-flow` (which would otherwise be mis-parsed as the
  // auto-flow shorthand form with an empty track list).
  if let Some(right) = right {
    if left.is_empty() || right.is_empty() {
      return None;
    }
  }

  let left_has_flow = contains_grid_auto_flow_keyword(left);
  let right_has_flow = right
    .as_ref()
    .is_some_and(|r| contains_grid_auto_flow_keyword(r));

  if !left_has_flow && !right_has_flow {
    return parse_grid_template_shorthand(value).map(|template| ParsedGridShorthand {
      template: Some(template),
      auto_rows: Some(vec![GridTrack::Auto]),
      auto_columns: Some(vec![GridTrack::Auto]),
      auto_flow: Some(crate::style::types::GridAutoFlow::Row),
    });
  }

  let mut auto_rows: Option<Vec<GridTrack>> = None;
  let mut auto_cols: Option<Vec<GridTrack>> = None;
  let mut auto_flow: Option<crate::style::types::GridAutoFlow> = None;

  let parse_auto_flow_tokens =
    |tokens: &str| -> (Option<crate::style::types::GridAutoFlow>, Option<String>) {
      let mut saw_auto_flow = false;
      let mut dense = false;
      let mut has_column = false;
      let mut remainder: Vec<&str> = Vec::new();
      for token in split_ascii_whitespace(tokens) {
        if token.eq_ignore_ascii_case("auto-flow") {
          saw_auto_flow = true;
          continue;
        }
        if token.eq_ignore_ascii_case("dense") {
          dense = true;
          continue;
        }
        if token.eq_ignore_ascii_case("column") {
          has_column = true;
          continue;
        }
        if token.eq_ignore_ascii_case("row") {
          continue;
        }
        remainder.push(token);
      }
      if !saw_auto_flow {
        return (None, None);
      }
      let primary = if has_column { "column" } else { "row" };
      let flow = match (primary, dense) {
        ("row", false) => crate::style::types::GridAutoFlow::Row,
        ("row", true) => crate::style::types::GridAutoFlow::RowDense,
        ("column", false) => crate::style::types::GridAutoFlow::Column,
        ("column", true) => crate::style::types::GridAutoFlow::ColumnDense,
        _ => crate::style::types::GridAutoFlow::Row,
      };
      let remainder = remainder.join(" ");
      let remainder = (!remainder.is_empty()).then_some(remainder);
      (Some(flow), remainder)
    };

  if left_has_flow {
    let (flow_parsed, remainder) = parse_auto_flow_tokens(left);
    if let Some(flow) = flow_parsed {
      auto_flow = Some(flow);
    }
    if let Some(rem) = remainder {
      let ParsedTracks { tracks, .. } = parse_track_list(&rem);
      if !tracks.is_empty() {
        auto_rows = Some(tracks);
      }
    }
    if let Some(r) = right {
      let ParsedTracks { tracks, .. } = parse_track_list(r);
      if !tracks.is_empty() {
        auto_cols = Some(tracks);
      }
    }
  } else if right_has_flow {
    let Some(right) = right else {
      return None;
    };
    let (flow_parsed, remainder) = parse_auto_flow_tokens(right);
    if let Some(flow) = flow_parsed {
      auto_flow = Some(flow);
    }
    if let Some(rem) = remainder {
      let ParsedTracks { tracks, .. } = parse_track_list(&rem);
      if !tracks.is_empty() {
        auto_cols = Some(tracks);
      }
    }
    let ParsedTracks { tracks, .. } = parse_track_list(left);
    if !tracks.is_empty() {
      auto_rows = Some(tracks);
    }
  } else {
    return None;
  }

  Some(ParsedGridShorthand {
    template: Some(empty_template_reset()),
    auto_rows: auto_rows.or_else(|| Some(vec![GridTrack::Auto])),
    auto_columns: auto_cols.or_else(|| Some(vec![GridTrack::Auto])),
    auto_flow: auto_flow.or(Some(crate::style::types::GridAutoFlow::Row)),
  })
}

fn reset_grid_shorthand() -> ParsedGridShorthand {
  ParsedGridShorthand {
    template: Some(empty_template_reset()),
    auto_rows: Some(vec![GridTrack::Auto]),
    auto_columns: Some(vec![GridTrack::Auto]),
    auto_flow: Some(crate::style::types::GridAutoFlow::Row),
  }
}

fn empty_template_reset() -> ParsedGridTemplate {
  ParsedGridTemplate {
    areas: Some(Vec::new()),
    row_tracks: Some((Vec::new(), Vec::new())),
    column_tracks: Some((Vec::new(), Vec::new())),
    row_is_subgrid: false,
    col_is_subgrid: false,
    row_subgrid_line_names: None,
    col_subgrid_line_names: None,
  }
}

fn parse_area_rows_with_sizes(input: &str) -> Option<(Vec<String>, Vec<Option<String>>)> {
  let mut rows = Vec::new();
  let mut row_sizes = Vec::new();
  let mut i = 0;
  let bytes = input.as_bytes();
  while i < bytes.len() {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() {
      break;
    }

    let quote = bytes[i];
    if quote != b'"' && quote != b'\'' {
      return None;
    }
    i += 1;
    let start = i;
    let mut escaped = false;
    while i < bytes.len() {
      let b = bytes[i];
      if escaped {
        escaped = false;
        i += 1;
        continue;
      }
      if b == b'\\' {
        escaped = true;
        i += 1;
        continue;
      }
      if b == quote {
        break;
      }
      i += 1;
    }
    if i >= bytes.len() {
      return None;
    }
    let row_str = &input[start..i];
    rows.push(row_str.to_string());
    i += 1; // skip closing quote

    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    // Capture optional row size until next quoted row (or end-of-input).
    let size_start = i;
    while i < bytes.len() && bytes[i] != b'"' && bytes[i] != b'\'' {
      i += 1;
    }
    let size = trim_ascii_whitespace(&input[size_start..i]);
    if size.is_empty() {
      row_sizes.push(None);
    } else {
      row_sizes.push(Some(size.to_string()));
    }
  }

  if rows.is_empty() {
    None
  } else {
    Some((rows, row_sizes))
  }
}

fn build_area_matrix(rows: &[String]) -> Option<Vec<Vec<Option<String>>>> {
  let mut matrix = Vec::with_capacity(rows.len());
  let mut expected_cols: Option<usize> = None;
  for row in rows {
    let cols: Vec<Option<String>> = split_ascii_whitespace(row)
      .map(|name| {
        if name == "." {
          None
        } else {
          Some(name.to_string())
        }
      })
      .collect();
    if cols.is_empty() {
      return None;
    }
    if let Some(exp) = expected_cols {
      if cols.len() != exp {
        return None;
      }
    } else {
      expected_cols = Some(cols.len());
    }
    matrix.push(cols);
  }
  Some(matrix)
}

fn split_once_unquoted(input: &str, delim: char) -> (&str, Option<&str>) {
  let mut paren_depth: usize = 0;
  let mut bracket_depth: usize = 0;
  let mut in_string: Option<char> = None;
  let mut escape = false;

  for (idx, ch) in input.char_indices() {
    if let Some(quote) = in_string {
      if escape {
        escape = false;
        continue;
      }
      if ch == '\\' {
        escape = true;
        continue;
      }
      if ch == quote {
        in_string = None;
      }
      continue;
    }

    match ch {
      '"' | '\'' => in_string = Some(ch),
      '(' => paren_depth += 1,
      ')' => paren_depth = paren_depth.saturating_sub(1),
      '[' if paren_depth == 0 => bracket_depth += 1,
      ']' if paren_depth == 0 => bracket_depth = bracket_depth.saturating_sub(1),
      d if d == delim && paren_depth == 0 && bracket_depth == 0 => {
        let (left, right) = input.split_at(idx);
        let right = right.get(delim.len_utf8()..).unwrap_or("");
        return (left, Some(right));
      }
      _ => {}
    }
  }
  (input, None)
}

/// Finalize grid placement: keep raw values for Taffy to resolve (named/numeric/nth)
pub fn finalize_grid_placement(_styles: &mut ComputedStyle) {}

/// Parse grid-column or grid-row placement (e.g., "text", "1 / 3", "auto")
pub fn parse_grid_line_placement<S: BuildHasher>(
  value: &str,
  named_lines: &HashMap<String, Vec<usize>, S>,
) -> (i32, i32) {
  let value = trim_ascii_whitespace(value);

  // Check if it contains a slash (explicit start / end)
  if let Some(slash_pos) = value.find('/') {
    let start_str = trim_ascii_whitespace(&value[..slash_pos]);
    let end_str = trim_ascii_whitespace(&value[slash_pos + 1..]);
    let start = parse_grid_line(start_str, named_lines);
    let end = parse_grid_line(end_str, named_lines);
    return (start, end);
  }

  // Single numeric value - treat as "start / span 1" (e.g., "2" means grid-row: 2 / 3)
  if let Ok(n) = value.parse::<i32>() {
    return (n, n + 1);
  }

  // Single value - check if it's a named area (e.g., "text")
  // Named areas should expand to area-start / area-end
  let start_name = format!("{}-start", value);
  let end_name = format!("{}-end", value);

  let start = if let Some(positions) = named_lines.get(&start_name) {
    if let Some(&pos) = positions.first() {
      (pos + 1) as i32
    } else {
      parse_grid_line(value, named_lines)
    }
  } else {
    parse_grid_line(value, named_lines)
  };

  let end = if let Some(positions) = named_lines.get(&end_name) {
    if let Some(&pos) = positions.first() {
      (pos + 1) as i32
    } else {
      0 // auto
    }
  } else {
    0 // auto
  };

  (start, end)
}

/// A parsed track list containing the concrete tracks and named line offsets.
#[derive(Default)]
pub(crate) struct ParsedTracks {
  pub tracks: Vec<GridTrack>,
  pub named_lines: HashMap<String, Vec<usize>>,
  pub line_names: Vec<Vec<String>>,
}

impl ParsedTracks {
  fn with_track(track: GridTrack) -> Self {
    Self {
      tracks: vec![track],
      named_lines: HashMap::new(),
      line_names: vec![Vec::new(), Vec::new()],
    }
  }
}

/// Lightweight parser for grid track lists
struct TrackListParser<'a> {
  input: &'a str,
  pos: usize,
}

impl<'a> TrackListParser<'a> {
  fn new(input: &'a str) -> Self {
    Self { input, pos: 0 }
  }

  fn remaining(&self) -> &'a str {
    self.input.get(self.pos..).unwrap_or("")
  }

  fn is_eof(&self) -> bool {
    self.remaining().is_empty()
  }

  fn skip_whitespace(&mut self) {
    loop {
      while let Some(ch) = self.peek_char() {
        if is_css_ascii_whitespace(ch) {
          self.advance_char();
        } else {
          break;
        }
      }

      // CSS comments act like whitespace at token boundaries.
      let rem = self.remaining();
      if rem.as_bytes().starts_with(b"/*") {
        if let Some(end) = rem.find("*/") {
          self.pos = self.pos.saturating_add(end + "*/".len());
          continue;
        }
        // Unterminated comment: treat as whitespace to EOF.
        self.pos = self.input.len();
        break;
      }

      break;
    }
  }

  fn peek_char(&self) -> Option<char> {
    self.remaining().chars().next()
  }

  fn advance_char(&mut self) -> Option<char> {
    let mut iter = self.remaining().char_indices();
    if let Some((idx, ch)) = iter.next() {
      self.pos += idx + ch.len_utf8();
      Some(ch)
    } else {
      None
    }
  }

  fn starts_with_ident(&self, ident: &str) -> bool {
    let rem = self.remaining();
    let Some(prefix) = rem.get(..ident.len()) else {
      return false;
    };
    if !prefix.eq_ignore_ascii_case(ident) {
      return false;
    }

    // Ensure we matched a full identifier token, not just a substring prefix (e.g. reject
    // `rowdense` when looking for `row`).
    //
    // This parser only needs to disambiguate ASCII keyword identifiers, so we treat common CSS
    // identifier continuation characters as part of the same token.
    let Some(rest) = rem.get(ident.len()..) else {
      return true;
    };
    let Some(next) = rest.chars().next() else {
      return true;
    };
    // CSS identifiers can contain non-ASCII codepoints, but all our grid keywords are ASCII.
    // If the next character could legally continue an identifier, do not treat this as a keyword
    // match.
    !(next.is_ascii_alphanumeric() || next == '-' || next == '_' || next as u32 >= 0x80 || next == '\\')
  }

  fn consume_bracketed_names(&mut self) -> Option<Vec<String>> {
    if self.peek_char()? != '[' {
      return None;
    }

    let rem = self.remaining();
    if !rem.starts_with('[') {
      return None;
    }
    let after_open = rem.get('['.len_utf8()..)?;
    let close_idx = after_open.find(']')?;
    let names_raw = after_open.get(..close_idx)?;

    let names = if names_raw.contains("/*") {
      let mut stripped = String::with_capacity(names_raw.len());
      let mut idx = 0usize;
      while let Some(rel_start) = names_raw.get(idx..).and_then(|s| s.find("/*")) {
        let start = idx + rel_start;
        stripped.push_str(names_raw.get(idx..start).unwrap_or(""));
        stripped.push(' ');
        let after_start = start + "/*".len();
        let Some(rel_end) = names_raw.get(after_start..).and_then(|s| s.find("*/")) else {
          idx = names_raw.len();
          break;
        };
        idx = after_start + rel_end + "*/".len();
      }
      stripped.push_str(names_raw.get(idx..).unwrap_or(""));
      split_ascii_whitespace(&stripped)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
    } else {
      split_ascii_whitespace(names_raw)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
    };

    // Advance past `[ ... ]`.
    self.pos = self
      .pos
      .saturating_add('['.len_utf8() + close_idx + ']'.len_utf8());

    Some(names)
  }

  fn consume_function_arguments(&mut self, name: &str) -> Option<String> {
    if !self.starts_with_ident(name) {
      return None;
    }
    let after_name = self.pos + name.len();
    let rem_after_name = self.input.get(after_name..)?;
    let mut chars = rem_after_name.chars();
    if chars.next()? != '(' {
      return None;
    }

    let rem = self.input.get(after_name..)?;
    let mut depth: i32 = 0;
    let mut end_paren: Option<usize> = None;
    for (idx, ch) in rem.char_indices() {
      match ch {
        '(' => depth += 1,
        ')' => {
          depth -= 1;
          if depth == 0 {
            end_paren = Some(idx);
            break;
          }
          if depth < 0 {
            return None;
          }
        }
        _ => {}
      }
    }
    let end_paren = end_paren?;
    let inner = rem.get('('.len_utf8()..end_paren)?.to_string();
    self.pos = after_name.saturating_add(end_paren + ')'.len_utf8());
    Some(inner)
  }

  /// Consumes a single track token, stopping at top-level whitespace or '['
  fn consume_track_token(&mut self) -> Option<String> {
    let rem = self.remaining();
    if rem.is_empty() {
      return None;
    }

    let bytes = rem.as_bytes();
    let mut depth: usize = 0;
    let mut end: Option<(usize, bool, usize)> = None;
    // bool: true if we should consume the delimiter, false if we should leave it (e.g. '[')
    // usize: delimiter length in bytes
    for (idx, ch) in rem.char_indices() {
      match ch {
        '(' => depth += 1,
        ')' => depth = depth.saturating_sub(1),
        '[' if depth == 0 => {
          end = Some((idx, false, '['.len_utf8()));
          break;
        }
        '/' if depth == 0 && bytes.get(idx + 1) == Some(&b'*') => {
          // Treat comments as top-level whitespace separators.
          let mut j = idx + 2;
          while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
            j += 1;
          }
          let comment_end = if j + 1 < bytes.len() { j + 2 } else { bytes.len() };
          end = Some((idx, true, comment_end.saturating_sub(idx)));
          break;
        }
        _ if is_css_ascii_whitespace(ch) && depth == 0 => {
          end = Some((idx, true, ch.len_utf8()));
          break;
        }
        _ => {}
      }
    }

    let (end_idx, consume_delim, delim_len) = end.unwrap_or((rem.len(), true, 0));
    let token = trim_ascii_whitespace(rem.get(..end_idx).unwrap_or(""));

    let advance_by = if consume_delim {
      end_idx.saturating_add(delim_len)
    } else {
      end_idx
    };
    self.pos = self.pos.saturating_add(advance_by);

    if token.is_empty() {
      None
    } else {
      Some(token.to_string())
    }
  }

  fn parse_repeat(&mut self) -> Option<ParsedTracks> {
    let inner = self.consume_function_arguments("repeat")?;
    let (count_str, pattern_str) = split_once_comma(&inner)?;
    let count_str = strip_css_comments_to_whitespace(count_str);
    let count_str = trim_ascii_whitespace(count_str.as_ref());
    let pattern = parse_track_list(pattern_str);
    if pattern.tracks.is_empty() {
      return None;
    }

    if count_str.eq_ignore_ascii_case("auto-fill") {
      return Some(ParsedTracks {
        tracks: vec![GridTrack::RepeatAutoFill {
          tracks: pattern.tracks,
          line_names: pattern.line_names.clone(),
        }],
        named_lines: pattern.named_lines,
        line_names: pattern.line_names,
      });
    }
    if count_str.eq_ignore_ascii_case("auto-fit") {
      return Some(ParsedTracks {
        tracks: vec![GridTrack::RepeatAutoFit {
          tracks: pattern.tracks,
          line_names: pattern.line_names.clone(),
        }],
        named_lines: pattern.named_lines,
        line_names: pattern.line_names,
      });
    }

    let repeat_count: usize = count_str.parse().ok()?;
    let mut tracks = Vec::new();
    let mut named_lines = HashMap::new();
    let mut line_names: Vec<Vec<String>> = vec![Vec::new()];
    for _ in 0..repeat_count {
      let offset = tracks.len();
      tracks.extend(pattern.tracks.iter().cloned());
      for (name, positions) in pattern.named_lines.iter() {
        let entry = named_lines.entry(name.clone()).or_insert_with(Vec::new);
        entry.extend(positions.iter().map(|p| p + offset));
      }
      // repeat line names: merge first entry with current line, then append rest
      if let Some(first) = pattern.line_names.first() {
        if let Some(last) = line_names.last_mut() {
          last.extend(first.iter().cloned());
        } else {
          line_names.push(first.clone());
        }
      }
      for names in pattern.line_names.iter().skip(1) {
        line_names.push(names.clone());
      }
    }

    if line_names.len() < tracks.len() + 1 {
      line_names.resize(tracks.len() + 1, Vec::new());
    }

    Some(ParsedTracks {
      tracks,
      named_lines,
      line_names,
    })
  }

  fn parse_component(&mut self) -> Option<ParsedTracks> {
    if self.starts_with_ident("repeat") {
      if let Some(repeated) = self.parse_repeat() {
        return Some(repeated);
      }
    }

    let token = self.consume_track_token()?;
    parse_single_grid_track(&token).map(ParsedTracks::with_track)
  }
}

fn parse_line_name_list(
  parser: &mut TrackListParser<'_>,
  require_non_empty: bool,
) -> Option<Vec<Vec<String>>> {
  fn parse_line_names_group(input: &str) -> Option<Vec<Vec<String>>> {
    let mut parser = TrackListParser::new(input);
    let mut line_names: Vec<Vec<String>> = Vec::new();
    while !parser.is_eof() {
      parser.skip_whitespace();
      if parser.is_eof() {
        break;
      }
      if let Some(names) = parser.consume_bracketed_names() {
        line_names.push(names);
        continue;
      }
      return None;
    }
    if line_names.is_empty() {
      None
    } else {
      Some(line_names)
    }
  }

  let mut out: Vec<Vec<String>> = Vec::new();

  while !parser.is_eof() {
    parser.skip_whitespace();
    if parser.is_eof() {
      break;
    }

    if let Some(names) = parser.consume_bracketed_names() {
      out.push(names);
      continue;
    }

    if parser.starts_with_ident("repeat") {
      let inner = parser.consume_function_arguments("repeat")?;
      let (count_str, pattern_str) = split_once_comma(&inner)?;
      let count_str = strip_css_comments_to_whitespace(count_str);
      let count_str = trim_ascii_whitespace(count_str.as_ref());

      if count_str.is_empty() || !count_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
      }
      let count: usize = count_str.parse().ok()?;
      if count == 0 {
        return None;
      }

      // CSS Grid 2: `repeat(<integer>, <line-names>+)` in subgrid line name lists.
      //
      // Note: `repeat()` is not allowed to be nested, so `pattern_str` must contain only bracketed
      // line-name lists.
      let pattern = parse_line_names_group(pattern_str)?;

      // Expand the repeat inline.
      out.reserve(pattern.len().saturating_mul(count));
      for _ in 0..count {
        out.extend(pattern.iter().cloned());
      }
      continue;
    }

    return None;
  }

  if require_non_empty && out.is_empty() {
    None
  } else {
    Some(out)
  }
}

/// Parse the line-name portions of a `subgrid` track list.
///
/// Returns any bracketed line name lists that appear alongside the `subgrid` keyword.
pub fn parse_subgrid_line_names(input: &str) -> Option<Vec<Vec<String>>> {
  let mut parser = TrackListParser::new(input);
  parser.skip_whitespace();

  // Grammar: `subgrid <<line-name-list>>?`
  // Require the `subgrid` keyword to appear first (ignoring leading whitespace).
  if !parser.starts_with_ident("subgrid") {
    return None;
  }
  parser.pos += "subgrid".len();
  parse_line_name_list(&mut parser, false)
}

fn split_once_comma(input: &str) -> Option<(&str, &str)> {
  let bytes = input.as_bytes();
  let mut depth: usize = 0;
  let mut idx = 0usize;
  while idx < bytes.len() {
    let b = bytes[idx];

    // Treat CSS comments as whitespace so commas inside comments don't terminate the split.
    if b == b'/' && bytes.get(idx + 1) == Some(&b'*') {
      idx += 2;
      while idx + 1 < bytes.len() && !(bytes[idx] == b'*' && bytes[idx + 1] == b'/') {
        idx += 1;
      }
      idx = idx.saturating_add(2).min(bytes.len());
      continue;
    }

    match b {
      b'(' => depth += 1,
      b')' => depth = depth.saturating_sub(1),
      b',' if depth == 0 => {
        let first = trim_ascii_whitespace(input.get(..idx).unwrap_or(""));
        let second =
          trim_ascii_whitespace(input.get(idx + ','.len_utf8()..).unwrap_or(""));
        return Some((first, second));
      }
      _ => {}
    }

    idx += 1;
  }
  None
}

pub(crate) fn parse_track_list(input: &str) -> ParsedTracks {
  let mut parser = TrackListParser::new(input);
  let mut tracks = Vec::new();
  let mut named_lines: HashMap<String, Vec<usize>> = HashMap::new();
  let mut line_names: Vec<Vec<String>> = vec![Vec::new()];

  while !parser.is_eof() {
    parser.skip_whitespace();
    while let Some(names) = parser.consume_bracketed_names() {
      let line = tracks.len();
      if line_names.len() <= line {
        line_names.resize(line + 1, Vec::new());
      }
      for name in names {
        line_names[line].push(name.clone());
        named_lines.entry(name).or_insert_with(Vec::new).push(line);
      }
      parser.skip_whitespace();
    }

    parser.skip_whitespace();
    if parser.is_eof() {
      break;
    }

    let parsed = match parser.parse_component() {
      Some(component) => component,
      None => break,
    };

    let offset = tracks.len();
    if !parsed.line_names.is_empty() {
      // Merge first line names into current line (before the first track of the parsed chunk)
      if let Some(first) = parsed.line_names.first() {
        if let Some(last) = line_names.last_mut() {
          last.extend(first.iter().cloned());
        } else {
          line_names.push(first.clone());
        }
      }
      // Append remaining line names
      for names in parsed.line_names.iter().skip(1) {
        line_names.push(names.clone());
      }
    } else {
      // Ensure we have a slot for subsequent lines
      line_names.resize(
        tracks.len().saturating_add(parsed.tracks.len()) + 1,
        Vec::new(),
      );
    }

    for (name, positions) in parsed.named_lines {
      let entry = named_lines.entry(name).or_insert_with(Vec::new);
      entry.extend(positions.into_iter().map(|p| p + offset));
    }
    tracks.extend(parsed.tracks);

    parser.skip_whitespace();
    while let Some(names) = parser.consume_bracketed_names() {
      let line = tracks.len();
      if line_names.len() <= line {
        line_names.resize(line + 1, Vec::new());
      }
      for name in names {
        line_names[line].push(name.clone());
        named_lines.entry(name).or_insert_with(Vec::new).push(line);
      }
      parser.skip_whitespace();
    }
  }

  if line_names.len() < tracks.len() + 1 {
    line_names.resize(tracks.len() + 1, Vec::new());
  }

  ParsedTracks {
    tracks,
    named_lines,
    line_names,
  }
}

/// Parse a single grid track value
pub(crate) fn parse_single_grid_track(track_str: &str) -> Option<GridTrack> {
  let track_str = trim_ascii_whitespace(track_str);
  let stripped = strip_css_comments_to_whitespace(track_str);
  let track_str = trim_ascii_whitespace(stripped.as_ref());
  if track_str.is_empty() {
    return None;
  }

  let lower = track_str.to_ascii_lowercase();

  if let Some(inner) = lower
    .strip_prefix("minmax(")
    .and_then(|s| s.strip_suffix(')'))
  {
    let (min_str, max_str) = split_once_comma(inner)?;
    let min = parse_track_breadth(min_str)?;
    let max = parse_track_breadth(max_str)?;
    return Some(GridTrack::MinMax(Box::new(min), Box::new(max)));
  }

  if let Some(inner) = lower
    .strip_prefix("fit-content(")
    .and_then(|s| s.strip_suffix(')'))
  {
    let len = parse_length_value(inner)?;
    return Some(GridTrack::FitContent(len));
  }

  if lower == "min-content" {
    return Some(GridTrack::MinContent);
  }
  if lower == "max-content" {
    return Some(GridTrack::MaxContent);
  }
  if lower == "auto" {
    return Some(GridTrack::Auto);
  }

  if let Some(val_str) = lower.strip_suffix("fr") {
    if let Ok(val) = trim_ascii_whitespace(val_str).parse::<f32>() {
      return Some(GridTrack::Fr(val));
    }
  }

  if let Some(length) = parse_length_value(&lower) {
    return Some(GridTrack::Length(length));
  }

  None
}

fn parse_track_breadth(value: &str) -> Option<GridTrack> {
  let trimmed = trim_ascii_whitespace(value);
  let lower = trimmed.to_ascii_lowercase();
  if lower == "auto" {
    return Some(GridTrack::Auto);
  }
  if lower == "min-content" {
    return Some(GridTrack::MinContent);
  }
  if lower == "max-content" {
    return Some(GridTrack::MaxContent);
  }
  if let Some(inner) = lower
    .strip_prefix("fit-content(")
    .and_then(|s| s.strip_suffix(')'))
  {
    return parse_length_value(inner).map(GridTrack::FitContent);
  }
  if let Some(val_str) = lower.strip_suffix("fr") {
    if let Ok(val) = trim_ascii_whitespace(val_str).parse::<f32>() {
      return Some(GridTrack::Fr(val));
    }
  }

  parse_length_value(&lower).map(GridTrack::Length)
}

fn parse_length_value(raw: &str) -> Option<Length> {
  match parse_property_value("", raw)? {
    PropertyValue::Length(l) => Some(l),
    PropertyValue::Percentage(p) => Some(Length::percent(p)),
    PropertyValue::Number(n) if n == 0.0 => Some(Length::px(n)),
    _ => None,
  }
}

#[cfg(test)]
mod tests;
