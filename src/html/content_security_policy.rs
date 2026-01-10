//! Minimal Content Security Policy (CSP) parsing and matching.
//!
//! FastRender historically enforced a small set of `ResourceAccessPolicy` knobs (same origin,
//! mixed content, etc). Many real pages additionally gate resource loading via Content Security
//! Policy. This module implements a deliberately small-but-useful subset:
//!
//! - Directives: `default-src`, `style-src`, `img-src`, `font-src`, `connect-src`, `frame-src`,
//!   `script-src`, `script-src-elem`, `script-src-attr`
//! - Source expressions:
//!   - `'self'`, `'none'`
//!   - `'unsafe-inline'` (inline scripts only; ignored when a nonce/hash is present, matching
//!     modern browser behavior)
//!   - `'strict-dynamic'` (only affects script directives; treated conservatively)
//!   - `'nonce-…'` (inline + external `<script nonce=...>`)
//!   - `'sha256-…'` (inline scripts only; base64-encoded SHA-256 of the inline source bytes)
//!   - `*`
//!   - scheme sources like `https:`, `http:`, `data:` (and other valid `scheme:` tokens)
//!   - host sources like `example.com`, `https://example.com`, `*.example.com`
//!
//! Notes / intentionally omitted (v1):
//! - Most of CSP3 (`'unsafe-eval'`, `'strict-dynamic'` trust propagation, `unsafe-hashes`, etc.).
//!   For `strict-dynamic` we do **not** implement trust propagation; when `strict-dynamic` is
//!   present alongside a nonce/hash, we conservatively treat URL-based allowlisting as disabled,
//!   requiring explicit nonces on `<script>` elements.
//! - Hash sources other than `'sha256-…'` (sha384/sha512, etc.).
//! - Nonce/hash/`'unsafe-inline'` semantics for non-script inline contexts (styles, event handlers,
//!   etc.). These tokens are ignored for URL-based matching and therefore do not allow external
//!   loads by themselves.

use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use crate::error::{Error, RenderStage, Result};
use crate::render_control::check_active_periodic;
use crate::resource::{origin_from_url, DocumentOrigin, FetchedResource};
use memchr::memchr;
use rustc_hash::FxHashMap;
use std::ops::ControlFlow;
use url::Url;

const CSP_DEADLINE_STRIDE: usize = 1024;
const MAX_CSP_SCAN_BYTES: usize = 256 * 1024;
const MAX_ATTRIBUTES_PER_TAG: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CspDirective {
  DefaultSrc,
  ScriptSrc,
  ScriptSrcElem,
  ScriptSrcAttr,
  StyleSrc,
  ImgSrc,
  FontSrc,
  ConnectSrc,
  FrameSrc,
}

