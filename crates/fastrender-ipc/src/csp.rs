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
  /// Effective base URL for resolving relative URLs in this document.
  ///
  /// This should reflect the document's `<base href>` when present; otherwise it defaults to the
  /// committed document URL.
  pub base_url: Option<String>,
  /// Mirror of the in-process `ResourceAccessPolicy::allow_file_from_http` knob for document loads.
  ///
  /// When `false` (default), `file://` navigations are blocked for HTTP(S) embedding documents.
  pub allow_file_from_http: bool,
  /// Mirror of the in-process `ResourceAccessPolicy::block_mixed_content` knob for document loads.
  ///
  /// When `true`, `http://` navigations are blocked for `https://` embedding documents.
  pub block_mixed_content: bool,
}

impl FrameNode {
  pub fn new(id: FrameId) -> Self {
    Self {
      id,
      url: None,
      origin: None,
      csp: None,
      base_url: None,
      allow_file_from_http: false,
      block_mixed_content: false,
    }
  }

  /// Update the frame with the committed URL and raw CSP header/meta values.
  pub fn navigation_committed(&mut self, url: String, csp_values: Vec<String>) {
    self.origin = DocumentOrigin::from_url_str(&url);
    self.base_url = Some(url.clone());
    self.url = Some(url);
    self.csp = CspPolicy::from_values(csp_values.iter().map(|s| s.as_str()));
  }

  /// Override the base URL used for resolving relative iframe URLs.
  pub fn set_base_url(&mut self, base_url: String) {
    self.base_url = Some(base_url);
  }

  /// Configure the subset of `ResourceAccessPolicy` needed to enforce embedder restrictions for
  /// out-of-process iframe navigations.
  ///
  /// These flags are typically inherited from the embedding document's `ResourceContext`.
  pub fn set_resource_policy(&mut self, allow_file_from_http: bool, block_mixed_content: bool) {
    self.allow_file_from_http = allow_file_from_http;
    self.block_mixed_content = block_mixed_content;
  }

  /// Check whether a child-frame navigation is allowed by the parent document policy
  /// (mixed-content + `file://` from HTTP(S)).
  ///
  /// This intentionally mirrors the in-process `ResourceAccessPolicy::allows_document` behaviour:
  /// it does **not** enforce same-origin restrictions.
  pub fn check_document_policy(&self, candidate: &str) -> Result<(), String> {
    check_document_policy(
      self.origin.as_ref(),
      self.base_url.as_deref().or(self.url.as_deref()),
      candidate,
      self.allow_file_from_http,
      self.block_mixed_content,
    )
  }

  /// Check whether a child-frame navigation is allowed by the parent document policy, validating
  /// both the requested URL and (when present) the final URL after redirects.
  pub fn check_document_policy_with_final(
    &self,
    requested_url: &str,
    final_url: Option<&str>,
  ) -> Result<(), String> {
    self.check_document_policy(requested_url)?;
    if let Some(final_url) = final_url {
      self.check_document_policy(final_url)?;
    }
    Ok(())
  }

  /// Check whether an out-of-process iframe navigation is allowed by the embedding document's
  /// policy and CSP.
  ///
  /// This is a convenience wrapper for browser-side enforcement that mirrors the in-process
  /// `ResourceContext::check_allowed(ResourceKind::Document, ...)` behavior:
  /// - document policy (mixed content / `file://` from HTTP(S)) is evaluated first
  /// - then CSP `frame-src` is evaluated
  pub fn check_iframe_navigation(&self, candidate: &str) -> Result<Url, String> {
    self.check_document_policy(candidate)?;
    self.check_frame_src(candidate)
  }

  /// Check whether an out-of-process iframe navigation is allowed by the embedding document's
  /// policy and CSP, taking redirects into account.
  ///
  /// This mirrors the in-process `ResourceContext::check_allowed_with_final` behavior for iframe
  /// loads: callers should run a pre-navigation check for the requested URL, and then a second
  /// check after the child process reports its committed/final URL.
  pub fn check_iframe_navigation_with_final(
    &self,
    requested_url: &str,
    final_url: Option<&str>,
  ) -> Result<Url, String> {
    self.check_document_policy_with_final(requested_url, final_url)?;
    self.check_frame_src_with_final(requested_url, final_url)
  }

  /// Check whether a child-frame navigation is allowed by the parent CSP (`frame-src`).
  ///
  /// Returns the resolved child URL on success. On failure, returns a diagnostic string matching the
  /// in-process renderer's CSP violation format.
  pub fn check_frame_src(&self, candidate: &str) -> Result<Url, String> {
    check_frame_src(
      self.csp.as_ref(),
      self.base_url.as_deref().or(self.url.as_deref()),
      self.origin.as_ref(),
      candidate,
    )
  }

