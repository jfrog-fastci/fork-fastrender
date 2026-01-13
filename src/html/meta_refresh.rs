//! Meta refresh parsing utilities.
//!
//! Provides a lightweight extractor for `<meta http-equiv="refresh">` URLs so
//! callers can follow scriptless redirects commonly used as `<noscript>` fallbacks.

use memchr::memchr;
use std::ops::ControlFlow;

const MAX_META_REFRESH_SCAN_BYTES: usize = 256 * 1024;
const MAX_JS_REDIRECT_SCAN_BYTES: usize = 256 * 1024;
const MAX_REFRESH_CONTENT_LEN: usize = 8 * 1024;
const MAX_ATTRIBUTES_PER_TAG: usize = 128;

// HTML defines "ASCII whitespace" as: U+0009 TAB, U+000A LF, U+000C FF, U+000D CR, U+0020 SPACE.
fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn scan_html_prefix(html: &str, max_bytes: usize) -> &str {
  if html.len() <= max_bytes {
    return html;
  }
  let mut end = max_bytes.min(html.len());
  while end > 0 && !html.is_char_boundary(end) {
    end -= 1;
  }
  &html[..end]
}

fn find_case_insensitive(bytes: &[u8], needle: &[u8], start: usize) -> Option<usize> {
  if needle.is_empty() {
    return Some(start);
  }
  if start >= bytes.len() {
    return None;
  }
  let len = needle.len();
  let mut i = start;
  while i + len <= bytes.len() {
    if bytes[i..i + len].eq_ignore_ascii_case(needle) {
      return Some(i);
    }
    i += 1;
  }
  None
}

fn for_each_attribute<'a>(
  tag: &'a str,
  mut visit: impl FnMut(&'a str, &'a str) -> ControlFlow<()>,
) {
  let bytes = tag.as_bytes();
  let mut i = 0usize;
  let mut attrs_seen = 0usize;

  // Skip opening `<` + tag name.
  if bytes.get(i) == Some(&b'<') {
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    while i < bytes.len() && bytes[i] != b'>' && !bytes[i].is_ascii_whitespace() {
      i += 1;
    }
  }

  while i < bytes.len() {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() || bytes[i] == b'>' {
      break;
    }
    // Ignore self-closing markers.
    if bytes[i] == b'/' {
      i += 1;
      continue;
    }

    let name_start = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' && bytes[i] != b'>'
    {
      i += 1;
    }
    let name_end = i;
    if name_end == name_start {
      i = i.saturating_add(1);
      continue;
    }
    let name = &tag[name_start..name_end];

    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }

    let mut value = "";
    if i < bytes.len() && bytes[i] == b'=' {
      i += 1;
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }

      if i + 1 < bytes.len() && bytes[i] == b'\\' && (bytes[i + 1] == b'"' || bytes[i + 1] == b'\'')
      {
        let quote = bytes[i + 1];
        i += 2;
        let start = i;
        while i < bytes.len() && bytes[i] != quote {
          i += 1;
        }
        value = &tag[start..i];
        if i < bytes.len() {
          i += 1;
        }
      } else if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
        let quote = bytes[i];
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i] != quote {
          i += 1;
        }
        value = &tag[start..i];
        if i < bytes.len() {
          i += 1;
        }
      } else {
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
          i += 1;
        }
        value = &tag[start..i];
      }
    }

    attrs_seen += 1;
    if let ControlFlow::Break(()) = visit(name, value) {
      break;
    }
    if attrs_seen >= MAX_ATTRIBUTES_PER_TAG {
      break;
    }
  }
}

