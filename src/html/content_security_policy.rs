//! Minimal Content Security Policy (CSP) parsing and matching.
//!
//! FastRender historically enforced a small set of `ResourceAccessPolicy` knobs (same origin,
//! mixed content, etc). Many real pages additionally gate resource loading via Content Security
//! Policy. This module implements a deliberately small-but-useful subset:
//!
//! - Directives: `default-src`, `style-src`, `img-src`, `font-src`, `connect-src`, `frame-src`
//! - Source expressions:
//!   - `'self'`, `'none'`
//!   - `*`
//!   - scheme sources like `https:`, `http:`, `data:` (and other valid `scheme:` tokens)
//!   - host sources like `example.com`, `https://example.com`, `*.example.com`
//!
//! Notes / intentionally omitted (v1):
//! - Path matching in host sources.
//! - Ports and IPv6 literal parsing for schemeless host sources.
//! - Nonce/hash/`'unsafe-inline'` semantics (they still effectively restrict external loads because
//!   they don't match any URL, so a directive that only contains those tokens becomes "deny all"
//!   for our external resource destinations).

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
  Any,
  Scheme(String),
  Host(CspHostSource),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CspHostSource {
  scheme: Option<String>,
  host: CspHostPattern,
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
}

fn set_allows_url(
  set: &CspDirectiveSet,
  directive: CspDirective,
  document_origin: Option<&DocumentOrigin>,
  url: &Url,
) -> bool {
  let list = set
    .directives
    .get(&directive)
    .or_else(|| set.directives.get(&CspDirective::DefaultSrc));
  let Some(list) = list else {
    // No directive and no default-src => allow.
    return true;
  };
  // Directive present but empty (or only non-URL tokens we ignore) => deny all external loads.
  if list.is_empty() {
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
        let Some(host) = url.host_str() else {
          continue;
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        match &host_source.host {
          CspHostPattern::Exact(expected) => {
            if host == *expected {
              return true;
            }
          }
          CspHostPattern::SubdomainWildcard(base) => {
            let suffix = format!(".{base}");
            if host.ends_with(&suffix) && host != *base {
              return true;
            }
          }
        }
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
    let directive = match_lower_directive(name)?;
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
      // Ignore: nonces, hashes, unsafe-inline/eval, etc.
    }
    directives.insert(directive, sources);
  }

  (!directives.is_empty()).then_some(CspDirectiveSet { directives })
}

fn match_lower_directive(name: &str) -> Option<CspDirective> {
  if name.eq_ignore_ascii_case("default-src") {
    Some(CspDirective::DefaultSrc)
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
        return Some(CspHostSource {
          scheme: Some(parsed.scheme().to_ascii_lowercase()),
          host: CspHostPattern::Exact(host),
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
  let host_port = rest
    .split(|c: char| matches!(c, '/' | '?' | '#'))
    .next()
    .unwrap_or(rest);
  if host_port.is_empty() {
    return None;
  }
  // Strip port if present (best-effort; does not handle IPv6 literals).
  let host = host_port
    .rsplit_once(':')
    .map(|(host, port)| {
      // Only strip when the port looks like digits.
      if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
        host
      } else {
        host_port
      }
    })
    .unwrap_or(host_port);

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

  Some(CspHostSource { scheme, host: pattern })
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
}

