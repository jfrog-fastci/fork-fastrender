//! Helpers for loading and inlining external stylesheets.
//!
//! These utilities resolve stylesheet URLs against a base, rewrite relative
//! `url(...)` references to absolute URLs, inline `@import` rules, and inject
//! fetched CSS into an HTML document. They are shared by the developer
//! tooling binaries so cached pages can be rendered with their real styles.

use crate::css::parser::{
  extract_scoped_css_sources, rel_list_contains_stylesheet, tokenize_rel_list, StylesheetSource,
};
use crate::debug::runtime;
use crate::dom::{DomNode, DomNodeType, DomParseOptions, HTML_NAMESPACE};
use crate::error::{Error, RenderError, RenderStage, Result};
use crate::render_control::{check_active, check_active_periodic, RenderDeadline};
use crate::resource::CorsMode;
use crate::resource::ReferrerPolicy;
use crate::url_normalize::{
  normalize_http_url_for_resolution, normalize_url_reference_for_resolution,
};
use cssparser::{serialize_identifier, Parser, ParserInput, Token};
use rustc_hash::{FxHashMap, FxHashSet};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use url::Url;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(test)]
thread_local! {
  static ABSOLUTIZE_CSS_URLS_TOKENIZE_COUNT: AtomicUsize = const { AtomicUsize::new(0) };
}

#[cfg(test)]
fn reset_absolutize_css_urls_tokenize_count() {
  ABSOLUTIZE_CSS_URLS_TOKENIZE_COUNT.with(|counter| counter.store(0, Ordering::Relaxed));
}

#[cfg(test)]
fn absolutize_css_urls_tokenize_count() -> usize {
  ABSOLUTIZE_CSS_URLS_TOKENIZE_COUNT.with(|counter| counter.load(Ordering::Relaxed))
}

// HTML/CSS URL attributes strip leading/trailing ASCII whitespace (TAB/LF/FF/CR/SPACE) but do not
// treat all Unicode whitespace as ignorable. Use an explicit trim instead of `str::trim()` to
// avoid incorrectly dropping characters like NBSP (U+00A0).
fn is_ascii_whitespace_html_css(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

fn is_ascii_whitespace_html_css_byte(b: u8) -> bool {
  matches!(b, b'\t' | b'\n' | b'\x0C' | b'\r' | b' ')
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(is_ascii_whitespace_html_css)
}

fn trim_ascii_whitespace_start(value: &str) -> &str {
  value.trim_start_matches(is_ascii_whitespace_html_css)
}

fn trim_ascii_whitespace_end(value: &str) -> &str {
  value.trim_end_matches(is_ascii_whitespace_html_css)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CssLinkCandidate {
  pub url: String,
  pub crossorigin: Option<CorsMode>,
  pub referrer_policy: Option<ReferrerPolicy>,
}

/// Determine whether a tokenized `<link rel>` list should be treated as a stylesheet.
///
/// This is used by both the HTML string pre-pass (`extract_css_links`) and the DOM-based
/// stylesheet loader (`FastRender::collect_document_style_set`) so the two discovery paths stay
/// in sync.
pub fn link_rel_is_stylesheet_candidate(
  rel_tokens: &[String],
  as_attr: Option<&str>,
  preload_stylesheets_enabled: bool,
  modulepreload_stylesheets_enabled: bool,
  alternate_stylesheets_enabled: bool,
) -> bool {
  let rel_has_stylesheet = rel_list_contains_stylesheet(rel_tokens);
  let rel_has_alternate = rel_tokens
    .iter()
    .any(|t| t.eq_ignore_ascii_case("alternate"));
  let rel_has_preload = rel_tokens.iter().any(|t| t.eq_ignore_ascii_case("preload"));
  let rel_has_modulepreload = rel_tokens
    .iter()
    .any(|t| t.eq_ignore_ascii_case("modulepreload"));
  let as_style = as_attr
    .map(|v| trim_ascii_whitespace(v).eq_ignore_ascii_case("style"))
    .unwrap_or(false);

  let mut is_stylesheet_link =
    rel_has_stylesheet && (alternate_stylesheets_enabled || !rel_has_alternate);

  if !is_stylesheet_link && preload_stylesheets_enabled && rel_has_preload && as_style {
    is_stylesheet_link = true;
  }

  if !is_stylesheet_link && modulepreload_stylesheets_enabled && rel_has_modulepreload && as_style {
    is_stylesheet_link = true;
  }

  is_stylesheet_link
}

/// Resolve a possibly-relative `href` against a base URL.
///
/// Supports protocol-relative URLs (`//example.com`), `data:` URLs (returned
/// as-is), absolute URLs, and filesystem bases (`file://`) that may reference
/// directory paths. JavaScript-escaped hrefs (e.g. `https:\/\/example.com`) are
/// unescaped before resolution.
pub fn resolve_href(base: &str, href: &str) -> Option<String> {
  let href = unescape_js_escapes(href);
  let href = trim_ascii_whitespace(href.as_ref());
  if href.is_empty() {
    return None;
  }

  fn starts_with_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
  }

  fn looks_like_absolute_url(bytes: &[u8]) -> bool {
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
      return false;
    }
    let mut idx = 1usize;
    while idx < bytes.len() {
      match bytes[idx] {
        b':' => return true,
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'+' | b'-' | b'.' => idx += 1,
        _ => return false,
      }
    }
    false
  }

  // CSS/HTML authors sometimes escape path characters with backslashes. The WHATWG URL parser
  // treats `\` as a path separator for special schemes, but for our resource fetching and URL
  // rewriting we want a stable percent-encoded representation instead.
  let href = if href.contains('\\') {
    Cow::Owned(href.replace('\\', "%5C"))
  } else {
    Cow::Borrowed(href)
  };

  // Ignore fragment-only hrefs (e.g., "#section") since they don't resolve to fetchable stylesheets.
  if href.starts_with('#') {
    return None;
  }

  let href_bytes = href.as_ref().as_bytes();
  if starts_with_ignore_ascii_case(href_bytes, b"data:") {
    return Some(href.into_owned());
  }

  if starts_with_ignore_ascii_case(href_bytes, b"javascript:")
    || starts_with_ignore_ascii_case(href_bytes, b"vbscript:")
    || starts_with_ignore_ascii_case(href_bytes, b"mailto:")
  {
    return None;
  }

  // Avoid invoking the full WHATWG URL parser for the common relative-path case.
  if looks_like_absolute_url(href_bytes) {
    if starts_with_ignore_ascii_case(href_bytes, b"http://")
      || starts_with_ignore_ascii_case(href_bytes, b"https://")
    {
      let normalized = normalize_http_url_for_resolution(href.as_ref());
      if let Ok(abs) = Url::parse(normalized.as_ref()) {
        return Some(abs.to_string());
      }
      if normalized.as_ref() != href.as_ref() {
        if let Ok(abs) = Url::parse(href.as_ref()) {
          return Some(abs.to_string());
        }
      }
    } else if let Ok(abs) = Url::parse(href.as_ref()) {
      return Some(abs.to_string());
    }
  }

  let mut base_candidate: Cow<'_, str> = Cow::Borrowed(base);
  if base_candidate.starts_with("file://") {
    let path = &base_candidate["file://".len()..];
    if Path::new(path).is_dir() && !base_candidate.ends_with('/') {
      let mut owned = base_candidate.into_owned();
      owned.push('/');
      base_candidate = Cow::Owned(owned);
    }
  }

  let base_url = Url::parse(base_candidate.as_ref())
    .or_else(|_| {
      Url::from_file_path(base_candidate.as_ref())
        .map_err(|()| url::ParseError::RelativeUrlWithoutBase)
    })
    .ok()?;

  let normalized_href = normalize_url_reference_for_resolution(href.as_ref());
  if normalized_href.as_ref() != href.as_ref() {
    if let Ok(joined) = base_url.join(normalized_href.as_ref()) {
      return Some(joined.to_string());
    }
  }

  base_url.join(href.as_ref()).ok().map(|u| u.to_string())
}

/// Resolve an href against an optional base, returning absolute URLs when possible.
///
/// When no base is provided, absolute URLs (including `data:`) are returned as-is while
/// relative URLs are ignored.
pub fn resolve_href_with_base(base: Option<&str>, href: &str) -> Option<String> {
  match base {
    Some(base) => resolve_href(base, href),
    None => resolve_href("", href),
  }
}

/// Best-effort unescaping for JavaScript-escaped URL strings embedded in HTML/JS.
///
/// Handles `\uXXXX`/`\UXXXX` Unicode escapes (common for `\u0026` encoded ampersands)
/// and `\xHH` byte escapes, plus simple backslash escaping of quotes or slashes.
/// Surrogate pairs (`\uD83D\uDE00`) are recognized and combined.
///
/// If the input contains no backslashes, it returns a borrowed slice to avoid
/// allocations; otherwise it builds a new string with the escapes resolved.
///
/// # Examples
///
/// ```rust,ignore
/// use fastrender::css::loader::unescape_js_escapes;
///
/// assert_eq!(unescape_js_escapes("https:\\u002f\\u002fexample.com"), "https://example.com");
/// assert_eq!(unescape_js_escapes(r#"https:\/\/example.com\/path"#), "https://example.com/path");
/// ```
fn unescape_js_escapes(input: &str) -> Cow<'_, str> {
  if !input.contains('\\') {
    return Cow::Borrowed(input);
  }

  let mut out = String::with_capacity(input.len());
  let bytes = input.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'\\' {
      if let Some(next) = bytes.get(i + 1) {
        match next {
          b'"' | b'\'' | b'/' => {
            out.push(*next as char);
            i += 2;
            continue;
          }
          b'x' | b'X' => {
            if i + 3 < bytes.len() {
              if let Ok(code) = u8::from_str_radix(&input[i + 2..i + 4], 16) {
                out.push(code as char);
                i += 4;
                continue;
              }
            }
          }
          b'u' | b'U' => {
            // Modern JS escape: `\u{...}`
            if bytes.get(i + 2) == Some(&b'{') {
              if let Some(end) = bytes[i + 3..].iter().position(|b| *b == b'}') {
                let end_idx = i + 3 + end;
                let len = end_idx - (i + 3);
                if (1..=6).contains(&len) {
                  if let Ok(code) = u32::from_str_radix(&input[i + 3..end_idx], 16) {
                    if let Some(ch) = char::from_u32(code) {
                      out.push(ch);
                      i = end_idx + 1;
                      continue;
                    }
                  }
                }
              }
            }

            // Classic JS escape: `\uXXXX`
            if i + 5 < bytes.len() {
              if let Ok(code) = u16::from_str_radix(&input[i + 2..i + 6], 16) {
                // Surrogate pair handling for non-BMP code points.
                if (0xD800..=0xDBFF).contains(&code) && i + 11 < bytes.len() {
                  if bytes.get(i + 6) == Some(&b'\\')
                    && matches!(bytes.get(i + 7), Some(b'u' | b'U'))
                  {
                    if let Ok(low) = u16::from_str_radix(&input[i + 8..i + 12], 16) {
                      if (0xDC00..=0xDFFF).contains(&low) {
                        let high = (code - 0xD800) as u32;
                        let low = (low - 0xDC00) as u32;
                        let combined = 0x10000 + ((high << 10) | low);
                        if let Some(ch) = char::from_u32(combined) {
                          out.push(ch);
                          i += 12;
                          continue;
                        }
                      }
                    }
                  }
                }

                // JavaScript strings can contain unpaired surrogates, but Rust `char` values cannot.
                //
                // When we encounter a surrogate that we cannot merge into a valid pair, preserve it
                // (and any immediately following `\uXXXX` escape) verbatim. This avoids partially
                // decoding UTF-16 escape sequences into a mixed "escape + decoded char" string.
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
          _ => {}
        }
      }
    }

    out.push(bytes[i] as char);
    i += 1;
  }

  Cow::Owned(out)
}

fn normalize_embedded_css_candidate(candidate: &str) -> Option<String> {
  let mut cleaned =
    trim_ascii_whitespace(candidate.trim_matches(|c: char| matches!(c, '"' | '\'' | '(' | ')')))
      .to_string();

  if cleaned.is_empty() {
    return None;
  }

  // Strip common sourceURL markers that get inlined with CSS text (e.g.,
  // "sourceURL=https://example.com/style.css").
  const SOURCEURL_PREFIX: &str = "sourceurl=";
  if cleaned
    .get(..SOURCEURL_PREFIX.len())
    .map(|prefix| prefix.eq_ignore_ascii_case(SOURCEURL_PREFIX))
    .unwrap_or(false)
  {
    if let Some((_, rest)) = cleaned.split_once('=') {
      cleaned = rest.to_string();
    }
  }

  fn rfind_dot_css(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    if bytes.len() < 4 {
      return None;
    }
    for idx in (0..=bytes.len() - 4).rev() {
      if bytes[idx] == b'.'
        && matches!(bytes[idx + 1], b'c' | b'C')
        && matches!(bytes[idx + 2], b's' | b'S')
        && matches!(bytes[idx + 3], b's' | b'S')
      {
        return Some(idx);
      }
    }
    None
  }

  if let Some(pos) = rfind_dot_css(&cleaned) {
    let trailing = &cleaned[pos + 4..];
    if trailing.chars().all(|c| c == '/') {
      cleaned.truncate(pos + 4);
    }
  }

  cleaned = decode_html_entities(&cleaned);
  cleaned = unescape_js_escapes(&cleaned).into_owned();
  cleaned = normalize_scheme_slashes(&cleaned);
  if cleaned.contains('\\') {
    cleaned = cleaned.replace('\\', "");
  }

  if cleaned.is_empty() {
    None
  } else {
    Some(cleaned)
  }
}

/// Rewrite `url(...)` references in a CSS string to be absolute using the stylesheet's base URL.
///
/// This walks cssparser tokens so only real `url` tokens are rewritten (including nested
/// `url()` calls inside other functions/blocks). Strings and comments are preserved verbatim.
pub fn absolutize_css_urls_cow<'a>(
  css: &'a str,
  base_url: &str,
) -> std::result::Result<Cow<'a, str>, RenderError> {
  fn push_escaped_url_for_css(out: &mut String, url: &str) {
    if !url
      .as_bytes()
      .iter()
      .any(|b| matches!(*b, b'"' | b'\\' | b'\n' | b'\r' | b'\t'))
    {
      out.push_str(url);
      return;
    }

    for ch in url.chars() {
      match ch {
        '"' => out.push_str("\\\""),
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\0a "),
        '\r' => out.push_str("\\0d "),
        '\t' => out.push_str("\\09 "),
        _ => out.push(ch),
      }
    }
  }

  fn looks_like_absolute_url(url: &str) -> bool {
    let bytes = url.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
      return false;
    }
    let mut idx = 1usize;
    while idx < bytes.len() {
      match bytes[idx] {
        b':' => return true,
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'+' | b'-' | b'.' => idx += 1,
        _ => return false,
      }
    }
    false
  }

  fn trim_ascii_whitespace(value: &str) -> &str {
    value
      .trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
  }

  fn should_resolve_css_url(url: &str) -> bool {
    let trimmed = trim_ascii_whitespace(url);
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('<') {
      return false;
    }
    // Inline SVG markup is supported by the image loader (it treats strings beginning with
    // `<svg` as a renderable document). Resolving these values against `base_url` turns them into
    // bogus network URLs like `https://example.com/%3Csvg...`, causing noisy pageset failures.
    if trimmed.starts_with('<')
      || trimmed
        .get(.."%3csvg".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("%3csvg"))
    {
      return false;
    }
    !looks_like_absolute_url(trimmed)
  }

  fn css_may_contain_resolvable_url_tokens(css: &str) -> bool {
    let bytes = css.as_bytes();
    if bytes.len() < 4 {
      return false;
    }

    fn matches_ignore_ascii_case_at(bytes: &[u8], at: usize, needle: &[u8]) -> bool {
      bytes
        .get(at..at + needle.len())
        .is_some_and(|slice| slice.eq_ignore_ascii_case(needle))
    }

    let mut in_string: Option<u8> = None;
    let mut in_comment = false;
    let mut i = 0usize;
    while i + 3 < bytes.len() {
      let byte = bytes[i];

      if in_comment {
        if byte == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
          in_comment = false;
          i += 2;
          continue;
        }
        i += 1;
        continue;
      }

      if let Some(quote) = in_string {
        if byte == b'\\' {
          i = (i + 2).min(bytes.len());
          continue;
        }
        if byte == quote {
          in_string = None;
        }
        i += 1;
        continue;
      }

      // Not inside a string/comment.
      if byte == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
        in_comment = true;
        i += 2;
        continue;
      }

      if byte == b'"' || byte == b'\'' {
        in_string = Some(byte);
        i += 1;
        continue;
      }

      if matches_ignore_ascii_case_at(bytes, i, b"image-set(")
        || matches_ignore_ascii_case_at(bytes, i, b"-webkit-image-set(")
      {
        return true;
      }

      if matches!(byte, b'u' | b'U')
        && matches!(bytes[i + 1], b'r' | b'R')
        && matches!(bytes[i + 2], b'l' | b'L')
        && bytes[i + 3] == b'('
      {
        // Scan ahead for the start of the URL payload and decide whether it might need resolution.
        let mut j = i + 4;
        while j < bytes.len() && is_ascii_whitespace_html_css_byte(bytes[j]) {
          j += 1;
        }
        if j >= bytes.len() {
          return true;
        }

        // Empty url() calls do not produce resolvable URLs.
        if bytes[j] == b')' {
          i = i.saturating_add(4);
          continue;
        }

        if matches!(bytes[j], b'"' | b'\'') {
          j += 1;
          while j < bytes.len() && is_ascii_whitespace_html_css_byte(bytes[j]) {
            j += 1;
          }
          if j >= bytes.len() {
            return true;
          }
        }

        let mut end = j;
        while end < bytes.len() && end.saturating_sub(j) < 64 {
          let b = bytes[end];
          if is_ascii_whitespace_html_css_byte(b) || matches!(b, b')' | b'"' | b'\'') {
            break;
          }
          if b == b'\\' {
            // Escapes inside url(...) need a real tokenizer.
            return true;
          }
          if b == b'/' && end + 1 < bytes.len() && bytes[end + 1] == b'*' {
            // Comments inside url(...) need a real tokenizer.
            return true;
          }
          end += 1;
        }

        let prefix = trim_ascii_whitespace(&css[j..end]);
        if prefix.is_empty() {
          i = i.saturating_add(4);
          continue;
        }

        if should_resolve_css_url(prefix) {
          return true;
        }

        i = i.saturating_add(4);
        continue;
      }

      i += 1;
    }

    false
  }

  fn parse_base_url_for_join(base_url: &str) -> Option<Url> {
    if base_url.starts_with("file://") && !base_url.ends_with('/') {
      let path = &base_url["file://".len()..];
      if Path::new(path).is_dir() {
        let mut candidate = base_url.to_string();
        candidate.push('/');
        return Url::parse(&candidate)
          .or_else(|_| {
            Url::from_file_path(&candidate).map_err(|()| url::ParseError::RelativeUrlWithoutBase)
          })
          .ok();
      }
    }

    Url::parse(base_url)
      .or_else(|_| {
        Url::from_file_path(base_url).map_err(|()| url::ParseError::RelativeUrlWithoutBase)
      })
      .ok()
  }

  enum ResolvedCssUrl {
    Joined(Url),
    Owned(String),
  }

  impl ResolvedCssUrl {
    fn as_str(&self) -> &str {
      match self {
        Self::Joined(url) => url.as_str(),
        Self::Owned(s) => s.as_str(),
      }
    }
  }

  struct BaseUrlJoinCache<'a> {
    raw: &'a str,
    parsed: Option<Url>,
    attempted: bool,
  }

  impl<'a> BaseUrlJoinCache<'a> {
    fn new(raw: &'a str) -> Self {
      Self {
        raw,
        parsed: None,
        attempted: false,
      }
    }

    fn parsed(&mut self) -> Option<&Url> {
      if self.attempted {
        return self.parsed.as_ref();
      }
      self.attempted = true;
      self.parsed = parse_base_url_for_join(self.raw);
      self.parsed.as_ref()
    }
  }

  fn resolve_css_url(base: &mut BaseUrlJoinCache<'_>, href: &str) -> Option<ResolvedCssUrl> {
    let href = unescape_js_escapes(href);
    let href = trim_ascii_whitespace(href.as_ref());
    if href.is_empty() {
      return None;
    }

    let href = if href.contains('\\') {
      Cow::Owned(href.replace('\\', "%5C"))
    } else {
      Cow::Borrowed(href)
    };

    match base.parsed() {
      Some(base) => base.join(href.as_ref()).ok().map(ResolvedCssUrl::Joined),
      None => resolve_href(base.raw, href.as_ref()).map(ResolvedCssUrl::Owned),
    }
  }

  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  enum UrlRewriteMode {
    Normal,
    /// Rewriting inside `image-set()` / `-webkit-image-set()` argument lists.
    ///
    /// CSS Images allows candidates to be specified as a quoted string URL (e.g.
    /// `image-set("foo.png" 1x, url(bar.png) 2x)`). Those quoted strings are treated as URL
    /// references and must resolve relative to the stylesheet base URL, not the document base.
    ImageSet,
  }

  fn rewrite_urls_in_parser<'i, 't>(
    parser: &mut Parser<'i, 't>,
    base_url: &mut BaseUrlJoinCache<'_>,
    capacity_hint: usize,
    deadline_counter: &mut usize,
    mode: UrlRewriteMode,
  ) -> std::result::Result<Cow<'i, str>, RenderError> {
    let start_pos = parser.position();
    let mut out: Option<String> = None;
    let mut last_emitted = start_pos;
    let mut image_set_candidate_start = matches!(mode, UrlRewriteMode::ImageSet);

    check_active(RenderStage::Css)?;
    // `Parser::is_exhausted()` ignores trailing whitespace/comments, but this routine must preserve
    // them verbatim when rewriting nested blocks. Drive the loop solely via
    // `next_including_whitespace_and_comments()` so `parser.position()` advances to the true end
    // of the input slice.
    loop {
      check_active_periodic(deadline_counter, 256, RenderStage::Css)?;
      let token_start = parser.position();
      let token = match parser.next_including_whitespace_and_comments() {
        Ok(t) => t,
        Err(_) => break,
      };

      let is_whitespace_or_comment = matches!(token, Token::WhiteSpace(_) | Token::Comment(_));
      let is_image_set_comma = matches!(token, Token::Comma);
      let is_image_set_candidate_token = matches!(mode, UrlRewriteMode::ImageSet)
        && image_set_candidate_start
        && !is_whitespace_or_comment
        && !is_image_set_comma;
      let is_image_set_string_candidate =
        is_image_set_candidate_token && matches!(token, Token::QuotedString(_));

      if matches!(mode, UrlRewriteMode::ImageSet) {
        if is_image_set_comma {
          image_set_candidate_start = true;
        } else if !is_whitespace_or_comment && image_set_candidate_start {
          // We've seen the first real token for this candidate (whether it's a string URL, url(),
          // gradient, etc). Subsequent quoted strings (e.g. inside `type("image/avif")`) are not
          // URL candidates.
          image_set_candidate_start = false;
        }
      }

      match token {
        Token::QuotedString(s) if is_image_set_string_candidate => {
          let url_arg = s.as_ref();
          if !should_resolve_css_url(url_arg) {
            continue;
          }
          let Some(resolved) = resolve_css_url(base_url, url_arg) else {
            continue;
          };

          let token_text = parser.slice_from(token_start);
          let chunk = parser.slice_from(last_emitted);
          let prefix_len = chunk.len().saturating_sub(token_text.len());
          let out = out.get_or_insert_with(|| String::with_capacity(capacity_hint));
          out.push_str(&chunk[..prefix_len]);

          out.push('"');
          push_escaped_url_for_css(out, resolved.as_str());
          out.push('"');

          last_emitted = parser.position();
        }
        Token::UnquotedUrl(url_value) => {
          let url_value = url_value.as_ref();
          if !should_resolve_css_url(url_value) {
            continue;
          }
          let Some(resolved) = resolve_css_url(base_url, url_value) else {
            continue;
          };

          let token_text = parser.slice_from(token_start);
          let chunk = parser.slice_from(last_emitted);
          let prefix_len = chunk.len().saturating_sub(token_text.len());
          let out = out.get_or_insert_with(|| String::with_capacity(capacity_hint));
          out.push_str(&chunk[..prefix_len]);

          out.push_str("url(\"");
          push_escaped_url_for_css(out, resolved.as_str());
          out.push_str("\")");

          last_emitted = parser.position();
        }
        Token::Function(ref name) if name.eq_ignore_ascii_case("url") => {
          let parse_result = parser.parse_nested_block(|nested| {
            let mut arg: Option<cssparser::CowRcStr<'i>> = None;

            while !nested.is_exhausted() {
              match nested.next_including_whitespace_and_comments() {
                Ok(Token::WhiteSpace(_)) | Ok(Token::Comment(_)) => {}
                Ok(Token::QuotedString(s)) | Ok(Token::UnquotedUrl(s)) => {
                  arg = Some(s.clone());
                  break;
                }
                Ok(Token::Ident(s)) => {
                  arg = Some(s.clone());
                  break;
                }
                Ok(Token::BadUrl(_)) => {
                  arg = None;
                  break;
                }
                Ok(_) => {}
                Err(_) => break,
              }
            }

            Ok::<_, cssparser::ParseError<'i, ()>>(arg)
          });

          let Ok(Some(url_arg)) = parse_result else {
            continue;
          };
          let url_arg = url_arg.as_ref();
          if !should_resolve_css_url(url_arg) {
            continue;
          }
          let Some(resolved) = resolve_css_url(base_url, url_arg) else {
            continue;
          };

          let block_text = parser.slice_from(token_start);
          let chunk = parser.slice_from(last_emitted);
          let prefix_len = chunk.len().saturating_sub(block_text.len());
          let out = out.get_or_insert_with(|| String::with_capacity(capacity_hint));
          out.push_str(&chunk[..prefix_len]);

           out.push_str("url(\"");
           push_escaped_url_for_css(out, resolved.as_str());
           out.push_str("\")");

          last_emitted = parser.position();
        }
        Token::Function(ref name)
          if name.eq_ignore_ascii_case("image-set")
            || name.eq_ignore_ascii_case("-webkit-image-set") =>
        {
          let open_len = parser.slice_from(token_start).len();
          let mut nested_error: Option<RenderError> = None;
          let parse_result = parser.parse_nested_block(|nested| {
            let rewritten = match rewrite_urls_in_parser(
              nested,
              base_url,
              0,
              deadline_counter,
              UrlRewriteMode::ImageSet,
            ) {
              Ok(r) => r,
              Err(err) => {
                nested_error = Some(err);
                return Err(nested.new_custom_error::<(), ()>(()));
              }
            };
            let changed = matches!(rewritten, Cow::Owned(_));
            Ok::<_, cssparser::ParseError<'i, ()>>((rewritten, changed))
          });

          if let Some(err) = nested_error {
            return Err(err);
          }
          let Ok((inner_rewritten, changed)) = parse_result else {
            continue;
          };
          if !changed {
            continue;
          }

          let block_text = parser.slice_from(token_start);
          const CLOSING_LEN: usize = 1;
          if block_text.len() < open_len + CLOSING_LEN {
            continue;
          }

          let chunk = parser.slice_from(last_emitted);
          let prefix_len = chunk.len().saturating_sub(block_text.len());
          let out = out.get_or_insert_with(|| String::with_capacity(capacity_hint));
          out.push_str(&chunk[..prefix_len]);

          let close_part = &block_text[block_text.len() - CLOSING_LEN..];
          out.push_str(&block_text[..open_len]);
          out.push_str(inner_rewritten.as_ref());
          out.push_str(close_part);

          last_emitted = parser.position();
        }
        Token::Function(_)
        | Token::ParenthesisBlock
        | Token::SquareBracketBlock
        | Token::CurlyBracketBlock => {
          let open_len = parser.slice_from(token_start).len();
          let mut nested_error: Option<RenderError> = None;
          let parse_result = parser.parse_nested_block(|nested| {
            let rewritten = match rewrite_urls_in_parser(
              nested,
              base_url,
              0,
              deadline_counter,
              UrlRewriteMode::Normal,
            ) {
              Ok(r) => r,
              Err(err) => {
                nested_error = Some(err);
                return Err(nested.new_custom_error::<(), ()>(()));
              }
            };
            let changed = matches!(rewritten, Cow::Owned(_));
            Ok::<_, cssparser::ParseError<'i, ()>>((rewritten, changed))
          });

          if let Some(err) = nested_error {
            return Err(err);
          }
          let Ok((inner_rewritten, changed)) = parse_result else {
            continue;
          };
          if !changed {
            continue;
          }

          let block_text = parser.slice_from(token_start);
          const CLOSING_LEN: usize = 1;
          if block_text.len() < open_len + CLOSING_LEN {
            continue;
          }

          let chunk = parser.slice_from(last_emitted);
          let prefix_len = chunk.len().saturating_sub(block_text.len());
          let out = out.get_or_insert_with(|| String::with_capacity(capacity_hint));
          out.push_str(&chunk[..prefix_len]);

          let close_part = &block_text[block_text.len() - CLOSING_LEN..];
          out.push_str(&block_text[..open_len]);
          out.push_str(inner_rewritten.as_ref());
          out.push_str(close_part);

          last_emitted = parser.position();
        }
        _ => {}
      }
    }

    let Some(mut out) = out else {
      return Ok(Cow::Borrowed(parser.slice_from(start_pos)));
    };
    out.push_str(parser.slice_from(last_emitted));
    Ok(Cow::Owned(out))
  }

  check_active(RenderStage::Css)?;
  if !css_may_contain_resolvable_url_tokens(css) {
    return Ok(Cow::Borrowed(css));
  }

  let mut base_url = BaseUrlJoinCache::new(base_url);
  #[cfg(test)]
  ABSOLUTIZE_CSS_URLS_TOKENIZE_COUNT.with(|counter| {
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
  });
  let mut input = ParserInput::new(css);
  let mut parser = Parser::new(&mut input);
  let mut deadline_counter = 0usize;
  rewrite_urls_in_parser(
    &mut parser,
    &mut base_url,
    css.len(),
    &mut deadline_counter,
    UrlRewriteMode::Normal,
  )
}