  /// Check whether a child-frame navigation is allowed by the parent CSP (`frame-src`), taking
  /// redirects into account.
  ///
  /// This mirrors the in-process renderer's `ResourceContext::check_allowed_with_final` behavior:
  /// the requested URL is validated first, then (when present) the final URL after redirects is
  /// validated as well.
  pub fn check_frame_src_with_final(
    &self,
    requested_url: &str,
    final_url: Option<&str>,
  ) -> Result<Url, String> {
    check_frame_src_with_final(
      self.csp.as_ref(),
      self.base_url.as_deref().or(self.url.as_deref()),
      self.origin.as_ref(),
      requested_url,
      final_url,
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
  check_frame_src_labeled(csp, document_url, document_origin, candidate, "requested")
}

/// Evaluate CSP `frame-src` for both the requested URL and an optional final URL after redirects.
///
/// The returned [`Url`] is the resolved requested URL.
pub fn check_frame_src_with_final(
  csp: Option<&CspPolicy>,
  document_url: Option<&str>,
  document_origin: Option<&DocumentOrigin>,
  requested_url: &str,
  final_url: Option<&str>,
) -> Result<Url, String> {
  let resolved = check_frame_src_labeled(
    csp,
    document_url,
    document_origin,
    requested_url,
    "requested",
  )?;
  if let Some(final_url) = final_url {
    let _ = check_frame_src_labeled(
      csp,
      document_url,
      document_origin,
      final_url,
      "final",
    )?;
  }
  Ok(resolved)
}

fn check_frame_src_labeled(
  csp: Option<&CspPolicy>,
  document_url: Option<&str>,
  document_origin: Option<&DocumentOrigin>,
  candidate: &str,
  label: &str,
) -> Result<Url, String> {
  let Some(csp) = csp else {
    return resolve_for_csp(candidate, document_url, document_origin).ok_or_else(|| {
      let candidate_display = format_candidate_for_error(candidate);
      format!(
        "Blocked by Content-Security-Policy ({}) for {label} URL (invalid URL): {candidate}",
        CspDirective::FrameSrc.as_str(),
        candidate = candidate_display,
      )
    });
  };

  let parsed = resolve_for_csp(candidate, document_url, document_origin).ok_or_else(|| {
    let candidate_display = format_candidate_for_error(candidate);
    format!(
      "Blocked by Content-Security-Policy ({}) for {label} URL (invalid URL): {candidate}",
      CspDirective::FrameSrc.as_str(),
      candidate = candidate_display,
    )
  })?;

  let resolved = parsed.as_str();
  if csp.allows_url(CspDirective::FrameSrc, document_origin, &parsed) {
    Ok(parsed)
  } else {
    Err(format!(
      "Blocked by Content-Security-Policy ({}) for {label} URL: {resolved}",
      CspDirective::FrameSrc.as_str()
    ))
  }
}

fn is_http_like(origin: &DocumentOrigin) -> bool {
  matches!(origin.scheme.as_str(), "http" | "https")
}

fn is_secure_http(origin: &DocumentOrigin) -> bool {
  origin.scheme == "https"
}

/// Evaluate the embedder's document policy (mixed content + `file://` from HTTP(S)) for a candidate
/// child-frame navigation.
///
/// This is a minimal browser-side mirror of the in-process `ResourceAccessPolicy::allows_document`
/// logic. We keep this small because the multiprocess prototype treats the renderer as untrusted
/// and needs deterministic enforcement in the browser process.
pub fn check_document_policy(
  document_origin: Option<&DocumentOrigin>,
  document_url: Option<&str>,
  candidate: &str,
  allow_file_from_http: bool,
  block_mixed_content: bool,
) -> Result<(), String> {
  let Some(origin) = document_origin else {
    return Ok(());
  };

  // Follow in-process policy behavior: if the URL cannot be parsed/resolved, avoid over-blocking.
  let Some(parsed) = resolve_for_csp(candidate, document_url, document_origin) else {
    return Ok(());
  };
  let scheme = parsed.scheme().to_ascii_lowercase();

  if scheme == "data" {
    return Ok(());
  }

  if is_http_like(origin) && scheme == "file" && !allow_file_from_http {
    return Err("Blocked file:// resource from HTTP(S) document".to_string());
  }

  if is_secure_http(origin) && block_mixed_content && scheme == "http" {
    return Err("Blocked mixed HTTP content from HTTPS document".to_string());
  }

  Ok(())
}

fn resolve_for_csp(
  candidate: &str,
  document_url: Option<&str>,
  document_origin: Option<&DocumentOrigin>,
) -> Option<Url> {
  let candidate = trim_ascii_whitespace(candidate);
  if candidate.is_empty() {
    return None;
  }
  if candidate.len() > crate::MAX_UNTRUSTED_URL_BYTES {
    return None;
  }
  Url::parse(candidate)
    .ok()
    .or_else(|| {
      document_url
        .filter(|base| base.len() <= crate::MAX_UNTRUSTED_URL_BYTES)
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

fn format_candidate_for_error(candidate: &str) -> String {
  let candidate = trim_ascii_whitespace(candidate);
  if candidate.len() > crate::MAX_UNTRUSTED_URL_BYTES {
    format!("<too long: {} bytes>", candidate.len())
  } else {
    candidate.to_string()
  }
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

  #[test]
  fn frame_src_checks_final_url_after_redirect() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed(
      "https://parent.example/".to_string(),
      vec!["frame-src https://allowed.example".to_string()],
    );

    let err = frame
      .check_frame_src_with_final(
        "https://allowed.example/start",
        Some("https://blocked.example/landing"),
      )
      .expect_err("expected frame-src to reject final redirected URL");
    assert_eq!(
      err,
      "Blocked by Content-Security-Policy (frame-src) for final URL: https://blocked.example/landing"
    );
  }

  #[test]
  fn frame_src_self_allows_same_origin_and_blocks_cross_origin() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed(
      "https://example.com/".to_string(),
      vec!["frame-src 'self'".to_string()],
    );

    let allowed = frame
      .check_frame_src("https://example.com/child")
      .expect("expected frame-src 'self' to allow same-origin iframe");
    assert_eq!(allowed.as_str(), "https://example.com/child");

    let err = frame
      .check_frame_src("https://evil.example/child")
      .expect_err("expected frame-src 'self' to block cross-origin iframe");
    assert_eq!(
      err,
      "Blocked by Content-Security-Policy (frame-src) for requested URL: https://evil.example/child"
    );
  }

  #[test]
  fn default_src_applies_to_frame_src_when_frame_src_is_missing() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed(
      "https://example.com/".to_string(),
      vec!["default-src 'none'".to_string()],
    );

    let err = frame
      .check_frame_src("https://example.com/child")
      .expect_err("expected default-src 'none' to apply to frame-src when frame-src is missing");
    assert_eq!(
      err,
      "Blocked by Content-Security-Policy (frame-src) for requested URL: https://example.com/child"
    );
  }

  #[test]
  fn invalid_frame_src_candidate_reports_invalid_url_diagnostic() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed("https://example.com/".to_string(), Vec::new());

    let err = frame
      .check_frame_src("http://example.com:99999/")
      .expect_err("expected invalid URL to be rejected");
    assert_eq!(
      err,
      "Blocked by Content-Security-Policy (frame-src) for requested URL (invalid URL): http://example.com:99999/"
    );
  }

  #[test]
  fn overly_long_frame_src_candidate_reports_invalid_url_without_echoing_input() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed("https://example.com/".to_string(), Vec::new());

    let long = "a".repeat(crate::MAX_UNTRUSTED_URL_BYTES + 1);
    let err = frame
      .check_frame_src(&long)
      .expect_err("expected overly long URL to be rejected");

    let expected = format!(
      "Blocked by Content-Security-Policy (frame-src) for requested URL (invalid URL): <too long: {} bytes>",
      crate::MAX_UNTRUSTED_URL_BYTES + 1
    );
    assert_eq!(err, expected);
    assert!(
      err.len() < 200,
      "expected diagnostic to stay small even for huge inputs (len={}, err={err:?})",
      err.len()
    );
  }