/// Parses the first `<meta http-equiv="refresh">` URL in the provided HTML.
///
/// Returns `Some(url)` when a refresh URL is found, otherwise `None`.
pub fn extract_meta_refresh_url(html: &str) -> Option<String> {
  let html = scan_html_prefix(html, MAX_META_REFRESH_SCAN_BYTES);
  let bytes = html.as_bytes();
  let mut template_depth: usize = 0;
  let mut i: usize = 0;

  while let Some(rel) = memchr(b'<', &bytes[i..]) {
    let tag_start = i + rel;

    if bytes
      .get(tag_start..tag_start + 4)
      .is_some_and(|head| head == b"<!--")
    {
      let end = super::find_bytes(bytes, tag_start + 4, b"-->")
        .map(|pos| pos + 3)
        .unwrap_or(bytes.len());
      i = end;
      continue;
    }

    if bytes
      .get(tag_start..tag_start + 9)
      .is_some_and(|head| head.eq_ignore_ascii_case(b"<![cdata["))
    {
      let end = super::find_bytes(bytes, tag_start + 9, b"]]>")
        .map(|pos| pos + 3)
        .unwrap_or(bytes.len());
      i = end;
      continue;
    }

    if bytes
      .get(tag_start + 1)
      .is_some_and(|b| *b == b'!' || *b == b'?')
    {
      let Some(end) = super::find_tag_end(bytes, tag_start) else {
        break;
      };
      i = end;
      continue;
    }

    let Some(tag_end) = super::find_tag_end(bytes, tag_start) else {
      break;
    };

    let Some((is_end, name_start, name_end)) =
      super::parse_tag_name_range(bytes, tag_start, tag_end)
    else {
      i = tag_start + 1;
      continue;
    };

    let name = &bytes[name_start..name_end];

    let raw_text_tag: Option<&'static [u8]> = if !is_end && name.eq_ignore_ascii_case(b"script") {
      Some(b"script")
    } else if !is_end && name.eq_ignore_ascii_case(b"style") {
      Some(b"style")
    } else if !is_end && name.eq_ignore_ascii_case(b"textarea") {
      Some(b"textarea")
    } else if !is_end && name.eq_ignore_ascii_case(b"title") {
      Some(b"title")
    } else if !is_end && name.eq_ignore_ascii_case(b"xmp") {
      Some(b"xmp")
    } else {
      None
    };

    if !is_end && name.eq_ignore_ascii_case(b"plaintext") {
      break;
    }

    if name.eq_ignore_ascii_case(b"template") {
      if is_end {
        if template_depth > 0 {
          template_depth -= 1;
        }
      } else {
        template_depth += 1;
      }
    }

    if template_depth == 0 && !is_end && name.eq_ignore_ascii_case(b"meta") {
      let tag = &html[tag_start..tag_end];
      let mut http_equiv: Option<String> = None;
      let mut content: Option<String> = None;

      for_each_attribute(tag, |attr, mut value| {
        if !attr.eq_ignore_ascii_case("http-equiv") && !attr.eq_ignore_ascii_case("content") {
          return ControlFlow::Continue(());
        }

        if value.len() > MAX_REFRESH_CONTENT_LEN {
          let mut end = MAX_REFRESH_CONTENT_LEN.min(value.len());
          while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
          }
          value = &value[..end];
        }

        let normalized = normalize_attr_value(value);
        if attr.eq_ignore_ascii_case("http-equiv") {
          http_equiv = Some(normalized);
        } else if attr.eq_ignore_ascii_case("content") {
          content = Some(normalized);
        }

        if http_equiv.is_some() && content.is_some() {
          ControlFlow::Break(())
        } else {
          ControlFlow::Continue(())
        }
      });

      if http_equiv
        .as_ref()
        .is_some_and(|value| value.eq_ignore_ascii_case("refresh"))
      {
        if let Some(content) = content {
          if let Some(url) = parse_refresh_content(&content) {
            return Some(url);
          }
        }
      }
    }

    if let Some(tag) = raw_text_tag {
      i = super::find_raw_text_element_end(bytes, tag_end, tag);
      continue;
    }

    i = tag_end;
  }

  None
}

fn find_raw_text_element_closing_tag(
  bytes: &[u8],
  start: usize,
  tag: &'static [u8],
) -> Option<(usize, usize)> {
  let mut idx = start;
  while let Some(rel) = memchr(b'<', &bytes[idx..]) {
    let pos = idx + rel;
    if bytes.get(pos + 1) == Some(&b'/') {
      let name_start = pos + 2;
      let name_end = name_start + tag.len();
      if name_end <= bytes.len()
        && bytes[name_start..name_end].eq_ignore_ascii_case(tag)
        && !bytes
          .get(name_end)
          .map(|b| super::is_tag_name_char(*b))
          .unwrap_or(false)
      {
        let end = super::find_tag_end(bytes, pos)?;
        return Some((pos, end));
      }
    }
    idx = pos + 1;
  }
  None
}