pub fn absolutize_css_urls(css: &str, base_url: &str) -> std::result::Result<String, RenderError> {
  Ok(absolutize_css_urls_cow(css, base_url)?.into_owned())
}

fn css_ident_continues(byte: u8) -> bool {
  byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'\\' || byte >= 0x80
}

fn matches_at_keyword(bytes: &[u8], at_pos: usize, keyword: &[u8]) -> bool {
  if bytes.get(at_pos).copied() != Some(b'@') {
    return false;
  }
  let start = at_pos + 1;
  let end = start.saturating_add(keyword.len());
  if end > bytes.len() {
    return false;
  }
  if !bytes[start..end].eq_ignore_ascii_case(keyword) {
    return false;
  }
  match bytes.get(end).copied() {
    None => true,
    Some(next) => !css_ident_continues(next),
  }
}

fn consume_nested_tokens<'i, 't, E>(
  parser: &mut Parser<'i, 't>,
) -> std::result::Result<(), cssparser::ParseError<'i, E>> {
  while !parser.is_exhausted() {
    let token = parser.next_including_whitespace()?;
    match token {
      Token::CurlyBracketBlock
      | Token::ParenthesisBlock
      | Token::SquareBracketBlock
      | Token::Function(_) => {
        parser.parse_nested_block(consume_nested_tokens)?;
      }
      _ => {}
    }
  }
  Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ImportLayerModifier {
  Anonymous,
  Named(Vec<String>),
}

fn serialize_layer_name(path: &[String]) -> String {
  let mut out = String::new();
  for (idx, name) in path.iter().enumerate() {
    if idx > 0 {
      out.push('.');
    }
    // Writing to a `String` cannot fail, so any formatting error is unreachable here.
    let _ = serialize_identifier(name, &mut out);
  }
  out
}

fn parse_import_modifiers_and_media(
  prelude: &str,
) -> Option<(Option<ImportLayerModifier>, Option<String>, &str)> {
  fn parse_import_layer_name<'i, 't>(
    parser: &mut Parser<'i, 't>,
  ) -> std::result::Result<Vec<String>, cssparser::ParseError<'i, ()>> {
    let mut components = Vec::new();
    loop {
      parser.skip_whitespace();
      match parser.next_including_whitespace() {
        Ok(Token::Ident(id)) => components.push(id.to_string()),
        _ => return Err(parser.new_custom_error::<(), ()>(())),
      }

      parser.skip_whitespace();
      let state = parser.state();
      if let Ok(Token::Delim('.')) = parser.next_including_whitespace() {
        continue;
      }
      parser.reset(&state);
      break;
    }

    parser.skip_whitespace();
    if components.is_empty() || !parser.is_exhausted() {
      return Err(parser.new_custom_error::<(), ()>(()));
    }
    Ok(components)
  }

  fn parse_import_layer_modifier<'i, 't>(
    parser: &mut Parser<'i, 't>,
  ) -> std::result::Result<ImportLayerModifier, cssparser::ParseError<'i, ()>> {
    if parser
      .try_parse(|p| p.expect_ident_matching("layer"))
      .is_ok()
    {
      return Ok(ImportLayerModifier::Anonymous);
    }

    parser.expect_function_matching("layer")?;
    parser.parse_nested_block(|nested| {
      let name = parse_import_layer_name(nested)?;
      Ok::<_, cssparser::ParseError<'i, ()>>(ImportLayerModifier::Named(name))
    })
  }

  fn parse_import_supports_modifier<'i, 't>(
    parser: &mut Parser<'i, 't>,
  ) -> std::result::Result<String, cssparser::ParseError<'i, ()>> {
    parser.expect_function_matching("supports")?;
    parser.parse_nested_block(|nested| {
      let start = nested.position();
      consume_nested_tokens(nested)?;
      Ok::<_, cssparser::ParseError<'i, ()>>(
        trim_ascii_whitespace(nested.slice_from(start)).to_string(),
      )
    })
  }

  let mut input = ParserInput::new(trim_ascii_whitespace(prelude));
  let mut parser = Parser::new(&mut input);

  let mut layer: Option<ImportLayerModifier> = None;
  let mut supports: Option<String> = None;
  let mut media_start = parser.position();

  while !parser.is_exhausted() {
    parser.skip_whitespace();
    media_start = parser.position();

    if let Ok(parsed_layer) = parser.try_parse(parse_import_layer_modifier) {
      if layer.is_some() {
        return None;
      }
      layer = Some(parsed_layer);
      media_start = parser.position();
      continue;
    }

    if let Ok(condition) = parser.try_parse(parse_import_supports_modifier) {
      if supports.is_some() {
        return None;
      }
      supports = Some(condition);
      media_start = parser.position();
      continue;
    }

    break;
  }

  // `cssparser::Parser::slice_from` returns the slice between `media_start` and the current parser
  // position. Advance to the end of the prelude so the slice captures any remaining media query
  // tokens (which are not otherwise consumed by the modifier parser).
  consume_nested_tokens::<()>(&mut parser).ok()?;
  let media_tokens = trim_ascii_whitespace(parser.slice_from(media_start));
  Some((layer, supports, media_tokens))
}

fn parse_import_target(rule: &str) -> Option<(&str, &str)> {
  let bytes = rule.as_bytes();
  if bytes.len() < 7 {
    return None;
  }
  if !bytes[..7].eq_ignore_ascii_case(b"@import") {
    return None;
  }
  if bytes.len() > 7 && css_ident_continues(bytes[7]) {
    return None;
  }
  let after_at = trim_ascii_whitespace_start(&rule[7..]);
  let bytes = after_at.as_bytes();
  let (target, rest) = if bytes.len() >= 4
    && matches!(bytes[0], b'u' | b'U')
    && matches!(bytes[1], b'r' | b'R')
    && matches!(bytes[2], b'l' | b'L')
    && bytes[3] == b'('
  {
    let inner = &after_at[4..];
    let close = inner.find(')')?;
    let url_part = trim_ascii_whitespace(&inner[..close]);
    let url_str = url_part.trim_matches(|c| c == '"' || c == '\'');
    let mut media = trim_ascii_whitespace(&inner[close + 1..]);
    media = media.trim_end_matches(';');
    media = trim_ascii_whitespace_end(media);
    (url_str, media)
  } else if let Some(quote) = after_at.chars().next().filter(|c| *c == '"' || *c == '\'') {
    let rest = &after_at[1..];
    let close_idx = rest.find(quote)?;
    let url_str = &rest[..close_idx];
    let mut media = trim_ascii_whitespace(&rest[close_idx + 1..]);
    media = media.trim_end_matches(';');
    media = trim_ascii_whitespace_end(media);
    (url_str, media)
  } else {
    return None;
  };
  Some((target, rest))
}

const DEFAULT_MAX_INLINE_STYLESHEETS: usize = 128;
const DEFAULT_MAX_INLINE_CSS_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_INLINE_IMPORT_DEPTH: usize = 8;
const DEFAULT_MAX_EMBEDDED_CSS_CANDIDATES: usize = 16;

/// Shared budget for stylesheet inlining across `<link>` tags and `@import` chains.
///
/// Tracks both resource count and total inlined bytes to bound work on pages with
/// extremely deep or large import graphs.
#[derive(Clone, Debug)]
pub struct StylesheetInlineBudget {
  max_stylesheets: usize,
  max_bytes: usize,
  max_import_depth: usize,
  used_stylesheets: usize,
  used_bytes: usize,
}

impl StylesheetInlineBudget {
  /// Construct a budget with explicit limits.
  pub fn new(max_stylesheets: usize, max_bytes: usize, max_import_depth: usize) -> Self {
    Self {
      max_stylesheets: max_stylesheets.max(1),
      max_bytes: max_bytes.max(1),
      max_import_depth: max_import_depth.max(1),
      used_stylesheets: 0,
      used_bytes: 0,
    }
  }

  /// Construct a budget using default limits.
  pub fn default_limits() -> Self {
    Self::new(
      DEFAULT_MAX_INLINE_STYLESHEETS,
      DEFAULT_MAX_INLINE_CSS_BYTES,
      DEFAULT_MAX_INLINE_IMPORT_DEPTH,
    )
  }

  /// Construct a budget using runtime overrides when present.
  ///
  /// The following `FASTR_*` toggles are respected:
  /// - `FASTR_INLINE_MAX_STYLESHEETS`
  /// - `FASTR_INLINE_MAX_INLINE_CSS_BYTES`
  /// - `FASTR_INLINE_MAX_INLINE_IMPORT_DEPTH`
  pub fn from_runtime_toggles() -> Self {
    let toggles = runtime::runtime_toggles();
    Self::new(
      toggles.usize_with_default(
        "FASTR_INLINE_MAX_STYLESHEETS",
        DEFAULT_MAX_INLINE_STYLESHEETS,
      ),
      toggles.usize_with_default(
        "FASTR_INLINE_MAX_INLINE_CSS_BYTES",
        DEFAULT_MAX_INLINE_CSS_BYTES,
      ),
      toggles.usize_with_default(
        "FASTR_INLINE_MAX_INLINE_IMPORT_DEPTH",
        DEFAULT_MAX_INLINE_IMPORT_DEPTH,
      ),
    )
  }

  pub fn remaining_bytes(&self) -> usize {
    self.max_bytes.saturating_sub(self.used_bytes)
  }

  pub fn try_spend_stylesheet<D>(&mut self, url: &str, diagnostics: &mut D) -> bool
  where
    D: FnMut(&str, &str),
  {
    if self.used_stylesheets >= self.max_stylesheets {
      diagnostics(
        url,
        &format!(
          "stylesheet budget exhausted (max {} stylesheets)",
          self.max_stylesheets
        ),
      );
      return false;
    }
    self.used_stylesheets += 1;
    true
  }

  pub fn try_spend_bytes<D>(&mut self, url: &str, bytes: usize, diagnostics: &mut D) -> bool
  where
    D: FnMut(&str, &str),
  {
    if bytes == 0 {
      return true;
    }
    let Some(total) = self.used_bytes.checked_add(bytes) else {
      diagnostics(url, "stylesheet byte budget exhausted");
      return false;
    };
    if total > self.max_bytes {
      diagnostics(
        url,
        &format!(
          "stylesheet byte budget exhausted (max {} bytes)",
          self.max_bytes
        ),
      );
      return false;
    }
    self.used_bytes = total;
    true
  }

  pub fn import_depth_allowed<D>(
    &self,
    current_depth: usize,
    url: &str,
    diagnostics: &mut D,
  ) -> bool
  where
    D: FnMut(&str, &str),
  {
    // current_depth counts the number of stylesheets on the stack. The next import would
    // increase it by one.
    if current_depth >= self.max_import_depth {
      diagnostics(
        url,
        &format!("import depth limit reached (max {})", self.max_import_depth),
      );
      return false;
    }
    true
  }
}

impl Default for StylesheetInlineBudget {
  fn default() -> Self {
    Self::from_runtime_toggles()
  }
}

/// Tracks recursion state and cached inlined content for `@import` processing.
#[derive(Debug)]
pub struct InlineImportState {
  stack: Vec<String>,
  seen: FxHashSet<String>,
  cache: FxHashMap<String, String>,
  redirects: FxHashMap<String, String>,
  budget: StylesheetInlineBudget,
}

impl InlineImportState {
  pub fn new() -> Self {
    Self::with_budget(StylesheetInlineBudget::default())
  }

  pub fn with_budget(budget: StylesheetInlineBudget) -> Self {
    Self {
      stack: Vec::new(),
      seen: FxHashSet::default(),
      cache: FxHashMap::default(),
      redirects: FxHashMap::default(),
      budget,
    }
  }

  /// Record that a stylesheet request resolved to a different final URL (e.g. after redirects).
  ///
  /// This allows later `@import` processing to reuse cached content and detect cycles when the
  /// same stylesheet is referenced via its original and redirected URL.
  pub fn record_redirect(&mut self, requested_url: &str, final_url: &str) {
    if requested_url == final_url {
      return;
    }
    self
      .redirects
      .insert(requested_url.to_string(), final_url.to_string());
  }

  fn canonicalize_url(&self, url: &str) -> String {
    let mut current = url;
    // Redirect chains for stylesheets are typically tiny, but guard against cycles in case a
    // caller records inconsistent mappings.
    for _ in 0..8 {
      let Some(next) = self.redirects.get(current) else {
        break;
      };
      if next == current {
        break;
      }
      current = next;
    }
    current.to_string()
  }

  pub fn register_stylesheet(&mut self, url: impl Into<String>) {
    let url = url.into();
    let mut discard = |_url: &str, _reason: &str| {};
    let _ = self.try_register_stylesheet_with_budget(&url, &mut discard);
  }

  pub fn register_stylesheet_alias(&mut self, url: &str) {
    if self.seen.contains(url) {
      return;
    }
    self.seen.insert(url.to_string());
  }

  pub fn try_register_stylesheet_with_budget<D>(&mut self, url: &str, diagnostics: &mut D) -> bool
  where
    D: FnMut(&str, &str),
  {
    if self.seen.contains(url) {
      return true;
    }
    if self.budget.try_spend_stylesheet(url, diagnostics) {
      self.seen.insert(url.to_string());
      true
    } else {
      false
    }
  }

  pub fn budget(&self) -> &StylesheetInlineBudget {
    &self.budget
  }

  pub fn budget_mut(&mut self) -> &mut StylesheetInlineBudget {
    &mut self.budget
  }
}

impl Default for InlineImportState {
  fn default() -> Self {
    Self::new()
  }
}

#[derive(Debug, Clone, Copy)]
pub struct ImportFetchContext<'a> {
  pub url: &'a str,
  pub importer_url: &'a str,
}