  #[test]
  fn frame_src_scheme_source_allows_any_url_with_matching_scheme() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed(
      "https://parent.example/".to_string(),
      vec!["frame-src https:".to_string()],
    );

    let ok = frame
      .check_frame_src("https://evil.example/child")
      .expect("expected https: scheme source to allow https URL");
    assert_eq!(ok.as_str(), "https://evil.example/child");

    let err = frame
      .check_frame_src("http://evil.example/child")
      .expect_err("expected https: scheme source to block http URL");
    assert_eq!(
      err,
      "Blocked by Content-Security-Policy (frame-src) for requested URL: http://evil.example/child"
    );
  }

  #[test]
  fn frame_src_host_source_matches_wildcards_ports_and_paths() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed(
      "https://parent.example/".to_string(),
      vec!["frame-src https://*.example.com:8443/path/".to_string()],
    );

    let ok = frame
      .check_frame_src("https://a.example.com:8443/path/child")
      .expect("expected host source to allow matching wildcard+port+path");
    assert_eq!(ok.as_str(), "https://a.example.com:8443/path/child");

    frame
      .check_frame_src("https://example.com:8443/path/child")
      .expect_err("wildcard host source should not match the base domain itself");

    frame
      .check_frame_src("https://a.example.com:8443/other")
      .expect_err("host source with /path/ should require URL path prefix");

    frame
      .check_frame_src("http://a.example.com:8443/path/child")
      .expect_err("host source scheme should be enforced when specified");

    frame
      .check_frame_src("https://a.example.com:443/path/child")
      .expect_err("host source port should be enforced when specified");
  }

  #[test]
  fn frame_src_scheme_relative_url_is_resolved_against_document_url() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed(
      "https://example.com/page.html".to_string(),
      vec!["frame-src 'none'".to_string()],
    );

    let err = frame
      .check_frame_src("//evil.example/child")
      .expect_err("expected scheme-relative URL to be resolved then blocked");
    assert_eq!(
      err,
      "Blocked by Content-Security-Policy (frame-src) for requested URL: https://evil.example/child"
    );
  }

  #[test]
  fn frame_src_host_source_ipv6_literal_with_port() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed(
      "http://[::1]/".to_string(),
      vec!["frame-src http://[::1]:8080".to_string()],
    );

    frame
      .check_frame_src("http://[::1]:8080/child")
      .expect("expected frame-src host source to allow matching IPv6 literal + port");

    frame
      .check_frame_src("http://[::1]/child")
      .expect_err("expected frame-src host source with port to reject default port");
  }

  #[test]
  fn frame_src_host_source_path_exact_matching() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed(
      "https://parent.example/".to_string(),
      vec!["frame-src https://example.com/images/logo.png".to_string()],
    );

    frame
      .check_frame_src("https://example.com/images/logo.png")
      .expect("expected host source with exact path to allow exact URL path");

    frame
      .check_frame_src("https://example.com/images/other.png")
      .expect_err("expected host source with exact path to block other URL paths");
  }

  #[test]
  fn duplicate_frame_src_directives_ignore_subsequent_occurrences() {
    // CSP: When a directive appears multiple times within a policy set, only the first is used.
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed(
      "https://example.com/".to_string(),
      vec!["frame-src 'none'; frame-src https:".to_string()],
    );

    frame
      .check_frame_src("https://example.com/child")
      .expect_err("expected first frame-src directive to win");
  }

  #[test]
  fn mixed_content_blocks_final_url_after_redirect() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed("https://secure.example/".to_string(), Vec::new());
    frame.set_resource_policy(false, true);

    let err = frame
      .check_document_policy_with_final(
        "https://allowed.example/start",
        Some("http://insecure.example/landing"),
      )
      .expect_err("expected mixed-content policy to reject final redirected URL");
    assert_eq!(err, "Blocked mixed HTTP content from HTTPS document");
  }

  #[test]
  fn file_from_http_blocks_final_url_after_redirect() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed("https://example.com/".to_string(), Vec::new());
    // Default allow_file_from_http is false.
    frame.set_resource_policy(false, false);

    let err = frame
      .check_document_policy_with_final(
        "https://allowed.example/start",
        Some("file:///etc/passwd"),
      )
      .expect_err("expected file:// policy to reject final redirected URL");
    assert_eq!(err, "Blocked file:// resource from HTTP(S) document");
  }

  #[test]
  fn iframe_navigation_combines_policy_and_csp() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed(
      "https://example.com/".to_string(),
      vec!["frame-src *".to_string()],
    );
    frame.set_resource_policy(false, true);

    let err = frame
      .check_iframe_navigation("http://example.com/insecure")
      .expect_err("expected mixed-content policy to win over CSP allowlist");
    assert_eq!(err, "Blocked mixed HTTP content from HTTPS document");
  }

  #[test]
  fn iframe_src_resolution_uses_base_url_when_provided() {
    let mut frame = FrameNode::new(FrameId(1));
    frame.navigation_committed(
      "https://example.com/page.html".to_string(),
      vec!["frame-src https://allowed.example".to_string()],
    );
    frame.set_base_url("https://allowed.example/base/".to_string());

    let resolved = frame
      .check_frame_src("child")
      .expect("expected base URL to influence iframe URL resolution");
    assert_eq!(resolved.as_str(), "https://allowed.example/base/child");
  }
}