fn extract_js_location_redirect_from_source(source: &str) -> Option<String> {
  const MAX_REDIRECT_LEN: usize = 2048;

  let decoded = decode_refresh_entities(source);
  let bytes = decoded.as_bytes();
  let mut needs_url_var_fallback = false;

  fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
      .as_bytes()
      .get(..prefix.len())
      .map(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
      .unwrap_or(false)
  }
  use crate::ui::string_match::find_ascii_case_insensitive_bytes_from;

  #[derive(Clone, Copy)]
  enum PatternKind {
    Call,
    Assign,
  }

  // Restrict to explicit navigations to avoid false positives from innocuous property reads like
  // `location.pathname`. We also support direct assignment to the Location object (e.g. `location =
  // "/next"`).
  let patterns: [(&str, PatternKind); 24] = [
    ("window.location.replace", PatternKind::Call),
    ("document.location.replace", PatternKind::Call),
    ("top.location.replace", PatternKind::Call),
    ("self.location.replace", PatternKind::Call),
    ("parent.location.replace", PatternKind::Call),
    ("location.replace", PatternKind::Call),
    ("window.location.assign", PatternKind::Call),
    ("document.location.assign", PatternKind::Call),
    ("top.location.assign", PatternKind::Call),
    ("self.location.assign", PatternKind::Call),
    ("parent.location.assign", PatternKind::Call),
    ("location.assign", PatternKind::Call),
    ("window.location.href", PatternKind::Assign),
    ("document.location.href", PatternKind::Assign),
    ("top.location.href", PatternKind::Assign),
    ("self.location.href", PatternKind::Assign),
    ("parent.location.href", PatternKind::Assign),
    ("location.href", PatternKind::Assign),
    ("window.location", PatternKind::Assign),
    ("document.location", PatternKind::Assign),
    ("top.location", PatternKind::Assign),
    ("self.location", PatternKind::Assign),
    ("parent.location", PatternKind::Assign),
    ("location", PatternKind::Assign),
    // Intentionally omit generic `.location` matches to avoid picking up unrelated object
    // properties (e.g. `foo.location = ...`).
  ];

  for (pat, kind) in patterns.iter() {
    let mut search_start = 0usize;
    let needle = pat.as_bytes();
    while let Some(idx) = find_ascii_case_insensitive_bytes_from(bytes, needle, search_start) {
      search_start = idx + needle.len();

      // Require the match to start on a non-identifier boundary to avoid picking up attributes
      // like data-location="...".
      if idx > 0 {
        let prev = bytes[idx - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'-' {
          continue;
        }
      }

      let after = idx + needle.len();
      if after < bytes.len() {
        let next = bytes[after];
        if next.is_ascii_alphanumeric() || next == b'_' {
          continue;
        }
      }

      // If we are matching an unqualified `location*` token, ensure it isn't a property access
      // (e.g. `foo.location = ...`).
      if pat.starts_with("location") && idx > 0 && bytes[idx - 1] == b'.' {
        continue;
      }

      // Avoid misclassifying `var/let/const location = ...` as a navigation.
      if *pat == "location" {
        let mut j = idx;
        while j > 0 && bytes[j - 1].is_ascii_whitespace() {
          j -= 1;
        }
        let mut k = j;
        while k > 0 && (bytes[k - 1].is_ascii_alphanumeric() || bytes[k - 1] == b'_') {
          k -= 1;
        }
        if let Some(word) = decoded.get(k..j) {
          if word.eq_ignore_ascii_case("var")
            || word.eq_ignore_ascii_case("let")
            || word.eq_ignore_ascii_case("const")
          {
            continue;
          }
        }
      }

      let mut i = idx + needle.len();
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }

      match kind {
        PatternKind::Assign => {
          if i >= bytes.len() || bytes[i] != b'=' {
            continue;
          }
          i += 1;
          while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
          }
        }
        PatternKind::Call => {
          if i >= bytes.len() || bytes[i] != b'(' {
            continue;
          }
          i += 1;
          while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
          }
        }
      }

      // Allow redundant grouping parentheses like `location = ('/next')` or
      // `location.replace(('/next'))`.
      while i < bytes.len() && bytes[i] == b'(' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
          i += 1;
        }
      }

      if let Some(url) = extract_js_string_literal(&decoded, i, MAX_REDIRECT_LEN) {
        return Some(url);
      }

      if let Some(url) = extract_wrapped_js_string_literal(&decoded, i, MAX_REDIRECT_LEN) {
        return Some(url);
      }

      {
        // Some redirects store the target in a local `url` variable:
        // `var url = "/next"; location.replace(url)`.
        //
        // Only fall back to scanning for `var url = ...` when we see `location.*(url)`; this avoids
        // false positives in scripts that use `url` for unrelated purposes (e.g. AJAX endpoints).
        let start = i;
        if start < bytes.len()
          && (bytes[start].is_ascii_alphabetic() || bytes[start] == b'_' || bytes[start] == b'$')
        {
          let mut end = start + 1;
          while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'$')
          {
            end += 1;
          }
          if decoded.get(start..end) == Some("url") {
            needs_url_var_fallback = true;
          }
        }

        let start = i;
        while i < bytes.len() {
          let b = bytes[i];
          if b.is_ascii_whitespace() || matches!(b, b';' | b')' | b',') {
            break;
          }
          i += 1;
        }
        if i > start {
          let candidate = trim_ascii_whitespace(&decoded[start..i]);
          if !candidate.is_empty()
            && candidate.len() <= MAX_REDIRECT_LEN
            && (starts_with_ignore_ascii_case(candidate, "http")
              || candidate.starts_with("//")
              || candidate.starts_with('/')
              || candidate.starts_with("www."))
          {
            return Some(unescape_js_literal(candidate));
          }
        }
      }
    }
  }

  if needs_url_var_fallback {
    // Fallback: look for a variable assignment that captures a URL literal.
    // (Only enabled when we saw an explicit `location.*(url)` navigation.)
    let url_decls = ["var url", "let url", "const url"];
    for decl in url_decls.iter() {
      let mut search_start = 0usize;
      let needle = decl.as_bytes();
      while let Some(idx) = find_ascii_case_insensitive_bytes_from(bytes, needle, search_start) {
        search_start = idx + needle.len();

        if idx > 0 {
          let prev = bytes[idx - 1];
          if prev.is_ascii_alphanumeric() || prev == b'_' {
            continue;
          }
        }

        let after = idx + needle.len();
        if after < bytes.len() {
          let next = bytes[after];
          if next.is_ascii_alphanumeric() || next == b'_' {
            continue;
          }
        }

        let mut i = after;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
          i += 1;
        }
        if bytes.get(i) != Some(&b'=') {
          continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
          i += 1;
        }
        while i < bytes.len() && bytes[i] == b'(' {
          i += 1;
          while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
          }
        }

        if let Some(mut url) = extract_js_string_literal(&decoded, i, MAX_REDIRECT_LEN)
          .or_else(|| extract_wrapped_js_string_literal(&decoded, i, MAX_REDIRECT_LEN))
        {
          if url.starts_with("//") {
            url = format!("https:{}", url);
          }
          return Some(url);
        }
      }
    }
  }

  None
}