pub fn inline_imports_with_request<F>(
  css: &str,
  base_url: &str,
  fetch: &mut F,
  state: &mut InlineImportState,
  deadline: Option<&RenderDeadline>,
) -> std::result::Result<String, RenderError>
where
  F: FnMut(ImportFetchContext<'_>) -> Result<FetchedStylesheet>,
{
  inline_imports_with_request_with_diagnostics(
    css,
    base_url,
    fetch,
    state,
    &mut |_url, _reason| {},
    deadline,
  )
}

pub fn inline_imports_with_request_with_diagnostics<F, D>(
  css: &str,
  base_url: &str,
  fetch: &mut F,
  state: &mut InlineImportState,
  diagnostics: &mut D,
  deadline: Option<&RenderDeadline>,
) -> std::result::Result<String, RenderError>
where
  F: FnMut(ImportFetchContext<'_>) -> Result<FetchedStylesheet>,
  D: FnMut(&str, &str),
{
  let canonical_base = state.canonicalize_url(base_url);
  if !state.try_register_stylesheet_with_budget(&canonical_base, diagnostics) {
    return Ok(String::new());
  }
  if state.budget.remaining_bytes() == 0 {
    diagnostics(&canonical_base, "stylesheet byte budget exhausted");
    return Ok(String::new());
  }
  state.stack.push(canonical_base.clone());
  let result = inline_imports_inner(css, &canonical_base, fetch, state, diagnostics, deadline);
  state.stack.pop();
  result
}

/// Inline `@import` rules by fetching their targets recursively.
///
/// All fetched stylesheets have their `url(...)` references rewritten against the
/// stylesheet URL before inlining, so relative asset references continue to work
/// once the CSS is embedded in the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedStylesheet {
  pub css: String,
  pub final_url: Option<String>,
}

impl FetchedStylesheet {
  pub fn new(css: String, final_url: Option<String>) -> Self {
    Self { css, final_url }
  }
}

pub fn inline_imports<F>(
  css: &str,
  base_url: &str,
  fetch: &mut F,
  state: &mut InlineImportState,
  deadline: Option<&RenderDeadline>,
) -> std::result::Result<String, RenderError>
where
  F: FnMut(&str, &str) -> Result<FetchedStylesheet>,
{
  let mut adapter = |ctx: ImportFetchContext<'_>| fetch(ctx.url, ctx.importer_url);
  inline_imports_with_request(css, base_url, &mut adapter, state, deadline)
}

/// Inline `@import` rules with diagnostics about cycles and cutoffs.
///
/// This variant mirrors [`inline_imports`] but surfaces skipped imports to the caller.
pub fn inline_imports_with_diagnostics<F, D>(
  css: &str,
  base_url: &str,
  fetch: &mut F,
  state: &mut InlineImportState,
  diagnostics: &mut D,
  deadline: Option<&RenderDeadline>,
) -> std::result::Result<String, RenderError>
where
  F: FnMut(&str, &str) -> Result<FetchedStylesheet>,
  D: FnMut(&str, &str),
{
  let mut adapter = |ctx: ImportFetchContext<'_>| fetch(ctx.url, ctx.importer_url);
  inline_imports_with_request_with_diagnostics(
    css,
    base_url,
    &mut adapter,
    state,
    diagnostics,
    deadline,
  )
}

fn inline_imports_inner<F, D>(
  css: &str,
  base_url: &str,
  fetch: &mut F,
  state: &mut InlineImportState,
  diagnostics: &mut D,
  deadline: Option<&RenderDeadline>,
) -> std::result::Result<String, RenderError>
where
  F: FnMut(ImportFetchContext<'_>) -> Result<FetchedStylesheet>,
  D: FnMut(&str, &str),
{
  fn push_with_budget<D>(
    out: &mut String,
    text: &str,
    url: &str,
    budget: &mut StylesheetInlineBudget,
    diagnostics: &mut D,
  ) -> bool
  where
    D: FnMut(&str, &str),
  {
    if text.is_empty() {
      return true;
    }
    if budget.try_spend_bytes(url, text.len(), diagnostics) {
      out.push_str(text);
      true
    } else {
      false
    }
  }

  #[derive(PartialEq)]
  enum State {
    Normal,
    Single,
    Double,
    Comment,
  }

  let mut out = String::with_capacity(css.len());
  let mut parser_state = State::Normal;
  let bytes = css.as_bytes();
  let mut i = 0usize;
  let mut last_emit = 0usize;
  let mut active_deadline_counter = 0usize;
  let mut explicit_deadline_counter = 0usize;
  let mut budget_exhausted = state.budget.remaining_bytes() == 0;
  let mut brace_depth = 0usize;
  let mut imports_allowed = true;

  check_active(RenderStage::Css)?;
  if let Some(limit) = deadline {
    limit.check(RenderStage::Css)?;
  }

  while i < bytes.len() {
    check_active_periodic(&mut active_deadline_counter, 512, RenderStage::Css)?;
    if let Some(limit) = deadline {
      limit.check_periodic(&mut explicit_deadline_counter, 512, RenderStage::Css)?;
    }
    match parser_state {
      State::Normal => {
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
          parser_state = State::Comment;
          i += 2;
          continue;
        }
        if bytes[i] == b'\'' {
          parser_state = State::Single;
          i += 1;
          continue;
        }
        if bytes[i] == b'"' {
          parser_state = State::Double;
          i += 1;
          continue;
        }

        if bytes[i] == b'{' {
          brace_depth = brace_depth.saturating_add(1);
          i += 1;
          continue;
        }
        if bytes[i] == b'}' {
          brace_depth = brace_depth.saturating_sub(1);
          i += 1;
          continue;
        }

        if imports_allowed && brace_depth == 0 && matches_at_keyword(bytes, i, b"layer") {
          let mut j = i;
          let mut inner_state = State::Normal;
          while j < bytes.len() {
            match inner_state {
              State::Normal => {
                if bytes[j] == b';' {
                  j += 1;
                  break;
                }
                if bytes[j] == b'{' {
                  imports_allowed = false;
                  break;
                }
                if bytes[j] == b'\'' {
                  inner_state = State::Single;
                } else if bytes[j] == b'"' {
                  inner_state = State::Double;
                } else if bytes[j] == b'/' && j + 1 < bytes.len() && bytes[j + 1] == b'*' {
                  inner_state = State::Comment;
                  j += 1;
                }
              }
              State::Single => {
                if bytes[j] == b'\\' {
                  j += 1;
                } else if bytes[j] == b'\'' {
                  inner_state = State::Normal;
                }
              }
              State::Double => {
                if bytes[j] == b'\\' {
                  j += 1;
                } else if bytes[j] == b'"' {
                  inner_state = State::Normal;
                }
              }
              State::Comment => {
                if bytes[j] == b'*' && j + 1 < bytes.len() && bytes[j + 1] == b'/' {
                  inner_state = State::Normal;
                  j += 1;
                }
              }
            }
            j += 1;
          }

          // Blockless @layer statements are allowed before @import rules. Skip ahead to the end of
          // the statement so later non-@ bytes (layer names) don't terminate the import prelude.
          if imports_allowed && j > i {
            i = j;
            continue;
          }
        }

        if imports_allowed && brace_depth == 0 && matches_at_keyword(bytes, i, b"charset") {
          let mut j = i;
          let mut inner_state = State::Normal;
          while j < bytes.len() {
            match inner_state {
              State::Normal => {
                if bytes[j] == b';' {
                  j += 1;
                  break;
                }
                if bytes[j] == b'\'' {
                  inner_state = State::Single;
                } else if bytes[j] == b'"' {
                  inner_state = State::Double;
                } else if bytes[j] == b'/' && j + 1 < bytes.len() && bytes[j + 1] == b'*' {
                  inner_state = State::Comment;
                  j += 1;
                }
              }
              State::Single => {
                if bytes[j] == b'\\' {
                  j += 1;
                } else if bytes[j] == b'\'' {
                  inner_state = State::Normal;
                }
              }
              State::Double => {
                if bytes[j] == b'\\' {
                  j += 1;
                } else if bytes[j] == b'"' {
                  inner_state = State::Normal;
                }
              }
              State::Comment => {
                if bytes[j] == b'*' && j + 1 < bytes.len() && bytes[j + 1] == b'/' {
                  inner_state = State::Normal;
                  j += 1;
                }
              }
            }
            j += 1;
          }

          // @charset is permitted before @import rules; skip it without terminating the prelude.
          if j > i {
            i = j;
            continue;
          }
        }

        if imports_allowed && brace_depth == 0 && bytes[i] == b'@' {
          let is_import = matches_at_keyword(bytes, i, b"import");
          let is_layer = matches_at_keyword(bytes, i, b"layer");
          let is_charset = matches_at_keyword(bytes, i, b"charset");
          if !is_import && !is_layer && !is_charset {
            imports_allowed = false;
          }
        } else if imports_allowed
          && brace_depth == 0
          && bytes[i] != b'@'
          && !bytes[i].is_ascii_whitespace()
          && bytes[i] != b';'
        {
          imports_allowed = false;
        }

        if matches_at_keyword(bytes, i, b"import") {
          if budget_exhausted {
            diagnostics(base_url, "stylesheet byte budget exhausted");
            break;
          }
          let mut j = i;
          let mut inner_state = State::Normal;
          while j < bytes.len() {
            match inner_state {
              State::Normal => {
                if bytes[j] == b';' {
                  j += 1;
                  break;
                }
                if bytes[j] == b'\'' {
                  inner_state = State::Single;
                } else if bytes[j] == b'"' {
                  inner_state = State::Double;
                } else if bytes[j] == b'/' && j + 1 < bytes.len() && bytes[j + 1] == b'*' {
                  inner_state = State::Comment;
                  j += 1;
                }
              }
              State::Single => {
                if bytes[j] == b'\\' {
                  j += 1;
                } else if bytes[j] == b'\'' {
                  inner_state = State::Normal;
                }
              }
              State::Double => {
                if bytes[j] == b'\\' {
                  j += 1;
                } else if bytes[j] == b'"' {
                  inner_state = State::Normal;
                }
              }
              State::Comment => {
                if bytes[j] == b'*' && j + 1 < bytes.len() && bytes[j + 1] == b'/' {
                  inner_state = State::Normal;
                  j += 1;
                }
              }
            }
            j += 1;
          }

          let rule = trim_ascii_whitespace(&css[i..j]);
          if brace_depth != 0 || !imports_allowed {
            if !push_with_budget(
              &mut out,
              &css[last_emit..i],
              base_url,
              &mut state.budget,
              diagnostics,
            ) {
              budget_exhausted = true;
              break;
            }
            last_emit = j;
            i = j;
            continue;
          }

          if let Some((target, rest)) = parse_import_target(rule) {
            let Some((layer, supports, media)) = parse_import_modifiers_and_media(rest) else {
              if !push_with_budget(
                &mut out,
                &css[last_emit..i],
                base_url,
                &mut state.budget,
                diagnostics,
              ) {
                budget_exhausted = true;
                break;
              }
              last_emit = j;
              i = j;
              continue;
            };
            if let Some(resolved) = resolve_href(base_url, target) {
              let canonical_resolved = state.canonicalize_url(&resolved);
              let mut budget_url = canonical_resolved.clone();
              if !push_with_budget(
                &mut out,
                &css[last_emit..i],
                base_url,
                &mut state.budget,
                diagnostics,
              ) {
                budget_exhausted = true;
                break;
              }
              if !state.budget.import_depth_allowed(
                state.stack.len(),
                &canonical_resolved,
                diagnostics,
              ) {
                if let Some(layer) = layer {
                  if let ImportLayerModifier::Named(path) = layer {
                    let mut wrapped: std::borrow::Cow<'_, str> =
                      std::borrow::Cow::Owned(format!("@layer {};\n", serialize_layer_name(&path)));
                    if let Some(condition) = supports {
                      wrapped = std::borrow::Cow::Owned(format!(
                        "@supports {} {{\n{}\n}}\n",
                        condition, wrapped
                      ));
                    }
                    if media.is_empty() || media.eq_ignore_ascii_case("all") {
                      if !push_with_budget(
                        &mut out,
                        wrapped.as_ref(),
                        &budget_url,
                        &mut state.budget,
                        diagnostics,
                      ) {
                        budget_exhausted = true;
                      }
                    } else {
                      let to_insert = format!("@media {} {{\n{}\n}}\n", media, wrapped);
                      if !push_with_budget(
                        &mut out,
                        &to_insert,
                        &budget_url,
                        &mut state.budget,
                        diagnostics,
                      ) {
                        budget_exhausted = true;
                      }
                    }
                  }
                }
                last_emit = j;
                i = j;
                continue;
              }
              if state.stack.contains(&canonical_resolved) {
                diagnostics(&canonical_resolved, "skipping cyclic @import");
              }

              let mut inlined_key: Option<String> = None;
              if !state.stack.contains(&canonical_resolved) {
                if state.budget.remaining_bytes() == 0 {
                  diagnostics(&canonical_resolved, "stylesheet byte budget exhausted");
                  budget_exhausted = true;
                } else if state.cache.contains_key(&canonical_resolved) {
                  inlined_key = Some(canonical_resolved.clone());
                } else {
                  match fetch(ImportFetchContext {
                    url: &resolved,
                    importer_url: base_url,
                  }) {
                    Ok(fetched) => {
                      let final_url = fetched.final_url.unwrap_or_else(|| resolved.clone());
                      state.record_redirect(&resolved, &final_url);
                      let canonical_final = state.canonicalize_url(&final_url);
                      budget_url = canonical_final.clone();

                      if state.stack.contains(&canonical_final) {
                        diagnostics(&canonical_final, "skipping cyclic @import");
                      } else if state.cache.contains_key(&canonical_final) {
                        inlined_key = Some(canonical_final.clone());
                      } else if !state
                        .try_register_stylesheet_with_budget(&canonical_final, diagnostics)
                      {
                        // Count exhausted; skip this import.
                      } else if state.budget.remaining_bytes() == 0 {
                        diagnostics(&canonical_final, "stylesheet byte budget exhausted");
                        budget_exhausted = true;
                      } else {
                        let rewritten = absolutize_css_urls_cow(&fetched.css, &canonical_final)?;
                        let rewritten_str = rewritten.as_ref();
                        if rewritten_str.len() > state.budget.remaining_bytes() {
                          diagnostics(&canonical_final, "stylesheet byte budget exhausted");
                        } else {
                          let nested = inline_imports_with_request_with_diagnostics(
                            rewritten_str,
                            &canonical_final,
                            fetch,
                            state,
                            diagnostics,
                            deadline,
                          )?;
                          state.cache.entry(canonical_final.clone()).or_insert(nested);
                          inlined_key = Some(canonical_final.clone());
                        }
                      }
                    }
                    Err(Error::Render(err)) => return Err(err),
                    Err(_) => {
                      // Per spec, failed imports are ignored. Render-control errors (deadline,
                      // cancellation) are handled above, but other fetch failures (network, parse,
                      // etc.) should behave like missing imports.
                    }
                  }
                }
              }

              // Failed imports are ignored, but `@import ... layer(foo)` still establishes the
              // layer ordering so subsequent `@layer foo { ... }` blocks don't change precedence
              // based on network outcomes.
              if inlined_key.is_some()
                || matches!(layer.as_ref(), Some(ImportLayerModifier::Named(_)))
              {
                let inlined = inlined_key
                  .as_deref()
                  .and_then(|key| state.cache.get(key))
                  .map(|css| css.as_str());
                let has_inlined = inlined.is_some();
                let mut wrapped: std::borrow::Cow<'_, str> =
                  std::borrow::Cow::Borrowed(inlined.unwrap_or_default());

                if let Some(layer) = layer {
                  match layer {
                    ImportLayerModifier::Anonymous => {
                      if has_inlined {
                        wrapped = std::borrow::Cow::Owned(format!("@layer {{\n{}\n}}\n", wrapped));
                      }
                    }
                    ImportLayerModifier::Named(path) => {
                      wrapped = if has_inlined {
                        std::borrow::Cow::Owned(format!(
                          "@layer {} {{\n{}\n}}\n",
                          serialize_layer_name(&path),
                          wrapped
                        ))
                      } else {
                        std::borrow::Cow::Owned(format!(
                          "@layer {};\n",
                          serialize_layer_name(&path)
                        ))
                      };
                    }
                  }
                }

                if let Some(condition) = supports {
                  wrapped = std::borrow::Cow::Owned(format!(
                    "@supports {} {{\n{}\n}}\n",
                    condition, wrapped
                  ));
                }
                if media.is_empty() || media.eq_ignore_ascii_case("all") {
                  if !push_with_budget(
                    &mut out,
                    wrapped.as_ref(),
                    &budget_url,
                    &mut state.budget,
                    diagnostics,
                  ) {
                    budget_exhausted = true;
                  }
                } else {
                  let to_insert = format!("@media {} {{\n{}\n}}\n", media, wrapped);
                  if !push_with_budget(
                    &mut out,
                    &to_insert,
                    &budget_url,
                    &mut state.budget,
                    diagnostics,
                  ) {
                    budget_exhausted = true;
                  }
                }
              }
              last_emit = j;
              i = j;
              continue;
            }
          }
        }

        i += 1;
      }
      State::Single => {
        if bytes[i] == b'\\' {
          i += 2;
          continue;
        }
        if bytes[i] == b'\'' {
          parser_state = State::Normal;
        }
        i += 1;
      }
      State::Double => {
        if bytes[i] == b'\\' {
          i += 2;
          continue;
        }
        if bytes[i] == b'"' {
          parser_state = State::Normal;
        }
        i += 1;
      }
      State::Comment => {
        if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
          parser_state = State::Normal;
          i += 2;
        } else {
          i += 1;
        }
      }
    }
  }

  if !budget_exhausted {
    let _ = push_with_budget(
      &mut out,
      &css[last_emit..],
      base_url,
      &mut state.budget,
      diagnostics,
    );
  }
  Ok(out)
}

fn extract_attr_value(tag_source: &str, attr: &str) -> Option<String> {
  let target = attr.to_ascii_lowercase();
  let mut chars = tag_source.chars().peekable();

  // Skip the tag name itself.
  if let Some('<') = chars.peek().copied() {
    chars.next();
  }
  while let Some(&c) = chars.peek() {
    if is_ascii_whitespace_html_css(c) || c == '>' {
      break;
    }
    chars.next();
  }

  loop {
    // Skip whitespace between attributes.
    while let Some(&c) = chars.peek() {
      if is_ascii_whitespace_html_css(c) {
        chars.next();
      } else {
        break;
      }
    }

    match chars.peek().copied() {
      None | Some('>') => break,
      Some('/') => {
        chars.next();
        continue;
      }
      _ => {}
    }

    let mut name = String::new();
    while let Some(&c) = chars.peek() {
      if is_ascii_whitespace_html_css(c) || c == '=' || c == '>' {
        break;
      }
      name.push(c);
      chars.next();
    }

    if name.is_empty() {
      if chars.next().is_none() {
        break;
      }
      continue;
    }

    let name_lower = name.to_ascii_lowercase();

    while let Some(&c) = chars.peek() {
      if is_ascii_whitespace_html_css(c) {
        chars.next();
      } else {
        break;
      }
    }

    let value = if let Some('=') = chars.peek().copied() {
      chars.next();

      while let Some(&c) = chars.peek() {
        if is_ascii_whitespace_html_css(c) {
          chars.next();
        } else {
          break;
        }
      }

      if let Some(next) = chars.peek().copied() {
        if next == '"' || next == '\'' {
          let quote = next;
          chars.next();
          let mut val = String::new();
          while let Some(ch) = chars.next() {
            if ch == quote {
              break;
            }
            val.push(ch);
          }
          val
        } else {
          let mut val = String::new();
          while let Some(&ch) = chars.peek() {
            if is_ascii_whitespace_html_css(ch) || ch == '>' {
              break;
            }
            val.push(ch);
            chars.next();
          }
          val
        }
      } else {
        String::new()
      }
    } else {
      // Boolean attribute: treat as present with an empty value.
      String::new()
    };

    if name_lower == target {
      return Some(decode_html_entities(&value));
    }
  }

  None
}

fn parse_tag_attributes(tag_source: &str) -> HashMap<String, String> {
  let mut attrs = HashMap::new();
  let bytes = tag_source.as_bytes();
  let mut i = tag_source.find('<').map(|idx| idx + 1).unwrap_or(0);

  while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
    i += 1;
  }

  while i < bytes.len() {
    while i < bytes.len() {
      let b = bytes[i];
      if b.is_ascii_whitespace() || b == b'/' {
        i += 1;
      } else {
        break;
      }
    }

    if i >= bytes.len() || bytes[i] == b'>' {
      break;
    }

    let name_start = i;
    while i < bytes.len() {
      let b = bytes[i];
      if b.is_ascii_whitespace() || b == b'=' || b == b'>' {
        break;
      }
      i += 1;
    }

    if name_start == i {
      i += 1;
      continue;
    }

    let name = tag_source[name_start..i].to_ascii_lowercase();

    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }

    let mut value = String::new();
    if i < bytes.len() && bytes[i] == b'=' {
      i += 1;
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }

      if i >= bytes.len() {
        attrs.insert(name, value);
        break;
      }

      let quote = bytes[i];
      if quote == b'"' || quote == b'\'' {
        i += 1;
        let val_start = i;
        while i < bytes.len() && bytes[i] != quote {
          i += 1;
        }
        value.push_str(&tag_source[val_start..i]);
        if i < bytes.len() {
          i += 1;
        }
      } else {
        let val_start = i;
        while i < bytes.len() {
          let b = bytes[i];
          if b.is_ascii_whitespace() || b == b'>' {
            break;
          }
          i += 1;
        }
        value.push_str(&tag_source[val_start..i]);
      }
    }

    attrs.insert(name, decode_html_entities(trim_ascii_whitespace(&value)));
  }

  attrs
}

fn decode_html_entities(input: &str) -> String {
  let mut out = String::with_capacity(input.len());
  let mut chars = input.chars().peekable();
  while let Some(c) = chars.next() {
    if c != '&' {
      out.push(c);
      continue;
    }

    let mut entity = String::new();
    while let Some(&next) = chars.peek() {
      entity.push(next);
      chars.next();
      if next == ';' {
        break;
      }
    }

    if entity.is_empty() {
      out.push('&');
      continue;
    }

    let mut ent = entity.as_str();
    if let Some(stripped) = ent.strip_prefix('/') {
      ent = stripped;
    }

    let decoded = match ent {
      "amp;" => Some('&'),
      "quot;" => Some('"'),
      "apos;" => Some('\''),
      "lt;" => Some('<'),
      "gt;" => Some('>'),
      _ => {
        if let Some(num) = ent.strip_prefix('#') {
          let trimmed = num.trim_end_matches(';');
          if let Some(hex) = trimmed.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
          } else {
            trimmed.parse::<u32>().ok().and_then(char::from_u32)
          }
        } else {
          None
        }
      }
    };

    if let Some(ch) = decoded {
      out.push(ch);
    } else {
      out.push('&');
      out.push_str(&entity);
    }
  }
  normalize_scheme_slashes(&out)
}

fn normalize_scheme_slashes(s: &str) -> String {
  if s.starts_with("//") {
    // Preserve scheme-relative URLs as-is so they can be resolved against the base
    // scheme rather than collapsing the host into a path segment.
    return s.to_string();
  }

  let mut path_end = s.len();
  if let Some(pos) = s.find('?') {
    path_end = path_end.min(pos);
  }
  if let Some(pos) = s.find('#') {
    path_end = path_end.min(pos);
  }

  let (path, suffix) = s.split_at(path_end);

  let normalized = if let Some(pos) = path.find("://") {
    let (scheme, rest) = path.split_at(pos + 3);
    let mut trimmed = rest.trim_start_matches('/').to_string();
    while trimmed.contains("//") {
      trimmed = trimmed.replace("//", "/");
    }
    format!("{}{}", scheme, trimmed)
  } else {
    let mut out = path.to_string();
    while out.contains("//") {
      out = out.replace("//", "/");
    }
    out
  };

  format!("{}{}", normalized, suffix)
}

/// Extract stylesheet candidate `<link>` URLs from an HTML document.
///
/// This includes normal `<link rel=stylesheet>` entries, plus the common
/// preload-as-style pattern (`<link rel=preload as=style>`) when enabled via
/// `FASTR_FETCH_PRELOAD_STYLESHEETS` (and `modulepreload` behind
/// `FASTR_FETCH_MODULEPRELOAD_STYLESHEETS`).
pub fn extract_css_links(
  html: &str,
  base_url: &str,
  media_type: crate::style::media::MediaType,
) -> std::result::Result<Vec<String>, RenderError> {
  Ok(dedupe_links_preserving_order(
    extract_css_links_with_meta(html, base_url, media_type)?
      .into_iter()
      .map(|candidate| candidate.url)
      .collect(),
  ))
}

pub fn extract_css_links_with_meta(
  html: &str,
  base_url: &str,
  media_type: crate::style::media::MediaType,
) -> std::result::Result<Vec<CssLinkCandidate>, RenderError> {
  extract_css_links_with_meta_for_scripting_enabled(
    html,
    base_url,
    media_type,
    DomParseOptions::default().scripting_enabled,
  )
}

pub(crate) fn extract_css_links_with_meta_for_scripting_enabled(
  html: &str,
  base_url: &str,
  media_type: crate::style::media::MediaType,
  scripting_enabled: bool,
) -> std::result::Result<Vec<CssLinkCandidate>, RenderError> {
  let Ok(dom) = crate::dom::parse_html_with_options(
    html,
    DomParseOptions::with_scripting_enabled(scripting_enabled),
  ) else {
    return extract_css_links_without_dom_with_meta(html, base_url, media_type);
  };

  extract_css_links_with_meta_from_dom(&dom, base_url, media_type)
}

fn extract_css_links_with_meta_from_dom(
  dom: &DomNode,
  base_url: &str,
  media_type: crate::style::media::MediaType,
) -> std::result::Result<Vec<CssLinkCandidate>, RenderError> {
  let mut css_links = Vec::new();
  let toggles = runtime::runtime_toggles();
  let debug = toggles.truthy("FASTR_LOG_CSS_LINKS");
  let preload_stylesheets_enabled =
    toggles.truthy_with_default("FASTR_FETCH_PRELOAD_STYLESHEETS", true);
  let modulepreload_stylesheets_enabled =
    toggles.truthy_with_default("FASTR_FETCH_MODULEPRELOAD_STYLESHEETS", false);
  let alternate_stylesheets_enabled =
    toggles.truthy_with_default("FASTR_FETCH_ALTERNATE_STYLESHEETS", true);
  let scoped_sources = extract_scoped_css_sources(&dom);

  fn trim_ascii_whitespace(value: &str) -> &str {
    value
      .trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
  }

  let mut consider_source = |source: &StylesheetSource| {
    let StylesheetSource::External(link) = source else {
      return;
    };
    if link.disabled {
      return;
    }
    let href_trimmed = trim_ascii_whitespace(&link.href);
    if href_trimmed.is_empty() {
      return;
    }

    if !link_rel_is_stylesheet_candidate(
      &link.rel,
      link.as_attr.as_deref(),
      preload_stylesheets_enabled,
      modulepreload_stylesheets_enabled,
      alternate_stylesheets_enabled,
    ) {
      return;
    }

    let allowed = link
      .media
      .as_deref()
      .map(|media| media_attr_allows_target(media, media_type))
      .unwrap_or(true);
    if !allowed {
      return;
    }

    // DOM attribute parsing decodes valid HTML entities, but we still run our permissive entity
    // decoder to catch common broken patterns observed in the wild (e.g. `&/#47;` for `/`).
    let href = decode_html_entities(href_trimmed);
    if debug {
      eprintln!(
        "[css] found <link>: href={href} rel={:?} media={:?} crossorigin={:?}",
        link.rel, link.media, link.crossorigin
      );
    }
    if let Some(full_url) = resolve_href(base_url, &href) {
      css_links.push(CssLinkCandidate {
        url: full_url,
        crossorigin: link.crossorigin,
        referrer_policy: link.referrer_policy,
      });
    }
  };

  for source in scoped_sources.document.iter() {
    consider_source(source);
  }

  let mut shadow_hosts: Vec<usize> = scoped_sources.shadows.keys().copied().collect();
  shadow_hosts.sort_unstable();
  for host in shadow_hosts {
    if let Some(sources) = scoped_sources.shadows.get(&host) {
      for source in sources {
        consider_source(source);
      }
    }
  }

  Ok(dedupe_link_candidates_preserving_order(css_links))
}

