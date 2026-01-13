//! Minimal Content Security Policy (CSP) parsing and matching used by the browser process.
//!
//! This crate intentionally keeps the renderer-facing IPC protocol small and safe to validate. CSP
//! enforcement for out-of-process iframes is a browser-side responsibility: the browser must apply
//! the parent document's `frame-src` restrictions when deciding whether to create/navigate a child
//! frame.
//!
//! The in-process FastRender renderer already has CSP enforcement. When iframe navigations move out
//! of process, we need a lightweight browser-side representation to preserve the same behavior.

use crate::{DocumentOrigin, FrameId};
use std::collections::HashMap;
use url::Url;

/// CSP directives supported by the multiprocess browser prototype.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CspDirective {
  DefaultSrc,
  FrameSrc,
}

impl CspDirective {
  pub const fn as_str(self) -> &'static str {
    match self {
      CspDirective::DefaultSrc => "default-src",
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
  directives: HashMap<CspDirective, Vec<CspSource>>,
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

  pub fn is_empty(&self) -> bool {
    self.policies.is_empty()
  }

  pub fn allows_url(
    &self,
    directive: CspDirective,
    document_origin: Option<&DocumentOrigin>,
    url: &Url,
  ) -> bool {
    self
      .policies
      .iter()
      .all(|set| set_allows_url(set, directive, document_origin, url))
  }
}

fn directive_sources_for_set(
  set: &CspDirectiveSet,
  directive: CspDirective,
) -> Option<&Vec<CspSource>> {
  match directive {
    CspDirective::FrameSrc => set
      .directives
      .get(&CspDirective::FrameSrc)
      .or_else(|| set.directives.get(&CspDirective::DefaultSrc)),
    CspDirective::DefaultSrc => set.directives.get(&CspDirective::DefaultSrc),
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

  let url_str = url.as_str();
  for source in list {
    match source {
      CspSource::None => {}
      CspSource::Self_ => {
        let Some(doc_origin) = document_origin else {
          continue;
        };
        let Some(target_origin) = DocumentOrigin::from_url_str(url_str) else {
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
          CspHostPattern::Exact(expected) => host == *expected,
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
  let mut directives: HashMap<CspDirective, Vec<CspSource>> = HashMap::new();

  for raw in value.split(';') {
    let raw = trim_ascii_whitespace(raw);
    if raw.is_empty() {
      continue;
    }

    let mut parts = raw.split_ascii_whitespace();
    let Some(name) = parts.next() else {
      continue;
    };
    // Unknown directives are ignored per CSP.
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
      // Ignore: nonces/hashes/unsafe-inline/etc. (not relevant for URL-based frame-src checks).
    }

    directives.insert(directive, sources);
  }

  (!directives.is_empty()).then_some(CspDirectiveSet { directives })
}

fn match_lower_directive(name: &str) -> Option<CspDirective> {
  if name.eq_ignore_ascii_case("default-src") {
    Some(CspDirective::DefaultSrc)
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
    Some(pos) => (Some(token[..pos].to_ascii_lowercase()), &token[pos + 3..]),
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
    .map(|path| {
      path
        .split(|c| matches!(c, '?' | '#'))
        .next()
        .unwrap_or("")
        .to_string()
    })
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

/// Browser-side per-frame state for CSP enforcement.
#[derive(Debug, Clone)]
pub struct FrameNode {
  pub id: FrameId,
  pub url: Option<String>,
  pub origin: Option<DocumentOrigin>,
  pub csp: Option<CspPolicy>,
}

impl FrameNode {
  pub fn new(id: FrameId) -> Self {
    Self {
      id,
      url: None,
      origin: None,
      csp: None,
    }
  }

  /// Update the frame with the committed URL and raw CSP header/meta values.
  pub fn navigation_committed(&mut self, url: String, csp_values: Vec<String>) {
    self.origin = DocumentOrigin::from_url_str(&url);
    self.url = Some(url);
    self.csp = CspPolicy::from_values(csp_values.iter().map(|s| s.as_str()));
  }

  /// Check whether a child-frame navigation is allowed by the parent CSP (`frame-src`).
  ///
  /// Returns the resolved child URL on success. On failure, returns a diagnostic string matching the
  /// in-process renderer's CSP violation format.
  pub fn check_frame_src(&self, candidate: &str) -> Result<Url, String> {
    check_frame_src(
      self.csp.as_ref(),
      self.url.as_deref(),
      self.origin.as_ref(),
      candidate,
    )
  }
}

/// Evaluate CSP `frame-src` for `candidate` in the context of a parent document.
pub fn check_frame_src(
  csp: Option<&CspPolicy>,
  document_url: Option<&str>,
  document_origin: Option<&DocumentOrigin>,
  candidate: &str,
) -> Result<Url, String> {
  let Some(csp) = csp else {
    return resolve_for_csp(candidate, document_url, document_origin).ok_or_else(|| {
      format!(
        "Blocked by Content-Security-Policy ({}) for requested URL (invalid URL): {candidate}",
        CspDirective::FrameSrc.as_str()
      )
    });
  };

  let parsed = resolve_for_csp(candidate, document_url, document_origin).ok_or_else(|| {
    format!(
      "Blocked by Content-Security-Policy ({}) for requested URL (invalid URL): {candidate}",
      CspDirective::FrameSrc.as_str()
    )
  })?;

  let resolved = parsed.as_str();
  if csp.allows_url(CspDirective::FrameSrc, document_origin, &parsed) {
    Ok(parsed)
  } else {
    Err(format!(
      "Blocked by Content-Security-Policy ({}) for requested URL: {resolved}",
      CspDirective::FrameSrc.as_str()
    ))
  }
}

fn resolve_for_csp(
  candidate: &str,
  document_url: Option<&str>,
  document_origin: Option<&DocumentOrigin>,
) -> Option<Url> {
  Url::parse(candidate)
    .ok()
    .or_else(|| {
      document_url
        .and_then(|base| Url::parse(base).ok())
        .and_then(|base| base.join(candidate).ok())
    })
    .or_else(|| {
      document_origin
        .filter(|origin| origin.scheme.eq_ignore_ascii_case("file") || origin.host.is_some())
        .and_then(|origin| Url::parse(&format!("{origin}/")).ok())
        .and_then(|base| base.join(candidate).ok())
    })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn frame_src_none_blocks_child_navigation() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed(
      "https://example.com/".to_string(),
      vec!["frame-src 'none'".to_string()],
    );

    let err = frame
      .check_frame_src("https://evil.example/child")
      .expect_err("expected CSP to block frame-src navigation");
    assert_eq!(
      err,
      "Blocked by Content-Security-Policy (frame-src) for requested URL: https://evil.example/child"
    );
  }
}