impl CspDirective {
  pub const fn as_str(self) -> &'static str {
    match self {
      CspDirective::DefaultSrc => "default-src",
      CspDirective::ScriptSrc => "script-src",
      CspDirective::ScriptSrcElem => "script-src-elem",
      CspDirective::ScriptSrcAttr => "script-src-attr",
      CspDirective::StyleSrc => "style-src",
      CspDirective::ImgSrc => "img-src",
      CspDirective::FontSrc => "font-src",
      CspDirective::ConnectSrc => "connect-src",
      CspDirective::FrameSrc => "frame-src",
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CspSource {
  None,
  Self_,
  UnsafeInline,
  StrictDynamic,
  Nonce(String),
  Sha256(String),
  Any,
  Scheme(String),
  Host(CspHostSource),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CspHostSource {
  scheme: Option<String>,
  host: CspHostPattern,
  port: Option<CspPort>,
  path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CspPort {
  Any,
  Exact(u16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CspHostPattern {
  Exact(String),
  SubdomainWildcard(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CspDirectiveSet {
  directives: FxHashMap<CspDirective, Vec<CspSource>>,
}

/// A CSP policy may contain multiple directive sets (multiple header values or `<meta>` tags).
///
/// When more than one set is present, *all* of them must allow a resource for it to load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CspPolicy {
  policies: Vec<CspDirectiveSet>,
}

impl CspPolicy {
  /// Parse a list of CSP header/meta values into a policy. Each value becomes a separate policy set.
  pub fn from_values<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<Self> {
    let mut policies = Vec::new();
    for value in values {
      if let Some(set) = parse_directive_set(value) {
        policies.push(set);
      }
    }
    (!policies.is_empty()).then_some(Self { policies })
  }

  /// Extract a CSP policy from HTTP response headers.
  pub fn from_response_headers(resource: &FetchedResource) -> Option<Self> {
    let values = resource.header_values("Content-Security-Policy");
    Self::from_values(values)
  }

  /// True if this policy is effectively empty.
  pub fn is_empty(&self) -> bool {
    self.policies.is_empty()
  }

  /// Extend this policy with additional directive sets, de-duplicating identical sets.
  ///
  /// Returns `true` when new sets were added.
  pub fn extend(&mut self, other: Self) -> bool {
    let mut changed = false;
    for set in other.policies {
      if !self.policies.contains(&set) {
        self.policies.push(set);
        changed = true;
      }
    }
    changed
  }

  pub fn allows_url(
    &self,
    directive: CspDirective,
    document_origin: Option<&DocumentOrigin>,
    url: &Url,
  ) -> bool {
    // Multiple policies combine by intersection: the resource must be allowed by *every* policy.
    self
      .policies
      .iter()
      .all(|set| set_allows_url(set, directive, document_origin, url))
  }

  /// True if this policy allows an inline classic `<script>` element to execute.
  ///
  /// This is a narrow subset of CSP inline semantics:
  /// - If no `script-src`/`default-src` is present: allow.
  /// - If a matching nonce (`'nonce-…'` + `nonce=...`) is present: allow.
  /// - If a matching SHA-256 hash (`'sha256-…'`) is present: allow.
  /// - If `'unsafe-inline'` is present *and* no nonce/hash source is present: allow.
  ///
  /// Everything else is treated as blocked.
  pub fn allows_inline_script(&self, nonce: Option<&str>, source_text: &str) -> bool {
    // Multiple policies combine by intersection: the resource must be allowed by *every* policy.
    self
      .policies
      .iter()
      .all(|set| set_allows_inline_script(set, nonce, source_text))
  }

  /// True if this policy allows a classic external `<script src=...>` element to load/execute.
  ///
  /// In addition to URL matching, this checks nonce-based allowlisting when the element has
  /// `nonce=...` and the policy contains a matching `'nonce-…'` source.
  pub fn allows_script_url(
    &self,
    document_origin: Option<&DocumentOrigin>,
    nonce: Option<&str>,
    url: &Url,
  ) -> bool {
    self.policies.iter().all(|set| {
      // For `<script>` elements, `script-src-elem` takes precedence over `script-src` (and both fall
      // back to `default-src`).
      //
      // If neither `script-src-elem`, `script-src`, nor `default-src` is present, this policy set
      // does not restrict scripts.
      let Some(list) = directive_sources_for_set(set, CspDirective::ScriptSrcElem) else {
        return true;
      };

      if list.is_empty() {
        return false;
      }

      if let Some(nonce) = nonce.and_then(non_empty_trimmed_ascii) {
        if list.iter().any(|s| matches!(s, CspSource::Nonce(n) if n == nonce)) {
          return true;
        }
      }

      set_allows_url(set, CspDirective::ScriptSrcElem, document_origin, url)
    })
  }
}

fn directive_sources_for_set(set: &CspDirectiveSet, directive: CspDirective) -> Option<&Vec<CspSource>> {
  match directive {
    CspDirective::ScriptSrcElem => set
      .directives
      .get(&CspDirective::ScriptSrcElem)
      .or_else(|| set.directives.get(&CspDirective::ScriptSrc))
      .or_else(|| set.directives.get(&CspDirective::DefaultSrc)),
    CspDirective::ScriptSrcAttr => set
      .directives
      .get(&CspDirective::ScriptSrcAttr)
      .or_else(|| set.directives.get(&CspDirective::ScriptSrc))
      .or_else(|| set.directives.get(&CspDirective::DefaultSrc)),
    CspDirective::ScriptSrc => set
      .directives
      .get(&CspDirective::ScriptSrc)
      .or_else(|| set.directives.get(&CspDirective::DefaultSrc)),
    _ => set
      .directives
      .get(&directive)
      .or_else(|| set.directives.get(&CspDirective::DefaultSrc)),
  }
}

fn set_allows_url(
  set: &CspDirectiveSet,
  directive: CspDirective,
  document_origin: Option<&DocumentOrigin>,
  url: &Url,
) -> bool {
  let list = directive_sources_for_set(set, directive);
  let Some(list) = list else {
    // No directive and no default-src => allow.
    return true;
  };
  // Directive present but empty (or only non-URL tokens we ignore) => deny all external loads.
  if list.is_empty() {
    return false;
  }

  // CSP3: `strict-dynamic` disables host/scheme-source allowlisting when a nonce/hash is present.
  // We do not model trust propagation here; treat it conservatively as "URL sources do not match".
  if matches!(
    directive,
    CspDirective::ScriptSrc | CspDirective::ScriptSrcElem | CspDirective::ScriptSrcAttr
  )
    && list.iter().any(|s| matches!(s, CspSource::StrictDynamic))
    && list
      .iter()
      .any(|s| matches!(s, CspSource::Nonce(_) | CspSource::Sha256(_)))
  {
    return false;
  }

  let url_str = url.as_str();
  for source in list {
    match source {
      CspSource::None => {}
      CspSource::Self_ => {
        let Some(doc_origin) = document_origin else {
          continue;
        };
        let Some(target_origin) = origin_from_url(url_str) else {
          continue;
        };
        if &target_origin == doc_origin {
          return true;
        }
      }
      CspSource::Any => {
        // Follow common browser behavior: `*` matches network schemes, but not `data:`.
        if matches!(url.scheme(), "http" | "https") {
          return true;
        }
      }
      CspSource::UnsafeInline | CspSource::StrictDynamic | CspSource::Nonce(_) | CspSource::Sha256(_) => {
        // Not applicable to URL-based checks.
      }
      CspSource::Scheme(scheme) => {
        if url.scheme().eq_ignore_ascii_case(scheme) {
          return true;
        }
      }
      CspSource::Host(host_source) => {
        if let Some(scheme) = host_source.scheme.as_deref() {
          if !url.scheme().eq_ignore_ascii_case(scheme) {
            continue;
          }
        }
        if let Some(expected_port) = host_source.port {
          let port = if matches!(url.scheme(), "http" | "https") {
            url.port_or_known_default()
          } else {
            url.port()
          };
          match expected_port {
            CspPort::Any => {
              if port.is_none() {
                continue;
              }
            }
            CspPort::Exact(expected) => {
              if port != Some(expected) {
                continue;
              }
            }
          }
        }
        let Some(host) = url.host_str() else {
          continue;
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let host_matches = match &host_source.host {
          CspHostPattern::Exact(expected) => {
            host == *expected
          }
          CspHostPattern::SubdomainWildcard(base) => {
            let suffix = format!(".{base}");
            host.ends_with(&suffix) && host != *base
          }
        };
        if !host_matches {
          continue;
        }
        if let Some(source_path) = host_source.path.as_deref() {
          let url_path = url.path();
          if source_path.ends_with('/') {
            if !url_path.starts_with(source_path) {
              continue;
            }
          } else if url_path != source_path {
            continue;
          }
        }
        return true;
      }
    }
  }
  false
}

fn parse_directive_set(value: &str) -> Option<CspDirectiveSet> {
  let mut directives: FxHashMap<CspDirective, Vec<CspSource>> = FxHashMap::default();
  for raw in value.split(';') {
    let raw = trim_ascii_whitespace(raw);
    if raw.is_empty() {
      continue;
    }
    let mut parts = raw.split_ascii_whitespace();
    let Some(name) = parts.next() else {
      continue;
    };
    // Unknown directives are ignored per CSP: a policy may contain many directives we do not model
    // (e.g. `base-uri`, `object-src`, `upgrade-insecure-requests`).
    //
    // Returning `None` here would discard the entire directive set and effectively disable CSP,
    // which is both non-spec and surprising.
    let Some(directive) = match_lower_directive(name) else {
      continue;
    };
    // CSP: when a directive appears multiple times, only the first occurrence is used.
    if directives.contains_key(&directive) {
      continue;
    }
    let mut sources = Vec::new();
    for token in parts {
      if token.eq_ignore_ascii_case("'self'") {
        sources.push(CspSource::Self_);
        continue;
      }
      if token.eq_ignore_ascii_case("'none'") {
        sources.push(CspSource::None);
        continue;
      }
      if token.eq_ignore_ascii_case("'unsafe-inline'") {
        sources.push(CspSource::UnsafeInline);
        continue;
      }
      if token.eq_ignore_ascii_case("'strict-dynamic'") {
        sources.push(CspSource::StrictDynamic);
        continue;
      }
      if let Some(nonce) = parse_nonce_source(token) {
        sources.push(CspSource::Nonce(nonce));
        continue;
      }
      if let Some(hash) = parse_sha256_source(token) {
        sources.push(CspSource::Sha256(hash));
        continue;
      }
      if token == "*" {
        sources.push(CspSource::Any);
        continue;
      }
      if let Some(scheme) = parse_scheme_source(token) {
        sources.push(CspSource::Scheme(scheme));
        continue;
      }
      if let Some(host) = parse_host_source(token) {
        sources.push(CspSource::Host(host));
        continue;
      }
      // Ignore: other nonces/hashes, unsafe-eval, etc.
    }
    directives.insert(directive, sources);
  }

  (!directives.is_empty()).then_some(CspDirectiveSet { directives })
}

fn match_lower_directive(name: &str) -> Option<CspDirective> {
  if name.eq_ignore_ascii_case("default-src") {
    Some(CspDirective::DefaultSrc)
  } else if name.eq_ignore_ascii_case("script-src") {
    Some(CspDirective::ScriptSrc)
  } else if name.eq_ignore_ascii_case("script-src-elem") {
    Some(CspDirective::ScriptSrcElem)
  } else if name.eq_ignore_ascii_case("script-src-attr") {
    Some(CspDirective::ScriptSrcAttr)
  } else if name.eq_ignore_ascii_case("style-src") {
    Some(CspDirective::StyleSrc)
  } else if name.eq_ignore_ascii_case("img-src") {
    Some(CspDirective::ImgSrc)
  } else if name.eq_ignore_ascii_case("font-src") {
    Some(CspDirective::FontSrc)
  } else if name.eq_ignore_ascii_case("connect-src") {
    Some(CspDirective::ConnectSrc)
  } else if name.eq_ignore_ascii_case("frame-src") {
    Some(CspDirective::FrameSrc)
  } else {
    None
  }
}

fn parse_scheme_source(token: &str) -> Option<String> {
  let token = token.trim();
  let scheme = token.strip_suffix(':')?;
  if scheme.is_empty() {
    return None;
  }
  // Scheme must start with an ASCII letter.
  let mut chars = scheme.chars();
  let Some(first) = chars.next() else {
    return None;
  };
  if !first.is_ascii_alphabetic() {
    return None;
  }
  if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
    return None;
  }
  Some(scheme.to_ascii_lowercase())
}

fn parse_quoted_value(token: &str) -> Option<&str> {
  let token = trim_ascii_whitespace(token);
  if !token.starts_with('\'') || !token.ends_with('\'') || token.len() < 2 {
    return None;
  }
  Some(&token[1..token.len() - 1])
}

fn parse_nonce_source(token: &str) -> Option<String> {
  let inner = parse_quoted_value(token)?;
  const PREFIX: &str = "nonce-";
  if inner.len() <= PREFIX.len() || !inner[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
    return None;
  }
  Some(inner[PREFIX.len()..].to_string())
}

fn parse_sha256_source(token: &str) -> Option<String> {
  let inner = parse_quoted_value(token)?;
  const PREFIX: &str = "sha256-";
  if inner.len() <= PREFIX.len() || !inner[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
    return None;
  }
  Some(inner[PREFIX.len()..].to_string())
}

fn non_empty_trimmed_ascii(value: &str) -> Option<&str> {
  let trimmed = trim_ascii_whitespace(value);
  (!trimmed.is_empty()).then_some(trimmed)
}

fn set_allows_inline_script(set: &CspDirectiveSet, nonce: Option<&str>, source_text: &str) -> bool {
  // Inline `<script>` elements use `script-src-elem` (fallback: `script-src` → `default-src`).
  let list = directive_sources_for_set(set, CspDirective::ScriptSrcElem);
  let Some(list) = list else {
    // No directive and no default-src => allow.
    return true;
  };
  if list.is_empty() {
    return false;
  }

  let nonce = nonce.and_then(non_empty_trimmed_ascii);

  let has_nonce_or_hash = list
    .iter()
    .any(|s| matches!(s, CspSource::Nonce(_) | CspSource::Sha256(_)));

  if let Some(nonce) = nonce {
    if list.iter().any(|s| matches!(s, CspSource::Nonce(n) if n == nonce)) {
      return true;
    }
  }

  // Compute SHA-256 once, but only if needed.
  let mut computed_sha256: Option<String> = None;
  if list.iter().any(|s| matches!(s, CspSource::Sha256(_))) {
    use base64::{engine::general_purpose, Engine as _};
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(source_text.as_bytes());
    computed_sha256 = Some(general_purpose::STANDARD.encode(digest));
  }

  if let Some(computed) = computed_sha256.as_deref() {
    let computed_trimmed = computed.trim_end_matches('=');
    for source in list {
      let CspSource::Sha256(expected) = source else {
        continue;
      };
      // Be tolerant of optional base64 padding.
      if expected.trim_end_matches('=') == computed_trimmed {
        return true;
      }
    }
  }

  // `unsafe-inline` is ignored when a nonce/hash is present (modern browsers).
  if !has_nonce_or_hash && list.iter().any(|s| matches!(s, CspSource::UnsafeInline)) {
    return true;
  }

  false
}

fn parse_host_source(token: &str) -> Option<CspHostSource> {
  let token = trim_ascii_whitespace(token);
  if token.is_empty() || token.starts_with('\'') {
    return None;
  }

  // If the token contains a wildcard, the `url` parser won't accept it; parse manually.
  if token.contains('*') {
    return parse_host_source_manual(token);
  }

  if token.contains("://") {
    if let Ok(parsed) = Url::parse(token) {
      if let Some(host) = parsed.host_str() {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let path = normalize_csp_source_path(parsed.path());
        return Some(CspHostSource {
          scheme: Some(parsed.scheme().to_ascii_lowercase()),
          host: CspHostPattern::Exact(host),
          port: parsed.port().map(CspPort::Exact),
          path,
        });
      }
    }
    // Fall through to manual parsing if `Url::parse` rejects it.
  }

  parse_host_source_manual(token)
}

fn parse_host_source_manual(token: &str) -> Option<CspHostSource> {
  let (scheme, rest) = match token.find("://") {
    Some(pos) => (
      Some(token[..pos].to_ascii_lowercase()),
      &token[pos + 3..],
    ),
    None => (None, token),
  };
  let host_port_end = rest
    .find(|c: char| matches!(c, '/' | '?' | '#'))
    .unwrap_or(rest.len());
  let host_port = rest.get(..host_port_end).unwrap_or(rest);
  if host_port.is_empty() {
    return None;
  }
  let path = rest
    .get(host_port_end..)
    .and_then(|tail| tail.strip_prefix('/'))
    .map(|tail| format!("/{tail}"))
    .map(|path| path.split(|c| matches!(c, '?' | '#')).next().unwrap_or("").to_string())
    .and_then(|path| normalize_csp_source_path(&path));

  let (host, port) = if host_port.starts_with('[') {
    let end = host_port.find(']')?;
    let host = &host_port[1..end];
    let after = &host_port[end + 1..];
    let port = if after.is_empty() {
      None
    } else if let Some(port) = after.strip_prefix(':') {
      if port.is_empty() {
        return None;
      }
      if port == "*" {
        Some(CspPort::Any)
      } else if port.chars().all(|c| c.is_ascii_digit()) {
        Some(CspPort::Exact(port.parse::<u16>().ok()?))
      } else {
        return None;
      }
    } else {
      return None;
    };
    (host, port)
  } else if let Some((host, port)) = host_port.rsplit_once(':') {
    if port == "*" {
      (host, Some(CspPort::Any))
    } else if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
      (host, Some(CspPort::Exact(port.parse::<u16>().ok()?)))
    } else {
      return None;
    }
  } else {
    (host_port, None)
  };

  let host = host.trim_end_matches('.').to_ascii_lowercase();
  if host.is_empty() {
    return None;
  }

  let pattern = if let Some(stripped) = host.strip_prefix("*.") {
    if stripped.is_empty() {
      return None;
    }
    CspHostPattern::SubdomainWildcard(stripped.to_string())
  } else {
    CspHostPattern::Exact(host)
  };

  Some(CspHostSource {
    scheme,
    host: pattern,
    port,
    path,
  })
}

fn normalize_csp_source_path(path: &str) -> Option<String> {
  let path = path.trim();
  if path.is_empty() || path == "/" {
    return None;
  }
  if !path.starts_with('/') {
    return None;
  }
  Some(path.to_string())
}

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

      if i + 1 < bytes.len()
        && bytes[i] == b'\\'
        && (bytes[i + 1] == b'"' || bytes[i + 1] == b'\'')
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

/// Extract CSP from an HTML string by scanning only the document head.
///
/// This is a bounded, best-effort scan used by helpers that want to avoid a full DOM parse.
pub fn extract_csp_from_html(html: &str) -> Option<CspPolicy> {
  let html = scan_html_prefix(html, MAX_CSP_SCAN_BYTES);
  let bytes = html.as_bytes();
  let mut template_depth: usize = 0;
  let mut i: usize = 0;
  let mut policies: Vec<CspDirectiveSet> = Vec::new();

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
        template_depth = template_depth.saturating_sub(1);
      } else {
        template_depth += 1;
      }
    }

    if template_depth == 0 && !is_end && name.eq_ignore_ascii_case(b"body") {
      break;
    }
    if template_depth == 0 && is_end && name.eq_ignore_ascii_case(b"head") {
      break;
    }

    if template_depth == 0 && !is_end && name.eq_ignore_ascii_case(b"meta") {
      let tag = &html[tag_start..tag_end];
      let mut http_equiv: Option<&str> = None;
      let mut content: Option<&str> = None;
      for_each_attribute(tag, |attr, value| {
        if attr.eq_ignore_ascii_case("http-equiv") {
          http_equiv = Some(value);
        } else if attr.eq_ignore_ascii_case("content") {
          content = Some(value);
        }
        ControlFlow::Continue(())
      });

      if http_equiv
        .map(|v| v.eq_ignore_ascii_case("content-security-policy"))
        .unwrap_or(false)
      {
        if let Some(content) = content {
          if let Some(set) = parse_directive_set(content) {
            policies.push(set);
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

  (!policies.is_empty()).then_some(CspPolicy { policies })
}

/// Extract CSP from `<meta http-equiv="Content-Security-Policy">` within the document head.
pub fn extract_csp(dom: &DomNode) -> Option<CspPolicy> {
  extract_csp_impl(dom, None).ok().flatten()
}

pub(crate) fn extract_csp_with_deadline(dom: &DomNode) -> Result<Option<CspPolicy>> {
  let mut counter = 0usize;
  extract_csp_impl(dom, Some(&mut counter))
}

fn extract_csp_impl(
  dom: &DomNode,
  mut deadline_counter: Option<&mut usize>,
) -> Result<Option<CspPolicy>> {
  let mut stack = vec![dom];
  let mut head: Option<&DomNode> = None;

  while let Some(node) = stack.pop() {
    if let Some(counter) = deadline_counter.as_deref_mut() {
      check_active_periodic(counter, CSP_DEADLINE_STRIDE, RenderStage::DomParse).map_err(Error::Render)?;
    }

    if let DomNodeType::ShadowRoot { .. } = node.node_type {
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
      head = Some(node);
      break;
    }

    for child in node.traversal_children().iter().rev() {
      stack.push(child);
    }
  }

  let Some(head) = head else {
    return Ok(None);
  };

  let mut policies: Vec<CspDirectiveSet> = Vec::new();
  let mut stack: Vec<(&DomNode, bool)> = vec![(head, false)];

  while let Some((node, in_foreign_namespace)) = stack.pop() {
    if let Some(counter) = deadline_counter.as_deref_mut() {
      check_active_periodic(counter, CSP_DEADLINE_STRIDE, RenderStage::DomParse).map_err(Error::Render)?;
    }

    let next_in_foreign_namespace = in_foreign_namespace
      || matches!(
        node.namespace(),
        Some(ns) if !(ns.is_empty() || ns == HTML_NAMESPACE)
      );

    if !in_foreign_namespace
      && node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("meta"))
      && matches!(node.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE)
    {
      let http_equiv = node.get_attribute_ref("http-equiv");
      let content = node.get_attribute_ref("content");

      if http_equiv
        .map(|v| trim_ascii_whitespace(v).eq_ignore_ascii_case("content-security-policy"))
        .unwrap_or(false)
      {
        if let Some(content) = content {
          if let Some(set) = parse_directive_set(content) {
            policies.push(set);
          }
        }
      }
    }

    let skip_children = matches!(node.node_type, DomNodeType::ShadowRoot { .. })
      || node.is_template_element()
      || next_in_foreign_namespace;
    if skip_children {
      continue;
    }

    for child in node.traversal_children().iter().rev() {
      stack.push((child, next_in_foreign_namespace));
    }
  }

  Ok((!policies.is_empty()).then_some(CspPolicy { policies }))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn doc_origin(url: &str) -> DocumentOrigin {
    origin_from_url(url).expect("origin")
  }

  fn allows(policy: &CspPolicy, directive: CspDirective, doc: &str, url: &str) -> bool {
    let url = Url::parse(url).expect("url");
    policy.allows_url(directive, Some(&doc_origin(doc)), &url)
  }

  #[test]
  fn none_blocks() {
    let policy = CspPolicy::from_values(["img-src 'none'"]).expect("parse");
    assert!(!allows(&policy, CspDirective::ImgSrc, "https://example.com/", "https://example.com/a.png"));
  }

  #[test]
  fn self_allows_same_origin_only() {
    let policy = CspPolicy::from_values(["default-src 'self'"]).expect("parse");
    assert!(allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://example.com/a.png"
    ));
    assert!(!allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://other.com/a.png"
    ));
  }

  #[test]
  fn img_src_data_allows_data_but_blocks_https() {
    let policy = CspPolicy::from_values(["img-src data:"]).expect("parse");
    let data_url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB";
    assert!(allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      data_url
    ));
    assert!(!allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://example.com/a.png"
    ));
  }

  #[test]
  fn wildcard_subdomain_matching() {
    let policy = CspPolicy::from_values(["img-src *.example.com"]).expect("parse");
    assert!(allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://cdn.example.com/a.png"
    ));
    assert!(!allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://example.com/a.png"
    ));
  }

  #[test]
  fn host_source_explicit_port_with_scheme_requires_port_match() {
    let policy = CspPolicy::from_values(["img-src https://example.com:8443"]).expect("parse");
    assert!(allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://example.com:8443/a.png"
    ));
    assert!(!allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://example.com/a.png"
    ));
  }

  #[test]
  fn host_source_explicit_port_without_scheme_requires_port_match() {
    let policy = CspPolicy::from_values(["img-src example.com:8443"]).expect("parse");
    assert!(allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://example.com:8443/a.png"
    ));
    assert!(!allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://example.com/a.png"
    ));
  }

  #[test]
  fn host_source_ipv6_literal_with_port() {
    let policy = CspPolicy::from_values(["img-src http://[::1]:8080"]).expect("parse");
    assert!(allows(
      &policy,
      CspDirective::ImgSrc,
      "http://[::1]/",
      "http://[::1]:8080/a.png"
    ));
    assert!(!allows(
      &policy,
      CspDirective::ImgSrc,
      "http://[::1]/",
      "http://[::1]/a.png"
    ));
  }

  #[test]
  fn host_source_path_prefix_matching() {
    let policy = CspPolicy::from_values(["img-src https://example.com/images/"]).expect("parse");
    assert!(allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://example.com/images/a.png"
    ));
    assert!(!allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://example.com/other/a.png"
    ));
  }

  #[test]
  fn host_source_path_exact_matching() {
    let policy = CspPolicy::from_values(["img-src https://example.com/images/logo.png"]).expect("parse");
    assert!(allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://example.com/images/logo.png"
    ));
    assert!(!allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://example.com/images/other.png"
    ));
  }

  #[test]
  fn unknown_directives_do_not_discard_entire_policy() {
    // Real-world CSP policies often include many directives we do not model; we should still parse
    // and enforce the subset we understand.
    let policy = CspPolicy::from_values(["default-src 'none'; object-src 'none'; img-src https:"])
      .expect("parse");
    assert!(allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://example.com/a.png"
    ));
  }

  #[test]
  fn duplicate_directives_ignore_subsequent_occurrences() {
    // CSP: When a directive appears multiple times within a policy, only the first is used.
    let policy = CspPolicy::from_values(["img-src 'none'; img-src https:"]).expect("parse");
    assert!(!allows(
      &policy,
      CspDirective::ImgSrc,
      "https://example.com/",
      "https://example.com/a.png"
    ));
  }

  #[test]
  fn script_src_elem_overrides_script_src_for_script_elements() {
    // `script-src-elem` takes precedence over `script-src` regardless of order.
    let policy = CspPolicy::from_values(["script-src-elem https:; script-src 'none'"]).expect("parse");
    let url = Url::parse("https://example.com/a.js").expect("url");
    assert!(policy.allows_script_url(Some(&doc_origin("https://example.com/")), None, &url));
  }
}