fn extract_css_links_without_dom_with_meta(
  html: &str,
  base_url: &str,
  media_type: crate::style::media::MediaType,
) -> std::result::Result<Vec<CssLinkCandidate>, RenderError> {
  let html = crate::html::strip_template_contents(html);
  let html = html.as_ref();
  let mut css_urls = Vec::new();
  let toggles = runtime::runtime_toggles();
  let debug = toggles.truthy("FASTR_LOG_CSS_LINKS");
  let preload_stylesheets_enabled =
    toggles.truthy_with_default("FASTR_FETCH_PRELOAD_STYLESHEETS", true);
  let modulepreload_stylesheets_enabled =
    toggles.truthy_with_default("FASTR_FETCH_MODULEPRELOAD_STYLESHEETS", false);
  let alternate_stylesheets_enabled =
    toggles.truthy_with_default("FASTR_FETCH_ALTERNATE_STYLESHEETS", true);

  let bytes = html.as_bytes();
  let mut pos = 0usize;
  let mut deadline_counter = 0usize;

  while pos < bytes.len() {
    check_active_periodic(&mut deadline_counter, 1024, RenderStage::Css)?;
    let Some(rel_start) = memchr::memchr(b'<', &bytes[pos..]) else {
      break;
    };
    let abs_start = pos + rel_start;
    pos = abs_start.saturating_add(1);
    if abs_start + 5 > bytes.len()
      || !bytes[abs_start..abs_start + 5].eq_ignore_ascii_case(b"<link")
    {
      continue;
    }
    if let Some(link_end) = bytes[abs_start..].iter().position(|&b| b == b'>') {
      let link_tag = &html[abs_start..=abs_start + link_end];
      let attrs = parse_tag_attributes(link_tag);
      let rel_tokens = attrs
        .get("rel")
        .map(|rel| tokenize_rel_list(rel))
        .unwrap_or_default();

      let mut is_stylesheet_link = link_rel_is_stylesheet_candidate(
        &rel_tokens,
        attrs.get("as").map(String::as_str),
        preload_stylesheets_enabled,
        modulepreload_stylesheets_enabled,
        alternate_stylesheets_enabled,
      );

      let link_tag_lower = link_tag.to_ascii_lowercase();

      if !is_stylesheet_link && rel_tokens.is_empty() && link_tag_lower.contains("stylesheet") {
        is_stylesheet_link = true;
      }

      if is_stylesheet_link {
        if debug {
          eprintln!("[css] found <link>: {}", link_tag);
        }
        let mut allowed = true;

        if let Some(media) = attrs.get("media") {
          allowed = media_attr_allows_target(media, media_type);
          if debug {
            eprintln!(
              "[css] media attr: {} (target={:?}, allow={})",
              media, media_type, allowed
            );
          }
        } else if link_tag_lower.contains("media") {
          let has_screen = link_tag_lower.contains("screen") || link_tag_lower.contains("all");
          let has_print = link_tag_lower.contains("print");
          let has_speech = link_tag_lower.contains("speech");
          allowed = media_flags_allow_target(has_screen, has_print, has_speech, media_type);
          if debug {
            eprintln!(
              "[css] media substring in tag (no attr parsed), print={}, screen={}, speech={}, allow={}",
              has_print, has_screen, has_speech, allowed
            );
          }
        }
        if let Some(href) = attrs.get("href") {
          let href = normalize_scheme_slashes(href);
          if let Some(full_url) = resolve_href(base_url, &href) {
            if allowed {
              let crossorigin = match attrs.get("crossorigin") {
                None => None,
                Some(value) => {
                  let value = trim_ascii_whitespace(value);
                  if value.eq_ignore_ascii_case("use-credentials") {
                    Some(CorsMode::UseCredentials)
                  } else {
                    Some(CorsMode::Anonymous)
                  }
                }
              };
              let referrer_policy = attrs
                .get("referrerpolicy")
                .and_then(|value| ReferrerPolicy::from_attribute(value));
              css_urls.push(CssLinkCandidate {
                url: full_url,
                crossorigin,
                referrer_policy,
              });
            }
          }
        }
      }

      pos = abs_start + link_end + 1;
    } else {
      break;
    }
  }

  Ok(dedupe_link_candidates_preserving_order(css_urls))
}

fn media_attr_allows_target(value: &str, target: crate::style::media::MediaType) -> bool {
  let value = trim_ascii_whitespace(value);
  if value.is_empty() {
    return true;
  }

  let mut allowed = false;
  for token in value.split(',') {
    let query = trim_ascii_whitespace(token);
    if query.is_empty() {
      allowed = true;
      continue;
    }

    let lowered = query.to_ascii_lowercase();
    let mut tokens = lowered
      .split(is_ascii_whitespace_html_css)
      .filter(|token| !token.is_empty());
    let Some(first) = tokens.next() else {
      continue;
    };

    let mut negated = false;
    let media_token = match first {
      "only" => tokens.next(),
      "not" => {
        negated = true;
        tokens.next()
      }
      other => Some(other),
    };

    let Some(media_type) = media_token else {
      // Malformed media query list entry; keep behavior permissive so we don't drop stylesheets.
      allowed = true;
      continue;
    };

    if media_type.starts_with('(') {
      // Media type omitted (e.g. `(min-width: 600px)`); this defaults to `all`.
      allowed = true;
      continue;
    }

    let matches_type = match media_type {
      "all" => true,
      "screen" => matches!(
        target,
        crate::style::media::MediaType::Screen | crate::style::media::MediaType::All
      ),
      "print" => matches!(
        target,
        crate::style::media::MediaType::Print | crate::style::media::MediaType::All
      ),
      "speech" => matches!(
        target,
        crate::style::media::MediaType::Speech | crate::style::media::MediaType::All
      ),
      _ => false,
    };

    if negated {
      // We cannot evaluate feature expressions like `not screen and (min-width: ...)` without a
      // full MediaContext; treat those as allowed so offline tooling doesn't drop resources.
      if lowered.contains('(') || lowered.contains(" and ") {
        allowed = true;
      } else {
        allowed |= !matches_type;
      }
    } else {
      allowed |= matches_type;
    }

    if !negated && media_type == "all" {
      return true;
    }
  }

  allowed
}

fn media_flags_allow_target(
  has_screen: bool,
  has_print: bool,
  has_speech: bool,
  target: crate::style::media::MediaType,
) -> bool {
  match target {
    crate::style::media::MediaType::Screen => {
      has_screen || (!has_print && !has_speech && !has_screen)
    }
    crate::style::media::MediaType::Print => {
      has_print || (!has_screen && !has_speech && !has_print)
    }
    crate::style::media::MediaType::Speech => {
      has_speech || (!has_screen && !has_print && !has_speech)
    }
    crate::style::media::MediaType::All => true,
  }
}

/// Heuristic extraction of CSS URLs that appear inside inline scripts or attributes.
///
/// Some sites load their primary stylesheets dynamically and never emit a
/// `<link rel="stylesheet">` element in the static HTML. To render those pages
/// without executing JavaScript, scan the raw HTML for any substring that looks
/// like a CSS URL (ends with `.css`, possibly with a query string) and try to
/// resolve and fetch it as a stylesheet.
#[derive(Debug, Clone)]
pub(crate) struct EmbeddedCssUrlDiscovery {
  pub urls: Vec<String>,
  pub truncated: bool,
  pub max_candidates: usize,
}

fn embedded_css_max_candidates() -> usize {
  let toggles = runtime::runtime_toggles();
  let max = toggles.usize_with_default(
    "FASTR_EMBEDDED_CSS_MAX_CANDIDATES",
    DEFAULT_MAX_EMBEDDED_CSS_CANDIDATES,
  );
  max.min(DEFAULT_MAX_INLINE_STYLESHEETS)
}

fn is_url_function_before_paren(bytes: &[u8], paren_pos: usize) -> bool {
  let mut end = paren_pos;
  while end > 0 && bytes[end - 1].is_ascii_whitespace() {
    end -= 1;
  }
  if end < 3 {
    return false;
  }
  let start = end - 3;
  if !bytes[start..end].eq_ignore_ascii_case(b"url") {
    return false;
  }
  if start == 0 {
    return true;
  }
  let before = bytes[start - 1];
  !(before.is_ascii_alphanumeric() || before == b'-' || before == b'_')
}

fn embedded_candidate_has_valid_context(bytes: &[u8], start: usize) -> bool {
  let mut i = start;
  while i > 0 && bytes[i - 1].is_ascii_whitespace() {
    i -= 1;
  }
  if i == 0 {
    return false;
  }
  match bytes[i - 1] {
    b'"' | b'\'' | b'`' => true,
    b'(' => is_url_function_before_paren(bytes, i - 1),
    _ => false,
  }
}

pub(crate) fn extract_embedded_css_urls_with_meta(
  html: &str,
  base_url: &str,
  max_candidates_hint: Option<usize>,
) -> std::result::Result<EmbeddedCssUrlDiscovery, RenderError> {
  let html = crate::html::strip_template_contents(html);
  let html = html.as_ref();
  let runtime_cap = embedded_css_max_candidates();
  let max_candidates = match max_candidates_hint {
    Some(hint) => runtime_cap.min(hint),
    None => runtime_cap,
  };

  if max_candidates == 0 {
    return Ok(EmbeddedCssUrlDiscovery {
      urls: Vec::new(),
      truncated: false,
      max_candidates,
    });
  }

  let mut urls = Vec::new();
  let mut seen: FxHashSet<String> = FxHashSet::default();
  let bytes = html.as_bytes();
  let mut idx = 0;
  let mut deadline_counter = 0usize;
  let mut truncated = false;

  fn record_url(
    resolved: String,
    seen: &mut FxHashSet<String>,
    urls: &mut Vec<String>,
    truncated: &mut bool,
    max_candidates: usize,
  ) -> bool {
    if seen.insert(resolved.clone()) {
      urls.push(resolved);
      if urls.len() >= max_candidates {
        *truncated = true;
        return true;
      }
    }
    false
  }

  'css_scan: while let Some(pos) = memchr::memmem::find(&bytes[idx..], b".css") {
    check_active_periodic(&mut deadline_counter, 256, RenderStage::Css)?;
    let abs_pos = idx + pos;

    let mut start = abs_pos;
    while start > 0 {
      let c = bytes[start - 1] as char;
      if matches!(c, '"' | '\'' | '`' | '(' | '<') || is_ascii_whitespace_html_css(c) {
        break;
      }
      start -= 1;
    }

    // Require the token to be inside quotes or a `url(...)` context. This avoids treating
    // random `.css` substrings in inline scripts as fetchable stylesheet URLs.
    if !embedded_candidate_has_valid_context(bytes, start) {
      idx = abs_pos + 4;
      continue;
    }

    let mut end = abs_pos + 4;
    while end < bytes.len() {
      let c = bytes[end] as char;
      if matches!(c, '"' | '\'' | '`' | ')' | '>' | '{' | '}') || is_ascii_whitespace_html_css(c) {
        break;
      }
      end += 1;
    }

    // If this candidate appears inside a <link> tag that is print-only, skip it.
    if abs_pos > 0 {
      let tag_start = bytes[..abs_pos].iter().rposition(|&b| b == b'<');
      let tag_end = bytes[abs_pos..].iter().position(|&b| b == b'>');
      if let (Some(ts), Some(te_rel)) = (tag_start, tag_end) {
        let te = abs_pos + te_rel;
        if te > ts {
          let tag = &html[ts..=te];
          let tag_lower = tag.to_ascii_lowercase();
          if tag_lower.contains("<link") && tag_lower.contains("media") {
            let has_screen = tag_lower.contains("screen") || tag_lower.contains("all");
            let has_print = tag_lower.contains("print");
            if has_print && !has_screen {
              idx = end;
              continue;
            }
          }
        }
      }
    }

    // Skip identifiers like `window.css = ...` where the token is an assignment target
    // rather than a URL. If the next non-whitespace character after the match is '=',
    // treat it as a property access and ignore it.
    let mut lookahead = end;
    while lookahead < bytes.len() && is_ascii_whitespace_html_css_byte(bytes[lookahead]) {
      lookahead += 1;
    }
    if lookahead < bytes.len() && bytes[lookahead] == b'=' {
      idx = end;
      continue;
    }

    if end > start {
      let candidate = &html[start..end];
      if candidate.len() < 512 {
        let raw_lower = candidate.to_ascii_lowercase();

        // Detect sourceURL/sourceMappingURL-style sourcemap markers (/*# or //#) immediately
        // preceding the token.
        let mut marker_back = start;
        while marker_back > 0 && is_ascii_whitespace_html_css_byte(bytes[marker_back - 1]) {
          marker_back -= 1;
        }
        let sourcemap_marker = if marker_back > 0 && bytes[marker_back - 1] == b'#' {
          (marker_back >= 3 && bytes[marker_back - 2] == b'*' && bytes[marker_back - 3] == b'/')
            || (marker_back >= 2 && bytes[marker_back - 2] == b'/')
        } else {
          false
        };

        if sourcemap_marker
          && (raw_lower.contains("sourceurl=") || raw_lower.contains("sourcemappingurl="))
        {
          idx = end;
          continue;
        }

        if let Some(cleaned) = normalize_embedded_css_candidate(candidate) {
          if cleaned.contains('{') || cleaned.contains('}') {
            idx = end;
            continue;
          }
          if let Some(first) = cleaned.chars().next() {
            if !(first.is_ascii_alphanumeric() || matches!(first, '/' | '.' | '#')) {
              idx = end;
              continue;
            }
          }

          let cleaned_lower = cleaned.to_ascii_lowercase();
          if cleaned_lower.contains("sourceurl=") || cleaned_lower.contains("sourcemappingurl=") {
            idx = end;
            continue;
          }
          if cleaned_lower.contains(".css.map") {
            idx = end;
            continue;
          }
          let css_pos = cleaned_lower.find(".css");
          if let Some(pos) = css_pos {
            let after = cleaned_lower.as_bytes().get(pos + 4).copied();
            if let Some(ch) = after {
              let ch = ch as char;
              if ch != '?' && ch != '#' && ch != '/' && ch != '%' && ch != '"' && ch != '\'' {
                idx = end;
                continue;
              }
            }
          } else {
            idx = end;
            continue;
          }
          if !cleaned_lower.contains("style.csstext")
            && !trim_ascii_whitespace_end(&cleaned).ends_with(':')
          {
            if let Some(resolved) = resolve_href(base_url, &cleaned) {
              if record_url(
                resolved,
                &mut seen,
                &mut urls,
                &mut truncated,
                max_candidates,
              ) {
                break 'css_scan;
              }
            }
          }
        }
      }
    }

    idx = end;
  }

  if !truncated {
    let mut pos = 0usize;
    while pos < bytes.len() {
      let Some(rel) = memchr::memchr2(b'c', b'C', &bytes[pos..]) else {
        break;
      };
      let abs = pos + rel;
      pos = abs.saturating_add(1);
      if abs + 6 > bytes.len() || !bytes[abs..abs + 6].eq_ignore_ascii_case(b"cssurl") {
        continue;
      }
      check_active_periodic(&mut deadline_counter, 256, RenderStage::Css)?;
      let slice = &html[abs..];
      if let Some(colon) = slice.find(':') {
        let after_colon = &slice[colon + 1..];
        if let Some(q_start_rel) = after_colon.find(['"', '\'']) {
          let quote = match after_colon.as_bytes().get(q_start_rel) {
            Some(b'"') => '"',
            Some(b'\'') => '\'',
            _ => {
              pos = abs + 6;
              continue;
            }
          };
          let after_quote = &after_colon[q_start_rel + 1..];
          if let Some(q_end_rel) = after_quote.find(quote) {
            let candidate = &after_quote[..q_end_rel];
            if !candidate.to_ascii_lowercase().contains("style.csstext")
              && !trim_ascii_whitespace_end(candidate).ends_with(':')
            {
              if let Some(cleaned) = normalize_embedded_css_candidate(candidate) {
                let lower = cleaned.to_ascii_lowercase();
                if lower.contains("sourcemappingurl=") || lower.contains(".css.map") {
                  pos = abs + 6;
                  continue;
                }
                if !lower.contains("style.csstext")
                  && !trim_ascii_whitespace_end(&cleaned).ends_with(':')
                {
                  if let Some(resolved) = resolve_href(base_url, &cleaned) {
                    if record_url(
                      resolved,
                      &mut seen,
                      &mut urls,
                      &mut truncated,
                      max_candidates,
                    ) {
                      break;
                    }
                  }
                }
              }
            }
          }
        }
      }
    }
  }

  Ok(EmbeddedCssUrlDiscovery {
    urls,
    truncated,
    max_candidates,
  })
}

#[allow(clippy::cognitive_complexity)]
pub fn extract_embedded_css_urls(
  html: &str,
  base_url: &str,
) -> std::result::Result<Vec<String>, RenderError> {
  Ok(extract_embedded_css_urls_with_meta(html, base_url, None)?.urls)
}

pub(crate) fn html_has_inline_style_tag(html: &str) -> bool {
  crate::html::find_tag_case_insensitive_outside_templates(html, "style", false).is_some()
}

pub(crate) fn should_scan_embedded_css_urls(
  html: &str,
  has_link_stylesheets: bool,
  remaining_limit: usize,
) -> bool {
  !has_link_stylesheets && remaining_limit > 0 && !html_has_inline_style_tag(html)
}

/// Deduplicate a list while preserving the order of first occurrence.
pub fn dedupe_links_preserving_order(mut links: Vec<String>) -> Vec<String> {
  let mut seen: FxHashSet<String> = FxHashSet::default();
  seen.reserve(links.len());
  links.retain(|link| seen.insert(link.clone()));
  links
}

fn dedupe_link_candidates_preserving_order(
  mut links: Vec<CssLinkCandidate>,
) -> Vec<CssLinkCandidate> {
  let mut seen: FxHashSet<(String, Option<CorsMode>)> = FxHashSet::default();
  seen.reserve(links.len());
  links.retain(|link| seen.insert((link.url.clone(), link.crossorigin)));
  links
}

/// Inject a `<style>` block containing `css` into the HTML document.
pub fn inject_css_into_html(html: &str, css: &str) -> String {
  let style_tag = format!("<style>{css}</style>");

  if let Some(head_end) =
    crate::html::find_tag_case_insensitive_outside_templates(html, "head", true)
  {
    let mut result = String::with_capacity(html.len() + style_tag.len());
    result.push_str(&html[..head_end]);
    result.push_str(&style_tag);
    result.push_str(&html[head_end..]);
    result
  } else if let Some(body_start) =
    crate::html::find_tag_case_insensitive_outside_templates(html, "body", false)
  {
    let mut result = String::with_capacity(html.len() + style_tag.len());
    result.push_str(&html[..body_start]);
    result.push_str(&style_tag);
    result.push_str(&html[body_start..]);
    result
  } else {
    format!("{style_tag}{html}")
  }
}

/// Infer a reasonable base URL for the document.
///
/// Infer a reasonable document URL for the current HTML input.
///
/// This is primarily intended for pageset-style cached HTML where the "true" document URL is not
/// known at render time (the input is often a `file://...` URL). In those cases, we try to guess
/// the original URL using:
/// - `<link rel="canonical" href="...">`
/// - `<meta property="og:url" content="...">`
/// - as a last resort: `https://{filename}/` when the cached filename looks like a domain.
///
/// For arbitrary local HTML files (including imported offline fixtures that use `index.html` as the
/// entrypoint) we intentionally keep the `file://...` URL as-is so canonical/OG metadata cannot
/// override offline resource resolution.
///
/// Note: This function intentionally does **not** consider `<base href>`; base URL resolution is
/// handled after DOM parsing so `<base>` inside inert contexts (e.g. `<template>`, declarative
/// shadow DOM templates) cannot poison the result.
pub fn infer_document_url_guess<'a>(html: &'a str, input_url: &'a str) -> Cow<'a, str> {
  // Canonicalize file:// inputs so relative cached paths become absolute.
  let input = canonicalize_file_input_url(input_url);
  if !is_file_url(input.as_ref()) {
    return input;
  }
  if !file_url_looks_like_cached_pageset_html(input.as_ref()) {
    return input;
  }

  // For cached file inputs, prefer an explicit canonical URL in the metadata so subsequent URL
  // resolution (including `<base href>`) uses the original site instead of the local filesystem.
  let Ok(dom) = crate::dom::parse_html(html) else {
    return infer_document_url_guess_without_dom(input);
  };

  infer_document_url_guess_from_dom_with_input(&dom, input)
}

fn canonicalize_file_input_url<'a>(input_url: &'a str) -> Cow<'a, str> {
  fn normalize_path_components(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
      match component {
        Component::CurDir => {}
        Component::ParentDir => {
          normalized.pop();
        }
        Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
        Component::Normal(part) => normalized.push(part),
      }
    }
    normalized
  }

  fn canonicalize_path_lexical(path: PathBuf) -> Option<String> {
    let abs = if path.is_absolute() {
      path
    } else {
      std::env::current_dir().ok()?.join(path)
    };
    let normalized = normalize_path_components(&abs);
    Url::from_file_path(normalized)
      .ok()
      .map(|url| url.to_string())
  }

  // Prefer the WHATWG URL parser for standard file:// URLs (including `file://localhost/...`).
  if let Ok(url) = Url::parse(input_url) {
    if url.scheme() == "file" {
      if let Ok(path) = url.to_file_path() {
        if let Some(url) = canonicalize_path_lexical(path) {
          return Cow::Owned(url);
        }
      }
    }
  }

  // Legacy `file://relative/path` form: treat the remainder as a local path, not an authority.
  if input_url.starts_with("file://") && !input_url.starts_with("file:///") {
    let rel = &input_url["file://".len()..];
    if let Some(url) = canonicalize_path_lexical(PathBuf::from(rel)) {
      return Cow::Owned(url);
    }
  }

  Cow::Borrowed(input_url)
}

fn is_file_url(url: &str) -> bool {
  Url::parse(url)
    .ok()
    .is_some_and(|url| url.scheme() == "file")
}

fn file_url_looks_like_cached_pageset_html(doc_url: &str) -> bool {
  let Ok(url) = Url::parse(doc_url) else {
    return false;
  };
  if url.scheme() != "file" {
    return false;
  }
  let Some(seg) = url.path_segments().and_then(|mut s| s.next_back()) else {
    return false;
  };
  let lower = seg.to_ascii_lowercase();
  let stem = if lower.ends_with(".html") {
    &seg[..seg.len() - ".html".len()]
  } else if lower.ends_with(".htm") {
    &seg[..seg.len() - ".htm".len()]
  } else {
    return false;
  };

  // Pageset cached HTML snapshots are named using the normalized host/path stem (e.g.
  // `macrumors.com.html`). Imported fixtures use `index.html`, and treating those as cached pages
  // would cause `<link rel="canonical">` / `og:url` metadata to override the file base URL and
  // break offline resource loading.
  stem.contains('.')
}