/// Extracts a literal URL from simple JavaScript redirects such as
/// `window.location.href = "https://example.com"` or `location.replace('/next')`.
pub fn extract_js_location_redirect(html: &str) -> Option<String> {
  let html = scan_html_prefix(html, MAX_JS_REDIRECT_SCAN_BYTES);
  let bytes = html.as_bytes();
  let mut template_depth: usize = 0;
  let mut i: usize = 0;

  while let Some(rel) = memchr(b'<', &bytes[i..]) {
    let tag_start = i + rel;

    if bytes
      .get(tag_start..tag_start + 4)
      .is_some_and(|head| head == b"<!--")
    {
      let end = super::find_bytes(bytes, tag_start + 4, b"-->")
        .map(|pos| pos + 3)
        .unwrap_or(bytes.len());
      i = end;
      continue;
    }

    if bytes
      .get(tag_start..tag_start + 9)
      .is_some_and(|head| head.eq_ignore_ascii_case(b"<![cdata["))
    {
      let end = super::find_bytes(bytes, tag_start + 9, b"]]>")
        .map(|pos| pos + 3)
        .unwrap_or(bytes.len());
      i = end;
      continue;
    }

    if bytes
      .get(tag_start + 1)
      .is_some_and(|b| *b == b'!' || *b == b'?')
    {
      let Some(end) = super::find_tag_end(bytes, tag_start) else {
        break;
      };
      i = end;
      continue;
    }

    let Some(tag_end) = super::find_tag_end(bytes, tag_start) else {
      break;
    };

    let Some((is_end, name_start, name_end)) =
      super::parse_tag_name_range(bytes, tag_start, tag_end)
    else {
      i = tag_start + 1;
      continue;
    };

    let name = &bytes[name_start..name_end];

    let raw_text_tag: Option<&'static [u8]> = if !is_end && name.eq_ignore_ascii_case(b"script") {
      Some(b"script")
    } else if !is_end && name.eq_ignore_ascii_case(b"style") {
      Some(b"style")
    } else if !is_end && name.eq_ignore_ascii_case(b"textarea") {
      Some(b"textarea")
    } else if !is_end && name.eq_ignore_ascii_case(b"title") {
      Some(b"title")
    } else if !is_end && name.eq_ignore_ascii_case(b"xmp") {
      Some(b"xmp")
    } else {
      None
    };

    if !is_end && name.eq_ignore_ascii_case(b"plaintext") {
      break;
    }

    if name.eq_ignore_ascii_case(b"template") {
      if is_end {
        if template_depth > 0 {
          template_depth -= 1;
        }
      } else {
        template_depth += 1;
      }
    }

    if template_depth == 0 && !is_end {
      if name.eq_ignore_ascii_case(b"script") {
        let Some((close_start, close_end)) =
          find_raw_text_element_closing_tag(bytes, tag_end, b"script")
        else {
          let script = &html[tag_end..];
          return extract_js_location_redirect_from_source(script);
        };
        let script = &html[tag_end..close_start];
        if let Some(url) = extract_js_location_redirect_from_source(script) {
          return Some(url);
        }
        i = close_end;
        continue;
      }

      if name.eq_ignore_ascii_case(b"body") || name.eq_ignore_ascii_case(b"html") {
        let tag = &html[tag_start..tag_end];
        let mut onload: Option<&str> = None;
        for_each_attribute(tag, |attr, value| {
          if attr.eq_ignore_ascii_case("onload") {
            onload = Some(value);
            return ControlFlow::Break(());
          }
          ControlFlow::Continue(())
        });
        if let Some(code) = onload {
          if let Some(url) = extract_js_location_redirect_from_source(code) {
            return Some(url);
          }
        }
      }
    }

    if let Some(tag) = raw_text_tag {
      i = super::find_raw_text_element_end(bytes, tag_end, tag);
      continue;
    }

    i = tag_end;
  }

  None
}

fn extract_js_string_literal(decoded: &str, start_idx: usize, max_len: usize) -> Option<String> {
  let bytes = decoded.as_bytes();
  let quote = *bytes.get(start_idx)?;
  if quote != b'"' && quote != b'\'' && quote != b'`' {
    return None;
  }

  let mut i = start_idx + 1;
  let start = i;
  let mut has_interpolation = false;
  while i < bytes.len() {
    let b = bytes[i];

    if quote == b'`' && b == b'$' && bytes.get(i + 1) == Some(&b'{') {
      has_interpolation = true;
    }

    if b == b'\\' {
      i += 1;
      if i < bytes.len() {
        i += 1;
      }
      continue;
    }

    if b == quote {
      break;
    }
    i += 1;
  }

  if quote == b'`' && has_interpolation {
    return None;
  }

  let end = i.min(decoded.len());
  let candidate = trim_ascii_whitespace(&decoded[start..end]);
  if candidate.is_empty() || candidate.len() > max_len {
    return None;
  }

  Some(unescape_js_literal(candidate))
}

