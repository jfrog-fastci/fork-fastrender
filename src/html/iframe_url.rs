use std::borrow::Cow;

use crate::image_loader::resolve_against_base;
use crate::resource::is_data_url;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IframeNavigation {
  /// Navigate the iframe to `about:blank`.
  ///
  /// This covers both the HTML defaulting behavior when `src` is missing and explicit `about:blank`
  /// (including `about:blank#...` / `about:blank?...`) references.
  AboutBlank,
  /// Navigate the iframe to the provided (absolute or otherwise normalized) URL.
  Url(String),
  /// Do not navigate/render an iframe document.
  None,
}

// HTML URL-ish values strip leading/trailing ASCII whitespace (TAB/LF/FF/CR/SPACE) but do not treat
// all Unicode whitespace (e.g. NBSP) as ignorable.
fn trim_ascii_whitespace(value: &str) -> &str {
  value
    .trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
  value
    .as_bytes()
    .get(..prefix.len())
    .map(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
    .unwrap_or(false)
}

fn unescape_js_escapes(input: &str) -> Cow<'_, str> {
  if !input.contains('\\') {
    return Cow::Borrowed(input);
  }

  let mut out = String::with_capacity(input.len());
  let bytes = input.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'\\' {
      if i + 1 < bytes.len()
        && (bytes[i + 1] == b'"' || bytes[i + 1] == b'\'' || bytes[i + 1] == b'/')
      {
        out.push(bytes[i + 1] as char);
        i += 2;
        continue;
      }

      if i + 5 < bytes.len() && (bytes[i + 1] == b'u' || bytes[i + 1] == b'U') {
        if let Ok(code) = u16::from_str_radix(&input[i + 2..i + 6], 16) {
          if (0xD800..=0xDBFF).contains(&code) {
            out.push_str(&input[i..i + 6]);
            i += 6;
            if i + 5 < bytes.len()
              && bytes[i] == b'\\'
              && matches!(bytes[i + 1], b'u' | b'U')
              && u16::from_str_radix(&input[i + 2..i + 6], 16).is_ok()
            {
              out.push_str(&input[i..i + 6]);
              i += 6;
            }
            continue;
          }
          if (0xDC00..=0xDFFF).contains(&code) {
            out.push_str(&input[i..i + 6]);
            i += 6;
            continue;
          }
          if let Some(ch) = char::from_u32(code as u32) {
            out.push(ch);
            i += 6;
            continue;
          }
        }
      }
    }

    out.push(bytes[i] as char);
    i += 1;
  }

  Cow::Owned(out)
}

fn resolve_url_reference(url: &str, base_url: &str) -> String {
  let url = trim_ascii_whitespace(url);
  if url.is_empty() {
    return String::new();
  }

  // Absolute or data URLs can be returned directly.
  if is_data_url(url) {
    // Data URLs can appear in JS/CSS-escaped contexts inside HTML. Match `ImageCache::resolve_url`
    // and unescape simple backslash escapes so downstream parsers see valid markup.
    if url.contains('\\') {
      return unescape_js_escapes(url).into_owned();
    }
    return url.to_string();
  }
  if let Ok(parsed) = Url::parse(url) {
    return parsed.to_string();
  }

  if !base_url.is_empty() {
    if let Some(resolved) = resolve_against_base(base_url, url) {
      return resolved;
    }
  }

  url.to_string()
}

fn is_about_blank(url: &str) -> bool {
  const PREFIX: &str = "about:blank";
  let Some(head) = url.get(..PREFIX.len()) else {
    return false;
  };
  if !head.eq_ignore_ascii_case(PREFIX) {
    return false;
  }
  matches!(
    url.as_bytes().get(PREFIX.len()),
    None | Some(b'#') | Some(b'?')
  )
}

pub fn iframe_navigation_from_src(raw_src: Option<&str>, base_url: &str) -> IframeNavigation {
  let Some(raw_src) = raw_src else {
    return IframeNavigation::AboutBlank;
  };

  let src = trim_ascii_whitespace(raw_src);
  if src.is_empty() {
    return IframeNavigation::None;
  }
  if src.starts_with('#')
    || starts_with_ignore_ascii_case(src, "javascript:")
    || starts_with_ignore_ascii_case(src, "vbscript:")
    || starts_with_ignore_ascii_case(src, "mailto:")
  {
    return IframeNavigation::None;
  }

  let resolved = resolve_url_reference(src, base_url);
  if resolved.is_empty() {
    return IframeNavigation::None;
  }
  if is_about_blank(&resolved) {
    return IframeNavigation::AboutBlank;
  }

  IframeNavigation::Url(resolved)
}

#[cfg(test)]
mod tests {
  use super::{iframe_navigation_from_src, unescape_js_escapes, IframeNavigation};

  #[test]
  fn iframe_navigation_whitespace_only_is_none() {
    assert_eq!(
      iframe_navigation_from_src(Some("   "), "https://example.com/"),
      IframeNavigation::None
    );
  }

  #[test]
  fn iframe_navigation_fragment_only_is_none() {
    assert_eq!(
      iframe_navigation_from_src(Some("#"), "https://example.com/"),
      IframeNavigation::None
    );
  }

  #[test]
  fn iframe_navigation_javascript_is_none() {
    assert_eq!(
      iframe_navigation_from_src(Some("javascript:alert(1)"), "https://example.com/"),
      IframeNavigation::None
    );
  }

  #[test]
  fn iframe_navigation_missing_src_defaults_to_about_blank() {
    assert_eq!(
      iframe_navigation_from_src(None, "https://example.com/"),
      IframeNavigation::AboutBlank
    );
  }

  #[test]
  fn iframe_navigation_trims_ascii_whitespace() {
    let nav = iframe_navigation_from_src(Some(" \t  https://example.com"), "https://bad.example/");
    assert_eq!(
      nav,
      IframeNavigation::Url("https://example.com/".to_string())
    );
  }

  #[test]
  fn iframe_navigation_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let src = format!("foo{nbsp}");
    let nav = iframe_navigation_from_src(Some(&src), "https://example.com/");
    assert_eq!(
      nav,
      IframeNavigation::Url("https://example.com/foo%C2%A0".to_string())
    );
  }

  #[test]
  fn unescape_js_escapes_preserves_unpaired_surrogates() {
    let input = r"\uD800\u0061";
    let out = unescape_js_escapes(input);
    assert_eq!(out, input);
  }
}