fn infer_http_base_from_file_url(doc_url: &str) -> Option<String> {
  let Ok(url) = Url::parse(doc_url) else {
    return None;
  };
  if url.scheme() != "file" {
    return None;
  }
  let Some(seg) = url.path_segments().and_then(|mut s| s.next_back()) else {
    return None;
  };
  let Some(host) = seg.strip_suffix(".html") else {
    return None;
  };
  // Heuristic: cached pages typically use a `{host}.html` filename. Avoid applying this
  // to local files like `page.html` by requiring the host portion to look like a domain.
  if !host.contains('.') {
    return None;
  }
  let guess = format!("https://{host}/");
  Url::parse(&guess).ok().map(|url| url.to_string())
}

fn infer_document_url_guess_without_dom<'a>(input: Cow<'a, str>) -> Cow<'a, str> {
  if is_file_url(input.as_ref()) {
    if let Some(guess) = infer_http_base_from_file_url(input.as_ref()) {
      return Cow::Owned(guess);
    }
  }
  input
}

/// Infer a reasonable document URL given a parsed DOM and an input URL.
///
/// For non-file inputs, this returns the input URL unchanged.
/// For `file://` cached pages, this attempts to recover the original http(s) URL using:
/// - `<link rel="canonical" href="...">`
/// - `<meta property="og:url" content="...">`
/// - a `{host}.html` filename heuristic.
///
/// Like `<base href>` handling, DOM traversal skips inert `<template>` contents and declarative
/// shadow roots so base hints cannot be extracted from script/template text.
pub fn infer_document_url_guess_from_dom<'a>(dom: &DomNode, input_url: &'a str) -> Cow<'a, str> {
  let input = canonicalize_file_input_url(input_url);
  if !is_file_url(input.as_ref()) {
    return input;
  }
  infer_document_url_guess_from_dom_with_input(dom, input)
}

fn infer_document_url_guess_from_dom_with_input<'a>(
  dom: &DomNode,
  input: Cow<'a, str>,
) -> Cow<'a, str> {
  if !is_file_url(input.as_ref()) {
    return input;
  }
  if !file_url_looks_like_cached_pageset_html(input.as_ref()) {
    return input;
  }

  fn find_head<'a>(node: &'a DomNode) -> Option<&'a DomNode> {
    let mut stack: Vec<&DomNode> = vec![node];
    while let Some(node) = stack.pop() {
      if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) {
        continue;
      }
      if node.is_template_element() {
        continue;
      }
      if node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("head"))
        && matches!(node.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE)
      {
        return Some(node);
      }
      for child in node.traversal_children().iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn node_is_html(node: &DomNode) -> bool {
    matches!(node.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE)
  }

  fn node_is_foreign_element(node: &DomNode) -> bool {
    node.is_element() && !node_is_html(node)
  }

  fn find_first_http_canonical(node: &DomNode, base_url: &str) -> Option<String> {
    let mut stack: Vec<(&DomNode, bool)> = vec![(node, false)];
    while let Some((node, prev_sibling_foreign)) = stack.pop() {
      if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) {
        continue;
      }
      if node.is_template_element() {
        continue;
      }

      let is_html = node_is_html(node);
      if node.is_element() && !is_html {
        continue;
      }

      // Some invalid SVG content (e.g. `<meta>` inside `<svg>`) can be relocated by the HTML parser
      // so it appears as an HTML sibling immediately following the foreign element. When scanning
      // for document-level hints, ignore matches that follow a foreign sibling so those relocated
      // tokens cannot poison the inferred URL.
      if !prev_sibling_foreign
        && node
          .tag_name()
          .is_some_and(|tag| tag.eq_ignore_ascii_case("link"))
        && is_html
      {
        if let Some(rel) = node.get_attribute_ref("rel") {
          if rel
            .split(is_ascii_whitespace_html_css)
            .filter(|token| !token.is_empty())
            .any(|token| token.eq_ignore_ascii_case("canonical"))
          {
            if let Some(href) = node.get_attribute_ref("href") {
              if let Some(resolved) = resolve_href(base_url, href) {
                if resolved.starts_with("http://") || resolved.starts_with("https://") {
                  return Some(resolved);
                }
              }
            }
          }
        }
      }

      let children = node.traversal_children();
      for idx in (0..children.len()).rev() {
        let child = &children[idx];
        let prev_foreign = idx > 0 && node_is_foreign_element(&children[idx - 1]);
        stack.push((child, prev_foreign));
      }
    }
    None
  }

  fn find_first_http_og_url(node: &DomNode, base_url: &str) -> Option<String> {
    let mut stack: Vec<(&DomNode, bool)> = vec![(node, false)];
    while let Some((node, prev_sibling_foreign)) = stack.pop() {
      if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) {
        continue;
      }
      if node.is_template_element() {
        continue;
      }

      let is_html = node_is_html(node);
      if node.is_element() && !is_html {
        continue;
      }

      if !prev_sibling_foreign
        && node
          .tag_name()
          .is_some_and(|tag| tag.eq_ignore_ascii_case("meta"))
        && is_html
      {
        if node
          .get_attribute_ref("property")
          .is_some_and(|prop| prop.eq_ignore_ascii_case("og:url"))
        {
          if let Some(content) = node.get_attribute_ref("content") {
            if let Some(resolved) = resolve_href(base_url, content) {
              if resolved.starts_with("http://") || resolved.starts_with("https://") {
                return Some(resolved);
              }
            }
          }
        }
      }

      let children = node.traversal_children();
      for idx in (0..children.len()).rev() {
        let child = &children[idx];
        let prev_foreign = idx > 0 && node_is_foreign_element(&children[idx - 1]);
        stack.push((child, prev_foreign));
      }
    }
    None
  }

  let head = find_head(dom);

  if let Some(head) = head {
    if let Some(canonical) = find_first_http_canonical(head, input.as_ref()) {
      return Cow::Owned(canonical);
    }
    if let Some(og_url) = find_first_http_og_url(head, input.as_ref()) {
      return Cow::Owned(og_url);
    }
  }

  // Some malformed inputs (e.g. non-metadata elements placed in `<head>`) may cause the HTML parser
  // to relocate subsequent `<link>`/`<meta>` tokens outside the head element. Fall back to scanning
  // the full document so cached pages still pick up canonical/OG URL hints.
  if let Some(canonical) = find_first_http_canonical(dom, input.as_ref()) {
    return Cow::Owned(canonical);
  }
  if let Some(og_url) = find_first_http_og_url(dom, input.as_ref()) {
    return Cow::Owned(og_url);
  }

  // Some cached pages use a relative canonical/og:url hint (e.g. `href="/path/page"`), which
  // would resolve to a `file://` URL when joined against the cached file path. When we can infer
  // the original host from the cache filename (e.g. `{host}.html`), retry resolution against that
  // inferred http(s) base so relative hints still influence `<base href>` resolution.
  if let Some(http_base) = infer_http_base_from_file_url(input.as_ref()) {
    if let Some(head) = head {
      if let Some(canonical) = find_first_http_canonical(head, &http_base) {
        return Cow::Owned(canonical);
      }
      if let Some(og_url) = find_first_http_og_url(head, &http_base) {
        return Cow::Owned(og_url);
      }
    }
    if let Some(canonical) = find_first_http_canonical(dom, &http_base) {
      return Cow::Owned(canonical);
    }
    if let Some(og_url) = find_first_http_og_url(dom, &http_base) {
      return Cow::Owned(og_url);
    }
  }

  infer_document_url_guess_without_dom(input)
}