fn extract_wrapped_js_string_literal(
  decoded: &str,
  start_idx: usize,
  max_len: usize,
) -> Option<String> {
  // Some redirects wrap a static string in common URL-decoding helpers.
  let bytes = decoded.as_bytes();
  let wrappers = ["decodeuricomponent", "decodeuri", "unescape"];
  let rest = decoded.get(start_idx..)?;
  for wrapper in wrappers.iter() {
    if rest
      .get(..wrapper.len())
      .map(|head| head.eq_ignore_ascii_case(wrapper))
      .unwrap_or(false)
    {
      let mut i = start_idx + wrapper.len();
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }
      if bytes.get(i) != Some(&b'(') {
        continue;
      }
      i += 1;
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }
      while i < bytes.len() && bytes[i] == b'(' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
          i += 1;
        }
      }
      if let Some(url) = extract_js_string_literal(decoded, i, max_len) {
        return Some(url);
      }
    }
  }
  None
}

fn unescape_js_literal(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  let mut s = s.replace("\\\\/", "/");
  s = s.replace("\\/", "/");
  let mut chars = s.chars();
  while let Some(ch) = chars.next() {
    if ch == '\\' {
      if let Some(next) = chars.next() {
        match next {
          '\\' | '"' | '\'' => out.push(next),
          '/' => out.push('/'),
          'x' => {
            let hi = chars.next();
            let lo = chars.next();
            if let (Some(hi), Some(lo)) = (hi, lo) {
              if let (Some(hi_v), Some(lo_v)) = (hi.to_digit(16), lo.to_digit(16)) {
                if let Some(c) = char::from_u32(hi_v * 16 + lo_v) {
                  out.push(c);
                  continue;
                }
              }
            }
            out.push_str("\\x");
            if let Some(h) = hi {
              out.push(h);
            }
            if let Some(l) = lo {
              out.push(l);
            }
          }
          'u' => {
            let mut code = String::new();
            for _ in 0..4 {
              if let Some(d) = chars.next() {
                code.push(d);
              }
            }
            if code.len() == 4 {
              if let Ok(val) = u16::from_str_radix(&code, 16) {
                if let Some(c) = char::from_u32(val as u32) {
                  out.push(c);
                  continue;
                }
              }
            }
            out.push_str("\\u");
            out.push_str(&code);
          }
          _ => out.push(next),
        }
      }
    } else if ch == '%' {
      let a = chars.next();
      let b = chars.next();
      if let (Some(a), Some(b)) = (a, b) {
        if let (Some(hi), Some(lo)) = (a.to_digit(16), b.to_digit(16)) {
          if let Some(c) = char::from_u32(hi * 16 + lo) {
            out.push(c);
            continue;
          }
        }
        out.push('%');
        out.push(a);
        out.push(b);
      } else {
        out.push('%');
        if let Some(a) = a {
          out.push(a);
        }
        if let Some(b) = b {
          out.push(b);
        }
      }
    } else {
      out.push(ch);
    }
  }
  let mut out = out.replace("\\/", "/");
  out = out.replace("\\\\", "\\");
  out
}

fn normalize_attr_value(value: &str) -> String {
  let unescaped = value.replace("\\\"", "\"").replace("\\'", "'");
  let trimmed_slashes = unescaped.trim_end_matches('\\');
  let unquoted = trimmed_slashes.trim_matches(|c| c == '"' || c == '\'');
  trim_ascii_whitespace(unquoted).to_string()
}

fn parse_refresh_content(content: &str) -> Option<String> {
  let decoded = decode_refresh_entities(content);
  let bytes = decoded.as_bytes();

  // Only treat *immediate* refreshes as navigations. Browsers delay navigations when the leading
  // delay token is non-zero (e.g. `5; url=/next`). FastRender URL renders should only follow the
  // common `0; url=...` / `url=...` patterns.
  //
  // The refresh "delay" token is optional; when missing (e.g. `url=/next`) we treat it as `0`.
  let trimmed = trim_ascii_whitespace(&decoded);
  if !trimmed.is_empty() {
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    // Accept an optional sign to avoid treating `-1; url=...` as the missing-delay form.
    if bytes[i] == b'+' || bytes[i] == b'-' {
      i += 1;
    }
    if i < bytes.len() {
      let mut saw_digit = false;
      while i < bytes.len() && bytes[i].is_ascii_digit() {
        saw_digit = true;
        i += 1;
      }
      if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
          saw_digit = true;
          i += 1;
        }
      }
      if saw_digit {
        let token = &trimmed[..i];
        let Ok(delay) = token.parse::<f64>() else {
          return None;
        };
        if !delay.is_finite() || delay > 0.0 {
          return None;
        }
      }
    }
  }
  let lower: Vec<u8> = bytes.iter().map(|b| b.to_ascii_lowercase()).collect();

  let mut i = 0usize;
  while i + 2 < lower.len() {
    if lower[i] == b'u' && lower[i + 1] == b'r' && lower[i + 2] == b'l' {
      let prev_is_delim = i == 0
        || bytes[i - 1].is_ascii_whitespace()
        || bytes[i - 1] == b';'
        || bytes[i - 1] == b',';
      if prev_is_delim {
        let mut j = i + 3;
        while j < lower.len() && bytes[j].is_ascii_whitespace() {
          j += 1;
        }
        if j < lower.len() && bytes[j] == b'=' {
          j += 1;
          while j < lower.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
          }

          let value = slice_until_unquoted_semicolon(&decoded, j);
          let cleaned = trim_ascii_whitespace(value).trim_matches(['"', '\'']);
          if !cleaned.is_empty() {
            return Some(cleaned.to_string());
          }
        }
      }
    }

    // Skip over quoted segments so we don't match "url" inside a quoted URL value.
    if bytes[i] == b'"' || bytes[i] == b'\'' {
      let quote = bytes[i] as char;
      i += 1;
      while i < lower.len() {
        if bytes[i] as char == quote {
          break;
        }
        i += 1;
      }
    }

    i += 1;
  }

  None
}

fn decode_refresh_entities(content: &str) -> String {
  if !content.contains('&') {
    return content.to_string();
  }

  let mut out = String::with_capacity(content.len());
  let bytes = content.as_bytes();
  let mut i = 0usize;
  while i < bytes.len() {
    if bytes[i] != b'&' {
      out.push(bytes[i] as char);
      i += 1;
      continue;
    }

    let start = i;
    i += 1;
    let mut j = i;
    while j < bytes.len() && j - start <= 16 && bytes[j] != b';' {
      j += 1;
    }
    if j >= bytes.len() || bytes[j] != b';' {
      out.push('&');
      continue;
    }

    let raw_entity = &content[i..j];
    let entity = trim_ascii_whitespace(raw_entity);
    let decoded = if let Some(num) = entity.strip_prefix('#') {
      let trimmed = trim_ascii_whitespace(num);
      let parsed = if let Some(hex) = trimmed.strip_prefix(['x', 'X']) {
        u32::from_str_radix(hex, 16).ok()
      } else {
        trimmed.parse::<u32>().ok()
      };
      // Keep numeric entity decoding conservative to avoid turning escaped markup into actual tags
      // (e.g. `&#60;meta ...` -> `<meta ...`).
      match parsed {
        Some(34) => Some('"'),
        Some(38) => Some('&'),
        Some(39) => Some('\''),
        _ => None,
      }
    } else {
      let lowered = entity.to_ascii_lowercase();
      match lowered.as_str() {
        "quot" => Some('"'),
        "amp" => Some('&'),
        "apos" => Some('\''),
        _ => None,
      }
    };

    if let Some(ch) = decoded {
      out.push(ch);
      i = j + 1;
    } else {
      out.push_str(&content[start..=j]);
      i = j + 1;
    }
  }

  out
}

fn slice_until_unquoted_semicolon(s: &str, start: usize) -> &str {
  let mut in_quote: Option<char> = None;
  for (idx, ch) in s[start..].char_indices() {
    match in_quote {
      Some(q) if ch == q => in_quote = None,
      None => match ch {
        '"' | '\'' => in_quote = Some(ch),
        ';' => return &s[start..start + idx],
        _ => {}
      },
      _ => {}
    }
  }

  &s[start..]
}

#[cfg(test)]
mod tests {
  use super::extract_js_location_redirect;
  use super::extract_meta_refresh_url;

  #[test]
  fn extracts_meta_refresh_url() {
    let html =
      r"<html><head><meta http-equiv='refresh' content='0; url=/fallback.html'></head></html>";
    assert_eq!(
      extract_meta_refresh_url(html),
      Some("/fallback.html".to_string())
    );
  }

  #[test]
  fn extracts_quoted_and_entity_decoded_url() {
    let html = r#"<meta http-equiv="refresh" content="0;URL='https://example.com/?a=1&amp;b=2'">"#;
    assert_eq!(
      extract_meta_refresh_url(html),
      Some("https://example.com/?a=1&b=2".to_string())
    );
  }

  #[test]
  fn parses_quoted_meta_refresh_url() {
    let html = r#"
            <html><head>
            <noscript>
                <meta http-equiv=\"refresh\" content=\"0; url=&quot;https://html.duckduckgo.com/html&quot;\">
            </noscript>
            </head><body></body></html>
        "#;
    assert_eq!(
      extract_meta_refresh_url(html),
      Some("https://html.duckduckgo.com/html".to_string())
    );
  }

  #[test]
  fn extracts_meta_refresh_url_with_semicolon_in_value() {
    let html =
      r#"<meta http-equiv="REFRESH" content="0; URL='https://example.com/path;param=1?q=2'">"#;
    assert_eq!(
      extract_meta_refresh_url(html),
      Some("https://example.com/path;param=1?q=2".to_string())
    );
  }

  #[test]
  fn decodes_entities_in_refresh_url() {
    let html = r#"<meta http-equiv="refresh" content="0; url=&apos;/html/?q=1&amp;r=2&apos;">"#;
    assert_eq!(
      extract_meta_refresh_url(html),
      Some("/html/?q=1&r=2".to_string())
    );
  }