/// Infer a reasonable base URL for the document.
///
/// This parses the HTML, infers a best-effort document URL (see [`infer_document_url_guess`]), and
/// resolves the first valid `<base href>` against that document URL. This avoids base URL
/// poisoning from string scanning (e.g., `<base>` inside `<template>` or declarative shadow DOM
/// templates).
pub fn infer_base_url<'a>(html: &'a str, input_url: &'a str) -> Cow<'a, str> {
  let input = canonicalize_file_input_url(input_url);
  // `infer_base_url` previously parsed the full document to safely locate `<base href>` and
  // canonical/OG URL hints without being vulnerable to poisoning from inert contexts. That is
  // correct but can be prohibitively expensive for very large HTML inputs (e.g. pageset fixtures
  // used by CLI tooling) where the only relevant metadata lives near the top of the document.
  //
  // To keep CLI soft timeouts effective (and avoid consuming the entire hard timeout budget while
  // *guessing* the base URL), only parse a prefix of the document that covers `<head>` and the
  // start of `<body>`.
  const MAX_BASE_URL_SCAN_BYTES: usize = 256 * 1024;
  const BASE_URL_SCAN_BODY_PREFIX_BYTES: usize = 32 * 1024;

  fn scan_html_prefix(html: &str) -> &str {
    if html.len() <= MAX_BASE_URL_SCAN_BYTES {
      return html;
    }

    let mut limit = MAX_BASE_URL_SCAN_BYTES.min(html.len());
    while limit > 0 && !html.is_char_boundary(limit) {
      limit -= 1;
    }

    let prefix = &html[..limit];
    let mut end = limit;

    if let Some(pos) =
      crate::html::find_tag_case_insensitive_outside_templates(prefix, "body", false)
    {
      end = (pos + BASE_URL_SCAN_BODY_PREFIX_BYTES).min(limit);
    } else if let Some(pos) =
      crate::html::find_tag_case_insensitive_outside_templates(prefix, "head", true)
    {
      end = pos.min(limit);
    }

    while end > 0 && !html.is_char_boundary(end) {
      end -= 1;
    }
    &html[..end]
  }

  let scanned_html = scan_html_prefix(html);
  let Ok(dom) = crate::dom::parse_html(scanned_html) else {
    return infer_document_url_guess_without_dom(input);
  };
  let document_url_guess = infer_document_url_guess_from_dom_with_input(&dom, input);
  if let Some(base_href) = crate::html::find_base_href(&dom) {
    if let Some(resolved) = resolve_href(document_url_guess.as_ref(), &base_href) {
      return Cow::Owned(resolved);
    }
  }
  document_url_guess
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::runtime::{self, RuntimeToggles};
  use crate::style::media::MediaType;
  use cssparser::ToCss;
  use selectors::context::QuirksMode;
  use std::borrow::Cow;
  use std::collections::HashMap;
  use std::sync::Arc;
  use tempfile;

  #[test]
  fn resolves_relative_http_links() {
    let base = "https://example.com/a/b/page.html";
    let href = "../styles/site.css";
    let resolved = resolve_href(base, href).expect("resolved");
    assert_eq!(resolved, "https://example.com/a/styles/site.css");
  }

  #[test]
  fn resolves_protocol_relative_links() {
    let base = "https://example.com/index.html";
    let href = "//cdn.example.com/main.css";
    let resolved = resolve_href(base, href).expect("resolved");
    assert_eq!(resolved, "https://cdn.example.com/main.css");
  }

  #[test]
  fn resolves_relative_href_with_pipe_character() {
    let resolved = resolve_href("https://example.com/dir/page.html", "a|b.png").expect("resolved");
    assert_eq!(resolved, "https://example.com/dir/a%7Cb.png");
  }

  #[test]
  fn resolves_absolute_href_with_pipe_character() {
    let resolved = resolve_href(
      "https://example.com/dir/page.html",
      "https://cdn.example.com/a|b.png",
    )
    .expect("resolved");
    assert_eq!(resolved, "https://cdn.example.com/a%7Cb.png");
  }

  #[test]
  fn resolves_href_preserving_nbsp() {
    let nbsp = "\u{00A0}";
    let href = format!("a{nbsp}b.png");
    let resolved = resolve_href("https://example.com/dir/page.html", &href).expect("resolved");
    assert_eq!(resolved, "https://example.com/dir/a%C2%A0b.png");
  }

  #[test]
  fn resolves_relative_links_with_pipe_by_percent_encoding() {
    let base = "https://example.com/a/";
    let href = "img|1.png";
    let resolved = resolve_href(base, href).expect("resolved");
    assert_eq!(resolved, "https://example.com/a/img%7C1.png");
  }

  #[test]
  fn resolves_relative_links_with_spaces_by_percent_encoding() {
    let base = "https://example.com/a/";
    let href = "img 1.png";
    let resolved = resolve_href(base, href).expect("resolved");
    assert_eq!(resolved, "https://example.com/a/img%201.png");
  }

  #[test]
  fn resolves_absolute_links_with_pipe_by_percent_encoding() {
    let base = "https://example.com/a/";
    let href = "https://static.example.com/img|1.png?x=1|2";
    let resolved = resolve_href(base, href).expect("resolved");
    assert_eq!(resolved, "https://static.example.com/img%7C1.png?x=1%7C2");
  }

  #[test]
  fn does_not_double_encode_existing_percent_escapes() {
    let base = "https://example.com/a/";
    let href = "img%7C1.png";
    let resolved = resolve_href(base, href).expect("resolved");
    assert_eq!(resolved, "https://example.com/a/img%7C1.png");
  }

  #[test]
  fn resolves_data_urls_case_insensitively() {
    let base = "https://example.com/index.html";
    let href = "DATA:text/plain;base64,aGk=";
    let resolved = resolve_href(base, href).expect("resolved");
    assert_eq!(resolved, href);
  }

  #[test]
  fn resolve_href_accepts_uppercase_data_urls() {
    let data_url = "DATA:text/plain,hello";
    let resolved = resolve_href("https://example.com/root/page.html", data_url)
      .expect("data: urls should resolve without needing a base");
    assert_eq!(resolved, data_url);
  }

  #[test]
  fn resolve_href_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let href = format!("app.js{nbsp}");
    let resolved = resolve_href("https://example.com/base/", &href).expect("href should resolve");
    assert_eq!(resolved, "https://example.com/base/app.js%C2%A0");
  }

  #[test]
  fn absolutizes_css_urls_cow_fast_path_skips_when_no_url_tokens() {
    reset_absolutize_css_urls_tokenize_count();
    let css = "body { color: red; }";
    let out = absolutize_css_urls_cow(css, "https://example.com/styles/main.css").unwrap();
    assert!(matches!(out, Cow::Borrowed(_)));
    assert_eq!(out.as_ref(), css);
    assert_eq!(
      absolutize_css_urls_tokenize_count(),
      0,
      "expected no tokenizer pass for CSS without url() tokens"
    );
  }

  #[test]
  fn absolutizes_css_urls_cow_fast_path_skips_when_no_url_tokens_need_rewriting() {
    reset_absolutize_css_urls_tokenize_count();
    let css = r#"body { background: url("data:text/plain;base64,abc"); mask: url(); }"#;
    let out = absolutize_css_urls_cow(css, "https://example.com/styles/main.css").unwrap();
    assert!(matches!(out, Cow::Borrowed(_)));
    assert_eq!(out.as_ref(), css);
    assert_eq!(
      absolutize_css_urls_tokenize_count(),
      0,
      "expected tokenizer to be skipped when all url() tokens are non-resolvable"
    );
  }

  #[test]
  fn absolutizes_css_urls_cow_fast_path_does_not_skip_image_set_string_candidates() {
    reset_absolutize_css_urls_tokenize_count();
    let css = r#"body { background-image: IMAGE-SET("bg.png" 1x); }"#;
    let out = absolutize_css_urls_cow(css, "https://example.com/styles/main.css").unwrap();
    assert_eq!(
      absolutize_css_urls_tokenize_count(),
      1,
      "expected tokenizer to run for image-set() string candidates"
    );

    // If image-set() string-candidate rewriting is implemented, ensure the relative URL is
    // absolutized. Otherwise, the output is expected to be unchanged.
    if out.as_ref() != css {
      assert!(
        out.as_ref()
          .contains("https://example.com/styles/bg.png"),
        "expected image-set() string candidate to be absolutized, got: {}",
        out.as_ref()
      );
    }
  }

  #[test]
  fn absolutizes_css_urls_cow_fast_path_does_not_skip_webkit_image_set_string_candidates() {
    reset_absolutize_css_urls_tokenize_count();
    let css = r#"body { background-image: -WEBKIT-IMAGE-SET("bg.png" 1x); }"#;
    let out = absolutize_css_urls_cow(css, "https://example.com/styles/main.css").unwrap();
    assert_eq!(
      absolutize_css_urls_tokenize_count(),
      1,
      "expected tokenizer to run for -webkit-image-set() string candidates"
    );

    // If string-candidate rewriting is implemented, ensure the relative URL is absolutized.
    if out.as_ref() != css {
      assert!(
        out.as_ref()
          .contains("https://example.com/styles/bg.png"),
        "expected -webkit-image-set() string candidate to be absolutized, got: {}",
        out.as_ref()
      );
    }
  }

  #[test]
  fn absolutizes_css_urls_cow_fast_path_skips_image_set_inside_comments_and_strings() {
    reset_absolutize_css_urls_tokenize_count();
    let css = r#"
      /* image-set("should-not-tokenize.png" 1x) */
      .icon::before { content: "image-set('in-string.png' 1x)"; }
      body { color: red; }
    "#;
    let out = absolutize_css_urls_cow(css, "https://example.com/styles/main.css").unwrap();
    assert!(matches!(out, Cow::Borrowed(_)));
    assert_eq!(out.as_ref(), css);
    assert_eq!(
      absolutize_css_urls_tokenize_count(),
      0,
      "expected image-set() occurrences inside comments/strings to not trigger tokenization"
    );
  }

  #[test]
  fn absolutizes_css_urls_cow_rewrites_relative_urls() {
    reset_absolutize_css_urls_tokenize_count();
    let css = "body { background: url(\"images/bg.png\"); }";
    let out = absolutize_css_urls_cow(css, "https://example.com/styles/main.css").unwrap();
    assert!(matches!(out, Cow::Owned(_)));
    assert_eq!(
      out.as_ref(),
      "body { background: url(\"https://example.com/styles/images/bg.png\"); }"
    );
    assert_eq!(
      absolutize_css_urls_tokenize_count(),
      1,
      "expected tokenizer to run when url() tokens need rewriting"
    );
  }

  #[test]
  fn absolutizes_css_urls_cow_rewrites_image_set_string_urls_without_url_tokens() {
    reset_absolutize_css_urls_tokenize_count();
    let css = "body { background-image: image-set(\"foo.png\" 1x); }";
    let out = absolutize_css_urls_cow(css, "https://example.com/styles/main.css").unwrap();
    assert!(matches!(out, Cow::Owned(_)));
    assert_eq!(
      out.as_ref(),
      "body { background-image: image-set(\"https://example.com/styles/foo.png\" 1x); }"
    );
    assert_eq!(
      absolutize_css_urls_tokenize_count(),
      1,
      "expected tokenizer to run for image-set() string URL candidates"
    );
  }

  #[test]
  fn absolutizes_css_urls_cow_rewrites_webkit_image_set_string_urls_without_url_tokens() {
    reset_absolutize_css_urls_tokenize_count();
    let css = "body { background-image: -webkit-image-set(\"foo.png\" 1x); }";
    let out = absolutize_css_urls_cow(css, "https://example.com/styles/main.css").unwrap();
    assert!(matches!(out, Cow::Owned(_)));
    assert_eq!(
      out.as_ref(),
      "body { background-image: -webkit-image-set(\"https://example.com/styles/foo.png\" 1x); }"
    );
    assert_eq!(
      absolutize_css_urls_tokenize_count(),
      1,
      "expected tokenizer to run for -webkit-image-set() string URL candidates"
    );
  }

  #[test]
  fn absolutizes_css_urls_cow_leaves_absolute_urls_unchanged() {
    let css = "body { background: URL(https://example.com/images/bg.png); }";
    let out = absolutize_css_urls_cow(css, "https://example.com/styles/main.css").unwrap();
    assert!(matches!(out, Cow::Borrowed(_)));
    assert_eq!(out.as_ref(), css);

    let css = "body { background: url(\"data:text/plain;base64,abc\"); }";
    let out = absolutize_css_urls_cow(css, "https://example.com/styles/main.css").unwrap();
    assert!(matches!(out, Cow::Borrowed(_)));
    assert_eq!(out.as_ref(), css);
  }

  #[test]
  fn absolutizes_css_urls_cow_leaves_inline_svg_markup_unchanged() {
    let css = r#"body { mask-image: url("<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1 1'></svg>"); }"#;
    let out = absolutize_css_urls_cow(css, "https://example.com/styles/main.css").unwrap();
    assert_eq!(out.as_ref(), css);
  }

  #[test]
  fn absolutizes_css_urls_cow_rewrites_unquoted_relative_urls() {
    let css = "body { background: url(images/bg.png); }";
    let out = absolutize_css_urls_cow(css, "https://example.com/styles/main.css").unwrap();
    assert!(matches!(out, Cow::Owned(_)));
    assert_eq!(
      out.as_ref(),
      "body { background: url(\"https://example.com/styles/images/bg.png\"); }"
    );
  }

  #[test]
  fn absolutizes_css_urls_cow_ignores_urls_inside_comments_and_strings() {
    let css = r#"
      /* url('should-not-change.png') */
      .icon::before { content: "url(in-string.png)"; }
      body { color: red; }
    "#;
    let out = absolutize_css_urls_cow(css, "https://example.com/styles/main.css").unwrap();
    assert!(matches!(out, Cow::Borrowed(_)));
    assert_eq!(out.as_ref(), css);
  }

  #[test]
  fn absolutizes_css_urls_cow_rewrites_urls_inside_nested_blocks() {
    let css = "@media screen { body { background: url(images/bg.png); } }";
    let out = absolutize_css_urls_cow(css, "https://example.com/styles/main.css").unwrap();
    assert!(matches!(out, Cow::Owned(_)));
    assert_eq!(
      out.as_ref(),
      "@media screen { body { background: url(\"https://example.com/styles/images/bg.png\"); } }"
    );
  }

  #[test]
  fn absolutizes_css_urls_cow_rewrites_image_set_string_candidates() {
    let css = r#"div{background-image:image-set("foo.png" 1x, url(bar.png) 2x)}"#;
    let out = absolutize_css_urls_cow(css, "https://example.com/css/main.css").unwrap();
    assert_eq!(
      out.as_ref(),
      r#"div{background-image:image-set("https://example.com/css/foo.png" 1x, url("https://example.com/css/bar.png") 2x)}"#
    );
  }

  #[test]
  fn absolutizes_css_urls_cow_rewrites_webkit_image_set_string_candidates() {
    let css = r#"div{background-image:-webkit-image-set("foo.png" 1x, url(bar.png) 2x)}"#;
    let out = absolutize_css_urls_cow(css, "https://example.com/css/main.css").unwrap();
    assert_eq!(
      out.as_ref(),
      r#"div{background-image:-webkit-image-set("https://example.com/css/foo.png" 1x, url("https://example.com/css/bar.png") 2x)}"#
    );
  }

  #[test]
  fn absolutizes_css_urls_cow_does_not_rewrite_non_url_strings_in_image_set() {
    let css = r#"div{background-image:image-set("foo.png" 1x type("image/avif"), url(bar.png) 2x type("image/avif"))}"#;
    let out = absolutize_css_urls_cow(css, "https://example.com/css/main.css").unwrap();
    assert_eq!(
      out.as_ref(),
      r#"div{background-image:image-set("https://example.com/css/foo.png" 1x type("image/avif"), url("https://example.com/css/bar.png") 2x type("image/avif"))}"#
    );
  }

  #[test]
  fn absolutize_ignores_comments_and_strings() {
    let css = r#"
      /* url('should-not-change.png') */
      .icon::before { content: "url(in-string.png)"; }
      body { background: url(images/bg.png); }
    "#;

    let out = absolutize_css_urls(css, "https://example.com/static/main.css").unwrap();
    assert!(out.contains("/* url('should-not-change.png') */"));
    assert!(out.contains("content: \"url(in-string.png)\";"));
    assert!(out.contains("url(\"https://example.com/static/images/bg.png\")"));
  }

  #[test]
  fn absolutize_rewrites_import_and_font_face_sources() {
    let css = r#"
      @import url('../reset.css');
      @font-face { src: url('../fonts/font.woff2') format('woff2'); }
    "#;

    let out = absolutize_css_urls(css, "https://example.com/assets/css/main.css").unwrap();
    assert!(out.contains("url(\"https://example.com/assets/reset.css\")"));
    assert!(out.contains("url(\"https://example.com/assets/fonts/font.woff2\")"));
  }

  #[test]
  fn absolutize_rewrites_nested_image_set_urls() {
    let css = "div { background-image: image-set(url(./a.png) 1x, url('../b.png') 2x); }";
    let out = absolutize_css_urls(css, "https://example.com/styles/site.css").unwrap();
    assert!(out.contains("url(\"https://example.com/styles/a.png\")"));
    assert!(out.contains("url(\"https://example.com/b.png\")"));
  }

  #[test]
  fn absolutize_handles_data_urls_with_parentheses() {
    let data_url = "data:image/svg+xml,<svg>text)</svg>";
    let css = format!("div {{ background: url(\"{data_url}\"); }}");
    let out = absolutize_css_urls(&css, "https://example.com/app/site.css").unwrap();
    assert!(out.contains(&format!("url(\"{data_url}\")")));
  }

  #[test]
  fn absolutize_handles_escaped_paren_in_url_function() {
    let css = r#"div { mask: url("icons/close\).svg"); }"#;
    let out = absolutize_css_urls(css, "https://example.com/theme/style.css").unwrap();
    assert!(out.contains("url(\"https://example.com/theme/icons/close).svg\")"));
  }

  #[test]
  fn absolutize_handles_uppercase_url_function_and_whitespace() {
    let css = "div { background-image: URL(   './img/bg.png'  ); }";
    let out = absolutize_css_urls(css, "https://example.com/css/main.css").unwrap();
    assert!(out.contains("url(\"https://example.com/css/img/bg.png\")"));
  }

  #[test]
  fn absolutize_rewrites_inside_parenthesized_blocks() {
    let css =
      "@supports (background: url('../check.png')) { body { background: url(./inner.png); } }";
    let out = absolutize_css_urls(css, "https://example.com/styles/app.css").unwrap();
    assert!(out.contains("url(\"https://example.com/check.png\")"));
    assert!(out.contains("url(\"https://example.com/styles/inner.png\")"));
  }

  #[test]
  fn absolutize_escapes_quotes_and_backslashes() {
    let css = r#"div { background: url("images/sp\"ace\\ path.png"); }"#;
    let out = absolutize_css_urls(css, "https://example.com/css/main.css").unwrap();
    // The resolved URL percent-encodes the quotes/backslashes/spaces, but the rewriter
    // must still emit a valid quoted url() string.
    assert!(out.contains("url(\"https://example.com/css/images/sp%22ace%5C%20path.png\")"));
  }

  #[test]
  fn absolutize_rewrites_protocol_relative_urls() {
    let css = "body { background-image: url(//cdn.example.com/bg.png); }";
    let out = absolutize_css_urls(css, "https://example.com/a.css").unwrap();
    assert!(out.contains("url(\"https://cdn.example.com/bg.png\")"));
  }

  #[test]
  fn absolutize_leaves_urls_inside_comments_untouched() {
    let css = "/* gradient url(foo.png) */ div { background: linear-gradient(red, blue); }";
    let out = absolutize_css_urls(css, "https://example.com/base.css").unwrap();
    assert!(out.contains("/* gradient url(foo.png) */"));
  }

  #[test]
  fn absolutize_does_not_trim_non_ascii_whitespace_in_url_tokens() {
    let nbsp = "\u{00A0}";
    let css = format!("div {{ background: url(\"  foo{nbsp}  \"); }}");
    let out = absolutize_css_urls(&css, "https://example.com/base/main.css").unwrap();
    assert!(
      out.contains("url(\"https://example.com/base/foo%C2%A0\")"),
      "expected NBSP to be preserved and percent-encoded: {out}"
    );
  }

  #[test]
  fn inline_imports_parses_url_functions_case_insensitively() {
    let mut state = InlineImportState::new();
    let css = "@import URL(\"https://example.com/nested.css\");\nbody { color: black; }";
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      assert_eq!(url, "https://example.com/nested.css");
      Ok(FetchedStylesheet::new("p { margin: 0; }".to_string(), None))
    };
    let out = inline_imports(
      css,
      "https://example.com/base.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");
    assert!(
      out.contains("p { margin: 0; }"),
      "expected imported stylesheet to be inlined: {out}"
    );
  }

  #[test]
  fn inline_imports_flattens_nested_imports() {
    let mut state = InlineImportState::new();
    let css = "@import \"nested.css\";\nbody { color: black; }";
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      if url.ends_with("nested.css") {
        Ok(FetchedStylesheet::new("p { margin: 0; }".to_string(), None))
      } else {
        Ok(FetchedStylesheet::new(String::new(), None))
      }
    };
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .unwrap();
    if !out.contains("p { margin: 0; }") {
      eprintln!("inline_imports output: {out}");
    }
    assert!(out.contains("p { margin: 0; }"));
    assert!(out.contains("body { color: black; }"));
  }

  #[test]
  fn inline_imports_passes_importing_stylesheet_as_referrer() {
    let mut state = InlineImportState::new();
    let mut calls: Vec<(String, String)> = Vec::new();
    let mut fetched = |url: &str, referrer: &str| -> Result<FetchedStylesheet> {
      calls.push((url.to_string(), referrer.to_string()));
      match url {
        "https://example.com/styles/imports/child.css" => {
          assert_eq!(referrer, "https://example.com/styles/main.css");
          Ok(FetchedStylesheet::new(
            "@import \"grand.css\";".to_string(),
            None,
          ))
        }
        "https://example.com/styles/imports/grand.css" => {
          assert_eq!(referrer, "https://example.com/styles/imports/child.css");
          Ok(FetchedStylesheet::new(
            "body { color: red; }".to_string(),
            None,
          ))
        }
        other => panic!("unexpected url {other}"),
      }
    };

    let out = inline_imports(
      "@import \"imports/child.css\";",
      "https://example.com/styles/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");
    assert!(out.contains("body { color: red; }"));
    assert_eq!(
      calls,
      vec![
        (
          "https://example.com/styles/imports/child.css".to_string(),
          "https://example.com/styles/main.css".to_string(),
        ),
        (
          "https://example.com/styles/imports/grand.css".to_string(),
          "https://example.com/styles/imports/child.css".to_string(),
        )
      ]
    );
  }

  #[test]
  fn inline_imports_rewrites_urls_against_final_url_after_redirect() {
    let mut state = InlineImportState::new();
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      assert_eq!(url, "https://example.com/redir.css");
      Ok(FetchedStylesheet::new(
        "div { background: url(\"asset.png\"); }".to_string(),
        Some("https://cdn.example.com/css/redir.css".to_string()),
      ))
    };

    let out = inline_imports(
      "@import \"redir.css\";",
      "https://example.com/root.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");
    assert!(
      out.contains("url(\"https://cdn.example.com/css/asset.png\")"),
      "expected url() inside redirected stylesheet to resolve against final_url: {out}"
    );
    assert!(
      !out.contains("url(\"https://example.com/asset.png\")"),
      "should not resolve url() against the original requested URL: {out}"
    );
  }

  #[test]
  fn inline_imports_resolves_urls_relative_to_imported_sheet() {
    let mut state = InlineImportState::new();
    let mut fetch = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      assert_eq!(url, "https://example.com/styles/imports/inner.css");
      Ok(FetchedStylesheet::new(
        "body { background: url(\"./img/bg.png\"); }".to_string(),
        None,
      ))
    };
    let mut diagnostics = Vec::new();
    let mut diag = |url: &str, reason: &str| diagnostics.push((url.to_string(), reason.to_string()));

    let output = inline_imports_with_diagnostics(
      "@import \"imports/inner.css\";",
      "https://example.com/styles/main.css",
      &mut fetch,
      &mut state,
      &mut diag,
      None,
    )
    .unwrap();

    assert!(
      output.contains("url(\"https://example.com/styles/imports/img/bg.png\")"),
      "url() inside imported sheet should resolve against that sheet"
    );
    assert!(diagnostics.is_empty());
  }

  #[test]
  fn inline_imports_reports_self_cycles() {
    let mut state = InlineImportState::new();
    state.register_stylesheet("https://example.com/main.css");
    let mut diagnostics = Vec::new();
    let mut diag = |url: &str, reason: &str| diagnostics.push((url.to_string(), reason.to_string()));

    let out = inline_imports_with_diagnostics(
      "@import url(\"main.css\");",
      "https://example.com/main.css",
      &mut |_url, _referrer| -> Result<FetchedStylesheet> { unreachable!("cycle should short-circuit") },
      &mut state,
      &mut diag,
      None,
    )
    .unwrap();

    assert!(out.trim().is_empty(), "cyclic imports should be skipped");
    assert!(
      diagnostics
        .iter()
        .any(|(url, reason)| url == "https://example.com/main.css" && reason.contains("cyclic")),
      "cycle diagnostics should be reported: {:?}",
      diagnostics
    );
  }

  #[test]
  fn inline_imports_respects_stylesheet_budget() {
    let mut state = InlineImportState::with_budget(StylesheetInlineBudget::new(2, 1024, 8));
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      if url.ends_with("first.css") {
        Ok(FetchedStylesheet::new(
          "h1 { color: rgb(1, 2, 3); }".to_string(),
          None,
        ))
      } else if url.ends_with("second.css") {
        Ok(FetchedStylesheet::new(
          "p { color: rgb(4, 5, 6); }".to_string(),
          None,
        ))
      } else {
        Err(Error::Io(std::io::Error::new(
          std::io::ErrorKind::NotFound,
          "unexpected url",
        )))
      }
    };
    let mut diags: Vec<(String, String)> = Vec::new();
    let mut record = |url: &str, reason: &str| diags.push((url.to_string(), reason.to_string()));
    let out = inline_imports_with_diagnostics(
      "@import \"first.css\";\n@import \"second.css\";\nbody { color: rgb(7, 8, 9); }",
      "https://example.com/root.css",
      &mut fetched,
      &mut state,
      &mut record,
      None,
    )
    .unwrap();
    assert!(
      out.contains("h1 { color: rgb(1, 2, 3); }"),
      "first import should inline within budget: {out}"
    );
    assert!(
      !out.contains("p { color: rgb(4, 5, 6); }"),
      "second import should be skipped once stylesheet budget is exhausted: {out}"
    );
    assert!(
      diags
        .iter()
        .any(|(url, reason)| url.ends_with("second.css") && reason.contains("budget")),
      "expected diagnostics about stylesheet budget cutoff: {:?}",
      diags
    );
  }

  #[test]
  fn inline_imports_respects_byte_budget() {
    let mut state = InlineImportState::with_budget(StylesheetInlineBudget::new(8, 64, 8));
    let big_css = "a { color: blue; }".repeat(8);
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      if url.ends_with("big.css") {
        Ok(FetchedStylesheet::new(big_css.clone(), None))
      } else {
        Ok(FetchedStylesheet::new(String::new(), None))
      }
    };
    let mut diags: Vec<(String, String)> = Vec::new();
    let mut record = |url: &str, reason: &str| diags.push((url.to_string(), reason.to_string()));
    let out = inline_imports_with_diagnostics(
      "@import \"big.css\";\nbody { color: black; }",
      "https://example.com/root.css",
      &mut fetched,
      &mut state,
      &mut record,
      None,
    )
    .unwrap();
    assert!(
      out.contains("body { color: black; }"),
      "base stylesheet content should remain even when imports are skipped: {out}"
    );
    assert!(
      !out.contains("color: blue"),
      "large imported stylesheet should be dropped once byte budget is exceeded: {out}"
    );
    assert!(
      diags
        .iter()
        .any(|(url, reason)| url.ends_with("big.css") && reason.contains("byte")),
      "expected diagnostics about byte budget cutoff: {:?}",
      diags
    );
    assert!(
      out.len() <= 64,
      "output should not exceed configured byte budget ({}): {}",
      out.len(),
      out
    );
  }

  #[test]
  fn inline_imports_respects_depth_budget() {
    let mut state = InlineImportState::with_budget(StylesheetInlineBudget::new(8, 1024, 2));
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      if url.ends_with("a.css") {
        Ok(FetchedStylesheet::new(
          "@import \"b.css\";\na { color: red; }".to_string(),
          None,
        ))
      } else {
        Ok(FetchedStylesheet::new("b { color: green; }".to_string(), None))
      }
    };
    let mut diags: Vec<(String, String)> = Vec::new();
    let mut record = |url: &str, reason: &str| diags.push((url.to_string(), reason.to_string()));
    let out = inline_imports_with_diagnostics(
      "@import \"a.css\";\nbody { color: black; }",
      "https://example.com/root.css",
      &mut fetched,
      &mut state,
      &mut record,
      None,
    )
    .unwrap();
    assert!(
      out.contains("a { color: red; }"),
      "first-level import should inline within depth budget: {out}"
    );
    assert!(
      !out.contains("b { color: green; }"),
      "imports beyond the depth budget should be dropped: {out}"
    );
    assert!(
      diags
        .iter()
        .any(|(url, reason)| url.ends_with("b.css") && reason.contains("depth")),
      "expected diagnostics about depth cutoff: {:?}",
      diags
    );
  }

  #[test]
  fn injects_before_uppercase_head_close() {
    let html = "<html><HEAD><title>Test</title></HEAD   ><body></body></html>";
    let css = "body { color: red; }";
    let injected = inject_css_into_html(html, css);
    assert_eq!(
      injected,
      "<html><HEAD><title>Test</title><style>body { color: red; }</style></HEAD   ><body></body></html>"
    );
  }

  #[test]
  fn injects_before_body_with_attributes() {
    let html = "<html><BODY class=\"main\" data-flag=\"1\">Content</BODY></html>";
    let css = "body { background: blue; }";
    let injected = inject_css_into_html(html, css);
    assert_eq!(
      injected,
      "<html><style>body { background: blue; }</style><BODY class=\"main\" data-flag=\"1\">Content</BODY></html>"
    );
  }

  #[test]
  fn inject_css_only_adds_style_block() {
    let html = "<!doctype html>\n<body>\n  <p>unchanged</p>\n</body>";
    let css = "p { margin: 0; }";
    let injected = inject_css_into_html(html, css);
    let style_tag = format!("<style>{css}</style>");
    assert_eq!(injected.matches(&style_tag).count(), 1);
    assert_eq!(injected.replace(&style_tag, ""), html);
  }

  #[test]
  fn inline_imports_handles_uppercase_at_keyword() {
    let mut state = InlineImportState::new();
    let css = "@IMPORT url(\"nested.css\");\nbody { color: black; }";
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      assert_eq!(url, "https://example.com/nested.css");
      Ok(FetchedStylesheet::new(
        "p { color: blue; }".to_string(),
        None,
      ))
    };
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .unwrap();
    assert!(out.contains("p { color: blue; }"));
    assert!(out.contains("body { color: black; }"));
  }

  #[test]
  fn inline_imports_handles_mixedcase_at_keyword() {
    let mut state = InlineImportState::new();
    let css = "@ImPoRt \"nested.css\";\nbody { color: black; }";
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      assert_eq!(url, "https://example.com/nested.css");
      Ok(FetchedStylesheet::new(
        "p { color: blue; }".to_string(),
        None,
      ))
    };
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .unwrap();
    assert!(out.contains("p { color: blue; }"));
    assert!(out.contains("body { color: black; }"));
  }

  #[test]
  fn inline_imports_handles_many_at_tokens() {
    let mut state = InlineImportState::new();
    let css = format!(
      "body {{ color: black; }}\n{}\nbody {{ color: blue; }}",
      "@".repeat(50_000)
    );
    let mut fetched = |_url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      panic!("inline_imports should not attempt to fetch when there are no @import rules")
    };
    let out = inline_imports(
      &css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .unwrap();
    assert_eq!(out, css);
  }

  #[test]
  fn inline_imports_preserves_media_wrappers_for_duplicates() {
    let mut state = InlineImportState::new();
    let css = "@import url(\"shared.css\") screen;\n@import url(\"shared.css\") print;";
    let mut fetched = |_url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      Ok(FetchedStylesheet::new(
        "p { color: green; }".to_string(),
        None,
      ))
    };
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .unwrap();
    if !out.contains("@media screen") || !out.contains("@media print") {
      eprintln!("inline_imports output: {out}");
    }
    assert!(out.contains("@media screen"));
    assert!(out.contains("@media print"));
    assert_eq!(out.matches("p { color: green; }").count(), 2);
  }

  #[test]
  fn inline_imports_ignores_late_import_after_style_rule() {
    let mut state = InlineImportState::new();
    let mut fetched_urls: Vec<String> = Vec::new();
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      fetched_urls.push(url.to_string());
      Ok(FetchedStylesheet::new(
        ".imported { color: blue; }".to_string(),
        None,
      ))
    };
    let css = "body { color: black; } @import \"late.css\";\n.x { color: red; }";
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");

    assert!(
      !out.contains(".imported"),
      "late @import should not be inlined: {out}"
    );
    assert!(
      !out.contains("@import"),
      "late @import should be dropped from the output: {out}"
    );
    assert!(
      fetched_urls.is_empty(),
      "late @import should not trigger fetches: {fetched_urls:?}"
    );
  }

  #[test]
  fn inline_imports_allows_import_after_layer_statement() {
    let mut state = InlineImportState::new();
    let mut fetched_urls: Vec<String> = Vec::new();
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      fetched_urls.push(url.to_string());
      Ok(FetchedStylesheet::new(
        ".imported { color: blue; }".to_string(),
        None,
      ))
    };
    let css = "@layer base; @import \"layered.css\";";
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");

    assert!(
      out.contains(".imported { color: blue; }"),
      "expected import after @layer statement to inline: {out}"
    );
    assert_eq!(
      fetched_urls,
      vec!["https://example.com/layered.css".to_string()],
      "expected import after @layer statement to be fetched"
    );
  }

  #[test]
  fn inline_imports_layer_block_blocks_subsequent_imports() {
    let mut state = InlineImportState::new();
    let mut fetched_urls: Vec<String> = Vec::new();
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      fetched_urls.push(url.to_string());
      if url.ends_with("/a.css") {
        Ok(FetchedStylesheet::new(
          ".from-a { color: blue; }".to_string(),
          None,
        ))
      } else if url.ends_with("/b.css") {
        Ok(FetchedStylesheet::new(
          ".from-b { color: green; }".to_string(),
          None,
        ))
      } else {
        Ok(FetchedStylesheet::new(String::new(), None))
      }
    };
    let css = "@import \"a.css\"; @layer base { .x { color: red; } } @import \"b.css\";";
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");

    assert!(
      out.contains(".from-a { color: blue; }"),
      "first @import should be inlined: {out}"
    );
    assert!(
      !out.contains(".from-b"),
      "@import after @layer block should be ignored: {out}"
    );
    assert!(
      out.contains(".x { color: red; }"),
      "@layer block contents should remain: {out}"
    );
    assert_eq!(
      fetched_urls,
      vec!["https://example.com/a.css".to_string()],
      "ignored imports should not be fetched"
    );
  }

  #[test]
  fn inline_imports_allows_import_after_charset() {
    let mut state = InlineImportState::new();
    let mut fetched_urls: Vec<String> = Vec::new();
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      fetched_urls.push(url.to_string());
      Ok(FetchedStylesheet::new(
        ".imported { color: blue; }".to_string(),
        None,
      ))
    };
    let css = "@charset \"UTF-8\"; @import \"charset.css\";";
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");

    assert!(
      out.contains(".imported { color: blue; }"),
      "expected import after @charset to inline: {out}"
    );
    assert_eq!(
      fetched_urls,
      vec!["https://example.com/charset.css".to_string()],
      "expected import after @charset to be fetched"
    );
  }

  #[test]
  fn inline_imports_inlines_named_layer_modifier() {
    let mut state = InlineImportState::new();
    let mut fetched_urls: Vec<String> = Vec::new();
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      fetched_urls.push(url.to_string());
      Ok(FetchedStylesheet::new(
        ".imported { color: blue; }".to_string(),
        None,
      ))
    };
    let css = "@import \"layered.css\" layer(foo);";
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");

    assert!(
      out.contains("@layer foo {"),
      "expected layer modifier to wrap inlined stylesheet: {out}"
    );
    assert!(
      !out.contains("@media layer("),
      "layer modifier must not be treated as media query: {out}"
    );
    assert!(
      out.contains(".imported { color: blue; }"),
      "expected imported stylesheet to be inlined: {out}"
    );
    assert_eq!(
      fetched_urls,
      vec!["https://example.com/layered.css".to_string()],
      "expected import with layer modifier to be fetched"
    );
  }

  #[test]
  fn inline_imports_declares_layer_on_failed_layered_import() {
    let mut state = InlineImportState::new();
    let mut fetched_urls: Vec<String> = Vec::new();
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      fetched_urls.push(url.to_string());
      Err(crate::error::Error::Other("network error".to_string()))
    };

    let css = r#"
      @import "missing.css" layer(foo);
      @layer bar { .x { color: red; } }
      @layer foo { .y { color: blue; } }
    "#;
    let out = inline_imports(
      css,
      "https://example.com/failed-layer/root.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");

    assert!(
      !out.contains("@import"),
      "failed @import should be removed from output: {out}"
    );

    let sheet = crate::css::parser::parse_stylesheet(&out).expect("parse output stylesheet");
    let Some(crate::css::types::CssRule::Layer(layer)) = sheet.rules.first() else {
      panic!(
        "expected a leading @layer declaration, got: {:?}",
        sheet.rules
      );
    };
    assert_eq!(layer.names, vec![vec!["foo".to_string()]]);
    assert!(layer.rules.is_empty());
    assert!(!layer.has_block);

    let media_ctx = crate::style::media::MediaContext::screen(800.0, 600.0);
    let collected = sheet.collect_style_rules(&media_ctx);
    let mut bar_layer = None;
    let mut foo_layer = None;
    for rule in &collected {
      let selector = rule
        .rule
        .selectors
        .slice()
        .first()
        .map(|sel| sel.to_css_string())
        .unwrap_or_default();
      if selector == ".x" {
        bar_layer = Some(rule.layer_order.clone());
      } else if selector == ".y" {
        foo_layer = Some(rule.layer_order.clone());
      }
    }

    let bar_layer = bar_layer.expect("expected to find bar layer");
    let foo_layer = foo_layer.expect("expected to find foo layer");
    assert!(
      foo_layer.as_ref() < bar_layer.as_ref(),
      "expected failing @import layer(foo) to still declare foo before bar (foo={foo_layer:?} bar={bar_layer:?})"
    );

    assert_eq!(
      fetched_urls,
      vec!["https://example.com/failed-layer/missing.css".to_string()],
      "expected failing import to still attempt fetching"
    );
  }

  #[test]
  fn inline_imports_propagates_render_errors() {
    let mut state = InlineImportState::new();
    let mut fetched = |_url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      Err(Error::Render(RenderError::Timeout {
        stage: RenderStage::Css,
        elapsed: std::time::Duration::from_millis(1),
      }))
    };

    let css = r#"@import "missing.css"; body { color: black; }"#;
    let err = inline_imports(
      css,
      "https://example.com/timeout/root.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect_err("expected render error to propagate");

    match err {
      RenderError::Timeout {
        stage: RenderStage::Css,
        elapsed,
      } => {
        assert_eq!(elapsed, std::time::Duration::from_millis(1));
      }
      other => panic!("expected RenderError::Timeout, got {other:?}"),
    }
  }

  #[test]
  fn inline_imports_preserves_escaped_layer_names() {
    let mut state = InlineImportState::new();
    let mut fetched = |_url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      Ok(FetchedStylesheet::new(
        ".imported { color: blue; }".to_string(),
        None,
      ))
    };
    // Layer names can contain escaped whitespace. Ensure we re-serialize them so the parser sees a
    // single identifier rather than treating the whitespace as a separator.
    let css = r#"@import "layered.css" layer(foo\ bar);"#;
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");

    let sheet = crate::css::parser::parse_stylesheet(&out).expect("parse output stylesheet");
    let Some(crate::css::types::CssRule::Layer(layer)) = sheet.rules.first() else {
      panic!(
        "expected first rule to be a @layer block, got: {:?}",
        sheet.rules
      );
    };
    assert_eq!(layer.names, vec![vec!["foo bar".to_string()]]);
    assert!(!layer.anonymous);
    assert!(layer.has_block);
  }

  #[test]
  fn inline_imports_inlines_anonymous_layer_modifier() {
    let mut state = InlineImportState::new();
    let mut fetched_urls: Vec<String> = Vec::new();
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      fetched_urls.push(url.to_string());
      Ok(FetchedStylesheet::new(
        ".imported { color: blue; }".to_string(),
        None,
      ))
    };
    let css = "@import \"layered.css\" layer;";
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");

    assert!(
      out.contains("@layer {"),
      "expected anonymous layer modifier to wrap inlined stylesheet: {out}"
    );
    assert!(
      !out.contains("@media layer"),
      "layer modifier must not be treated as media query: {out}"
    );
    assert!(out.contains(".imported { color: blue; }"));
    assert_eq!(
      fetched_urls,
      vec!["https://example.com/layered.css".to_string()]
    );
  }

  #[test]
  fn inline_imports_inlines_supports_modifier() {
    let mut state = InlineImportState::new();
    let mut fetched_urls: Vec<String> = Vec::new();
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      fetched_urls.push(url.to_string());
      Ok(FetchedStylesheet::new(
        ".imported { color: blue; }".to_string(),
        None,
      ))
    };
    let css = "@import \"supported.css\" supports((display: grid));";
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");

    assert!(
      out.contains("@supports (display: grid) {"),
      "expected supports modifier to wrap inlined stylesheet: {out}"
    );
    assert!(
      !out.contains("@media supports"),
      "supports modifier must not be treated as media query: {out}"
    );
    assert!(out.contains(".imported { color: blue; }"));
    assert_eq!(
      fetched_urls,
      vec!["https://example.com/supported.css".to_string()]
    );
  }

  #[test]
  fn inline_imports_nests_layer_supports_and_media_wrappers() {
    let mut state = InlineImportState::new();
    let mut fetched_urls: Vec<String> = Vec::new();
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      fetched_urls.push(url.to_string());
      Ok(FetchedStylesheet::new(
        ".imported { color: blue; }".to_string(),
        None,
      ))
    };
    let css = "@import \"combo.css\" layer(foo) supports((display: grid)) screen;";
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");

    if !out.contains("@media screen") {
      eprintln!("inline_imports output: {out}");
    }
    let media_pos = out.find("@media screen").expect("expected @media wrapper");
    let supports_pos = out
      .find("@supports (display: grid)")
      .expect("expected @supports wrapper");
    let layer_pos = out.find("@layer foo").expect("expected @layer wrapper");
    assert!(
      media_pos < supports_pos && supports_pos < layer_pos,
      "expected wrappers to be nested as @media > @supports > @layer: {out}"
    );
    assert!(out.contains(".imported { color: blue; }"));
    assert_eq!(
      fetched_urls,
      vec!["https://example.com/combo.css".to_string()]
    );
  }

  #[test]
  fn inline_imports_does_not_treat_at_layered_as_layer_statement() {
    let mut state = InlineImportState::new();
    let mut fetched_urls: Vec<String> = Vec::new();
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      fetched_urls.push(url.to_string());
      Ok(FetchedStylesheet::new(
        ".imported { color: blue; }".to_string(),
        None,
      ))
    };
    let css = "@layered foo; @import \"a.css\";";
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");

    assert!(
      !out.contains(".imported { color: blue; }"),
      "expected import after unknown @layered at-rule to be ignored: {out}"
    );
    assert!(
      !out.contains("\"a.css\""),
      "expected ignored import rule to be removed: {out}"
    );
    assert!(
      fetched_urls.is_empty(),
      "unknown @layered at-rule should terminate the import prelude (fetched={fetched_urls:?})"
    );
  }

  #[test]
  fn inline_imports_does_not_treat_at_imported_as_import() {
    let mut state = InlineImportState::new();
    let mut fetched_urls: Vec<String> = Vec::new();
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      fetched_urls.push(url.to_string());
      Ok(FetchedStylesheet::new(
        ".imported { color: blue; }".to_string(),
        None,
      ))
    };
    let css = "@imported \"ignored.css\"; @import \"a.css\";";
    let out = inline_imports(
      css,
      "https://example.com/main.css",
      &mut fetched,
      &mut state,
      None,
    )
    .expect("inline imports");

    assert!(
      !out.contains(".imported { color: blue; }"),
      "expected import after unknown @imported at-rule to be ignored: {out}"
    );
    assert!(
      !out.contains("\"a.css\""),
      "expected ignored import rule to be removed: {out}"
    );
    assert!(
      fetched_urls.is_empty(),
      "unknown @imported at-rule should terminate the import prelude (fetched={fetched_urls:?})"
    );
  }

  #[test]
  fn inline_imports_reports_cycles() {
    let mut state = InlineImportState::new();
    let mut fetched = |url: &str, _referrer: &str| -> Result<FetchedStylesheet> {
      if url.ends_with("a.css") {
        Ok(FetchedStylesheet::new(
          "@import \"b.css\";\nbody { color: red; }".to_string(),
          None,
        ))
      } else {
        Ok(FetchedStylesheet::new(
          "@import \"a.css\";".to_string(),
          None,
        ))
      }
    };
    let mut diags: Vec<(String, String)> = Vec::new();
    let mut record = |url: &str, reason: &str| {
      diags.push((url.to_string(), reason.to_string()));
    };
    let out = inline_imports_with_diagnostics(
      "@import \"a.css\";",
      "https://example.com/root.css",
      &mut fetched,
      &mut state,
      &mut record,
      None,
    )
    .unwrap();
    assert!(out.contains("body { color: red; }"));
    assert!(diags.iter().any(|(_, reason)| reason.contains("cyclic")));
  }

  #[test]
  fn parse_import_modifiers_and_media_preserves_media_queries() {
    let (layer, supports, media) =
      parse_import_modifiers_and_media("screen").expect("expected parse");
    assert!(layer.is_none(), "expected no layer modifier");
    assert!(supports.is_none(), "expected no supports modifier");
    assert_eq!(media, "screen");
  }

  #[test]
  fn extracts_stylesheet_hrefs_with_resolution() {
    let html = r#"
            <link rel="stylesheet" href="../styles/a.css">
            <link rel="alternate stylesheet" href="b.css">
            <link rel="icon" href="favicon.ico">
        "#;
    let urls = extract_css_links(
      html,
      "https://example.com/app/index.html",
      MediaType::Screen,
    )
    .unwrap();
    assert_eq!(urls.len(), 2);
    assert!(urls.contains(&"https://example.com/styles/a.css".to_string()));
    assert!(urls.contains(&"https://example.com/app/b.css".to_string()));
  }

  #[test]
  fn extract_css_links_preserves_non_ascii_whitespace_in_href() {
    let nbsp = "\u{00A0}";
    let html = format!(r#"<link rel="stylesheet" href="a.css{nbsp}">"#);
    let urls = extract_css_links(&html, "https://example.com/", MediaType::Screen).unwrap();
    assert_eq!(urls, vec!["https://example.com/a.css%C2%A0".to_string()]);
  }

  #[test]
  fn extract_css_links_does_not_treat_non_ascii_whitespace_as_attr_delimiter() {
    let nbsp = "\u{00A0}";
    let html = format!(r#"<link rel=stylesheet href=a.css{nbsp}>"#);
    let urls = extract_css_links(&html, "https://example.com/", MediaType::Screen).unwrap();
    assert_eq!(urls, vec!["https://example.com/a.css%C2%A0".to_string()]);
  }

  #[test]
  fn extract_css_links_preload_as_style_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let html = format!(r#"<link rel="preload" as="{nbsp}style" href="/bad.css">"#);
    let urls = extract_css_links(&html, "https://example.com/", MediaType::Screen).unwrap();
    assert!(urls.is_empty());

    let html = r#"<link rel="preload" as=" style " href="/good.css">"#;
    let urls = extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap();
    assert_eq!(urls, vec!["https://example.com/good.css".to_string()]);
  }

  #[test]
  fn parse_import_target_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let rule = format!("@import url(a.css{nbsp});");
    let (target, media) = parse_import_target(&rule).expect("parse");
    assert_eq!(target, format!("a.css{nbsp}"));
    assert!(media.is_empty());
  }

  #[test]
  fn extracts_unquoted_stylesheet_hrefs() {
    let html = r#"
            <link rel=stylesheet href=/styles/a.css media=screen>
            <link rel=stylesheet href=/styles/print.css media=print>
            <link rel=stylesheet href=/styles/b.css media=all>
        "#;
    let urls =
      extract_css_links(html, "https://example.com/app/page.html", MediaType::Screen).unwrap();
    assert_eq!(
      urls,
      vec![
        "https://example.com/styles/a.css".to_string(),
        "https://example.com/styles/b.css".to_string(),
      ]
    );
  }

  #[test]
  fn ignores_non_stylesheet_links_even_with_stylesheet_in_hrefs() {
    let html = r#"
            <link rel=icon href="/foo/stylesheet-logo.png">
            <link rel=icon data-kind="stylesheet icon">
        "#;
    let urls =
      extract_css_links(html, "https://example.com/app/page.html", MediaType::Screen).unwrap();
    assert!(urls.is_empty());
  }

  #[test]
  fn extract_css_links_ignores_stylesheets_inside_template() {
    let html = r#"
            <head>
              <template><link rel="stylesheet" href="/bad.css"></template>
              <link rel="stylesheet" href="/good.css">
            </head>
        "#;
    let urls = extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap();
    assert_eq!(urls, vec!["https://example.com/good.css".to_string()]);
  }

  #[test]
  fn extract_css_links_respects_noscript_scripting_mode() {
    let html = r#"
      <head>
        <link rel="stylesheet" href="/main.css">
        <noscript><link rel="stylesheet" href="/noscript.css"></noscript>
      </head>
    "#;

    let scripting_disabled = extract_css_links_with_meta_for_scripting_enabled(
      html,
      "https://example.com/",
      MediaType::Screen,
      false,
    )
    .unwrap()
    .into_iter()
    .map(|candidate| candidate.url)
    .collect::<Vec<_>>();
    assert_eq!(
      scripting_disabled,
      vec![
        "https://example.com/main.css".to_string(),
        "https://example.com/noscript.css".to_string(),
      ],
      "scripting disabled should treat <noscript> contents as normal markup"
    );

    let scripting_enabled = extract_css_links_with_meta_for_scripting_enabled(
      html,
      "https://example.com/",
      MediaType::Screen,
      true,
    )
    .unwrap()
    .into_iter()
    .map(|candidate| candidate.url)
    .collect::<Vec<_>>();
    assert_eq!(
      scripting_enabled,
      vec!["https://example.com/main.css".to_string()],
      "scripting enabled should treat <noscript> contents as text and ignore nested <link> tags"
    );
  }

  #[test]
  fn extract_css_links_without_dom_ignores_stylesheets_inside_template() {
    let html = r#"
      <template><link rel="stylesheet" href="/bad.css"></template>
      <link rel="stylesheet" href="/good.css">
    "#;
    let urls =
      extract_css_links_without_dom_with_meta(html, "https://example.com/", MediaType::Screen)
        .unwrap()
        .into_iter()
        .map(|candidate| candidate.url)
        .collect::<Vec<_>>();
    assert_eq!(urls, vec!["https://example.com/good.css".to_string()]);
  }

  #[test]
  fn extract_css_links_with_meta_parses_crossorigin() {
    let html = r#"
      <link rel="stylesheet" crossorigin href="/anon.css">
      <link rel="stylesheet" crossorigin="anonymous" href="/anon2.css">
      <link rel="stylesheet" crossorigin="use-credentials" href="/cred.css">
      <link rel="stylesheet" crossorigin="weird" href="/unknown.css">
      <link rel="stylesheet" href="/nocors.css">
    "#;

    let requests =
      extract_css_links_with_meta(html, "https://example.com/", MediaType::Screen).unwrap();
    assert_eq!(
      requests,
      vec![
        CssLinkCandidate {
          url: "https://example.com/anon.css".to_string(),
          crossorigin: Some(CorsMode::Anonymous),
          referrer_policy: None,
        },
        CssLinkCandidate {
          url: "https://example.com/anon2.css".to_string(),
          crossorigin: Some(CorsMode::Anonymous),
          referrer_policy: None,
        },
        CssLinkCandidate {
          url: "https://example.com/cred.css".to_string(),
          crossorigin: Some(CorsMode::UseCredentials),
          referrer_policy: None,
        },
        CssLinkCandidate {
          url: "https://example.com/unknown.css".to_string(),
          crossorigin: Some(CorsMode::Anonymous),
          referrer_policy: None,
        },
        CssLinkCandidate {
          url: "https://example.com/nocors.css".to_string(),
          crossorigin: None,
          referrer_policy: None,
        },
      ]
    );
  }

  #[test]
  fn extract_css_links_with_meta_dedupes_by_url_and_crossorigin() {
    let html = r#"
      <link rel="stylesheet" href="/a.css">
      <link rel="stylesheet" href="/a.css">
      <link rel="stylesheet" crossorigin href="/a.css">
      <link rel="stylesheet" crossorigin href="/a.css">
    "#;

    let requests =
      extract_css_links_with_meta(html, "https://example.com/", MediaType::Screen).unwrap();
    assert_eq!(
      requests,
      vec![
        CssLinkCandidate {
          url: "https://example.com/a.css".to_string(),
          crossorigin: None,
          referrer_policy: None,
        },
        CssLinkCandidate {
          url: "https://example.com/a.css".to_string(),
          crossorigin: Some(CorsMode::Anonymous),
          referrer_policy: None,
        },
      ]
    );
  }

  #[test]
  fn extract_css_links_dedupes_by_url_even_when_crossorigin_differs() {
    let html = r#"
      <link rel="stylesheet" href="/a.css">
      <link rel="stylesheet" crossorigin="anonymous" href="/a.css">
    "#;

    let urls = extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap();
    assert_eq!(urls, vec!["https://example.com/a.css".to_string()]);
  }

  #[test]
  fn extract_css_links_ignores_stylesheets_inside_svg() {
    let html = r#"
            <head>
              <svg>
                <link rel="stylesheet" href="/bad.css"></link>
              </svg>
              <link rel="stylesheet" href="/good.css">
            </head>
        "#;
    let urls = extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap();
    assert_eq!(urls, vec!["https://example.com/good.css".to_string()]);
  }

  #[test]
  fn extract_css_links_includes_declarative_shadow_dom_stylesheets() {
    let html = r#"
            <div id="host">
              <template shadowroot="open">
                <link rel="stylesheet" href="/shadow.css">
              </template>
            </div>
        "#;
    let urls = extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap();
    assert_eq!(urls, vec!["https://example.com/shadow.css".to_string()]);
  }

  #[test]
  fn extract_css_links_allows_screen_media_queries() {
    let html = r#"
            <link rel="stylesheet" media="screen and (min-width: 600px)" href="/a.css">
        "#;
    let urls = extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap();
    assert_eq!(urls, vec!["https://example.com/a.css".to_string()]);

    let print_urls = extract_css_links(html, "https://example.com/", MediaType::Print).unwrap();
    assert!(print_urls.is_empty());
  }

  #[test]
  fn extracts_alternate_stylesheet_hrefs() {
    let html = r#"
            <link rel="alternate stylesheet" href="/styles/alt.css">
        "#;
    let urls =
      extract_css_links(html, "https://example.com/app/page.html", MediaType::Screen).unwrap();
    assert_eq!(urls, vec!["https://example.com/styles/alt.css".to_string()]);
  }

  #[test]
  fn alternate_stylesheets_can_be_disabled() {
    let html = r#"
            <link rel="alternate stylesheet" href="/styles/alt.css">
        "#;
    let toggles = RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_ALTERNATE_STYLESHEETS".to_string(),
      "0".to_string(),
    )]));
    let urls = runtime::with_thread_runtime_toggles(Arc::new(toggles), || {
      extract_css_links(html, "https://example.com/app/page.html", MediaType::Screen).unwrap()
    });
    assert!(urls.is_empty());
  }

  #[test]
  fn unescapes_js_escaped_stylesheet_hrefs() {
    let html = r#"
            <link rel="stylesheet" href="https://cdn.example.com/app.css?foo=bar\u0026baz=qux">
        "#;
    let urls = extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap();
    assert_eq!(
      urls,
      vec!["https://cdn.example.com/app.css?foo=bar&baz=qux".to_string()]
    );
  }

  #[test]
  fn detects_embedded_css_urls() {
    let html = r#"
            <script>var cssUrl="assets/site.css?v=1";</script>
            <style>@import url("/shared/base.css");</style>
        "#;
    let urls = extract_embedded_css_urls(html, "https://example.com/app/").unwrap();
    assert!(urls.contains(&"https://example.com/app/assets/site.css?v=1".to_string()));
    assert!(urls.contains(&"https://example.com/shared/base.css".to_string()));
  }

  #[test]
  fn embedded_css_scan_ignores_urls_inside_template() {
    let html = r#"
      <template><script>var cssUrl="bad.css";</script></template>
      <script>var cssUrl="good.css";</script>
    "#;
    let urls = extract_embedded_css_urls(html, "https://example.com/").unwrap();
    assert_eq!(urls, vec!["https://example.com/good.css".to_string()]);
  }

  #[test]
  fn should_scan_embedded_css_urls_ignores_inline_style_inside_template() {
    let html = r#"<template><style>body { color: red; }</style></template><div></div>"#;
    assert!(should_scan_embedded_css_urls(html, false, 1));
  }

  #[test]
  fn inject_css_into_html_ignores_head_tags_inside_template() {
    let html = "<html><head><template></head></template></head><body></body></html>";
    let injected = inject_css_into_html(html, "body{color:red}");
    assert_eq!(
      injected,
      "<html><head><template></head></template><style>body{color:red}</style></head><body></body></html>"
    );
  }

  #[test]
  fn embedded_css_scan_does_not_panic_on_multibyte_prefix_before_quote() {
    let urls = extract_embedded_css_urls("cssurl:€'", "https://example.com/").unwrap();
    assert!(urls.is_empty());
  }

  #[test]
  fn normalizes_escaped_embedded_css_urls() {
    let html = r#"
            <link rel="stylesheet" href="https://cdn.example.com/styles/main.css">
            <script>
                var url = "https://cdn.example.com/styles/main.css\\\"/\u003c";
            </script>
        "#;
    let urls = extract_embedded_css_urls(html, "https://example.com/").unwrap();
    assert_eq!(
      urls,
      vec!["https://cdn.example.com/styles/main.css".to_string()]
    );
  }

  #[test]
  fn decodes_html_entities_in_stylesheet_hrefs() {
    let html = r#"
            <link rel="stylesheet" href="https://&#47;&#47;cdn.example.com&#47;main.css">
            <link rel="stylesheet" href="https://&/#47;&#47;cdn.example.com&#47;other.css">
            <link rel="stylesheet" href="https:////cdn.example.com////more.css">
        "#;
    let urls = extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap();
    assert_eq!(
      urls,
      vec![
        "https://cdn.example.com/main.css".to_string(),
        "https://cdn.example.com/other.css".to_string(),
        "https://cdn.example.com/more.css".to_string(),
      ]
    );
  }

  #[test]
  fn resolves_scheme_relative_urls() {
    let html = r#"
            <link rel="stylesheet" href="//cdn.example.com/app.css">
        "#;
    let urls = extract_css_links(html, "https://example.com/page", MediaType::Screen).unwrap();
    assert_eq!(urls, vec!["https://cdn.example.com/app.css".to_string()]);

    let resolved = resolve_href("https://example.com/page", "//cdn.example.com/app.css");
    assert_eq!(
      resolved,
      Some("https://cdn.example.com/app.css".to_string())
    );
  }

  #[test]
  fn skips_print_only_stylesheets() {
    let html = r#"
            <link rel="stylesheet" media="print" href="https://cdn.example.com/print.css">
            <link rel="stylesheet" media="print, screen" href="https://cdn.example.com/both.css">
            <link rel="stylesheet" media="screen" href="https://cdn.example.com/screen.css">
        "#;
    let urls = extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap();
    assert_eq!(
      urls,
      vec![
        "https://cdn.example.com/both.css".to_string(),
        "https://cdn.example.com/screen.css".to_string(),
      ]
    );
  }

  #[test]
  fn unescapes_json_style_embedded_urls() {
    let html = r#"
            <script>
                window.css = "https:\\/\\/cdn.example.com\\/app.css\\"";
            </script>
        "#;
    let urls = extract_embedded_css_urls(html, "https://example.com/").unwrap();
    assert_eq!(urls, vec!["https://cdn.example.com/app.css".to_string()]);
  }

  #[test]
  fn ignores_sourceurl_comments_in_embedded_css_scan() {
    let html = r"
            <style>
            /*# sourceURL=https://example.com/wp-includes/blocks/button/style.min.css */
            body { color: black; }
            </style>
        ";
    let urls = extract_embedded_css_urls(html, "https://example.com/").unwrap();
    assert!(urls.is_empty());
  }

  #[test]
  fn unescapes_js_escaped_embedded_css_urls() {
    let html = r#"
            <script>
                const css = "https://cdn.example.com/app.css?foo=bar\\u0026baz=qux";
            </script>
        "#;
    let urls = extract_embedded_css_urls(html, "https://example.com/").unwrap();
    assert_eq!(
      urls,
      vec!["https://cdn.example.com/app.css?foo=bar&baz=qux".to_string()]
    );
  }

  #[test]
  fn embedded_scan_skips_print_only_link_tags() {
    let html = r#"
            <link rel="stylesheet" media="print" href="https://cdn.example.com/print.css">
        "#;
    let urls = extract_embedded_css_urls(html, "https://example.com/").unwrap();
    assert!(urls.is_empty());
  }

  #[test]
  fn decodes_html_entities_in_embedded_css_urls() {
    let html = r#"
            <script>
                const css = "https://&/#47;&#47;cdn.example.com&#47;main.css";
                const other = "https:////cdn.example.com////more.css";
            </script>
        "#;
    let urls = extract_embedded_css_urls(html, "https://example.com/").unwrap();
    assert_eq!(
      urls,
      vec![
        "https://cdn.example.com/main.css".to_string(),
        "https://cdn.example.com/more.css".to_string()
      ]
    );
  }

  #[test]
  fn strips_sourceurl_prefix_in_embedded_css_urls() {
    let html = r#"
            <script>
                const url = "sourceURL=https://example.com/assets/style.css";
            </script>
        "#;
    let urls = extract_embedded_css_urls(html, "https://example.com/").unwrap();
    assert_eq!(
      urls,
      vec!["https://example.com/assets/style.css".to_string()]
    );
  }

  #[test]
  fn embedded_css_discovery_enforces_candidate_cap() {
    let mut html = String::new();
    for i in 0..100 {
      html.push_str(&format!(
        r#"<script>var css="style{idx}.css";</script>"#,
        idx = i
      ));
    }

    let toggles = RuntimeToggles::from_map(HashMap::from([(
      "FASTR_EMBEDDED_CSS_MAX_CANDIDATES".to_string(),
      "5".to_string(),
    )]));
    let urls = runtime::with_thread_runtime_toggles(Arc::new(toggles), || {
      extract_embedded_css_urls(&html, "https://example.com/").unwrap()
    });
    assert_eq!(urls.len(), 5);
  }

  #[test]
  fn embedded_css_discovery_is_gated_by_static_stylesheets() {
    let mut html = String::from(r#"<link rel="stylesheet" href="/static.css">"#);
    for i in 0..50 {
      html.push_str(&format!(
        r#"<script>var css="dyn{idx}.css";</script>"#,
        idx = i
      ));
    }

    let base_url = "https://example.com/";
    let css_links = extract_css_links(&html, base_url, MediaType::Screen).unwrap();
    assert_eq!(
      css_links,
      vec!["https://example.com/static.css".to_string()]
    );

    let should_scan = should_scan_embedded_css_urls(&html, !css_links.is_empty(), usize::MAX);
    assert!(!should_scan);
    let urls = if should_scan {
      extract_embedded_css_urls(&html, base_url).unwrap()
    } else {
      Vec::new()
    };
    assert!(urls.is_empty());
  }

  #[test]
  fn includes_preload_style_links() {
    let html = r#"
            <link rel=preload as=style href=/a.css>
        "#;
    let toggles = RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_PRELOAD_STYLESHEETS".to_string(),
      "1".to_string(),
    )]));
    let urls = runtime::with_thread_runtime_toggles(Arc::new(toggles), || {
      extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap()
    });
    assert_eq!(urls, vec!["https://example.com/a.css".to_string()]);
  }

  #[test]
  fn ignores_non_style_preloads() {
    let html = r#"
            <link rel=preload as=font href=/a.css>
        "#;
    let urls = extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap();
    assert!(urls.is_empty());
  }

  #[test]
  fn preload_style_links_can_be_disabled() {
    let html = r#"
            <link rel=preload as=style href=/a.css>
        "#;
    let toggles = RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_PRELOAD_STYLESHEETS".to_string(),
      "0".to_string(),
    )]));
    let urls = runtime::with_thread_runtime_toggles(Arc::new(toggles), || {
      extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap()
    });
    assert!(urls.is_empty());
  }

  #[test]
  fn modulepreload_style_links_are_opt_in() {
    let html = r#"
            <link rel=modulepreload as=style href=/a.css>
        "#;
    let disabled = RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_MODULEPRELOAD_STYLESHEETS".to_string(),
      "0".to_string(),
    )]));
    let default_urls = runtime::with_thread_runtime_toggles(Arc::new(disabled), || {
      extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap()
    });
    assert!(default_urls.is_empty());

    let enabled = RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_MODULEPRELOAD_STYLESHEETS".to_string(),
      "1".to_string(),
    )]));
    let enabled_urls = runtime::with_thread_runtime_toggles(Arc::new(enabled), || {
      extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap()
    });
    assert_eq!(enabled_urls, vec!["https://example.com/a.css".to_string()]);
  }

  #[test]
  fn dedupes_stylesheet_links_preserving_order() {
    let html = r#"
            <link rel="stylesheet" href="/a.css">
            <link rel="stylesheet" href="/b.css">
            <link rel="stylesheet" href="/a.css">
        "#;
    let urls = extract_css_links(
      html,
      "https://example.com/app/index.html",
      MediaType::Screen,
    )
    .unwrap();
    assert_eq!(
      urls,
      vec![
        "https://example.com/a.css".to_string(),
        "https://example.com/b.css".to_string(),
      ]
    );
  }

  #[test]
  fn dedupes_preload_and_stylesheet_preserving_order() {
    let html = r#"
            <link rel=preload as=style href="/a.css">
            <link rel="stylesheet" href="/a.css">
            <link rel="stylesheet" href="/b.css">
        "#;
    let urls = extract_css_links(
      html,
      "https://example.com/app/index.html",
      MediaType::Screen,
    )
    .unwrap();
    assert_eq!(
      urls,
      vec![
        "https://example.com/a.css".to_string(),
        "https://example.com/b.css".to_string(),
      ]
    );
  }

  #[test]
  fn ignores_embedded_css_class_tokens() {
    let html = r"
            <style>
                .css-v2kfba{height:100%;width:100%;}
            </style>
            <script>
                const cls = '.css-15ru6p1{font-size:inherit;font-weight:normal;}'
            </script>
        ";
    let urls = extract_embedded_css_urls(html, "https://example.com/").unwrap();
    assert!(urls.is_empty());
  }

  #[test]
  fn ignores_percent_encoded_css_class_tokens() {
    let html = r#"
            <script>
                const bogus = ">%3E.css-v2kfba%7Bheight:100%;width:100%;%7D%3C/style";
            </script>
        "#;
    let urls = extract_embedded_css_urls(html, "https://example.com/").unwrap();
    assert!(urls.is_empty());
  }

  #[test]
  fn does_not_fall_back_to_print_styles_when_no_screen_stylesheets() {
    let html = r#"
            <link rel="stylesheet" media="print" href="/print.css">
        "#;
    let urls = extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap();
    assert!(urls.is_empty());
  }

  #[test]
  fn does_not_fall_back_to_unquoted_print_stylesheets() {
    let html = r#"
            <link rel=stylesheet media=print href=/print.css>
        "#;
    let urls = extract_css_links(html, "https://example.com/", MediaType::Screen).unwrap();
    assert!(urls.is_empty());
  }

  #[test]
  fn infers_base_from_cached_file() {
    let html = "<html><head></head></html>";
    let base = infer_base_url(html, "file:///tmp/fetches/html/news.ycombinator.com.html");
    assert_eq!(base, "https://news.ycombinator.com/");
  }

  #[test]
  fn canonicalizes_relative_file_url_before_inference() {
    let html = "<html><head></head></html>";

    let rel_suffix = Path::new("fetches")
      .join("html")
      .join("news.ycombinator.com.html");
    let input = "file://fetches/html/../html/./news.ycombinator.com.html";

    let canonicalized = canonicalize_file_input_url(input);
    let parsed = Url::parse(canonicalized.as_ref()).expect("file URL");
    assert_eq!(parsed.scheme(), "file");
    let canonical_path = parsed.to_file_path().expect("file path");
    assert!(canonical_path.is_absolute(), "expected absolute file path");
    assert!(
      canonical_path.ends_with(&rel_suffix),
      "expected canonicalized path to end with {} (got {})",
      rel_suffix.display(),
      canonical_path.display()
    );
    assert!(
      !canonical_path
        .components()
        .any(|c| matches!(c, Component::CurDir | Component::ParentDir)),
      "expected dot-segments to be removed (got {})",
      canonical_path.display()
    );

    let inferred = infer_base_url(html, input);
    // We still expect the HTTPS origin guess.
    assert_eq!(inferred, "https://news.ycombinator.com/");
  }

  #[test]
  fn prefers_document_url_over_canonical_for_http_inputs() {
    let html = r#"
            <link rel=canonical href="https://example.com/">
        "#;
    let base = infer_base_url(html, "https://example.com/path/page.html");
    assert_eq!(base, "https://example.com/path/page.html");

    let resolved = resolve_href(&base, "../styles/app.css").expect("resolved");
    assert_eq!(resolved, "https://example.com/styles/app.css");
  }

  #[test]
  fn uses_canonical_hint_for_file_inputs() {
    let html = r#"
            <link rel="canonical" href="https://example.net/app/">
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.net.html");
    assert_eq!(base, "https://example.net/app/");
  }

  #[test]
  fn uses_canonical_hint_preserving_non_ascii_whitespace_for_file_inputs() {
    let nbsp = "\u{00A0}";
    let html = format!(r#"<link rel="canonical" href="https://example.net/app/{nbsp}">"#);
    let base = infer_base_url(&html, "file:///tmp/cache/example.net.html");
    assert_eq!(base, "https://example.net/app/%C2%A0");
  }

  #[test]
  fn uses_relative_canonical_hint_for_file_inputs() {
    let html = r#"
            <link rel="canonical" href="/app/">
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.net.html");
    assert_eq!(base, "https://example.net/app/");
  }

  #[test]
  fn uses_single_quoted_canonical_for_file_inputs() {
    let html = r#"
            <link rel='canonical' href='https://example.net/single/'>
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.net.single.html");
    assert_eq!(base, "https://example.net/single/");
  }

  #[test]
  fn uses_unquoted_canonical_for_file_inputs() {
    let html = r#"
            <link rel=canonical href="https://example.net/unquoted/">
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.net.unquoted.html");
    assert_eq!(base, "https://example.net/unquoted/");
  }

  #[test]
  fn uses_single_quoted_og_url_for_file_inputs() {
    let html = r#"
            <meta property='og:url' content='https://example.org/from-og/'>
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.org.from-og.html");
    assert_eq!(base, "https://example.org/from-og/");
  }

  #[test]
  fn uses_unquoted_og_url_for_file_inputs() {
    let html = r#"
            <meta property=og:url content="https://example.org/unquoted-og/">
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.org.unquoted.html");
    assert_eq!(base, "https://example.org/unquoted-og/");
  }

  #[test]
  fn infer_document_url_guess_from_dom_deep_dom_does_not_overflow_stack() {
    const DEPTH: usize = 20_000;

    let meta = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "meta".to_string(),
        namespace: String::new(),
        attributes: vec![
          ("property".to_string(), "og:url".to_string()),
          (
            "content".to_string(),
            "https://example.com/app/".to_string(),
          ),
        ],
      },
      children: Vec::new(),
    };

    let mut node = meta;
    for _ in 0..DEPTH {
      node = DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: String::new(),
          attributes: Vec::new(),
        },
        children: vec![node],
      };
    }

    let dom = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
        scripting_enabled: true,
        is_html_document: true,
      },
      children: vec![node],
    };

    let inferred = infer_document_url_guess_from_dom(&dom, "file:///tmp/cache/example.com.html");
    assert_eq!(inferred.as_ref(), "https://example.com/app/");
  }

  #[test]
  fn infer_base_url_ignores_canonical_like_text_in_scripts_for_file_inputs() {
    let html = r#"
            <head>
              <script>var s = '<link rel="canonical" href="https://bad.example/poison/">';</script>
              <link rel="canonical" href="https://good.example/app/">
            </head>
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.net.html").into_owned();
    assert_eq!(base, "https://good.example/app/");
  }

  #[test]
  fn infer_base_url_ignores_canonical_inside_template_for_file_inputs() {
    let html = r#"
            <head>
              <template><link rel="canonical" href="https://bad.example/poison/"></template>
              <link rel="canonical" href="https://good.example/app/">
            </head>
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.net.html").into_owned();
    assert_eq!(base, "https://good.example/app/");
  }

  #[test]
  fn infer_base_url_ignores_canonical_inside_declarative_shadow_dom_for_file_inputs() {
    let html = r#"
            <head>
              <div id="host">
                <template shadowroot="open">
                  <link rel="canonical" href="https://bad.example/poison/">
                </template>
              </div>
              <link rel="canonical" href="https://good.example/app/">
            </head>
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.net.html").into_owned();
    assert_eq!(base, "https://good.example/app/");
  }

  #[test]
  fn infer_base_url_ignores_canonical_only_shadow_dom_for_file_inputs() {
    let html = r#"
            <head>
              <div id="host">
                <template shadowroot="open">
                  <link rel="canonical" href="https://bad.example/poison/">
                </template>
              </div>
            </head>
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.net.html").into_owned();
    assert_eq!(base, "https://example.net/");
  }

  #[test]
  fn infer_base_url_ignores_canonical_inside_svg_for_file_inputs() {
    let html = r#"
            <head>
              <svg>
                <link rel="canonical" href="https://bad.example/poison/"></link>
              </svg>
              <link rel="canonical" href="https://good.example/app/">
            </head>
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.net.html").into_owned();
    assert_eq!(base, "https://good.example/app/");
  }

  #[test]
  fn infer_base_url_ignores_og_url_like_text_in_scripts_for_file_inputs() {
    let html = r#"
            <head>
              <script>var s = '<meta property="og:url" content="https://bad.example/poison/">';</script>
              <meta property="og:url" content="https://good.example/app/">
            </head>
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.net.html").into_owned();
    assert_eq!(base, "https://good.example/app/");
  }

  #[test]
  fn infer_base_url_ignores_og_url_inside_template_for_file_inputs() {
    let html = r#"
            <head>
              <template><meta property="og:url" content="https://bad.example/poison/"></template>
              <meta property="og:url" content="https://good.example/app/">
            </head>
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.net.html").into_owned();
    assert_eq!(base, "https://good.example/app/");
  }

  #[test]
  fn infer_base_url_ignores_og_url_inside_declarative_shadow_dom_for_file_inputs() {
    let html = r#"
            <head>
              <div id="host">
                <template shadowroot="open">
                  <meta property="og:url" content="https://bad.example/poison/">
                </template>
              </div>
              <meta property="og:url" content="https://good.example/app/">
            </head>
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.net.html").into_owned();
    assert_eq!(base, "https://good.example/app/");
  }

  #[test]
  fn infer_base_url_ignores_og_url_only_shadow_dom_for_file_inputs() {
    let html = r#"
            <head>
              <div id="host">
                <template shadowroot="open">
                  <meta property="og:url" content="https://bad.example/poison/">
                </template>
              </div>
            </head>
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.org.html").into_owned();
    assert_eq!(base, "https://example.org/");
  }

  #[test]
  fn infer_base_url_ignores_og_url_inside_svg_for_file_inputs() {
    let html = r#"
            <head>
              <svg>
                <meta property="og:url" content="https://bad.example/poison/"></meta>
              </svg>
              <meta property="og:url" content="https://good.example/app/">
            </head>
        "#;
    let base = infer_base_url(html, "file:///tmp/cache/example.net.html").into_owned();
    assert_eq!(base, "https://good.example/app/");
  }

  #[test]
  fn infer_base_url_ignores_base_inside_template() {
    let html = "<html><head><template><base href='https://bad/'></template><base href='https://good/'></head></html>";
    let base = infer_base_url(html, "https://example.com/").into_owned();
    assert_eq!(base, "https://good/");
  }

  #[test]
  fn infer_base_url_ignores_base_inside_declarative_shadow_dom_template() {
    let html = "<div><template shadowroot='open'><base href='https://bad/'></template></div><base href='https://good/'>";
    let base = infer_base_url(html, "https://example.com/").into_owned();
    assert_eq!(base, "https://good/");
  }

  #[test]
  fn infer_base_url_honors_relative_base_href_with_trailing_slash() {
    let html = r#"<html><head><base href="static/"></head></html>"#;
    let base = infer_base_url(html, "https://example.com/site/page.html").into_owned();
    assert_eq!(base, "https://example.com/site/static/");

    let resolved = resolve_href(&base, "css/app.css").expect("resolved stylesheet URL");
    assert_eq!(resolved, "https://example.com/site/static/css/app.css");
  }

  #[test]
  fn infer_base_url_preserves_file_like_base_href_without_trailing_slash() {
    let html = r#"<html><head><base href="assets"></head></html>"#;
    let base = infer_base_url(html, "https://example.com/root/page.html").into_owned();
    assert_eq!(base, "https://example.com/root/assets");

    let resolved = resolve_href(&base, "img/logo.png").expect("resolved asset URL");
    assert_eq!(resolved, "https://example.com/root/img/logo.png");
  }

  #[test]
  fn infer_base_url_resolves_relative_base_against_canonical_for_file_input() {
    let html = "<link rel=canonical href='https://example.com/app/'><base href='assets/'>";
    let base = infer_base_url(html, "file:///tmp/cache/example.com.html").into_owned();
    assert_eq!(base, "https://example.com/app/assets/");
  }

  #[test]
  fn infer_base_url_resolves_relative_base_against_relative_canonical_for_file_input() {
    let html = "<link rel=canonical href='/app/'><base href='assets/'>";
    let base = infer_base_url(html, "file:///tmp/cache/example.com.html").into_owned();
    assert_eq!(base, "https://example.com/app/assets/");
  }

  #[test]
  fn infer_base_url_resolves_relative_base_against_relative_og_url_for_file_input() {
    let html = "<meta property='og:url' content='/app/'><base href='assets/'>";
    let base = infer_base_url(html, "file:///tmp/cache/example.com.html").into_owned();
    assert_eq!(base, "https://example.com/app/assets/");
  }

  #[test]
  fn unescape_js_handles_slashes_and_quotes() {
    let input = r#"https:\/\/example.com\/path\"quoted\'"#;
    let unescaped = unescape_js_escapes(input);
    assert_eq!(unescaped, "https://example.com/path\"quoted\'");
  }

  #[test]
  fn unescape_js_handles_unicode_escapes() {
    let input = r"foo\u0026bar\U0041baz"; // & and 'A'
    let unescaped = unescape_js_escapes(input);
    assert_eq!(unescaped, "foo&barAbaz");
  }

  #[test]
  fn unescape_js_handles_hex_escapes() {
    let input = r"foo\x26bar\x3Abaz"; // & and ':'
    let unescaped = unescape_js_escapes(input);
    assert_eq!(unescaped, "foo&bar:baz");
  }

  #[test]
  fn unescape_js_handles_unicode_surrogate_pairs() {
    let input = r"\uD83D\uDE0D"; // U+1F60D (😍)
    let unescaped = unescape_js_escapes(input);
    assert_eq!(unescaped, "\u{1F60D}");
  }

  #[test]
  fn unescape_js_handles_unicode_braced_escape() {
    let input = r"\u{1F60D}"; // U+1F60D (😍)
    let unescaped = unescape_js_escapes(input);
    assert_eq!(unescaped, "\u{1F60D}");
  }

  #[test]
  fn unescape_js_preserves_unpaired_surrogates() {
    let input = r"\uD800\u0061";
    let unescaped = unescape_js_escapes(input);
    assert_eq!(unescaped, input);
  }

  #[test]
  fn unescape_js_borrows_when_unescaped() {
    use std::borrow::Cow;

    let input = "https://example.com/path";
    let out = unescape_js_escapes(input);
    match out {
      Cow::Borrowed(s) => assert_eq!(s, input),
      Cow::Owned(_) => panic!("expected borrowed output for unescaped input"),
    }
  }

  #[test]
  fn resolve_href_unescapes_js_escapes() {
    let base = "https://example.com/";
    let href = r"https:\/\/cdn.example.com\/styles\/main.css";
    let resolved = resolve_href(base, href).expect("resolved href");
    assert_eq!(resolved, "https://cdn.example.com/styles/main.css");
  }

  #[test]
  fn resolve_href_preserves_data_urls() {
    let base = "https://example.com/";
    let href = "data:text/css,body%7Bcolor%3Ared%7D";
    let resolved = resolve_href(base, href).expect("resolved href");
    assert_eq!(resolved, href);
  }

  #[test]
  fn resolve_href_with_file_base_directory() {
    let dir = tempfile::tempdir().unwrap();
    let mut base_url = Url::from_directory_path(dir.path()).unwrap();
    let trimmed = base_url.path().trim_end_matches('/').to_string();
    base_url.set_path(&trimmed);
    let base = base_url.to_string();
    let resolved = resolve_href(&base, "styles/app.css").expect("resolved file href");
    let expected = Url::from_file_path(dir.path().join("styles/app.css"))
      .unwrap()
      .to_string();
    assert_eq!(resolved, expected);
  }

  #[test]
  fn resolve_href_with_file_base_file_parent() {
    let dir = tempfile::tempdir().unwrap();
    let base = Url::from_file_path(dir.path().join("html/page.html"))
      .unwrap()
      .to_string();
    std::fs::create_dir_all(dir.path().join("html")).unwrap();
    let resolved = resolve_href(&base, "../styles/app.css").expect("resolved file href");
    let expected = Url::from_file_path(dir.path().join("styles/app.css"))
      .unwrap()
      .to_string();
    assert_eq!(resolved, expected);
  }

  #[test]
  fn resolve_href_rejects_non_parseable_base() {
    // Base that cannot be parsed as URL or file path should yield None
    let base = "not-a-url";
    assert_eq!(resolve_href(base, "styles/app.css"), None);
  }

  #[test]
  fn resolve_href_rejects_script_and_mailto_schemes() {
    let base = "https://example.com/";
    assert_eq!(resolve_href(base, "javascript:alert(1)"), None);
    assert_eq!(resolve_href(base, "mailto:test@example.com"), None);
    assert_eq!(resolve_href(base, "vbscript:msgbox('hi')"), None);

    // Schemes are matched case-insensitively.
    assert_eq!(resolve_href(base, "JaVaScRiPt:alert(1)"), None);
    assert_eq!(resolve_href(base, "MAILTO:UPPER@EXAMPLE.COM"), None);
    assert_eq!(resolve_href(base, "VbScRiPt:msgbox('hi')"), None);
  }

  #[test]
  fn resolve_href_ignores_fragment_only_hrefs() {
    let base = "https://example.com/";
    assert_eq!(resolve_href(base, "#section"), None);
    assert_eq!(resolve_href(base, "#"), None);
  }

  #[test]
  fn resolve_href_trims_whitespace() {
    let base = "https://example.com/";
    let resolved = resolve_href(base, "   ./foo.css \n").expect("resolved href");
    assert_eq!(resolved, "https://example.com/foo.css");
  }

  #[test]
  fn resolve_href_rejects_whitespace_only() {
    let base = "https://example.com/";
    assert_eq!(resolve_href(base, "   \t\n"), None);
  }

  #[test]
  fn decode_html_entities_decodes_known_and_preserves_unknown() {
    let input = "&amp;&lt;&gt;&quot;&apos;&#65;&#x41;&copy;";
    let decoded = decode_html_entities(input);
    assert_eq!(decoded, "&<>\"'AA&copy;");
  }

  #[test]
  fn normalize_scheme_slashes_collapses_extra_slashes() {
    let input = "https:////example.com//foo//bar";
    let normalized = normalize_scheme_slashes(input);
    assert_eq!(normalized, "https://example.com/foo/bar");
  }

  #[test]
  fn normalize_scheme_slashes_preserves_scheme_relative() {
    let input = "//cdn.example.com//assets//img.png";
    let normalized = normalize_scheme_slashes(input);
    assert_eq!(normalized, input);
  }

  #[test]
  fn normalize_scheme_slashes_preserves_embedded_scheme_in_query() {
    let input = "https://example.com/?u=https://cdn.com/x//y";
    let normalized = normalize_scheme_slashes(input);
    assert_eq!(normalized, input);
  }

  #[test]
  fn normalize_scheme_slashes_only_touches_path_before_query_and_fragment() {
    let input =
      "https:////cdn.example.com////more.css?redirect=https://cdn.example.com//next#hash=https://foo.bar//baz";
    let normalized = normalize_scheme_slashes(input);
    let expected =
      "https://cdn.example.com/more.css?redirect=https://cdn.example.com//next#hash=https://foo.bar//baz";
    assert_eq!(normalized, expected);
  }

  #[test]
  fn resolve_href_returns_none_for_empty_href() {
    let base = "https://example.com/";
    assert_eq!(resolve_href(base, ""), None);
  }

  #[test]
  fn unescape_js_preserves_invalid_sequences() {
    let input = r"bad\u00zzescape and \q";
    let unescaped = unescape_js_escapes(input);
    assert_eq!(unescaped, input);
  }
}