  #[test]
  fn preserves_non_ascii_whitespace_in_meta_refresh_url() {
    let nbsp = "\u{00A0}";
    let html = format!(r#"<meta http-equiv="refresh" content="0; url={nbsp}/fallback.html">"#);
    assert_eq!(
      extract_meta_refresh_url(&html),
      Some(format!("{nbsp}/fallback.html"))
    );
  }

  #[test]
  fn handles_refresh_without_delay() {
    let html = r#"<meta http-equiv="refresh" content="url=/noscript/landing">"#;
    assert_eq!(
      extract_meta_refresh_url(html),
      Some("/noscript/landing".to_string())
    );
  }

  #[test]
  fn ignores_non_refresh_meta() {
    let html = "<meta charset=\"utf-8\"><meta name='viewport' content='width=device-width'>";
    assert_eq!(extract_meta_refresh_url(html), None);
  }

  #[test]
  fn parses_common_meta_refresh_content_formats() {
    let html = r#"<meta http-equiv="refresh" content="0; url=/next">"#;
    assert_eq!(extract_meta_refresh_url(html), Some("/next".to_string()));

    let html = r#"<meta http-equiv="REFRESH" content="0; URL=/caps">"#;
    assert_eq!(extract_meta_refresh_url(html), Some("/caps".to_string()));

    let html = r#"<meta http-equiv="refresh" content="0; url='https://example.com'">"#;
    assert_eq!(
      extract_meta_refresh_url(html),
      Some("https://example.com".to_string())
    );

    let html = r#"<meta http-equiv="refresh" content=" 0 ;  URL =  '/spaced'  ">"#;
    assert_eq!(extract_meta_refresh_url(html), Some("/spaced".to_string()));
  }

  #[test]
  fn ignores_non_immediate_meta_refresh() {
    let html = r#"<meta http-equiv="refresh" content="5; url=/later">"#;
    assert_eq!(extract_meta_refresh_url(html), None);

    let html = r#"<meta http-equiv="refresh" content="0.25; url=/later">"#;
    assert_eq!(extract_meta_refresh_url(html), None);

    let html = r#"<meta http-equiv="refresh" content=".25; url=/later">"#;
    assert_eq!(extract_meta_refresh_url(html), None);
  }

  #[test]
  fn decodes_numeric_entities_in_refresh_url() {
    let html = r#"<meta http-equiv="refresh" content="0; url=/next?a=1&#38;b=2">"#;
    assert_eq!(
      extract_meta_refresh_url(html),
      Some("/next?a=1&b=2".to_string())
    );

    let html = r#"<meta http-equiv="refresh" content="0; url=/hex?a=1&#x26;b=2">"#;
    assert_eq!(
      extract_meta_refresh_url(html),
      Some("/hex?a=1&b=2".to_string())
    );
  }

  #[test]
  fn ignores_meta_refresh_inside_template() {
    let html = r#"
      <template><meta http-equiv="refresh" content="0; url=/bad"></template>
      <meta http-equiv="refresh" content="0; url=/good">
    "#;
    assert_eq!(extract_meta_refresh_url(html), Some("/good".to_string()));
  }

  #[test]
  fn ignores_meta_refresh_inside_scripts() {
    let html = r#"
      <script>var s = '<meta http-equiv="refresh" content="0; url=/bad">';</script>
      <meta http-equiv="refresh" content="0; url=/good">
    "#;
    assert_eq!(extract_meta_refresh_url(html), Some("/good".to_string()));
  }

  #[test]
  fn extracts_js_location_href() {
    let html = "<script>window.location.href = 'https://example.com/next';</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("https://example.com/next".to_string())
    );
  }

  #[test]
  fn extracts_js_location_href_with_entities() {
    let html = "<script>window.location.href=&quot;/entity/path&quot;;</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/entity/path".to_string())
    );
  }

  #[test]
  fn decodes_numeric_entities_in_js_redirect() {
    let html = "<script>location.href='/entity?a=1&#38;b=2';</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/entity?a=1&b=2".to_string())
    );
  }

  #[test]
  fn extracts_js_location_href_with_escaped_slashes() {
    let html = r#"<script>location.href = "https:\\/\\/example.com\\/next";</script>"#;
    assert_eq!(
      extract_js_location_redirect(html),
      Some("https://example.com/next".to_string())
    );

    let html = "<script>window.location.assign('https://example.com/assign');</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("https://example.com/assign".to_string())
    );

    let html = "<script>document.location.assign('/plain');</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/plain".to_string())
    );
  }

  #[test]
  fn unescapes_js_literal_sequences() {
    assert_eq!(
      super::unescape_js_literal("https:\\/\\/example.com\\/next"),
      "https://example.com/next"
    );
    assert_eq!(super::unescape_js_literal("/path\\x2fwith"), "/path/with");
    assert_eq!(
      super::unescape_js_literal("https:\\/\\/example.com\\/unicode\\u002fpath"),
      "https://example.com/unicode/path"
    );
    assert_eq!(
      super::unescape_js_literal("/encoded%2Fpath%20with"),
      "/encoded/path with"
    );
  }

  #[test]
  fn extracts_js_location_replace() {
    let html = "<script>location.replace(\"/foo\");</script>";
    assert_eq!(extract_js_location_redirect(html), Some("/foo".to_string()));
  }

  #[test]
  fn extracts_url_from_var_assignment() {
    let html = "<script>var url = \"//example.com/next\"; window.location.replace(url);</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("https://example.com/next".to_string())
    );
  }

  #[test]
  fn extracts_url_from_let_const_assignment() {
    let html = "<script>let url = '/from-let'; location.replace(url);</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/from-let".to_string())
    );

    let html = "<script>const url = '/from-const'; window.location.assign(url);</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/from-const".to_string())
    );
  }

  #[test]
  fn extracts_js_location_assignments() {
    let html = "<script>location = '/next';</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/next".to_string())
    );

    let html = "<script>window.location='/win';</script>";
    assert_eq!(extract_js_location_redirect(html), Some("/win".to_string()));

    let html = "<script>document.location = \"https:\\/\\/example.com\\/doc\";</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("https://example.com/doc".to_string())
    );

    let html = "<script>top.location = \"https:\\/\\/example.com\\/top?x=1\\u0026y=2\";</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("https://example.com/top?x=1&y=2".to_string())
    );

    let html = "<script>self.location = '/encoded%2Fpath%20with';</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/encoded/path with".to_string())
    );
  }

  #[test]
  fn extracts_js_location_from_body_onload() {
    let html = r#"<body onload="location.href='/from-onload'"></body>"#;
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/from-onload".to_string())
    );
  }

  #[test]
  fn extracts_js_location_qualified_calls_and_href_assignments() {
    let html = "<script>top.location.href = '/top-href';</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/top-href".to_string())
    );

    let html = "<script>parent.location.replace('/parent-replace');</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/parent-replace".to_string())
    );

    let html = "<script>self.location.assign('https://example.com/self-assign');</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("https://example.com/self-assign".to_string())
    );
  }

  #[test]
  fn extracts_js_location_backtick_literal() {
    let html = "<script>window.location = `/backtick`;</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/backtick".to_string())
    );
  }

  #[test]
  fn ignores_js_template_literal_interpolation() {
    let html = "<script>location = `/next?x=${y}`;</script>";
    assert_eq!(extract_js_location_redirect(html), None);
  }

  #[test]
  fn handles_escaped_quotes_in_js_string_literals() {
    let html = r#"<script>location.href = "https://example.com/with\"quote";</script>"#;
    assert_eq!(
      extract_js_location_redirect(html),
      Some("https://example.com/with\"quote".to_string())
    );
  }

  #[test]
  fn extracts_parenthesized_js_location_literals() {
    let html = "<script>location = ('/paren');</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/paren".to_string())
    );

    let html = "<script>window.location.href=(\"/paren-href\");</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/paren-href".to_string())
    );

    let html = "<script>location.replace((\"/paren-call\"));</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/paren-call".to_string())
    );
  }

  #[test]
  fn extracts_js_location_wrapped_decode_uri_literals() {
    let html = "<script>location.href = decodeURIComponent(\"%2Fwrapped%2Fnext\");</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/wrapped/next".to_string())
    );

    let html = "<script>location.replace(decodeURI('%2Fwrapped-uri'));</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/wrapped-uri".to_string())
    );

    let html = "<script>location = (decodeURIComponent(('%2Fdouble-paren')));</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/double-paren".to_string())
    );
  }

  #[test]
  fn ignores_js_location_wrapped_non_literals() {
    let html = "<script>location.href = decodeURIComponent(path);</script>";
    assert_eq!(extract_js_location_redirect(html), None);
  }

  #[test]
  fn ignores_location_pathname_reads() {
    let html = "<script>var p = location.pathname; console.log(p);</script>";
    assert_eq!(extract_js_location_redirect(html), None);
  }

  #[test]
  fn ignores_location_variable_declarations() {
    let html = "<script>var location = '/shadowed';</script>";
    assert_eq!(extract_js_location_redirect(html), None);

    let html = "<script>const location = '/shadowed';</script>";
    assert_eq!(extract_js_location_redirect(html), None);
  }

  #[test]
  fn ignores_non_window_location_properties() {
    let html = "<script>foo.location = '/not-a-redirect';</script>";
    assert_eq!(extract_js_location_redirect(html), None);

    let html = "<script>foo.location.href = '/not-a-redirect';</script>";
    assert_eq!(extract_js_location_redirect(html), None);

    let html = "<script>foo.location.replace('/not-a-redirect');</script>";
    assert_eq!(extract_js_location_redirect(html), None);
  }

  #[test]
  fn ignores_data_location_attributes() {
    // data-location attribute should not be mistaken for a JS redirect target.
    let html = r#"<head data-location="{\"minlon\":1}"></head>"#;
    assert_eq!(extract_js_location_redirect(html), None);
  }

  #[test]
  fn ignores_unrelated_url_var_assignments() {
    // Regression: `extract_js_location_redirect` used to treat any `var url = "...";` as a redirect,
    // even when the variable was only used for XHR/AJAX calls.
    let html = r#"<script>
      if (window.location.pathname == '/') {
        var url = '/ajax.pl?op=nel';
        $.ajax({ url: url, type: 'POST' });
      }
    </script>"#;
    assert_eq!(extract_js_location_redirect(html), None);
  }

  #[test]
  fn ignores_js_location_redirect_inside_template() {
    let html = "<template><script>location.href='/bad';</script></template><script>location.href='/good';</script>";
    assert_eq!(
      extract_js_location_redirect(html),
      Some("/good".to_string())
    );
  }
}
