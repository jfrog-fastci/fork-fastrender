//! Resource fetching abstraction
//!
//! This module provides a trait-based abstraction for fetching external resources
//! (images, CSS, etc.) from various sources. This allows the core library to remain
//! agnostic about how resources are retrieved, enabling:
//!
//! - Custom caching strategies (in test/dev tooling, not the library)
//! - Offline modes
//! - Mocking for tests
//! - Rate limiting
//!
//! # Example
//!
//! ```rust,no_run
//! # use fastrender::resource::{ResourceFetcher, HttpFetcher};
//! # fn main() -> fastrender::Result<()> {
//!
//! let fetcher = HttpFetcher::new();
//! let resource = fetcher.fetch("https://example.com/image.png")?;
//! println!("Got {} bytes", resource.bytes.len());
//! # Ok(())
//! # }
//! ```

use crate::debug::runtime;
use crate::error::{Error, ImageError, RenderError, RenderStage, ResourceError, Result};
use crate::fallible_vec_writer::FallibleVecWriter;
use crate::render_control::{self, check_active_periodic};
use brotli::Decompressor;
use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use http::HeaderMap;
use httpdate::parse_http_date;
use lru::LruCache;
use publicsuffix::{List, Psl};
use reqwest::blocking as reqwest_blocking;
use reqwest::cookie::{CookieStore, Jar as ReqwestCookieJar};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::collections::HashSet;
use std::convert::TryFrom;
use std::io::{self, Cursor, Read, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use ureq::ResponseExt;
use url::Url;

pub mod bundle;
mod cors;
mod curl_backend;
pub(crate) mod data_url;
#[cfg(feature = "disk_cache")]
pub mod disk_cache;
pub mod web_fetch;
pub use cors::{cors_enforcement_enabled, validate_cors_allow_origin, CorsMode};
#[cfg(feature = "disk_cache")]
pub use disk_cache::{DiskCacheConfig, DiskCachingFetcher};

// ============================================================================
// Origin and resource policy
// ============================================================================

/// Origin model capturing scheme, host, and port.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DocumentOrigin {
  scheme: String,
  host: Option<String>,
  port: Option<u16>,
}

impl DocumentOrigin {
  fn new(scheme: String, host: Option<String>, port: Option<u16>) -> Self {
    Self { scheme, host, port }
  }

  fn from_parsed_url(url: &Url) -> Self {
    let scheme = url.scheme().to_ascii_lowercase();
    let host = url.host_str().map(|h| h.to_ascii_lowercase());
    let port = match scheme.as_str() {
      "http" | "https" => url.port_or_known_default(),
      _ => url.port(),
    };
    Self::new(scheme, host, port)
  }

  /// Return the scheme string for this origin.
  pub fn scheme(&self) -> &str {
    &self.scheme
  }

  /// Host portion of the origin, if present.
  pub fn host(&self) -> Option<&str> {
    self.host.as_deref()
  }

  /// Port portion of the origin, if present.
  pub fn port(&self) -> Option<u16> {
    self.port
  }

  fn effective_port(&self) -> Option<u16> {
    match (self.scheme.as_str(), self.port) {
      ("http", None) => Some(80),
      ("https", None) => Some(443),
      _ => self.port,
    }
  }

  fn same_origin(&self, other: &DocumentOrigin) -> bool {
    if self.scheme != other.scheme {
      return false;
    }
    if self.is_http_like() {
      return self.host == other.host && self.effective_port() == other.effective_port();
    }
    if self.scheme == "file" {
      return true;
    }
    self.host == other.host && self.port == other.port
  }

  /// True for HTTPS documents.
  pub fn is_secure_http(&self) -> bool {
    self.scheme == "https"
  }

  /// True for HTTP/HTTPS documents.
  pub fn is_http_like(&self) -> bool {
    matches!(self.scheme.as_str(), "http" | "https")
  }
}

impl std::fmt::Display for DocumentOrigin {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    if self.scheme == "file" {
      return write!(f, "file://");
    }
    let host = self.host.as_deref().unwrap_or("<unknown>");
    match self.effective_port() {
      Some(port) => write!(f, "{}://{}:{}", self.scheme, host, port),
      None => write!(f, "{}://{}", self.scheme, host),
    }
  }
}

/// Attempt to derive a document origin from a URL string.
pub fn origin_from_url(url: &str) -> Option<DocumentOrigin> {
  let parsed = Url::parse(url).ok()?;
  Some(DocumentOrigin::from_parsed_url(&parsed))
}

/// Policy controlling which subresources can be loaded for a document.
#[derive(Debug, Clone)]
pub struct ResourceAccessPolicy {
  /// Origin of the current document (if known).
  pub document_origin: Option<DocumentOrigin>,
  /// Allow loading file:// resources from HTTP(S) documents.
  pub allow_file_from_http: bool,
  /// Block mixed HTTP content when the document is HTTPS.
  pub block_mixed_content: bool,
  /// Restrict subresources to the document origin unless explicitly allowlisted.
  pub same_origin_only: bool,
  /// Additional origins allowed when enforcing same-origin subresource loading.
  pub allowed_origins: Vec<DocumentOrigin>,
}

impl Default for ResourceAccessPolicy {
  fn default() -> Self {
    Self {
      document_origin: None,
      allow_file_from_http: false,
      block_mixed_content: false,
      same_origin_only: false,
      allowed_origins: Vec::new(),
    }
  }
}

impl ResourceAccessPolicy {
  /// Return a copy of this policy with a different document origin.
  pub fn for_origin(&self, origin: Option<DocumentOrigin>) -> Self {
    let mut cloned = self.clone();
    cloned.document_origin = origin;
    cloned
  }

  /// Check whether a subresource URL is allowed under this policy.
  pub fn allows(&self, target_url: &str) -> std::result::Result<(), PolicyError> {
    self.allows_with_final(target_url, None)
  }

  /// Check whether a document URL is allowed under this policy.
  ///
  /// Document navigations (e.g. iframe/embed loads) intentionally skip `same_origin_only`
  /// enforcement. The `same_origin_subresources` knob is intended for subresources like
  /// stylesheets/images/fonts; blocking cross-origin frames requires a dedicated policy.
  ///
  /// Note: other checks (e.g. mixed content and `file://` from HTTP(S)) still apply.
  pub fn allows_document(&self, target_url: &str) -> std::result::Result<(), PolicyError> {
    self.allows_document_with_final(target_url, None)
  }

  /// Check whether a subresource URL is allowed, considering any final URL after redirects.
  pub fn allows_with_final(
    &self,
    target_url: &str,
    final_url: Option<&str>,
  ) -> std::result::Result<(), PolicyError> {
    self.allows_internal(target_url, final_url, self.same_origin_only)
  }

  /// Check whether a document URL is allowed, considering any final URL after redirects, while
  /// skipping same-origin enforcement.
  pub fn allows_document_with_final(
    &self,
    target_url: &str,
    final_url: Option<&str>,
  ) -> std::result::Result<(), PolicyError> {
    self.allows_internal(target_url, final_url, false)
  }

  fn allows_internal(
    &self,
    target_url: &str,
    final_url: Option<&str>,
    enforce_same_origin: bool,
  ) -> std::result::Result<(), PolicyError> {
    let Some(origin) = &self.document_origin else {
      return Ok(());
    };

    let effective_url = final_url.unwrap_or(target_url);
    let parsed = match Url::parse(effective_url) {
      Ok(parsed) => parsed,
      Err(_) => {
        if enforce_same_origin
          && (effective_url.starts_with("http://") || effective_url.starts_with("https://"))
        {
          return Err(PolicyError {
            reason: format!("Blocked subresource with invalid or missing host: {effective_url}"),
          });
        }
        return Ok(());
      }
    };

    let target_origin = DocumentOrigin::from_parsed_url(&parsed);
    if target_origin.is_http_like() && target_origin.host().is_none() && enforce_same_origin {
      return Err(PolicyError {
        reason: format!("Blocked subresource with missing host: {effective_url}"),
      });
    }

    // Parse the target URL scheme; if unparseable, allow to avoid over-blocking.
    let scheme = parsed.scheme().to_ascii_lowercase();

    if scheme == "data" {
      return Ok(());
    }

    if origin.is_http_like() && scheme == "file" && !self.allow_file_from_http {
      return Err(PolicyError {
        reason: "Blocked file:// resource from HTTP(S) document".to_string(),
      });
    }

    if origin.is_secure_http() && self.block_mixed_content && scheme == "http" {
      return Err(PolicyError {
        reason: "Blocked mixed HTTP content from HTTPS document".to_string(),
      });
    }

    if !enforce_same_origin {
      return Ok(());
    }

    if self
      .allowed_origins
      .iter()
      .any(|allowed| allowed.same_origin(&target_origin))
    {
      return Ok(());
    }

    if origin.same_origin(&target_origin) {
      return Ok(());
    }

    Err(PolicyError {
      reason: format!(
        "Blocked cross-origin subresource: document origin {} does not match {} ({})",
        origin, target_origin, effective_url
      ),
    })
  }
}

/// Subresource load blocked by policy.
#[derive(Debug, Clone)]
pub struct PolicyError {
  pub reason: String,
}

/// Normalize a page identifier (full URL or hostname) to a cache/output stem.
///
/// Strips schemes and leading "www.", lowercases the host, and sanitizes for filenames.
pub fn normalize_page_name(raw: &str) -> Option<String> {
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return None;
  }
  // First try full URL parsing so we can normalize host casing and strip www. reliably.
  if let Ok(parsed) = Url::parse(trimmed) {
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    // Treat trailing dots as punctuation so inputs like "example.com./path" normalize to the same
    // cache stem as "example.com/path".
    let host = host.trim_end_matches('.');
    let mut stem = String::from(host);
    stem.push_str(&parsed[url::Position::BeforePath..url::Position::AfterQuery]);
    return Some(sanitize_filename(&stem));
  }

  // Fallback: case-insensitive scheme + www stripping for bare hosts or host+path strings.
  let mut without_scheme = trimmed;
  for scheme in ["https://", "http://"] {
    if trimmed
      .get(..scheme.len())
      .map(|prefix| prefix.eq_ignore_ascii_case(scheme))
      .unwrap_or(false)
    {
      without_scheme = &trimmed[scheme.len()..];
      break;
    }
  }

  let without_www = if without_scheme
    .get(..4)
    .map(|prefix| prefix.eq_ignore_ascii_case("www."))
    .unwrap_or(false)
  {
    &without_scheme[4..]
  } else {
    without_scheme
  };

  let (host, rest) = match without_www.find('/') {
    Some(idx) => (&without_www[..idx], &without_www[idx..]),
    None => match without_www.find('_') {
      // When called with an already-sanitized cache stem (e.g. `example.com_Path`), `without_www`
      // will not contain `/` separators. Treat the first underscore as the boundary between the
      // hostname and the rest of the stem so we preserve case-sensitive path/query segments.
      //
      // This keeps normalization idempotent for `normalize_page_name` outputs and matches how the
      // CLI tools name cached pages.
      Some(idx) => (&without_www[..idx], &without_www[idx..]),
      None => (without_www, ""),
    },
  };

  let host = host.trim_end_matches('.');
  let lowered = format!("{}{}", host.to_ascii_lowercase(), rest);
  let no_fragment = lowered
    .split_once('#')
    .map(|(before, _)| before.to_string())
    .unwrap_or(lowered);

  Some(sanitize_filename(&no_fragment))
}

/// Normalize a URL into a filename-safe stem used for caches and outputs.
pub fn url_to_filename(url: &str) -> String {
  let trimmed = url.trim();
  // First, try to parse the URL so we can lowercase the hostname (case-insensitive per URL
  // spec) and strip the scheme regardless of casing. If parsing fails, fall back to a best-effort
  // scheme-stripping path similar to the old behavior.
  if let Ok(parsed) = Url::parse(trimmed) {
    let mut stem = String::new();
    if let Some(host) = parsed.host_str() {
      stem.push_str(&host.to_ascii_lowercase());
    }
    // Preserve path/query casing while normalizing separators later.
    stem.push_str(&parsed[url::Position::BeforePath..url::Position::AfterQuery]);
    return sanitize_filename(&stem);
  }

  // Fallback: remove common schemes case-insensitively and lowercase only the hostname portion.
  let mut trimmed = trimmed;
  for scheme in ["https://", "http://"] {
    if trimmed
      .get(..scheme.len())
      .map(|prefix| prefix.eq_ignore_ascii_case(scheme))
      .unwrap_or(false)
    {
      trimmed = &trimmed[scheme.len()..];
      break;
    }
  }

  let (host, rest) = match trimmed.find('/') {
    Some(idx) => (&trimmed[..idx], &trimmed[idx..]),
    None => (trimmed, ""),
  };
  let lowered = format!("{}{}", host.to_ascii_lowercase(), rest);
  let no_fragment = lowered
    .split_once('#')
    .map(|(before, _)| before.to_string())
    .unwrap_or(lowered);
  sanitize_filename(&no_fragment)
}

fn sanitize_filename(input: &str) -> String {
  // Trim trailing slashes so we don’t leave a dangling underscore after replacement.
  let trimmed_slash = input.trim_end_matches('/');
  let sanitized: String = trimmed_slash
    .replace('/', "_")
    .chars()
    .map(|c| {
      if c.is_alphanumeric() || c == '.' || c == '_' || c == '-' {
        c
      } else {
        '_'
      }
    })
    .collect();

  // Trim trailing underscores/dots introduced by trailing slashes or punctuation so that
  // "https://example.com/" normalizes to "example.com" instead of "example.com_".
  // If trimming would produce an empty string, fall back to the sanitized value.
  let mut result = sanitized.trim_end_matches(['_', '.']).to_string();
  if result.is_empty() {
    result = sanitized;
  }

  // Avoid a trailing underscore when the path ends with a slash (common for canonical URLs).
  while result.ends_with('_') {
    result.pop();
  }

  result
}

/// Default User-Agent string used by HTTP fetchers
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36";

/// Default Accept-Language header value
pub const DEFAULT_ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

/// Default Accept header value.
///
/// This is intentionally "browser-like" while remaining broadly compatible with non-HTML
/// subresource requests (it includes `*/*`).
const DEFAULT_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

const BROWSER_ACCEPT_ALL: &str = "*/*";
const BROWSER_ACCEPT_STYLESHEET: &str = "text/css,*/*;q=0.1";
const BROWSER_ACCEPT_IMAGE: &str =
  "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8";

/// Content-Encoding algorithms this fetcher can decode.
const SUPPORTED_ACCEPT_ENCODING: &str = "gzip, deflate, br";

/// Maximum time we allow the primary `ureq` backend to spend before falling back to cURL
/// when `FASTR_HTTP_BACKEND=auto`.
///
/// This is primarily to avoid HTTP/1.1 hangs consuming the entire timeout budget (e.g. sites that
/// only respond reliably over HTTP/2).
const AUTO_BACKEND_UREQ_TIMEOUT_CAP: Duration = Duration::from_secs(5);

fn auto_backend_ureq_timeout_slice(total: Duration) -> Duration {
  if total.is_zero() {
    return Duration::ZERO;
  }
  let half = Duration::from_secs_f64(total.as_secs_f64() * 0.5);
  half.min(AUTO_BACKEND_UREQ_TIMEOUT_CAP)
}

fn rewrite_known_pageset_url(url: &str) -> Option<String> {
  Url::parse(url).ok().and_then(|mut parsed| {
    if parsed.scheme() != "https" {
      return None;
    }
    let host = parsed.host_str()?;

    // Some pageset domains (notably `tesco.com` and `nhk.or.jp`) do not resolve/reply reliably
    // without the `www.` subdomain in certain environments. Rewrite to the canonical host so the
    // pageset can still fetch and render deterministically.
    //
    // Note: this is intentionally scoped to a small allowlist to avoid surprising callers with
    // implicit host changes.
    if host.eq_ignore_ascii_case("tesco.com") {
      parsed.set_host(Some("www.tesco.com")).ok()?;
    } else if host.eq_ignore_ascii_case("nhk.or.jp") {
      parsed.set_host(Some("www.nhk.or.jp")).ok()?;
    } else if host.eq_ignore_ascii_case("developer.mozilla.org") {
      // MDN occasionally moves pages without leaving an HTTP redirect. Rewrite known moved pages
      // so the pageset can continue to fetch deterministically while keeping the original cache
      // stem/progress artifact name stable.
      if parsed.path() == "/en-US/docs/Web/CSS/CSS_multicol_layout/Using_multi-column_layouts" {
        parsed.set_path("/en-US/docs/Web/CSS/Guides/Multicol_layout/Using");
      } else {
        return None;
      }
    } else {
      return None;
    }

    Some(parsed.to_string())
  })
}

fn http_browser_headers_enabled() -> bool {
  static ENABLED: OnceLock<bool> = OnceLock::new();
  *ENABLED.get_or_init(|| {
    std::env::var("FASTR_HTTP_BROWSER_HEADERS")
      .ok()
      .map(|raw| {
        !matches!(
          raw.trim().to_ascii_lowercase().as_str(),
          "0" | "false" | "no" | "off"
        )
      })
      .unwrap_or(true)
  })
}

/// Best-effort origin extraction for use in browser-like header generation.
///
/// Unlike `Url::parse`, this intentionally ignores strict URL validation in the path/query portion
/// so we can still classify same-origin / same-site relationships for requests whose paths contain
/// characters rejected by `url` (e.g. `|` in query strings).
///
/// This must only be used for header decisions; it must not be used to mutate the actual request
/// URL.
fn http_browser_tolerant_origin_from_url(url: &str) -> Option<DocumentOrigin> {
  let trimmed = url.trim();
  let scheme_end = trimmed.find("://")?;
  let scheme = trimmed[..scheme_end].to_ascii_lowercase();
  if !matches!(scheme.as_str(), "http" | "https") {
    return None;
  }

  let after_scheme = &trimmed[scheme_end + "://".len()..];
  let authority_end = after_scheme
    .find(|c| matches!(c, '/' | '?' | '#'))
    .unwrap_or(after_scheme.len());
  let authority = &after_scheme[..authority_end];
  if authority.is_empty() {
    return None;
  }

  // Drop userinfo (`user:pass@host`) if present. Use the last `@` so passwords containing `@`
  // don't confuse the split.
  let authority = authority
    .rsplit_once('@')
    .map(|(_, host)| host)
    .unwrap_or(authority);

  if authority.is_empty() {
    return None;
  }

  let (host, port) = if authority.starts_with('[') {
    let end = authority.find(']')?;
    let host = &authority[1..end];
    let rest = &authority[end + 1..];
    let port = if rest.is_empty() {
      None
    } else if let Some(port) = rest.strip_prefix(':') {
      if port.is_empty() {
        return None;
      }
      Some(port.parse::<u16>().ok()?)
    } else {
      return None;
    };
    (host, port)
  } else if let Some((host, port)) = authority.rsplit_once(':') {
    // Reject IPv6 hosts without brackets.
    if host.contains(':') {
      return None;
    }
    if port.is_empty() {
      return None;
    }
    let port = port.parse::<u16>().ok()?;
    (host, Some(port))
  } else {
    (authority, None)
  };

  if host.is_empty() {
    return None;
  }
  let host = host.to_ascii_lowercase();

  Some(DocumentOrigin::new(scheme, Some(host), port))
}

/// Normalize a referrer URL for use in the HTTP `Referer` header without requiring strict URL
/// parsing.
///
/// This exists for tolerant referrer handling: `http_referer_header_value` supports referrer URLs
/// that contain characters rejected by `url::Url::parse` (e.g. `|` in query strings) so we can
/// still generate `Referer` headers for real-world pages. When taking the tolerant path, we:
///
/// - strip any `user:pass@` credentials
/// - lowercase scheme + host (matching URL serialization)
/// - drop default ports (80/443)
fn http_normalize_referrer_url_tolerant(url: &str) -> String {
  let trimmed = url.trim();
  let Some(scheme_end) = trimmed.find("://") else {
    return trimmed.to_string();
  };
  let scheme = trimmed[..scheme_end].to_ascii_lowercase();
  if !matches!(scheme.as_str(), "http" | "https") {
    return trimmed.to_string();
  }

  let after_scheme = &trimmed[scheme_end + "://".len()..];
  let authority_end = after_scheme
    .find(|c| matches!(c, '/' | '?' | '#'))
    .unwrap_or(after_scheme.len());
  let authority = &after_scheme[..authority_end];
  let host_port = authority
    .rsplit_once('@')
    .map(|(_, host_port)| host_port)
    .unwrap_or(authority);
  if host_port.is_empty() {
    return trimmed.to_string();
  }

  let default_port = match scheme.as_str() {
    "http" => Some(80u16),
    "https" => Some(443u16),
    _ => None,
  };

  let normalize_port = |port: &str| -> Option<Option<String>> {
    if port.is_empty() {
      return None;
    }
    let Ok(port_num) = port.parse::<u16>() else {
      return Some(Some(port.to_string()));
    };
    if default_port.is_some_and(|default| default == port_num) {
      return Some(None);
    }
    Some(Some(port_num.to_string()))
  };

  let normalized_host_port = if host_port.starts_with('[') {
    match host_port.find(']') {
      Some(end) => {
        let host = host_port.get(1..end).unwrap_or("");
        if host.is_empty() {
          host_port.to_string()
        } else {
          let rest = &host_port[end + 1..];
          let port = if rest.is_empty() {
            Some(None)
          } else if let Some(port) = rest.strip_prefix(':') {
            normalize_port(port)
          } else {
            None
          };
          if let Some(port) = port {
            let mut out = String::with_capacity(host_port.len());
            out.push('[');
            out.push_str(&host.to_ascii_lowercase());
            out.push(']');
            if let Some(port) = port {
              out.push(':');
              out.push_str(&port);
            }
            out
          } else {
            host_port.to_string()
          }
        }
      }
      None => host_port.to_string(),
    }
  } else {
    let (host, port) = host_port
      .rsplit_once(':')
      .and_then(|(host, port)| (!host.contains(':')).then_some((host, port)))
      .unwrap_or((host_port, ""));
    let port = if host == host_port { Some(None) } else { normalize_port(port) };
    if let Some(port) = port {
      let mut out = String::with_capacity(host_port.len());
      out.push_str(&host.to_ascii_lowercase());
      if let Some(port) = port {
        out.push(':');
        out.push_str(&port);
      }
      out
    } else {
      host_port.to_string()
    }
  };

  let mut out = String::with_capacity(trimmed.len());
  out.push_str(&scheme);
  out.push_str("://");
  out.push_str(&normalized_host_port);
  out.push_str(&after_scheme[authority_end..]);
  out
}

fn http_browser_origin_and_referer_for_origin(origin: &DocumentOrigin) -> Option<(String, String)> {
  if !matches!(origin.scheme.as_str(), "http" | "https") {
    return None;
  }
  let host = origin.host.as_deref()?;
  let host = match host.parse::<std::net::IpAddr>() {
    Ok(std::net::IpAddr::V6(_)) => format!("[{host}]"),
    _ => host.to_string(),
  };

  let mut origin_str = format!("{}://{}", origin.scheme, host);
  if let Some(port) = origin.port {
    let default_port = match origin.scheme.as_str() {
      "http" => 80,
      "https" => 443,
      _ => port,
    };
    if port != default_port {
      origin_str.push_str(&format!(":{port}"));
    }
  }

  let referer = format!("{origin_str}/");
  Some((origin_str, referer))
}

fn http_browser_origin_and_referer_for_url(url: &Url) -> Option<(String, String)> {
  if !matches!(url.scheme(), "http" | "https") {
    return None;
  }

  let host = match url.host()? {
    url::Host::Domain(domain) => domain.to_string(),
    url::Host::Ipv4(addr) => addr.to_string(),
    url::Host::Ipv6(addr) => format!("[{addr}]"),
  };

  let mut origin = format!("{}://{}", url.scheme(), host);
  if let Some(port) = url.port() {
    let default_port = match url.scheme() {
      "http" => 80,
      "https" => 443,
      _ => port,
    };
    if port != default_port {
      origin.push_str(&format!(":{port}"));
    }
  }

  let referer = format!("{origin}/");
  Some((origin, referer))
}

fn http_browser_registrable_domain(host: &str) -> Option<String> {
  static PSL: OnceLock<List> = OnceLock::new();
  let list = PSL.get_or_init(List::default);

  let lowered = host.trim_end_matches('.').to_ascii_lowercase();
  let domain = list.domain(lowered.as_bytes())?;
  let domain = std::str::from_utf8(domain.as_bytes()).ok()?;
  Some(domain.to_ascii_lowercase())
}

fn http_browser_schemeful_same_site(referrer_url: &Url, target_url: &Url) -> bool {
  let referrer_origin = DocumentOrigin::from_parsed_url(referrer_url);
  let target_origin = DocumentOrigin::from_parsed_url(target_url);
  http_browser_schemeful_same_site_from_origins(&referrer_origin, &target_origin)
}

fn http_browser_schemeful_same_site_from_origins(
  referrer_origin: &DocumentOrigin,
  target_origin: &DocumentOrigin,
) -> bool {
  if referrer_origin.scheme != target_origin.scheme {
    return false;
  }

  let (Some(referrer_host), Some(target_host)) = (referrer_origin.host(), target_origin.host())
  else {
    return false;
  };

  // IP hosts do not have registrable domains, so treat them as cross-site unless they were already
  // classified as same-origin.
  if referrer_host.parse::<std::net::IpAddr>().is_ok()
    || target_host.parse::<std::net::IpAddr>().is_ok()
  {
    return false;
  }

  let Some(referrer_site) = http_browser_registrable_domain(referrer_host) else {
    return false;
  };
  let Some(target_site) = http_browser_registrable_domain(target_host) else {
    return false;
  };
  referrer_site == target_site
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FetchDestination {
  Document,
  /// Top-level document navigation without user activation (e.g. `<meta http-equiv=refresh>` or
  /// `window.location = ...`).
  ///
  /// Chromium omits `Sec-Fetch-User` for these requests, but otherwise uses the same headers as
  /// user-activated document navigations.
  DocumentNoUser,
  /// Subframe document navigation (e.g. `<iframe src=...>`).
  ///
  /// Uses the same `Accept` header as top-level documents, but Chromium sends `Sec-Fetch-Dest:
  /// iframe` and omits `Sec-Fetch-User` (subframe navigations are not user-activated).
  Iframe,
  Style,
  /// Stylesheet fetched in CORS mode (e.g. `<link rel=stylesheet crossorigin>`).
  ///
  /// `Sec-Fetch-Dest` remains `style`, but `Sec-Fetch-Mode` becomes `cors` and a browser-like
  /// `Origin` header is sent when request headers are enabled.
  StyleCors,
  Image,
  /// Image fetched in CORS mode (e.g. `<img crossorigin>`).
  ///
  /// `Sec-Fetch-Dest` remains `image`, but `Sec-Fetch-Mode` becomes `cors` and a browser-like
  /// `Origin` header is sent when request headers are enabled.
  ImageCors,
  Font,
  Other,
  /// JavaScript `fetch()` request.
  ///
  /// This matches typical `window.fetch()` traffic (as opposed to [`FetchDestination::Other`],
  /// which uses a `no-cors` mode closer to passive subresource fetches).
  ///
  /// Note: `Sec-Fetch-Site` classification is computed in `build_http_header_pairs()` when
  /// `client_origin` or `referrer_url` is provided. The value returned by
  /// `sec_fetch_site()` is only a conservative fallback when that context is
  /// unavailable.
  Fetch,
}

/// Credentials mode for a fetch request (roughly aligned with the Fetch spec).
///
/// This is primarily used to partition caches for requests that may carry cookies/auth headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FetchCredentialsMode {
  /// Do not include credentials (anonymous).
  Omit,
  /// Include credentials only for same-origin requests.
  SameOrigin,
  /// Include credentials (cookies/auth).
  Include,
}

impl FetchCredentialsMode {
  const fn cache_id(self) -> u8 {
    match self {
      Self::Omit => 0,
      Self::SameOrigin => 1,
      Self::Include => 2,
    }
  }

  fn as_cache_tag(self) -> &'static str {
    match self {
      Self::Omit => "omit",
      Self::SameOrigin => "same-origin",
      Self::Include => "include",
    }
  }
}

impl FetchDestination {
  fn accept(self) -> &'static str {
    match self {
      Self::Document | Self::DocumentNoUser | Self::Iframe => DEFAULT_ACCEPT,
      Self::Style | Self::StyleCors => BROWSER_ACCEPT_STYLESHEET,
      Self::Image | Self::ImageCors => BROWSER_ACCEPT_IMAGE,
      Self::Font | Self::Other | Self::Fetch => BROWSER_ACCEPT_ALL,
    }
  }

  fn sec_fetch_dest(self) -> &'static str {
    match self {
      Self::Document | Self::DocumentNoUser => "document",
      Self::Iframe => "iframe",
      Self::Style | Self::StyleCors => "style",
      Self::Image | Self::ImageCors => "image",
      Self::Font => "font",
      Self::Other | Self::Fetch => "empty",
    }
  }

  fn sec_fetch_mode(self) -> &'static str {
    match self {
      Self::Document | Self::DocumentNoUser | Self::Iframe => "navigate",
      Self::Font | Self::ImageCors | Self::StyleCors | Self::Fetch => "cors",
      Self::Style | Self::Image | Self::Other => "no-cors",
    }
  }

  fn sec_fetch_site(self) -> &'static str {
    match self {
      Self::Document | Self::DocumentNoUser | Self::Iframe => "none",
      Self::Style
      | Self::StyleCors
      | Self::Image
      | Self::ImageCors
      | Self::Font
      | Self::Other
      | Self::Fetch => "same-origin",
    }
  }

  fn sec_fetch_user(self) -> Option<&'static str> {
    match self {
      Self::Document => Some("?1"),
      _ => None,
    }
  }

  fn upgrade_insecure_requests(self) -> Option<&'static str> {
    match self {
      Self::Document | Self::DocumentNoUser | Self::Iframe => Some("1"),
      _ => None,
    }
  }

  fn origin_and_referer(self, url: &Url) -> Option<(String, String)> {
    match self {
      Self::Font | Self::ImageCors | Self::StyleCors => http_browser_origin_and_referer_for_url(url),
      _ => None,
    }
  }
}

pub(crate) fn http_browser_request_profile_for_url(url: &str) -> FetchDestination {
  let Ok(parsed) = Url::parse(url) else {
    return FetchDestination::Document;
  };
  let ext = Path::new(parsed.path())
    .extension()
    .and_then(|e| e.to_str());
  match ext {
    None => FetchDestination::Document,
    Some(ext) if ext.eq_ignore_ascii_case("css") => FetchDestination::Style,
    Some(ext)
      if ext.eq_ignore_ascii_case("woff")
        || ext.eq_ignore_ascii_case("woff2")
        || ext.eq_ignore_ascii_case("ttf")
        || ext.eq_ignore_ascii_case("otf") =>
    {
      FetchDestination::Font
    }
    Some(ext)
      if ext.eq_ignore_ascii_case("png")
        || ext.eq_ignore_ascii_case("jpg")
        || ext.eq_ignore_ascii_case("jpeg")
        || ext.eq_ignore_ascii_case("gif")
        || ext.eq_ignore_ascii_case("webp")
        || ext.eq_ignore_ascii_case("avif")
        || ext.eq_ignore_ascii_case("svg")
        || ext.eq_ignore_ascii_case("ico")
        || ext.eq_ignore_ascii_case("bmp") =>
    {
      FetchDestination::Image
    }
    Some(ext)
      if ext.eq_ignore_ascii_case("html")
        || ext.eq_ignore_ascii_case("htm")
        || ext.eq_ignore_ascii_case("php")
        || ext.eq_ignore_ascii_case("asp")
        || ext.eq_ignore_ascii_case("aspx")
        || ext.eq_ignore_ascii_case("jsp")
        || ext.eq_ignore_ascii_case("cgi") =>
    {
      FetchDestination::Document
    }
    Some(_) => FetchDestination::Other,
  }
}

// ============================================================================
// Referrer Policy
// ============================================================================

/// Referrer policy applied when generating the `Referer` request header.
///
/// This models the [Referrer Policy specification](https://www.w3.org/TR/referrer-policy/) token
/// values. The [`ReferrerPolicy::EmptyString`] variant represents the empty-string state used by
/// the spec (meaning "use the default policy").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReferrerPolicy {
  /// Empty-string / unspecified referrer policy ("use default").
  EmptyString,
  NoReferrer,
  NoReferrerWhenDowngrade,
  Origin,
  OriginWhenCrossOrigin,
  SameOrigin,
  StrictOrigin,
  StrictOriginWhenCrossOrigin,
  UnsafeUrl,
}

impl ReferrerPolicy {
  /// Chromium's default policy for requests without an explicit referrer policy.
  pub const CHROMIUM_DEFAULT: ReferrerPolicy = ReferrerPolicy::StrictOriginWhenCrossOrigin;

  pub const fn as_str(self) -> &'static str {
    match self {
      Self::EmptyString => "",
      Self::NoReferrer => "no-referrer",
      Self::NoReferrerWhenDowngrade => "no-referrer-when-downgrade",
      Self::Origin => "origin",
      Self::OriginWhenCrossOrigin => "origin-when-cross-origin",
      Self::SameOrigin => "same-origin",
      Self::StrictOrigin => "strict-origin",
      Self::StrictOriginWhenCrossOrigin => "strict-origin-when-cross-origin",
      Self::UnsafeUrl => "unsafe-url",
    }
  }

  /// Parse a `referrerpolicy` attribute value.
  ///
  /// Returns `None` when the attribute is missing, empty, or invalid, which signals that the
  /// request should use the document's default referrer policy.
  pub fn from_attribute(value: &str) -> Option<Self> {
    match Self::parse(value)? {
      Self::EmptyString => None,
      other => Some(other),
    }
  }

  /// Parse a referrer policy token (case-insensitive, trims whitespace).
  ///
  /// Returns `None` when the token is unrecognized.
  pub fn parse(raw: &str) -> Option<Self> {
    let token = raw.trim();
    if token.is_empty() {
      return Some(Self::EmptyString);
    }
    if token.eq_ignore_ascii_case("no-referrer") {
      Some(Self::NoReferrer)
    } else if token.eq_ignore_ascii_case("no-referrer-when-downgrade") {
      Some(Self::NoReferrerWhenDowngrade)
    } else if token.eq_ignore_ascii_case("origin") {
      Some(Self::Origin)
    } else if token.eq_ignore_ascii_case("origin-when-cross-origin") {
      Some(Self::OriginWhenCrossOrigin)
    } else if token.eq_ignore_ascii_case("same-origin") {
      Some(Self::SameOrigin)
    } else if token.eq_ignore_ascii_case("strict-origin") {
      Some(Self::StrictOrigin)
    } else if token.eq_ignore_ascii_case("strict-origin-when-cross-origin") {
      Some(Self::StrictOriginWhenCrossOrigin)
    } else if token.eq_ignore_ascii_case("unsafe-url") {
      Some(Self::UnsafeUrl)
    } else {
      None
    }
  }

  /// Parse a comma/whitespace separated referrer policy token list.
  ///
  /// This is used by both `<meta name="referrer">` and the `Referrer-Policy` response header.
  /// When multiple recognized tokens are present, the last one wins.
  pub fn parse_value_list(value: &str) -> Option<Self> {
    let mut policy = None;
    for raw_token in value.split(|c: char| c == ',' || c.is_whitespace()) {
      let Some(parsed) = Self::parse(raw_token) else {
        continue;
      };
      if parsed == Self::EmptyString {
        continue;
      }
      policy = Some(parsed);
    }
    policy
  }

  fn resolve(self, default: ReferrerPolicy) -> ReferrerPolicy {
    match self {
      ReferrerPolicy::EmptyString => default,
      other => other,
    }
  }
}

impl Default for ReferrerPolicy {
  fn default() -> Self {
    Self::EmptyString
  }
}

/// Contextual metadata associated with a resource fetch.
///
/// This is primarily used by HTTP fetchers to populate browser-like request headers (e.g. `Accept`,
/// `Sec-Fetch-*`, and `Referer`) so captured bundles reflect a realistic fetch profile.
#[derive(Debug, Clone, Copy)]
pub struct FetchRequest<'a> {
  pub url: &'a str,
  pub destination: FetchDestination,
  /// URL used to populate the HTTP `Referer` header.
  ///
  /// This is not necessarily the origin of the initiating document; for example, CORS-mode CSS
  /// resources (notably web fonts) use the stylesheet URL as the referrer even when the document
  /// origin differs.
  pub referrer_url: Option<&'a str>,
  /// Origin of the client/environment settings object that initiated the fetch.
  ///
  /// This is used for CORS-mode request headers (`Origin`) and `Sec-Fetch-Site` classification.
  pub client_origin: Option<&'a DocumentOrigin>,
  pub referrer_policy: ReferrerPolicy,
  pub credentials_mode: FetchCredentialsMode,
}

impl<'a> FetchRequest<'a> {
  /// Create a new request for the given URL and destination.
  pub fn new(url: &'a str, destination: FetchDestination) -> Self {
    Self {
      url,
      destination,
      referrer_url: None,
      client_origin: None,
      referrer_policy: ReferrerPolicy::default(),
      credentials_mode: if destination.sec_fetch_mode() == "cors" {
        FetchCredentialsMode::Omit
      } else {
        FetchCredentialsMode::Include
      },
    }
  }

  /// Create a document navigation request.
  pub fn document(url: &'a str) -> Self {
    Self::new(url, FetchDestination::Document)
  }

  /// Create a document navigation request without user activation.
  pub fn document_no_user(url: &'a str) -> Self {
    Self::new(url, FetchDestination::DocumentNoUser)
  }

  /// Create an image fetch request.
  pub fn image(url: &'a str) -> Self {
    Self::new(url, FetchDestination::Image)
  }

  /// Attach a referrer URL used to construct the HTTP `Referer` header.
  pub fn with_referrer_url(mut self, referrer_url: &'a str) -> Self {
    self.referrer_url = Some(referrer_url);
    self
  }

  /// Attach the initiating client origin used for CORS-mode headers and `Sec-Fetch-Site`.
  pub fn with_client_origin(mut self, client_origin: &'a DocumentOrigin) -> Self {
    self.client_origin = Some(client_origin);
    self
  }

  /// Override the referrer policy used when generating the `Referer` header.
  pub fn with_referrer_policy(mut self, policy: ReferrerPolicy) -> Self {
    self.referrer_policy = policy;
    self
  }

  /// Override the credentials mode for this request.
  pub fn with_credentials_mode(mut self, mode: FetchCredentialsMode) -> Self {
    self.credentials_mode = mode;
    self
  }
}

fn cookies_allowed_for_request(
  credentials_mode: FetchCredentialsMode,
  url: &str,
  client_origin: Option<&DocumentOrigin>,
) -> bool {
  match credentials_mode {
    FetchCredentialsMode::Include => true,
    FetchCredentialsMode::Omit => false,
    FetchCredentialsMode::SameOrigin => {
      let Some(client_origin) = client_origin else {
        return false;
      };
      let request_origin =
        origin_from_url(url).or_else(|| http_browser_tolerant_origin_from_url(url));
      match request_origin {
        Some(request_origin) => client_origin.same_origin(&request_origin),
        None => false,
      }
    }
  }
}

/// Safety buffer subtracted from deadline-derived HTTP timeouts.
///
/// When rendering under a cooperative timeout, we must ensure that HTTP fetches never extend past
/// the remaining render budget. Leaving a small buffer gives the renderer time to unwind and
/// return a structured timeout/error before any external hard-kill triggers.
const HTTP_DEADLINE_BUFFER: Duration = Duration::from_millis(25);

/// Stride for cooperative deadline checks while decoding compressed bodies.
const CONTENT_DECODE_DEADLINE_STRIDE: usize = 16;

/// Retry/backoff policy for [`HttpFetcher`].
#[derive(Debug, Clone)]
pub struct HttpRetryPolicy {
  /// Total number of attempts (initial request + retries).
  ///
  /// Set to `1` to disable retries.
  pub max_attempts: usize,
  /// Base delay used for exponential backoff.
  pub backoff_base: Duration,
  /// Maximum delay between retries.
  pub backoff_cap: Duration,
  /// When true, honor `Retry-After` for retryable responses.
  ///
  /// `backoff_cap` still caps the computed exponential backoff, but `Retry-After` can exceed it.
  /// Any active render deadline (timeout) remains the final cap so we never sleep past the
  /// remaining budget.
  pub respect_retry_after: bool,
}

impl Default for HttpRetryPolicy {
  fn default() -> Self {
    Self {
      max_attempts: 6,
      backoff_base: Duration::from_millis(200),
      backoff_cap: Duration::from_secs(2),
      respect_retry_after: true,
    }
  }
}

fn retryable_http_status(status: u16) -> bool {
  is_transient_http_status(status)
}

fn is_transient_http_status(status: u16) -> bool {
  matches!(status, 202 | 429 | 500 | 502 | 503 | 504)
}

fn http_status_allows_empty_body(status: u16) -> bool {
  // Most 2xx responses are allowed to omit a response body, but we treat empty bodies as
  // suspicious by default because they often indicate a broken fetch (truncated connection,
  // wrong server, etc). The exceptions here are statuses where an empty body is explicitly
  // expected or common in practice.
  //
  // Note: 202 (Accepted) frequently returns an empty body and is used by some CDNs/API gateways
  // to indicate "try again later". We still treat it as transient/retryable, but the caller may
  // want to proceed even if retries are exhausted.
  //
  // For HTTP error statuses (>= 400), empty bodies are common and should be treated as a valid
  // response so higher-level code can key off the status code (and caching layers can persist the
  // failure deterministically) instead of surfacing a fetch-layer "empty body" error.
  matches!(status, 202 | 204 | 205 | 304) || (100..200).contains(&status) || status >= 400
}

const AKAMAI_TRACKING_PIXEL_PATH_NEEDLE: &[u8] = b"/akam/13/pixel_";

fn url_is_akamai_tracking_pixel(url: &str) -> bool {
  let Ok(parsed) = Url::parse(url) else {
    return false;
  };
  let path = parsed.path().as_bytes();
  if path.len() < AKAMAI_TRACKING_PIXEL_PATH_NEEDLE.len() {
    return false;
  }
  path
    .windows(AKAMAI_TRACKING_PIXEL_PATH_NEEDLE.len())
    .any(|window| window.eq_ignore_ascii_case(AKAMAI_TRACKING_PIXEL_PATH_NEEDLE))
}

fn should_substitute_akamai_pixel_empty_image_body(
  kind: FetchContextKind,
  url: &str,
  status: u16,
  headers: &HeaderMap,
) -> bool {
  if !matches!(kind, FetchContextKind::Image | FetchContextKind::ImageCors)
    || !(200..300).contains(&status)
    || !url_is_akamai_tracking_pixel(url)
  {
    return false;
  }

  // Only replace responses that are actually empty (or explicitly `Content-Length: 0`). If the
  // server claims a non-zero body, an empty read is more likely a broken/truncated transfer than
  // a deliberate tracking pixel response.
  if headers
    .get("content-length")
    .and_then(|h| h.to_str().ok())
    .and_then(|raw| raw.trim().parse::<u64>().ok())
    .is_some_and(|len| len > 0)
  {
    return false;
  }

  true
}

fn header_content_length_is_zero(headers: &HeaderMap) -> bool {
  headers
    .get("content-length")
    .and_then(|h| h.to_str().ok())
    .and_then(|raw| raw.trim().parse::<u64>().ok())
    .is_some_and(|len| len == 0)
}

fn http_response_allows_empty_body(
  kind: FetchContextKind,
  status: u16,
  headers: &HeaderMap,
) -> bool {
  if http_status_allows_empty_body(status) {
    return true;
  }

  // For error statuses (4xx/5xx), many servers legitimately return an empty body. Treating that as
  // a fetch error obscures the real root cause (`HTTP status <code>`) and prevents higher-level
  // code from surfacing the status+final_url via `ensure_http_success`.
  if status >= 400 {
    return true;
  }

  // Empty stylesheets are valid and used in practice (e.g. `https://www.debian.org/empty.css`),
  // but we still want to treat unexpected empty bodies as suspicious to catch truncation/corrupt
  // fetches. Only accept empty bodies for stylesheet requests when the server explicitly signals
  // an empty entity with `Content-Length: 0`.
  matches!(kind, FetchContextKind::Stylesheet | FetchContextKind::StylesheetCors)
    && (200..300).contains(&status)
    && header_content_length_is_zero(headers)
}

fn http_empty_body_is_error(status: u16, allows_empty_body: bool) -> bool {
  // Treat empty HTTP bodies as suspicious only for success/redirect statuses. For error statuses
  // (>=400), callers should surface the status code (via `ensure_http_success` or higher-level
  // handling) rather than masking it with an "empty body" diagnostic.
  status < 400 && !allows_empty_body
}

fn http_retry_logging_enabled() -> bool {
  static ENABLED: OnceLock<bool> = OnceLock::new();
  *ENABLED.get_or_init(|| {
    std::env::var("FASTR_HTTP_LOG_RETRIES")
      .ok()
      .map(|raw| {
        !matches!(
          raw.trim().to_ascii_lowercase().as_str(),
          "0" | "false" | "no" | "off"
        )
      })
      .unwrap_or(false)
  })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HttpBackendMode {
  Ureq,
  Reqwest,
  Curl,
  Auto,
}

fn http_backend_mode() -> HttpBackendMode {
  static MODE: OnceLock<HttpBackendMode> = OnceLock::new();
  *MODE.get_or_init(|| {
    let raw = std::env::var("FASTR_HTTP_BACKEND").ok().unwrap_or_default();
    let lowered = raw.trim().to_ascii_lowercase();
    match lowered.as_str() {
      "" | "auto" | "fallback" => HttpBackendMode::Auto,
      "ureq" | "rust" | "native" => HttpBackendMode::Ureq,
      "reqwest" => HttpBackendMode::Reqwest,
      "curl" => HttpBackendMode::Curl,
      _ => HttpBackendMode::Auto,
    }
  })
}

fn http_www_fallback_enabled() -> bool {
  static ENABLED: OnceLock<bool> = OnceLock::new();
  *ENABLED.get_or_init(|| {
    std::env::var("FASTR_HTTP_WWW_FALLBACK")
      .ok()
      .map(|raw| {
        !matches!(
          raw.trim().to_ascii_lowercase().as_str(),
          "0" | "false" | "no" | "off"
        )
      })
      .unwrap_or(true)
  })
}

fn should_fallback_to_curl(err: &Error) -> bool {
  let Error::Resource(resource) = err else {
    return false;
  };

  let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
  while let Some(current) = source {
    if let Some(io_err) = current.downcast_ref::<io::Error>() {
      if is_retryable_io_error(io_err) {
        return true;
      }
    }
    source = current.source();
  }

  // Inspect only the message (not the URL) so we don't accidentally fall back for domains that
  // happen to include keywords like "tls" in the hostname.
  let msg = resource.message.to_ascii_lowercase();
  if msg.contains("overall http timeout budget exceeded") {
    return false;
  }
  msg.contains("timeout")
    || msg.contains("timed out")
    // `reqwest` often surfaces connection failures as "error sending request for url (...)" without
    // including the underlying TLS/HTTP2 detail in the top-level message. Treat this as
    // fallback-worthy in `FASTR_HTTP_BACKEND=auto` so we can retry with the cURL backend, which is
    // frequently more compatible with real-world CDNs.
    || msg.contains("error sending request")
    || msg.contains("connection reset")
    || msg.contains("connection aborted")
    || msg.contains("broken pipe")
    || msg.contains("http2")
    || msg.contains("http/2")
    || msg.contains("h2")
    || msg.contains("tls")
    || msg.contains("ssl")
    || msg.contains("handshake")
    || msg.contains("certificate")
    || msg.contains("x509")
    || msg.contains("alpn")
    || msg.contains("alert")
}

fn error_looks_like_dns_failure(err: &Error) -> bool {
  let Error::Resource(resource) = err else {
    return false;
  };
  let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
  while let Some(current) = source {
    if let Some(io_err) = current.downcast_ref::<io::Error>() {
      // `std` maps common name-resolution failures (`EAI_NONAME`) to `NotFound`. Reqwest/hyper
      // sometimes surface these as a generic "error sending request" message, so inspect the error
      // chain instead of relying purely on the formatted message.
      if io_err.kind() == io::ErrorKind::NotFound {
        return true;
      }
    }
    source = current.source();
  }
  // Match on the error message (not the URL) so we don't trigger a fallback for domains that happen
  // to include keywords in their hostname.
  let msg = resource.message.to_ascii_lowercase();
  msg.contains("could not resolve host")
    || msg.contains("couldn't resolve host")
    || msg.contains("failed to resolve host")
    || msg.contains("no such host")
    || msg.contains("name resolution")
    || msg.contains("dns")
    || msg.contains("getaddrinfo")
}

fn http_www_fallback_url(url: &str) -> Option<String> {
  let mut parsed = Url::parse(url).ok()?;
  if !matches!(parsed.scheme(), "http" | "https") {
    return None;
  }

  let url::Host::Domain(domain) = parsed.host()? else {
    return None;
  };
  let domain_lower = domain.to_ascii_lowercase();
  if domain_lower.starts_with("www.") {
    return None;
  }
  // Avoid turning single-label hosts like `localhost` into `www.localhost`.
  if !domain_lower.contains('.') {
    return None;
  }

  let new_host = format!("www.{domain}");
  parsed.set_host(Some(&new_host)).ok()?;
  Some(parsed.to_string())
}

fn is_timeout_or_no_response_error(err: &Error) -> bool {
  let Error::Resource(resource_err) = err else {
    return false;
  };
  // If we received an HTTP status code, then we received an HTTP response (even if the body was
  // empty/truncated). The `www.` fallback is intended for cases where the origin never responds,
  // so keep it narrow.
  if resource_err.status.is_some() {
    return false;
  }

  let msg = resource_err.message.to_ascii_lowercase();
  if msg.contains("overall http timeout budget exceeded") {
    return false;
  }

  let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
  while let Some(current) = source {
    if let Some(io_err) = current.downcast_ref::<io::Error>() {
      if matches!(
        io_err.kind(),
        io::ErrorKind::TimedOut
          | io::ErrorKind::ConnectionReset
          | io::ErrorKind::ConnectionAborted
          | io::ErrorKind::NotConnected
          | io::ErrorKind::BrokenPipe
          | io::ErrorKind::UnexpectedEof
          | io::ErrorKind::WouldBlock
          | io::ErrorKind::Interrupted
      ) {
        return true;
      }
    }
    source = current.source();
  }

  msg.contains("timeout")
    || msg.contains("timed out")
    || msg.contains("no response")
    || msg.contains("empty reply")
    || msg.contains("connection reset")
    || msg.contains("connection aborted")
    || msg.contains("broken pipe")
    || msg.contains("unexpected eof")
    || msg.contains("no http headers")
}

fn rewrite_url_host_with_www_prefix(
  url: &str,
  destination: Option<FetchDestination>,
) -> Option<String> {
  let Ok(mut parsed) = Url::parse(url) else {
    return None;
  };
  if !matches!(parsed.scheme(), "http" | "https") {
    return None;
  }
  let profile = destination.unwrap_or_else(|| http_browser_request_profile_for_url(url));
  if !matches!(
    profile,
    FetchDestination::Document | FetchDestination::DocumentNoUser | FetchDestination::Iframe
  ) {
    return None;
  }

  let host = match parsed.host()? {
    url::Host::Domain(domain) => domain.to_string(),
    url::Host::Ipv4(_) | url::Host::Ipv6(_) => return None,
  };

  if host.len() >= 4 && host[..4].eq_ignore_ascii_case("www.") {
    return None;
  }

  let host = format!("www.{host}");
  parsed.set_host(Some(&host)).ok()?;
  Some(parsed.to_string())
}

fn log_http_retry(reason: &str, attempt: usize, max_attempts: usize, url: &str, backoff: Duration) {
  if !http_retry_logging_enabled() {
    return;
  }
  eprintln!(
    "http retry {attempt}/{max_attempts} {url}: {reason} (sleep {}ms)",
    backoff.as_millis()
  );
}

fn format_attempt_suffix(attempt: usize, max_attempts: usize) -> String {
  if max_attempts <= 1 {
    String::new()
  } else {
    format!(" (attempt {attempt}/{max_attempts})")
  }
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
  let value = headers.get("retry-after")?.to_str().ok()?.trim();
  if value.is_empty() {
    return None;
  }
  if let Ok(secs) = value.parse::<u64>() {
    return Some(Duration::from_secs(secs));
  }
  parse_http_date(value)
    .ok()
    .and_then(|when| when.duration_since(SystemTime::now()).ok())
}

fn hash_u64(input: &str) -> u64 {
  // 64-bit FNV-1a.
  let mut hash: u64 = 0xcbf29ce484222325;
  for &b in input.as_bytes() {
    hash ^= u64::from(b);
    hash = hash.wrapping_mul(0x100000001b3);
  }
  hash
}

fn pseudo_rand_u64(mut x: u64) -> u64 {
  // xorshift64*
  x ^= x >> 12;
  x ^= x << 25;
  x ^= x >> 27;
  x.wrapping_mul(0x2545F4914F6CDD1D)
}

fn jitter_duration(max: Duration, seed: u64) -> Duration {
  if max.is_zero() {
    return Duration::ZERO;
  }
  let max_ns = max.as_nanos();
  // Avoid division by zero / overflow in the modulo below.
  let denom = max_ns.saturating_add(1);
  let rand = pseudo_rand_u64(seed) as u128;
  let jitter_ns = rand % denom;
  let secs = (jitter_ns / 1_000_000_000) as u64;
  let nanos = (jitter_ns % 1_000_000_000) as u32;
  Duration::new(secs, nanos)
}

fn sleep_with_deadline(
  deadline: Option<&render_control::RenderDeadline>,
  stage: RenderStage,
  duration: Duration,
) -> std::result::Result<(), RenderError> {
  if duration.is_zero() {
    return Ok(());
  }

  let Some(deadline) = deadline.filter(|deadline| deadline.is_enabled()) else {
    thread::sleep(duration);
    return Ok(());
  };

  let mut remaining = duration;
  while !remaining.is_zero() {
    deadline.check(stage)?;
    let slice = remaining.min(Duration::from_millis(10));
    thread::sleep(slice);
    remaining = remaining.saturating_sub(slice);
  }
  Ok(())
}

fn compute_backoff(policy: &HttpRetryPolicy, retry_count: usize, url: &str) -> Duration {
  // Exponential backoff with "equal jitter":
  //   sleep = backoff/2 + rand(0..backoff/2)
  let retry_count = retry_count.max(1);
  let shift = (retry_count - 1).min(30) as u32;
  let factor = 1u32 << shift;
  let backoff = policy
    .backoff_base
    .checked_mul(factor)
    .unwrap_or(policy.backoff_cap)
    .min(policy.backoff_cap);
  let half = backoff / 2;
  let now = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_nanos() as u64)
    .unwrap_or(0);
  let seed = now ^ hash_u64(url) ^ (retry_count as u64).wrapping_mul(0x9E3779B97F4A7C15);
  half + jitter_duration(half, seed)
}

fn is_retryable_io_error(err: &io::Error) -> bool {
  matches!(
    err.kind(),
    io::ErrorKind::TimedOut
      | io::ErrorKind::ConnectionReset
      | io::ErrorKind::ConnectionAborted
      | io::ErrorKind::NotConnected
      | io::ErrorKind::BrokenPipe
      | io::ErrorKind::UnexpectedEof
      | io::ErrorKind::WouldBlock
      | io::ErrorKind::Interrupted
  )
}

fn is_retryable_ureq_error(err: &ureq::Error) -> bool {
  let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
  while let Some(current) = source {
    if let Some(io_err) = current.downcast_ref::<io::Error>() {
      if is_retryable_io_error(io_err) {
        return true;
      }
    }
    source = current.source();
  }

  // Fallback: match common transient error strings when we can't downcast.
  let msg = err.to_string().to_ascii_lowercase();
  msg.contains("timeout")
    || msg.contains("timed out")
    || msg.contains("connection reset")
    || msg.contains("connection aborted")
    || msg.contains("broken pipe")
    || msg.contains("temporary failure")
    || msg.contains("dns")
}

fn is_retryable_reqwest_error(err: &reqwest::Error) -> bool {
  if err.is_timeout() || err.is_connect() {
    return true;
  }

  let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
  while let Some(current) = source {
    if let Some(io_err) = current.downcast_ref::<io::Error>() {
      if is_retryable_io_error(io_err) {
        return true;
      }
    }
    source = current.source();
  }

  let msg = err.to_string().to_ascii_lowercase();
  msg.contains("timeout")
    || msg.contains("timed out")
    || msg.contains("connection reset")
    || msg.contains("connection aborted")
    || msg.contains("broken pipe")
    || msg.contains("temporary failure")
    || msg.contains("dns")
}

/// Allowed URL schemes for resource fetching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllowedSchemes {
  /// Whether `http://` URLs are permitted.
  pub http: bool,
  /// Whether `https://` URLs are permitted.
  pub https: bool,
  /// Whether `file://` URLs (and bare filesystem paths) are permitted.
  pub file: bool,
  /// Whether `data:` URLs are permitted.
  pub data: bool,
}

impl AllowedSchemes {
  /// Allow all supported schemes.
  pub const fn all() -> Self {
    Self {
      http: true,
      https: true,
      file: true,
      data: true,
    }
  }

  fn allows(&self, scheme: ResourceScheme) -> bool {
    match scheme {
      ResourceScheme::Http => self.http,
      ResourceScheme::Https => self.https,
      ResourceScheme::File | ResourceScheme::Relative => self.file,
      ResourceScheme::Data => self.data,
      ResourceScheme::Other => false,
    }
  }
}

#[derive(Debug)]
struct ResourceBudget {
  consumed: AtomicUsize,
}

impl ResourceBudget {
  fn new() -> Self {
    Self {
      consumed: AtomicUsize::new(0),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceScheme {
  Http,
  Https,
  File,
  Data,
  Relative,
  Other,
}

/// Policy controlling which resources may be fetched and how large they may be.
///
/// The policy is clonable; clones share the same underlying byte budget so limits apply across
/// wrapper fetchers (e.g., [`CachingFetcher`]) that share a policy instance.
#[derive(Debug, Clone)]
pub struct ResourcePolicy {
  /// Permitted URL schemes.
  pub allowed_schemes: AllowedSchemes,
  /// Maximum bytes to read per response. Defaults to 50MB.
  pub max_response_bytes: usize,
  /// Total bytes budget across all responses. `None` disables the budget.
  pub total_bytes_budget: Option<usize>,
  /// Per-request timeout.
  pub request_timeout: Duration,
  /// When true, treat [`ResourcePolicy::request_timeout`] as a total budget for the full HTTP
  /// fetch attempt sequence (including retries and backoff) when no render deadline is active.
  pub request_timeout_is_total_budget: bool,
  /// Maximum number of request attempts when following redirects.
  pub max_redirects: usize,
  /// Optional hostname allowlist applied to HTTP(S) requests.
  pub host_allowlist: Option<HashSet<String>>,
  /// Optional hostname denylist applied to HTTP(S) requests.
  pub host_denylist: Option<HashSet<String>>,
  budget: Arc<ResourceBudget>,
}

/// Alias for callers that prefer fetch-oriented terminology.
pub type FetchPolicy = ResourcePolicy;

impl Default for ResourcePolicy {
  fn default() -> Self {
    Self {
      allowed_schemes: AllowedSchemes::all(),
      max_response_bytes: 50 * 1024 * 1024,
      total_bytes_budget: None,
      request_timeout: Duration::from_secs(30),
      request_timeout_is_total_budget: false,
      max_redirects: 10,
      host_allowlist: None,
      host_denylist: None,
      budget: Arc::new(ResourceBudget::new()),
    }
  }
}

impl ResourcePolicy {
  /// Create a new policy with defaults matching the historical behavior.
  pub fn new() -> Self {
    Self::default()
  }

  /// Override allowed schemes.
  pub fn with_allowed_schemes(mut self, allowed: AllowedSchemes) -> Self {
    self.allowed_schemes = allowed;
    self
  }

  /// Allow or disallow `http://` URLs.
  pub fn allow_http(mut self, allow: bool) -> Self {
    self.allowed_schemes.http = allow;
    self
  }

  /// Allow or disallow `https://` URLs.
  pub fn allow_https(mut self, allow: bool) -> Self {
    self.allowed_schemes.https = allow;
    self
  }

  /// Allow or disallow `file://` URLs and bare filesystem paths.
  pub fn allow_file(mut self, allow: bool) -> Self {
    self.allowed_schemes.file = allow;
    self
  }

  /// Allow or disallow `data:` URLs.
  pub fn allow_data(mut self, allow: bool) -> Self {
    self.allowed_schemes.data = allow;
    self
  }

  /// Override the maximum response size for a single resource.
  pub fn with_max_response_bytes(mut self, max: usize) -> Self {
    self.max_response_bytes = max;
    self
  }

  /// Override the total byte budget across all fetched resources.
  pub fn with_total_bytes_budget(mut self, budget: Option<usize>) -> Self {
    self.total_bytes_budget = budget;
    self.budget = Arc::new(ResourceBudget::new());
    self
  }

  /// Override the per-request timeout.
  pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
    self.request_timeout = timeout;
    self
  }

  /// Interpret [`ResourcePolicy::request_timeout`] as a total budget for retries/backoff when no
  /// render deadline is active.
  pub fn with_request_timeout_total_budget(mut self, enabled: bool) -> Self {
    self.request_timeout_is_total_budget = enabled;
    self
  }

  /// Override the maximum number of request attempts when following redirects.
  ///
  /// This is the number of HTTP requests allowed per fetch, including the initial request.
  pub fn with_max_redirects(mut self, max_redirects: usize) -> Self {
    self.max_redirects = max_redirects.max(1);
    self
  }

  /// Restrict fetches to the provided hostnames (case-insensitive).
  pub fn with_host_allowlist<I, S>(mut self, hosts: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    self.host_allowlist = normalize_hosts(hosts);
    self
  }

  /// Deny fetches to the provided hostnames (case-insensitive).
  pub fn with_host_denylist<I, S>(mut self, hosts: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    self.host_denylist = normalize_hosts(hosts);
    self
  }

  fn remaining_budget(&self) -> Option<usize> {
    self
      .total_bytes_budget
      .map(|limit| limit.saturating_sub(self.budget.consumed.load(Ordering::Relaxed)))
  }

  fn reserve_budget(&self, amount: usize) -> Result<()> {
    if let Some(limit) = self.total_bytes_budget {
      let mut current = self.budget.consumed.load(Ordering::Relaxed);
      loop {
        let next = current.saturating_add(amount);
        if next > limit {
          return Err(policy_error(format!(
            "total bytes budget exceeded ({} of {} bytes)",
            next, limit
          )));
        }
        match self.budget.consumed.compare_exchange(
          current,
          next,
          Ordering::SeqCst,
          Ordering::SeqCst,
        ) {
          Ok(_) => break,
          Err(actual) => current = actual,
        }
      }
    }
    Ok(())
  }

  fn ensure_url_allowed(&self, url: &str) -> Result<ResourceScheme> {
    if url.trim().is_empty() {
      return Err(policy_error("empty URL"));
    }
    let scheme = classify_scheme(url);
    if !self.allowed_schemes.allows(scheme) {
      return Err(policy_error(format!("scheme {:?} is not allowed", scheme)));
    }

    if matches!(scheme, ResourceScheme::Http | ResourceScheme::Https) {
      let parsed = Url::parse(url).map_err(|e| policy_error(format!("invalid URL: {e}")))?;
      let host = parsed.host_str();
      self.check_host_lists(host)?;
    }

    Ok(scheme)
  }

  fn check_host_lists(&self, host: Option<&str>) -> Result<()> {
    let Some(host) = host.map(str::to_ascii_lowercase) else {
      if self.host_allowlist.is_some() {
        return Err(policy_error(
          "host missing and host allowlist is configured",
        ));
      }
      return Ok(());
    };

    if let Some(allowlist) = &self.host_allowlist {
      if !allowlist.contains(&host) {
        return Err(policy_error(format!("host {host} is not in the allowlist")));
      }
    }

    if let Some(denylist) = &self.host_denylist {
      if denylist.contains(&host) {
        return Err(policy_error(format!("host {host} is denied")));
      }
    }

    Ok(())
  }

  fn allowed_response_limit(&self) -> Result<usize> {
    if self.max_response_bytes == 0 {
      return Err(policy_error("max_response_bytes is zero"));
    }

    let limit = match self.remaining_budget() {
      Some(0) => {
        return Err(policy_error("total bytes budget exhausted"));
      }
      Some(remaining) => remaining.min(self.max_response_bytes),
      None => self.max_response_bytes,
    };

    Ok(limit)
  }
}

fn normalize_hosts<I, S>(hosts: I) -> Option<HashSet<String>>
where
  I: IntoIterator<Item = S>,
  S: Into<String>,
{
  let normalized: HashSet<String> = hosts
    .into_iter()
    .map(|h| h.into().to_ascii_lowercase())
    .filter(|h| !h.is_empty())
    .collect();
  if normalized.is_empty() {
    None
  } else {
    Some(normalized)
  }
}

/// Returns true if the provided URL starts with the `data:` scheme (case-insensitive).
pub fn is_data_url(url: &str) -> bool {
  url
    .as_bytes()
    .get(.."data:".len())
    .map(|prefix| prefix.eq_ignore_ascii_case(b"data:"))
    .unwrap_or(false)
}

fn classify_scheme(url: &str) -> ResourceScheme {
  let bytes = url.as_bytes();
  if is_data_url(url) {
    return ResourceScheme::Data;
  }
  if bytes
    .get(.."file://".len())
    .map(|prefix| prefix.eq_ignore_ascii_case(b"file://"))
    .unwrap_or(false)
  {
    return ResourceScheme::File;
  }
  if bytes
    .get(.."http://".len())
    .map(|prefix| prefix.eq_ignore_ascii_case(b"http://"))
    .unwrap_or(false)
  {
    return ResourceScheme::Http;
  }
  if bytes
    .get(.."https://".len())
    .map(|prefix| prefix.eq_ignore_ascii_case(b"https://"))
    .unwrap_or(false)
  {
    return ResourceScheme::Https;
  }

  match Url::parse(url) {
    Ok(parsed) => match parsed.scheme() {
      "http" => ResourceScheme::Http,
      "https" => ResourceScheme::Https,
      "file" => ResourceScheme::File,
      "data" => ResourceScheme::Data,
      _ => ResourceScheme::Other,
    },
    Err(_) => ResourceScheme::Relative,
  }
}

fn policy_error(reason: impl Into<String>) -> Error {
  Error::Other(format!("fetch blocked by policy: {}", reason.into()))
}

/// Strip a leading "User-Agent:" prefix so logs don't double-prefix when callers
/// pass a full header value. Case-insensitive and trims surrounding whitespace after the prefix.
pub fn normalize_user_agent_for_log(ua: &str) -> &str {
  const PREFIX: &str = "user-agent:";
  if ua
    .get(..PREFIX.len())
    .map(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
    .unwrap_or(false)
  {
    let trimmed = ua[PREFIX.len()..].trim();
    if !trimmed.is_empty() {
      return trimmed;
    }
  }
  ua
}

// ============================================================================
// Core types
// ============================================================================

/// Parsed HTTP caching policy extracted from response headers.
#[derive(Debug, Clone, Default)]
pub struct HttpCachePolicy {
  pub max_age: Option<u64>,
  pub no_cache: bool,
  pub no_store: bool,
  pub must_revalidate: bool,
  pub expires: Option<SystemTime>,
}

impl HttpCachePolicy {
  fn is_empty(&self) -> bool {
    self.max_age.is_none()
      && !self.no_cache
      && !self.no_store
      && !self.must_revalidate
      && self.expires.is_none()
  }
}

#[derive(Debug, Clone)]
struct CachedHttpMetadata {
  stored_at: SystemTime,
  max_age: Option<Duration>,
  expires: Option<SystemTime>,
  no_cache: bool,
  no_store: bool,
  must_revalidate: bool,
}

impl CachedHttpMetadata {
  fn from_policy(policy: &HttpCachePolicy, stored_at: SystemTime) -> Option<Self> {
    if policy.is_empty() {
      return None;
    }
    Some(Self {
      stored_at,
      max_age: policy.max_age.map(Duration::from_secs),
      expires: policy.expires,
      no_cache: policy.no_cache,
      no_store: policy.no_store,
      must_revalidate: policy.must_revalidate,
    })
  }

  fn with_updated_policy(
    &self,
    policy: Option<&HttpCachePolicy>,
    stored_at: SystemTime,
  ) -> Option<Self> {
    match policy {
      Some(policy) if policy.no_store => None,
      Some(policy) => CachedHttpMetadata::from_policy(policy, stored_at),
      None => Some(Self {
        stored_at,
        ..self.clone()
      }),
    }
  }

  fn requires_revalidation(&self) -> bool {
    self.no_cache || self.must_revalidate
  }

  fn expires_at(&self) -> Option<SystemTime> {
    if let Some(max_age) = self.max_age {
      return self.stored_at.checked_add(max_age);
    }
    self.expires
  }

  fn is_fresh(&self, now: SystemTime, freshness_cap: Option<Duration>) -> bool {
    let mut expires_at = self.expires_at();
    if let Some(cap) = freshness_cap {
      if let Some(capped) = self.stored_at.checked_add(cap) {
        expires_at = match expires_at {
          Some(exp) if exp < capped => Some(exp),
          _ => Some(capped),
        };
      }
    }
    expires_at.map(|t| t > now).unwrap_or(false)
  }
}

/// Result of fetching an external resource
#[derive(Debug, Clone)]
pub struct FetchedResource {
  /// Raw bytes of the resource
  pub bytes: Vec<u8>,
  /// Content-Type header value, if available (e.g., "image/png", "text/css")
  pub content_type: Option<String>,
  /// Whether `X-Content-Type-Options: nosniff` was present on the response.
  ///
  /// Real browsers enforce stricter MIME checks for certain destinations (notably stylesheets)
  /// when `nosniff` is set; we preserve the bit so offline replays (disk cache / bundles) can
  /// apply the same behavior deterministically.
  pub nosniff: bool,
  /// Content-Encoding header value, if available (e.g., "br", "gzip").
  ///
  /// Note: the bytes in [`FetchedResource::bytes`] are returned *after* any content encoding has
  /// been decoded by the fetcher. This field exists purely for diagnostics so callers can surface
  /// actionable errors when decoders reject the response body.
  pub content_encoding: Option<String>,
  /// HTTP status code when fetched over HTTP(S)
  pub status: Option<u16>,
  /// HTTP ETag header (weak or strong) when present
  pub etag: Option<String>,
  /// HTTP Last-Modified header when present
  pub last_modified: Option<String>,
  /// Access-Control-Allow-Origin response header value when fetched over HTTP(S).
  ///
  /// Stored for downstream CORS enforcement (e.g. web fonts, `<img crossorigin>`).
  pub access_control_allow_origin: Option<String>,
  /// Timing-Allow-Origin response header value when fetched over HTTP(S).
  pub timing_allow_origin: Option<String>,
  /// Normalized `Vary` response header value when fetched over HTTP(S).
  ///
  /// - Header names are lowercased, sorted, and de-duplicated.
  /// - Multiple `Vary` headers and comma-separated values are coalesced.
  /// - `Some("*")` represents `Vary: *` which must be treated as uncacheable.
  pub vary: Option<String>,
  /// Parsed referrer policy from the `Referrer-Policy` response header, when present.
  ///
  /// This is primarily relevant for HTML documents: the renderer uses it as the initial
  /// [`crate::api::ResourceContext::referrer_policy`] until overridden by `<meta name="referrer">`.
  pub response_referrer_policy: Option<ReferrerPolicy>,
  /// Whether `Access-Control-Allow-Credentials: true` was present on the response.
  pub access_control_allow_credentials: bool,
  /// The final URL after redirects, when available
  pub final_url: Option<String>,
  /// Parsed HTTP caching policy when fetched over HTTP(S).
  pub cache_policy: Option<HttpCachePolicy>,
  /// Raw list of HTTP response headers when fetched over HTTP(S).
  ///
  /// - Stored as a flat list so duplicates (notably `Set-Cookie`) are preserved.
  /// - Header names are case-insensitive; helpers perform ASCII-insensitive matching.
  /// - Non-HTTP resources (e.g. `file://`, `data:`) set this to `None`.
  pub response_headers: Option<Vec<(String, String)>>,
}

/// Parsed metadata stored alongside cached HTML documents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CachedHtmlMetadata {
  pub content_type: Option<String>,
  pub url: Option<String>,
  pub status: Option<u16>,
  /// Parsed referrer policy from the `Referrer-Policy` response header, when available.
  pub response_referrer_policy: Option<ReferrerPolicy>,
}

impl FetchedResource {
  /// Create a new FetchedResource
  pub fn new(bytes: Vec<u8>, content_type: Option<String>) -> Self {
    Self {
      bytes,
      content_type,
      nosniff: false,
      content_encoding: None,
      status: None,
      etag: None,
      last_modified: None,
      access_control_allow_origin: None,
      timing_allow_origin: None,
      vary: None,
      response_referrer_policy: None,
      access_control_allow_credentials: false,
      final_url: None,
      cache_policy: None,
      response_headers: None,
    }
  }

  /// Create a new FetchedResource while recording the final URL.
  pub fn with_final_url(
    bytes: Vec<u8>,
    content_type: Option<String>,
    final_url: Option<String>,
  ) -> Self {
    Self {
      bytes,
      content_type,
      nosniff: false,
      content_encoding: None,
      status: None,
      etag: None,
      last_modified: None,
      access_control_allow_origin: None,
      timing_allow_origin: None,
      vary: None,
      response_referrer_policy: None,
      access_control_allow_credentials: false,
      final_url,
      cache_policy: None,
      response_headers: None,
    }
  }

  /// Returns true when this response represents a 304 Not Modified HTTP response.
  pub fn is_not_modified(&self) -> bool {
    matches!(self.status, Some(304))
  }

  /// Check if this resource appears to be an image based on content-type
  pub fn is_image(&self) -> bool {
    self
      .content_type
      .as_ref()
      .map(|ct| ct.starts_with("image/"))
      .unwrap_or(false)
  }

  /// Check if this resource appears to be CSS based on content-type
  pub fn is_css(&self) -> bool {
    self
      .content_type
      .as_ref()
      .map(|ct| ct.contains("text/css"))
      .unwrap_or(false)
  }

  /// Check if this resource appears to be SVG
  pub fn is_svg(&self) -> bool {
    self
      .content_type
      .as_ref()
      .map(|ct| ct.contains("image/svg"))
      .unwrap_or(false)
  }

  /// Return all response header values matching `name` (case-insensitive).
  ///
  /// Duplicates are preserved (e.g. multiple `Set-Cookie` headers).
  pub fn header_values<'a>(&'a self, name: &str) -> Vec<&'a str> {
    let Some(headers) = self.response_headers.as_ref() else {
      return Vec::new();
    };
    headers
      .iter()
      .filter(|(h_name, _)| h_name.eq_ignore_ascii_case(name))
      .map(|(_, value)| value.as_str())
      .collect()
  }

  /// Return the first matching response header value (case-insensitive).
  pub fn header_get(&self, name: &str) -> Option<&str> {
    self
      .response_headers
      .as_ref()?
      .iter()
      .find(|(h_name, _)| h_name.eq_ignore_ascii_case(name))
      .map(|(_, value)| value.as_str())
  }

  /// Return matching response header values joined with `", "` (case-insensitive).
  ///
  /// This matches the historical `header_values_joined` helper for `HeaderMap`.
  pub fn header_get_joined(&self, name: &str) -> Option<String> {
    let values = self.header_values(name);
    match values.as_slice() {
      [] => None,
      [value] => Some((*value).to_string()),
      _ => Some(values.join(", ")),
    }
  }
}

// ============================================================================
// Subresource response validation
// ============================================================================

/// Returns true if strict MIME sanity checks are enabled for HTTP subresources.
///
/// Controlled by `FASTR_FETCH_STRICT_MIME` (truthy/falsey). Defaults to `true`.
pub fn strict_mime_checks_enabled() -> bool {
  runtime::runtime_toggles().truthy_with_default("FASTR_FETCH_STRICT_MIME", true)
}

fn cors_cache_partitioning_enabled() -> bool {
  runtime::runtime_toggles().truthy_with_default("FASTR_FETCH_PARTITION_CORS_CACHE", true)
}

fn cors_origin_key_from_client_origin(client_origin: Option<&DocumentOrigin>) -> Option<String> {
  let client_origin = client_origin?;
  if client_origin.is_http_like() {
    http_browser_origin_and_referer_for_origin(client_origin).map(|(origin, _)| origin)
  } else {
    Some("null".to_string())
  }
}

fn cors_origin_key_from_referrer_url(referrer_url: Option<&str>) -> Option<String> {
  let referrer = referrer_url?;
  match Url::parse(referrer) {
    Ok(parsed) => match parsed.scheme() {
      "http" | "https" => {
        http_browser_origin_and_referer_for_url(&parsed).map(|(origin, _)| origin)
      }
      _ => Some("null".to_string()),
    },
    Err(_) => http_browser_tolerant_origin_from_url(referrer).and_then(|origin| {
      http_browser_origin_and_referer_for_origin(&origin).map(|(origin, _)| origin)
    }),
  }
}

fn cors_origin_key_for_request(req: &FetchRequest<'_>) -> Option<String> {
  if let Some(origin) = cors_origin_key_from_client_origin(req.client_origin) {
    return Some(origin);
  }
  if let Some(origin) = cors_origin_key_from_referrer_url(req.referrer_url) {
    return Some(origin);
  }
  // When a referrer was supplied but we could not derive a browser-style origin (invalid URL, etc),
  // match header generation behavior: no `Origin` header is emitted.
  if req.referrer_url.is_some() {
    return None;
  }

  match Url::parse(req.url) {
    Ok(parsed) => http_browser_origin_and_referer_for_url(&parsed).map(|(origin, _)| origin),
    Err(_) => http_browser_tolerant_origin_from_url(req.url).and_then(|origin| {
      http_browser_origin_and_referer_for_origin(&origin).map(|(origin, _)| origin)
    }),
  }
}

fn cors_cache_partition_key(req: &FetchRequest<'_>) -> Option<String> {
  if !cors_cache_partitioning_enabled() {
    return None;
  }
  if !http_browser_headers_enabled() {
    return None;
  }
  if req.destination.sec_fetch_mode() != "cors" {
    return None;
  }
  let origin = cors_origin_key_for_request(req)?;
  // Partition by credentials inclusion to avoid mixing anonymous vs credentialed responses.
  if cookies_allowed_for_request(req.credentials_mode, req.url, req.client_origin) {
    Some(format!("{origin}|cred=include"))
  } else {
    Some(origin)
  }
}

fn response_final_url(resource: &FetchedResource, requested_url: &str) -> String {
  resource
    .final_url
    .clone()
    .unwrap_or_else(|| requested_url.to_string())
}

fn response_resource_error(
  resource: &FetchedResource,
  requested_url: &str,
  message: impl Into<String>,
) -> Error {
  let mut err = ResourceError::new(requested_url.to_string(), message)
    .with_final_url(response_final_url(resource, requested_url))
    .with_validators(resource.etag.clone(), resource.last_modified.clone())
    .with_content_type(resource.content_type.clone());
  if let Some(status) = resource.status {
    err = err.with_status(status);
  }
  Error::Resource(err)
}

fn content_type_mime(content_type: &str) -> &str {
  content_type
    .split(';')
    .next()
    .unwrap_or(content_type)
    .trim()
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
  value
    .get(..prefix.len())
    .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn mime_is_html(mime: &str) -> bool {
  let mime = mime.trim();
  starts_with_ignore_ascii_case(mime, "text/html")
    || starts_with_ignore_ascii_case(mime, "application/xhtml+xml")
}

fn mime_is_svg(mime: &str) -> bool {
  starts_with_ignore_ascii_case(mime.trim(), "image/svg")
}

fn url_looks_like_suffix(url: &str, suffix: &str) -> bool {
  let url = url_without_query_fragment(url);
  ends_with_ignore_ascii_case(url, suffix)
}

fn url_without_query_fragment(url: &str) -> &str {
  let url = url.trim();
  let url = url.split_once('#').map(|(before, _)| before).unwrap_or(url);
  url.split_once('?').map(|(before, _)| before).unwrap_or(url)
}

fn ends_with_ignore_ascii_case(value: &str, suffix: &str) -> bool {
  let Some(tail) = value
    .len()
    .checked_sub(suffix.len())
    .and_then(|idx| value.get(idx..))
  else {
    return false;
  };
  tail.eq_ignore_ascii_case(suffix)
}

fn url_looks_like_html(url: &str) -> bool {
  url_looks_like_suffix(url, ".html") || url_looks_like_suffix(url, ".htm")
}

pub(crate) fn url_looks_like_image_asset(url: &str) -> bool {
  let url = url_without_query_fragment(url);
  [
    ".png", ".apng", ".gif", ".jpg", ".jpeg", ".webp", ".avif", ".bmp", ".ico", ".tif", ".tiff",
    ".svg", ".svgz", ".jxl", ".heic", ".heif",
  ]
  .into_iter()
  .any(|suffix| ends_with_ignore_ascii_case(url, suffix))
}

fn url_looks_like_stylesheet_asset(url: &str) -> bool {
  url_looks_like_suffix(url, ".css")
}

fn url_looks_like_font_asset(url: &str) -> bool {
  let url = url_without_query_fragment(url);
  [".woff2", ".woff", ".ttf", ".otf", ".eot"]
    .into_iter()
    .any(|suffix| ends_with_ignore_ascii_case(url, suffix))
}

fn url_looks_like_svg_or_html(url: &str) -> bool {
  url_looks_like_suffix(url, ".svg")
    || url_looks_like_suffix(url, ".svgz")
    || url_looks_like_html(url)
}

/// Ensures an HTTP response represents a successful fetch for a subresource.
///
/// `HttpFetcher` intentionally returns `Ok(FetchedResource)` for many non-2xx responses so that
/// higher-level code can decide how to handle HTTP errors. For subresources (images, fonts,
/// stylesheets), we generally want to surface the HTTP status and final URL as a fetch error
/// instead of attempting to decode/parse an HTML error page.
pub fn ensure_http_success(resource: &FetchedResource, requested_url: &str) -> Result<()> {
  let Some(code) = resource.status else {
    return Ok(());
  };
  if code < 400 {
    return Ok(());
  }
  Err(response_resource_error(
    resource,
    requested_url,
    format!("HTTP status {code}"),
  ))
}

/// Enforce Chromium-like CORS checks based on the `Access-Control-Allow-Origin` response header.
///
/// This helper is intentionally small and reusable: callers supply the initiating document origin,
/// the fetched resource (including any final URL after redirects), and the originally requested URL.
///
/// Behavior summary:
/// - If the request origin is missing/unparseable: allow (skip enforcement).
/// - If the response is same-origin: allow.
/// - If the response is cross-origin:
///   - allow `Access-Control-Allow-Origin: *`
///   - allow an `Access-Control-Allow-Origin` origin that matches the request origin
///   - otherwise reject with the caller-provided error mapping.
///
/// Note: CORS enforcement only applies to HTTP(S) origins; non-HTTP schemes (e.g. `data:`) are
/// always allowed because there is no response header surface to validate.
pub fn ensure_cors_allows_origin_with<E>(
  request_origin: Option<&DocumentOrigin>,
  resource: &FetchedResource,
  requested_url: &str,
  map_error: impl FnOnce(String) -> E,
) -> std::result::Result<(), E> {
  let Some(request_origin) = request_origin else {
    return Ok(());
  };
  if !request_origin.is_http_like() {
    return Ok(());
  }

  let response_url = resource.final_url.as_deref().unwrap_or(requested_url);
  let parsed_response_url = match Url::parse(response_url) {
    Ok(parsed) => parsed,
    Err(_) => return Ok(()),
  };
  let response_origin = DocumentOrigin::from_parsed_url(&parsed_response_url);
  if !response_origin.is_http_like() {
    return Ok(());
  }

  if request_origin.same_origin(&response_origin) {
    return Ok(());
  }

  let header_value = resource
    .access_control_allow_origin
    .as_deref()
    .unwrap_or_default();
  let header_value = header_value
    .split(',')
    .map(|v| v.trim())
    .find(|v| !v.is_empty())
    .unwrap_or_default();

  if header_value == "*" {
    return Ok(());
  }

  let parsed_allowed_origin = match Url::parse(header_value) {
    Ok(parsed) => parsed,
    Err(_) => {
      let message = if header_value.is_empty() {
        "blocked by CORS: missing Access-Control-Allow-Origin header".to_string()
      } else {
        format!(
          "blocked by CORS: invalid Access-Control-Allow-Origin header value {header_value:?}"
        )
      };
      return Err(map_error(message));
    }
  };
  let allowed_origin = DocumentOrigin::from_parsed_url(&parsed_allowed_origin);
  if request_origin.same_origin(&allowed_origin) {
    return Ok(());
  }

  Err(map_error(format!(
    "blocked by CORS: Access-Control-Allow-Origin {header_value:?} does not match request origin {request_origin}"
  )))
}

/// Enforce Chromium-like CORS checks for cross-origin resources.
///
/// This is a convenience wrapper around [`ensure_cors_allows_origin_with`] that returns an
/// [`Error::Resource`] on failure.
pub fn ensure_cors_allows_origin(
  request_origin: Option<&DocumentOrigin>,
  resource: &FetchedResource,
  requested_url: &str,
) -> Result<()> {
  ensure_cors_allows_origin_with(request_origin, resource, requested_url, |message| {
    response_resource_error(resource, requested_url, message)
  })
}

/// Best-effort MIME sanity check for fetched images.
///
/// When enabled, prevents common bot-mitigation HTML responses from being fed into image decoders,
/// surfacing a `ResourceError` instead.
pub fn ensure_image_mime_sane(resource: &FetchedResource, requested_url: &str) -> Result<()> {
  if !strict_mime_checks_enabled() || resource.status.is_none() {
    return Ok(());
  }
  if url_looks_like_svg_or_html(requested_url) {
    return Ok(());
  }
  if let Some(content_type) = resource.content_type.as_deref() {
    let mime = content_type_mime(content_type);
    if mime_is_html(mime) {
      return Err(response_resource_error(
        resource,
        requested_url,
        format!("unexpected content-type {mime}"),
      ));
    }
    if starts_with_ignore_ascii_case(mime, "text/plain") {
      return Err(response_resource_error(
        resource,
        requested_url,
        format!("unexpected content-type {mime}"),
      ));
    }
  }

  // Some bot mitigation pages respond to image requests with HTML markup but lie about the
  // response content-type (e.g. `image/png`). Detect obvious markup payloads for URLs that should
  // be images and surface them as resource errors.
  if file_payload_looks_like_markup_but_not_svg(&resource.bytes) {
    return Err(response_resource_error(
      resource,
      requested_url,
      "unexpected markup response body",
    ));
  }
  Ok(())
}

/// Best-effort MIME sanity check for fetched fonts.
pub fn ensure_font_mime_sane(resource: &FetchedResource, requested_url: &str) -> Result<()> {
  if !strict_mime_checks_enabled() || resource.status.is_none() {
    return Ok(());
  }

  if let Some(content_type) = resource.content_type.as_deref() {
    let mime = content_type_mime(content_type);
    if mime_is_html(mime) {
      return Err(response_resource_error(
        resource,
        requested_url,
        format!("unexpected content-type {mime}"),
      ));
    }
  }

  let final_url = resource.final_url.as_deref().unwrap_or(requested_url);
  if (url_looks_like_font_asset(requested_url) || url_looks_like_font_asset(final_url))
    && file_payload_looks_like_markup_but_not_svg(&resource.bytes)
  {
    return Err(response_resource_error(
      resource,
      requested_url,
      "unexpected markup response body",
    ));
  }
  Ok(())
}

/// Best-effort MIME sanity check for fetched stylesheets.
pub fn ensure_stylesheet_mime_sane(resource: &FetchedResource, requested_url: &str) -> Result<()> {
  if !strict_mime_checks_enabled() || resource.status.is_none() {
    return Ok(());
  }

  if resource.nosniff {
    let content_type = resource
      .content_type
      .as_deref()
      .map(str::trim)
      .filter(|ct| !ct.is_empty())
      .ok_or_else(|| {
        response_resource_error(
          resource,
          requested_url,
          "X-Content-Type-Options: nosniff blocked stylesheet with missing content-type",
        )
      })?;
    let mime = content_type_mime(content_type);
    if !mime.eq_ignore_ascii_case("text/css") {
      return Err(response_resource_error(
        resource,
        requested_url,
        format!(
          "X-Content-Type-Options: nosniff blocked stylesheet with unexpected content-type {mime}"
        ),
      ));
    }
  }

  if let Some(content_type) = resource.content_type.as_deref() {
    let mime = content_type_mime(content_type);
    if mime_is_html(mime) {
      return Err(response_resource_error(
        resource,
        requested_url,
        format!("unexpected content-type {mime}"),
      ));
    }
  }

  let final_url = resource.final_url.as_deref().unwrap_or(requested_url);
  if (url_looks_like_stylesheet_asset(requested_url) || url_looks_like_stylesheet_asset(final_url))
    && file_payload_looks_like_markup_but_not_svg(&resource.bytes)
  {
    return Err(response_resource_error(
      resource,
      requested_url,
      "unexpected markup response body",
    ));
  }
  Ok(())
}

/// Parses cached HTML metadata sidecars.
///
/// Supports the legacy format where the meta file contains only the content-type
/// string, and a key/value format where lines are prefixed with:
/// - `content-type:`
/// - `referrer-policy:` (from the `Referrer-Policy` response header)
/// - `status:`
/// - `url:`
pub fn parse_cached_html_meta(meta: &str) -> CachedHtmlMetadata {
  let trimmed = meta.trim();
  if trimmed.is_empty() {
    return CachedHtmlMetadata::default();
  }

  let mut parsed = CachedHtmlMetadata::default();

  for line in meta.lines() {
    let mut parts = line.splitn(2, ':');
    let key = parts.next().map(|s| s.trim().to_ascii_lowercase());
    let value = parts.next().map(|s| s.trim());
    match (key.as_deref(), value) {
      (Some("content-type"), Some(v)) if !v.is_empty() => parsed.content_type = Some(v.to_string()),
      (Some("referrer-policy"), Some(v)) if !v.is_empty() => {
        parsed.response_referrer_policy = ReferrerPolicy::parse_value_list(v)
      }
      (Some("url"), Some(v)) if !v.is_empty() => parsed.url = Some(v.to_string()),
      (Some("status"), Some(v)) => {
        if let Ok(code) = v.parse::<u16>() {
          parsed.status = Some(code);
        }
      }
      _ => {}
    }
  }

  if parsed.content_type.is_none()
    && parsed.response_referrer_policy.is_none()
    && parsed.url.is_none()
    && parsed.status.is_none()
    && !trimmed.contains('\n')
  {
    return CachedHtmlMetadata {
      content_type: Some(trimmed.to_string()),
      ..CachedHtmlMetadata::default()
    };
  }

  parsed
}

// ============================================================================
// ResourceFetcher trait
// ============================================================================

/// High-level request context for resource fetching.
///
/// This is intentionally defined in the `resource` layer (instead of reusing `api::ResourceKind`)
/// to avoid cyclic dependencies: low-level fetch/caching code must not depend on the public API
/// module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FetchContextKind {
  Document,
  /// Subframe document navigation (e.g. `<iframe src=...>`).
  Iframe,
  Stylesheet,
  /// Stylesheet fetched in CORS mode (e.g. `<link rel=stylesheet crossorigin>`).
  StylesheetCors,
  Image,
  /// Image fetched in CORS mode (e.g. `<img crossorigin>`).
  ImageCors,
  Font,
  Other,
}

impl FetchContextKind {
  const fn cache_id(self) -> u8 {
    match self {
      Self::Document => 0,
      Self::Stylesheet => 1,
      Self::Image => 2,
      Self::Font => 3,
      Self::Other => 4,
      Self::ImageCors => 5,
      Self::Iframe => 6,
      Self::StylesheetCors => 7,
    }
  }

  const fn as_str(self) -> &'static str {
    match self {
      Self::Document => "document",
      Self::Iframe => "iframe",
      Self::Stylesheet => "stylesheet",
      Self::StylesheetCors => "stylesheet-cors",
      Self::Image => "image",
      Self::ImageCors => "image-cors",
      Self::Font => "font",
      Self::Other => "other",
    }
  }
}

/// Auxiliary artifacts that can be stored in a disk-backed cache alongside fetched resources.
///
/// These are keyed by the tuple `(FetchContextKind, url)` (plus any disk cache namespace) but use a
/// different on-disk entry key than the primary resource bytes so they do not interfere with
/// `fetch()` / `fetch_partial()` semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheArtifactKind {
  /// Serialized image probe metadata (intrinsic dimensions, EXIF orientation, etc.).
  ///
  /// Used by [`crate::image_loader::ImageCache::probe`] to avoid re-fetching image bytes just to
  /// compute intrinsic sizes across repeated runs.
  ImageProbeMetadata,
}

impl From<FetchDestination> for FetchContextKind {
  fn from(value: FetchDestination) -> Self {
    match value {
      FetchDestination::Document => Self::Document,
      FetchDestination::DocumentNoUser => Self::Document,
      FetchDestination::Iframe => Self::Iframe,
      FetchDestination::Style => Self::Stylesheet,
      FetchDestination::StyleCors => Self::StylesheetCors,
      FetchDestination::Image => Self::Image,
      FetchDestination::ImageCors => Self::ImageCors,
      FetchDestination::Font => Self::Font,
      FetchDestination::Other => Self::Other,
      FetchDestination::Fetch => Self::Other,
    }
  }
}

impl From<FetchContextKind> for FetchDestination {
  fn from(value: FetchContextKind) -> Self {
    match value {
      FetchContextKind::Document => Self::Document,
      FetchContextKind::Iframe => Self::Iframe,
      FetchContextKind::Stylesheet => Self::Style,
      FetchContextKind::StylesheetCors => Self::StyleCors,
      FetchContextKind::Image => Self::Image,
      FetchContextKind::ImageCors => Self::ImageCors,
      FetchContextKind::Font => Self::Font,
      FetchContextKind::Other => Self::Other,
    }
  }
}

/// Trait for fetching external resources
///
/// This abstraction allows different fetch implementations:
/// - [`HttpFetcher`]: Default HTTP implementation with timeouts
/// - Custom implementations for caching, mocking, offline mode, etc.
///
/// URLs can be:
/// - `http://` or `https://` - fetch over network
/// - `file://` - read from filesystem
/// - `data:` - decode data URL inline
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync` to allow sharing across threads.
pub trait ResourceFetcher: Send + Sync {
  /// Fetch a resource from the given URL
  ///
  /// # Arguments
  ///
  /// * `url` - The URL to fetch (http://, https://, file://, or data:)
  ///
  /// # Returns
  ///
  /// Returns `Ok(FetchedResource)` containing the bytes and optional content-type,
  /// or an error if the fetch fails.
  fn fetch(&self, url: &str) -> Result<FetchedResource>;

  /// Fetch a resource with contextual request metadata (destination + referrer).
  ///
  /// HTTP implementations can use this to set browser-like request headers (e.g. `Accept`,
  /// `Sec-Fetch-*`, and `Referer`) that vary by resource type and initiating document.
  ///
  /// The default implementation delegates to [`ResourceFetcher::fetch`] using `req.url`.
  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    self.fetch(req.url)
  }

  /// Returns the value of a request header for the given fetch request, if known.
  ///
  /// This is used by caching layers to compute a deterministic variant key for `Vary`-aware cache
  /// indexing.
  ///
  /// Implementations that can deterministically construct their outbound HTTP request headers
  /// should return `Some(value)` (using an empty string to represent a header that would not be
  /// sent). Implementations that cannot determine the header value must return `None`; caching
  /// layers will treat responses that `Vary` on unknown headers as uncacheable to avoid poisoning.
  ///
  /// The default implementation returns `None`.
  fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
    let _ = (req, header_name);
    None
  }

  /// Fetch a resource with contextual request metadata and optional validators.
  ///
  /// This is primarily used by caching wrappers so they can issue conditional
  /// HTTP requests (`If-None-Match` / `If-Modified-Since`) while still providing
  /// the destination + referrer needed for browser-ish headers.
  ///
  /// The default implementation ignores the validators and delegates to
  /// [`ResourceFetcher::fetch_with_request`].
  fn fetch_with_request_and_validation(
    &self,
    req: FetchRequest<'_>,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    let _ = (etag, last_modified);
    self.fetch_with_request(req)
  }

  /// Fetch a resource with an explicit request context.
  ///
  /// Fetchers that vary request headers (e.g. `Accept`, `Sec-Fetch-*`, `Origin`, `Referer`) should
  /// use `kind` to select the appropriate header profile. The default implementation delegates to
  /// [`ResourceFetcher::fetch_with_request`] so any fetcher that overrides `fetch_with_request`
  /// automatically becomes context-aware.
  fn fetch_with_context(&self, kind: FetchContextKind, url: &str) -> Result<FetchedResource> {
    self.fetch_with_request(FetchRequest::new(url, kind.into()))
  }

  /// Fetch a prefix of a resource body.
  ///
  /// Returns up to the first `max_bytes` of the response body. Callers must treat truncated bodies
  /// as success (e.g. for probing headers/metadata).
  ///
  /// The default implementation delegates to [`ResourceFetcher::fetch_partial_with_context`] with
  /// [`FetchContextKind::Other`].
  fn fetch_partial(&self, url: &str, max_bytes: usize) -> Result<FetchedResource> {
    self.fetch_partial_with_context(FetchContextKind::Other, url, max_bytes)
  }

  /// Fetch a prefix of a resource body with an explicit request context.
  ///
  /// HTTP fetchers can use `kind` to set destination-specific headers (e.g. `Sec-Fetch-Dest`) even
  /// for partial/range requests. The default implementation delegates to
  /// [`ResourceFetcher::fetch_with_context`] and truncates the returned bytes.
  fn fetch_partial_with_context(
    &self,
    kind: FetchContextKind,
    url: &str,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    if max_bytes == 0 {
      let mut res = self.fetch_with_context(kind, url)?;
      res.bytes.clear();
      return Ok(res);
    }

    let mut res = self.fetch_with_context(kind, url)?;
    if res.bytes.len() > max_bytes {
      res.bytes.truncate(max_bytes);
    }
    Ok(res)
  }

  /// Fetch a prefix of a resource body using a contextual request (destination + referrer).
  ///
  /// This is the request-aware variant of [`ResourceFetcher::fetch_partial_with_context`]. Callers
  /// that need correct browser-like headers for partial/range requests (notably `Origin` for CORS
  /// mode) should prefer this API.
  ///
  /// The default implementation preserves legacy behavior for non-CORS-mode requests by delegating
  /// to [`ResourceFetcher::fetch_partial_with_context`]. When the request is in CORS mode and a
  /// referrer is available, it falls back to a full [`ResourceFetcher::fetch_with_request`] and
  /// truncates the returned body so the fetcher can still incorporate the referrer into request
  /// headers.
  fn fetch_partial_with_request(
    &self,
    req: FetchRequest<'_>,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    let kind: FetchContextKind = req.destination.into();
    if req.destination.sec_fetch_mode() == "cors"
      && (req.referrer_url.is_some() || req.client_origin.is_some())
    {
      if max_bytes == 0 {
        let mut res = self.fetch_with_request(req)?;
        res.bytes.clear();
        return Ok(res);
      }

      let mut res = self.fetch_with_request(req)?;
      if res.bytes.len() > max_bytes {
        res.bytes.truncate(max_bytes);
      }
      return Ok(res);
    }

    self.fetch_partial_with_context(kind, req.url, max_bytes)
  }

  /// Fetch a resource with optional cache validators for HTTP requests.
  ///
  /// Implementors can ignore the validators if they do not support conditional
  /// requests. The default implementation delegates to [`ResourceFetcher::fetch`].
  fn fetch_with_validation(
    &self,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    let _ = (etag, last_modified);
    self.fetch(url)
  }

  /// Fetch a resource with validators and an explicit request context.
  ///
  /// This is the context-aware variant of [`ResourceFetcher::fetch_with_validation`]. The default
  /// implementation delegates to `fetch_with_validation` to preserve compatibility.
  fn fetch_with_validation_and_context(
    &self,
    kind: FetchContextKind,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    let _ = kind;
    self.fetch_with_validation(url, etag, last_modified)
  }

  /// Read a cached auxiliary artifact blob for the given `(kind, url)` tuple.
  ///
  /// This is primarily implemented by disk-backed fetchers so higher-level callers can persist
  /// derived metadata (e.g. image probe results) without re-hitting the network across runs.
  ///
  /// The default implementation returns `None`.
  fn read_cache_artifact(
    &self,
    kind: FetchContextKind,
    url: &str,
    artifact: CacheArtifactKind,
  ) -> Option<FetchedResource> {
    let _ = (kind, url, artifact);
    None
  }

  /// Read a cached auxiliary artifact blob for the given request.
  ///
  /// This mirrors [`ResourceFetcher::read_cache_artifact`], but allows implementations to
  /// incorporate request metadata (e.g. the initiating document origin) into their cache keys.
  fn read_cache_artifact_with_request(
    &self,
    req: FetchRequest<'_>,
    artifact: CacheArtifactKind,
  ) -> Option<FetchedResource> {
    self.read_cache_artifact(req.destination.into(), req.url, artifact)
  }

  /// Persist an auxiliary artifact blob for the given `(kind, url)` tuple.
  ///
  /// Implementations should treat this as best-effort; failures must not surface to callers.
  ///
  /// `source` provides HTTP caching metadata (ETag/Last-Modified/Cache-Control) for staleness
  /// checks. Implementations may ignore it.
  ///
  /// The default implementation is a no-op.
  fn write_cache_artifact(
    &self,
    kind: FetchContextKind,
    url: &str,
    artifact: CacheArtifactKind,
    bytes: &[u8],
    source: Option<&FetchedResource>,
  ) {
    let _ = (kind, url, artifact, bytes, source);
  }

  /// Persist an auxiliary artifact blob for the given request.
  ///
  /// This mirrors [`ResourceFetcher::write_cache_artifact`] but allows implementations to
  /// incorporate request metadata (e.g. the initiating document origin) into their cache keys.
  fn write_cache_artifact_with_request(
    &self,
    req: FetchRequest<'_>,
    artifact: CacheArtifactKind,
    bytes: &[u8],
    source: Option<&FetchedResource>,
  ) {
    self.write_cache_artifact(req.destination.into(), req.url, artifact, bytes, source);
  }

  /// Remove a cached auxiliary artifact blob for the given `(kind, url)` tuple.
  ///
  /// The default implementation is a no-op.
  fn remove_cache_artifact(&self, kind: FetchContextKind, url: &str, artifact: CacheArtifactKind) {
    let _ = (kind, url, artifact);
  }

  /// Remove a cached auxiliary artifact blob for the given request.
  ///
  /// This mirrors [`ResourceFetcher::remove_cache_artifact`] but allows implementations to
  /// incorporate request metadata (e.g. the initiating document origin) into their cache keys.
  fn remove_cache_artifact_with_request(&self, req: FetchRequest<'_>, artifact: CacheArtifactKind) {
    self.remove_cache_artifact(req.destination.into(), req.url, artifact);
  }
}

// Allow Arc<dyn ResourceFetcher> to be used as ResourceFetcher
impl<T: ResourceFetcher + ?Sized> ResourceFetcher for Arc<T> {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    (**self).fetch(url)
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    (**self).fetch_with_request(req)
  }

  fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
    (**self).request_header_value(req, header_name)
  }

  fn fetch_with_request_and_validation(
    &self,
    req: FetchRequest<'_>,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    (**self).fetch_with_request_and_validation(req, etag, last_modified)
  }

  fn fetch_with_context(&self, kind: FetchContextKind, url: &str) -> Result<FetchedResource> {
    (**self).fetch_with_context(kind, url)
  }

  fn fetch_partial(&self, url: &str, max_bytes: usize) -> Result<FetchedResource> {
    (**self).fetch_partial(url, max_bytes)
  }

  fn fetch_partial_with_context(
    &self,
    kind: FetchContextKind,
    url: &str,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    (**self).fetch_partial_with_context(kind, url, max_bytes)
  }

  fn fetch_partial_with_request(
    &self,
    req: FetchRequest<'_>,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    (**self).fetch_partial_with_request(req, max_bytes)
  }

  fn fetch_with_validation(
    &self,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    (**self).fetch_with_validation(url, etag, last_modified)
  }

  fn fetch_with_validation_and_context(
    &self,
    kind: FetchContextKind,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    (**self).fetch_with_validation_and_context(kind, url, etag, last_modified)
  }

  fn read_cache_artifact(
    &self,
    kind: FetchContextKind,
    url: &str,
    artifact: CacheArtifactKind,
  ) -> Option<FetchedResource> {
    (**self).read_cache_artifact(kind, url, artifact)
  }

  fn read_cache_artifact_with_request(
    &self,
    req: FetchRequest<'_>,
    artifact: CacheArtifactKind,
  ) -> Option<FetchedResource> {
    (**self).read_cache_artifact_with_request(req, artifact)
  }

  fn write_cache_artifact(
    &self,
    kind: FetchContextKind,
    url: &str,
    artifact: CacheArtifactKind,
    bytes: &[u8],
    source: Option<&FetchedResource>,
  ) {
    (**self).write_cache_artifact(kind, url, artifact, bytes, source)
  }

  fn write_cache_artifact_with_request(
    &self,
    req: FetchRequest<'_>,
    artifact: CacheArtifactKind,
    bytes: &[u8],
    source: Option<&FetchedResource>,
  ) {
    (**self).write_cache_artifact_with_request(req, artifact, bytes, source)
  }

  fn remove_cache_artifact(&self, kind: FetchContextKind, url: &str, artifact: CacheArtifactKind) {
    (**self).remove_cache_artifact(kind, url, artifact)
  }

  fn remove_cache_artifact_with_request(&self, req: FetchRequest<'_>, artifact: CacheArtifactKind) {
    (**self).remove_cache_artifact_with_request(req, artifact)
  }
}

// ============================================================================
// HttpFetcher - Default implementation
// ============================================================================

/// Default HTTP resource fetcher
///
/// Fetches resources over HTTP/HTTPS with configurable timeouts and user agent.
/// Also handles `file://` URLs and `data:` URLs.
/// Clones share internal HTTP client state (`ureq::Agent` and `reqwest::blocking::Client`) so
/// connections and cookies can be reused.
///
/// The HTTP backend can be selected via `FASTR_HTTP_BACKEND=ureq|reqwest|curl|auto` (defaults to
/// `auto`).
///
/// # Example
///
/// ```rust,no_run
/// use fastrender::resource::HttpFetcher;
/// use std::time::Duration;
///
/// let fetcher = HttpFetcher::new()
///     .with_timeout(Duration::from_secs(60))
///     .with_user_agent("MyApp/1.0");
/// ```
#[derive(Clone)]
pub struct HttpFetcher {
  user_agent: String,
  accept_language: String,
  policy: ResourcePolicy,
  agent: Arc<ureq::Agent>,
  reqwest_client: Arc<reqwest_blocking::Client>,
  cookie_jar: Arc<ReqwestCookieJar>,
  retry_policy: HttpRetryPolicy,
}

#[derive(Clone, Copy, Debug)]
struct HttpCacheValidators<'a> {
  etag: Option<&'a str>,
  last_modified: Option<&'a str>,
}

fn http_referer_header_value(
  raw_referrer: &str,
  target_url: &str,
  policy: ReferrerPolicy,
) -> Option<String> {
  let policy = policy.resolve(ReferrerPolicy::CHROMIUM_DEFAULT);
  if policy == ReferrerPolicy::NoReferrer {
    return None;
  }
  // Guard against header injection: if the referrer URL contains raw control characters, treat it
  // as invalid and suppress the `Referer` header (matching browser behavior for invalid referrers).
  if raw_referrer
    .chars()
    .any(|c| matches!(c, '\r' | '\n' | '\0'))
  {
    return None;
  }

  // We only synthesize `Referer` for network referrers. For `file://` fixtures and other
  // opaque-origin schemes, browsers generally omit `Referer` while still sending `Origin: null`
  // where required (e.g. CORS-mode fetches).
  let parsed_referrer_url = Url::parse(raw_referrer).ok();
  let (referrer_origin, full_referrer, origin_only_referrer) = if let Some(referrer_url) =
    parsed_referrer_url.as_ref()
  {
    if !matches!(referrer_url.scheme(), "http" | "https") {
      return None;
    }
    let mut sanitized = referrer_url.clone();
    sanitized.set_fragment(None);
    // `Referer` must not leak credentials even when the initiating document URL contains them.
    // This matches the Referrer Policy spec's "strip URL for use as referrer" algorithm.
    let _ = sanitized.set_username("");
    let _ = sanitized.set_password(None);
    let full = sanitized.to_string();
    let origin_only = http_browser_origin_and_referer_for_url(&sanitized)
      .map(|(_, origin_only)| origin_only)
      .or_else(|| Some(full.clone()));
    (
      DocumentOrigin::from_parsed_url(&sanitized),
      Some(full),
      origin_only,
    )
  } else {
    let origin = http_browser_tolerant_origin_from_url(raw_referrer)?;
    let full = raw_referrer
      .split_once('#')
      .map(|(before, _)| before.to_string())
      .unwrap_or_else(|| raw_referrer.to_string());
    let full = http_normalize_referrer_url_tolerant(&full);
    let origin_only = http_browser_origin_and_referer_for_origin(&origin)
      .map(|(_, origin_only)| origin_only)
      .or_else(|| Some(full.clone()));
    (origin, Some(full), origin_only)
  };

  let parsed_target_url = Url::parse(target_url).ok();
  let target_origin = parsed_target_url
    .as_ref()
    .map(DocumentOrigin::from_parsed_url)
    .or_else(|| http_browser_tolerant_origin_from_url(target_url));

  let target_scheme = parsed_target_url
    .as_ref()
    .map(Url::scheme)
    .or_else(|| target_origin.as_ref().map(|origin| origin.scheme()));
  let downgrade = referrer_origin.scheme() == "https" && target_scheme == Some("http");

  let same_origin = target_origin
    .as_ref()
    .map(|target_origin| referrer_origin.same_origin(target_origin))
    .unwrap_or(false);

  let origin_only_value = origin_only_referrer.or_else(|| full_referrer.clone());
  let full_value = full_referrer;

  match policy {
    ReferrerPolicy::EmptyString => unreachable!("policy resolved above"),
    ReferrerPolicy::NoReferrer => None,
    ReferrerPolicy::NoReferrerWhenDowngrade => {
      if downgrade {
        None
      } else {
        full_value
      }
    }
    ReferrerPolicy::Origin => origin_only_value,
    ReferrerPolicy::OriginWhenCrossOrigin => {
      if same_origin {
        full_value
      } else {
        origin_only_value
      }
    }
    ReferrerPolicy::SameOrigin => {
      if same_origin {
        full_value
      } else {
        None
      }
    }
    ReferrerPolicy::StrictOrigin => {
      if downgrade {
        None
      } else {
        origin_only_value
      }
    }
    ReferrerPolicy::StrictOriginWhenCrossOrigin => {
      if same_origin {
        full_value
      } else if downgrade {
        None
      } else {
        origin_only_value
      }
    }
    ReferrerPolicy::UnsafeUrl => full_value,
  }
}

fn build_http_header_pairs<'a>(
  url: &str,
  user_agent: &str,
  accept_language: &str,
  accept_encoding: &str,
  validators: Option<HttpCacheValidators<'a>>,
  destination: FetchDestination,
  client_origin: Option<&DocumentOrigin>,
  referrer_url: Option<&str>,
  referrer_policy: ReferrerPolicy,
) -> Vec<(String, String)> {
  let parsed_target_url = Url::parse(url).ok();
  let parsed_referrer_url = referrer_url.and_then(|referrer| Url::parse(referrer).ok());
  let target_origin = parsed_target_url
    .as_ref()
    .map(DocumentOrigin::from_parsed_url)
    .or_else(|| http_browser_tolerant_origin_from_url(url));
  let referrer_origin = parsed_referrer_url
    .as_ref()
    .map(DocumentOrigin::from_parsed_url)
    .or_else(|| referrer_url.and_then(http_browser_tolerant_origin_from_url));

  let mut headers = vec![
    ("User-Agent".to_string(), user_agent.to_string()),
    ("Accept-Language".to_string(), accept_language.to_string()),
    ("Accept-Encoding".to_string(), accept_encoding.to_string()),
  ];

  if http_browser_headers_enabled() {
    let profile = destination;
    // When `FASTR_HTTP_BROWSER_HEADERS` is enabled we approximate Chromium's request headers.
    //
    // - `Sec-Fetch-Site` uses schemeful same-site (scheme + registrable domain), so sibling
    //   subdomains can be labelled `same-site` rather than `cross-site`.
    // - For `Referer`, callers can provide a full referrer URL and an explicit `ReferrerPolicy`.
    //   We apply the policy further down (fragment stripping, origin-only referrers for
    //   cross-origin requests when applicable, and HTTPS→HTTP downgrade suppression for strict
    //   policies).
    let sec_fetch_site = if let Some(client_origin) = client_origin {
      match target_origin.as_ref() {
        Some(target_origin) => {
          if client_origin.same_origin(target_origin) {
            "same-origin"
          } else if http_browser_schemeful_same_site_from_origins(client_origin, target_origin) {
            "same-site"
          } else {
            "cross-site"
          }
        }
        // If we have a client origin but cannot derive the target origin (invalid URL), treat the
        // request as cross-site to avoid incorrectly labelling cross-origin requests as same-origin.
        None => "cross-site",
      }
    } else if referrer_url.is_some() {
      match (referrer_origin.as_ref(), target_origin.as_ref()) {
        (Some(ref_origin), Some(target_origin)) => {
          if ref_origin.same_origin(target_origin) {
            "same-origin"
          } else if http_browser_schemeful_same_site_from_origins(ref_origin, target_origin) {
            "same-site"
          } else {
            "cross-site"
          }
        }
        // If we have a referrer but cannot derive the target origin (invalid URL), treat the
        // request as cross-site to avoid incorrectly labelling cross-origin requests as same-origin.
        (Some(_), None) => "cross-site",
        _ => profile.sec_fetch_site(),
      }
    } else {
      profile.sec_fetch_site()
    };
    headers.push(("Accept".to_string(), profile.accept().to_string()));
    headers.push((
      "Sec-Fetch-Dest".to_string(),
      profile.sec_fetch_dest().to_string(),
    ));
    headers.push((
      "Sec-Fetch-Mode".to_string(),
      profile.sec_fetch_mode().to_string(),
    ));
    headers.push(("Sec-Fetch-Site".to_string(), sec_fetch_site.to_string()));
    if let Some(user) = profile.sec_fetch_user() {
      headers.push(("Sec-Fetch-User".to_string(), user.to_string()));
    }
    if let Some(value) = profile.upgrade_insecure_requests() {
      headers.push(("Upgrade-Insecure-Requests".to_string(), value.to_string()));
    }
    if profile.sec_fetch_mode() == "cors" {
      if let Some(client_origin) = client_origin {
        if client_origin.is_http_like() {
          if let Some((origin, _)) = http_browser_origin_and_referer_for_origin(client_origin) {
            headers.push(("Origin".to_string(), origin));
          }
        } else {
          headers.push(("Origin".to_string(), "null".to_string()));
        }
      } else if referrer_url.is_some() {
        if let Some(referrer_url) = parsed_referrer_url.as_ref() {
          match referrer_url.scheme() {
            "http" | "https" => {
              if let Some((origin, _)) = http_browser_origin_and_referer_for_url(referrer_url) {
                headers.push(("Origin".to_string(), origin));
              }
            }
            // Non-HTTP(S) referrers (notably `file://` fixtures) use `Origin: null` for CORS-mode
            // subresource requests.
            _ => headers.push(("Origin".to_string(), "null".to_string())),
          }
        } else if let Some(referrer_origin) = referrer_origin.as_ref() {
          if let Some((origin, _)) = http_browser_origin_and_referer_for_origin(referrer_origin) {
            headers.push(("Origin".to_string(), origin));
          }
        }
      } else if profile == FetchDestination::Font {
        if let Some(parsed) = parsed_target_url.as_ref() {
          if let Some((origin, referer)) = profile.origin_and_referer(parsed) {
            headers.push(("Origin".to_string(), origin));
            headers.push(("Referer".to_string(), referer));
          }
        } else if let Some(target_origin) = target_origin.as_ref() {
          if let Some((origin, referer)) = http_browser_origin_and_referer_for_origin(target_origin)
          {
            headers.push(("Origin".to_string(), origin));
            headers.push(("Referer".to_string(), referer));
          }
        }
      } else if profile == FetchDestination::ImageCors || profile == FetchDestination::StyleCors {
        // For CORS-mode image/stylesheet requests, always send an `Origin` header. When a client
        // origin isn't available, fall back to using the target origin as a best-effort
        // approximation.
        if let Some(parsed) = parsed_target_url.as_ref() {
          if let Some((origin, _)) = profile.origin_and_referer(parsed) {
            headers.push(("Origin".to_string(), origin));
          }
        } else if let Some(target_origin) = target_origin.as_ref() {
          if let Some((origin, _)) = http_browser_origin_and_referer_for_origin(target_origin) {
            headers.push(("Origin".to_string(), origin));
          }
        }
      }
    }
  } else {
    headers.push(("Accept".to_string(), DEFAULT_ACCEPT.to_string()));
  }

  if let Some(raw_referrer) = referrer_url {
    let raw_referrer = raw_referrer.trim();
    if !raw_referrer.is_empty() {
      if let Some(value) = http_referer_header_value(raw_referrer, url, referrer_policy) {
        headers.push(("Referer".to_string(), value));
      }
    }
  }

  if let Some(v) = validators {
    if let Some(tag) = v.etag {
      headers.push(("If-None-Match".to_string(), tag.to_string()));
    }
    if let Some(modified) = v.last_modified {
      headers.push(("If-Modified-Since".to_string(), modified.to_string()));
    }
  }

  headers
}

impl std::fmt::Debug for HttpFetcher {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("HttpFetcher")
      .field("user_agent", &self.user_agent)
      .field("accept_language", &self.accept_language)
      .field("policy", &self.policy)
      .field("http_backend", &http_backend_mode())
      .field("retry_policy", &self.retry_policy)
      .finish()
  }
}

impl HttpFetcher {
  fn build_agent(policy: &ResourcePolicy) -> Arc<ureq::Agent> {
    let config = ureq::Agent::config_builder()
      .http_status_as_error(false)
      // Disable ureq's internal redirect following so we can:
      // - enforce `ResourcePolicy::max_redirects` consistently across backends
      // - observe redirect response headers (e.g. `Referrer-Policy`) for subsequent requests
      // - keep request header behaviour stable regardless of the selected HTTP backend
      .max_redirects(0)
      .timeout_global(Some(policy.request_timeout))
      .build();
    Arc::new(config.into())
  }

  fn build_reqwest_client() -> Arc<reqwest_blocking::Client> {
    Arc::new(
      reqwest_blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build reqwest HTTP client"),
    )
  }

  fn cookie_header_value(&self, url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let header = self.cookie_jar.cookies(&parsed)?;
    header.to_str().ok().map(|value| value.to_string())
  }

  fn store_cookies_from_headers(&self, url: &str, headers: &HeaderMap) {
    let Ok(parsed) = Url::parse(url) else {
      return;
    };
    for value in headers.get_all("set-cookie") {
      if let Ok(raw) = value.to_str() {
        self.cookie_jar.add_cookie_str(raw, &parsed);
      }
    }
  }

  fn rebuild_agent(&mut self) {
    self.agent = Self::build_agent(&self.policy);
  }

  /// Create a new HttpFetcher with default settings
  pub fn new() -> Self {
    Self::default()
  }

  /// Set the request timeout
  pub fn with_timeout(mut self, timeout: Duration) -> Self {
    self.policy.request_timeout = timeout;
    self.policy.request_timeout_is_total_budget = false;
    self.rebuild_agent();
    self
  }

  /// Set a total timeout budget for a single fetch call.
  ///
  /// This budget is shared across retries and backoff sleeps so a request cannot take
  /// `max_attempts × timeout` wall time in CLI tooling. When a render deadline is active, the
  /// effective per-attempt timeout is additionally clamped by the remaining render budget.
  pub fn with_timeout_budget(mut self, timeout: Duration) -> Self {
    self.policy.request_timeout = timeout;
    self.policy.request_timeout_is_total_budget = true;
    self.rebuild_agent();
    self
  }

  /// Set the User-Agent header
  pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Self {
    self.user_agent = user_agent.into();
    self
  }

  /// Set the Accept-Language header
  pub fn with_accept_language(mut self, accept_language: impl Into<String>) -> Self {
    self.accept_language = accept_language.into();
    self
  }

  /// Set the maximum response size in bytes
  pub fn with_max_size(mut self, max_size: usize) -> Self {
    self.policy.max_response_bytes = max_size;
    self
  }

  /// Apply a complete resource policy.
  pub fn with_policy(mut self, policy: ResourcePolicy) -> Self {
    self.policy = policy;
    self.rebuild_agent();
    self
  }

  /// Override the retry/backoff policy used for HTTP(S) fetches.
  pub fn with_retry_policy(mut self, policy: HttpRetryPolicy) -> Self {
    self.retry_policy = policy;
    self
  }

  /// Override the maximum redirect attempts.
  pub fn with_max_redirects(mut self, max_redirects: usize) -> Self {
    self.policy.max_redirects = max_redirects.max(1);
    self
  }

  fn timeout_budget(&self, deadline: &Option<render_control::RenderDeadline>) -> Option<Duration> {
    if !self.policy.request_timeout_is_total_budget || self.policy.request_timeout.is_zero() {
      return None;
    }

    let mut budget = self.policy.request_timeout;

    // When we're running under a cooperative render deadline, cap any per-fetch timeout budget so
    // we never claim a larger budget than the remaining deadline window.
    if let Some(deadline) = deadline.as_ref() {
      if deadline.timeout_limit().is_some() {
        budget = match deadline.remaining_timeout() {
          Some(remaining) => budget.min(remaining),
          None => Duration::ZERO,
        };
      }
    }

    Some(budget)
  }

  fn deadline_aware_timeout(
    &self,
    kind: FetchContextKind,
    deadline: Option<&render_control::RenderDeadline>,
    url: &str,
  ) -> Result<Option<Duration>> {
    let Some(deadline) = deadline else {
      return Ok(None);
    };
    if deadline.timeout_limit().is_none() {
      return Ok(None);
    }
    let Some(remaining) = deadline.remaining_timeout() else {
      return Err(Error::Render(RenderError::Timeout {
        stage: render_stage_hint_for_context(kind, url),
        elapsed: deadline.elapsed(),
      }));
    };
    if remaining <= HTTP_DEADLINE_BUFFER {
      return Err(Error::Resource(
        ResourceError::new(
          url.to_string(),
          "render deadline exceeded before HTTP request (timeout)",
        )
        .with_final_url(url.to_string()),
      ));
    }
    let budget = remaining
      .saturating_sub(HTTP_DEADLINE_BUFFER)
      .min(self.policy.request_timeout);
    Ok(Some(budget))
  }
 
  /// Fetch from an HTTP/HTTPS URL
  fn fetch_http(&self, kind: FetchContextKind, url: &str) -> Result<FetchedResource> {
    let destination: FetchDestination = kind.into();
    let default_credentials = FetchRequest::new(url, destination).credentials_mode;
    self.fetch_http_with_context(
      kind,
      destination,
      url,
      None,
      None,
      None,
      None,
      ReferrerPolicy::default(),
      default_credentials,
    )
  }

  fn fetch_http_partial(
    &self,
    kind: FetchContextKind,
    destination: FetchDestination,
    url: &str,
    max_bytes: usize,
    client_origin: Option<&DocumentOrigin>,
    referrer_url: Option<&str>,
    referrer_policy: ReferrerPolicy,
    credentials_mode: FetchCredentialsMode,
  ) -> Result<FetchedResource> {
    let deadline = render_control::root_deadline();
    let started = Instant::now();
    render_control::with_deadline(deadline.as_ref(), || {
      self.fetch_http_partial_inner(
        kind,
        destination,
        url,
        max_bytes,
        client_origin,
        referrer_url,
        referrer_policy,
        credentials_mode,
        &deadline,
        started,
      )
    })
  }

  fn fetch_http_with_context(
    &self,
    kind: FetchContextKind,
    destination: FetchDestination,
    url: &str,
    accept_encoding: Option<&str>,
    validators: Option<HttpCacheValidators<'_>>,
    client_origin: Option<&DocumentOrigin>,
    referrer_url: Option<&str>,
    referrer_policy: ReferrerPolicy,
    credentials_mode: FetchCredentialsMode,
  ) -> Result<FetchedResource> {
    let deadline = render_control::root_deadline();
    let started = Instant::now();
    render_control::with_deadline(deadline.as_ref(), || {
      self.fetch_http_with_context_inner(
        kind,
        destination,
        url,
        accept_encoding,
        validators,
        client_origin,
        referrer_url,
        referrer_policy,
        credentials_mode,
        &deadline,
        started,
      )
    })
  }

  fn fetch_http_with_context_inner<'a>(
    &self,
    kind: FetchContextKind,
    destination: FetchDestination,
    url: &str,
    accept_encoding: Option<&str>,
    validators: Option<HttpCacheValidators<'a>>,
    client_origin: Option<&DocumentOrigin>,
    referrer_url: Option<&str>,
    referrer_policy: ReferrerPolicy,
    credentials_mode: FetchCredentialsMode,
    deadline: &Option<render_control::RenderDeadline>,
    started: Instant,
  ) -> Result<FetchedResource> {
    let rewritten_url = rewrite_known_pageset_url(url);
    let mut effective_url = rewritten_url
      .map(Cow::Owned)
      .unwrap_or_else(|| Cow::Borrowed(url));

    let mut attempted_www_fallback = false;
    let mut www_fallback_error: Option<Error> = None;

    loop {
      render_control::check_active(render_stage_hint_for_context(kind, effective_url.as_ref()))
        .map_err(Error::Render)?;
      let result = match http_backend_mode() {
        HttpBackendMode::Curl => curl_backend::fetch_http_with_accept_inner(
          self,
          kind,
          destination,
          effective_url.as_ref(),
          accept_encoding,
          validators,
          client_origin,
          referrer_url,
          referrer_policy,
          credentials_mode,
          deadline,
          started,
        ),
        HttpBackendMode::Ureq => self.fetch_http_with_accept_inner_ureq(
          kind,
          destination,
          effective_url.as_ref(),
          accept_encoding,
          validators,
          client_origin,
          referrer_url,
          referrer_policy,
          credentials_mode,
          deadline,
          started,
          false,
        ),
        HttpBackendMode::Reqwest => self.fetch_http_with_accept_inner_reqwest(
          kind,
          destination,
          effective_url.as_ref(),
          accept_encoding,
          validators,
          client_origin,
          referrer_url,
          referrer_policy,
          credentials_mode,
          deadline,
          started,
          false,
        ),
        HttpBackendMode::Auto => {
          let curl_available = curl_backend::curl_available();
          let prefer_reqwest = effective_url
            .get(..8)
            .map(|prefix| prefix.eq_ignore_ascii_case("https://"))
            .unwrap_or(false);
          let result = if prefer_reqwest {
            self.fetch_http_with_accept_inner_reqwest(
              kind,
              destination,
              effective_url.as_ref(),
              accept_encoding,
              validators,
              client_origin,
              referrer_url,
              referrer_policy,
              credentials_mode,
              deadline,
              started,
              curl_available,
            )
          } else {
            self.fetch_http_with_accept_inner_ureq(
              kind,
              destination,
              effective_url.as_ref(),
              accept_encoding,
              validators,
              client_origin,
              referrer_url,
              referrer_policy,
              credentials_mode,
              deadline,
              started,
              curl_available,
            )
          };

          match result {
            Ok(res) => Ok(res),
            Err(err) => {
              if curl_available && should_fallback_to_curl(&err) {
                match curl_backend::fetch_http_with_accept_inner(
                  self,
                  kind,
                  destination,
                  effective_url.as_ref(),
                  accept_encoding,
                  validators,
                  client_origin,
                  referrer_url,
                  referrer_policy,
                  credentials_mode,
                  deadline,
                  started,
                ) {
                  Ok(res) => Ok(res),
                  Err(curl_err) => {
                    let mut err = err;
                    if let Error::Resource(ref mut res) = err {
                      res.message = format!("{}; curl fallback failed: {}", res.message, curl_err);
                    }
                    Err(err)
                  }
                }
              } else {
                Err(err)
              }
            }
          }
        }
      };

      match result {
        Ok(res) => return Ok(res),
        Err(err) => {
          if attempted_www_fallback {
            if let Some(mut original) = www_fallback_error.take() {
              match &mut original {
                Error::Resource(ref mut res) => {
                  res.message = format!("{}; www fallback failed: {}", res.message, err);
                }
                Error::Other(ref mut msg) => {
                  *msg = format!("{msg}; www fallback failed: {err}");
                }
                _ => {}
              }
              return Err(original);
            }
            return Err(err);
          }

          let fallback_url = if error_looks_like_dns_failure(&err) {
            http_www_fallback_url(effective_url.as_ref())
          } else if http_www_fallback_enabled() && is_timeout_or_no_response_error(&err) {
            rewrite_url_host_with_www_prefix(effective_url.as_ref(), Some(destination))
          } else {
            None
          };

          if let Some(url) = fallback_url {
            attempted_www_fallback = true;
            www_fallback_error = Some(err);
            effective_url = Cow::Owned(url);
            continue;
          }

          return Err(err);
        }
      }
    }
  }

  fn fetch_http_partial_inner(
    &self,
    kind: FetchContextKind,
    destination: FetchDestination,
    url: &str,
    max_bytes: usize,
    client_origin: Option<&DocumentOrigin>,
    referrer_url: Option<&str>,
    referrer_policy: ReferrerPolicy,
    credentials_mode: FetchCredentialsMode,
    deadline: &Option<render_control::RenderDeadline>,
    started: Instant,
  ) -> Result<FetchedResource> {
    let rewritten_url = rewrite_known_pageset_url(url);
    let effective_url = rewritten_url.as_deref().unwrap_or(url);
    let prefer_reqwest = effective_url
      .get(..8)
      .map(|prefix| prefix.eq_ignore_ascii_case("https://"))
      .unwrap_or(false);

    match http_backend_mode() {
      HttpBackendMode::Ureq => self.fetch_http_partial_inner_ureq(
        kind,
        destination,
        effective_url,
        max_bytes,
        client_origin,
        referrer_url,
        referrer_policy,
        credentials_mode,
        deadline,
        started,
      ),
      HttpBackendMode::Reqwest => self.fetch_http_partial_inner_reqwest(
        kind,
        destination,
        effective_url,
        max_bytes,
        client_origin,
        referrer_url,
        referrer_policy,
        credentials_mode,
        deadline,
        started,
      ),
      // The cURL backend shells out to the system binary and reads the entire response body before
      // returning. For partial probes we instead use the Rust backends, falling back to a full
      // `fetch()` when needed (the image probe path already does this).
      HttpBackendMode::Curl | HttpBackendMode::Auto => {
        if prefer_reqwest {
          self.fetch_http_partial_inner_reqwest(
            kind,
            destination,
            effective_url,
            max_bytes,
            client_origin,
            referrer_url,
            referrer_policy,
            credentials_mode,
            deadline,
            started,
          )
        } else {
          self.fetch_http_partial_inner_ureq(
            kind,
            destination,
            effective_url,
            max_bytes,
            client_origin,
            referrer_url,
            referrer_policy,
            credentials_mode,
            deadline,
            started,
          )
        }
      }
    }
  }

  fn fetch_http_partial_inner_ureq(
    &self,
    kind: FetchContextKind,
    destination: FetchDestination,
    url: &str,
    max_bytes: usize,
    client_origin: Option<&DocumentOrigin>,
    referrer_url: Option<&str>,
    referrer_policy: ReferrerPolicy,
    credentials_mode: FetchCredentialsMode,
    deadline: &Option<render_control::RenderDeadline>,
    started: Instant,
  ) -> Result<FetchedResource> {
    let mut current = url.to_string();
    let agent = &self.agent;
    let timeout_budget = self.timeout_budget(deadline);
    let mut effective_referrer_policy = referrer_policy;
    let mut redirect_referrer_policy: Option<ReferrerPolicy> = None;
    let deadline_retries_disabled = deadline.as_ref().is_some_and(|deadline| {
      deadline.timeout_limit().is_some() && !deadline.http_retries_enabled()
    });
    let max_attempts = if deadline_retries_disabled {
      1
    } else {
      self.retry_policy.max_attempts.max(1)
    };

    let budget_exhausted_error = |current_url: &str, attempt: usize| -> Error {
      let budget = timeout_budget.expect("budget mode should be active");
      let elapsed = started.elapsed();
      Error::Resource(
        ResourceError::new(
          current_url.to_string(),
          format!(
            "overall HTTP timeout budget exceeded (budget={budget:?}, elapsed={elapsed:?}){}",
            format_attempt_suffix(attempt, max_attempts)
          ),
        )
        .with_final_url(current_url.to_string()),
      )
    };

    'redirects: for _ in 0..self.policy.max_redirects {
      self.policy.ensure_url_allowed(&current)?;
      for attempt in 1..=max_attempts {
        self.policy.ensure_url_allowed(&current)?;
        let stage_hint = render_stage_hint_for_context(kind, &current);
        render_control::check_active(stage_hint).map_err(Error::Render)?;
        let allowed_limit = self.policy.allowed_response_limit()?;
        let read_limit = max_bytes.min(allowed_limit).max(1);
        let per_request_timeout = self.deadline_aware_timeout(kind, deadline.as_ref(), &current)?;
        let mut effective_timeout = per_request_timeout.unwrap_or(self.policy.request_timeout);

        if let Some(budget) = timeout_budget {
          match budget.checked_sub(started.elapsed()) {
            Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
              let budget_timeout = remaining.saturating_sub(HTTP_DEADLINE_BUFFER);
              effective_timeout = effective_timeout.min(budget_timeout);
            }
            _ => return Err(budget_exhausted_error(&current, attempt)),
          }
        }

        let mut headers = build_http_header_pairs(
          &current,
          &self.user_agent,
          &self.accept_language,
          // Partial responses + content encoding are a headache: the range is expressed over the
          // encoded representation and the prefix is often not independently decodable. Request
          // identity encoding so that `Range` applies directly to the image bytes we want to probe.
          "identity",
          None,
          destination,
          client_origin,
          referrer_url,
          effective_referrer_policy,
        );
        if cookies_allowed_for_request(credentials_mode, &current, client_origin) {
          if let Some(value) = self.cookie_header_value(&current) {
            headers.push(("Cookie".to_string(), value));
          }
        }
        headers.push((
          "Range".to_string(),
          format!("bytes=0-{}", read_limit.saturating_sub(1)),
        ));
        let mut request = agent.get(&current);
        for (name, value) in &headers {
          request = request.header(name, value);
        }

        if !effective_timeout.is_zero() {
          request = request
            .config()
            .timeout_global(Some(effective_timeout))
            .build();
        }

        let mut network_timer = start_network_fetch_diagnostics();
        let mut response = match request.call() {
          Ok(resp) => resp,
          Err(err) => {
            finish_network_fetch_diagnostics(network_timer.take());
            if attempt < max_attempts && is_retryable_ureq_error(&err) {
              let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
              let mut can_retry = true;
              if let Some(deadline) = deadline.as_ref() {
                if deadline.timeout_limit().is_some() {
                  match deadline.remaining_timeout() {
                    Some(remaining) if !remaining.is_zero() => {
                      let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }
              }
              if let Some(budget) = timeout_budget {
                match budget.checked_sub(started.elapsed()) {
                  Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                    let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                    backoff = backoff.min(max_sleep);
                  }
                  _ => can_retry = false,
                }
              }
              if can_retry {
                render_control::check_active(stage_hint).map_err(Error::Render)?;
                let reason = err.to_string();
                log_http_retry(&reason, attempt, max_attempts, &current, backoff);
                if !backoff.is_zero() {
                  sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                    .map_err(Error::Render)?;
                }
                continue;
              }
              if timeout_budget.is_some() {
                return Err(budget_exhausted_error(&current, attempt));
              }
            }

            let overall_timeout = deadline.as_ref().and_then(|d| d.timeout_limit());
            let mut message = err.to_string();
            let lower = message.to_ascii_lowercase();
            if lower.contains("timeout") || lower.contains("timed out") {
              if let Some(overall) = overall_timeout {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout={overall:?})"
                ));
              } else if let Some(budget) = timeout_budget {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout_budget={budget:?})"
                ));
              } else {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?})"
                ));
              }
            } else {
              message.push_str(&format_attempt_suffix(attempt, max_attempts));
            }

            let err = ResourceError::new(current.clone(), message)
              .with_final_url(current.clone())
              .with_source(err);
            return Err(Error::Resource(err));
          }
        };

        if cookies_allowed_for_request(credentials_mode, &current, client_origin) {
          self.store_cookies_from_headers(&current, response.headers());
        }

        let status = response.status();
        if (300..400).contains(&status.as_u16()) {
          if let Some(loc) = response
            .headers()
            .get("location")
            .and_then(|h| h.to_str().ok())
          {
            if let Some(policy) = header_values_joined(response.headers(), "referrer-policy")
              .as_deref()
              .and_then(ReferrerPolicy::parse_value_list)
            {
              effective_referrer_policy = policy;
              redirect_referrer_policy = Some(policy);
            }
            let next = Url::parse(&current)
              .ok()
              .and_then(|base| base.join(loc).ok())
              .map(|u| u.to_string())
              .unwrap_or_else(|| loc.to_string());
            finish_network_fetch_diagnostics(network_timer.take());
            render_control::check_active(render_stage_hint_for_context(kind, &next))
              .map_err(Error::Render)?;
            self.policy.ensure_url_allowed(&next)?;
            current = next;
            continue 'redirects;
          }
        }

        let status_code = status.as_u16();
        let retry_after =
          if self.retry_policy.respect_retry_after && retryable_http_status(status_code) {
            parse_retry_after(response.headers())
          } else {
            None
          };

        let encodings = parse_content_encodings(response.headers());
        if encodings.iter().any(|enc| enc != "identity") {
          finish_network_fetch_diagnostics(network_timer.take());
          let final_url = response.get_uri().to_string();
          let err = ResourceError::new(
            current.clone(),
            format!(
              "received content-encoding {:?} for partial fetch (expected identity)",
              encodings
            ),
          )
          .with_status(status_code)
          .with_final_url(final_url);
          return Err(Error::Resource(err));
        }

        let mut content_type = response
          .headers()
          .get("content-type")
          .and_then(|h| h.to_str().ok())
          .map(|s| s.to_string());
        let nosniff = header_has_nosniff(response.headers());
        let mut decode_stage = decode_stage_for_content_type(content_type.as_deref());
        let etag = response
          .headers()
          .get("etag")
          .and_then(|h| h.to_str().ok())
          .map(|s| s.to_string());
        let last_modified = response
          .headers()
          .get("last-modified")
          .and_then(|h| h.to_str().ok())
          .map(|s| s.to_string());
        let (access_control_allow_origin, access_control_allow_credentials) =
          parse_cors_response_headers(response.headers());
        let timing_allow_origin = header_values_joined(response.headers(), "timing-allow-origin");
        let response_referrer_policy = header_values_joined(response.headers(), "referrer-policy")
          .as_deref()
          .and_then(ReferrerPolicy::parse_value_list)
          .or(redirect_referrer_policy);
        let cache_policy = parse_http_cache_policy(response.headers());
        let vary = parse_vary_headers(response.headers());
        let final_url = response.get_uri().to_string();
        let response_headers = collect_response_headers(response.headers());
        let allows_empty_body =
          http_response_allows_empty_body(kind, status_code, response.headers());

        let substitute_empty_image_body =
          should_substitute_empty_image_body(kind, status_code, response.headers())
            || should_substitute_akamai_pixel_empty_image_body(
              kind,
              &final_url,
              status_code,
              response.headers(),
            );
        let substitute_captcha_image =
          should_substitute_captcha_image_response(kind, status_code, &final_url);
        let mut body_reader = response.body_mut().as_reader();
        let body_result = read_response_prefix(&mut body_reader, read_limit);
        match body_result {
          Ok(mut bytes) => {
            record_network_fetch_bytes(bytes.len());
            if bytes.is_empty() && substitute_empty_image_body {
              let take = OFFLINE_FIXTURE_PLACEHOLDER_PNG.len().min(read_limit);
              bytes = OFFLINE_FIXTURE_PLACEHOLDER_PNG[..take].to_vec();
              content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
              decode_stage = decode_stage_for_content_type(content_type.as_deref());
            }
            if should_substitute_markup_image_body(
              kind,
              url,
              &final_url,
              content_type.as_deref(),
              &bytes,
            ) {
              let take = OFFLINE_FIXTURE_PLACEHOLDER_PNG.len().min(read_limit);
              bytes = OFFLINE_FIXTURE_PLACEHOLDER_PNG[..take].to_vec();
              content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
              decode_stage = decode_stage_for_content_type(content_type.as_deref());
            }
            if substitute_captcha_image {
              let take = OFFLINE_FIXTURE_PLACEHOLDER_PNG.len().min(read_limit);
              bytes = OFFLINE_FIXTURE_PLACEHOLDER_PNG[..take].to_vec();
              content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
              decode_stage = decode_stage_for_content_type(content_type.as_deref());
            }
            let is_retryable_status = retryable_http_status(status_code);

            if bytes.is_empty() && http_empty_body_is_error(status_code, allows_empty_body) {
              finish_network_fetch_diagnostics(network_timer.take());
              let mut can_retry = attempt < max_attempts;
              if can_retry {
                let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
                if let Some(retry_after) = retry_after {
                  backoff = backoff.max(retry_after);
                }

                if let Some(deadline) = deadline.as_ref() {
                  if deadline.timeout_limit().is_some() {
                    match deadline.remaining_timeout() {
                      Some(remaining) if !remaining.is_zero() => {
                        let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                        backoff = backoff.min(max_sleep);
                      }
                      _ => can_retry = false,
                    }
                  }
                }
                if let Some(budget) = timeout_budget {
                  match budget.checked_sub(started.elapsed()) {
                    Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                      let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }

                if can_retry {
                  render_control::check_active(stage_hint).map_err(Error::Render)?;
                  log_http_retry(
                    &format!("empty body (status {status_code})"),
                    attempt,
                    max_attempts,
                    &current,
                    backoff,
                  );
                  if !backoff.is_zero() {
                    sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                      .map_err(Error::Render)?;
                  }
                  continue;
                }
                if timeout_budget.is_some() {
                  return Err(budget_exhausted_error(&current, attempt));
                }
              }

              let mut message = "empty HTTP response body".to_string();
              if attempt < max_attempts {
                message.push_str(" (retry aborted: render deadline exceeded)");
              }
              message.push_str(&format_attempt_suffix(attempt, max_attempts));
              let err = ResourceError::new(current.clone(), message)
                .with_status(status_code)
                .with_final_url(final_url.clone());
              return Err(Error::Resource(err));
            }

            if is_retryable_status {
              finish_network_fetch_diagnostics(network_timer.take());
              let mut can_retry = attempt < max_attempts;
              if can_retry {
                let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
                if let Some(retry_after) = retry_after {
                  backoff = backoff.max(retry_after);
                }
                if let Some(deadline) = deadline.as_ref() {
                  if deadline.timeout_limit().is_some() {
                    match deadline.remaining_timeout() {
                      Some(remaining) if !remaining.is_zero() => {
                        let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                        backoff = backoff.min(max_sleep);
                      }
                      _ => can_retry = false,
                    }
                  }
                }
                if let Some(budget) = timeout_budget {
                  match budget.checked_sub(started.elapsed()) {
                    Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                      let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }
                if can_retry {
                  render_control::check_active(stage_hint).map_err(Error::Render)?;
                  log_http_retry(
                    &format!("status {status_code}"),
                    attempt,
                    max_attempts,
                    &current,
                    backoff,
                  );
                  if !backoff.is_zero() {
                    sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                      .map_err(Error::Render)?;
                  }
                  continue;
                }
                if timeout_budget.is_some() {
                  return Err(budget_exhausted_error(&current, attempt));
                }
              }

              // Treat retryable statuses as success once retries are exhausted so caching layers can
              // persist the HTTP response deterministically (especially important for pageset
              // warm-cache runs that want to avoid repeated network fetches for persistent 429/5xx
              // responses). Still surface a hard error when retries are aborted early due to a
              // render deadline; callers can decide whether to fall back to cached bytes.
              if status_code != 202 && (deadline_retries_disabled || attempt < max_attempts) {
                let mut message =
                  "retryable HTTP status (retry aborted: render deadline exceeded)".to_string();
                message.push_str(&format_attempt_suffix(attempt, max_attempts));
                let err = ResourceError::new(current.clone(), message)
                  .with_status(status_code)
                  .with_final_url(final_url.clone());
                return Err(Error::Resource(err));
              }
            }

            finish_network_fetch_diagnostics(network_timer.take());
            self.policy.reserve_budget(bytes.len())?;
            let mut resource =
              FetchedResource::with_final_url(bytes, content_type, Some(final_url));
            resource.response_headers = Some(response_headers);
            if !encodings.is_empty() {
              resource.content_encoding = Some(encodings.join(", "));
            }
            resource.nosniff = nosniff;
            resource.status = Some(status_code);
            resource.etag = etag;
            resource.last_modified = last_modified;
            resource.access_control_allow_origin = access_control_allow_origin;
            resource.timing_allow_origin = timing_allow_origin;
            resource.vary = vary;
            resource.response_referrer_policy = response_referrer_policy;
            resource.access_control_allow_credentials = access_control_allow_credentials;
            resource.cache_policy = cache_policy;
            render_control::check_root(decode_stage).map_err(Error::Render)?;
            return Ok(resource);
          }
          Err(err) => {
            finish_network_fetch_diagnostics(network_timer.take());
            if attempt < max_attempts && is_retryable_io_error(&err) {
              let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
              let mut can_retry = true;
              if let Some(deadline) = deadline.as_ref() {
                if deadline.timeout_limit().is_some() {
                  match deadline.remaining_timeout() {
                    Some(remaining) if !remaining.is_zero() => {
                      let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }
              }
              if let Some(budget) = timeout_budget {
                match budget.checked_sub(started.elapsed()) {
                  Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                    let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                    backoff = backoff.min(max_sleep);
                  }
                  _ => can_retry = false,
                }
              }
              if can_retry {
                render_control::check_active(stage_hint).map_err(Error::Render)?;
                let reason = err.to_string();
                log_http_retry(&reason, attempt, max_attempts, &current, backoff);
                if !backoff.is_zero() {
                  sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                    .map_err(Error::Render)?;
                }
                continue;
              }
              if timeout_budget.is_some() {
                return Err(budget_exhausted_error(&current, attempt));
              }
            }

            let overall_timeout = deadline.as_ref().and_then(|d| d.timeout_limit());
            let mut message = err.to_string();
            if err.kind() == io::ErrorKind::TimedOut {
              if let Some(overall) = overall_timeout {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout={overall:?})"
                ));
              } else if let Some(budget) = timeout_budget {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout_budget={budget:?})"
                ));
              } else {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?})"
                ));
              }
            } else {
              message.push_str(&format_attempt_suffix(attempt, max_attempts));
            }

            let err = ResourceError::new(current.clone(), message)
              .with_status(status_code)
              .with_final_url(final_url)
              .with_source(err);
            return Err(Error::Resource(err));
          }
        }
      }
    }

    Err(Error::Resource(
      ResourceError::new(
        url,
        format!("too many redirects (limit {})", self.policy.max_redirects),
      )
      .with_final_url(current),
    ))
  }

  fn fetch_http_partial_inner_reqwest(
    &self,
    kind: FetchContextKind,
    destination: FetchDestination,
    url: &str,
    max_bytes: usize,
    client_origin: Option<&DocumentOrigin>,
    referrer_url: Option<&str>,
    referrer_policy: ReferrerPolicy,
    credentials_mode: FetchCredentialsMode,
    deadline: &Option<render_control::RenderDeadline>,
    started: Instant,
  ) -> Result<FetchedResource> {
    let mut current = url.to_string();
    let client = &self.reqwest_client;
    let timeout_budget = self.timeout_budget(deadline);
    let mut effective_referrer_policy = referrer_policy;
    let mut redirect_referrer_policy: Option<ReferrerPolicy> = None;
    let deadline_retries_disabled = deadline.as_ref().is_some_and(|deadline| {
      deadline.timeout_limit().is_some() && !deadline.http_retries_enabled()
    });
    let max_attempts = if deadline_retries_disabled {
      1
    } else {
      self.retry_policy.max_attempts.max(1)
    };

    let budget_exhausted_error = |current_url: &str, attempt: usize| -> Error {
      let budget = timeout_budget.expect("budget mode should be active");
      let elapsed = started.elapsed();
      Error::Resource(
        ResourceError::new(
          current_url.to_string(),
          format!(
            "overall HTTP timeout budget exceeded (budget={budget:?}, elapsed={elapsed:?}){}",
            format_attempt_suffix(attempt, max_attempts)
          ),
        )
        .with_final_url(current_url.to_string()),
      )
    };

    'redirects: for _ in 0..self.policy.max_redirects {
      self.policy.ensure_url_allowed(&current)?;
      for attempt in 1..=max_attempts {
        self.policy.ensure_url_allowed(&current)?;
        let stage_hint = render_stage_hint_for_context(kind, &current);
        render_control::check_active(stage_hint).map_err(Error::Render)?;
        let allowed_limit = self.policy.allowed_response_limit()?;
        let read_limit = max_bytes.min(allowed_limit).max(1);
        let per_request_timeout = self.deadline_aware_timeout(kind, deadline.as_ref(), &current)?;
        let mut effective_timeout = per_request_timeout.unwrap_or(self.policy.request_timeout);

        if let Some(budget) = timeout_budget {
          match budget.checked_sub(started.elapsed()) {
            Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
              let budget_timeout = remaining.saturating_sub(HTTP_DEADLINE_BUFFER);
              effective_timeout = effective_timeout.min(budget_timeout);
            }
            _ => return Err(budget_exhausted_error(&current, attempt)),
          }
        }

        let mut headers = build_http_header_pairs(
          &current,
          &self.user_agent,
          &self.accept_language,
          "identity",
          None,
          destination,
          client_origin,
          referrer_url,
          effective_referrer_policy,
        );
        if cookies_allowed_for_request(credentials_mode, &current, client_origin) {
          if let Some(value) = self.cookie_header_value(&current) {
            headers.push(("Cookie".to_string(), value));
          }
        }
        headers.push((
          "Range".to_string(),
          format!("bytes=0-{}", read_limit.saturating_sub(1)),
        ));

        let mut request = client.get(&current);
        for (name, value) in &headers {
          request = request.header(name, value);
        }
        if !effective_timeout.is_zero() {
          request = request.timeout(effective_timeout);
        }

        let mut network_timer = start_network_fetch_diagnostics();
        let mut response = match request.send() {
          Ok(resp) => resp,
          Err(err) => {
            finish_network_fetch_diagnostics(network_timer.take());
            if attempt < max_attempts && is_retryable_reqwest_error(&err) {
              let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
              let mut can_retry = true;
              if let Some(deadline) = deadline.as_ref() {
                if deadline.timeout_limit().is_some() {
                  match deadline.remaining_timeout() {
                    Some(remaining) if !remaining.is_zero() => {
                      let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }
              }
              if let Some(budget) = timeout_budget {
                match budget.checked_sub(started.elapsed()) {
                  Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                    let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                    backoff = backoff.min(max_sleep);
                  }
                  _ => can_retry = false,
                }
              }
              if can_retry {
                render_control::check_active(stage_hint).map_err(Error::Render)?;
                let reason = err.to_string();
                log_http_retry(&reason, attempt, max_attempts, &current, backoff);
                if !backoff.is_zero() {
                  sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                    .map_err(Error::Render)?;
                }
                continue;
              }
              if timeout_budget.is_some() {
                return Err(budget_exhausted_error(&current, attempt));
              }
            }

            let overall_timeout = deadline.as_ref().and_then(|d| d.timeout_limit());
            let mut message = err.to_string();
            if err.is_timeout() {
              if let Some(overall) = overall_timeout {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout={overall:?})"
                ));
              } else if let Some(budget) = timeout_budget {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout_budget={budget:?})"
                ));
              } else {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?})"
                ));
              }
            } else {
              message.push_str(&format_attempt_suffix(attempt, max_attempts));
            }

            let err = ResourceError::new(current.clone(), message)
              .with_final_url(current.clone())
              .with_source(err);
            return Err(Error::Resource(err));
          }
        };

        if cookies_allowed_for_request(credentials_mode, &current, client_origin) {
          self.store_cookies_from_headers(&current, response.headers());
        }

        let status = response.status();
        if (300..400).contains(&status.as_u16()) {
          if let Some(loc) = response
            .headers()
            .get("location")
            .and_then(|h| h.to_str().ok())
          {
            if let Some(policy) = header_values_joined(response.headers(), "referrer-policy")
              .as_deref()
              .and_then(ReferrerPolicy::parse_value_list)
            {
              effective_referrer_policy = policy;
              redirect_referrer_policy = Some(policy);
            }
            let next = Url::parse(&current)
              .ok()
              .and_then(|base| base.join(loc).ok())
              .map(|u| u.to_string())
              .unwrap_or_else(|| loc.to_string());
            finish_network_fetch_diagnostics(network_timer.take());
            render_control::check_active(render_stage_hint_for_context(kind, &next))
              .map_err(Error::Render)?;
            self.policy.ensure_url_allowed(&next)?;
            current = next;
            continue 'redirects;
          }
        }

        let status_code = status.as_u16();
        let retry_after =
          if self.retry_policy.respect_retry_after && retryable_http_status(status_code) {
            parse_retry_after(response.headers())
          } else {
            None
          };

        let encodings = parse_content_encodings(response.headers());
        if encodings.iter().any(|enc| enc != "identity") {
          finish_network_fetch_diagnostics(network_timer.take());
          let final_url = response.url().to_string();
          let err = ResourceError::new(
            current.clone(),
            format!(
              "received content-encoding {:?} for partial fetch (expected identity)",
              encodings
            ),
          )
          .with_status(status_code)
          .with_final_url(final_url);
          return Err(Error::Resource(err));
        }

        let mut content_type = response
          .headers()
          .get("content-type")
          .and_then(|h| h.to_str().ok())
          .map(|s| s.to_string());
        let nosniff = header_has_nosniff(response.headers());
        let mut decode_stage = decode_stage_for_content_type(content_type.as_deref());
        let etag = response
          .headers()
          .get("etag")
          .and_then(|h| h.to_str().ok())
          .map(|s| s.to_string());
        let last_modified = response
          .headers()
          .get("last-modified")
          .and_then(|h| h.to_str().ok())
          .map(|s| s.to_string());
        let (access_control_allow_origin, access_control_allow_credentials) =
          parse_cors_response_headers(response.headers());
        let timing_allow_origin = header_values_joined(response.headers(), "timing-allow-origin");
        let response_referrer_policy = header_values_joined(response.headers(), "referrer-policy")
          .as_deref()
          .and_then(ReferrerPolicy::parse_value_list)
          .or(redirect_referrer_policy);
        let cache_policy = parse_http_cache_policy(response.headers());
        let vary = parse_vary_headers(response.headers());
        let final_url = response.url().to_string();
        let response_headers = collect_response_headers(response.headers());
        let allows_empty_body =
          http_response_allows_empty_body(kind, status_code, response.headers());

        let substitute_empty_image_body =
          should_substitute_empty_image_body(kind, status_code, response.headers())
            || should_substitute_akamai_pixel_empty_image_body(
              kind,
              &final_url,
              status_code,
              response.headers(),
            );
        let substitute_captcha_image =
          should_substitute_captcha_image_response(kind, status_code, &final_url);
        let body_result = read_response_prefix(&mut response, read_limit);
        match body_result {
          Ok(mut bytes) => {
            record_network_fetch_bytes(bytes.len());
            if bytes.is_empty() && substitute_empty_image_body {
              let take = OFFLINE_FIXTURE_PLACEHOLDER_PNG.len().min(read_limit);
              bytes = OFFLINE_FIXTURE_PLACEHOLDER_PNG[..take].to_vec();
              content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
              decode_stage = decode_stage_for_content_type(content_type.as_deref());
            }
            if should_substitute_markup_image_body(
              kind,
              url,
              &final_url,
              content_type.as_deref(),
              &bytes,
            ) {
              let take = OFFLINE_FIXTURE_PLACEHOLDER_PNG.len().min(read_limit);
              bytes = OFFLINE_FIXTURE_PLACEHOLDER_PNG[..take].to_vec();
              content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
              decode_stage = decode_stage_for_content_type(content_type.as_deref());
            }
            if substitute_captcha_image {
              let take = OFFLINE_FIXTURE_PLACEHOLDER_PNG.len().min(read_limit);
              bytes = OFFLINE_FIXTURE_PLACEHOLDER_PNG[..take].to_vec();
              content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
              decode_stage = decode_stage_for_content_type(content_type.as_deref());
            }
            let is_retryable_status = retryable_http_status(status_code);

            if bytes.is_empty() && http_empty_body_is_error(status_code, allows_empty_body) {
              finish_network_fetch_diagnostics(network_timer.take());
              let mut can_retry = attempt < max_attempts;
              if can_retry {
                let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
                if let Some(retry_after) = retry_after {
                  backoff = backoff.max(retry_after);
                }

                if let Some(deadline) = deadline.as_ref() {
                  if deadline.timeout_limit().is_some() {
                    match deadline.remaining_timeout() {
                      Some(remaining) if !remaining.is_zero() => {
                        let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                        backoff = backoff.min(max_sleep);
                      }
                      _ => can_retry = false,
                    }
                  }
                }
                if let Some(budget) = timeout_budget {
                  match budget.checked_sub(started.elapsed()) {
                    Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                      let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }

                if can_retry {
                  render_control::check_active(stage_hint).map_err(Error::Render)?;
                  log_http_retry(
                    &format!("empty body (status {status_code})"),
                    attempt,
                    max_attempts,
                    &current,
                    backoff,
                  );
                  if !backoff.is_zero() {
                    sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                      .map_err(Error::Render)?;
                  }
                  continue;
                }
                if timeout_budget.is_some() {
                  return Err(budget_exhausted_error(&current, attempt));
                }
              }

              let mut message = "empty HTTP response body".to_string();
              if attempt < max_attempts {
                message.push_str(" (retry aborted: render deadline exceeded)");
              }
              message.push_str(&format_attempt_suffix(attempt, max_attempts));
              let err = ResourceError::new(current.clone(), message)
                .with_status(status_code)
                .with_final_url(final_url.clone());
              return Err(Error::Resource(err));
            }

            if is_retryable_status {
              finish_network_fetch_diagnostics(network_timer.take());
              let mut can_retry = attempt < max_attempts;
              if can_retry {
                let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
                if let Some(retry_after) = retry_after {
                  backoff = backoff.max(retry_after);
                }
                if let Some(deadline) = deadline.as_ref() {
                  if deadline.timeout_limit().is_some() {
                    match deadline.remaining_timeout() {
                      Some(remaining) if !remaining.is_zero() => {
                        let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                        backoff = backoff.min(max_sleep);
                      }
                      _ => can_retry = false,
                    }
                  }
                }
                if let Some(budget) = timeout_budget {
                  match budget.checked_sub(started.elapsed()) {
                    Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                      let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }
                if can_retry {
                  render_control::check_active(stage_hint).map_err(Error::Render)?;
                  log_http_retry(
                    &format!("status {status_code}"),
                    attempt,
                    max_attempts,
                    &current,
                    backoff,
                  );
                  if !backoff.is_zero() {
                    sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                      .map_err(Error::Render)?;
                  }
                  continue;
                }
                if timeout_budget.is_some() {
                  return Err(budget_exhausted_error(&current, attempt));
                }
              }

              // Treat retryable statuses as success once retries are exhausted so caching layers can
              // persist the HTTP response deterministically (especially important for pageset
              // warm-cache runs that want to avoid repeated network fetches for persistent 429/5xx
              // responses). Still surface a hard error when retries are aborted early due to a
              // render deadline; callers can decide whether to fall back to cached bytes.
              if status_code != 202 && (deadline_retries_disabled || attempt < max_attempts) {
                let mut message =
                  "retryable HTTP status (retry aborted: render deadline exceeded)".to_string();
                message.push_str(&format_attempt_suffix(attempt, max_attempts));
                let err = ResourceError::new(current.clone(), message)
                  .with_status(status_code)
                  .with_final_url(final_url.clone());
                return Err(Error::Resource(err));
              }
            }

            finish_network_fetch_diagnostics(network_timer.take());
            self.policy.reserve_budget(bytes.len())?;
            let mut resource =
              FetchedResource::with_final_url(bytes, content_type, Some(final_url));
            resource.response_headers = Some(response_headers);
            if !encodings.is_empty() {
              resource.content_encoding = Some(encodings.join(", "));
            }
            resource.nosniff = nosniff;
            resource.status = Some(status_code);
            resource.etag = etag;
            resource.last_modified = last_modified;
            resource.access_control_allow_origin = access_control_allow_origin;
            resource.timing_allow_origin = timing_allow_origin;
            resource.vary = vary;
            resource.response_referrer_policy = response_referrer_policy;
            resource.access_control_allow_credentials = access_control_allow_credentials;
            resource.cache_policy = cache_policy;
            render_control::check_root(decode_stage).map_err(Error::Render)?;
            return Ok(resource);
          }
          Err(err) => {
            finish_network_fetch_diagnostics(network_timer.take());
            if attempt < max_attempts && is_retryable_io_error(&err) {
              let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
              let mut can_retry = true;
              if let Some(deadline) = deadline.as_ref() {
                if deadline.timeout_limit().is_some() {
                  match deadline.remaining_timeout() {
                    Some(remaining) if !remaining.is_zero() => {
                      let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }
              }
              if let Some(budget) = timeout_budget {
                match budget.checked_sub(started.elapsed()) {
                  Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                    let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                    backoff = backoff.min(max_sleep);
                  }
                  _ => can_retry = false,
                }
              }
              if can_retry {
                render_control::check_active(stage_hint).map_err(Error::Render)?;
                let reason = err.to_string();
                log_http_retry(&reason, attempt, max_attempts, &current, backoff);
                if !backoff.is_zero() {
                  sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                    .map_err(Error::Render)?;
                }
                continue;
              }
              if timeout_budget.is_some() {
                return Err(budget_exhausted_error(&current, attempt));
              }
            }

            let overall_timeout = deadline.as_ref().and_then(|d| d.timeout_limit());
            let mut message = err.to_string();
            if err.kind() == io::ErrorKind::TimedOut {
              if let Some(overall) = overall_timeout {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout={overall:?})"
                ));
              } else if let Some(budget) = timeout_budget {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout_budget={budget:?})"
                ));
              } else {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?})"
                ));
              }
            } else {
              message.push_str(&format_attempt_suffix(attempt, max_attempts));
            }

            let err = ResourceError::new(current.clone(), message)
              .with_status(status_code)
              .with_final_url(final_url)
              .with_source(err);
            return Err(Error::Resource(err));
          }
        }
      }
    }

    Err(Error::Resource(
      ResourceError::new(
        url,
        format!("too many redirects (limit {})", self.policy.max_redirects),
      )
      .with_final_url(current),
    ))
  }

  fn fetch_http_with_accept_inner_ureq<'a>(
    &self,
    kind: FetchContextKind,
    destination: FetchDestination,
    url: &str,
    accept_encoding: Option<&str>,
    validators: Option<HttpCacheValidators<'a>>,
    client_origin: Option<&DocumentOrigin>,
    referrer_url: Option<&str>,
    referrer_policy: ReferrerPolicy,
    credentials_mode: FetchCredentialsMode,
    deadline: &Option<render_control::RenderDeadline>,
    started: Instant,
    auto_fallback: bool,
  ) -> Result<FetchedResource> {
    let mut current = url.to_string();
    let mut validators = validators;
    let agent = &self.agent;
    let timeout_budget = self.timeout_budget(deadline);
    let mut effective_referrer_policy = referrer_policy;
    let mut redirect_referrer_policy: Option<ReferrerPolicy> = None;
    let deadline_retries_disabled = deadline.as_ref().is_some_and(|deadline| {
      deadline.timeout_limit().is_some() && !deadline.http_retries_enabled()
    });
    let max_attempts = if deadline_retries_disabled {
      1
    } else {
      self.retry_policy.max_attempts.max(1)
    };

    let budget_exhausted_error = |current_url: &str, attempt: usize| -> Error {
      let budget = timeout_budget.expect("budget mode should be active");
      let elapsed = started.elapsed();
      Error::Resource(
        ResourceError::new(
          current_url.to_string(),
          format!(
            "overall HTTP timeout budget exceeded (budget={budget:?}, elapsed={elapsed:?}){}",
            format_attempt_suffix(attempt, max_attempts)
          ),
        )
        .with_final_url(current_url.to_string()),
      )
    };

    'redirects: for _ in 0..self.policy.max_redirects {
      self.policy.ensure_url_allowed(&current)?;
      for attempt in 1..=max_attempts {
        self.policy.ensure_url_allowed(&current)?;
        let stage_hint = render_stage_hint_for_context(kind, &current);
        render_control::check_active(stage_hint).map_err(Error::Render)?;
        let allowed_limit = self.policy.allowed_response_limit()?;
        let per_request_timeout = self.deadline_aware_timeout(kind, deadline.as_ref(), &current)?;
        let mut effective_timeout = per_request_timeout.unwrap_or(self.policy.request_timeout);

        // In `auto` backend mode we may fall back to cURL (HTTP/2 capable). To avoid HTTP/1.1 hangs
        // consuming the full timeout budget, cap the `ureq` attempt so some time remains for the
        // fallback backend.
        if auto_fallback {
          if let Some(timeout) = per_request_timeout {
            // Deadline-derived timeout: keep at most half for ureq.
            effective_timeout = effective_timeout.min(auto_backend_ureq_timeout_slice(timeout));
          } else if let Some(budget) = timeout_budget {
            // Timeout-budget mode (CLI): keep at most half of the remaining budget for ureq.
            if let Some(remaining) = budget.checked_sub(started.elapsed()) {
              effective_timeout = effective_timeout.min(auto_backend_ureq_timeout_slice(remaining));
            }
          }
        }

        if let Some(budget) = timeout_budget {
          match budget.checked_sub(started.elapsed()) {
            Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
              let budget_timeout = remaining.saturating_sub(HTTP_DEADLINE_BUFFER);
              effective_timeout = effective_timeout.min(budget_timeout);
            }
            _ => return Err(budget_exhausted_error(&current, attempt)),
          }
        }

        let accept_encoding_value = accept_encoding.unwrap_or(SUPPORTED_ACCEPT_ENCODING);
        let mut headers = build_http_header_pairs(
          &current,
          &self.user_agent,
          &self.accept_language,
          accept_encoding_value,
          validators,
          destination,
          client_origin,
          referrer_url,
          effective_referrer_policy,
        );
        if cookies_allowed_for_request(credentials_mode, &current, client_origin) {
          if let Some(value) = self.cookie_header_value(&current) {
            headers.push(("Cookie".to_string(), value));
          }
        }
        let mut request = agent.get(&current);
        for (name, value) in &headers {
          request = request.header(name, value);
        }

        if !effective_timeout.is_zero() {
          request = request
            .config()
            .timeout_global(Some(effective_timeout))
            .build();
        }

        let mut network_timer = start_network_fetch_diagnostics();
        let mut response = match request.call() {
          Ok(resp) => resp,
          Err(err) => {
            finish_network_fetch_diagnostics(network_timer.take());
            if attempt < max_attempts && is_retryable_ureq_error(&err) && !auto_fallback {
              let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
              let mut can_retry = true;
              if let Some(deadline) = deadline.as_ref() {
                if deadline.timeout_limit().is_some() {
                  match deadline.remaining_timeout() {
                    Some(remaining) if !remaining.is_zero() => {
                      // Ensure we never sleep past the deadline; cap and allow immediate retry if the
                      // remaining budget is tiny.
                      let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }
              }
              if let Some(budget) = timeout_budget {
                match budget.checked_sub(started.elapsed()) {
                  Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                    let max_sleep = remaining.saturating_sub(HTTP_DEADLINE_BUFFER);
                    backoff = backoff.min(max_sleep);
                  }
                  _ => can_retry = false,
                }
              }
              if can_retry {
                render_control::check_active(stage_hint).map_err(Error::Render)?;
                let reason = err.to_string();
                log_http_retry(&reason, attempt, max_attempts, &current, backoff);
                if !backoff.is_zero() {
                  sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                    .map_err(Error::Render)?;
                }
                continue;
              }
              if timeout_budget.is_some() {
                return Err(budget_exhausted_error(&current, attempt));
              }
            }

            let overall_timeout = deadline.as_ref().and_then(|d| d.timeout_limit());
            let mut message = err.to_string();
            let lower = message.to_ascii_lowercase();
            if lower.contains("timeout") || lower.contains("timed out") {
              if let Some(overall) = overall_timeout {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout={overall:?})"
                ));
              } else if let Some(budget) = timeout_budget {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout_budget={budget:?})"
                ));
              } else {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?})"
                ));
              }
            } else {
              message.push_str(&format_attempt_suffix(attempt, max_attempts));
            }

            let err = ResourceError::new(current.clone(), message)
              .with_final_url(current.clone())
              .with_source(err);
            return Err(Error::Resource(err));
          }
        };

        if cookies_allowed_for_request(credentials_mode, &current, client_origin) {
          self.store_cookies_from_headers(&current, response.headers());
        }

        let status = response.status();
        if (300..400).contains(&status.as_u16()) {
          if let Some(loc) = response
            .headers()
            .get("location")
            .and_then(|h| h.to_str().ok())
          {
            if let Some(policy) = header_values_joined(response.headers(), "referrer-policy")
              .as_deref()
              .and_then(ReferrerPolicy::parse_value_list)
            {
              effective_referrer_policy = policy;
              redirect_referrer_policy = Some(policy);
            }
            let next = Url::parse(&current)
              .ok()
              .and_then(|base| base.join(loc).ok())
              .map(|u| u.to_string())
              .unwrap_or_else(|| loc.to_string());
            finish_network_fetch_diagnostics(network_timer.take());
            render_control::check_active(render_stage_hint_for_context(kind, &next))
              .map_err(Error::Render)?;
            self.policy.ensure_url_allowed(&next)?;
            current = next;
            validators = None;
            continue 'redirects;
          }
        }

        let status_code = status.as_u16();
        let retry_after =
          if self.retry_policy.respect_retry_after && retryable_http_status(status_code) {
            parse_retry_after(response.headers())
          } else {
            None
          };

        let encodings = parse_content_encodings(response.headers());
        let mut content_type = response
          .headers()
          .get("content-type")
          .and_then(|h| h.to_str().ok())
          .map(|s| s.to_string());
        let nosniff = header_has_nosniff(response.headers());
        let mut decode_stage = decode_stage_for_content_type(content_type.as_deref());
        let etag = response
          .headers()
          .get("etag")
          .and_then(|h| h.to_str().ok())
          .map(|s| s.to_string());
        let last_modified = response
          .headers()
          .get("last-modified")
          .and_then(|h| h.to_str().ok())
          .map(|s| s.to_string());
        let (access_control_allow_origin, access_control_allow_credentials) =
          parse_cors_response_headers(response.headers());
        let timing_allow_origin = header_values_joined(response.headers(), "timing-allow-origin");
        let response_referrer_policy = header_values_joined(response.headers(), "referrer-policy")
          .as_deref()
          .and_then(ReferrerPolicy::parse_value_list)
          .or(redirect_referrer_policy);
        let cache_policy = parse_http_cache_policy(response.headers());
        let vary = parse_vary_headers(response.headers());
        let final_url = response.get_uri().to_string();
        let response_headers = collect_response_headers(response.headers());
        let allows_empty_body =
          http_response_allows_empty_body(kind, status_code, response.headers());
        let substitute_captcha_image =
          should_substitute_captcha_image_response(kind, status_code, &final_url);

        let body_result = response
          .body_mut()
          .with_config()
          .limit(allowed_limit as u64)
          .read_to_vec()
          .map_err(|e| e.into_io());

        match body_result {
          Ok(bytes) => {
            if let Err(err) = render_control::check_active(stage_hint) {
              finish_network_fetch_diagnostics(network_timer.take());
              return Err(Error::Render(err));
            }
            let mut bytes =
              match decode_content_encodings(bytes, &encodings, allowed_limit, decode_stage) {
                Ok(decoded) => decoded,
                Err(ContentDecodeError::DeadlineExceeded { stage, elapsed, .. }) => {
                  finish_network_fetch_diagnostics(network_timer.take());
                  return Err(Error::Render(RenderError::Timeout { stage, elapsed }));
                }
                Err(ContentDecodeError::DecompressionFailed { .. })
                  if accept_encoding.is_none() =>
                {
                  finish_network_fetch_diagnostics(network_timer.take());
                  return self.fetch_http_with_accept_inner_ureq(
                    kind,
                    destination,
                    url,
                    Some("identity"),
                    validators,
                    client_origin,
                    referrer_url,
                    referrer_policy,
                    credentials_mode,
                    deadline,
                    started,
                    auto_fallback,
                  );
                }
                Err(err) => {
                  finish_network_fetch_diagnostics(network_timer.take());
                  let err =
                    err.into_resource_error(current.clone(), status.as_u16(), final_url.clone());
                  return Err(Error::Resource(err));
                }
              };

            record_network_fetch_bytes(bytes.len());
            if bytes.is_empty()
              && (should_substitute_empty_image_body(kind, status_code, response.headers())
                || should_substitute_akamai_pixel_empty_image_body(
                  kind,
                  &final_url,
                  status_code,
                  response.headers(),
                ))
            {
              bytes = OFFLINE_FIXTURE_PLACEHOLDER_PNG.to_vec();
              content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
              decode_stage = decode_stage_for_content_type(content_type.as_deref());
            }
            if should_substitute_markup_image_body(
              kind,
              url,
              &final_url,
              content_type.as_deref(),
              &bytes,
            ) {
              bytes = OFFLINE_FIXTURE_PLACEHOLDER_PNG.to_vec();
              content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
              decode_stage = decode_stage_for_content_type(content_type.as_deref());
            }
            if substitute_captcha_image {
              bytes = OFFLINE_FIXTURE_PLACEHOLDER_PNG.to_vec();
              content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
              decode_stage = decode_stage_for_content_type(content_type.as_deref());
            }
            let is_retryable_status = retryable_http_status(status_code);

            if bytes.is_empty() && http_empty_body_is_error(status_code, allows_empty_body) {
              finish_network_fetch_diagnostics(network_timer.take());
              let mut can_retry = attempt < max_attempts;
              if can_retry {
                let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
                if let Some(retry_after) = retry_after {
                  backoff = backoff.max(retry_after);
                }

                if let Some(deadline) = deadline.as_ref() {
                  if deadline.timeout_limit().is_some() {
                    match deadline.remaining_timeout() {
                      Some(remaining) if !remaining.is_zero() => {
                        let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                        backoff = backoff.min(max_sleep);
                      }
                      _ => can_retry = false,
                    }
                  }
                }
                if let Some(budget) = timeout_budget {
                  match budget.checked_sub(started.elapsed()) {
                    Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                      let max_sleep = remaining.saturating_sub(HTTP_DEADLINE_BUFFER);
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }

                if can_retry {
                  render_control::check_active(stage_hint).map_err(Error::Render)?;
                  log_http_retry(
                    &format!("empty body (status {status_code})"),
                    attempt,
                    max_attempts,
                    &current,
                    backoff,
                  );
                  if !backoff.is_zero() {
                    sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                      .map_err(Error::Render)?;
                  }
                  continue;
                }
                if timeout_budget.is_some() {
                  return Err(budget_exhausted_error(&current, attempt));
                }
              }

              let mut message = "empty HTTP response body".to_string();
              if attempt < max_attempts {
                message.push_str(" (retry aborted: render deadline exceeded)");
              }
              message.push_str(&format_attempt_suffix(attempt, max_attempts));
              let err = ResourceError::new(current.clone(), message)
                .with_status(status_code)
                .with_final_url(final_url.clone());
              return Err(Error::Resource(err));
            }

            if is_retryable_status {
              finish_network_fetch_diagnostics(network_timer.take());
              let mut can_retry = attempt < max_attempts;
              if can_retry {
                let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
                if let Some(retry_after) = retry_after {
                  backoff = backoff.max(retry_after);
                }
                if let Some(deadline) = deadline.as_ref() {
                  if deadline.timeout_limit().is_some() {
                    match deadline.remaining_timeout() {
                      Some(remaining) if !remaining.is_zero() => {
                        let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                        backoff = backoff.min(max_sleep);
                      }
                      _ => can_retry = false,
                    }
                  }
                }
                if let Some(budget) = timeout_budget {
                  match budget.checked_sub(started.elapsed()) {
                    Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                      let max_sleep = remaining.saturating_sub(HTTP_DEADLINE_BUFFER);
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }
                if can_retry {
                  render_control::check_active(stage_hint).map_err(Error::Render)?;
                  log_http_retry(
                    &format!("status {status_code}"),
                    attempt,
                    max_attempts,
                    &current,
                    backoff,
                  );
                  if !backoff.is_zero() {
                    sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                      .map_err(Error::Render)?;
                  }
                  continue;
                }
                if timeout_budget.is_some() {
                  return Err(budget_exhausted_error(&current, attempt));
                }
              }

              // 202 (Accepted) is a "success" status code that is often used to mean "poll again
              // later". We still treat it as transient and retryable, but once retries are
              // exhausted (or a render deadline prevents further retries) we return `Ok` and let
              // higher-level code decide how to handle the empty/placeholder body.
              // Treat retryable statuses as success once retries are exhausted so caching layers can
              // persist the HTTP response deterministically (especially important for pageset
              // warm-cache runs that want to avoid repeated network fetches for persistent 429/5xx
              // responses). Still surface a hard error when retries are aborted early due to a
              // render deadline; callers can decide whether to fall back to cached bytes.
              if status_code != 202 && (deadline_retries_disabled || attempt < max_attempts) {
                let mut message =
                  "retryable HTTP status (retry aborted: render deadline exceeded)".to_string();
                message.push_str(&format_attempt_suffix(attempt, max_attempts));
                let err = ResourceError::new(current.clone(), message)
                  .with_status(status_code)
                  .with_final_url(final_url.clone());
                return Err(Error::Resource(err));
              }
            }

            if bytes.len() > allowed_limit {
              finish_network_fetch_diagnostics(network_timer.take());
              if let Some(remaining) = self.policy.remaining_budget() {
                if bytes.len() > remaining {
                  let err = ResourceError::new(
                    current.clone(),
                    format!(
                      "total bytes budget exceeded ({} > {} bytes remaining)",
                      bytes.len(),
                      remaining
                    ),
                  )
                  .with_status(status.as_u16())
                  .with_final_url(final_url.clone());
                  return Err(Error::Resource(err));
                }
              }

              let err = ResourceError::new(
                current.clone(),
                format!(
                  "response too large ({} > {} bytes)",
                  bytes.len(),
                  allowed_limit
                ),
              )
              .with_status(status.as_u16())
              .with_final_url(final_url.clone());
              return Err(Error::Resource(err));
            }

            finish_network_fetch_diagnostics(network_timer.take());
            self.policy.reserve_budget(bytes.len())?;
            let mut resource =
              FetchedResource::with_final_url(bytes, content_type, Some(final_url));
            resource.response_headers = Some(response_headers);
            if !encodings.is_empty() {
              resource.content_encoding = Some(encodings.join(", "));
            }
            resource.nosniff = nosniff;
            resource.status = Some(status.as_u16());
            resource.etag = etag;
            resource.last_modified = last_modified;
            resource.access_control_allow_origin = access_control_allow_origin;
            resource.timing_allow_origin = timing_allow_origin;
            resource.vary = vary;
            resource.response_referrer_policy = response_referrer_policy;
            resource.access_control_allow_credentials = access_control_allow_credentials;
            resource.cache_policy = cache_policy;
            render_control::check_root(decode_stage).map_err(Error::Render)?;
            return Ok(resource);
          }
          Err(err) => {
            finish_network_fetch_diagnostics(network_timer.take());
            if attempt < max_attempts && is_retryable_io_error(&err) {
              let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
              let mut can_retry = true;
              if let Some(deadline) = deadline.as_ref() {
                if deadline.timeout_limit().is_some() {
                  match deadline.remaining_timeout() {
                    Some(remaining) if !remaining.is_zero() => {
                      let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }
              }
              if let Some(budget) = timeout_budget {
                match budget.checked_sub(started.elapsed()) {
                  Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                    let max_sleep = remaining.saturating_sub(HTTP_DEADLINE_BUFFER);
                    backoff = backoff.min(max_sleep);
                  }
                  _ => can_retry = false,
                }
              }
              if can_retry {
                render_control::check_active(stage_hint).map_err(Error::Render)?;
                let reason = err.to_string();
                log_http_retry(&reason, attempt, max_attempts, &current, backoff);
                if !backoff.is_zero() {
                  sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                    .map_err(Error::Render)?;
                }
                continue;
              }
              if timeout_budget.is_some() {
                return Err(budget_exhausted_error(&current, attempt));
              }
            }

            let overall_timeout = deadline.as_ref().and_then(|d| d.timeout_limit());
            let mut message = err.to_string();
            if err.kind() == io::ErrorKind::TimedOut {
              if let Some(overall) = overall_timeout {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout={overall:?})"
                ));
              } else if let Some(budget) = timeout_budget {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout_budget={budget:?})"
                ));
              } else {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?})"
                ));
              }
            } else {
              message.push_str(&format_attempt_suffix(attempt, max_attempts));
            }

            let err = ResourceError::new(current.clone(), message)
              .with_status(status.as_u16())
              .with_final_url(final_url)
              .with_source(err);
            return Err(Error::Resource(err));
          }
        }
      }
    }

    Err(Error::Resource(
      ResourceError::new(
        url,
        format!("too many redirects (limit {})", self.policy.max_redirects),
      )
      .with_final_url(current),
    ))
  }

  fn fetch_http_with_accept_inner_reqwest<'a>(
    &self,
    kind: FetchContextKind,
    destination: FetchDestination,
    url: &str,
    accept_encoding: Option<&str>,
    validators: Option<HttpCacheValidators<'a>>,
    client_origin: Option<&DocumentOrigin>,
    referrer_url: Option<&str>,
    referrer_policy: ReferrerPolicy,
    credentials_mode: FetchCredentialsMode,
    deadline: &Option<render_control::RenderDeadline>,
    started: Instant,
    auto_fallback: bool,
  ) -> Result<FetchedResource> {
    let mut current = url.to_string();
    let mut validators = validators;
    let client = &self.reqwest_client;
    let timeout_budget = self.timeout_budget(deadline);
    let mut effective_referrer_policy = referrer_policy;
    let mut redirect_referrer_policy: Option<ReferrerPolicy> = None;
    let deadline_retries_disabled = deadline.as_ref().is_some_and(|deadline| {
      deadline.timeout_limit().is_some() && !deadline.http_retries_enabled()
    });
    let max_attempts = if auto_fallback && timeout_budget.is_some() {
      1
    } else if deadline_retries_disabled {
      1
    } else {
      self.retry_policy.max_attempts.max(1)
    };

    let budget_exhausted_error = |current_url: &str, attempt: usize| -> Error {
      let budget = timeout_budget.expect("budget mode should be active");
      let elapsed = started.elapsed();
      Error::Resource(
        ResourceError::new(
          current_url.to_string(),
          format!(
            "overall HTTP timeout budget exceeded (budget={budget:?}, elapsed={elapsed:?}){}",
            format_attempt_suffix(attempt, max_attempts)
          ),
        )
        .with_final_url(current_url.to_string()),
      )
    };

    'redirects: for _ in 0..self.policy.max_redirects {
      self.policy.ensure_url_allowed(&current)?;
      for attempt in 1..=max_attempts {
        self.policy.ensure_url_allowed(&current)?;
        let stage_hint = render_stage_hint_for_context(kind, &current);
        render_control::check_active(stage_hint).map_err(Error::Render)?;
        let allowed_limit = self.policy.allowed_response_limit()?;
        let per_request_timeout = self.deadline_aware_timeout(kind, deadline.as_ref(), &current)?;
        let mut effective_timeout = per_request_timeout.unwrap_or(self.policy.request_timeout);

        if let Some(budget) = timeout_budget {
          match budget.checked_sub(started.elapsed()) {
            Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
              let budget_timeout = remaining.saturating_sub(HTTP_DEADLINE_BUFFER);
              effective_timeout = effective_timeout.min(budget_timeout);
            }
            _ => return Err(budget_exhausted_error(&current, attempt)),
          }
        }

        let accept_encoding_value = accept_encoding.unwrap_or(SUPPORTED_ACCEPT_ENCODING);
        let mut headers = build_http_header_pairs(
          &current,
          &self.user_agent,
          &self.accept_language,
          accept_encoding_value,
          validators,
          destination,
          client_origin,
          referrer_url,
          effective_referrer_policy,
        );
        if cookies_allowed_for_request(credentials_mode, &current, client_origin) {
          if let Some(value) = self.cookie_header_value(&current) {
            headers.push(("Cookie".to_string(), value));
          }
        }
        let mut request = client.get(&current);
        for (name, value) in &headers {
          request = request.header(name, value);
        }

        if !effective_timeout.is_zero() {
          request = request.timeout(effective_timeout);
        }

        let mut network_timer = start_network_fetch_diagnostics();
        let response = match request.send() {
          Ok(resp) => resp,
          Err(err) => {
            finish_network_fetch_diagnostics(network_timer.take());
            if attempt < max_attempts && is_retryable_reqwest_error(&err) {
              let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
              let mut can_retry = true;
              if let Some(deadline) = deadline.as_ref() {
                if deadline.timeout_limit().is_some() {
                  match deadline.remaining_timeout() {
                    Some(remaining) if !remaining.is_zero() => {
                      let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }
              }
              if let Some(budget) = timeout_budget {
                match budget.checked_sub(started.elapsed()) {
                  Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                    let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                    backoff = backoff.min(max_sleep);
                  }
                  _ => can_retry = false,
                }
              }
              if can_retry {
                render_control::check_active(stage_hint).map_err(Error::Render)?;
                let reason = err.to_string();
                log_http_retry(&reason, attempt, max_attempts, &current, backoff);
                if !backoff.is_zero() {
                  sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                    .map_err(Error::Render)?;
                }
                continue;
              }
              if timeout_budget.is_some() {
                return Err(budget_exhausted_error(&current, attempt));
              }
            }

            let overall_timeout = deadline.as_ref().and_then(|d| d.timeout_limit());
            let mut message = err.to_string();
            if err.is_timeout() {
              if let Some(overall) = overall_timeout {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout={overall:?})"
                ));
              } else if let Some(budget) = timeout_budget {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout_budget={budget:?})"
                ));
              } else {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?})"
                ));
              }
            } else {
              message.push_str(&format_attempt_suffix(attempt, max_attempts));
            }

            let err = ResourceError::new(current.clone(), message)
              .with_final_url(current.clone())
              .with_source(err);
            return Err(Error::Resource(err));
          }
        };

        if cookies_allowed_for_request(credentials_mode, &current, client_origin) {
          self.store_cookies_from_headers(&current, response.headers());
        }

        let status = response.status();
        if (300..400).contains(&status.as_u16()) {
          if let Some(loc) = response
            .headers()
            .get("location")
            .and_then(|h| h.to_str().ok())
          {
            if let Some(policy) = header_values_joined(response.headers(), "referrer-policy")
              .as_deref()
              .and_then(ReferrerPolicy::parse_value_list)
            {
              effective_referrer_policy = policy;
              redirect_referrer_policy = Some(policy);
            }
            let next = Url::parse(&current)
              .ok()
              .and_then(|base| base.join(loc).ok())
              .map(|u| u.to_string())
              .unwrap_or_else(|| loc.to_string());
            finish_network_fetch_diagnostics(network_timer.take());
            render_control::check_active(render_stage_hint_for_context(kind, &next))
              .map_err(Error::Render)?;
            self.policy.ensure_url_allowed(&next)?;
            current = next;
            validators = None;
            continue 'redirects;
          }
        }

        let status_code = status.as_u16();
        let retry_after =
          if self.retry_policy.respect_retry_after && retryable_http_status(status_code) {
            parse_retry_after(response.headers())
          } else {
            None
          };

        let encodings = parse_content_encodings(response.headers());
        let mut content_type = response
          .headers()
          .get("content-type")
          .and_then(|h| h.to_str().ok())
          .map(|s| s.to_string());
        let nosniff = header_has_nosniff(response.headers());
        let mut decode_stage = decode_stage_for_content_type(content_type.as_deref());
        let etag = response
          .headers()
          .get("etag")
          .and_then(|h| h.to_str().ok())
          .map(|s| s.to_string());
        let last_modified = response
          .headers()
          .get("last-modified")
          .and_then(|h| h.to_str().ok())
          .map(|s| s.to_string());
        let (access_control_allow_origin, access_control_allow_credentials) =
          parse_cors_response_headers(response.headers());
        let timing_allow_origin = header_values_joined(response.headers(), "timing-allow-origin");
        let response_referrer_policy = header_values_joined(response.headers(), "referrer-policy")
          .as_deref()
          .and_then(ReferrerPolicy::parse_value_list)
          .or(redirect_referrer_policy);
        let cache_policy = parse_http_cache_policy(response.headers());
        let vary = parse_vary_headers(response.headers());
        let final_url = response.url().to_string();
        let response_headers = collect_response_headers(response.headers());
        let allows_empty_body =
          http_response_allows_empty_body(kind, status_code, response.headers());
        let substitute_empty_image_body =
          should_substitute_empty_image_body(kind, status_code, response.headers())
            || should_substitute_akamai_pixel_empty_image_body(
              kind,
              &final_url,
              status_code,
              response.headers(),
            );
        let substitute_captcha_image =
          should_substitute_captcha_image_response(kind, status_code, &final_url);

        let read_limit = allowed_limit.saturating_add(1);
        let mut limited_body = response.take(read_limit as u64);
        let body_result = read_response_prefix(&mut limited_body, read_limit);

        match body_result {
          Ok(body) => {
            if body.len() > allowed_limit {
              finish_network_fetch_diagnostics(network_timer.take());
              if let Some(remaining) = self.policy.remaining_budget() {
                if body.len() > remaining {
                  let err = ResourceError::new(
                    current.clone(),
                    format!(
                      "total bytes budget exceeded ({} > {} bytes remaining)",
                      body.len(),
                      remaining
                    ),
                  )
                  .with_status(status.as_u16())
                  .with_final_url(final_url.clone());
                  return Err(Error::Resource(err));
                }
              }
              let err = ResourceError::new(
                current.clone(),
                format!(
                  "response too large ({} > {} bytes)",
                  body.len(),
                  allowed_limit
                ),
              )
              .with_status(status.as_u16())
              .with_final_url(final_url.clone());
              return Err(Error::Resource(err));
            }

            if let Err(err) = render_control::check_active(stage_hint) {
              finish_network_fetch_diagnostics(network_timer.take());
              return Err(Error::Render(err));
            }
            let mut bytes =
              match decode_content_encodings(body, &encodings, allowed_limit, decode_stage) {
                Ok(decoded) => decoded,
                Err(ContentDecodeError::DeadlineExceeded { stage, elapsed, .. }) => {
                  finish_network_fetch_diagnostics(network_timer.take());
                  return Err(Error::Render(RenderError::Timeout { stage, elapsed }));
                }
                Err(ContentDecodeError::DecompressionFailed { .. })
                  if accept_encoding.is_none() =>
                {
                  finish_network_fetch_diagnostics(network_timer.take());
                  return self.fetch_http_with_accept_inner_reqwest(
                    kind,
                    destination,
                    url,
                    Some("identity"),
                    validators,
                    client_origin,
                    referrer_url,
                    referrer_policy,
                    credentials_mode,
                    deadline,
                    started,
                    auto_fallback,
                  );
                }
                Err(err) => {
                  finish_network_fetch_diagnostics(network_timer.take());
                  let err =
                    err.into_resource_error(current.clone(), status.as_u16(), final_url.clone());
                  return Err(Error::Resource(err));
                }
              };

            record_network_fetch_bytes(bytes.len());
            if bytes.is_empty() && substitute_empty_image_body {
              bytes = OFFLINE_FIXTURE_PLACEHOLDER_PNG.to_vec();
              content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
              decode_stage = decode_stage_for_content_type(content_type.as_deref());
            }
            if should_substitute_markup_image_body(
              kind,
              url,
              &final_url,
              content_type.as_deref(),
              &bytes,
            ) {
              bytes = OFFLINE_FIXTURE_PLACEHOLDER_PNG.to_vec();
              content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
              decode_stage = decode_stage_for_content_type(content_type.as_deref());
            }
            if substitute_captcha_image {
              bytes = OFFLINE_FIXTURE_PLACEHOLDER_PNG.to_vec();
              content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
              decode_stage = decode_stage_for_content_type(content_type.as_deref());
            }
            let is_retryable_status = retryable_http_status(status_code);

            if bytes.is_empty() && http_empty_body_is_error(status_code, allows_empty_body) {
              finish_network_fetch_diagnostics(network_timer.take());
              let mut can_retry = attempt < max_attempts;
              if can_retry {
                let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
                if let Some(retry_after) = retry_after {
                  backoff = backoff.max(retry_after);
                }

                if let Some(deadline) = deadline.as_ref() {
                  if deadline.timeout_limit().is_some() {
                    match deadline.remaining_timeout() {
                      Some(remaining) if !remaining.is_zero() => {
                        let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                        backoff = backoff.min(max_sleep);
                      }
                      _ => can_retry = false,
                    }
                  }
                }
                if let Some(budget) = timeout_budget {
                  match budget.checked_sub(started.elapsed()) {
                    Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                      let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }

                if can_retry {
                  render_control::check_active(stage_hint).map_err(Error::Render)?;
                  log_http_retry(
                    &format!("empty body (status {status_code})"),
                    attempt,
                    max_attempts,
                    &current,
                    backoff,
                  );
                  if !backoff.is_zero() {
                    sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                      .map_err(Error::Render)?;
                  }
                  continue;
                }
                if timeout_budget.is_some() {
                  return Err(budget_exhausted_error(&current, attempt));
                }
              }

              let mut message = "empty HTTP response body".to_string();
              if attempt < max_attempts {
                message.push_str(" (retry aborted: render deadline exceeded)");
              }
              message.push_str(&format_attempt_suffix(attempt, max_attempts));
              let err = ResourceError::new(current.clone(), message)
                .with_status(status_code)
                .with_final_url(final_url.clone());
              return Err(Error::Resource(err));
            }

            if is_retryable_status {
              finish_network_fetch_diagnostics(network_timer.take());
              let mut can_retry = attempt < max_attempts;
              if can_retry {
                let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
                if let Some(retry_after) = retry_after {
                  backoff = backoff.max(retry_after);
                }
                if let Some(deadline) = deadline.as_ref() {
                  if deadline.timeout_limit().is_some() {
                    match deadline.remaining_timeout() {
                      Some(remaining) if !remaining.is_zero() => {
                        let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                        backoff = backoff.min(max_sleep);
                      }
                      _ => can_retry = false,
                    }
                  }
                }
                if let Some(budget) = timeout_budget {
                  match budget.checked_sub(started.elapsed()) {
                    Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                      let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }
                if can_retry {
                  render_control::check_active(stage_hint).map_err(Error::Render)?;
                  log_http_retry(
                    &format!("status {status_code}"),
                    attempt,
                    max_attempts,
                    &current,
                    backoff,
                  );
                  if !backoff.is_zero() {
                    sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                      .map_err(Error::Render)?;
                  }
                  continue;
                }
                if timeout_budget.is_some() {
                  return Err(budget_exhausted_error(&current, attempt));
                }
              }

              // Treat retryable statuses as success once retries are exhausted so caching layers can
              // persist the HTTP response deterministically (especially important for pageset
              // warm-cache runs that want to avoid repeated network fetches for persistent 429/5xx
              // responses). Still surface a hard error when retries are aborted early due to a
              // render deadline; callers can decide whether to fall back to cached bytes.
              if status_code != 202 && (deadline_retries_disabled || attempt < max_attempts) {
                let mut message =
                  "retryable HTTP status (retry aborted: render deadline exceeded)".to_string();
                message.push_str(&format_attempt_suffix(attempt, max_attempts));
                let err = ResourceError::new(current.clone(), message)
                  .with_status(status_code)
                  .with_final_url(final_url.clone());
                return Err(Error::Resource(err));
              }
            }

            if bytes.len() > allowed_limit {
              finish_network_fetch_diagnostics(network_timer.take());
              if let Some(remaining) = self.policy.remaining_budget() {
                if bytes.len() > remaining {
                  let err = ResourceError::new(
                    current.clone(),
                    format!(
                      "total bytes budget exceeded ({} > {} bytes remaining)",
                      bytes.len(),
                      remaining
                    ),
                  )
                  .with_status(status.as_u16())
                  .with_final_url(final_url.clone());
                  return Err(Error::Resource(err));
                }
              }

              let err = ResourceError::new(
                current.clone(),
                format!(
                  "response too large ({} > {} bytes)",
                  bytes.len(),
                  allowed_limit
                ),
              )
              .with_status(status.as_u16())
              .with_final_url(final_url.clone());
              return Err(Error::Resource(err));
            }

            finish_network_fetch_diagnostics(network_timer.take());
            self.policy.reserve_budget(bytes.len())?;
            let mut resource =
              FetchedResource::with_final_url(bytes, content_type, Some(final_url));
            resource.response_headers = Some(response_headers);
            if !encodings.is_empty() {
              resource.content_encoding = Some(encodings.join(", "));
            }
            resource.nosniff = nosniff;
            resource.status = Some(status.as_u16());
            resource.etag = etag;
            resource.last_modified = last_modified;
            resource.access_control_allow_origin = access_control_allow_origin;
            resource.timing_allow_origin = timing_allow_origin;
            resource.vary = vary;
            resource.response_referrer_policy = response_referrer_policy;
            resource.access_control_allow_credentials = access_control_allow_credentials;
            resource.cache_policy = cache_policy;
            render_control::check_root(decode_stage).map_err(Error::Render)?;
            return Ok(resource);
          }
          Err(err) => {
            finish_network_fetch_diagnostics(network_timer.take());
            if attempt < max_attempts && is_retryable_io_error(&err) {
              let mut backoff = compute_backoff(&self.retry_policy, attempt, &current);
              let mut can_retry = true;
              if let Some(deadline) = deadline.as_ref() {
                if deadline.timeout_limit().is_some() {
                  match deadline.remaining_timeout() {
                    Some(remaining) if !remaining.is_zero() => {
                      let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                      backoff = backoff.min(max_sleep);
                    }
                    _ => can_retry = false,
                  }
                }
              }
              if let Some(budget) = timeout_budget {
                match budget.checked_sub(started.elapsed()) {
                  Some(remaining) if remaining > HTTP_DEADLINE_BUFFER => {
                    let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                    backoff = backoff.min(max_sleep);
                  }
                  _ => can_retry = false,
                }
              }
              if can_retry {
                render_control::check_active(stage_hint).map_err(Error::Render)?;
                let reason = err.to_string();
                log_http_retry(&reason, attempt, max_attempts, &current, backoff);
                if !backoff.is_zero() {
                  sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                    .map_err(Error::Render)?;
                }
                continue;
              }
              if timeout_budget.is_some() {
                return Err(budget_exhausted_error(&current, attempt));
              }
            }

            let overall_timeout = deadline.as_ref().and_then(|d| d.timeout_limit());
            let mut message = err.to_string();
            if err.kind() == io::ErrorKind::TimedOut {
              if let Some(overall) = overall_timeout {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout={overall:?})"
                ));
              } else if let Some(budget) = timeout_budget {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout_budget={budget:?})"
                ));
              } else {
                message.push_str(&format!(
                  " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?})"
                ));
              }
            } else {
              message.push_str(&format_attempt_suffix(attempt, max_attempts));
            }

            let err = ResourceError::new(current.clone(), message)
              .with_status(status.as_u16())
              .with_final_url(final_url)
              .with_source(err);
            return Err(Error::Resource(err));
          }
        }
      }
    }

    Err(Error::Resource(
      ResourceError::new(
        url,
        format!("too many redirects (limit {})", self.policy.max_redirects),
      )
      .with_final_url(current),
    ))
  }

  /// Fetch from a file:// URL
  fn fetch_file(&self, kind: FetchContextKind, url: &str) -> Result<FetchedResource> {
    let path_candidates = file_url_path_candidates(url);
    let limit = self.policy.allowed_response_limit()?;
    let mut chosen_path: Option<std::path::PathBuf> = None;
    let mut bytes: Option<Vec<u8>> = None;
    let mut file_too_large = false;
    let mut last_err = None;

    'candidates: for candidate in &path_candidates {
      if let Ok(meta) = std::fs::metadata(candidate) {
        if let Ok(len) = usize::try_from(meta.len()) {
          if len > limit {
            if let Some(remaining) = self.policy.remaining_budget() {
              if len > remaining {
                return Err(policy_error(format!(
                  "total bytes budget exceeded ({} > {} bytes remaining)",
                  len, remaining
                )));
              }
            }
            return Err(policy_error(format!(
              "response too large ({} > {} bytes)",
              len, limit
            )));
          }
        } else {
          return Err(policy_error("file is larger than supported limit"));
        }
      }

      match std::fs::File::open(candidate) {
        Ok(mut file) => match read_response_prefix(&mut file, limit) {
          Ok(read) => {
            let mut candidate_too_large = false;
            if read.len() == limit {
              let mut extra = [0u8; 1];
              loop {
                match file.read(&mut extra) {
                  Ok(0) => break,
                  Ok(_) => {
                    candidate_too_large = true;
                    break;
                  }
                  Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                  Err(err) => {
                    last_err = Some(err);
                    continue 'candidates;
                  }
                }
              }
            }
            chosen_path = Some(candidate.clone());
            bytes = Some(read);
            file_too_large = candidate_too_large;
            break;
          }
          Err(err) => last_err = Some(err),
        },
        Err(err) => last_err = Some(err),
      };
    }

    let chosen_path = chosen_path.ok_or_else(|| {
      let err = last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"));
      Error::Resource(
        ResourceError::new(url.to_string(), err.to_string())
          .with_final_url(url.to_string())
          .with_source(err),
      )
    })?;
    let mut bytes = bytes.unwrap_or_default();
    let observed_len = if file_too_large {
      limit.saturating_add(1)
    } else {
      bytes.len()
    };

    if observed_len > limit {
      if let Some(remaining) = self.policy.remaining_budget() {
        if observed_len > remaining {
          return Err(policy_error(format!(
            "total bytes budget exceeded ({} > {} bytes remaining)",
            observed_len,
            remaining
          )));
        }
      }
      return Err(policy_error(format!(
        "response too large ({} > {} bytes)",
        observed_len,
        limit
      )));
    }
    let path_str = chosen_path.to_string_lossy();
    let mut content_type = guess_content_type_from_path(path_str.as_ref());
    substitute_offline_fixture_placeholder_full(kind, &mut bytes, &mut content_type);

    self.policy.reserve_budget(bytes.len())?;

    render_control::check_root(render_stage_hint_for_context(kind, url))
      .map_err(Error::Render)?;
    Ok(FetchedResource::with_final_url(
      bytes,
      content_type,
      Some(url.to_string()),
    ))
  }

  fn fetch_file_prefix(
    &self,
    kind: FetchContextKind,
    url: &str,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    let path_candidates = file_url_path_candidates(url);
    let limit = self.policy.allowed_response_limit()?;
    let read_limit = max_bytes.min(limit);

    let mut file = None;
    let mut chosen_path: Option<std::path::PathBuf> = None;
    let mut last_err = None;

    for candidate in &path_candidates {
      match std::fs::File::open(candidate) {
        Ok(handle) => {
          file = Some(handle);
          chosen_path = Some(candidate.clone());
          break;
        }
        Err(err) => last_err = Some(err),
      }
    }

    let mut file = file.ok_or_else(|| {
      let err = last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"));
      Error::Resource(
        ResourceError::new(url.to_string(), err.to_string())
          .with_final_url(url.to_string())
          .with_source(err),
      )
    })?;
    let bytes = read_response_prefix(&mut file, read_limit).map_err(|e| {
      Error::Resource(
        ResourceError::new(url.to_string(), e.to_string())
          .with_final_url(url.to_string())
          .with_source(e),
      )
    })?;

    let chosen_path = chosen_path.unwrap_or_else(|| std::path::PathBuf::from(url));
    let path_str = chosen_path.to_string_lossy();
    let mut content_type = guess_content_type_from_path(path_str.as_ref());

    let mut bytes = bytes;
    substitute_offline_fixture_placeholder_prefix(kind, &mut bytes, &mut content_type, read_limit);

    self.policy.reserve_budget(bytes.len())?;

    render_control::check_root(render_stage_hint_for_context(kind, url))
      .map_err(Error::Render)?;
    Ok(FetchedResource::with_final_url(
      bytes,
      content_type,
      Some(url.to_string()),
    ))
  }

  /// Decode a data: URL
  fn fetch_data(&self, kind: FetchContextKind, url: &str) -> Result<FetchedResource> {
    let limit = self.policy.allowed_response_limit()?;
    // Decode at most `limit + 1` bytes so oversized data: URLs fail without allocating the entire
    // payload (which could otherwise abort the process on OOM).
    let decode_limit = limit.saturating_add(1);
    let mut resource = data_url::decode_data_url_prefix(url, decode_limit)?;
    substitute_offline_fixture_placeholder_full(
      kind,
      &mut resource.bytes,
      &mut resource.content_type,
    );
    let len = resource.bytes.len();
    if len > limit {
      if let Some(remaining) = self.policy.remaining_budget() {
        if len > remaining {
          return Err(policy_error(format!(
            "total bytes budget exceeded ({} > {} bytes remaining)",
            len, remaining
          )));
        }
      }
      return Err(policy_error(format!(
        "response too large ({} > {} bytes)",
        len, limit
      )));
    }
    self.policy.reserve_budget(len)?;
    render_control::check_root(render_stage_hint_for_context(kind, url))
      .map_err(Error::Render)?;
    Ok(resource)
  }

  fn fetch_data_prefix(
    &self,
    kind: FetchContextKind,
    url: &str,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    let limit = self.policy.allowed_response_limit()?;
    let read_limit = max_bytes.min(limit);
    let mut resource = data_url::decode_data_url_prefix(url, read_limit)?;
    substitute_offline_fixture_placeholder_prefix(
      kind,
      &mut resource.bytes,
      &mut resource.content_type,
      read_limit,
    );
    self.policy.reserve_budget(resource.bytes.len())?;
    render_control::check_root(render_stage_hint_from_url(url)).map_err(Error::Render)?;
    Ok(resource)
  }
}

impl Default for HttpFetcher {
  fn default() -> Self {
    let policy = ResourcePolicy::default();
    let cookie_jar = Arc::new(ReqwestCookieJar::default());
    let reqwest_client = Self::build_reqwest_client();
    Self {
      user_agent: DEFAULT_USER_AGENT.to_string(),
      accept_language: DEFAULT_ACCEPT_LANGUAGE.to_string(),
      agent: Self::build_agent(&policy),
      reqwest_client,
      cookie_jar,
      policy,
      retry_policy: HttpRetryPolicy::default(),
    }
  }
}

impl ResourceFetcher for HttpFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    self.fetch_with_context(FetchContextKind::Other, url)
  }

  fn fetch_with_context(&self, kind: FetchContextKind, url: &str) -> Result<FetchedResource> {
    render_control::check_root(render_stage_hint_for_context(kind, url))
      .map_err(Error::Render)?;
    match self.policy.ensure_url_allowed(url)? {
      ResourceScheme::Data => self.fetch_data(kind, url),
      ResourceScheme::File => self.fetch_file(kind, url),
      ResourceScheme::Http | ResourceScheme::Https => self.fetch_http(kind, url),
      ResourceScheme::Relative => self.fetch_file(kind, &format!("file://{}", url)),
      ResourceScheme::Other => Err(policy_error("unsupported URL scheme")),
    }
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    let kind: FetchContextKind = req.destination.into();
    render_control::check_root(render_stage_hint_for_context(kind, req.url))
      .map_err(Error::Render)?;
    match self.policy.ensure_url_allowed(req.url)? {
      ResourceScheme::Data => self.fetch_data(kind, req.url),
      ResourceScheme::File => self.fetch_file(kind, req.url),
      ResourceScheme::Http | ResourceScheme::Https => {
        self.fetch_http_with_context(
          kind,
          req.destination,
          req.url,
          None,
          None,
          req.client_origin,
          req.referrer_url,
          req.referrer_policy,
          req.credentials_mode,
        )
      }
      ResourceScheme::Relative => self.fetch_file(kind, &format!("file://{}", req.url)),
      ResourceScheme::Other => Err(policy_error("unsupported URL scheme")),
    }
  }

  fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
    // Use the same header construction logic as the HTTP backend so cache variant keys match the
    // actual request header values (including browser-like `Origin`/`Referer` handling).
    let headers = build_http_header_pairs(
      req.url,
      &self.user_agent,
      &self.accept_language,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      req.destination,
      req.client_origin,
      req.referrer_url,
      req.referrer_policy,
    );
    for (name, value) in headers {
      if name.eq_ignore_ascii_case(header_name) {
        return Some(value);
      }
    }
    // Some commonly varied headers (notably `Origin`/`Referer`) are only emitted for certain
    // destinations. Treat their absence deterministically as an empty string so callers can still
    // compute a stable variant key.
    if header_name.eq_ignore_ascii_case("origin")
      || header_name.eq_ignore_ascii_case("accept-encoding")
      || header_name.eq_ignore_ascii_case("accept-language")
      || header_name.eq_ignore_ascii_case("user-agent")
      || header_name.eq_ignore_ascii_case("referer")
    {
      return Some(String::new());
    }

    // We cannot reliably determine the value for other headers (e.g. `Cookie` supplied by the
    // backend cookie jar). Treat such `Vary` responses as uncacheable to avoid poisoning.
    None
  }

  fn fetch_with_request_and_validation(
    &self,
    req: FetchRequest<'_>,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    let kind: FetchContextKind = req.destination.into();
    render_control::check_root(render_stage_hint_for_context(kind, req.url))
      .map_err(Error::Render)?;
    match self.policy.ensure_url_allowed(req.url)? {
      ResourceScheme::Data => self.fetch_data(kind, req.url),
      ResourceScheme::File => self.fetch_file(kind, req.url),
      ResourceScheme::Http | ResourceScheme::Https => self.fetch_http_with_context(
        kind,
        req.destination,
        req.url,
        None,
        Some(HttpCacheValidators {
          etag,
          last_modified,
        }),
        req.client_origin,
        req.referrer_url,
        req.referrer_policy,
        req.credentials_mode,
      ),
      ResourceScheme::Relative => self.fetch_file(kind, &format!("file://{}", req.url)),
      ResourceScheme::Other => Err(policy_error("unsupported URL scheme")),
    }
  }

  fn fetch_partial(&self, url: &str, max_bytes: usize) -> Result<FetchedResource> {
    self.fetch_partial_with_context(FetchContextKind::Other, url, max_bytes)
  }

  fn fetch_partial_with_context(
    &self,
    kind: FetchContextKind,
    url: &str,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    if max_bytes == 0 {
      let mut res = self.fetch_with_context(kind, url)?;
      res.bytes.clear();
      return Ok(res);
    }

    render_control::check_root(render_stage_hint_for_context(kind, url))
      .map_err(Error::Render)?;
    match self.policy.ensure_url_allowed(url)? {
      ResourceScheme::Data => self.fetch_data_prefix(kind, url, max_bytes),
      ResourceScheme::File => self.fetch_file_prefix(kind, url, max_bytes),
      ResourceScheme::Http | ResourceScheme::Https => {
        let destination: FetchDestination = kind.into();
        let default_credentials = FetchRequest::new(url, destination).credentials_mode;
        self.fetch_http_partial(
          kind,
          destination,
          url,
          max_bytes,
          None,
          None,
          ReferrerPolicy::default(),
          default_credentials,
        )
      }
      ResourceScheme::Relative => {
        self.fetch_file_prefix(kind, &format!("file://{}", url), max_bytes)
      }
      ResourceScheme::Other => Err(policy_error("unsupported URL scheme")),
    }
  }

  fn fetch_partial_with_request(
    &self,
    req: FetchRequest<'_>,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    let kind: FetchContextKind = req.destination.into();
    if max_bytes == 0 {
      let mut res = self.fetch_with_request(req)?;
      res.bytes.clear();
      return Ok(res);
    }

    render_control::check_root(render_stage_hint_for_context(kind, req.url))
      .map_err(Error::Render)?;
    match self.policy.ensure_url_allowed(req.url)? {
      ResourceScheme::Data => self.fetch_data_prefix(kind, req.url, max_bytes),
      ResourceScheme::File => self.fetch_file_prefix(kind, req.url, max_bytes),
      ResourceScheme::Http | ResourceScheme::Https => {
        self.fetch_http_partial(
          kind,
          req.destination,
          req.url,
          max_bytes,
          req.client_origin,
          req.referrer_url,
          req.referrer_policy,
          req.credentials_mode,
        )
      }
      ResourceScheme::Relative => {
        self.fetch_file_prefix(kind, &format!("file://{}", req.url), max_bytes)
      }
      ResourceScheme::Other => Err(policy_error("unsupported URL scheme")),
    }
  }

  fn fetch_with_validation(
    &self,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    self.fetch_with_validation_and_context(FetchContextKind::Other, url, etag, last_modified)
  }

  fn fetch_with_validation_and_context(
    &self,
    kind: FetchContextKind,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    render_control::check_root(render_stage_hint_for_context(kind, url))
      .map_err(Error::Render)?;
    match self.policy.ensure_url_allowed(url)? {
      ResourceScheme::Http | ResourceScheme::Https => {
        let destination: FetchDestination = kind.into();
        let default_credentials = FetchRequest::new(url, destination).credentials_mode;
        self.fetch_http_with_context(
          kind,
          destination,
          url,
          None,
          Some(HttpCacheValidators {
            etag,
            last_modified,
          }),
          None,
          None,
          ReferrerPolicy::default(),
          default_credentials,
        )
      }
      _ => self.fetch_with_context(kind, url),
    }
  }
}

fn parse_http_cache_policy(headers: &HeaderMap) -> Option<HttpCachePolicy> {
  let mut policy = HttpCachePolicy::default();
  if let Some(value) = headers.get("cache-control").and_then(|h| h.to_str().ok()) {
    for directive in value.split(',') {
      let directive = directive.trim();
      if directive.is_empty() {
        continue;
      }
      let (name, value) = directive
        .split_once('=')
        .map(|(k, v)| (k.trim(), Some(v.trim())))
        .unwrap_or((directive, None));
      match name.to_ascii_lowercase().as_str() {
        "max-age" => {
          if let Some(raw) = value {
            let raw = raw.trim_matches('"');
            if let Ok(parsed) = raw.parse::<u64>() {
              policy.max_age = Some(parsed);
            }
          }
        }
        "no-cache" => policy.no_cache = true,
        "no-store" => policy.no_store = true,
        "must-revalidate" => policy.must_revalidate = true,
        _ => {}
      }
    }
  }

  if let Some(expires) = headers
    .get("expires")
    .and_then(|h| h.to_str().ok())
    .and_then(|v| parse_http_date(v).ok())
  {
    policy.expires = Some(expires);
  }

  if policy.is_empty() {
    None
  } else {
    Some(policy)
  }
}

fn allow_unhandled_vary_env() -> bool {
  runtime::runtime_toggles().truthy_with_default("FASTR_CACHE_ALLOW_VARY_UNHANDLED", false)
}

fn vary_contains_star(vary: &str) -> bool {
  vary.split(',').any(|part| part.trim() == "*")
}

fn vary_is_cacheable(vary: &str, _kind: FetchContextKind, _origin_key: Option<&str>) -> bool {
  for part in vary.split(',') {
    let part = part.trim();
    if part.is_empty() {
      continue;
    }
    if part == "*" {
      return false;
    }
    match part.to_ascii_lowercase().as_str() {
      // We always decode supported content-encodings, so the cached bytes are the decoded
      // representation.
      "accept-encoding" => {}
      // `Accept-Language` and `User-Agent` are stable within a render process and are included in
      // the computed `Vary` key when servers opt into varying on them.
      "accept-language" | "user-agent" => {}
      // `Referer` is derived from the request referrer URL and is included in the `Vary` key.
      "referer" => {}
      // The browser-like request profile derives `Accept` solely from the fetch destination, which
      // is already part of the cache key (`FetchContextKind`). When browser headers are disabled
      // we use a single default `Accept` value for all requests.
      "accept" => {}
      // `Sec-Fetch-*` headers and `Upgrade-Insecure-Requests` are derived solely from the fetch
      // destination, which is already part of the cache key (`FetchContextKind`).
      "sec-fetch-dest" | "sec-fetch-mode" | "sec-fetch-user" | "upgrade-insecure-requests" => {}
      // `Origin` is also safe because we include it in the computed `Vary` key (or treat the
      // response as uncacheable if we cannot determine the `Origin` request header value).
      "origin" => {}
      _ => return false,
    }
  }
  true
}

// ============================================================================
// CachingFetcher - in-memory cache + single-flight
// ============================================================================

/// Policy controlling how stale cached entries are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStalePolicy {
  /// Preserve existing behavior: stale entries trigger revalidation (conditional request) when
  /// validators are available, falling back to cached bytes only after the network attempt fails.
  Revalidate,
  /// When a render deadline with a timeout is active, serve cached bytes immediately even if the
  /// entry is stale or requires revalidation.
  UseStaleWhenDeadline,
}

/// Configuration for [`CachingFetcher`].
#[derive(Debug, Clone, Copy)]
pub struct CachingFetcherConfig {
  /// Maximum total bytes to keep in-memory across cached entries. `0` disables the limit.
  pub max_bytes: usize,
  /// Maximum number of cached entries. `0` disables the limit.
  pub max_items: usize,
  /// Whether to cache failed fetch results (by error string) to avoid hammering endpoints.
  pub cache_errors: bool,
  /// Whether to use HTTP validators (ETag/Last-Modified) when present.
  pub honor_http_cache_headers: bool,
  /// Whether to honor HTTP freshness metadata (Cache-Control/Expires) to avoid revalidation.
  pub honor_http_cache_freshness: bool,
  /// Whether to cache responses that set `Cache-Control: no-store`.
  ///
  /// When enabled, `no-store` entries are treated as "always stale": callers will normally
  /// attempt a network refresh, but the cached bytes can be used as a fallback (or served
  /// immediately when `stale_policy` is [`CacheStalePolicy::UseStaleWhenDeadline`] under an active
  /// render deadline).
  ///
  /// Defaults to `false` to remain spec-faithful unless explicitly enabled by tooling.
  pub allow_no_store: bool,
  /// Whether to cache responses that include `Vary` header names that we do not explicitly
  /// partition/normalize.
  ///
  /// When `false` (default), any response that includes `Vary: *` or a header name outside the
  /// small allowlist in [`vary_is_cacheable`] is treated as uncacheable and will not be stored.
  pub allow_unhandled_vary: bool,
  /// Policy controlling whether stale cached entries are served without revalidation when a
  /// render deadline is active.
  pub stale_policy: CacheStalePolicy,
}

impl Default for CachingFetcherConfig {
  fn default() -> Self {
    Self {
      max_bytes: 64 * 1024 * 1024,
      max_items: 512,
      cache_errors: true,
      honor_http_cache_headers: true,
      honor_http_cache_freshness: false,
      allow_no_store: false,
      allow_unhandled_vary: false,
      stale_policy: CacheStalePolicy::Revalidate,
    }
  }
}

#[derive(Clone)]
enum CacheValue {
  Resource(FetchedResource),
  Error(Error),
}

impl CacheValue {
  fn size(&self) -> usize {
    match self {
      Self::Resource(res) => res.bytes.len(),
      Self::Error(_) => 0,
    }
  }

  fn as_result(&self) -> Result<FetchedResource> {
    match self {
      Self::Resource(res) => Ok(res.clone()),
      Self::Error(err) => Err(err.clone()),
    }
  }
}

#[derive(Clone)]
struct CacheEntry {
  value: CacheValue,
  etag: Option<String>,
  last_modified: Option<String>,
  http_cache: Option<CachedHttpMetadata>,
}

impl CacheEntry {
  fn weight(&self) -> usize {
    self.value.size()
  }
}

#[derive(Clone)]
struct CachedSnapshot {
  value: CacheValue,
  etag: Option<String>,
  last_modified: Option<String>,
  http_cache: Option<CachedHttpMetadata>,
}

impl CachedSnapshot {
  fn is_successful_http_response(&self) -> bool {
    match &self.value {
      CacheValue::Resource(res) => res.status.map(|code| code < 400).unwrap_or(true),
      CacheValue::Error(_) => false,
    }
  }

  #[cfg(feature = "disk_cache")]
  pub(crate) fn as_resource(&self) -> Option<FetchedResource> {
    match &self.value {
      CacheValue::Resource(res) => Some(res.clone()),
      CacheValue::Error(_) => None,
    }
  }
}

#[derive(Clone)]
pub(crate) enum CacheAction {
  UseCached,
  Validate {
    etag: Option<String>,
    last_modified: Option<String>,
  },
  Fetch,
}

#[derive(Clone)]
pub(crate) struct CachePlan {
  pub(crate) cached: Option<CachedSnapshot>,
  pub(crate) action: CacheAction,
  pub(crate) is_stale: bool,
}

#[derive(Debug, Default, Clone)]
pub struct ResourceCacheDiagnostics {
  pub fresh_hits: usize,
  pub stale_hits: usize,
  pub revalidated_hits: usize,
  pub misses: usize,
  /// Total bytes returned from the caching layer (fresh/stale/revalidated hits only).
  ///
  /// This is intentionally "bytes served from cache", not "bytes stored in cache", and excludes
  /// cache misses that hit the network.
  pub resource_cache_bytes: usize,
  pub disk_cache_hits: usize,
  pub disk_cache_misses: usize,
  pub disk_cache_bytes: usize,
  pub disk_cache_ms: f64,
  /// Number of times disk cache reads waited for an in-progress writer to release a `.lock` file.
  pub disk_cache_lock_waits: usize,
  /// Time spent waiting for disk cache `.lock` files to clear.
  pub disk_cache_lock_wait_ms: f64,
  /// Number of times a `CachingFetcher` caller waited for another thread to finish an in-flight
  /// fetch (single-flight de-duplication).
  pub fetch_inflight_waits: usize,
  /// Total time spent waiting for in-flight fetches to resolve.
  pub fetch_inflight_wait_ms: f64,
  pub network_fetches: usize,
  pub network_fetch_bytes: usize,
  pub network_fetch_ms: f64,
}

struct ResourceCacheDiagnosticsState {
  fresh_hits: AtomicUsize,
  stale_hits: AtomicUsize,
  revalidated_hits: AtomicUsize,
  misses: AtomicUsize,
  resource_cache_bytes: AtomicUsize,
  disk_cache_hits: AtomicUsize,
  disk_cache_misses: AtomicUsize,
  disk_cache_bytes: AtomicUsize,
  disk_cache_ns: AtomicU64,
  disk_cache_lock_waits: AtomicUsize,
  disk_cache_lock_wait_ns: AtomicU64,
  fetch_inflight_waits: AtomicUsize,
  fetch_inflight_wait_ns: AtomicU64,
  network_fetches: AtomicUsize,
  network_fetch_bytes: AtomicUsize,
  network_fetch_ns: AtomicU64,
}

#[derive(Clone, Copy)]
struct ResourceCacheDiagnosticsSnapshot {
  fresh_hits: usize,
  stale_hits: usize,
  revalidated_hits: usize,
  misses: usize,
  resource_cache_bytes: usize,
  disk_cache_hits: usize,
  disk_cache_misses: usize,
  disk_cache_bytes: usize,
  disk_cache_ns: u64,
  disk_cache_lock_waits: usize,
  disk_cache_lock_wait_ns: u64,
  fetch_inflight_waits: usize,
  fetch_inflight_wait_ns: u64,
  network_fetches: usize,
  network_fetch_bytes: usize,
  network_fetch_ns: u64,
}

#[derive(Default)]
struct ResourceCacheDiagnosticsBaseline {
  baseline: Option<ResourceCacheDiagnosticsSnapshot>,
}

impl Drop for ResourceCacheDiagnosticsBaseline {
  fn drop(&mut self) {
    if self.baseline.is_some() {
      end_resource_cache_diagnostics_session();
    }
  }
}

thread_local! {
  static RESOURCE_CACHE_DIAGNOSTICS_BASELINE: RefCell<ResourceCacheDiagnosticsBaseline> =
    RefCell::new(ResourceCacheDiagnosticsBaseline::default());
  static RESOURCE_CACHE_DIAGNOSTICS_THREAD_ENABLED: Cell<bool> = const { Cell::new(false) };
}

static RESOURCE_CACHE_DIAGNOSTICS_ACTIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);

/// Enables resource-cache diagnostics collection for the current thread.
///
/// Resource fetching can happen on helper threads (e.g. speculative prefetch / parallel crawlers).
/// Make collection opt-in per thread so unrelated work running concurrently in the process doesn't
/// pollute per-render diagnostics snapshots.
pub(crate) struct ResourceCacheDiagnosticsThreadGuard {
  prev_enabled: bool,
}

impl ResourceCacheDiagnosticsThreadGuard {
  pub(crate) fn enter() -> Self {
    let prev_enabled = RESOURCE_CACHE_DIAGNOSTICS_THREAD_ENABLED.with(|cell| {
      let prev = cell.get();
      cell.set(true);
      prev
    });
    Self { prev_enabled }
  }
}

impl Drop for ResourceCacheDiagnosticsThreadGuard {
  fn drop(&mut self) {
    RESOURCE_CACHE_DIAGNOSTICS_THREAD_ENABLED.with(|cell| cell.set(self.prev_enabled));
  }
}

static RESOURCE_CACHE_DIAGNOSTICS: ResourceCacheDiagnosticsState = ResourceCacheDiagnosticsState {
  fresh_hits: AtomicUsize::new(0),
  stale_hits: AtomicUsize::new(0),
  revalidated_hits: AtomicUsize::new(0),
  misses: AtomicUsize::new(0),
  resource_cache_bytes: AtomicUsize::new(0),
  disk_cache_hits: AtomicUsize::new(0),
  disk_cache_misses: AtomicUsize::new(0),
  disk_cache_bytes: AtomicUsize::new(0),
  disk_cache_ns: AtomicU64::new(0),
  disk_cache_lock_waits: AtomicUsize::new(0),
  disk_cache_lock_wait_ns: AtomicU64::new(0),
  fetch_inflight_waits: AtomicUsize::new(0),
  fetch_inflight_wait_ns: AtomicU64::new(0),
  network_fetches: AtomicUsize::new(0),
  network_fetch_bytes: AtomicUsize::new(0),
  network_fetch_ns: AtomicU64::new(0),
};

fn resource_cache_diagnostics_snapshot() -> ResourceCacheDiagnosticsSnapshot {
  ResourceCacheDiagnosticsSnapshot {
    fresh_hits: RESOURCE_CACHE_DIAGNOSTICS
      .fresh_hits
      .load(Ordering::Relaxed),
    stale_hits: RESOURCE_CACHE_DIAGNOSTICS
      .stale_hits
      .load(Ordering::Relaxed),
    revalidated_hits: RESOURCE_CACHE_DIAGNOSTICS
      .revalidated_hits
      .load(Ordering::Relaxed),
    misses: RESOURCE_CACHE_DIAGNOSTICS.misses.load(Ordering::Relaxed),
    resource_cache_bytes: RESOURCE_CACHE_DIAGNOSTICS
      .resource_cache_bytes
      .load(Ordering::Relaxed),
    disk_cache_hits: RESOURCE_CACHE_DIAGNOSTICS
      .disk_cache_hits
      .load(Ordering::Relaxed),
    disk_cache_misses: RESOURCE_CACHE_DIAGNOSTICS
      .disk_cache_misses
      .load(Ordering::Relaxed),
    disk_cache_bytes: RESOURCE_CACHE_DIAGNOSTICS
      .disk_cache_bytes
      .load(Ordering::Relaxed),
    disk_cache_ns: RESOURCE_CACHE_DIAGNOSTICS
      .disk_cache_ns
      .load(Ordering::Relaxed),
    disk_cache_lock_waits: RESOURCE_CACHE_DIAGNOSTICS
      .disk_cache_lock_waits
      .load(Ordering::Relaxed),
    disk_cache_lock_wait_ns: RESOURCE_CACHE_DIAGNOSTICS
      .disk_cache_lock_wait_ns
      .load(Ordering::Relaxed),
    fetch_inflight_waits: RESOURCE_CACHE_DIAGNOSTICS
      .fetch_inflight_waits
      .load(Ordering::Relaxed),
    fetch_inflight_wait_ns: RESOURCE_CACHE_DIAGNOSTICS
      .fetch_inflight_wait_ns
      .load(Ordering::Relaxed),
    network_fetches: RESOURCE_CACHE_DIAGNOSTICS
      .network_fetches
      .load(Ordering::Relaxed),
    network_fetch_bytes: RESOURCE_CACHE_DIAGNOSTICS
      .network_fetch_bytes
      .load(Ordering::Relaxed),
    network_fetch_ns: RESOURCE_CACHE_DIAGNOSTICS
      .network_fetch_ns
      .load(Ordering::Relaxed),
  }
}

fn resource_cache_diagnostics_enabled() -> bool {
  if RESOURCE_CACHE_DIAGNOSTICS_ACTIVE_SESSIONS.load(Ordering::Relaxed) == 0 {
    return false;
  }
  RESOURCE_CACHE_DIAGNOSTICS_THREAD_ENABLED.with(|cell| cell.get())
}

fn end_resource_cache_diagnostics_session() {
  // Avoid underflow: diagnostics state can leak when callers enable collection but never call
  // `take_resource_cache_diagnostics()` (e.g. panic during a diagnostics-enabled render). Use
  // `fetch_update` so a leaked session doesn't permanently keep diagnostics enabled.
  let _ = RESOURCE_CACHE_DIAGNOSTICS_ACTIVE_SESSIONS.fetch_update(
    Ordering::Relaxed,
    Ordering::Relaxed,
    |value| value.checked_sub(1),
  );
}

pub(crate) fn enable_resource_cache_diagnostics() {
  let baseline = resource_cache_diagnostics_snapshot();
  let enabled = RESOURCE_CACHE_DIAGNOSTICS_BASELINE.with(|cell| {
    let mut guard = cell.borrow_mut();
    let was_enabled = guard.baseline.is_some();
    // Reset the baseline every time enable is called so a panic that skips
    // `take_resource_cache_diagnostics()` doesn't permanently poison subsequent sessions (e.g.
    // pageset dump capture after a panic).
    guard.baseline = Some(baseline);
    !was_enabled
  });
  RESOURCE_CACHE_DIAGNOSTICS_THREAD_ENABLED.with(|cell| cell.set(true));
  if enabled {
    RESOURCE_CACHE_DIAGNOSTICS_ACTIVE_SESSIONS.fetch_add(1, Ordering::Relaxed);
  }
}

pub(crate) fn take_resource_cache_diagnostics() -> Option<ResourceCacheDiagnostics> {
  let baseline = RESOURCE_CACHE_DIAGNOSTICS_BASELINE.with(|cell| {
    let mut guard = cell.borrow_mut();
    guard.baseline.take()
  })?;
  RESOURCE_CACHE_DIAGNOSTICS_THREAD_ENABLED.with(|cell| cell.set(false));
  end_resource_cache_diagnostics_session();

  let current = resource_cache_diagnostics_snapshot();
  let disk_cache_ns = current.disk_cache_ns.saturating_sub(baseline.disk_cache_ns);
  let disk_cache_lock_wait_ns = current
    .disk_cache_lock_wait_ns
    .saturating_sub(baseline.disk_cache_lock_wait_ns);
  let fetch_inflight_wait_ns = current
    .fetch_inflight_wait_ns
    .saturating_sub(baseline.fetch_inflight_wait_ns);
  let network_fetch_ns = current
    .network_fetch_ns
    .saturating_sub(baseline.network_fetch_ns);
  Some(ResourceCacheDiagnostics {
    fresh_hits: current.fresh_hits.saturating_sub(baseline.fresh_hits),
    stale_hits: current.stale_hits.saturating_sub(baseline.stale_hits),
    revalidated_hits: current
      .revalidated_hits
      .saturating_sub(baseline.revalidated_hits),
    misses: current.misses.saturating_sub(baseline.misses),
    resource_cache_bytes: current
      .resource_cache_bytes
      .saturating_sub(baseline.resource_cache_bytes),
    disk_cache_hits: current
      .disk_cache_hits
      .saturating_sub(baseline.disk_cache_hits),
    disk_cache_misses: current
      .disk_cache_misses
      .saturating_sub(baseline.disk_cache_misses),
    disk_cache_bytes: current
      .disk_cache_bytes
      .saturating_sub(baseline.disk_cache_bytes),
    disk_cache_ms: (disk_cache_ns as f64) / 1_000_000.0,
    disk_cache_lock_waits: current
      .disk_cache_lock_waits
      .saturating_sub(baseline.disk_cache_lock_waits),
    disk_cache_lock_wait_ms: (disk_cache_lock_wait_ns as f64) / 1_000_000.0,
    fetch_inflight_waits: current
      .fetch_inflight_waits
      .saturating_sub(baseline.fetch_inflight_waits),
    fetch_inflight_wait_ms: (fetch_inflight_wait_ns as f64) / 1_000_000.0,
    network_fetches: current
      .network_fetches
      .saturating_sub(baseline.network_fetches),
    network_fetch_bytes: current
      .network_fetch_bytes
      .saturating_sub(baseline.network_fetch_bytes),
    network_fetch_ms: (network_fetch_ns as f64) / 1_000_000.0,
  })
}

fn record_cache_fresh_hit() {
  if !resource_cache_diagnostics_enabled() {
    return;
  }
  RESOURCE_CACHE_DIAGNOSTICS
    .fresh_hits
    .fetch_add(1, Ordering::Relaxed);
}

fn record_cache_stale_hit() {
  if !resource_cache_diagnostics_enabled() {
    return;
  }
  RESOURCE_CACHE_DIAGNOSTICS
    .stale_hits
    .fetch_add(1, Ordering::Relaxed);
}

fn record_cache_revalidated_hit() {
  if !resource_cache_diagnostics_enabled() {
    return;
  }
  RESOURCE_CACHE_DIAGNOSTICS
    .revalidated_hits
    .fetch_add(1, Ordering::Relaxed);
}

fn record_cache_miss() {
  if !resource_cache_diagnostics_enabled() {
    return;
  }
  RESOURCE_CACHE_DIAGNOSTICS
    .misses
    .fetch_add(1, Ordering::Relaxed);
}

fn record_resource_cache_bytes(bytes: usize) {
  if !resource_cache_diagnostics_enabled() {
    return;
  }
  RESOURCE_CACHE_DIAGNOSTICS
    .resource_cache_bytes
    .fetch_add(bytes, Ordering::Relaxed);
}

#[cfg(feature = "disk_cache")]
fn record_disk_cache_hit() {
  if !resource_cache_diagnostics_enabled() {
    return;
  }
  RESOURCE_CACHE_DIAGNOSTICS
    .disk_cache_hits
    .fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "disk_cache")]
fn record_disk_cache_miss() {
  if !resource_cache_diagnostics_enabled() {
    return;
  }
  RESOURCE_CACHE_DIAGNOSTICS
    .disk_cache_misses
    .fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "disk_cache")]
fn record_disk_cache_bytes(bytes: usize) {
  if !resource_cache_diagnostics_enabled() {
    return;
  }
  RESOURCE_CACHE_DIAGNOSTICS
    .disk_cache_bytes
    .fetch_add(bytes, Ordering::Relaxed);
}

#[cfg(feature = "disk_cache")]
fn start_disk_cache_diagnostics() -> Option<Instant> {
  resource_cache_diagnostics_enabled().then(Instant::now)
}

#[cfg(feature = "disk_cache")]
fn finish_disk_cache_diagnostics(start: Option<Instant>) {
  let Some(start) = start else {
    return;
  };
  let elapsed = start.elapsed();
  let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
  RESOURCE_CACHE_DIAGNOSTICS
    .disk_cache_ns
    .fetch_add(nanos, Ordering::Relaxed);
}

#[cfg(feature = "disk_cache")]
fn start_disk_cache_lock_wait_diagnostics() -> Option<Instant> {
  if !resource_cache_diagnostics_enabled() {
    return None;
  }
  RESOURCE_CACHE_DIAGNOSTICS
    .disk_cache_lock_waits
    .fetch_add(1, Ordering::Relaxed);
  Some(Instant::now())
}

#[cfg(feature = "disk_cache")]
fn finish_disk_cache_lock_wait_diagnostics(start: Option<Instant>) {
  let Some(start) = start else {
    return;
  };
  let elapsed = start.elapsed();
  let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
  RESOURCE_CACHE_DIAGNOSTICS
    .disk_cache_lock_wait_ns
    .fetch_add(nanos, Ordering::Relaxed);
}

fn start_fetch_inflight_wait_diagnostics() -> Option<Instant> {
  if !resource_cache_diagnostics_enabled() {
    return None;
  }
  RESOURCE_CACHE_DIAGNOSTICS
    .fetch_inflight_waits
    .fetch_add(1, Ordering::Relaxed);
  Some(Instant::now())
}

fn finish_fetch_inflight_wait_diagnostics(start: Option<Instant>) {
  let Some(start) = start else {
    return;
  };
  let elapsed = start.elapsed();
  let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
  RESOURCE_CACHE_DIAGNOSTICS
    .fetch_inflight_wait_ns
    .fetch_add(nanos, Ordering::Relaxed);
}

fn start_network_fetch_diagnostics() -> Option<Instant> {
  if !resource_cache_diagnostics_enabled() {
    return None;
  }
  RESOURCE_CACHE_DIAGNOSTICS
    .network_fetches
    .fetch_add(1, Ordering::Relaxed);
  Some(Instant::now())
}

fn record_network_fetch_bytes(bytes: usize) {
  if !resource_cache_diagnostics_enabled() {
    return;
  }
  RESOURCE_CACHE_DIAGNOSTICS
    .network_fetch_bytes
    .fetch_add(bytes, Ordering::Relaxed);
}

fn finish_network_fetch_diagnostics(start: Option<Instant>) {
  let Some(start) = start else {
    return;
  };
  let elapsed = start.elapsed();
  let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
  RESOURCE_CACHE_DIAGNOSTICS
    .network_fetch_ns
    .fetch_add(nanos, Ordering::Relaxed);
}

/// Reserve bytes against a configured policy for a resource being returned to a caller.
///
/// The same [`ResourcePolicy`] can be shared across fetchers and caches; clones share the
/// underlying [`ResourceBudget`], so cached hits and cloned results are accounted for even when
/// they avoid network I/O.
fn reserve_policy_bytes(policy: &Option<ResourcePolicy>, resource: &FetchedResource) -> Result<()> {
  if let Some(policy) = policy {
    policy.reserve_budget(resource.bytes.len())?;
  }
  Ok(())
}

const VARY_KEY_EMPTY: &str = "";
// Conservative header set used to avoid single-flight de-duplication across requests that may
// produce different `Vary` variants before we know the server-provided `Vary` list.
const INFLIGHT_VARY_SIGNATURE_HEADERS: &str =
  "accept-encoding, accept-language, origin, referer, user-agent";

fn inflight_signature_for_request<F: ResourceFetcher>(fetcher: &F, request: FetchRequest<'_>) -> String {
  compute_vary_key_for_request(fetcher, request, Some(INFLIGHT_VARY_SIGNATURE_HEADERS))
    .unwrap_or_else(|| format!("fallback:{:?}:{:?}", request.destination, request.referrer_url))
}

/// Compute a deterministic `Vary` variant key for a request.
///
/// Returns `None` when the response is uncacheable (`Vary: *`) or when the fetcher cannot
/// determine the value of a required request header.
fn compute_vary_key_for_request<F: ResourceFetcher>(
  fetcher: &F,
  req: FetchRequest<'_>,
  vary: Option<&str>,
) -> Option<String> {
  let Some(vary) = vary.filter(|v| !v.trim().is_empty()) else {
    return Some(VARY_KEY_EMPTY.to_string());
  };
  if vary.trim() == "*" {
    return None;
  }

  let mut hasher = Sha256::new();
  let mut saw_field = false;
  for field in vary.split(',') {
    let name = field.trim();
    if name.is_empty() {
      continue;
    }
    saw_field = true;
    let value = fetcher.request_header_value(req, name)?;
    hasher.update(name.as_bytes());
    hasher.update(b"\n");
    hasher.update(value.as_bytes());
    hasher.update(b"\n");
  }

  if !saw_field {
    return Some(VARY_KEY_EMPTY.to_string());
  }

  let digest = hasher.finalize();
  const HEX: &[u8; 16] = b"0123456789abcdef";
  let mut out = String::with_capacity(64);
  for &b in digest.iter() {
    out.push(HEX[(b >> 4) as usize] as char);
    out.push(HEX[(b & 0x0f) as usize] as char);
  }
  Some(out)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
  kind: FetchContextKind,
  url: String,
  origin_key: Option<String>,
  credentials_mode: FetchCredentialsMode,
}

#[derive(Clone)]
struct CacheVariants {
  vary: Option<String>,
  entries: HashMap<String, CacheEntry>,
}

impl CacheVariants {
  fn new(vary: Option<String>) -> Self {
    Self {
      vary,
      entries: HashMap::new(),
    }
  }

  fn weight(&self) -> usize {
    self.entries.values().map(|entry| entry.weight()).sum()
  }

  fn snapshot_for(&self, vary_key: &str) -> Option<CachedSnapshot> {
    self.entries.get(vary_key).cloned().map(|entry| CachedSnapshot {
      value: entry.value,
      etag: entry.etag,
      last_modified: entry.last_modified,
      http_cache: entry.http_cache,
    })
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct InFlightKey {
  cache: CacheKey,
  request_sig: String,
}

impl InFlightKey {
  fn new(cache: CacheKey, request_sig: String) -> Self {
    Self { cache, request_sig }
  }
}

impl CacheKey {
  fn new(kind: FetchContextKind, url: impl Into<String>) -> Self {
    let url = url.into();
    let credentials_mode = FetchRequest::new(&url, kind.into()).credentials_mode;
    Self {
      kind,
      url,
      origin_key: None,
      credentials_mode,
    }
  }

  fn new_with_origin(
    kind: FetchContextKind,
    url: impl Into<String>,
    origin_key: Option<String>,
    credentials_mode: FetchCredentialsMode,
  ) -> Self {
    Self {
      kind,
      url: url.into(),
      origin_key,
      credentials_mode,
    }
  }

  fn with_url(&self, url: impl Into<String>) -> Self {
    Self {
      kind: self.kind,
      url: url.into(),
      origin_key: self.origin_key.clone(),
      credentials_mode: self.credentials_mode,
    }
  }
}

struct CacheState {
  lru: LruCache<CacheKey, CacheVariants>,
  current_bytes: usize,
  aliases: HashMap<CacheKey, HashMap<String, CacheKey>>,
}

impl CacheState {
  fn new() -> Self {
    Self {
      lru: LruCache::unbounded(),
      current_bytes: 0,
      aliases: HashMap::new(),
    }
  }
}

const MAX_ALIAS_HOPS: usize = 8;

#[derive(Clone)]
enum SharedResult {
  Success(FetchedResource),
  Error(Error),
}

impl SharedResult {
  fn as_result(&self) -> Result<FetchedResource> {
    match self {
      Self::Success(res) => Ok(res.clone()),
      Self::Error(err) => Err(err.clone()),
    }
  }
}

struct InFlight {
  result: Mutex<Option<SharedResult>>,
  cv: Condvar,
}

impl InFlight {
  fn new() -> Self {
    Self {
      result: Mutex::new(None),
      cv: Condvar::new(),
    }
  }

  fn set(&self, result: SharedResult) {
    let mut slot = self
      .result
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(result);
    self.cv.notify_all();
  }

  fn wait(&self, key: &CacheKey) -> Result<FetchedResource> {
    let mut guard = self
      .result
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let deadline = render_control::root_deadline().filter(|d| d.is_enabled());
    let stage = render_stage_hint_for_context(key.kind, &key.url);

    while guard.is_none() {
      if let Some(deadline) = deadline.as_ref() {
        deadline.check(stage).map_err(Error::Render)?;
        let wait_for = if deadline.timeout_limit().is_some() {
          match deadline.remaining_timeout() {
            Some(remaining) if !remaining.is_zero() => remaining.min(Duration::from_millis(10)),
            _ => {
              return Err(Error::Render(RenderError::Timeout {
                stage,
                elapsed: deadline.elapsed(),
              }));
            }
          }
        } else {
          Duration::from_millis(10)
        };
        guard = self
          .cv
          .wait_timeout(guard, wait_for)
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .0;
      } else {
        guard = self
          .cv
          .wait(guard)
          .unwrap_or_else(|poisoned| poisoned.into_inner());
      }
    }
    guard.as_ref().unwrap().as_result()
  }
}

fn render_stage_hint_from_url(url: &str) -> RenderStage {
  if let Some(stage) = crate::render_control::active_stage() {
    return stage;
  }
  let path_hint = Url::parse(url)
    .ok()
    .map(|parsed| parsed.path().to_string())
    .unwrap_or_else(|| {
      url
        .split(|c| c == '?' || c == '#')
        .next()
        .unwrap_or(url)
        .to_string()
    });
  let content_type = guess_content_type_from_path(&path_hint);
  decode_stage_for_content_type(content_type.as_deref())
}

fn render_stage_hint_for_context(kind: FetchContextKind, url: &str) -> RenderStage {
  if let Some(stage) = crate::render_control::active_stage() {
    return stage;
  }
  match kind {
    FetchContextKind::Document | FetchContextKind::Iframe => RenderStage::DomParse,
    FetchContextKind::Stylesheet | FetchContextKind::StylesheetCors | FetchContextKind::Font => {
      RenderStage::Css
    }
    FetchContextKind::Image | FetchContextKind::ImageCors => RenderStage::Paint,
    FetchContextKind::Other => render_stage_hint_from_url(url),
  }
}

/// In-memory caching [`ResourceFetcher`] with LRU eviction and single-flight
/// de-duplication of concurrent requests.
#[derive(Clone)]
pub struct CachingFetcher<F: ResourceFetcher> {
  inner: F,
  state: Arc<Mutex<CacheState>>,
  in_flight: Arc<Mutex<HashMap<InFlightKey, Arc<InFlight>>>>,
  config: CachingFetcherConfig,
  policy: Option<ResourcePolicy>,
}

impl<F: ResourceFetcher> std::fmt::Debug for CachingFetcher<F> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("CachingFetcher")
      .field("config", &self.config)
      .field("policy", &self.policy)
      .finish_non_exhaustive()
  }
}

impl<F: ResourceFetcher> CachingFetcher<F> {
  /// Creates a new caching wrapper with default limits.
  pub fn new(inner: F) -> Self {
    Self::with_config(inner, CachingFetcherConfig::default())
  }

  /// Creates a new caching wrapper with a custom configuration.
  pub fn with_config(inner: F, config: CachingFetcherConfig) -> Self {
    Self {
      inner,
      state: Arc::new(Mutex::new(CacheState::new())),
      in_flight: Arc::new(Mutex::new(HashMap::new())),
      config,
      policy: None,
    }
  }

  /// Updates the maximum total bytes retained in the cache.
  pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
    self.config.max_bytes = max_bytes;
    self
  }

  /// Updates the maximum number of cached entries.
  pub fn with_max_items(mut self, max_items: usize) -> Self {
    self.config.max_items = max_items;
    self
  }

  /// Enables or disables caching of failed fetches.
  pub fn with_cache_errors(mut self, cache_errors: bool) -> Self {
    self.config.cache_errors = cache_errors;
    self
  }

  /// Enables or disables conditional requests using cached ETag/Last-Modified headers.
  pub fn with_http_cache_validation(mut self, enabled: bool) -> Self {
    self.config.honor_http_cache_headers = enabled;
    self
  }

  /// Enables or disables honoring HTTP freshness headers (Cache-Control/Expires).
  ///
  /// When enabled, fresh cached entries are served without revalidation. Stale
  /// or `no-cache`/`must-revalidate` entries will trigger conditional requests
  /// when validators are available.
  pub fn with_http_cache_freshness(mut self, enabled: bool) -> Self {
    self.config.honor_http_cache_freshness = enabled;
    self
  }

  /// Apply a resource policy. When set, disallowed URLs are rejected before consulting the cache
  /// and total byte budgets are enforced for cached responses returned to callers.
  pub fn with_policy(mut self, policy: ResourcePolicy) -> Self {
    self.policy = Some(policy);
    self
  }

  fn allowed_alias_target<'a>(
    &self,
    requested: &str,
    final_url: Option<&'a str>,
  ) -> Option<&'a str> {
    let final_url = final_url?;
    if final_url == requested {
      return None;
    }
    if let Some(policy) = &self.policy {
      if policy.ensure_url_allowed(final_url).is_err() {
        return None;
      }
    }
    Some(final_url)
  }

  fn canonical_url(&self, requested: &str, final_url: Option<&str>) -> String {
    self
      .allowed_alias_target(requested, final_url)
      .unwrap_or(requested)
      .to_string()
  }

  fn plan_cache_use(
    &self,
    url: &str,
    cached: Option<CachedSnapshot>,
    freshness_cap: Option<Duration>,
  ) -> CachePlan {
    let is_http = matches!(
      classify_scheme(url),
      ResourceScheme::Http | ResourceScheme::Https
    );
    let Some(snapshot) = cached else {
      return CachePlan {
        cached: None,
        action: CacheAction::Fetch,
        is_stale: false,
      };
    };

    let has_validators = snapshot.etag.is_some() || snapshot.last_modified.is_some();
    let http_cache = snapshot.http_cache.clone();
    let etag = snapshot.etag.clone();
    let last_modified = snapshot.last_modified.clone();
    let cached = Some(snapshot);

    if !is_http {
      return CachePlan {
        cached,
        action: CacheAction::UseCached,
        is_stale: false,
      };
    }

    if http_cache
      .as_ref()
      .map(|meta| meta.no_store)
      .unwrap_or(false)
    {
      if !self.config.allow_no_store {
        return CachePlan {
          cached: None,
          action: CacheAction::Fetch,
          is_stale: false,
        };
      }

      if self.config.stale_policy == CacheStalePolicy::UseStaleWhenDeadline
        && render_control::root_deadline()
          .as_ref()
          .and_then(|deadline| deadline.timeout_limit())
          .is_some()
      {
        return CachePlan {
          cached,
          action: CacheAction::UseCached,
          is_stale: true,
        };
      }

      if self.config.honor_http_cache_headers && has_validators {
        return CachePlan {
          cached,
          action: CacheAction::Validate {
            etag,
            last_modified,
          },
          is_stale: false,
        };
      }

      return CachePlan {
        cached,
        action: CacheAction::Fetch,
        is_stale: false,
      };
    }

    if self.config.honor_http_cache_freshness {
      if let Some(meta) = http_cache.as_ref() {
        let now = SystemTime::now();
        let is_fresh = meta.is_fresh(now, freshness_cap);
        let requires_revalidation = meta.requires_revalidation();
        if is_fresh && !requires_revalidation {
          return CachePlan {
            cached,
            action: CacheAction::UseCached,
            is_stale: false,
          };
        }
        if self.config.stale_policy == CacheStalePolicy::UseStaleWhenDeadline
          && render_control::root_deadline()
            .as_ref()
            .and_then(|deadline| deadline.timeout_limit())
            .is_some()
        {
          return CachePlan {
            cached,
            action: CacheAction::UseCached,
            is_stale: true,
          };
        }
        if self.config.honor_http_cache_headers && has_validators {
          return CachePlan {
            cached,
            action: CacheAction::Validate {
              etag,
              last_modified,
            },
            is_stale: false,
          };
        }
        return CachePlan {
          cached,
          action: CacheAction::Fetch,
          is_stale: false,
        };
      } else if !self.config.honor_http_cache_headers {
        return CachePlan {
          cached,
          action: CacheAction::UseCached,
          is_stale: false,
        };
      }
    }

    if self.config.honor_http_cache_headers && has_validators {
      // When a cooperative render timeout is active, avoid spending the remaining budget on
      // conditional revalidation requests for cached entries whose freshness is unknown (e.g. the
      // server supplied `ETag`/`Last-Modified` but no `Cache-Control`/`Expires` metadata). In this
      // situation we treat the entry as stale and serve the cached bytes immediately.
      //
      // This keeps `CacheStalePolicy::UseStaleWhenDeadline` consistent with its docs: serve cached
      // bytes even when the entry "requires revalidation".
      if self.config.stale_policy == CacheStalePolicy::UseStaleWhenDeadline
        && render_control::root_deadline()
          .as_ref()
          .and_then(|deadline| deadline.timeout_limit())
          .is_some()
      {
        return CachePlan {
          cached,
          action: CacheAction::UseCached,
          is_stale: true,
        };
      }
      CachePlan {
        cached,
        action: CacheAction::Validate {
          etag,
          last_modified,
        },
        is_stale: false,
      }
    } else {
      CachePlan {
        cached,
        action: CacheAction::UseCached,
        is_stale: false,
      }
    }
  }

  fn insert_canonical_locked(&self, state: &mut CacheState, key: &CacheKey, bucket: CacheVariants) {
    if let Some(existing) = state.lru.peek(key) {
      state.current_bytes = state.current_bytes.saturating_sub(existing.weight());
    }
    state.current_bytes = state.current_bytes.saturating_add(bucket.weight());
    state.aliases.remove(key);
    state.lru.put(key.clone(), bucket);
    self.evict_locked(state);
  }

  fn remove_alias_mapping_locked(state: &mut CacheState, alias: &CacheKey, request_sig: &str) {
    if let Some(map) = state.aliases.get_mut(alias) {
      map.remove(request_sig);
      if map.is_empty() {
        state.aliases.remove(alias);
      }
    }
  }

  fn set_alias_locked(
    &self,
    state: &mut CacheState,
    alias: &CacheKey,
    request_sig: String,
    canonical: &CacheKey,
  ) {
    if alias == canonical {
      Self::remove_alias_mapping_locked(state, alias, &request_sig);
      return;
    }

    state
      .aliases
      .entry(alias.clone())
      .or_default()
      .insert(request_sig, canonical.clone());
  }

  fn remove_aliases_targeting(&self, state: &mut CacheState, key: &CacheKey) {
    state.aliases.retain(|alias, targets| {
      if alias == key {
        return false;
      }
      targets.retain(|_, target| target != key);
      !targets.is_empty()
    });
  }

  fn cache_entry(
    &self,
    requested: &CacheKey,
    request: FetchRequest<'_>,
    vary: Option<String>,
    vary_key: String,
    entry: CacheEntry,
    final_url: Option<&str>,
  ) -> CacheKey {
    let canonical_url = self.canonical_url(&requested.url, final_url);
    let canonical = requested.with_url(canonical_url.clone());

    if self.config.max_bytes > 0 && entry.weight() > self.config.max_bytes {
      return canonical;
    }

    let request_sig = inflight_signature_for_request(&self.inner, request);
    let canonical_request = FetchRequest {
      url: &canonical_url,
      destination: request.destination,
      referrer_url: request.referrer_url,
      client_origin: request.client_origin,
      referrer_policy: request.referrer_policy,
      credentials_mode: request.credentials_mode,
    };
    let canonical_sig = inflight_signature_for_request(&self.inner, canonical_request);

    if let Ok(mut state) = self.state.lock() {
      let mut bucket = state
        .lru
        .pop(&canonical)
        .unwrap_or_else(|| CacheVariants::new(vary.clone()));
      let old_weight = bucket.weight();
      state.current_bytes = state.current_bytes.saturating_sub(old_weight);

      if bucket.vary != vary {
        bucket = CacheVariants::new(vary.clone());
      }
      bucket.entries.insert(vary_key, entry);

      let new_weight = bucket.weight();
      state.current_bytes = state.current_bytes.saturating_add(new_weight);
      Self::remove_alias_mapping_locked(&mut state, &canonical, &canonical_sig);
      state.lru.put(canonical.clone(), bucket);
      self.evict_locked(&mut state);

      if requested.url != canonical_url {
        self.set_alias_locked(&mut state, requested, request_sig, &canonical);
      } else {
        Self::remove_alias_mapping_locked(&mut state, requested, &request_sig);
      }
    }

    canonical
  }

  fn remove_cached(&self, key: &CacheKey, request: FetchRequest<'_>) {
    let request_sig = inflight_signature_for_request(&self.inner, request);
    if let Ok(mut state) = self.state.lock() {
      let canonical = self.resolve_alias_locked(&mut state, key, request);
      if let Some((_k, bucket)) = state.lru.pop_entry(&canonical) {
        state.current_bytes = state.current_bytes.saturating_sub(bucket.weight());
      }
      self.remove_aliases_targeting(&mut state, &canonical);
      Self::remove_alias_mapping_locked(&mut state, key, &request_sig);
    }
  }

  fn build_cache_entry(
    &self,
    key: &CacheKey,
    resource: &FetchedResource,
    stored_at: SystemTime,
  ) -> Option<CacheEntry> {
    if resource.vary.as_deref() == Some("*") {
      return None;
    }
    if !self.config.allow_no_store
      && resource
        .cache_policy
        .as_ref()
        .map(|p| p.no_store)
        .unwrap_or(false)
    {
      return None;
    }

    if let Some(vary) = resource.vary.as_deref() {
      if vary_contains_star(vary) {
        return None;
      }
      let allow_unhandled = self.config.allow_unhandled_vary || allow_unhandled_vary_env();
      if !allow_unhandled && !vary_is_cacheable(vary, key.kind, key.origin_key.as_deref()) {
        return None;
      }
    }

    let http_cache = resource
      .cache_policy
      .as_ref()
      .and_then(|policy| CachedHttpMetadata::from_policy(policy, stored_at));

    Some(CacheEntry {
      etag: resource.etag.clone(),
      last_modified: resource.last_modified.clone(),
      http_cache,
      value: CacheValue::Resource(resource.clone()),
    })
  }

  fn evict_locked(&self, state: &mut CacheState) {
    while (self.config.max_items > 0 && state.lru.len() > self.config.max_items)
      || (self.config.max_bytes > 0 && state.current_bytes > self.config.max_bytes)
    {
      if let Some((key, entry)) = state.lru.pop_lru() {
        state.current_bytes = state.current_bytes.saturating_sub(entry.weight());
        self.remove_aliases_targeting(state, &key);
      } else {
        break;
      }
    }
  }

  fn resolve_alias_locked(
    &self,
    state: &mut CacheState,
    key: &CacheKey,
    base_request: FetchRequest<'_>,
  ) -> CacheKey {
    let origin = key.clone();
    let mut current = origin.clone();
    let mut hops = 0usize;
    let mut visited: HashSet<CacheKey> = HashSet::new();
    let mut removed = false;

    while state.aliases.contains_key(&current) {
      let request = FetchRequest {
        url: &current.url,
        destination: base_request.destination,
        referrer_url: base_request.referrer_url,
        client_origin: base_request.client_origin,
        referrer_policy: base_request.referrer_policy,
        credentials_mode: base_request.credentials_mode,
      };
      let request_sig = inflight_signature_for_request(&self.inner, request);
      let next = state
        .aliases
        .get(&current)
        .and_then(|targets| targets.get(&request_sig))
        .cloned();
      let Some(next) = next else {
        break;
      };
      if hops >= MAX_ALIAS_HOPS || !visited.insert(current.clone()) || next == current {
        let origin_sig = inflight_signature_for_request(
          &self.inner,
          FetchRequest {
            url: &origin.url,
            destination: base_request.destination,
            referrer_url: base_request.referrer_url,
            client_origin: base_request.client_origin,
            referrer_policy: base_request.referrer_policy,
            credentials_mode: base_request.credentials_mode,
          },
        );
        Self::remove_alias_mapping_locked(state, &origin, &origin_sig);
        removed = true;
        break;
      }
      current = next;
      hops += 1;
    }

    if !removed && current != origin {
      let origin_sig = inflight_signature_for_request(
        &self.inner,
        FetchRequest {
          url: &origin.url,
          destination: base_request.destination,
          referrer_url: base_request.referrer_url,
          client_origin: base_request.client_origin,
          referrer_policy: base_request.referrer_policy,
          credentials_mode: base_request.credentials_mode,
        },
      );
      state
        .aliases
        .entry(origin)
        .or_default()
        .insert(origin_sig, current.clone());
    }

    current
  }

  fn cached_entry(&self, key: &CacheKey, request: Option<FetchRequest<'_>>) -> Option<CachedSnapshot> {
    self.state.lock().ok().and_then(|mut state| {
      let base_request = request.unwrap_or_else(|| FetchRequest::new(&key.url, key.kind.into()));
      let canonical = self.resolve_alias_locked(&mut state, key, base_request);
      let bucket = match state.lru.get(&canonical) {
        Some(bucket) => bucket,
        None => {
          if &canonical != key {
            let request_sig = inflight_signature_for_request(&self.inner, base_request);
            Self::remove_alias_mapping_locked(&mut state, key, &request_sig);
          }
          return None;
        }
      };

      let request = FetchRequest {
        url: &canonical.url,
        destination: base_request.destination,
        referrer_url: base_request.referrer_url,
        client_origin: base_request.client_origin,
        referrer_policy: base_request.referrer_policy,
        credentials_mode: base_request.credentials_mode,
      };

      let vary_key = compute_vary_key_for_request(&self.inner, request, bucket.vary.as_deref())?;
      bucket.snapshot_for(&vary_key)
    })
  }

  #[cfg(feature = "disk_cache")]
  pub(crate) fn cached_snapshot(
    &self,
    kind: FetchContextKind,
    url: &str,
  ) -> Option<CachedSnapshot> {
    self.cached_entry(&CacheKey::new(kind, url.to_string()), None)
  }

  #[cfg(feature = "disk_cache")]
  pub(crate) fn prime_cache_with_snapshot(
    &self,
    kind: FetchContextKind,
    url: &str,
    snapshot: CachedSnapshot,
  ) -> String {
    let final_url = match &snapshot.value {
      CacheValue::Resource(res) => res.final_url.clone(),
      CacheValue::Error(_) => None,
    };
    let vary = match &snapshot.value {
      CacheValue::Resource(res) => res.vary.clone(),
      CacheValue::Error(_) => None,
    };
    let request = FetchRequest::new(url, kind.into());
    let Some(vary_key) =
      compute_vary_key_for_request(&self.inner, request, vary.as_deref())
    else {
      return url.to_string();
    };
    self
      .cache_entry(
        &CacheKey::new(kind, url.to_string()),
        request,
        vary,
        vary_key,
        CacheEntry {
          etag: snapshot.etag.clone(),
          last_modified: snapshot.last_modified.clone(),
          http_cache: snapshot.http_cache.clone(),
          value: snapshot.value,
        },
        final_url.as_deref(),
      )
      .url
  }

  #[cfg(feature = "disk_cache")]
  pub(crate) fn prime_cache_with_resource(
    &self,
    kind: FetchContextKind,
    url: &str,
    resource: FetchedResource,
  ) {
    let stored_at = SystemTime::now();
    let vary = resource.vary.clone();
    let request = FetchRequest::new(url, kind.into());
    let Some(vary_key) =
      compute_vary_key_for_request(&self.inner, request, vary.as_deref())
    else {
      return;
    };
    let key = CacheKey::new(kind, url.to_string());
    if let Some(entry) = self.build_cache_entry(&key, &resource, stored_at) {
      self.cache_entry(
        &key,
        request,
        vary,
        vary_key,
        entry,
        resource.final_url.as_deref(),
      );
    }
  }

  fn join_inflight(&self, key: &InFlightKey) -> (Arc<InFlight>, bool) {
    let mut map = match self.in_flight.lock() {
      Ok(map) => map,
      Err(poisoned) => {
        let mut map = poisoned.into_inner();
        map.clear();
        map
      }
    };
    if let Some(existing) = map.get(key) {
      return (Arc::clone(existing), false);
    }

    let flight = Arc::new(InFlight::new());
    map.insert(key.clone(), Arc::clone(&flight));
    (flight, true)
  }

  fn finish_inflight(&self, key: &InFlightKey, flight: &Arc<InFlight>, result: SharedResult) {
    flight.set(result);
    let mut map = self
      .in_flight
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.remove(key);
  }
}
struct InFlightOwnerGuard<'a, F: ResourceFetcher> {
  fetcher: &'a CachingFetcher<F>,
  key: InFlightKey,
  flight: Arc<InFlight>,
  finished: bool,
}

impl<'a, F: ResourceFetcher> InFlightOwnerGuard<'a, F> {
  fn new(fetcher: &'a CachingFetcher<F>, key: InFlightKey, flight: Arc<InFlight>) -> Self {
    Self {
      fetcher,
      key,
      flight,
      finished: false,
    }
  }

  fn finish(&mut self, result: SharedResult) {
    if self.finished {
      return;
    }
    self.finished = true;
    self
      .fetcher
      .finish_inflight(&self.key, &self.flight, result);
  }
}

impl<'a, F: ResourceFetcher> Drop for InFlightOwnerGuard<'a, F> {
  fn drop(&mut self) {
    if self.finished {
      return;
    }

    self.finished = true;
    let err = Error::Resource(ResourceError::new(
      self.key.cache.url.to_string(),
      "in-flight fetch owner dropped without resolving",
    ));
    self
      .fetcher
      .finish_inflight(&self.key, &self.flight, SharedResult::Error(err));
  }
}

impl<F: ResourceFetcher> ResourceFetcher for CachingFetcher<F> {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    self.fetch_with_context(FetchContextKind::Other, url)
  }

  fn fetch_with_context(&self, kind: FetchContextKind, url: &str) -> Result<FetchedResource> {
    if let Some(policy) = &self.policy {
      policy.ensure_url_allowed(url)?;
    }

    let key = CacheKey::new(kind, url.to_string());
    let request = FetchRequest::new(url, kind.into());
    let cached = self.cached_entry(&key, Some(request));
    let plan = self.plan_cache_use(url, cached.clone(), None);
    if let CacheAction::UseCached = plan.action {
      if let Some(snapshot) = plan.cached.as_ref() {
        if plan.is_stale {
          record_cache_stale_hit();
        } else {
          record_cache_fresh_hit();
        }
        let result = snapshot.value.as_result();
        if let Ok(ref res) = result {
          record_resource_cache_bytes(res.bytes.len());
          reserve_policy_bytes(&self.policy, res)?;
        }
        return result;
      }
    }

    let request_sig = inflight_signature_for_request(&self.inner, request);
    let inflight_key = InFlightKey::new(key.clone(), request_sig);
    let (flight, is_owner) = self.join_inflight(&inflight_key);
    if !is_owner {
      let inflight_timer = start_fetch_inflight_wait_diagnostics();
      let result = flight.wait(&key);
      finish_fetch_inflight_wait_diagnostics(inflight_timer);
      if let Ok(ref res) = result {
        reserve_policy_bytes(&self.policy, res)?;
      }
      return result;
    }

    let mut inflight_guard = InFlightOwnerGuard::new(self, inflight_key, flight);

    let validators = match &plan.action {
      CacheAction::Validate {
        etag,
        last_modified,
      } => Some((etag.as_deref(), last_modified.as_deref())),
      _ => None,
    };

    let fetch_result = match validators {
      Some((etag, last_modified)) => {
        self
          .inner
          .fetch_with_validation_and_context(kind, url, etag, last_modified)
      }
      None => self.inner.fetch_with_context(kind, url),
    };

    let (mut result, charge_budget) = match fetch_result {
      Ok(res) => {
        if res.is_not_modified() {
          if let Some(snapshot) = plan.cached.as_ref() {
            let value = snapshot.value.as_result();
            if let Ok(ref ok) = value {
              let mut stored_resource = ok.clone();
              if res.vary.is_some() {
                stored_resource.vary = res.vary.clone();
              }

              let stored_at = SystemTime::now();
              let should_store = self.config.allow_no_store
                || !res
                  .cache_policy
                  .as_ref()
                  .map(|p| p.no_store)
                  .unwrap_or(false);
              let http_cache = snapshot
                .http_cache
                .as_ref()
                .and_then(|meta| meta.with_updated_policy(res.cache_policy.as_ref(), stored_at))
                .or_else(|| {
                  res
                    .cache_policy
                    .as_ref()
                    .and_then(|policy| CachedHttpMetadata::from_policy(policy, stored_at))
                });

              let allow_unhandled = self.config.allow_unhandled_vary || allow_unhandled_vary_env();
              let memory_cacheable = match stored_resource.vary.as_deref() {
                Some(vary) => {
                  !vary_contains_star(vary)
                    && (allow_unhandled
                      || vary_is_cacheable(vary, key.kind, key.origin_key.as_deref()))
                }
                None => true,
              };

              if should_store && memory_cacheable {
                let canonical_url = self.canonical_url(url, ok.final_url.as_deref());
                let canonical_request = FetchRequest {
                  url: &canonical_url,
                  destination: request.destination,
                  referrer_url: request.referrer_url,
                  client_origin: request.client_origin,
                  referrer_policy: request.referrer_policy,
                  credentials_mode: request.credentials_mode,
                };

                if let Some(vary_key) = compute_vary_key_for_request(
                  &self.inner,
                  canonical_request,
                  stored_resource.vary.as_deref(),
                ) {
                  let _ = self.cache_entry(
                    &key,
                    request,
                    stored_resource.vary.clone(),
                    vary_key,
                    CacheEntry {
                      value: CacheValue::Resource(stored_resource),
                      etag: res.etag.clone().or_else(|| snapshot.etag.clone()),
                      last_modified: res
                        .last_modified
                        .clone()
                        .or_else(|| snapshot.last_modified.clone()),
                      http_cache,
                    },
                    ok.final_url.as_deref(),
                  );
                } else {
                  self.remove_cached(&key, request);
                }
              } else {
                self.remove_cached(&key, request);
              }
            }
            record_cache_revalidated_hit();
            if let Ok(ref ok) = value {
              record_resource_cache_bytes(ok.bytes.len());
            }
            let is_ok = value.is_ok();
            (value, is_ok)
          } else {
            (
              Err(Error::Resource(
                ResourceError::new(url.to_string(), "received 304 without cached entry")
                  .with_final_url(url.to_string()),
              )),
              false,
            )
          }
        } else if res.status.is_some_and(|code| code >= 400)
          && plan
            .cached
            .as_ref()
            .is_some_and(|snapshot| snapshot.is_successful_http_response())
        {
          if let Some(snapshot) = plan.cached.as_ref() {
            record_cache_stale_hit();
            let fallback = snapshot.value.as_result();
            if let Ok(ref ok) = fallback {
              record_resource_cache_bytes(ok.bytes.len());
            }
            let is_ok = fallback.is_ok();
            (fallback, is_ok)
          } else {
            record_cache_miss();
            (Ok(res), false)
          }
        } else if res.status.map(is_transient_http_status).unwrap_or(false) {
          if let Some(snapshot) = plan.cached.as_ref() {
            record_cache_stale_hit();
            let fallback = snapshot.value.as_result();
            if let Ok(ref ok) = fallback {
              record_resource_cache_bytes(ok.bytes.len());
            }
            let is_ok = fallback.is_ok();
            (fallback, is_ok)
          } else {
            // When callers opt into caching `no-store` responses for pageset determinism, also
            // persist transient HTTP error responses (429/5xx) as always-stale entries. This avoids
            // repeatedly hammering blocked endpoints during warm-cache runs while still allowing
            // non-deadline fetches to attempt a refresh.
            if self.config.allow_no_store && res.status.is_some_and(|code| code >= 400) {
              let allow_unhandled = self.config.allow_unhandled_vary || allow_unhandled_vary_env();
              let memory_cacheable = match res.vary.as_deref() {
                Some(vary) => {
                  !vary_contains_star(vary)
                    && (allow_unhandled
                      || vary_is_cacheable(vary, key.kind, key.origin_key.as_deref()))
                }
                None => true,
              };
              if memory_cacheable {
                let stored_at = SystemTime::now();
                let canonical_url = self.canonical_url(url, res.final_url.as_deref());
                let canonical_request = FetchRequest {
                  url: &canonical_url,
                  destination: request.destination,
                  referrer_url: request.referrer_url,
                  client_origin: request.client_origin,
                  referrer_policy: request.referrer_policy,
                  credentials_mode: request.credentials_mode,
                };
                let vary = res.vary.clone();
                if let Some(vary_key) =
                  compute_vary_key_for_request(&self.inner, canonical_request, vary.as_deref())
                {
                  let _ = self.cache_entry(
                    &key,
                    request,
                    vary,
                    vary_key,
                    CacheEntry {
                      value: CacheValue::Resource(res.clone()),
                      etag: res.etag.clone(),
                      last_modified: res.last_modified.clone(),
                      http_cache: Some(CachedHttpMetadata {
                        stored_at,
                        max_age: None,
                        expires: None,
                        no_cache: false,
                        no_store: true,
                        must_revalidate: false,
                      }),
                    },
                    res.final_url.as_deref(),
                  );
                }
              }
            }
            record_cache_miss();
            (Ok(res), false)
          }
        } else {
          let stored_at = SystemTime::now();
          if let Some(entry) = self.build_cache_entry(&key, &res, stored_at) {
            let canonical_url = self.canonical_url(url, res.final_url.as_deref());
            let canonical_request = FetchRequest {
              url: &canonical_url,
              destination: request.destination,
              referrer_url: request.referrer_url,
              client_origin: request.client_origin,
              referrer_policy: request.referrer_policy,
              credentials_mode: request.credentials_mode,
            };
            let vary = res.vary.clone();
            if let Some(vary_key) =
              compute_vary_key_for_request(&self.inner, canonical_request, vary.as_deref())
            {
              let _ = self.cache_entry(&key, request, vary, vary_key, entry, res.final_url.as_deref());
            }
          } else if !self.config.allow_no_store
            && res
              .cache_policy
              .as_ref()
              .map(|p| p.no_store)
              .unwrap_or(false)
          {
            self.remove_cached(&key, request);
          }
          record_cache_miss();
          (Ok(res), false)
        }
      }
      Err(err) => {
        if let Some(snapshot) = plan.cached.as_ref() {
          record_cache_stale_hit();
          let fallback = snapshot.value.as_result();
          if let Ok(ref ok) = fallback {
            record_resource_cache_bytes(ok.bytes.len());
          }
          let is_ok = fallback.is_ok();
          (fallback, is_ok)
        } else {
          let has_bucket = self
            .state
            .lock()
            .ok()
            .map(|mut state| {
              let canonical = self.resolve_alias_locked(&mut state, &key, request);
              state.lru.peek(&canonical).is_some()
            })
            .unwrap_or(false);

          if self.config.cache_errors && !has_bucket {
            if let Some(vary_key) = compute_vary_key_for_request(
              &self.inner,
              request,
              Some(INFLIGHT_VARY_SIGNATURE_HEADERS),
            ) {
              let _ = self.cache_entry(
                &key,
                request,
                Some(INFLIGHT_VARY_SIGNATURE_HEADERS.to_string()),
                vary_key,
                CacheEntry {
                  value: CacheValue::Error(err.clone()),
                  etag: None,
                  last_modified: None,
                  http_cache: None,
                },
                None,
              );
            }
          }
          (Err(err), false)
        }
      }
    };

    if charge_budget {
      if let Ok(ref res) = result {
        if let Err(err) = reserve_policy_bytes(&self.policy, res) {
          result = Err(err);
        }
      }
    }

    let notify = match &result {
      Ok(res) => SharedResult::Success(res.clone()),
      Err(err) => SharedResult::Error(err.clone()),
    };
    inflight_guard.finish(notify);

    result
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    let kind: FetchContextKind = req.destination.into();
    let url = req.url;
    if let Some(policy) = &self.policy {
      policy.ensure_url_allowed(url)?;
    }

    let key = CacheKey::new_with_origin(
      kind,
      url.to_string(),
      cors_cache_partition_key(&req),
      req.credentials_mode,
    );
    let cached = self.cached_entry(&key, Some(req));
    let plan = self.plan_cache_use(url, cached.clone(), None);
    if let CacheAction::UseCached = plan.action {
      if let Some(snapshot) = plan.cached.as_ref() {
        if plan.is_stale {
          record_cache_stale_hit();
        } else {
          record_cache_fresh_hit();
        }
        let result = snapshot.value.as_result();
        if let Ok(ref res) = result {
          record_resource_cache_bytes(res.bytes.len());
          reserve_policy_bytes(&self.policy, res)?;
        }
        return result;
      }
    }

    let request_sig = inflight_signature_for_request(&self.inner, req);
    let inflight_key = InFlightKey::new(key.clone(), request_sig);
    let (flight, is_owner) = self.join_inflight(&inflight_key);
    if !is_owner {
      let inflight_timer = start_fetch_inflight_wait_diagnostics();
      let result = flight.wait(&key);
      finish_fetch_inflight_wait_diagnostics(inflight_timer);
      if let Ok(ref res) = result {
        reserve_policy_bytes(&self.policy, res)?;
      }
      return result;
    }

    let mut inflight_guard = InFlightOwnerGuard::new(self, inflight_key, flight);

    let validators = match &plan.action {
      CacheAction::Validate {
        etag,
        last_modified,
      } => Some((etag.as_deref(), last_modified.as_deref())),
      _ => None,
    };

    let fetch_result = match validators {
      Some((etag, last_modified)) => {
        self
          .inner
          .fetch_with_request_and_validation(req, etag, last_modified)
      }
      None => self.inner.fetch_with_request(req),
    };

    let (mut result, charge_budget) = match fetch_result {
      Ok(res) => {
        if res.is_not_modified() {
          if let Some(snapshot) = plan.cached.as_ref() {
            let value = snapshot.value.as_result();
            if let Ok(ref ok) = value {
              let mut stored_resource = ok.clone();
              if res.vary.is_some() {
                stored_resource.vary = res.vary.clone();
              }
              let stored_at = SystemTime::now();
              let canonical_url = self.canonical_url(url, ok.final_url.as_deref());
              let canonical_request = FetchRequest {
                url: &canonical_url,
                destination: req.destination,
                referrer_url: req.referrer_url,
                client_origin: req.client_origin,
                referrer_policy: req.referrer_policy,
                credentials_mode: req.credentials_mode,
              };
              let should_store = self.config.allow_no_store
                || !res
                  .cache_policy
                  .as_ref()
                  .map(|p| p.no_store)
                  .unwrap_or(false);
              let updated_meta = snapshot
                .http_cache
                .as_ref()
                .and_then(|meta| meta.with_updated_policy(res.cache_policy.as_ref(), stored_at))
                .or_else(|| {
                  res
                    .cache_policy
                    .as_ref()
                    .and_then(|policy| CachedHttpMetadata::from_policy(policy, stored_at))
                });
 
              let allow_unhandled = self.config.allow_unhandled_vary || allow_unhandled_vary_env();
              let memory_cacheable = match stored_resource.vary.as_deref() {
                Some(vary) => {
                  !vary_contains_star(vary)
                    && (allow_unhandled
                      || vary_is_cacheable(vary, key.kind, key.origin_key.as_deref()))
                }
                None => true,
              };
 
              if should_store && memory_cacheable {
                if let Some(vary_key) = compute_vary_key_for_request(
                  &self.inner,
                  canonical_request,
                  stored_resource.vary.as_deref(),
                ) {
                  let _ = self.cache_entry(
                    &key,
                    req,
                    stored_resource.vary.clone(),
                    vary_key,
                    CacheEntry {
                      value: CacheValue::Resource(stored_resource),
                      etag: res.etag.clone().or_else(|| snapshot.etag.clone()),
                      last_modified: res
                        .last_modified
                        .clone()
                        .or_else(|| snapshot.last_modified.clone()),
                      http_cache: updated_meta,
                    },
                    ok.final_url.as_deref(),
                  );
                } else {
                  self.remove_cached(&key, req);
                }
              } else {
                self.remove_cached(&key, req);
              }
            }
            record_cache_revalidated_hit();
            if let Ok(ref ok) = value {
              record_resource_cache_bytes(ok.bytes.len());
            }
            let is_ok = value.is_ok();
            (value, is_ok)
          } else {
            (
              Err(Error::Resource(
                ResourceError::new(url.to_string(), "received 304 without cached entry")
                  .with_final_url(url.to_string()),
              )),
              false,
            )
          }
        } else {
          if res.status.is_some_and(|code| code >= 400)
            && plan
              .cached
              .as_ref()
              .is_some_and(|snapshot| snapshot.is_successful_http_response())
          {
            if let Some(snapshot) = plan.cached.as_ref() {
              record_cache_stale_hit();
              let fallback = snapshot.value.as_result();
              if let Ok(ref ok) = fallback {
                record_resource_cache_bytes(ok.bytes.len());
              }
              let is_ok = fallback.is_ok();
              (fallback, is_ok)
            } else {
              record_cache_miss();
              (Ok(res), false)
            }
          } else if res.status.map(is_transient_http_status).unwrap_or(false) {
            if let Some(snapshot) = plan.cached.as_ref() {
              record_cache_stale_hit();
              let fallback = snapshot.value.as_result();
              if let Ok(ref ok) = fallback {
                record_resource_cache_bytes(ok.bytes.len());
              }
              let is_ok = fallback.is_ok();
              (fallback, is_ok)
            } else {
              record_cache_miss();
              (Ok(res), false)
            }
          } else {
            let stored_at = SystemTime::now();
            if let Some(entry) = self.build_cache_entry(&key, &res, stored_at) {
              let canonical_url = self.canonical_url(url, res.final_url.as_deref());
              let canonical_request = FetchRequest {
                url: &canonical_url,
                destination: req.destination,
                referrer_url: req.referrer_url,
                client_origin: req.client_origin,
                referrer_policy: req.referrer_policy,
                credentials_mode: req.credentials_mode,
              };
              let vary = res.vary.clone();
              if let Some(vary_key) =
                compute_vary_key_for_request(&self.inner, canonical_request, vary.as_deref())
              {
                let _ = self.cache_entry(&key, req, vary, vary_key, entry, res.final_url.as_deref());
              }
            } else if !self.config.allow_no_store
              && res
                .cache_policy
                .as_ref()
                .map(|p| p.no_store)
                .unwrap_or(false)
            {
              self.remove_cached(&key, req);
            }
            record_cache_miss();
            (Ok(res), false)
          }
        }
      }
      Err(err) => {
        if let Some(snapshot) = plan.cached.as_ref() {
          record_cache_stale_hit();
          let fallback = snapshot.value.as_result();
          if let Ok(ref ok) = fallback {
            record_resource_cache_bytes(ok.bytes.len());
          }
          let is_ok = fallback.is_ok();
          (fallback, is_ok)
        } else {
          let has_bucket = self
            .state
            .lock()
            .ok()
            .map(|mut state| {
              let canonical = self.resolve_alias_locked(&mut state, &key, req);
              state.lru.peek(&canonical).is_some()
            })
            .unwrap_or(false);

          if self.config.cache_errors && !has_bucket {
            if let Some(vary_key) = compute_vary_key_for_request(
              &self.inner,
              req,
              Some(INFLIGHT_VARY_SIGNATURE_HEADERS),
            ) {
              let _ = self.cache_entry(
                &key,
                req,
                Some(INFLIGHT_VARY_SIGNATURE_HEADERS.to_string()),
                vary_key,
                CacheEntry {
                  value: CacheValue::Error(err.clone()),
                  etag: None,
                  last_modified: None,
                  http_cache: None,
                },
                None,
              );
            }
          }
          (Err(err), false)
        }
      }
    };

    if charge_budget {
      if let Ok(ref res) = result {
        if let Err(err) = reserve_policy_bytes(&self.policy, res) {
          result = Err(err);
        }
      }
    }

    let notify = match &result {
      Ok(res) => SharedResult::Success(res.clone()),
      Err(err) => SharedResult::Error(err.clone()),
    };
    inflight_guard.finish(notify);

    result
  }

  fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
    self.inner.request_header_value(req, header_name)
  }

  fn fetch_partial(&self, url: &str, max_bytes: usize) -> Result<FetchedResource> {
    self.fetch_partial_with_context(FetchContextKind::Other, url, max_bytes)
  }

  fn fetch_partial_with_context(
    &self,
    kind: FetchContextKind,
    url: &str,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    if let Some(policy) = &self.policy {
      policy.ensure_url_allowed(url)?;
    }

    let key = CacheKey::new(kind, url.to_string());
    if let Some(snapshot) = self.cached_entry(&key, None) {
      if let CacheValue::Resource(mut res) = snapshot.value {
        if res.bytes.len() > max_bytes {
          res.bytes.truncate(max_bytes);
        }
        record_cache_fresh_hit();
        record_resource_cache_bytes(res.bytes.len());
        reserve_policy_bytes(&self.policy, &res)?;
        return Ok(res);
      }
    }

    self.inner.fetch_partial_with_context(kind, url, max_bytes)
  }

  fn fetch_partial_with_request(
    &self,
    req: FetchRequest<'_>,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    let kind: FetchContextKind = req.destination.into();
    let url = req.url;
    if let Some(policy) = &self.policy {
      policy.ensure_url_allowed(url)?;
    }

    let key = CacheKey::new_with_origin(
      kind,
      url.to_string(),
      cors_cache_partition_key(&req),
      req.credentials_mode,
    );
    if let Some(snapshot) = self.cached_entry(&key, Some(req)) {
      if let CacheValue::Resource(mut res) = snapshot.value {
        if res.bytes.len() > max_bytes {
          res.bytes.truncate(max_bytes);
        }
        record_cache_fresh_hit();
        record_resource_cache_bytes(res.bytes.len());
        reserve_policy_bytes(&self.policy, &res)?;
        return Ok(res);
      }
    }

    self.inner.fetch_partial_with_request(req, max_bytes)
  }
}

/// Returns a sanitized header value from a response header map.
///
/// When the header appears multiple times, values are joined with `", "` (after trimming).
fn header_values_joined(headers: &HeaderMap, name: &str) -> Option<String> {
  let values: Vec<&str> = headers
    .get_all(name)
    .iter()
    .filter_map(|value| value.to_str().ok())
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .collect();
  match values.as_slice() {
    [] => None,
    [value] => Some((*value).to_string()),
    _ => Some(values.join(", ")),
  }
}

fn header_value_to_string_lossy(value: &http::HeaderValue) -> String {
  value
    .to_str()
    .map(str::to_string)
    .unwrap_or_else(|_| String::from_utf8_lossy(value.as_bytes()).into_owned())
}

fn collect_response_headers(headers: &HeaderMap) -> Vec<(String, String)> {
  headers
    .iter()
    .map(|(name, value)| {
      (
        name.as_str().to_string(),
        header_value_to_string_lossy(value),
      )
    })
    .collect()
}

fn header_has_nosniff(headers: &HeaderMap) -> bool {
  header_values_joined(headers, "x-content-type-options")
    .map(|value| value.to_ascii_lowercase().contains("nosniff"))
    .unwrap_or(false)
}

/// Returns a normalized `Vary` header value from response headers.
///
/// - Header names are lowercased and sorted lexicographically.
/// - Multiple `Vary` headers and comma-separated values are coalesced.
/// - Returns `Some("*")` when `Vary: *` is present.
fn parse_vary_headers(headers: &HeaderMap) -> Option<String> {
  let mut out: Vec<String> = Vec::new();
  for value in headers.get_all("vary").iter() {
    let Ok(value) = value.to_str() else {
      continue;
    };
    for token in value.split(',') {
      let token = token.trim();
      if token.is_empty() {
        continue;
      }
      if token == "*" {
        return Some("*".to_string());
      }
      out.push(token.to_ascii_lowercase());
    }
  }
  out.sort();
  out.dedup();
  if out.is_empty() {
    None
  } else {
    Some(out.join(", "))
  }
}

fn parse_content_encodings(headers: &HeaderMap) -> Vec<String> {
  headers
    .get_all("content-encoding")
    .iter()
    .filter_map(|h| h.to_str().ok())
    .flat_map(|value| value.split(','))
    .map(|value| value.trim().to_ascii_lowercase())
    .filter(|value| !value.is_empty())
    .collect()
}

fn parse_cors_response_headers(headers: &HeaderMap) -> (Option<String>, bool) {
  let allow_origin = header_values_joined(headers, "access-control-allow-origin");
  let credentials_values: Vec<&str> = headers
    .get_all("access-control-allow-credentials")
    .iter()
    .filter_map(|value| value.to_str().ok())
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .collect();
  // Chromium expects `Access-Control-Allow-Credentials: true` exactly; treat multiple values as
  // invalid.
  let allow_credentials = matches!(
    credentials_values.as_slice(),
    [value] if value.eq_ignore_ascii_case("true")
  );
  (allow_origin, allow_credentials)
}

fn read_response_prefix<R: Read>(
  reader: &mut R,
  max_bytes: usize,
) -> std::result::Result<Vec<u8>, io::Error> {
  if max_bytes == 0 {
    return Ok(Vec::new());
  }

  let mut bytes = FallibleVecWriter::new(max_bytes, "response prefix");
  let mut written = 0usize;
  let mut buf = [0u8; 8 * 1024];
  while written < max_bytes {
    let remaining = max_bytes - written;
    let to_read = remaining.min(buf.len());
    let n = match reader.read(&mut buf[..to_read]) {
      Ok(0) => break,
      Ok(n) => n,
      Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
      Err(err) => return Err(err),
    };
    bytes.write_all(&buf[..n])?;
    written = written.saturating_add(n);
  }
  Ok(bytes.into_inner())
}

fn decode_stage_for_content_type(content_type: Option<&str>) -> RenderStage {
  if let Some(stage) = crate::render_control::active_stage() {
    return stage;
  }
  let mime = content_type
    .and_then(|ct| ct.split(';').next())
    .map(|ct| ct.trim().to_ascii_lowercase())
    .unwrap_or_else(String::new);

  if mime.contains("text/css") || mime.contains("font/") {
    return RenderStage::Css;
  }
  if mime.contains("html") {
    return RenderStage::DomParse;
  }
  RenderStage::Paint
}

#[derive(Debug)]
enum ContentDecodeError {
  UnsupportedEncoding(String),
  DecompressionFailed {
    encoding: String,
    source: io::Error,
  },
  SizeLimitExceeded {
    decoded: usize,
    limit: usize,
  },
  DeadlineExceeded {
    encoding: String,
    stage: RenderStage,
    elapsed: Duration,
  },
}

impl std::fmt::Display for ContentDecodeError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::UnsupportedEncoding(encoding) => write!(f, "unsupported content encoding: {encoding}"),
      Self::DecompressionFailed { encoding, source } => {
        write!(f, "{encoding} decompression failed: {source}")
      }
      Self::SizeLimitExceeded { decoded, limit } => write!(
        f,
        "decoded body exceeds limit ({} > {} bytes)",
        decoded, limit
      ),
      Self::DeadlineExceeded {
        encoding,
        stage,
        elapsed,
      } => write!(
        f,
        "rendering timed out during {stage} while decompressing {encoding} after {elapsed:?}"
      ),
    }
  }
}

impl ContentDecodeError {
  fn into_resource_error(self, url: String, status: u16, final_url: String) -> ResourceError {
    let mut err = ResourceError::new(url, self.to_string())
      .with_status(status)
      .with_final_url(final_url);
    if let Self::DecompressionFailed { source, .. } = self {
      err = err.with_source(source);
    }
    err
  }
}

fn decode_content_encodings(
  body: Vec<u8>,
  encodings: &[String],
  limit: usize,
  stage: RenderStage,
) -> std::result::Result<Vec<u8>, ContentDecodeError> {
  if encodings.is_empty() {
    ensure_within_limit(body.len(), limit)?;
    return Ok(body);
  }

  let mut decoded = body;
  for encoding in encodings.iter().rev() {
    // "identity" is a no-op; avoid cloning the entire body.
    if encoding.is_empty() || encoding.eq_ignore_ascii_case("identity") {
      ensure_within_limit(decoded.len(), limit)?;
      continue;
    }
    decoded = decode_single_encoding(encoding, &decoded, limit, stage)?;
  }

  Ok(decoded)
}

fn decode_single_encoding(
  encoding: &str,
  input: &[u8],
  limit: usize,
  stage: RenderStage,
) -> std::result::Result<Vec<u8>, ContentDecodeError> {
  match encoding {
    "" | "identity" => {
      ensure_within_limit(input.len(), limit)?;
      Ok(input.to_vec())
    }
    "gzip" => decode_with_reader("gzip", GzDecoder::new(Cursor::new(input)), limit, stage),
    "deflate" => decode_deflate(input, limit, stage),
    "br" => decode_with_reader(
      "br",
      Decompressor::new(Cursor::new(input), 4096),
      limit,
      stage,
    ),
    other => Err(ContentDecodeError::UnsupportedEncoding(other.to_string())),
  }
}

fn decode_deflate(
  input: &[u8],
  limit: usize,
  stage: RenderStage,
) -> std::result::Result<Vec<u8>, ContentDecodeError> {
  match decode_with_reader(
    "deflate",
    ZlibDecoder::new(Cursor::new(input)),
    limit,
    stage,
  ) {
    Ok(decoded) => Ok(decoded),
    Err(ContentDecodeError::DecompressionFailed { .. }) => decode_with_reader(
      "deflate",
      DeflateDecoder::new(Cursor::new(input)),
      limit,
      stage,
    ),
    Err(err) => Err(err),
  }
}

fn decode_with_reader<R: Read>(
  encoding: &str,
  mut reader: R,
  limit: usize,
  stage: RenderStage,
) -> std::result::Result<Vec<u8>, ContentDecodeError> {
  let mut decoded = FallibleVecWriter::new(limit, "content decode");
  let mut decoded_len = 0usize;
  let mut buf = [0u8; 8192];
  let mut deadline_counter = 0usize;

  loop {
    if let Err(err) =
      check_active_periodic(&mut deadline_counter, CONTENT_DECODE_DEADLINE_STRIDE, stage)
    {
      if let RenderError::Timeout { stage, elapsed } = err {
        return Err(ContentDecodeError::DeadlineExceeded {
          encoding: encoding.to_string(),
          stage,
          elapsed,
        });
      }
      return Err(ContentDecodeError::DecompressionFailed {
        encoding: encoding.to_string(),
        source: io::Error::new(io::ErrorKind::Other, err.to_string()),
      });
    }
    let read = reader
      .read(&mut buf)
      .map_err(|source| ContentDecodeError::DecompressionFailed {
        encoding: encoding.to_string(),
        source,
      })?;
    if read == 0 {
      break;
    }

    let next_len = decoded_len.saturating_add(read);
    if next_len > limit {
      return Err(ContentDecodeError::SizeLimitExceeded {
        decoded: next_len,
        limit,
      });
    }

    decoded
      .write_all(&buf[..read])
      .map_err(|source| ContentDecodeError::DecompressionFailed {
        encoding: encoding.to_string(),
        source,
      })?;
    decoded_len = next_len;
  }

  Ok(decoded.into_inner())
}

fn ensure_within_limit(len: usize, limit: usize) -> std::result::Result<(), ContentDecodeError> {
  if len > limit {
    return Err(ContentDecodeError::SizeLimitExceeded {
      decoded: len,
      limit,
    });
  }
  Ok(())
}

// ============================================================================
// Helper functions
// ============================================================================

/// Guess content-type from file path extension
fn guess_content_type_from_path(path: &str) -> Option<String> {
  let ext = Path::new(path)
    .extension()
    .and_then(|e| e.to_str())
    .map(|e| e.to_lowercase())?;

  let mime = match ext.as_str() {
    "png" => "image/png",
    "jpg" | "jpeg" => "image/jpeg",
    "gif" => "image/gif",
    "webp" => "image/webp",
    "avif" => "image/avif",
    "svg" => "image/svg+xml",
    "ico" => "image/x-icon",
    "bmp" => "image/bmp",
    "css" => "text/css",
    "html" | "htm" => "text/html",
    "js" => "application/javascript",
    "json" => "application/json",
    "woff" => "font/woff",
    "woff2" => "font/woff2",
    "ttf" => "font/ttf",
    "otf" => "font/otf",
    _ => return None,
  };

  Some(mime.to_string())
}

const OFFLINE_FIXTURE_PLACEHOLDER_PNG: &[u8] = &[
  0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
  0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
  0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
  0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
  0x42, 0x60, 0x82,
];
const OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME: &str = "image/png";
const OFFLINE_FIXTURE_PLACEHOLDER_WOFF2: &[u8] =
  include_bytes!("../tests/pages/fixtures/assets/fonts/DejaVuSans-subset.woff2");
const OFFLINE_FIXTURE_PLACEHOLDER_WOFF2_MIME: &str = "font/woff2";

/// Deterministic 1x1 PNG placeholder used for offline fixtures.
///
/// Exposed so tooling (e.g. `bundle_page cache --allow-missing`) can avoid writing empty/invalid
/// bytes for missing image resources.
pub fn offline_placeholder_png_bytes() -> &'static [u8] {
  OFFLINE_FIXTURE_PLACEHOLDER_PNG
}

/// Deterministic WOFF2 placeholder font used for offline fixtures.
///
/// Exposed so tooling (e.g. `bundle_page cache --allow-missing`) can avoid writing empty/invalid
/// bytes for missing font resources.
pub fn offline_placeholder_woff2_bytes() -> &'static [u8] {
  OFFLINE_FIXTURE_PLACEHOLDER_WOFF2
}

fn should_substitute_empty_image_body(
  kind: FetchContextKind,
  status: u16,
  headers: &HeaderMap,
) -> bool {
  // Some sites (notably Akamai `akam/13/pixel_*` tracking endpoints used on multiple pageset pages)
  // respond to `<img>` requests with an explicit empty entity body (`Content-Length: 0`) while still
  // returning a successful 2xx status and a non-image content-type. Treat those as a 1x1
  // transparent PNG so they don't surface as fetch failures (and so replaced element sizing has a
  // stable intrinsic size).
  matches!(kind, FetchContextKind::Image | FetchContextKind::ImageCors)
    && (200..300).contains(&status)
    && header_content_length_is_zero(headers)
}

fn url_has_captcha_param(url: &str) -> bool {
  url.contains("?captcha=") || url.contains("&captcha=")
}

fn should_substitute_captcha_image_response(
  kind: FetchContextKind,
  status: u16,
  final_url: &str,
) -> bool {
  // NYU (and similar bot-mitigation setups) can redirect blocked subresource requests to a URL with
  // `?captcha=...` and return an HTTP error status. Treat these as missing images rather than hard
  // failures so the renderer can proceed deterministically.
  matches!(kind, FetchContextKind::Image | FetchContextKind::ImageCors)
    && status == 405
    && url_has_captcha_param(final_url)
}

fn should_substitute_markup_image_body(
  kind: FetchContextKind,
  requested_url: &str,
  final_url: &str,
  content_type: Option<&str>,
  bytes: &[u8],
) -> bool {
  if !matches!(kind, FetchContextKind::Image | FetchContextKind::ImageCors) || bytes.is_empty() {
    return false;
  }
  // Avoid hiding "bot mitigation returned HTML for an image URL" cases (e.g. `.jpg` / `.png` / etc.)
  // which should remain visible via `ensure_image_mime_sane`.
  if url_looks_like_image_asset(requested_url) || url_looks_like_image_asset(final_url) {
    return false;
  }

  // Only substitute when the URL/content-type strongly suggests the response is an HTML document
  // being used in an image context (e.g. DailyMail `.html` modules).
  let is_obvious_html = url_looks_like_html(requested_url)
    || url_looks_like_html(final_url)
    || content_type.is_some_and(|ct| mime_is_html(content_type_mime(ct)));
  if !is_obvious_html {
    return false;
  }

  if content_type.is_some_and(|ct| mime_is_svg(content_type_mime(ct))) {
    return false;
  }
  file_payload_looks_like_markup_but_not_svg(bytes)
}

fn file_payload_looks_like_markup_but_not_svg(bytes: &[u8]) -> bool {
  let sample = &bytes[..bytes.len().min(256)];
  let mut i = 0;
  if sample.starts_with(b"\xef\xbb\xbf") {
    i = 3;
  }
  while i < sample.len() && sample[i].is_ascii_whitespace() {
    i += 1;
  }

  let mut rest = &sample[i..];
  if rest.is_empty() || rest[0] != b'<' {
    return false;
  }

  loop {
    if rest.len() >= 4 && rest[..4].eq_ignore_ascii_case(b"<svg") {
      return false;
    }
    if rest.len() >= 5 && rest[..5].eq_ignore_ascii_case(b"<?xml") {
      return false;
    }

    // Allow leading HTML-style comments in SVG documents.
    if rest.len() >= 4 && &rest[..4] == b"<!--" {
      let Some(end) = rest.windows(3).position(|window| window == b"-->") else {
        return true;
      };
      rest = &rest[end + 3..];
      while !rest.is_empty() && rest[0].is_ascii_whitespace() {
        rest = &rest[1..];
      }
      if rest.is_empty() {
        return true;
      }
      if rest[0] == b'<' {
        continue;
      }
      return false;
    }

    if rest.len() >= 9 && rest[..9].eq_ignore_ascii_case(b"<!doctype") {
      let mut j = 9;
      while j < rest.len() && rest[j].is_ascii_whitespace() {
        j += 1;
      }
      if j + 3 <= rest.len() && rest[j..j + 3].eq_ignore_ascii_case(b"svg") {
        return false;
      }
      return true;
    }

    return true;
  }
}

fn substitute_offline_fixture_placeholder_full(
  kind: FetchContextKind,
  bytes: &mut Vec<u8>,
  content_type: &mut Option<String>,
) {
  let should_replace = bytes.is_empty() || file_payload_looks_like_markup_but_not_svg(bytes);
  if !should_replace {
    return;
  }

  match kind {
    FetchContextKind::Image | FetchContextKind::ImageCors => {
      *bytes = OFFLINE_FIXTURE_PLACEHOLDER_PNG.to_vec();
      *content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
    }
    FetchContextKind::Font => {
      *bytes = OFFLINE_FIXTURE_PLACEHOLDER_WOFF2.to_vec();
      *content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_WOFF2_MIME.to_string());
    }
    _ => {}
  }
}

fn substitute_offline_fixture_placeholder_prefix(
  kind: FetchContextKind,
  bytes: &mut Vec<u8>,
  content_type: &mut Option<String>,
  read_limit: usize,
) {
  let should_replace = bytes.is_empty() || file_payload_looks_like_markup_but_not_svg(bytes);
  if !should_replace {
    return;
  }

  match kind {
    FetchContextKind::Image | FetchContextKind::ImageCors => {
      let take = OFFLINE_FIXTURE_PLACEHOLDER_PNG.len().min(read_limit);
      *bytes = OFFLINE_FIXTURE_PLACEHOLDER_PNG[..take].to_vec();
      *content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
    }
    FetchContextKind::Font => {
      let take = OFFLINE_FIXTURE_PLACEHOLDER_WOFF2.len().min(read_limit);
      *bytes = OFFLINE_FIXTURE_PLACEHOLDER_WOFF2[..take].to_vec();
      *content_type = Some(OFFLINE_FIXTURE_PLACEHOLDER_WOFF2_MIME.to_string());
    }
    _ => {}
  }
}

/// Return a list of filesystem path candidates for a given `file:` URL.
///
/// We prefer correct RFC semantics (strip query/fragment, percent-decode) via `Url::to_file_path`,
/// but fall back to the historical "strip `file://` and use the remaining string verbatim" behavior
/// for compatibility with existing offline fixtures.
fn file_url_path_candidates(url: &str) -> Vec<std::path::PathBuf> {
  let mut candidates = Vec::new();

  if let Ok(parsed) = Url::parse(url) {
    if parsed.scheme() == "file" {
      if let Ok(path) = parsed.to_file_path() {
        candidates.push(path);
      }
    }
  }

  let stripped = url.strip_prefix("file://").unwrap_or(url);
  let without_fragment = stripped
    .split_once('#')
    .map(|(before, _)| before)
    .unwrap_or(stripped);
  let without_query = without_fragment
    .split_once('?')
    .map(|(before, _)| before)
    .unwrap_or(without_fragment);

  candidates.push(std::path::PathBuf::from(without_query));
  candidates.push(std::path::PathBuf::from(stripped));

  let mut seen = HashSet::new();
  candidates.retain(|candidate| seen.insert(candidate.clone()));
  candidates
}

/// Decode a data: URL into bytes.
pub(crate) fn decode_data_url(url: &str) -> Result<FetchedResource> {
  if !is_data_url(url) {
    return Err(Error::Image(ImageError::InvalidDataUrl {
      reason: "URL does not start with 'data:'".to_string(),
    }));
  }

  data_url::decode_data_url(url)
}
// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
  use super::data_url;
  use super::*;
  use brotli::CompressorWriter;
  use flate2::write::GzEncoder;
  use flate2::Compression;
  use std::collections::VecDeque;
  use std::io;
  use std::io::Read;
  use std::io::Write;
  use std::net::TcpListener;
  use std::net::TcpStream;
  use std::sync::atomic::AtomicUsize;
  use std::sync::atomic::Ordering;
  use std::sync::Arc;
  use std::sync::Barrier;
  use std::sync::Mutex;
  use std::sync::OnceLock;
  use std::thread;
  use std::time::{Duration, Instant};
  use tempfile::NamedTempFile;

  fn try_bind_localhost(context: &str) -> Option<TcpListener> {
    match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => Some(listener),
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!("skipping {context}: cannot bind localhost in this environment: {err}");
        None
      }
      Err(err) => panic!("bind {context}: {err}"),
    }
  }

  #[test]
  fn url_looks_like_suffix_ignores_query_fragment_and_case() {
    assert!(url_looks_like_suffix(
      "http://example.com/PHOTO.JPG",
      ".jpg"
    ));
    assert!(url_looks_like_suffix(
      "http://example.com/photo.jpg?x=1",
      ".jpg"
    ));
    assert!(url_looks_like_suffix(
      "http://example.com/photo.jpg#frag",
      ".jpg"
    ));
    assert!(url_looks_like_suffix(
      "  http://example.com/photo.JpG?x=1#frag  ",
      ".jpg"
    ));
    assert!(!url_looks_like_suffix(
      "http://example.com/photo.jpgx",
      ".jpg"
    ));
    assert!(!url_looks_like_suffix("jpg", ".jpeg"));
  }

  #[test]
  fn url_looks_like_image_asset_ignores_query_fragment_and_case() {
    assert!(url_looks_like_image_asset(
      "  http://example.com/PHOTO.JPG?x=1#frag  "
    ));
    assert!(!url_looks_like_image_asset("http://example.com/photo.html"));
  }

  #[test]
  fn mime_predicates_are_case_insensitive() {
    assert!(mime_is_html("Text/Html"));
    assert!(mime_is_html("application/xhtml+xml"));
    assert!(mime_is_svg("IMAGE/SVG+XML"));
    assert!(mime_is_svg("image/svg+xml"));
    assert!(!mime_is_svg("image/png"));
  }

  #[test]
  fn header_has_nosniff_detection_is_case_insensitive() {
    let mut headers = HeaderMap::new();
    headers.insert(
      "X-Content-Type-Options",
      http::HeaderValue::from_static("NoSnIfF"),
    );
    assert!(header_has_nosniff(&headers));

    // Be permissive: treat values that *contain* `nosniff` as enabling the flag (matches how we
    // interpret real-world servers that include extra tokens).
    let mut headers = HeaderMap::new();
    headers.insert(
      "X-Content-Type-Options",
      http::HeaderValue::from_static("foo NoSnIfF bar"),
    );
    assert!(header_has_nosniff(&headers));
  }

  #[test]
  fn content_decode_does_not_overallocate_beyond_limit() {
    // The content decode limit should also act as an allocation cap, so the decoder doesn't
    // over-allocate (and potentially OOM abort) while still producing an output <= limit.
    let body = vec![b'a'; 10_000];
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&body).expect("gzip encode");
    let compressed = encoder.finish().expect("gzip finish");

    let decoded =
      decode_single_encoding("gzip", &compressed, body.len(), RenderStage::Paint).expect("decode");
    assert_eq!(decoded, body);
    assert!(
      decoded.capacity() <= body.len(),
      "decoded capacity {} exceeds limit {}",
      decoded.capacity(),
      body.len()
    );
  }

  #[test]
  fn read_response_prefix_does_not_overallocate_beyond_limit() {
    let body = vec![0x7fu8; 10_000];
    let mut cursor = std::io::Cursor::new(body.as_slice());
    let decoded = read_response_prefix(&mut cursor, body.len()).expect("read prefix");
    assert_eq!(decoded, body);
    assert!(
      decoded.capacity() <= body.len(),
      "read_response_prefix capacity {} exceeds limit {}",
      decoded.capacity(),
      body.len()
    );
  }

  #[test]
  fn fetch_file_does_not_overallocate_beyond_limit() {
    let body = vec![0x7fu8; 10_000];
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(&body).expect("write file");
    file.flush().expect("flush");

    let url = Url::from_file_path(file.path())
      .expect("file url")
      .to_string();
    let fetcher = HttpFetcher::new().with_max_size(body.len());
    let res = fetcher
      .fetch_with_context(FetchContextKind::Other, &url)
      .expect("fetch file");
    assert_eq!(res.bytes, body);
    assert!(
      res.bytes.capacity() <= body.len(),
      "file response capacity {} exceeds limit {}",
      res.bytes.capacity(),
      body.len()
    );
  }

  #[test]
  fn stylesheet_mime_sanity_rejects_markup_bodies_for_css_urls() {
    let mut resource = FetchedResource::new(
      b"<!DOCTYPE html><html><title>blocked</title></html>".to_vec(),
      Some("text/css".to_string()),
    );
    resource.status = Some(200);
    let url = "https://example.com/style.css";
    let err = ensure_stylesheet_mime_sane(&resource, url)
      .expect_err("expected markup payload to be rejected");
    assert!(
      err.to_string().contains("unexpected markup response body"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn stylesheet_mime_sanity_respects_nosniff_and_rejects_non_css_content_type() {
    let Some(listener) = try_bind_localhost(
      "stylesheet_mime_sanity_respects_nosniff_and_rejects_non_css_content_type",
    ) else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let body = b"body { color: red; }";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Content-Type-Options: nosniff\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;
    let started = Instant::now();
    let url = format!("http://{addr}/style.css");
    let res = fetcher
      .fetch_http_with_accept_inner_ureq(
        FetchContextKind::Stylesheet,
        FetchContextKind::Stylesheet.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        started,
        false,
      )
      .expect("fetch");
    handle.join().unwrap();

    assert!(res.nosniff, "expected nosniff header bit to be captured");
    let err = ensure_stylesheet_mime_sane(&res, &url)
      .expect_err("expected nosniff+text/plain stylesheet to be rejected");
    assert!(
      err.to_string().to_ascii_lowercase().contains("nosniff"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn stylesheet_mime_sanity_allows_text_plain_without_nosniff() {
    // Without `X-Content-Type-Options: nosniff`, FastRender intentionally preserves a looser
    // heuristic to avoid breaking pages that serve stylesheets with a non-CSS MIME type.
    let Some(listener) =
      try_bind_localhost("stylesheet_mime_sanity_allows_text_plain_without_nosniff")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let body = b"body { color: red; }";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;
    let started = Instant::now();
    let url = format!("http://{addr}/style.css");
    let res = fetcher
      .fetch_http_with_accept_inner_ureq(
        FetchContextKind::Stylesheet,
        FetchContextKind::Stylesheet.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        started,
        false,
      )
      .expect("fetch");
    handle.join().unwrap();

    assert!(
      !res.nosniff,
      "nosniff should default to false when header is absent"
    );
    ensure_stylesheet_mime_sane(&res, &url).expect("expected text/plain stylesheet to be allowed");
  }

  #[test]
  fn stylesheet_mime_sanity_rejects_non_css_content_type_when_nosniff_is_present() {
    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_STRICT_MIME".to_string(),
      "1".to_string(),
    )])));
    runtime::with_thread_runtime_toggles(toggles, || {
      let mut resource =
        FetchedResource::new(b"body{}".to_vec(), Some("text/plain".to_string()));
      resource.status = Some(200);
      resource.nosniff = true;
      let url = "https://example.com/style.css";
      let err = ensure_stylesheet_mime_sane(&resource, url)
        .expect_err("expected non-css content-type to be rejected under nosniff");
      assert!(
        err.to_string().contains("unexpected content-type"),
        "unexpected error: {err}"
      );
    });
  }

  #[test]
  fn stylesheet_mime_sanity_allows_non_css_content_type_without_nosniff() {
    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_STRICT_MIME".to_string(),
      "1".to_string(),
    )])));
    runtime::with_thread_runtime_toggles(toggles, || {
      let mut resource =
        FetchedResource::new(b"body{}".to_vec(), Some("text/plain".to_string()));
      resource.status = Some(200);
      let url = "https://example.com/style.css";
      ensure_stylesheet_mime_sane(&resource, url)
        .expect("expected permissive content-type handling without nosniff");
    });
  }

  #[test]
  fn font_mime_sanity_rejects_markup_bodies_for_font_urls() {
    let mut resource = FetchedResource::new(
      b"<!DOCTYPE html><html><title>blocked</title></html>".to_vec(),
      Some("application/octet-stream".to_string()),
    );
    resource.status = Some(200);
    let url = "https://example.com/font.woff2";
    let err =
      ensure_font_mime_sane(&resource, url).expect_err("expected markup payload to be rejected");
    assert!(
      err.to_string().contains("unexpected markup response body"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn inflight_wait_recovers_from_poisoned_lock() {
    let inflight = InFlight::new();
    let result = std::panic::catch_unwind(|| {
      let _guard = inflight.result.lock().unwrap();
      panic!("poison inflight lock");
    });
    assert!(result.is_err(), "expected panic to be caught");

    let deadline = render_control::RenderDeadline::new(Some(Duration::from_millis(50)), None);
    render_control::with_deadline(Some(&deadline), || {
      inflight.set(SharedResult::Success(FetchedResource::new(
        vec![1, 2, 3],
        Some("text/plain".to_string()),
      )));
      let key = CacheKey::new(
        FetchContextKind::Other,
        "https://example.com/test.txt".to_string(),
      );
      let res = inflight.wait(&key).expect("wait result");
      assert_eq!(res.bytes, vec![1, 2, 3]);
    });
  }

  #[test]
  fn http_browser_font_origin_and_referer_are_derived_from_url_origin() {
    let url = "https://www.washingtonpost.com/wp-stat/assets/fonts/ITC_Franklin-Light.woff2";
    let profile = http_browser_request_profile_for_url(url);
    assert_eq!(profile, FetchDestination::Font);
    let parsed = Url::parse(url).expect("parse url");
    let (origin, referer) = profile
      .origin_and_referer(&parsed)
      .expect("expected origin+referer for font profile");
    assert_eq!(origin, "https://www.washingtonpost.com");
    assert_eq!(referer, "https://www.washingtonpost.com/");
  }

  #[test]
  fn http_browser_font_origin_and_referer_include_non_default_port() {
    let url = "https://example.com:8443/fonts/test.woff2";
    let profile = http_browser_request_profile_for_url(url);
    assert_eq!(profile, FetchDestination::Font);
    let parsed = Url::parse(url).expect("parse url");
    let (origin, referer) = profile
      .origin_and_referer(&parsed)
      .expect("expected origin+referer for font profile");
    assert_eq!(origin, "https://example.com:8443");
    assert_eq!(referer, "https://example.com:8443/");
  }

  #[test]
  fn http_browser_non_font_does_not_set_origin_or_referer() {
    let url = "https://example.com/style.css";
    let profile = http_browser_request_profile_for_url(url);
    assert_eq!(profile, FetchDestination::Style);
    let parsed = Url::parse(url).expect("parse url");
    assert_eq!(profile.origin_and_referer(&parsed), None);
  }

  #[test]
  fn cors_cache_partition_key_partitions_by_client_origin_and_credentials() {
    if !http_browser_headers_enabled() {
      eprintln!(
        "skipping cors_cache_partition_key_partitions_by_client_origin_and_credentials: browser-like request headers are disabled"
      );
      return;
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
      "1".to_string(),
    )])));
    runtime::with_thread_runtime_toggles(toggles, || {
      let origin_a = origin_from_url("https://a.test/page").expect("origin A");
      let origin_b = origin_from_url("https://b.test/page").expect("origin B");

      let cors_url = "https://static.example.com/font.woff2";

      let no_cors = FetchRequest::new("https://static.example.com/style.css", FetchDestination::Style)
        .with_client_origin(&origin_a);
      assert_eq!(cors_cache_partition_key(&no_cors), None);

      let omit = FetchRequest::new(cors_url, FetchDestination::Font).with_client_origin(&origin_a);
      assert_eq!(
        cors_cache_partition_key(&omit).as_deref(),
        Some("https://a.test")
      );

      let include = omit.with_credentials_mode(FetchCredentialsMode::Include);
      assert_eq!(
        cors_cache_partition_key(&include).as_deref(),
        Some("https://a.test|cred=include")
      );

      let same_origin_url = "https://a.test/font.woff2";
      let same_origin = FetchRequest::new(same_origin_url, FetchDestination::Font)
        .with_client_origin(&origin_a)
        .with_credentials_mode(FetchCredentialsMode::SameOrigin);
      assert_eq!(
        cors_cache_partition_key(&same_origin).as_deref(),
        Some("https://a.test|cred=include")
      );

      let cross_origin = FetchRequest::new(cors_url, FetchDestination::Font)
        .with_client_origin(&origin_a)
        .with_credentials_mode(FetchCredentialsMode::SameOrigin);
      assert_eq!(
        cors_cache_partition_key(&cross_origin).as_deref(),
        Some("https://a.test")
      );

      let other_origin = FetchRequest::new(cors_url, FetchDestination::Font).with_client_origin(&origin_b);
      assert_eq!(
        cors_cache_partition_key(&other_origin).as_deref(),
        Some("https://b.test")
      );
    });
  }

  #[test]
  fn cors_cache_partition_key_can_be_disabled() {
    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
      "0".to_string(),
    )])));
    runtime::with_thread_runtime_toggles(toggles, || {
      let origin = origin_from_url("https://example.com/page").expect("origin");
      let req = FetchRequest::new("https://static.example.com/font.woff2", FetchDestination::Font)
        .with_client_origin(&origin)
        .with_credentials_mode(FetchCredentialsMode::Include);
      assert_eq!(cors_cache_partition_key(&req), None);
    });
  }

  #[test]
  fn cors_cache_partition_key_falls_back_to_referrer_url_and_null_for_file() {
    if !http_browser_headers_enabled() {
      eprintln!(
        "skipping cors_cache_partition_key_falls_back_to_referrer_url_and_null_for_file: browser-like request headers are disabled"
      );
      return;
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
      "1".to_string(),
    )])));
    runtime::with_thread_runtime_toggles(toggles, || {
      let url = "https://static.example.com/font.woff2";

      let http_referrer =
        FetchRequest::new(url, FetchDestination::Font).with_referrer_url("https://a.test/page.html");
      assert_eq!(
        cors_cache_partition_key(&http_referrer).as_deref(),
        Some("https://a.test")
      );

      let file_referrer =
        FetchRequest::new(url, FetchDestination::Font).with_referrer_url("file:///tmp/page.html");
      assert_eq!(
        cors_cache_partition_key(&file_referrer).as_deref(),
        Some("null")
      );

      // When a referrer is provided but we cannot derive a browser-style origin from it, match
      // header generation behavior: omit the Origin header entirely (and therefore do not
      // partition).
      let invalid_referrer =
        FetchRequest::new(url, FetchDestination::Font).with_referrer_url("not a url");
      assert_eq!(cors_cache_partition_key(&invalid_referrer), None);
    });
  }

  #[test]
  fn cors_cache_partition_key_uses_null_for_non_http_client_origin() {
    if !http_browser_headers_enabled() {
      eprintln!(
        "skipping cors_cache_partition_key_uses_null_for_non_http_client_origin: browser-like request headers are disabled"
      );
      return;
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
      "1".to_string(),
    )])));
    runtime::with_thread_runtime_toggles(toggles, || {
      let url = "https://static.example.com/font.woff2";
      let file_origin = origin_from_url("file:///tmp/doc.html").expect("file origin");
      let req = FetchRequest::new(url, FetchDestination::Font).with_client_origin(&file_origin);
      assert_eq!(cors_cache_partition_key(&req).as_deref(), Some("null"));
    });
  }

  fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
      .iter()
      .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
      .map(|(_, value)| value.as_str())
  }

  #[test]
  fn http_headers_iframe_navigation_profile_sets_iframe_dest_and_omits_user() {
    let req = FetchRequest::new("https://www.example.com/frame", FetchDestination::Iframe)
      .with_referrer_url("https://www.example.com/page");
    let headers = build_http_header_pairs(
      req.url,
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      req.destination,
      req.client_origin,
      req.referrer_url,
      req.referrer_policy,
    );
    assert_eq!(header_value(&headers, "Sec-Fetch-Dest"), Some("iframe"));
    assert_eq!(header_value(&headers, "Sec-Fetch-Mode"), Some("navigate"));
    assert_eq!(header_value(&headers, "Sec-Fetch-User"), None);
    assert_eq!(header_value(&headers, "Upgrade-Insecure-Requests"), Some("1"));
  }

  #[test]
  fn http_headers_fetch_profile_uses_empty_dest_and_cors_mode() {
    let client_origin =
      origin_from_url("https://client.example.com/page.html").expect("client origin");
    let req = FetchRequest::new("https://api.other.com/data.json", FetchDestination::Fetch)
      .with_client_origin(&client_origin);
    let headers = build_http_header_pairs(
      req.url,
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      req.destination,
      req.client_origin,
      req.referrer_url,
      req.referrer_policy,
    );
    assert_eq!(header_value(&headers, "Accept"), Some("*/*"));
    assert_eq!(header_value(&headers, "Sec-Fetch-Dest"), Some("empty"));
    assert_eq!(header_value(&headers, "Sec-Fetch-Mode"), Some("cors"));
    assert_eq!(
      header_value(&headers, "Origin"),
      Some("https://client.example.com")
    );
    assert_eq!(header_value(&headers, "Sec-Fetch-User"), None);
    assert_eq!(header_value(&headers, "Upgrade-Insecure-Requests"), None);
  }

  #[test]
  fn http_headers_fetch_profile_sends_null_origin_for_file_client_origin() {
    let client_origin = origin_from_url("file:///tmp/page.html").expect("file origin");
    let req = FetchRequest::new("https://example.com/data.json", FetchDestination::Fetch)
      .with_client_origin(&client_origin);
    let headers = build_http_header_pairs(
      req.url,
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      req.destination,
      req.client_origin,
      req.referrer_url,
      req.referrer_policy,
    );
    assert_eq!(header_value(&headers, "Origin"), Some("null"));
  }

  #[test]
  fn http_headers_other_profile_remains_no_cors() {
    let client_origin =
      origin_from_url("https://client.example.com/page.html").expect("client origin");
    let req = FetchRequest::new("https://api.other.com/data.json", FetchDestination::Other)
      .with_client_origin(&client_origin);
    let headers = build_http_header_pairs(
      req.url,
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      req.destination,
      req.client_origin,
      req.referrer_url,
      req.referrer_policy,
    );
    assert_eq!(header_value(&headers, "Sec-Fetch-Dest"), Some("empty"));
    assert_eq!(header_value(&headers, "Sec-Fetch-Mode"), Some("no-cors"));
    assert_eq!(header_value(&headers, "Origin"), None);
  }

  #[test]
  fn referrer_policy_parse_trims_and_is_case_insensitive() {
    assert_eq!(
      ReferrerPolicy::parse(" strict-origin-when-cross-origin "),
      Some(ReferrerPolicy::StrictOriginWhenCrossOrigin)
    );
    assert_eq!(
      ReferrerPolicy::parse("UNSAFE-URL"),
      Some(ReferrerPolicy::UnsafeUrl)
    );
    assert_eq!(ReferrerPolicy::parse(""), Some(ReferrerPolicy::EmptyString));
    assert_eq!(ReferrerPolicy::parse("unknown-policy"), None);
  }

  #[test]
  fn referrer_policy_parse_value_list_last_token_wins() {
    assert_eq!(
      ReferrerPolicy::parse_value_list("unknown, origin, no-referrer"),
      Some(ReferrerPolicy::NoReferrer)
    );
    assert_eq!(
      ReferrerPolicy::parse_value_list("origin no-referrer"),
      Some(ReferrerPolicy::NoReferrer)
    );
    assert_eq!(ReferrerPolicy::parse_value_list(""), None);
  }

  #[test]
  fn referrer_policy_matrix_generates_expected_referer_values() {
    let referrer = "https://example.com/path/page.html?q=1#frag";
    let same_origin = "https://example.com/img.png";
    let cross_origin = "https://other.com/img.png";
    let downgrade = "http://other.com/img.png";

    let full = Some("https://example.com/path/page.html?q=1");
    let origin_only = Some("https://example.com/");

    let cases = [
      (ReferrerPolicy::NoReferrer, None, None, None),
      (ReferrerPolicy::NoReferrerWhenDowngrade, full, full, None),
      (ReferrerPolicy::Origin, origin_only, origin_only, origin_only),
      (ReferrerPolicy::OriginWhenCrossOrigin, full, origin_only, origin_only),
      (ReferrerPolicy::SameOrigin, full, None, None),
      (ReferrerPolicy::StrictOrigin, origin_only, origin_only, None),
      (ReferrerPolicy::StrictOriginWhenCrossOrigin, full, origin_only, None),
      (ReferrerPolicy::UnsafeUrl, full, full, full),
      (ReferrerPolicy::EmptyString, full, origin_only, None),
    ];

    for (policy, expected_same, expected_cross, expected_downgrade) in cases {
      assert_eq!(
        http_referer_header_value(referrer, same_origin, policy).as_deref(),
        expected_same,
        "same-origin policy mismatch for {policy:?}"
      );
      assert_eq!(
        http_referer_header_value(referrer, cross_origin, policy).as_deref(),
        expected_cross,
        "cross-origin policy mismatch for {policy:?}"
      );
      assert_eq!(
        http_referer_header_value(referrer, downgrade, policy).as_deref(),
        expected_downgrade,
        "downgrade policy mismatch for {policy:?}"
      );
    }
  }

  fn referrer_policy_omits_file_referrers() {
    let referrer = "file:///tmp/page.html#frag";
    let target = "https://example.com/img.png";
    for policy in [
      ReferrerPolicy::EmptyString,
      ReferrerPolicy::NoReferrer,
      ReferrerPolicy::NoReferrerWhenDowngrade,
      ReferrerPolicy::Origin,
      ReferrerPolicy::OriginWhenCrossOrigin,
      ReferrerPolicy::SameOrigin,
      ReferrerPolicy::StrictOrigin,
      ReferrerPolicy::StrictOriginWhenCrossOrigin,
      ReferrerPolicy::UnsafeUrl,
    ] {
      assert_eq!(
        http_referer_header_value(referrer, target, policy),
        None,
        "expected no referer for file referrer under {policy:?}"
      );
    }
  }

  #[test]
  fn http_headers_referrer_policy_no_referrer_omits_header() {
    let headers = build_http_header_pairs(
      "https://www.example.com/img.png",
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      FetchContextKind::Image.into(),
      None,
      Some("https://www.example.com/page"),
      ReferrerPolicy::NoReferrer,
    );
    assert_eq!(header_value(&headers, "Referer"), None);
  }

  #[test]
  fn http_headers_respect_referrer_policy_override() {
    let headers = build_http_header_pairs(
      "https://static.example.com/img.png",
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      FetchContextKind::Image.into(),
      None,
      Some("https://www.example.com/page"),
      ReferrerPolicy::NoReferrer,
    );
    assert_eq!(header_value(&headers, "Sec-Fetch-Site"), Some("same-site"));
    assert_eq!(header_value(&headers, "Referer"), None);
  }

  #[test]
  fn http_headers_referrer_policy_origin_sends_origin_only_even_same_origin() {
    let headers = build_http_header_pairs(
      "https://www.example.com/img.png",
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      FetchContextKind::Image.into(),
      None,
      Some("https://www.example.com/page?a=b#frag"),
      ReferrerPolicy::Origin,
    );
    assert_eq!(
      header_value(&headers, "Referer"),
      Some("https://www.example.com/")
    );
  }

  #[test]
  fn http_headers_referrer_policy_same_origin_omits_cross_origin_referrer() {
    let headers = build_http_header_pairs(
      "https://cdn.other.com/img.png",
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      FetchContextKind::Image.into(),
      None,
      Some("https://www.example.com/page"),
      ReferrerPolicy::SameOrigin,
    );
    assert_eq!(header_value(&headers, "Referer"), None);
  }

  #[test]
  fn http_headers_same_origin_referrer_strips_fragment() {
    let headers = build_http_header_pairs(
      "https://www.example.com/img.png",
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      FetchContextKind::Image.into(),
      None,
      Some("https://www.example.com/page?a=b#frag"),
      ReferrerPolicy::default(),
    );
    assert_eq!(
      header_value(&headers, "Sec-Fetch-Site"),
      Some("same-origin")
    );
    assert_eq!(
      header_value(&headers, "Referer"),
      Some("https://www.example.com/page?a=b")
    );
  }

  #[test]
  fn http_headers_font_request_splits_client_origin_and_referrer_url() {
    let client_origin = origin_from_url("http://client.example.com/").expect("client origin");
    let headers = build_http_header_pairs(
      "http://static.example.com/asset.woff2",
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      FetchContextKind::Font.into(),
      Some(&client_origin),
      Some("http://ref.other.com/style.css"),
      ReferrerPolicy::default(),
    );

    assert_eq!(
      header_value(&headers, "Origin"),
      Some("http://client.example.com")
    );
    assert_eq!(header_value(&headers, "Referer"), Some("http://ref.other.com/"));
    assert_eq!(header_value(&headers, "Sec-Fetch-Site"), Some("same-site"));
  }

  #[test]
  fn http_headers_font_request_with_file_client_origin_sends_null_origin() {
    let client_origin = origin_from_url("file:///tmp/doc.html").expect("file origin");
    let headers = build_http_header_pairs(
      "https://example.com/asset.woff2",
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      FetchContextKind::Font.into(),
      Some(&client_origin),
      None,
      ReferrerPolicy::default(),
    );

    assert_eq!(header_value(&headers, "Origin"), Some("null"));
  }

  #[test]
  fn http_headers_same_site_cross_origin_referrer_is_origin_only() {
    let headers = build_http_header_pairs(
      "https://static.example.com/img.png",
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      FetchContextKind::Image.into(),
      None,
      Some("https://www.example.com/page"),
      ReferrerPolicy::default(),
    );
    assert_eq!(header_value(&headers, "Sec-Fetch-Site"), Some("same-site"));
    assert_eq!(
      header_value(&headers, "Referer"),
      Some("https://www.example.com/")
    );
  }

  #[test]
  fn http_headers_cross_site_referrer_is_origin_only() {
    let headers = build_http_header_pairs(
      "https://cdn.other.com/img.png",
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      FetchContextKind::Image.into(),
      None,
      Some("https://www.example.com/page"),
      ReferrerPolicy::default(),
    );
    assert_eq!(header_value(&headers, "Sec-Fetch-Site"), Some("cross-site"));
    assert_eq!(
      header_value(&headers, "Referer"),
      Some("https://www.example.com/")
    );
  }

  #[test]
  fn http_headers_https_to_http_downgrade_omits_referrer() {
    let headers = build_http_header_pairs(
      "http://static.example.com/img.png",
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      FetchContextKind::Image.into(),
      None,
      Some("https://www.example.com/page"),
      ReferrerPolicy::default(),
    );
    assert_eq!(header_value(&headers, "Sec-Fetch-Site"), Some("cross-site"));
    assert_eq!(header_value(&headers, "Referer"), None);
  }

  #[test]
  fn http_headers_unparseable_target_url_normalizes_referrer_origin() {
    let headers = build_http_header_pairs(
      "https://www.apple.com/wss/fonts?families=SF+Pro,v3|SF+Pro+Icons,v3",
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      FetchContextKind::Stylesheet.into(),
      None,
      Some("https://developer.apple.com"),
      ReferrerPolicy::default(),
    );
    assert_eq!(header_value(&headers, "Sec-Fetch-Site"), Some("same-site"));
    assert_eq!(
      header_value(&headers, "Referer"),
      Some("https://developer.apple.com/")
    );
  }

  #[test]
  fn http_headers_unparseable_target_url_does_not_fall_back_to_raw_referrer() {
    let headers = build_http_header_pairs(
      "https://static.example.com/img.png?q=hello world",
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      FetchContextKind::Image.into(),
      None,
      Some("https://www.example.com/page"),
      ReferrerPolicy::default(),
    );
    assert_eq!(header_value(&headers, "Sec-Fetch-Site"), Some("same-site"));
    assert_eq!(
      header_value(&headers, "Referer"),
      Some("https://www.example.com/")
    );
  }

  #[test]
  fn http_headers_unparseable_target_url_strips_referrer_fragment() {
    let headers = build_http_header_pairs(
      "https://www.apple.com/wss/fonts?families=SF+Pro,v3|SF+Pro+Icons,v3",
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      SUPPORTED_ACCEPT_ENCODING,
      None,
      FetchContextKind::Stylesheet.into(),
      None,
      Some("https://developer.apple.com#frag"),
      ReferrerPolicy::default(),
    );
    assert_eq!(
      header_value(&headers, "Referer"),
      Some("https://developer.apple.com/")
    );
  }

  #[test]
  fn http_browser_tolerant_origin_extracts_authority() {
    let origin = http_browser_tolerant_origin_from_url(
      "https://www.apple.com/wss/fonts?families=SF+Pro,v3|SF+Pro+Icons,v3",
    )
    .expect("origin");
    assert_eq!(origin.scheme(), "https");
    assert_eq!(origin.host(), Some("www.apple.com"));
    assert_eq!(origin.effective_port(), Some(443));
  }

  #[test]
  fn http_browser_tolerant_origin_extracts_non_default_port() {
    let origin =
      http_browser_tolerant_origin_from_url("https://example.com:8443/foo?q=a|b").expect("origin");
    assert_eq!(origin.scheme(), "https");
    assert_eq!(origin.host(), Some("example.com"));
    assert_eq!(origin.port(), Some(8443));
  }

  #[test]
  fn http_backend_auto_falls_back_to_curl_for_tls_like_errors() {
    let err = Error::Resource(ResourceError::new(
      "https://example.com",
      "TLS handshake failed",
    ));
    assert!(should_fallback_to_curl(&err));
  }

  #[test]
  fn http_backend_auto_falls_back_to_curl_for_http2_like_errors() {
    let err = Error::Resource(ResourceError::new(
      "https://example.com",
      "HTTP/2 internal error",
    ));
    assert!(should_fallback_to_curl(&err));
  }

  #[test]
  fn error_looks_like_dns_failure_detects_not_found_error_sources() {
    let io_err = io::Error::new(io::ErrorKind::NotFound, "dns lookup failed");
    let err = Error::Resource(
      ResourceError::new(
        "https://example.com",
        "error sending request for url (https://example.com)",
      )
      .with_source(io_err),
    );
    assert!(error_looks_like_dns_failure(&err));
  }

  #[test]
  fn http_backend_auto_falls_back_to_curl_for_error_sending_request_messages() {
    let err = Error::Resource(ResourceError::new(
      "https://example.com",
      "error sending request for url (https://example.com)",
    ));
    assert!(should_fallback_to_curl(&err));
  }

  #[test]
  fn http_backend_auto_does_not_fallback_based_on_url_substrings() {
    let err = Error::Resource(ResourceError::new(
      "https://tls.example.com",
      "unexpected status",
    ));
    assert!(!should_fallback_to_curl(&err));
  }

  #[test]
  fn reqwest_auto_backend_does_not_disable_retries_without_budget() {
    let Some(listener) =
      try_bind_localhost("reqwest_auto_backend_does_not_disable_retries_without_budget")
    else {
      return;
    };
    let addr = listener.local_addr().expect("local addr");
    drop(listener);

    let retry = HttpRetryPolicy {
      max_attempts: 3,
      backoff_base: Duration::ZERO,
      backoff_cap: Duration::ZERO,
      respect_retry_after: true,
    };
    let fetcher = HttpFetcher::new()
      .with_retry_policy(retry)
      .with_timeout(Duration::from_millis(50));

    let deadline = None;
    let started = Instant::now();
    let url = format!("http://{addr}/");
    let err = fetcher
      .fetch_http_with_accept_inner_reqwest(
        FetchContextKind::Other,
        FetchContextKind::Other.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        started,
        true,
      )
      .expect_err("reqwest should error when no server is listening");
    let msg = err.to_string();
    assert!(
      msg.contains("attempt 3/3"),
      "expected retries to be attempted (message: {msg})"
    );
  }

  #[test]
  fn http_404_empty_body_surfaces_status_not_empty_body_ureq() {
    let Some(listener) =
      try_bind_localhost("http_404_empty_body_surfaces_status_not_empty_body_ureq")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let headers =
        "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(headers.as_bytes()).unwrap();
    });

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;
    let started = Instant::now();
    let url = format!("http://{addr}/missing.txt");
    let res = fetcher
      .fetch_http_with_accept_inner_ureq(
        FetchContextKind::Other,
        FetchContextKind::Other.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        started,
        false,
      )
      .expect("HTTP 404 should return a resource so callers can surface the status");
    handle.join().unwrap();

    assert_eq!(res.status, Some(404));
    assert!(
      res.bytes.is_empty(),
      "expected empty body from test server, got {} bytes",
      res.bytes.len()
    );

    let err = ensure_http_success(&res, &url).expect_err("HTTP 404 should surface as an error");
    let msg = err.to_string();
    assert!(msg.contains("HTTP status 404"), "unexpected error: {msg}");
    assert!(
      !msg.contains("empty HTTP response body"),
      "unexpected error: {msg}"
    );
  }

  #[test]
  fn http_404_empty_body_surfaces_status_not_empty_body_reqwest() {
    let Some(listener) =
      try_bind_localhost("http_404_empty_body_surfaces_status_not_empty_body_reqwest")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let headers =
        "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(headers.as_bytes()).unwrap();
    });

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;
    let started = Instant::now();
    let url = format!("http://{addr}/missing.txt");
    let res = fetcher
      .fetch_http_with_accept_inner_reqwest(
        FetchContextKind::Other,
        FetchContextKind::Other.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        started,
        false,
      )
      .expect("HTTP 404 should return a resource so callers can surface the status");
    handle.join().unwrap();

    assert_eq!(res.status, Some(404));
    assert!(
      res.bytes.is_empty(),
      "expected empty body from test server, got {} bytes",
      res.bytes.len()
    );

    let err = ensure_http_success(&res, &url).expect_err("HTTP 404 should surface as an error");
    let msg = err.to_string();
    assert!(msg.contains("HTTP status 404"), "unexpected error: {msg}");
    assert!(
      !msg.contains("empty HTTP response body"),
      "unexpected error: {msg}"
    );
  }

  #[test]
  fn http_fetch_populates_cors_response_headers() {
    if matches!(http_backend_mode(), HttpBackendMode::Curl) && !curl_backend::curl_available() {
      eprintln!(
        "skipping http_fetch_populates_cors_response_headers: curl backend selected but curl is unavailable"
      );
      return;
    }

    let Some(listener) = try_bind_localhost("http_fetch_populates_cors_response_headers") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let body = b"ok";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin:  https://example.com  \r\nTiming-Allow-Origin: *  \r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let url = format!("http://{addr}/cors.txt");
    let res = fetcher
      .fetch_with_context(FetchContextKind::Other, &url)
      .expect("fetch");

    handle.join().unwrap();

    assert_eq!(
      res.access_control_allow_origin.as_deref(),
      Some("https://example.com")
    );
    assert_eq!(res.timing_allow_origin.as_deref(), Some("*"));
  }

  #[test]
  fn http_200_empty_body_is_still_an_error_ureq() {
    let Some(listener) = try_bind_localhost("http_200_empty_body_is_still_an_error_ureq") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n";
      stream.write_all(headers.as_bytes()).unwrap();
    });

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;
    let started = Instant::now();
    let url = format!("http://{addr}/empty.txt");
    let err = fetcher
      .fetch_http_with_accept_inner_ureq(
        FetchContextKind::Other,
        FetchContextKind::Other.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        started,
        false,
      )
      .expect_err("empty 200 response should be treated as suspicious");
    handle.join().unwrap();
    assert!(
      err.to_string().contains("empty HTTP response body"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn http_empty_stylesheet_with_content_length_zero_is_ok_ureq() {
    let Some(listener) =
      try_bind_localhost("http_empty_stylesheet_with_content_length_zero_is_ok_ureq")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let headers =
        "HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(headers.as_bytes()).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let deadline = None;
    let started = Instant::now();
    let url = format!("http://{addr}/empty.css");
    let res = fetcher
      .fetch_http_with_accept_inner_ureq(
        FetchContextKind::Stylesheet,
        FetchContextKind::Stylesheet.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        started,
        false,
      )
      .expect("stylesheet fetch should succeed");
    handle.join().unwrap();

    assert!(
      res.bytes.is_empty(),
      "expected empty stylesheet body to be accepted"
    );
    assert_eq!(res.status, Some(200));
    assert_eq!(res.content_type.as_deref(), Some("text/css"));
  }

  #[test]
  fn http_empty_stylesheet_without_content_length_is_error() {
    let Some(listener) =
      try_bind_localhost("http_empty_stylesheet_without_content_length_is_error")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nConnection: close\r\n\r\n";
      stream.write_all(headers.as_bytes()).unwrap();
    });

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;
    let started = Instant::now();
    let url = format!("http://{addr}/empty.css");
    let err = fetcher
      .fetch_http_with_accept_inner_ureq(
        FetchContextKind::Stylesheet,
        FetchContextKind::Stylesheet.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        started,
        false,
      )
      .expect_err("stylesheet fetch should reject unexpected empty body");
    handle.join().unwrap();
    assert!(
      err.to_string().contains("empty HTTP response body"),
      "unexpected error message: {err}"
    );
  }

  #[test]
  fn http_empty_image_with_content_length_zero_substitutes_placeholder() {
    let headers =
      "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;

    {
      let Some(listener) = try_bind_localhost("http_empty_image_body_substitutes_placeholder_ureq")
      else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let _ = read_http_request(&mut stream);
        stream.write_all(headers.as_bytes()).unwrap();
      });

      let url = format!("http://{addr}/pixel");
      let res = fetcher
        .fetch_http_with_accept_inner_ureq(
          FetchContextKind::Image,
          FetchContextKind::Image.into(),
          &url,
          None,
          None,
          None,
          None,
          ReferrerPolicy::default(),
          FetchCredentialsMode::Include,
          &deadline,
          Instant::now(),
          false,
        )
        .expect("ureq image fetch should substitute placeholder");
      handle.join().unwrap();
      assert_eq!(res.bytes, OFFLINE_FIXTURE_PLACEHOLDER_PNG);
      assert_eq!(
        res.content_type.as_deref(),
        Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME)
      );
    }

    {
      let Some(listener) =
        try_bind_localhost("http_empty_image_body_substitutes_placeholder_ureq_partial")
      else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let _ = read_http_request(&mut stream);
        stream.write_all(headers.as_bytes()).unwrap();
      });

      let url = format!("http://{addr}/pixel");
      let res = fetcher
        .fetch_http_partial_inner_ureq(
          FetchContextKind::Image,
          FetchContextKind::Image.into(),
          &url,
          8,
          None,
          None,
          ReferrerPolicy::default(),
          FetchCredentialsMode::Include,
          &deadline,
          Instant::now(),
        )
        .expect("ureq image prefix should substitute placeholder");
      handle.join().unwrap();
      assert_eq!(res.bytes, OFFLINE_FIXTURE_PLACEHOLDER_PNG[..8]);
      assert_eq!(
        res.content_type.as_deref(),
        Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME)
      );
    }

    {
      let Some(listener) =
        try_bind_localhost("http_empty_image_body_substitutes_placeholder_reqwest")
      else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let _ = read_http_request(&mut stream);
        stream.write_all(headers.as_bytes()).unwrap();
      });

      let url = format!("http://{addr}/pixel");
      let res = fetcher
        .fetch_http_with_accept_inner_reqwest(
          FetchContextKind::Image,
          FetchContextKind::Image.into(),
          &url,
          None,
          None,
          None,
          None,
          ReferrerPolicy::default(),
          FetchCredentialsMode::Include,
          &deadline,
          Instant::now(),
          false,
        )
        .expect("reqwest image fetch should substitute placeholder");
      handle.join().unwrap();
      assert_eq!(res.bytes, OFFLINE_FIXTURE_PLACEHOLDER_PNG);
      assert_eq!(
        res.content_type.as_deref(),
        Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME)
      );
    }

    {
      let Some(listener) =
        try_bind_localhost("http_empty_image_body_substitutes_placeholder_reqwest_partial")
      else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let _ = read_http_request(&mut stream);
        stream.write_all(headers.as_bytes()).unwrap();
      });

      let url = format!("http://{addr}/pixel");
      let res = fetcher
        .fetch_http_partial_inner_reqwest(
          FetchContextKind::Image,
          FetchContextKind::Image.into(),
          &url,
          8,
          None,
          None,
          ReferrerPolicy::default(),
          FetchCredentialsMode::Include,
          &deadline,
          Instant::now(),
        )
        .expect("reqwest image prefix should substitute placeholder");
      handle.join().unwrap();
      assert_eq!(res.bytes, OFFLINE_FIXTURE_PLACEHOLDER_PNG[..8]);
      assert_eq!(
        res.content_type.as_deref(),
        Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME)
      );
    }
  }

  #[test]
  fn http_html_image_payload_substitutes_placeholder() {
    let body = "<!DOCTYPE html><html><title>nope</title></html>";
    let headers = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
      body.len(),
      body
    );

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;

    {
      let Some(listener) =
        try_bind_localhost("http_html_image_payload_substitutes_placeholder_ureq")
      else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      let response = headers.clone();
      let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let _ = read_http_request(&mut stream);
        stream.write_all(response.as_bytes()).unwrap();
      });

      let url = format!("http://{addr}/pixel.html");
      let res = fetcher
        .fetch_http_with_accept_inner_ureq(
          FetchContextKind::Image,
          FetchContextKind::Image.into(),
          &url,
          None,
          None,
          None,
          None,
          ReferrerPolicy::default(),
          FetchCredentialsMode::Include,
          &deadline,
          Instant::now(),
          false,
        )
        .expect("ureq image fetch should substitute placeholder");
      handle.join().unwrap();
      assert_eq!(res.bytes, OFFLINE_FIXTURE_PLACEHOLDER_PNG);
      assert_eq!(
        res.content_type.as_deref(),
        Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME)
      );
    }

    {
      let Some(listener) =
        try_bind_localhost("http_html_image_payload_substitutes_placeholder_ureq_partial")
      else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      let response = headers.clone();
      let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let _ = read_http_request(&mut stream);
        stream.write_all(response.as_bytes()).unwrap();
      });

      let url = format!("http://{addr}/pixel.html");
      let res = fetcher
        .fetch_http_partial_inner_ureq(
          FetchContextKind::Image,
          FetchContextKind::Image.into(),
          &url,
          8,
          None,
          None,
          ReferrerPolicy::default(),
          FetchCredentialsMode::Include,
          &deadline,
          Instant::now(),
        )
        .expect("ureq image prefix should substitute placeholder");
      handle.join().unwrap();
      assert_eq!(res.bytes, OFFLINE_FIXTURE_PLACEHOLDER_PNG[..8]);
      assert_eq!(
        res.content_type.as_deref(),
        Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME)
      );
    }

    {
      let Some(listener) =
        try_bind_localhost("http_html_image_payload_substitutes_placeholder_reqwest")
      else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      let response = headers.clone();
      let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let _ = read_http_request(&mut stream);
        stream.write_all(response.as_bytes()).unwrap();
      });

      let url = format!("http://{addr}/pixel.html");
      let res = fetcher
        .fetch_http_with_accept_inner_reqwest(
          FetchContextKind::Image,
          FetchContextKind::Image.into(),
          &url,
          None,
          None,
          None,
          None,
          ReferrerPolicy::default(),
          FetchCredentialsMode::Include,
          &deadline,
          Instant::now(),
          false,
        )
        .expect("reqwest image fetch should substitute placeholder");
      handle.join().unwrap();
      assert_eq!(res.bytes, OFFLINE_FIXTURE_PLACEHOLDER_PNG);
      assert_eq!(
        res.content_type.as_deref(),
        Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME)
      );
    }

    {
      let Some(listener) =
        try_bind_localhost("http_html_image_payload_substitutes_placeholder_reqwest_partial")
      else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      let response = headers.clone();
      let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let _ = read_http_request(&mut stream);
        stream.write_all(response.as_bytes()).unwrap();
      });

      let url = format!("http://{addr}/pixel.html");
      let res = fetcher
        .fetch_http_partial_inner_reqwest(
          FetchContextKind::Image,
          FetchContextKind::Image.into(),
          &url,
          8,
          None,
          None,
          ReferrerPolicy::default(),
          FetchCredentialsMode::Include,
          &deadline,
          Instant::now(),
        )
        .expect("reqwest image prefix should substitute placeholder");
      handle.join().unwrap();
      assert_eq!(res.bytes, OFFLINE_FIXTURE_PLACEHOLDER_PNG[..8]);
      assert_eq!(
        res.content_type.as_deref(),
        Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME)
      );
    }
  }

  #[test]
  fn http_html_image_payload_for_jpg_does_not_substitute_placeholder() {
    let body = "<!DOCTYPE html><html><title>blocked</title></html>";
    let headers = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
      body.len(),
      body
    );

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;

    {
      let Some(listener) =
        try_bind_localhost("http_html_image_payload_for_jpg_does_not_substitute_placeholder_ureq")
      else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      let response = headers.clone();
      let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let _ = read_http_request(&mut stream);
        stream.write_all(response.as_bytes()).unwrap();
      });

      let url = format!("http://{addr}/photo.jpg");
      let res = fetcher
        .fetch_http_with_accept_inner_ureq(
          FetchContextKind::Image,
          FetchContextKind::Image.into(),
          &url,
          None,
          None,
          None,
          None,
          ReferrerPolicy::default(),
          FetchCredentialsMode::Include,
          &deadline,
          Instant::now(),
          false,
        )
        .expect("ureq fetch should succeed");
      handle.join().unwrap();
      assert_eq!(res.bytes, body.as_bytes());
      assert_eq!(res.content_type.as_deref(), Some("text/html"));
    }

    {
      let Some(listener) = try_bind_localhost(
        "http_html_image_payload_for_jpg_does_not_substitute_placeholder_ureq_partial",
      ) else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      let response = headers.clone();
      let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let _ = read_http_request(&mut stream);
        stream.write_all(response.as_bytes()).unwrap();
      });

      let url = format!("http://{addr}/photo.jpg");
      let res = fetcher
        .fetch_http_partial_inner_ureq(
          FetchContextKind::Image,
          FetchContextKind::Image.into(),
          &url,
          8,
          None,
          None,
          ReferrerPolicy::default(),
          FetchCredentialsMode::Include,
          &deadline,
          Instant::now(),
        )
        .expect("ureq prefix should succeed");
      handle.join().unwrap();
      assert_eq!(res.bytes, body.as_bytes()[..8]);
      assert_eq!(res.content_type.as_deref(), Some("text/html"));
    }

    {
      let Some(listener) = try_bind_localhost(
        "http_html_image_payload_for_jpg_does_not_substitute_placeholder_reqwest",
      ) else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      let response = headers.clone();
      let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let _ = read_http_request(&mut stream);
        stream.write_all(response.as_bytes()).unwrap();
      });

      let url = format!("http://{addr}/photo.jpg");
      let res = fetcher
        .fetch_http_with_accept_inner_reqwest(
          FetchContextKind::Image,
          FetchContextKind::Image.into(),
          &url,
          None,
          None,
          None,
          None,
          ReferrerPolicy::default(),
          FetchCredentialsMode::Include,
          &deadline,
          Instant::now(),
          false,
        )
        .expect("reqwest fetch should succeed");
      handle.join().unwrap();
      assert_eq!(res.bytes, body.as_bytes());
      assert_eq!(res.content_type.as_deref(), Some("text/html"));
    }

    {
      let Some(listener) = try_bind_localhost(
        "http_html_image_payload_for_jpg_does_not_substitute_placeholder_reqwest_partial",
      ) else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      let response = headers.clone();
      let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let _ = read_http_request(&mut stream);
        stream.write_all(response.as_bytes()).unwrap();
      });

      let url = format!("http://{addr}/photo.jpg");
      let res = fetcher
        .fetch_http_partial_inner_reqwest(
          FetchContextKind::Image,
          FetchContextKind::Image.into(),
          &url,
          8,
          None,
          None,
          ReferrerPolicy::default(),
          FetchCredentialsMode::Include,
          &deadline,
          Instant::now(),
        )
        .expect("reqwest prefix should succeed");
      handle.join().unwrap();
      assert_eq!(res.bytes, body.as_bytes()[..8]);
      assert_eq!(res.content_type.as_deref(), Some("text/html"));
    }
  }

  #[test]
  fn http_redirect_to_html_for_jpg_does_not_substitute_placeholder() {
    let body = "<!DOCTYPE html><html><title>blocked</title></html>";
    let redirect_headers = "HTTP/1.1 302 Found\r\nLocation: /blocked.html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
    let final_headers = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
      body.len(),
      body
    );

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;

    {
      let Some(listener) =
        try_bind_localhost("http_redirect_to_html_for_jpg_does_not_substitute_placeholder_ureq")
      else {
        return;
      };
      listener.set_nonblocking(true).unwrap();
      let addr = listener.local_addr().unwrap();
      let redirect_response = redirect_headers.clone();
      let final_response = final_headers.clone();
      let handle = thread::spawn(move || {
        let start = Instant::now();
        let mut served_redirect = false;
        let mut served_final = false;
        while start.elapsed() < Duration::from_secs(2) && !served_final {
          match listener.accept() {
            Ok((mut stream, _)) => {
              stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
              for _ in 0..2 {
                let req = match read_http_request(&mut stream) {
                  Ok(Some(req)) => req,
                  Ok(None) => break,
                  Err(err) => panic!("read http_redirect_to_html_for_jpg_does_not_substitute_placeholder_ureq: {err}"),
                };
                let req = String::from_utf8_lossy(&req);
                if !served_redirect {
                  assert!(
                    req.contains("GET /photo.jpg"),
                    "expected /photo.jpg request, got: {req}"
                  );
                  stream.write_all(redirect_response.as_bytes()).unwrap();
                  served_redirect = true;
                } else {
                  assert!(
                    req.contains("GET /blocked.html"),
                    "expected /blocked.html request, got: {req}"
                  );
                  stream.write_all(final_response.as_bytes()).unwrap();
                  served_final = true;
                  break;
                }
              }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
              thread::sleep(Duration::from_millis(5));
            }
            Err(err) => panic!(
              "accept http_redirect_to_html_for_jpg_does_not_substitute_placeholder_ureq: {err}"
            ),
          }
        }
        assert!(
          served_redirect && served_final,
          "expected redirect+final requests (redirect={served_redirect}, final={served_final})"
        );
      });

      let url = format!("http://{addr}/photo.jpg");
      let res = fetcher
        .fetch_http_with_accept_inner_ureq(
          FetchContextKind::Image,
          FetchContextKind::Image.into(),
          &url,
          None,
          None,
          None,
          None,
          ReferrerPolicy::default(),
          FetchCredentialsMode::Include,
          &deadline,
          Instant::now(),
          false,
        )
        .expect("ureq fetch should succeed");
      handle.join().unwrap();
      assert_eq!(res.bytes, body.as_bytes());
      assert_eq!(res.content_type.as_deref(), Some("text/html"));
    }

    {
      let Some(listener) =
        try_bind_localhost("http_redirect_to_html_for_jpg_does_not_substitute_placeholder_reqwest")
      else {
        return;
      };
      listener.set_nonblocking(true).unwrap();
      let addr = listener.local_addr().unwrap();
      let redirect_response = redirect_headers.clone();
      let final_response = final_headers.clone();
      let handle = thread::spawn(move || {
        let start = Instant::now();
        let mut served_redirect = false;
        let mut served_final = false;
        while start.elapsed() < Duration::from_secs(2) && !served_final {
          match listener.accept() {
            Ok((mut stream, _)) => {
              stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
              for _ in 0..2 {
                let req = match read_http_request(&mut stream) {
                  Ok(Some(req)) => req,
                  Ok(None) => break,
                  Err(err) => panic!("read http_redirect_to_html_for_jpg_does_not_substitute_placeholder_reqwest: {err}"),
                };
                let req = String::from_utf8_lossy(&req);
                if !served_redirect {
                  assert!(
                    req.contains("GET /photo.jpg"),
                    "expected /photo.jpg request, got: {req}"
                  );
                  stream.write_all(redirect_response.as_bytes()).unwrap();
                  served_redirect = true;
                } else {
                  assert!(
                    req.contains("GET /blocked.html"),
                    "expected /blocked.html request, got: {req}"
                  );
                  stream.write_all(final_response.as_bytes()).unwrap();
                  served_final = true;
                  break;
                }
              }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
              thread::sleep(Duration::from_millis(5));
            }
            Err(err) => panic!(
              "accept http_redirect_to_html_for_jpg_does_not_substitute_placeholder_reqwest: {err}"
            ),
          }
        }
        assert!(
          served_redirect && served_final,
          "expected redirect+final requests (redirect={served_redirect}, final={served_final})"
        );
      });

      let url = format!("http://{addr}/photo.jpg");
      let res = fetcher
        .fetch_http_with_accept_inner_reqwest(
          FetchContextKind::Image,
          FetchContextKind::Image.into(),
          &url,
          None,
          None,
          None,
          None,
          ReferrerPolicy::default(),
          FetchCredentialsMode::Include,
          &deadline,
          Instant::now(),
          false,
        )
        .expect("reqwest fetch should succeed");
      handle.join().unwrap();
      assert_eq!(res.bytes, body.as_bytes());
      assert_eq!(res.content_type.as_deref(), Some("text/html"));
    }
  }

  #[test]
  fn http_redirect_to_html_for_jpg_with_bad_gzip_does_not_substitute_placeholder() {
    let body = "<!DOCTYPE html><html><title>blocked</title></html>";
    let redirect_headers =
      "HTTP/1.1 302 Found\r\nLocation: /blocked.html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        .to_string();
    let good_headers = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
      body.len(),
      body
    );

    let bad_gzip_body = b"not a gzip stream";
    let bad_gzip_headers = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
      bad_gzip_body.len()
    );

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;

    {
      let Some(listener) = try_bind_localhost(
        "http_redirect_to_html_for_jpg_with_bad_gzip_does_not_substitute_placeholder_ureq",
      ) else {
        return;
      };
      listener.set_nonblocking(true).unwrap();
      let addr = listener.local_addr().unwrap();
      let redirect_response = redirect_headers.clone();
      let good_response = good_headers.clone();
      let bad_response_headers = bad_gzip_headers.clone();
      let bad_response_body = bad_gzip_body.to_vec();
      let handle = thread::spawn(move || {
        let start = Instant::now();
        let mut served_bad = false;
        let mut served_good = false;
        while start.elapsed() < Duration::from_secs(2) && !(served_bad && served_good) {
          match listener.accept() {
            Ok((mut stream, _)) => {
              stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
              loop {
                let req = match read_http_request(&mut stream) {
                  Ok(Some(req)) => req,
                  Ok(None) => break,
                  Err(err) => panic!(
                    "read http_redirect_to_html_for_jpg_with_bad_gzip_does_not_substitute_placeholder_ureq: {err}"
                  ),
                };
                let req = String::from_utf8_lossy(&req);
                if req.contains("GET /photo.jpg") {
                  stream.write_all(redirect_response.as_bytes()).unwrap();
                } else if req.contains("GET /blocked.html") {
                  let lower = req.to_ascii_lowercase();
                  if lower.contains("accept-encoding: identity") {
                    served_good = true;
                    stream.write_all(good_response.as_bytes()).unwrap();
                  } else {
                    served_bad = true;
                    stream
                      .write_all(bad_response_headers.as_bytes())
                      .unwrap();
                    stream.write_all(&bad_response_body).unwrap();
                  }
                } else {
                  panic!(
                    "unexpected request for http_redirect_to_html_for_jpg_with_bad_gzip_does_not_substitute_placeholder_ureq: {req}"
                  );
                }
              }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
              thread::sleep(Duration::from_millis(5));
            }
            Err(err) => panic!(
              "accept http_redirect_to_html_for_jpg_with_bad_gzip_does_not_substitute_placeholder_ureq: {err}"
            ),
          }
        }
        assert!(
          served_bad && served_good,
          "expected invalid-gzip then identity retry (bad={served_bad}, good={served_good})"
        );
      });

      let url = format!("http://{addr}/photo.jpg");
      let res = fetcher.fetch_http_with_accept_inner_ureq(
        FetchContextKind::Image,
        FetchContextKind::Image.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        Instant::now(),
        false,
      );
      handle.join().unwrap();
      let res = res.expect("ureq fetch should succeed");
      assert_eq!(res.bytes, body.as_bytes());
      assert_eq!(res.content_type.as_deref(), Some("text/html"));
    }

    {
      let Some(listener) = try_bind_localhost(
        "http_redirect_to_html_for_jpg_with_bad_gzip_does_not_substitute_placeholder_reqwest",
      ) else {
        return;
      };
      listener.set_nonblocking(true).unwrap();
      let addr = listener.local_addr().unwrap();
      let redirect_response = redirect_headers.clone();
      let good_response = good_headers.clone();
      let bad_response_headers = bad_gzip_headers.clone();
      let bad_response_body = bad_gzip_body.to_vec();
      let handle = thread::spawn(move || {
        let start = Instant::now();
        let mut served_bad = false;
        let mut served_good = false;
        while start.elapsed() < Duration::from_secs(2) && !(served_bad && served_good) {
          match listener.accept() {
            Ok((mut stream, _)) => {
              stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
              loop {
                let req = match read_http_request(&mut stream) {
                  Ok(Some(req)) => req,
                  Ok(None) => break,
                  Err(err) => panic!(
                    "read http_redirect_to_html_for_jpg_with_bad_gzip_does_not_substitute_placeholder_reqwest: {err}"
                  ),
                };
                let req = String::from_utf8_lossy(&req);
                if req.contains("GET /photo.jpg") {
                  stream.write_all(redirect_response.as_bytes()).unwrap();
                } else if req.contains("GET /blocked.html") {
                  let lower = req.to_ascii_lowercase();
                  if lower.contains("accept-encoding: identity") {
                    served_good = true;
                    stream.write_all(good_response.as_bytes()).unwrap();
                  } else {
                    served_bad = true;
                    stream
                      .write_all(bad_response_headers.as_bytes())
                      .unwrap();
                    stream.write_all(&bad_response_body).unwrap();
                  }
                } else {
                  panic!(
                    "unexpected request for http_redirect_to_html_for_jpg_with_bad_gzip_does_not_substitute_placeholder_reqwest: {req}"
                  );
                }
              }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
              thread::sleep(Duration::from_millis(5));
            }
            Err(err) => panic!(
              "accept http_redirect_to_html_for_jpg_with_bad_gzip_does_not_substitute_placeholder_reqwest: {err}"
            ),
          }
        }
        assert!(
          served_bad && served_good,
          "expected invalid-gzip then identity retry (bad={served_bad}, good={served_good})"
        );
      });

      let url = format!("http://{addr}/photo.jpg");
      let res = fetcher.fetch_http_with_accept_inner_reqwest(
        FetchContextKind::Image,
        FetchContextKind::Image.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        Instant::now(),
        false,
      );
      handle.join().unwrap();
      let res = res.expect("reqwest fetch should succeed");
      assert_eq!(res.bytes, body.as_bytes());
      assert_eq!(res.content_type.as_deref(), Some("text/html"));
    }
  }

  #[test]
  fn http_empty_font_404_is_ok_ureq() {
    let Some(listener) = try_bind_localhost("http_empty_font_404_is_ok_ureq") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let headers =
        "HTTP/1.1 404 Not Found\r\nContent-Type: font/woff2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(headers.as_bytes()).unwrap();
    });

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;
    let started = Instant::now();
    let url = format!("http://{addr}/missing.woff2");
    let res = fetcher
      .fetch_http_with_accept_inner_ureq(
        FetchContextKind::Font,
        FetchContextKind::Font.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        started,
        false,
      )
      .expect("font fetch should succeed");
    handle.join().unwrap();

    assert!(
      res.bytes.is_empty(),
      "expected 404 font with empty body to be accepted"
    );
    assert_eq!(res.status, Some(404));
  }

  #[test]
  fn http_empty_font_404_is_ok_reqwest() {
    let Some(listener) = try_bind_localhost("http_empty_font_404_is_ok_reqwest") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let headers =
        "HTTP/1.1 404 Not Found\r\nContent-Type: font/woff2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(headers.as_bytes()).unwrap();
    });

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;
    let started = Instant::now();
    let url = format!("http://{addr}/missing.woff2");
    let res = fetcher
      .fetch_http_with_accept_inner_reqwest(
        FetchContextKind::Font,
        FetchContextKind::Font.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        started,
        false,
      )
      .expect("reqwest font fetch should succeed");
    handle.join().unwrap();

    assert!(
      res.bytes.is_empty(),
      "expected 404 font with empty body to be accepted"
    );
    assert_eq!(res.status, Some(404));
  }

  #[test]
  fn http_empty_body_for_error_status_is_allowed() {
    let Some(listener) = try_bind_localhost("http_empty_body_for_error_status_is_allowed") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      for _ in 0..2 {
        let (mut stream, _) = listener.accept().unwrap();
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();
        let _ = read_http_request(&mut stream);
        // Some servers legitimately return an empty body for 404/403 responses. We want the
        // fetcher to propagate the HTTP status so callers can report `HTTP status <code>` rather
        // than surfacing a misleading "empty HTTP response body" error.
        let response =
          "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n";
        stream.write_all(response.as_bytes()).unwrap();
      }
    });

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;
    let url = format!("http://{addr}/missing.woff2");

    let res_ureq = fetcher
      .fetch_http_with_accept_inner_ureq(
        FetchContextKind::Font,
        FetchContextKind::Font.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        Instant::now(),
        false,
      )
      .expect("ureq should return response for empty 404");
    assert_eq!(res_ureq.status, Some(404));
    assert!(
      res_ureq.bytes.is_empty(),
      "expected empty 404 body to be returned"
    );
    let err = ensure_http_success(&res_ureq, &url).expect_err("404 should be an HTTP failure");
    assert!(
      err.to_string().contains("HTTP status 404"),
      "unexpected error message: {err}"
    );

    let res_reqwest = fetcher
      .fetch_http_with_accept_inner_reqwest(
        FetchContextKind::Font,
        FetchContextKind::Font.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        Instant::now(),
        false,
      )
      .expect("reqwest should return response for empty 404");
    assert_eq!(res_reqwest.status, Some(404));
    assert!(
      res_reqwest.bytes.is_empty(),
      "expected empty 404 body to be returned"
    );
    let err = ensure_http_success(&res_reqwest, &url).expect_err("404 should be an HTTP failure");
    assert!(
      err.to_string().contains("HTTP status 404"),
      "unexpected error message: {err}"
    );

    handle.join().unwrap();
  }

  #[test]
  fn http_akamai_pixel_empty_body_without_content_length_substitutes_placeholder() {
    let Some(listener) = try_bind_localhost(
      "http_akamai_pixel_empty_body_without_content_length_substitutes_placeholder",
    ) else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let headers = "HTTP/1.1 200 OK\r\nContent-Type: image/gif\r\nConnection: close\r\n\r\n";
      stream.write_all(headers.as_bytes()).unwrap();
    });

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;
    let started = Instant::now();
    let url = format!("http://{addr}/aKaM/13/pIxEl_deadbeef?a=1");
    let res = fetcher
      .fetch_http_with_accept_inner_ureq(
        FetchContextKind::Image,
        FetchContextKind::Image.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        started,
        false,
      )
      .expect("pixel fetch should succeed");
    handle.join().unwrap();

    assert_eq!(res.status, Some(200));
    assert_eq!(res.bytes, OFFLINE_FIXTURE_PLACEHOLDER_PNG);
    assert_eq!(
      res.content_type.as_deref(),
      Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME)
    );
  }

  #[test]
  fn http_empty_image_without_content_length_is_error() {
    let Some(listener) = try_bind_localhost("http_empty_image_without_content_length_is_error")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let headers = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n";
      stream.write_all(headers.as_bytes()).unwrap();
    });

    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(HttpRetryPolicy {
        max_attempts: 1,
        ..HttpRetryPolicy::default()
      });
    let deadline = None;
    let started = Instant::now();
    let url = format!("http://{addr}/empty.png");
    let err = fetcher
      .fetch_http_with_accept_inner_ureq(
        FetchContextKind::Image,
        FetchContextKind::Image.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        started,
        false,
      )
      .expect_err("image fetch should reject unexpected empty body");
    handle.join().unwrap();
    assert!(
      err.to_string().contains("empty HTTP response body"),
      "unexpected error message: {err}"
    );
  }

  #[test]
  fn http_empty_stylesheet_with_content_length_zero_is_ok_reqwest() {
    let Some(listener) =
      try_bind_localhost("http_empty_stylesheet_with_content_length_zero_is_ok_reqwest")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream);
      let headers =
        "HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(headers.as_bytes()).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let deadline = None;
    let started = Instant::now();
    let url = format!("http://{addr}/empty.css");
    let res = fetcher
      .fetch_http_with_accept_inner_reqwest(
        FetchContextKind::Stylesheet,
        FetchContextKind::Stylesheet.into(),
        &url,
        None,
        None,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        started,
        false,
      )
      .expect("reqwest stylesheet fetch should succeed");
    handle.join().unwrap();

    assert!(
      res.bytes.is_empty(),
      "expected empty stylesheet body to be accepted"
    );
    assert_eq!(res.status, Some(200));
    assert_eq!(res.content_type.as_deref(), Some("text/css"));
  }

  #[test]
  fn http_www_fallback_url_prefixes_www_for_dns_failures() {
    assert_eq!(
      http_www_fallback_url("https://nhk.or.jp"),
      Some("https://www.nhk.or.jp/".to_string())
    );
    assert_eq!(
      http_www_fallback_url("http://example.com/path?x=1"),
      Some("http://www.example.com/path?x=1".to_string())
    );
  }

  #[test]
  fn http_www_fallback_url_ignores_www_and_single_label_hosts() {
    assert_eq!(http_www_fallback_url("https://www.example.com"), None);
    assert_eq!(http_www_fallback_url("https://localhost"), None);
    assert_eq!(http_www_fallback_url("https://127.0.0.1:1234"), None);
  }

  #[test]
  fn error_looks_like_dns_failure_matches_common_phrases() {
    let err = Error::Resource(ResourceError::new(
      "https://nhk.or.jp",
      "curl failed (exit code 6): Could not resolve host: nhk.or.jp",
    ));
    assert!(error_looks_like_dns_failure(&err));

    let err = Error::Resource(ResourceError::new(
      "https://example.com",
      "TLS handshake failed",
    ));
    assert!(!error_looks_like_dns_failure(&err));
  }

  #[test]
  fn auto_backend_ureq_timeout_slice_is_half_budget_capped() {
    assert_eq!(
      auto_backend_ureq_timeout_slice(Duration::from_secs(30)),
      Duration::from_secs(5)
    );
    assert_eq!(
      auto_backend_ureq_timeout_slice(Duration::from_secs(8)),
      Duration::from_secs(4)
    );
    assert_eq!(
      auto_backend_ureq_timeout_slice(Duration::from_secs(3)),
      Duration::from_millis(1500)
    );
    assert_eq!(
      auto_backend_ureq_timeout_slice(Duration::ZERO),
      Duration::ZERO
    );
  }

  #[test]
  fn rewrite_known_pageset_url_examples() {
    assert_eq!(
      rewrite_known_pageset_url("https://tesco.com").as_deref(),
      Some("https://www.tesco.com/")
    );
    assert_eq!(
      rewrite_known_pageset_url("https://nhk.or.jp").as_deref(),
      Some("https://www.nhk.or.jp/")
    );
    assert_eq!(
      rewrite_known_pageset_url(
        "https://developer.mozilla.org/en-US/docs/Web/CSS/CSS_multicol_layout/Using_multi-column_layouts"
      )
      .as_deref(),
      Some("https://developer.mozilla.org/en-US/docs/Web/CSS/Guides/Multicol_layout/Using")
    );
    assert_eq!(
      rewrite_known_pageset_url(
        "https://developer.mozilla.org/en-US/docs/Web/CSS/text-orientation"
      ),
      None
    );
    assert_eq!(rewrite_known_pageset_url("https://example.com"), None);
    assert_eq!(rewrite_known_pageset_url("http://tesco.com"), None);
  }

  static RESOURCE_CACHE_DIAGNOSTICS_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

  struct ResourceCacheDiagnosticsTestLock {
    _lock: std::sync::MutexGuard<'static, ()>,
    _diagnostics_session: crate::api::DiagnosticsSessionGuard,
  }

  fn resource_cache_diagnostics_test_lock() -> ResourceCacheDiagnosticsTestLock {
    let _diagnostics_session = crate::api::DiagnosticsSessionGuard::acquire();
    let _lock = RESOURCE_CACHE_DIAGNOSTICS_TEST_LOCK
      .get_or_init(|| Mutex::new(()))
      .lock()
      .unwrap();
    ResourceCacheDiagnosticsTestLock {
      _lock,
      _diagnostics_session,
    }
  }

  struct ResourceCacheDiagnosticsGuard;

  impl ResourceCacheDiagnosticsGuard {
    fn new() -> Self {
      enable_resource_cache_diagnostics();
      Self
    }
  }

  impl Drop for ResourceCacheDiagnosticsGuard {
    fn drop(&mut self) {
      let _ = take_resource_cache_diagnostics();
    }
  }

  #[test]
  fn http_browser_request_profile_for_url_examples() {
    let doc = http_browser_request_profile_for_url("https://tesco.com");
    assert_eq!(doc, FetchDestination::Document);
    assert_eq!(doc.accept(), DEFAULT_ACCEPT);
    assert_eq!(doc.sec_fetch_dest(), "document");
    assert_eq!(doc.sec_fetch_mode(), "navigate");
    assert_eq!(doc.sec_fetch_site(), "none");
    assert_eq!(doc.sec_fetch_user(), Some("?1"));
    assert_eq!(doc.upgrade_insecure_requests(), Some("1"));

    let font =
      http_browser_request_profile_for_url("https://www.tesco.com/fonts/TESCOModern-Regular.woff2");
    assert_eq!(font, FetchDestination::Font);
    assert_eq!(font.accept(), BROWSER_ACCEPT_ALL);
    assert_eq!(font.sec_fetch_dest(), "font");
    assert_eq!(font.sec_fetch_mode(), "cors");
    assert_eq!(font.sec_fetch_site(), "same-origin");
    assert_eq!(font.sec_fetch_user(), None);
    assert_eq!(font.upgrade_insecure_requests(), None);

    let stylesheet = http_browser_request_profile_for_url("https://example.com/styles/site.css");
    assert_eq!(stylesheet, FetchDestination::Style);
    assert_eq!(stylesheet.accept(), BROWSER_ACCEPT_STYLESHEET);
    assert_eq!(stylesheet.sec_fetch_dest(), "style");
    assert_eq!(stylesheet.sec_fetch_mode(), "no-cors");
    assert_eq!(stylesheet.sec_fetch_site(), "same-origin");
    assert_eq!(stylesheet.sec_fetch_user(), None);
    assert_eq!(stylesheet.upgrade_insecure_requests(), None);

    let image = http_browser_request_profile_for_url("https://example.com/images/logo.png");
    assert_eq!(image, FetchDestination::Image);
    assert_eq!(image.accept(), BROWSER_ACCEPT_IMAGE);
    assert_eq!(image.sec_fetch_dest(), "image");
    assert_eq!(image.sec_fetch_mode(), "no-cors");
    assert_eq!(image.sec_fetch_site(), "same-origin");
    assert_eq!(image.sec_fetch_user(), None);
    assert_eq!(image.upgrade_insecure_requests(), None);
  }

  #[test]
  fn resource_cache_diagnostics_aggregate_across_threads() {
    let _lock = resource_cache_diagnostics_test_lock();
    let _guard = ResourceCacheDiagnosticsGuard::new();

    let threads = 4usize;
    let fresh_per_thread = 5000usize;
    let revalidated_per_thread = 3000usize;
    let misses_per_thread = 2000usize;
    let barrier = Arc::new(Barrier::new(threads + 1));
    let mut handles = Vec::new();
    for _ in 0..threads {
      let barrier = Arc::clone(&barrier);
      handles.push(thread::spawn(move || {
        let _diag_guard = ResourceCacheDiagnosticsThreadGuard::enter();
        barrier.wait();
        for _ in 0..fresh_per_thread {
          record_cache_fresh_hit();
        }
        for _ in 0..revalidated_per_thread {
          record_cache_revalidated_hit();
        }
        for _ in 0..misses_per_thread {
          record_cache_miss();
        }
      }));
    }
    barrier.wait();

    for handle in handles {
      handle.join().unwrap();
    }

    let stats = take_resource_cache_diagnostics().expect("diagnostics should be enabled");
    assert!(
      stats.fresh_hits >= threads * fresh_per_thread,
      "expected fresh hit counters to aggregate across threads"
    );
    assert!(
      stats.revalidated_hits >= threads * revalidated_per_thread,
      "expected revalidated hit counters to aggregate across threads"
    );
    assert!(
      stats.misses >= threads * misses_per_thread,
      "expected miss counters to aggregate across threads"
    );
  }

  #[test]
  fn resource_cache_diagnostics_record_network_fetches() {
    let _lock = resource_cache_diagnostics_test_lock();
    let Some(listener) = try_bind_localhost("resource_cache_diagnostics_record_network_fetches")
    else {
      return;
    };

    let addr = listener.local_addr().unwrap();
    let _guard = ResourceCacheDiagnosticsGuard::new();
    let requests = 5usize;
    let handle = thread::spawn(move || {
      for _ in 0..requests {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let body = b"ok";
        let headers = format!(
          "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
          body.len()
        );
        stream.write_all(headers.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
      }
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}/", addr);
    for _ in 0..requests {
      let res = fetcher.fetch(&url).expect("fetch should succeed");
      assert_eq!(res.bytes, b"ok");
    }
    handle.join().unwrap();

    let stats = take_resource_cache_diagnostics().expect("diagnostics should be enabled");
    assert!(
      stats.network_fetches >= requests,
      "expected network fetch counter to increment for each request (got {})",
      stats.network_fetches
    );
    assert!(
      stats.network_fetch_ms.is_finite() && stats.network_fetch_ms >= 0.0,
      "expected finite network fetch duration, got {}",
      stats.network_fetch_ms
    );
    assert!(
      stats.network_fetch_bytes >= requests * 2,
      "expected network fetch byte counter to include response bodies (got {})",
      stats.network_fetch_bytes
    );
  }

  #[test]
  fn caching_fetcher_skips_unhandled_vary_responses() {
    if matches!(http_backend_mode(), HttpBackendMode::Curl) && !curl_backend::curl_available() {
      eprintln!(
        "skipping caching_fetcher_skips_unhandled_vary_responses: curl backend selected but curl is unavailable"
      );
      return;
    }

    let Some(listener) = try_bind_localhost("caching_fetcher_skips_unhandled_vary_responses")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let request_count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&request_count);

    let handle = thread::spawn(move || {
      let start = Instant::now();
      let mut last_request = None::<Instant>;
      while start.elapsed() < Duration::from_secs(2) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
            last_request = Some(Instant::now());

            stream
              .set_read_timeout(Some(Duration::from_millis(500)))
              .unwrap();
            let _ = read_http_request(&mut stream);

            let body = n.to_string();
            let headers = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nVary: Sec-Fetch-Site\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.write_all(body.as_bytes());

            if n >= 2 {
              return;
            }
          }
          Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
            if let Some(last) = last_request {
              if last.elapsed() > Duration::from_millis(500) {
                return;
              }
            }
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept caching_fetcher_skips_unhandled_vary_responses: {err}"),
        }
      }
    });

    let fetcher = CachingFetcher::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
    let url = format!("http://{addr}/vary.txt");
    let first = fetcher.fetch(&url).expect("first fetch");
    assert_eq!(first.vary.as_deref(), Some("sec-fetch-site"));
    let second = fetcher.fetch(&url).expect("second fetch");
    assert_eq!(second.vary.as_deref(), Some("sec-fetch-site"));

    handle.join().unwrap();

    assert_eq!(
      request_count.load(Ordering::SeqCst),
      2,
      "expected unhandled Vary response to bypass caching"
    );
    assert_ne!(
      first.bytes, second.bytes,
      "expected network body to differ across requests"
    );
  }

  #[test]
  fn caching_fetcher_allows_vary_origin() {
    if matches!(http_backend_mode(), HttpBackendMode::Curl) && !curl_backend::curl_available() {
      eprintln!(
        "skipping caching_fetcher_allows_vary_origin: curl backend selected but curl is unavailable"
      );
      return;
    }

    let Some(listener) = try_bind_localhost("caching_fetcher_allows_vary_origin") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let request_count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&request_count);

    let handle = thread::spawn(move || {
      let start = Instant::now();
      let mut last_request = None::<Instant>;
      while start.elapsed() < Duration::from_secs(2) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
            last_request = Some(Instant::now());

            stream
              .set_read_timeout(Some(Duration::from_millis(500)))
              .unwrap();
            let _ = read_http_request(&mut stream);

            let body = n.to_string();
            let headers = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nVary: Origin\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.write_all(body.as_bytes());

            if n >= 2 {
              return;
            }
          }
          Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
            if let Some(last) = last_request {
              if last.elapsed() > Duration::from_millis(500) {
                return;
              }
            }
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept caching_fetcher_allows_vary_origin: {err}"),
        }
      }
    });

    let fetcher = CachingFetcher::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
    let url = format!("http://{addr}/vary_origin.txt");
    let first = fetcher.fetch(&url).expect("first fetch");
    assert_eq!(first.vary.as_deref(), Some("origin"));
    let second = fetcher.fetch(&url).expect("second fetch");
    assert_eq!(second.vary.as_deref(), Some("origin"));

    handle.join().unwrap();

    assert_eq!(
      request_count.load(Ordering::SeqCst),
      1,
      "expected Vary: Origin response to be cached"
    );
    assert_eq!(first.bytes, second.bytes, "expected cached body to match");
  }

  #[test]
  fn caching_fetcher_allows_vary_accept() {
    if matches!(http_backend_mode(), HttpBackendMode::Curl) && !curl_backend::curl_available() {
      eprintln!(
        "skipping caching_fetcher_allows_vary_accept: curl backend selected but curl is unavailable"
      );
      return;
    }

    let Some(listener) = try_bind_localhost("caching_fetcher_allows_vary_accept") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let request_count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&request_count);

    let handle = thread::spawn(move || {
      let start = Instant::now();
      let mut last_request = None::<Instant>;
      while start.elapsed() < Duration::from_secs(2) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
            last_request = Some(Instant::now());

            stream
              .set_read_timeout(Some(Duration::from_millis(500)))
              .unwrap();
            let _ = read_http_request(&mut stream);

            let body = n.to_string();
            let headers = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nVary: Accept\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.write_all(body.as_bytes());

            if n >= 2 {
              return;
            }
          }
          Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
            if let Some(last) = last_request {
              if last.elapsed() > Duration::from_millis(500) {
                return;
              }
            }
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept caching_fetcher_allows_vary_accept: {err}"),
        }
      }
    });

    let fetcher = CachingFetcher::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
    let url = format!("http://{addr}/vary_accept.txt");
    let first = fetcher.fetch(&url).expect("first fetch");
    assert_eq!(first.vary.as_deref(), Some("accept"));
    let second = fetcher.fetch(&url).expect("second fetch");
    assert_eq!(second.vary.as_deref(), Some("accept"));

    handle.join().unwrap();

    assert_eq!(
      request_count.load(Ordering::SeqCst),
      1,
      "expected Vary: Accept response to be cached"
    );
    assert_eq!(first.bytes, second.bytes, "expected cached body to match");
  }

  #[test]
  fn caching_fetcher_allows_vary_sec_fetch_mode() {
    if matches!(http_backend_mode(), HttpBackendMode::Curl) && !curl_backend::curl_available() {
      eprintln!(
        "skipping caching_fetcher_allows_vary_sec_fetch_mode: curl backend selected but curl is unavailable"
      );
      return;
    }

    let Some(listener) = try_bind_localhost("caching_fetcher_allows_vary_sec_fetch_mode") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let request_count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&request_count);

    let handle = thread::spawn(move || {
      let start = Instant::now();
      let mut last_request = None::<Instant>;
      while start.elapsed() < Duration::from_secs(2) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
            last_request = Some(Instant::now());

            stream
              .set_read_timeout(Some(Duration::from_millis(500)))
              .unwrap();
            let _ = read_http_request(&mut stream);

            let body = n.to_string();
            let headers = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nVary: Sec-Fetch-Mode\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.write_all(body.as_bytes());

            if n >= 2 {
              return;
            }
          }
          Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
            if let Some(last) = last_request {
              if last.elapsed() > Duration::from_millis(500) {
                return;
              }
            }
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept caching_fetcher_allows_vary_sec_fetch_mode: {err}"),
        }
      }
    });

    let fetcher = CachingFetcher::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
    let url = format!("http://{addr}/vary_sec_fetch_mode.txt");
    let first = fetcher.fetch(&url).expect("first fetch");
    assert_eq!(first.vary.as_deref(), Some("sec-fetch-mode"));
    let second = fetcher.fetch(&url).expect("second fetch");
    assert_eq!(second.vary.as_deref(), Some("sec-fetch-mode"));

    handle.join().unwrap();

    assert_eq!(
      request_count.load(Ordering::SeqCst),
      1,
      "expected Vary: Sec-Fetch-Mode response to be cached"
    );
    assert_eq!(first.bytes, second.bytes, "expected cached body to match");
  }

  #[test]
  fn caching_fetcher_skips_vary_origin_without_origin_partitioning() {
    if matches!(http_backend_mode(), HttpBackendMode::Curl) && !curl_backend::curl_available() {
      eprintln!(
        "skipping caching_fetcher_skips_vary_origin_without_origin_partitioning: curl backend selected but curl is unavailable"
      );
      return;
    }

    if !http_browser_headers_enabled() {
      eprintln!(
        "skipping caching_fetcher_skips_vary_origin_without_origin_partitioning: browser-like request headers are disabled"
      );
      return;
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
      "0".to_string(),
    )])));

    runtime::with_thread_runtime_toggles(toggles, || {
      let Some(listener) =
        try_bind_localhost("caching_fetcher_skips_vary_origin_without_origin_partitioning")
      else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      listener.set_nonblocking(true).unwrap();

      let request_count = Arc::new(AtomicUsize::new(0));
      let seen = Arc::clone(&request_count);

      let handle = thread::spawn(move || {
        let start = Instant::now();
        let mut last_request = None::<Instant>;
        while start.elapsed() < Duration::from_secs(2) {
          match listener.accept() {
            Ok((mut stream, _)) => {
              let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
              last_request = Some(Instant::now());

              stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
              let _ = read_http_request(&mut stream);

              let body = n.to_string();
              let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: font/woff2\r\nVary: Origin\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
              );
              let _ = stream.write_all(headers.as_bytes());
              let _ = stream.write_all(body.as_bytes());

              if n >= 2 {
                return;
              }
            }
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
              if let Some(last) = last_request {
                if last.elapsed() > Duration::from_millis(500) {
                  return;
                }
              }
              thread::sleep(Duration::from_millis(5));
            }
            Err(err) => {
              panic!("accept caching_fetcher_skips_vary_origin_without_origin_partitioning: {err}")
            }
          }
        }
      });

      let fetcher = CachingFetcher::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
      let url = format!("http://{addr}/font.woff2");
      let req_a =
        FetchRequest::new(&url, FetchDestination::Font).with_referrer_url("http://a.test/page");
      let req_b =
        FetchRequest::new(&url, FetchDestination::Font).with_referrer_url("http://b.test/page");

      let first = fetcher.fetch_with_request(req_a).expect("first fetch");
      assert_eq!(first.vary.as_deref(), Some("origin"));
      let second = fetcher.fetch_with_request(req_b).expect("second fetch");
      assert_eq!(second.vary.as_deref(), Some("origin"));

      handle.join().unwrap();

      assert_eq!(
        request_count.load(Ordering::SeqCst),
        2,
        "expected Vary: Origin response to bypass caching when requests are not origin-partitioned"
      );
      assert_ne!(
        first.bytes, second.bytes,
        "expected network body to differ across requests"
      );
    });
  }

  #[test]
  fn caching_fetcher_skips_vary_origin_without_origin_partitioning_for_stylesheet_cors() {
    if matches!(http_backend_mode(), HttpBackendMode::Curl) && !curl_backend::curl_available() {
      eprintln!(
        "skipping caching_fetcher_skips_vary_origin_without_origin_partitioning_for_stylesheet_cors: curl backend selected but curl is unavailable"
      );
      return;
    }

    if !http_browser_headers_enabled() {
      eprintln!(
        "skipping caching_fetcher_skips_vary_origin_without_origin_partitioning_for_stylesheet_cors: browser-like request headers are disabled"
      );
      return;
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
      "0".to_string(),
    )])));

    runtime::with_thread_runtime_toggles(toggles, || {
      let Some(listener) = try_bind_localhost(
        "caching_fetcher_skips_vary_origin_without_origin_partitioning_for_stylesheet_cors",
      ) else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      listener.set_nonblocking(true).unwrap();

      let request_count = Arc::new(AtomicUsize::new(0));
      let seen = Arc::clone(&request_count);

      let handle = thread::spawn(move || {
        let start = Instant::now();
        let mut last_request = None::<Instant>;
        while start.elapsed() < Duration::from_secs(2) {
          match listener.accept() {
            Ok((mut stream, _)) => {
              let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
              last_request = Some(Instant::now());

              stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
              let _ = read_http_request(&mut stream);

              let body = n.to_string();
              let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nVary: Origin\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
              );
              let _ = stream.write_all(headers.as_bytes());
              let _ = stream.write_all(body.as_bytes());

              if n >= 2 {
                return;
              }
            }
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
              if let Some(last) = last_request {
                if last.elapsed() > Duration::from_millis(500) {
                  return;
                }
              }
              thread::sleep(Duration::from_millis(5));
            }
            Err(err) => panic!(
              "accept caching_fetcher_skips_vary_origin_without_origin_partitioning_for_stylesheet_cors: {err}"
            ),
          }
        }
      });

      let fetcher = CachingFetcher::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
      let url = format!("http://{addr}/style.css");
      let req_a = FetchRequest::new(&url, FetchDestination::StyleCors)
        .with_referrer_url("http://a.test/page")
        .with_credentials_mode(FetchCredentialsMode::SameOrigin);
      let req_b = FetchRequest::new(&url, FetchDestination::StyleCors)
        .with_referrer_url("http://b.test/page")
        .with_credentials_mode(FetchCredentialsMode::SameOrigin);

      let first = fetcher.fetch_with_request(req_a).expect("first fetch");
      assert_eq!(first.vary.as_deref(), Some("origin"));
      let second = fetcher.fetch_with_request(req_b).expect("second fetch");
      assert_eq!(second.vary.as_deref(), Some("origin"));

      handle.join().unwrap();

      assert_eq!(
        request_count.load(Ordering::SeqCst),
        2,
        "expected Vary: Origin response to bypass caching when requests are not origin-partitioned"
      );
      assert_ne!(
        first.bytes, second.bytes,
        "expected network body to differ across requests"
      );
    });
  }

  #[test]
  fn caching_fetcher_partitions_vary_origin_when_origin_partitioned() {
    if matches!(http_backend_mode(), HttpBackendMode::Curl) && !curl_backend::curl_available() {
      eprintln!(
        "skipping caching_fetcher_partitions_vary_origin_when_origin_partitioned: curl backend selected but curl is unavailable"
      );
      return;
    }

    if !http_browser_headers_enabled() {
      eprintln!(
        "skipping caching_fetcher_partitions_vary_origin_when_origin_partitioned: browser-like request headers are disabled"
      );
      return;
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
      "1".to_string(),
    )])));

    runtime::with_thread_runtime_toggles(toggles, || {
      let Some(listener) =
        try_bind_localhost("caching_fetcher_partitions_vary_origin_when_origin_partitioned")
      else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      listener.set_nonblocking(true).unwrap();

      let request_count = Arc::new(AtomicUsize::new(0));
      let seen = Arc::clone(&request_count);

      let handle = thread::spawn(move || {
        let start = Instant::now();
        let mut last_request = None::<Instant>;
        while start.elapsed() < Duration::from_secs(2) {
          match listener.accept() {
            Ok((mut stream, _)) => {
              let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
              last_request = Some(Instant::now());

              stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
              let _ = read_http_request(&mut stream);

              let body = n.to_string();
              let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: font/woff2\r\nVary: Origin\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
              );
              let _ = stream.write_all(headers.as_bytes());
              let _ = stream.write_all(body.as_bytes());

              if n >= 2 {
                return;
              }
            }
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
              if let Some(last) = last_request {
                if last.elapsed() > Duration::from_millis(500) {
                  return;
                }
              }
              thread::sleep(Duration::from_millis(5));
            }
            Err(err) => {
              panic!("accept caching_fetcher_partitions_vary_origin_when_origin_partitioned: {err}")
            }
          }
        }
      });

      let fetcher = CachingFetcher::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
      let url = format!("http://{addr}/font.woff2");
      let req_a =
        FetchRequest::new(&url, FetchDestination::Font).with_referrer_url("http://a.test/page");
      let req_b =
        FetchRequest::new(&url, FetchDestination::Font).with_referrer_url("http://b.test/page");

      let first_a = fetcher.fetch_with_request(req_a).expect("first A fetch");
      assert_eq!(first_a.vary.as_deref(), Some("origin"));

      let second_a = fetcher.fetch_with_request(req_a).expect("second A fetch");
      assert_eq!(second_a.vary.as_deref(), Some("origin"));
      assert_eq!(second_a.bytes, first_a.bytes, "expected A to hit cache");

      let first_b = fetcher.fetch_with_request(req_b).expect("first B fetch");
      assert_eq!(first_b.vary.as_deref(), Some("origin"));

      handle.join().unwrap();

      assert_eq!(
        request_count.load(Ordering::SeqCst),
        2,
        "expected Vary: Origin responses to be cached per-origin when origin partitioning is enabled"
      );
      assert_ne!(
        first_a.bytes, first_b.bytes,
        "expected distinct cached variants per origin"
      );
    });
  }

  #[test]
  fn caching_fetcher_partitions_vary_origin_when_origin_partitioned_for_stylesheet_cors() {
    if matches!(http_backend_mode(), HttpBackendMode::Curl) && !curl_backend::curl_available() {
      eprintln!(
        "skipping caching_fetcher_partitions_vary_origin_when_origin_partitioned_for_stylesheet_cors: curl backend selected but curl is unavailable"
      );
      return;
    }

    if !http_browser_headers_enabled() {
      eprintln!(
        "skipping caching_fetcher_partitions_vary_origin_when_origin_partitioned_for_stylesheet_cors: browser-like request headers are disabled"
      );
      return;
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
      "1".to_string(),
    )])));

    runtime::with_thread_runtime_toggles(toggles, || {
      let Some(listener) = try_bind_localhost(
        "caching_fetcher_partitions_vary_origin_when_origin_partitioned_for_stylesheet_cors",
      ) else {
        return;
      };
      let addr = listener.local_addr().unwrap();
      listener.set_nonblocking(true).unwrap();

      let request_count = Arc::new(AtomicUsize::new(0));
      let seen = Arc::clone(&request_count);

      let handle = thread::spawn(move || {
        let start = Instant::now();
        let mut last_request = None::<Instant>;
        while start.elapsed() < Duration::from_secs(2) {
          match listener.accept() {
            Ok((mut stream, _)) => {
              let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
              last_request = Some(Instant::now());

              stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
              let _ = read_http_request(&mut stream);

              let body = n.to_string();
              let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nVary: Origin\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
              );
              let _ = stream.write_all(headers.as_bytes());
              let _ = stream.write_all(body.as_bytes());

              if n >= 2 {
                return;
              }
            }
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
              if let Some(last) = last_request {
                if last.elapsed() > Duration::from_millis(500) {
                  return;
                }
              }
              thread::sleep(Duration::from_millis(5));
            }
            Err(err) => panic!(
              "accept caching_fetcher_partitions_vary_origin_when_origin_partitioned_for_stylesheet_cors: {err}"
            ),
          }
        }
      });

      let fetcher = CachingFetcher::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
      let url = format!("http://{addr}/style.css");
      let req_a = FetchRequest::new(&url, FetchDestination::StyleCors)
        .with_referrer_url("http://a.test/page")
        .with_credentials_mode(FetchCredentialsMode::SameOrigin);
      let req_b = FetchRequest::new(&url, FetchDestination::StyleCors)
        .with_referrer_url("http://b.test/page")
        .with_credentials_mode(FetchCredentialsMode::SameOrigin);

      let first_a = fetcher.fetch_with_request(req_a).expect("first A fetch");
      assert_eq!(first_a.vary.as_deref(), Some("origin"));

      let second_a = fetcher.fetch_with_request(req_a).expect("second A fetch");
      assert_eq!(second_a.vary.as_deref(), Some("origin"));
      assert_eq!(second_a.bytes, first_a.bytes, "expected A to hit cache");

      let first_b = fetcher.fetch_with_request(req_b).expect("first B fetch");
      assert_eq!(first_b.vary.as_deref(), Some("origin"));

      handle.join().unwrap();

      assert_eq!(
        request_count.load(Ordering::SeqCst),
        2,
        "expected Vary: Origin responses to be cached per-origin when origin partitioning is enabled"
      );
      assert_ne!(
        first_a.bytes, first_b.bytes,
        "expected distinct cached variants per origin"
      );
    });
  }

  #[test]
  fn caching_fetcher_partitions_stylesheet_cors_by_credentials_mode() {
    #[derive(Clone)]
    struct CredentialSensitiveFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for CredentialSensitiveFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let body = match req.credentials_mode {
          FetchCredentialsMode::Omit => b"omit".to_vec(),
          FetchCredentialsMode::SameOrigin => b"same-origin".to_vec(),
          FetchCredentialsMode::Include => b"include".to_vec(),
        };
        let mut res = FetchedResource::new(body, Some("text/css".to_string()));
        res.final_url = Some(req.url.to_string());
        Ok(res)
      }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = CachingFetcher::new(CredentialSensitiveFetcher {
      calls: Arc::clone(&calls),
    });
    let url = "https://example.com/style.css";
    let same_origin_req = FetchRequest::new(url, FetchDestination::StyleCors)
      .with_credentials_mode(FetchCredentialsMode::SameOrigin);
    let include_req = FetchRequest::new(url, FetchDestination::StyleCors)
      .with_credentials_mode(FetchCredentialsMode::Include);

    let first_same = fetcher
      .fetch_with_request(same_origin_req)
      .expect("fetch same-origin credentials mode");
    let second_same = fetcher
      .fetch_with_request(same_origin_req)
      .expect("fetch same-origin credentials mode (cached)");
    assert_eq!(
      first_same.bytes, second_same.bytes,
      "expected same-origin credential mode response to hit cache"
    );

    let first_include = fetcher
      .fetch_with_request(include_req)
      .expect("fetch include credentials mode");
    let second_include = fetcher
      .fetch_with_request(include_req)
      .expect("fetch include credentials mode (cached)");
    assert_eq!(
      first_include.bytes, second_include.bytes,
      "expected include credential mode response to hit cache"
    );

    assert_ne!(
      first_same.bytes, first_include.bytes,
      "expected distinct cached entries for different credential modes"
    );
    assert_eq!(
      calls.load(Ordering::SeqCst),
      2,
      "expected one underlying fetch per credential mode"
    );
  }

  #[test]
  fn resource_cache_diagnostics_record_inflight_waits() {
    let _lock = resource_cache_diagnostics_test_lock();

    #[derive(Clone)]
    struct SlowFetcher;

    impl ResourceFetcher for SlowFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        thread::sleep(Duration::from_millis(150));
        let mut resource = FetchedResource::new(b"ok".to_vec(), Some("text/plain".to_string()));
        resource.final_url = Some(url.to_string());
        Ok(resource)
      }
    }

    let fetcher = CachingFetcher::new(SlowFetcher);
    let url = "https://example.com/inflight";
    enable_resource_cache_diagnostics();

    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
      let barrier = Arc::clone(&barrier);
      let fetcher = fetcher.clone();
      handles.push(thread::spawn(move || {
        barrier.wait();
        fetcher.fetch(url)
      }));
    }
    barrier.wait();
    for handle in handles {
      handle
        .join()
        .expect("thread should join")
        .expect("fetch should succeed");
    }

    let stats = take_resource_cache_diagnostics().expect("diagnostics should be enabled");
    assert!(
      stats.fetch_inflight_waits >= 1,
      "expected at least one inflight wait (got {})",
      stats.fetch_inflight_waits
    );
    assert!(
      stats.fetch_inflight_wait_ms.is_finite() && stats.fetch_inflight_wait_ms >= 0.0,
      "expected finite inflight wait duration (got {})",
      stats.fetch_inflight_wait_ms
    );
  }

  #[test]
  fn resource_cache_diagnostics_record_stale_hits_under_deadline() {
    let _lock = resource_cache_diagnostics_test_lock();

    #[derive(Clone)]
    struct CountingFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for CountingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut resource = FetchedResource::new(b"cached".to_vec(), Some("text/plain".to_string()));
        resource.final_url = Some(url.to_string());
        resource.cache_policy = Some(HttpCachePolicy {
          max_age: Some(0),
          ..Default::default()
        });
        Ok(resource)
      }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = CachingFetcher::with_config(
      CountingFetcher {
        calls: Arc::clone(&calls),
      },
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        stale_policy: CacheStalePolicy::UseStaleWhenDeadline,
        ..CachingFetcherConfig::default()
      },
    );

    let url = "https://example.com/stale";
    let first = fetcher.fetch(url).expect("seed fetch");
    assert_eq!(first.bytes, b"cached");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    enable_resource_cache_diagnostics();
    let deadline = render_control::RenderDeadline::new(Some(Duration::from_secs(1)), None);
    let second = render_control::with_deadline(Some(&deadline), || fetcher.fetch(url))
      .expect("deadline stale hit should succeed");
    assert_eq!(second.bytes, b"cached");
    assert_eq!(
      calls.load(Ordering::SeqCst),
      1,
      "stale_policy should serve cached bytes under deadline"
    );

    let stats = take_resource_cache_diagnostics().expect("diagnostics should be enabled");
    assert!(
      stats.stale_hits >= 1,
      "expected stale hit counter to increment (got {})",
      stats.stale_hits
    );
  }

  #[test]
  fn resource_cache_diagnostics_avoid_validator_revalidation_under_deadline() {
    let _lock = resource_cache_diagnostics_test_lock();

    #[derive(Clone)]
    struct CountingFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for CountingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut resource = FetchedResource::new(b"cached".to_vec(), Some("text/plain".to_string()));
        resource.final_url = Some(url.to_string());
        resource.etag = Some("\"v1\"".to_string());
        // No Cache-Control/Expires metadata: this normally forces revalidation when validators are
        // present. Under a render deadline we should instead serve the cached bytes.
        resource.cache_policy = None;
        Ok(resource)
      }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = CachingFetcher::with_config(
      CountingFetcher {
        calls: Arc::clone(&calls),
      },
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        stale_policy: CacheStalePolicy::UseStaleWhenDeadline,
        ..CachingFetcherConfig::default()
      },
    );

    let url = "https://example.com/validators";
    let first = fetcher.fetch(url).expect("seed fetch");
    assert_eq!(first.bytes, b"cached");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    enable_resource_cache_diagnostics();
    let deadline = render_control::RenderDeadline::new(Some(Duration::from_secs(1)), None);
    let second = render_control::with_deadline(Some(&deadline), || fetcher.fetch(url))
      .expect("deadline fetch should succeed");
    assert_eq!(second.bytes, b"cached");
    assert_eq!(
      calls.load(Ordering::SeqCst),
      1,
      "stale_policy should serve cached bytes under deadline when freshness is unknown"
    );

    let stats = take_resource_cache_diagnostics().expect("diagnostics should be enabled");
    assert!(
      stats.stale_hits >= 1,
      "expected stale hit counter to increment (got {})",
      stats.stale_hits
    );
  }

  #[test]
  fn caching_fetcher_partitions_by_credentials_mode() {
    #[derive(Clone)]
    struct ModeFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for ModeFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let body: &[u8] = match req.credentials_mode {
          FetchCredentialsMode::Omit => b"omit",
          FetchCredentialsMode::SameOrigin => b"same-origin",
          FetchCredentialsMode::Include => b"include",
        };
        Ok(FetchedResource::new(
          body.to_vec(),
          Some("text/plain".to_string()),
        ))
      }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = CachingFetcher::new(ModeFetcher {
      calls: Arc::clone(&calls),
    });

    let url = "https://example.com/asset";
    let include_req = FetchRequest::new(url, FetchDestination::Other)
      .with_credentials_mode(FetchCredentialsMode::Include);
    let omit_req =
      FetchRequest::new(url, FetchDestination::Other).with_credentials_mode(FetchCredentialsMode::Omit);

    let include = fetcher
      .fetch_with_request(include_req)
      .expect("include fetch");
    assert_eq!(include.bytes, b"include");

    let omit = fetcher.fetch_with_request(omit_req).expect("omit fetch");
    assert_eq!(omit.bytes, b"omit");
    assert_eq!(
      calls.load(Ordering::SeqCst),
      2,
      "expected separate cache entries for include vs omit"
    );

    let include_again = fetcher
      .fetch_with_request(include_req)
      .expect("include cached fetch");
    assert_eq!(include_again.bytes, b"include");
    let omit_again = fetcher
      .fetch_with_request(omit_req)
      .expect("omit cached fetch");
    assert_eq!(omit_again.bytes, b"omit");
    assert_eq!(
      calls.load(Ordering::SeqCst),
      2,
      "expected cached results to avoid additional inner fetches"
    );
  }

  #[cfg(feature = "disk_cache")]
  #[test]
  fn resource_cache_diagnostics_record_disk_cache_hits() {
    let _lock = resource_cache_diagnostics_test_lock();

    #[derive(Clone)]
    struct SeedFetcher;

    impl ResourceFetcher for SeedFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        let mut resource = FetchedResource::new(b"cached".to_vec(), Some("text/plain".to_string()));
        resource.final_url = Some(url.to_string());
        Ok(resource)
      }
    }

    #[derive(Clone)]
    struct PanicFetcher;

    impl ResourceFetcher for PanicFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("inner fetcher should not be called for disk cache hit");
      }
    }

    let tmp = tempfile::tempdir().unwrap();
    let url = "https://example.com/resource";
    let seed = DiskCachingFetcher::new(SeedFetcher, tmp.path());
    let first = seed.fetch(url).expect("seed disk cache");
    assert_eq!(first.bytes, b"cached");

    let disk = DiskCachingFetcher::new(PanicFetcher, tmp.path());
    let _guard = ResourceCacheDiagnosticsGuard::new();
    let second = disk.fetch(url).expect("disk cache hit fetch");
    assert_eq!(second.bytes, b"cached");

    let stats = take_resource_cache_diagnostics().expect("diagnostics should be enabled");
    assert!(
      stats.disk_cache_hits >= 1,
      "expected disk cache hit counter to increment (got {})",
      stats.disk_cache_hits
    );
    assert!(
      stats.disk_cache_ms.is_finite() && stats.disk_cache_ms >= 0.0,
      "expected finite disk cache duration, got {}",
      stats.disk_cache_ms
    );
    assert!(
      stats.disk_cache_bytes >= b"cached".len(),
      "expected disk cache byte counter to include cached payload bytes (got {})",
      stats.disk_cache_bytes
    );
    assert!(
      stats.resource_cache_bytes >= b"cached".len(),
      "expected resource cache byte counter to include returned cached bytes (got {})",
      stats.resource_cache_bytes
    );
    assert_eq!(
      stats.disk_cache_lock_waits, 0,
      "expected disk cache hit to avoid lock waits"
    );
    assert!(
      stats.disk_cache_lock_wait_ms.is_finite()
        && stats.disk_cache_lock_wait_ms >= 0.0
        && stats.disk_cache_lock_wait_ms <= stats.disk_cache_ms,
      "expected lock wait ms to be finite and bounded by total disk cache time (got {})",
      stats.disk_cache_lock_wait_ms
    );
  }

  #[test]
  fn url_to_filename_normalizes_scheme_and_slashes() {
    assert_eq!(
      url_to_filename("https://example.com/foo/bar"),
      "example.com_foo_bar"
    );
    assert_eq!(url_to_filename("http://example.com"), "example.com");
  }

  #[test]
  fn url_to_filename_strips_www_and_replaces_invalid_chars() {
    assert_eq!(
      url_to_filename("https://www.exa mple.com/path?x=1"),
      "www.exa_mple.com_path_x_1"
    );
  }

  #[test]
  fn url_to_filename_trims_trailing_slash() {
    assert_eq!(url_to_filename("https://example.com/"), "example.com");
    assert_eq!(
      url_to_filename("HTTP://WWW.Example.COM/"),
      "www.example.com"
    );
  }

  #[test]
  fn url_to_filename_trims_whitespace_and_fragment() {
    assert_eq!(
      url_to_filename("  https://Example.com/Path#Frag  "),
      "example.com_Path"
    );
  }

  #[test]
  fn url_to_filename_trims_trailing_punctuation() {
    assert_eq!(url_to_filename("https://example.com./"), "example.com");
    assert_eq!(url_to_filename("https://example.com_"), "example.com");
  }

  #[test]
  fn url_to_filename_is_case_insensitive_for_scheme_and_host() {
    assert_eq!(
      url_to_filename("HTTP://WWW.Example.COM/Path/Up"),
      "www.example.com_Path_Up"
    );
  }

  #[test]
  fn url_to_filename_parses_uppercase_scheme() {
    assert_eq!(
      url_to_filename("HTTPS://Example.com/q?a=1"),
      "example.com_q_a_1"
    );
  }

  #[test]
  fn no_store_responses_are_not_cached_by_default() {
    #[derive(Clone)]
    struct NoStoreFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for NoStoreFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut resource = FetchedResource::new(b"ok".to_vec(), Some("text/plain".to_string()));
        resource.final_url = Some(url.to_string());
        resource.cache_policy = Some(HttpCachePolicy {
          no_store: true,
          ..Default::default()
        });
        Ok(resource)
      }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = CachingFetcher::new(NoStoreFetcher {
      calls: Arc::clone(&calls),
    });
    let url = "https://example.com/no-store";

    let first = fetcher.fetch(url).expect("first fetch");
    assert_eq!(first.bytes, b"ok");
    let second = fetcher.fetch(url).expect("second fetch");
    assert_eq!(second.bytes, b"ok");
    assert_eq!(
      calls.load(Ordering::SeqCst),
      2,
      "no-store responses should not be cached by default"
    );
  }

  #[test]
  fn allow_no_store_preserves_cached_bytes_for_fallback() {
    #[derive(Clone)]
    struct QueueFetcher {
      calls: Arc<AtomicUsize>,
      queue: Arc<Mutex<VecDeque<Result<FetchedResource>>>>,
    }

    impl ResourceFetcher for QueueFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self
          .queue
          .lock()
          .unwrap()
          .pop_front()
          .expect("expected queued fetch result")
      }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let mut resource = FetchedResource::new(b"cached".to_vec(), Some("text/plain".to_string()));
    resource.final_url = Some("https://example.com/no-store".to_string());
    resource.cache_policy = Some(HttpCachePolicy {
      no_store: true,
      ..Default::default()
    });

    let mut queue = VecDeque::new();
    queue.push_back(Ok(resource));
    queue.push_back(Err(Error::Other("network error".to_string())));
    let fetcher = CachingFetcher::with_config(
      QueueFetcher {
        calls: Arc::clone(&calls),
        queue: Arc::new(Mutex::new(queue)),
      },
      CachingFetcherConfig {
        allow_no_store: true,
        ..CachingFetcherConfig::default()
      },
    );

    let url = "https://example.com/no-store";
    let first = fetcher.fetch(url).expect("seed fetch");
    assert_eq!(first.bytes, b"cached");
    let second = fetcher.fetch(url).expect("fallback fetch");
    assert_eq!(second.bytes, b"cached");
    assert_eq!(
      calls.load(Ordering::SeqCst),
      2,
      "should attempt refresh but fall back to cached bytes"
    );
  }

  #[test]
  fn allow_no_store_serves_cached_bytes_under_deadline() {
    #[derive(Clone)]
    struct NoStoreFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for NoStoreFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut resource = FetchedResource::new(b"cached".to_vec(), Some("text/plain".to_string()));
        resource.final_url = Some(url.to_string());
        resource.cache_policy = Some(HttpCachePolicy {
          no_store: true,
          ..Default::default()
        });
        Ok(resource)
      }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = CachingFetcher::with_config(
      NoStoreFetcher {
        calls: Arc::clone(&calls),
      },
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        stale_policy: CacheStalePolicy::UseStaleWhenDeadline,
        allow_no_store: true,
        ..CachingFetcherConfig::default()
      },
    );

    let url = "https://example.com/no-store-deadline";
    let first = fetcher.fetch(url).expect("seed fetch");
    assert_eq!(first.bytes, b"cached");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let deadline = render_control::RenderDeadline::new(Some(Duration::from_secs(1)), None);
    let second = render_control::with_deadline(Some(&deadline), || fetcher.fetch(url))
      .expect("deadline fetch");
    assert_eq!(second.bytes, b"cached");
    assert_eq!(
      calls.load(Ordering::SeqCst),
      1,
      "no-store entries should be served from cache under deadline"
    );
  }

  #[test]
  fn parse_vary_headers_normalizes_and_deduplicates() {
    let mut headers = HeaderMap::new();
    headers.append("vary", http::HeaderValue::from_static("Origin, Accept-Encoding"));
    headers.append("vary", http::HeaderValue::from_static("ACCEPT-encoding, User-Agent"));
    assert_eq!(
      parse_vary_headers(&headers).as_deref(),
      Some("accept-encoding, origin, user-agent")
    );
  }

  #[test]
  fn caching_fetcher_does_not_cache_vary_star() {
    #[derive(Clone)]
    struct VaryFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for VaryFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut res = FetchedResource::new(b"ok".to_vec(), Some("text/plain".to_string()));
        res.final_url = Some(url.to_string());
        res.vary = Some("*".to_string());
        Ok(res)
      }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = CachingFetcher::new(VaryFetcher {
      calls: Arc::clone(&calls),
    });
    let url = "https://example.com/vary-star";
    assert_eq!(fetcher.fetch(url).expect("first").bytes, b"ok");
    assert_eq!(fetcher.fetch(url).expect("second").bytes, b"ok");
    assert_eq!(
      calls.load(Ordering::SeqCst),
      2,
      "expected Vary: * responses to be treated as uncacheable"
    );
  }

  #[test]
  fn caching_fetcher_does_not_cache_unhandled_vary_by_default() {
    #[derive(Clone)]
    struct VaryFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for VaryFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut res = FetchedResource::new(b"ok".to_vec(), Some("text/plain".to_string()));
        res.final_url = Some(url.to_string());
        res.vary = Some("x-foo".to_string());
        Ok(res)
      }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = CachingFetcher::new(VaryFetcher {
      calls: Arc::clone(&calls),
    });
    let url = "https://example.com/vary-unknown";
    assert_eq!(fetcher.fetch(url).expect("first").bytes, b"ok");
    assert_eq!(fetcher.fetch(url).expect("second").bytes, b"ok");
    assert_eq!(
      calls.load(Ordering::SeqCst),
      2,
      "expected unknown Vary headers to disable caching by default"
    );
  }

  #[test]
  fn caching_fetcher_caches_vary_origin_without_origin_partitioning() {
    #[derive(Clone)]
    struct OriginVaryFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for OriginVaryFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected request-aware fetch");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut res = FetchedResource::new(b"ok".to_vec(), Some("text/plain".to_string()));
        res.final_url = Some(req.url.to_string());
        res.vary = Some("origin".to_string());
        Ok(res)
      }

      fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
        if header_name.eq_ignore_ascii_case("origin") {
          return cors_origin_key_for_request(&req).or_else(|| Some(String::new()));
        }
        None
      }
    }

    let url = "https://example.com/vary-origin";
    let req =
      FetchRequest::new(url, FetchDestination::Font).with_referrer_url("https://a.test/page");

    let calls_unpartitioned = Arc::new(AtomicUsize::new(0));
    let fetcher_unpartitioned = CachingFetcher::new(OriginVaryFetcher {
      calls: Arc::clone(&calls_unpartitioned),
    });
    runtime::with_thread_runtime_toggles(
      Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
        "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
        "0".to_string(),
      )]))),
      || {
        assert!(fetcher_unpartitioned.fetch_with_request(req).is_ok());
        assert!(fetcher_unpartitioned.fetch_with_request(req).is_ok());
      },
    );
    assert_eq!(
      calls_unpartitioned.load(Ordering::SeqCst),
      1,
      "expected Vary: Origin response to be cached without origin partitioning"
    );

    let calls_partitioned = Arc::new(AtomicUsize::new(0));
    let fetcher_partitioned = CachingFetcher::new(OriginVaryFetcher {
      calls: Arc::clone(&calls_partitioned),
    });
    runtime::with_thread_runtime_toggles(
      Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
        "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
        "1".to_string(),
      )]))),
      || {
        assert!(fetcher_partitioned.fetch_with_request(req).is_ok());
        assert!(fetcher_partitioned.fetch_with_request(req).is_ok());
      },
    );
    assert_eq!(
      calls_partitioned.load(Ordering::SeqCst),
      1,
      "expected Vary: Origin response to be cached when origin partitioning is enabled"
    );
  }

  #[test]
  fn caching_fetcher_partitions_entries_by_context() {
    #[derive(Debug)]
    struct SeenCall {
      kind: FetchContextKind,
      url: String,
    }

    #[derive(Clone)]
    struct KindAwareFetcher {
      calls: Arc<Mutex<Vec<SeenCall>>>,
    }

    impl ResourceFetcher for KindAwareFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_context(FetchContextKind::Other, url)
      }

      fn fetch_with_context(&self, kind: FetchContextKind, url: &str) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push(SeenCall {
          kind,
          url: url.to_string(),
        });
        let mut resource = FetchedResource::new(
          format!("kind={}", kind.as_str()).into_bytes(),
          Some("text/plain".to_string()),
        );
        resource.final_url = Some(url.to_string());
        Ok(resource)
      }
    }

    let calls = Arc::new(Mutex::new(Vec::new()));
    let fetcher = CachingFetcher::new(KindAwareFetcher {
      calls: Arc::clone(&calls),
    });
    let url = "https://example.com/asset";

    let doc = fetcher
      .fetch_with_context(FetchContextKind::Document, url)
      .expect("document fetch");
    assert_eq!(doc.bytes, b"kind=document".to_vec());

    let font = fetcher
      .fetch_with_context(FetchContextKind::Font, url)
      .expect("font fetch");
    assert_eq!(font.bytes, b"kind=font".to_vec());

    // Both entries should be cached separately.
    let doc_again = fetcher
      .fetch_with_context(FetchContextKind::Document, url)
      .expect("document cache hit");
    assert_eq!(doc_again.bytes, b"kind=document".to_vec());
    let font_again = fetcher
      .fetch_with_context(FetchContextKind::Font, url)
      .expect("font cache hit");
    assert_eq!(font_again.bytes, b"kind=font".to_vec());

    let calls = calls.lock().unwrap();
    assert_eq!(
      calls.len(),
      2,
      "expected cache entries to be isolated by kind (calls={calls:?})"
    );
    assert_eq!(calls[0].kind, FetchContextKind::Document);
    assert_eq!(calls[1].kind, FetchContextKind::Font);
    assert_eq!(calls[0].url, url);
    assert_eq!(calls[1].url, url);
  }

  #[test]
  fn url_to_filename_trims_trailing_slashes() {
    assert_eq!(url_to_filename("https://example.com/"), "example.com");
    assert_eq!(
      url_to_filename("https://example.com/foo/"),
      "example.com_foo"
    );
  }

  #[test]
  fn normalize_page_name_does_not_panic_on_non_utf8_boundary_prefixes() {
    assert_eq!(
      normalize_page_name("€€€foobar").as_deref(),
      Some("___foobar")
    );
  }

  #[test]
  fn url_to_filename_does_not_panic_on_non_utf8_boundary_prefixes() {
    assert_eq!(url_to_filename("€€€foobar#frag"), "___foobar");
  }

  #[test]
  fn normalize_user_agent_for_log_does_not_panic_on_non_utf8_boundary_prefixes() {
    let ua = "€€€€foobar";
    assert_eq!(normalize_user_agent_for_log(ua), ua);
  }

  #[test]
  fn normalize_page_name_handles_urls_and_hosts() {
    assert_eq!(
      normalize_page_name("https://www.w3.org").as_deref(),
      Some("w3.org")
    );
    assert_eq!(
      normalize_page_name("http://example.com/foo").as_deref(),
      Some("example.com_foo")
    );
    assert_eq!(
      normalize_page_name("example.com").as_deref(),
      Some("example.com")
    );
  }

  #[test]
  fn normalize_page_name_rejects_empty() {
    assert!(normalize_page_name("").is_none());
    assert!(normalize_page_name("   ").is_none());
  }

  #[test]
  fn normalize_page_name_strips_fragment_and_whitespace() {
    assert_eq!(
      normalize_page_name("  https://example.com/path#section  ").as_deref(),
      Some("example.com_path")
    );
  }

  #[test]
  fn normalize_page_name_handles_query_and_uppercase_scheme() {
    assert_eq!(
      normalize_page_name("HTTP://Example.com/Path?foo=1").as_deref(),
      Some("example.com_Path_foo_1")
    );
  }

  #[test]
  fn normalize_page_name_handles_uppercase_host_and_trailing_slash() {
    assert_eq!(
      normalize_page_name("HTTP://WWW.Example.COM/").as_deref(),
      Some("example.com")
    );
  }

  #[test]
  fn normalize_page_name_strips_trailing_punctuation_with_whitespace() {
    assert_eq!(
      normalize_page_name("  HTTPS://Example.com./  ").as_deref(),
      Some("example.com")
    );
  }

  #[test]
  fn normalize_page_name_trims_trailing_punctuation() {
    assert_eq!(
      normalize_page_name("https://example.com./").as_deref(),
      Some("example.com")
    );
    assert_eq!(
      normalize_page_name("example.com_").as_deref(),
      Some("example.com")
    );
  }

  #[test]
  fn normalize_user_agent_for_log_strips_prefix_and_whitespace() {
    assert_eq!(
      normalize_user_agent_for_log("User-Agent: Foo/1.0"),
      "Foo/1.0"
    );
    assert_eq!(
      normalize_user_agent_for_log("User-Agent:   Foo/1.0   "),
      "Foo/1.0"
    );
    assert_eq!(
      normalize_user_agent_for_log("user-agent: Foo/1.0"),
      "Foo/1.0"
    );
    assert_eq!(
      normalize_user_agent_for_log("USER-AGENT:Foo/1.0"),
      "Foo/1.0"
    );
    assert_eq!(normalize_user_agent_for_log("Mozilla/5.0"), "Mozilla/5.0");
    assert_eq!(normalize_user_agent_for_log(""), "");
  }

  #[test]
  fn normalize_page_name_strips_www_case_insensitively() {
    assert_eq!(
      normalize_page_name("WWW.Example.com").as_deref(),
      Some("example.com")
    );
    assert_eq!(
      normalize_page_name("www.example.com").as_deref(),
      Some("example.com")
    );
  }

  #[test]
  fn url_to_filename_drops_fragments() {
    assert_eq!(
      url_to_filename("https://example.com/path#frag"),
      "example.com_path"
    );
    assert_eq!(
      url_to_filename("https://example.com/path?q=1#frag"),
      "example.com_path_q_1"
    );
  }

  #[test]
  fn url_to_filename_drops_fragments_without_scheme() {
    assert_eq!(url_to_filename("Example.Com/path#frag"), "example.com_path");
    assert_eq!(
      url_to_filename("Example.Com/path?q=1#frag"),
      "example.com_path_q_1"
    );
  }

  #[test]
  fn url_to_filename_strips_fragments_with_uppercase_scheme() {
    assert_eq!(
      url_to_filename("HTTP://Example.com/Path#Frag"),
      "example.com_Path"
    );
    assert_eq!(
      url_to_filename("HTTP://Example.com/Path?q=1#Frag"),
      "example.com_Path_q_1"
    );
  }

  #[test]
  fn url_to_filename_handles_data_urls() {
    // Url::parse understands data: URLs, so the scheme is stripped and we sanitize the payload.
    assert_eq!(url_to_filename("data:text/html,<p>hi"), "text_html__p_hi");
  }

  #[test]
  fn sanitize_filename_trims_trailing_separators_and_punctuation() {
    assert_eq!(sanitize_filename("example.com///"), "example.com");
    assert_eq!(sanitize_filename("foo_bar.."), "foo_bar");
  }

  #[test]
  fn fetched_resource_new_defaults_metadata() {
    let resource = FetchedResource::new(vec![1, 2, 3], Some("text/plain".to_string()));

    assert_eq!(resource.status, None);
    assert_eq!(resource.etag, None);
    assert_eq!(resource.last_modified, None);
    assert_eq!(resource.final_url, None);
  }

  #[test]
  fn fetched_resource_with_final_url_sets_only_final_url() {
    let resource = FetchedResource::with_final_url(
      b"bytes".to_vec(),
      Some("text/plain".to_string()),
      Some("http://example.com/data".to_string()),
    );

    assert_eq!(resource.status, None);
    assert_eq!(resource.etag, None);
    assert_eq!(resource.last_modified, None);
    assert_eq!(
      resource.final_url.as_deref(),
      Some("http://example.com/data")
    );
  }

  #[test]
  fn test_fetched_resource_is_image() {
    let resource = FetchedResource::new(vec![], Some("image/png".to_string()));
    assert!(resource.is_image());

    let resource = FetchedResource::new(vec![], Some("text/css".to_string()));
    assert!(!resource.is_image());
  }

  #[test]
  fn test_fetched_resource_is_css() {
    let resource = FetchedResource::new(vec![], Some("text/css".to_string()));
    assert!(resource.is_css());

    let resource = FetchedResource::new(vec![], Some("text/css; charset=utf-8".to_string()));
    assert!(resource.is_css());
  }

  #[test]
  fn test_guess_content_type() {
    assert_eq!(
      guess_content_type_from_path("/path/to/image.png"),
      Some("image/png".to_string())
    );
    assert_eq!(
      guess_content_type_from_path("/path/to/style.CSS"),
      Some("text/css".to_string())
    );
    assert_eq!(guess_content_type_from_path("/path/to/file"), None);
  }

  #[test]
  fn is_data_url_matches_case_insensitively() {
    assert!(is_data_url("data:text/plain,hello"));
    assert!(is_data_url("DATA:text/plain,hello"));
    assert!(!is_data_url("https://example.com/"));
  }

  #[test]
  fn test_decode_data_url_base64() {
    let url = "data:image/png;base64,aGVsbG8="; // "hello" in base64
    let resource = data_url::decode_data_url(url).unwrap();
    assert_eq!(resource.bytes, b"hello");
    assert_eq!(resource.content_type, Some("image/png".to_string()));
  }

  #[test]
  fn test_decode_data_url_percent() {
    let url = "data:text/plain,hello%20world";
    let resource = data_url::decode_data_url(url).unwrap();
    assert_eq!(resource.bytes, b"hello world");
    assert_eq!(resource.content_type, Some("text/plain".to_string()));
  }

  #[test]
  fn test_decode_data_url_no_mediatype() {
    let url = "data:,hello";
    let resource = data_url::decode_data_url(url).unwrap();
    assert_eq!(resource.bytes, b"hello");
    assert_eq!(
      resource.content_type,
      Some("text/plain;charset=US-ASCII".to_string())
    );
  }

  #[test]
  fn data_url_default_mediatype_can_be_overridden_by_parameter() {
    let resource = data_url::decode_data_url("data:;charset=utf-8,hi").unwrap();
    assert_eq!(resource.bytes, b"hi");
    assert_eq!(
      resource.content_type,
      Some("text/plain;charset=utf-8".to_string())
    );
  }

  #[test]
  fn data_url_plus_is_literal() {
    let resource = data_url::decode_data_url("data:,a+b").unwrap();
    assert_eq!(resource.bytes, b"a+b");
    assert_eq!(
      resource.content_type,
      Some("text/plain;charset=US-ASCII".to_string())
    );
  }

  #[test]
  fn data_url_percent_decoding_roundtrips_hex() {
    let resource = data_url::decode_data_url("data:text/plain,a%2Bb%20c").unwrap();
    assert_eq!(resource.bytes, b"a+b c");
  }

  #[test]
  fn data_url_tolerates_malformed_percent_escape() {
    let resource = data_url::decode_data_url("data:text/plain,abc%2").unwrap();
    assert_eq!(resource.bytes, b"abc%2");

    let resource = data_url::decode_data_url("data:text/plain,%2G").unwrap();
    assert_eq!(resource.bytes, b"%2G");
  }

  #[test]
  fn data_url_base64_is_case_insensitive_and_tolerates_whitespace() {
    let url = "data:text/plain;BASE64,aGVs bG8=\n";
    let resource = data_url::decode_data_url(url).unwrap();
    assert_eq!(resource.bytes, b"hello");
    assert_eq!(resource.content_type, Some("text/plain".to_string()));
  }

  #[test]
  fn data_url_preserves_additional_parameters() {
    let url = "data:text/plain;charset=UTF-8;param=value;base64,aA==";
    let resource = data_url::decode_data_url(url).unwrap();
    assert_eq!(resource.bytes, b"h");
    assert_eq!(
      resource.content_type,
      Some("text/plain;charset=UTF-8;param=value".to_string())
    );
  }

  #[test]
  fn parse_cached_meta_supports_legacy_content_type() {
    let meta = parse_cached_html_meta("text/html; charset=utf-8");
    assert_eq!(
      meta.content_type.as_deref(),
      Some("text/html; charset=utf-8")
    );
    assert_eq!(meta.url, None);
    assert_eq!(meta.status, None);
    assert_eq!(meta.response_referrer_policy, None);
  }

  #[test]
  fn parse_cached_meta_reads_key_value_lines() {
    let meta = "content-type: text/html\nurl: https://example.com/page\n";
    let parsed = parse_cached_html_meta(meta);
    assert_eq!(parsed.content_type.as_deref(), Some("text/html"));
    assert_eq!(parsed.url.as_deref(), Some("https://example.com/page"));
    assert_eq!(parsed.status, None);
    assert_eq!(parsed.response_referrer_policy, None);
  }

  #[test]
  fn parse_cached_meta_reads_status_code() {
    let meta = "content-type: text/html\nstatus: 302\nurl: https://example.com/page\n";
    let parsed = parse_cached_html_meta(meta);
    assert_eq!(parsed.content_type.as_deref(), Some("text/html"));
    assert_eq!(parsed.url.as_deref(), Some("https://example.com/page"));
    assert_eq!(parsed.status, Some(302));
    assert_eq!(parsed.response_referrer_policy, None);
  }

  #[test]
  fn parse_cached_meta_reads_referrer_policy() {
    let meta =
      "content-type: text/html\nreferrer-policy: no-referrer\nurl: https://example.com/page\n";
    let parsed = parse_cached_html_meta(meta);
    assert_eq!(parsed.content_type.as_deref(), Some("text/html"));
    assert_eq!(parsed.url.as_deref(), Some("https://example.com/page"));
    assert_eq!(parsed.response_referrer_policy, Some(ReferrerPolicy::NoReferrer));
  }

  #[test]
  fn http_fetcher_follows_redirects() {
    let Some(listener) = try_bind_localhost("http_fetcher_follows_redirects") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      let mut conn_count = 0;
      for stream in listener.incoming() {
        let mut stream = stream.unwrap();
        conn_count += 1;
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);

        if conn_count == 1 {
          let resp = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{}\r\nContent-Length: 0\r\n\r\n",
            addr
          );
          let _ = stream.write_all(resp.as_bytes());
        } else {
          let body = b"ok";
          let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
            body.len()
          );
          let _ = stream.write_all(headers.as_bytes());
          let _ = stream.write_all(body);
          break;
        }
      }
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(5));
    let url = format!("http://{}/", addr);
    let res = fetcher.fetch(&url).expect("fetch redirect");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"ok");
    assert_eq!(res.content_type, Some("text/plain".to_string()));
    assert!(
      res
        .final_url
        .as_deref()
        .unwrap_or("")
        .starts_with(&format!("http://{}", addr)),
      "final_url should record redirect destination: {:?}",
      res.final_url
    );
  }

  #[test]
  fn http_fetcher_persists_cookies_across_requests_and_clones() {
    let Some(listener) =
      try_bind_localhost("http_fetcher_persists_cookies_across_requests_and_clones")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      // First request sets a cookie.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream).unwrap();
      let body = b"first";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: a=b; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
      drop(stream);

      // Second request must send the cookie.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let request = read_http_request(&mut stream)
        .unwrap()
        .expect("expected second HTTP request");
      let req = String::from_utf8_lossy(&request).to_ascii_lowercase();
      assert!(
        req.contains("cookie: a=b"),
        "expected cookie header in follow-up request, got:\n{req}"
      );

      let body = b"second";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let cloned = fetcher.clone();
    let url = format!("http://{}/set", addr);
    let res = fetcher.fetch(&url).expect("fetch initial cookie setter");
    assert_eq!(res.bytes, b"first");

    let url = format!("http://{}/check", addr);
    let res = cloned
      .fetch(&url)
      .expect("fetch should include cookie from prior request");
    assert_eq!(res.bytes, b"second");
    handle.join().unwrap();
  }

  #[test]
  fn http_fetcher_sends_cookies_on_redirect_followups() {
    let Some(listener) = try_bind_localhost("http_fetcher_sends_cookies_on_redirect_followups")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      // First request triggers a redirect and sets a cookie.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream).unwrap();
      let headers = "HTTP/1.1 302 Found\r\nLocation: /final\r\nSet-Cookie: a=b; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
      stream.write_all(headers.as_bytes()).unwrap();
      drop(stream);

      // Redirect follow-up request must include the cookie.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let request = read_http_request(&mut stream)
        .unwrap()
        .expect("expected redirect follow-up request");
      let req = String::from_utf8_lossy(&request).to_ascii_lowercase();
      assert!(
        req.contains("cookie: a=b"),
        "expected cookie header in redirect follow-up request, got:\n{req}"
      );

      let body = b"ok";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}/start", addr);
    let res = fetcher.fetch(&url).expect("fetch should follow redirect");
    assert_eq!(res.bytes, b"ok");
    handle.join().unwrap();
  }

  #[test]
  fn http_fetcher_shares_cookie_jar_across_backends() {
    let Some(listener) =
      try_bind_localhost("http_fetcher_shares_cookie_jar_across_backends")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      // First request sets a cookie.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream).unwrap();
      let body = b"first";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: a=b; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
      drop(stream);

      // Second request must send the cookie, even when switching to the reqwest backend.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let request = read_http_request(&mut stream)
        .unwrap()
        .expect("expected second request");
      let req = String::from_utf8_lossy(&request).to_ascii_lowercase();
      assert!(
        req.contains("cookie: a=b"),
        "expected cookie header when switching backends, got:\n{req}"
      );

      let body = b"second";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let deadline: Option<render_control::RenderDeadline> = None;
    let validators: Option<HttpCacheValidators<'_>> = None;

    let url = format!("http://{}/set", addr);
    let first = fetcher
      .fetch_http_with_accept_inner_ureq(
        FetchContextKind::Other,
        FetchDestination::Other,
        &url,
        None,
        validators,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        Instant::now(),
        false,
      )
      .expect("ureq fetch should succeed");
    assert_eq!(first.bytes, b"first");

    let url = format!("http://{}/check", addr);
    let second = fetcher
      .fetch_http_with_accept_inner_reqwest(
        FetchContextKind::Other,
        FetchDestination::Other,
        &url,
        None,
        validators,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        Instant::now(),
        false,
      )
      .expect("reqwest fetch should succeed");
    assert_eq!(second.bytes, b"second");
    handle.join().unwrap();
  }

  #[test]
  fn http_fetcher_credentials_mode_omit_disables_cookie_send_and_store() {
    let Some(listener) =
      try_bind_localhost("http_fetcher_credentials_mode_omit_disables_cookie_send_and_store")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
      // First request sets a cookie.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let _ = read_http_request(&mut stream).unwrap();
      let body = b"first";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: a=b; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
      drop(stream);

      // Second request must not send cookies and must not store the new cookie.
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let request = read_http_request(&mut stream)
        .unwrap()
        .expect("expected second request");
      let req = String::from_utf8_lossy(&request).to_ascii_lowercase();
      assert!(
        !req.contains("cookie:"),
        "expected omit mode to suppress cookies, got:\n{req}"
      );

      let body = b"second";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: c=d; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let deadline: Option<render_control::RenderDeadline> = None;
    let validators: Option<HttpCacheValidators<'_>> = None;

    let url = format!("http://{}/set", addr);
    let first = fetcher
      .fetch_http_with_accept_inner_ureq(
        FetchContextKind::Other,
        FetchDestination::Other,
        &url,
        None,
        validators,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Include,
        &deadline,
        Instant::now(),
        false,
      )
      .expect("seed cookie fetch");
    assert_eq!(first.bytes, b"first");
    assert_eq!(
      fetcher.cookie_header_value(&url).as_deref(),
      Some("a=b"),
      "expected initial cookie to be stored"
    );

    let url = format!("http://{}/omit", addr);
    let second = fetcher
      .fetch_http_with_accept_inner_ureq(
        FetchContextKind::Other,
        FetchDestination::Other,
        &url,
        None,
        validators,
        None,
        None,
        ReferrerPolicy::default(),
        FetchCredentialsMode::Omit,
        &deadline,
        Instant::now(),
        false,
      )
      .expect("omit fetch");
    assert_eq!(second.bytes, b"second");

    let cookies = fetcher.cookie_header_value(&url).unwrap_or_default();
    assert!(
      cookies.contains("a=b"),
      "expected prior cookie to remain available, got: {cookies:?}"
    );
    assert!(
      !cookies.contains("c=d"),
      "expected omit mode to ignore Set-Cookie, got: {cookies:?}"
    );
    handle.join().unwrap();
  }

  #[test]
  fn http_fetcher_sets_accept_language() {
    let Some(listener) = try_bind_localhost("http_fetcher_sets_accept_language") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      if let Some(stream) = listener.incoming().next() {
        let mut stream = stream.unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let req = String::from_utf8_lossy(&buf);
        if let Ok(mut slot) = captured_req.lock() {
          *slot = req.to_string();
        }

        let body = b"hi";
        let headers = format!(
          "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
          body.len()
        );
        let _ = stream.write_all(headers.as_bytes());
        let _ = stream.write_all(body);
      }
    });

    let fetcher = HttpFetcher::new();
    let url = format!("http://{}/", addr);
    let res = fetcher.fetch(&url).expect("fetch lang");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"hi");
    let req = captured.lock().unwrap().to_lowercase();
    assert!(
      req.contains("accept-language: en-us,en;q=0.9"),
      "missing header: {}",
      req
    );
  }

  #[test]
  fn http_fetcher_sets_accept_header() {
    let Some(listener) = try_bind_localhost("http_fetcher_sets_accept_header") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let request = read_http_request(&mut stream)
        .unwrap()
        .expect("expected HTTP request");
      if let Ok(mut slot) = captured_req.lock() {
        *slot = String::from_utf8_lossy(&request).to_string();
      }

      let body = b"hi";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new();
    let url = format!("http://{}/", addr);
    let res = fetcher
      .fetch_with_context(FetchContextKind::Document, &url)
      .expect("fetch accept header");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"hi");
    let req = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.contains(&format!("accept: {DEFAULT_ACCEPT}").to_ascii_lowercase()),
      "missing accept header: {req}"
    );
  }

  #[test]
  fn http_fetcher_sets_font_request_headers() {
    let Some(listener) = try_bind_localhost("http_fetcher_sets_font_request_headers") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let request = read_http_request(&mut stream)
        .unwrap()
        .expect("expected HTTP request");
      if let Ok(mut slot) = captured_req.lock() {
        *slot = String::from_utf8_lossy(&request).to_string();
      }

      let body = b"font";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: font/woff2\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}/asset.woff2", addr);
    let res = fetcher
      .fetch_with_context(FetchContextKind::Font, &url)
      .expect("fetch font");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"font");
    let req = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.contains("accept: */*"),
      "expected font accept header, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-dest: font"),
      "expected Sec-Fetch-Dest for font, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-mode: cors"),
      "expected Sec-Fetch-Mode cors for font, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-site: same-origin"),
      "expected Sec-Fetch-Site same-origin for font, got: {req}"
    );
    let expected_origin = format!("origin: http://{}", addr).to_ascii_lowercase();
    assert!(
      req.contains(&expected_origin),
      "expected Origin header for font, got: {req}"
    );
    let expected_referer = format!("referer: http://{}/", addr).to_ascii_lowercase();
    assert!(
      req.contains(&expected_referer),
      "expected Referer header for font, got: {req}"
    );
    assert!(
      req.contains("accept-encoding: gzip, deflate, br"),
      "expected Accept-Encoding for font, got: {req}"
    );
    assert!(
      !req.contains("upgrade-insecure-requests: 1"),
      "font requests should not set Upgrade-Insecure-Requests, got: {req}"
    );
    assert!(
      !req.contains("sec-fetch-user: ?1"),
      "font requests should not set Sec-Fetch-User, got: {req}"
    );
  }

  #[test]
  fn http_fetcher_sets_font_origin_null_for_file_referrer() {
    let Some(listener) = try_bind_localhost("http_fetcher_sets_font_origin_null_for_file_referrer")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let request = read_http_request(&mut stream)
        .unwrap()
        .expect("expected HTTP request");
      if let Ok(mut slot) = captured_req.lock() {
        *slot = String::from_utf8_lossy(&request).to_string();
      }

      let body = b"font";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: font/woff2\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}/asset.woff2", addr);
    let referrer = "file:///fixture.html";
    let res = fetcher
      .fetch_with_request(
        FetchRequest::new(&url, FetchDestination::Font).with_referrer_url(referrer),
      )
      .expect("fetch font");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"font");
    let req = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.contains("origin: null"),
      "expected Origin: null for file referrer, got: {req}"
    );
  }

  #[test]
  fn http_fetcher_sets_image_cors_request_headers() {
    let Some(listener) = try_bind_localhost("http_fetcher_sets_image_cors_request_headers") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let request = read_http_request(&mut stream)
        .unwrap()
        .expect("expected HTTP request");
      if let Ok(mut slot) = captured_req.lock() {
        *slot = String::from_utf8_lossy(&request).to_string();
      }

      let body = b"img";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}/asset.png", addr);
    let referrer = format!("http://{}/doc.html", addr);
    let res = fetcher
      .fetch_with_request(
        FetchRequest::new(&url, FetchDestination::ImageCors).with_referrer_url(&referrer),
      )
      .expect("fetch image cors");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"img");
    let req = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.contains(&format!("accept: {BROWSER_ACCEPT_IMAGE}").to_ascii_lowercase()),
      "expected image accept header, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-dest: image"),
      "expected Sec-Fetch-Dest image, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-mode: cors"),
      "expected Sec-Fetch-Mode cors, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-site: same-origin"),
      "expected Sec-Fetch-Site same-origin, got: {req}"
    );
    let expected_origin = format!("origin: http://{}", addr).to_ascii_lowercase();
    assert!(
      req.contains(&expected_origin),
      "expected Origin header for image cors, got: {req}"
    );
    let expected_referer = format!("referer: http://{}/doc.html", addr).to_ascii_lowercase();
    assert!(
      req.contains(&expected_referer),
      "expected Referer header for image cors, got: {req}"
    );
  }

  #[test]
  fn http_fetcher_sets_image_cors_origin_null_for_file_referrer() {
    let Some(listener) =
      try_bind_localhost("http_fetcher_sets_image_cors_origin_null_for_file_referrer")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let request = read_http_request(&mut stream)
        .unwrap()
        .expect("expected HTTP request");
      if let Ok(mut slot) = captured_req.lock() {
        *slot = String::from_utf8_lossy(&request).to_string();
      }

      let body = b"img";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}/asset.png", addr);
    let referrer = "file:///fixture.html";
    let res = fetcher
      .fetch_with_request(
        FetchRequest::new(&url, FetchDestination::ImageCors).with_referrer_url(referrer),
      )
      .expect("fetch image cors");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"img");
    let req = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.contains("origin: null"),
      "expected Origin: null for file referrer, got: {req}"
    );
  }

  #[test]
  fn http_fetcher_sets_image_cors_partial_request_headers() {
    let Some(listener) = try_bind_localhost("http_fetcher_sets_image_cors_partial_request_headers")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let request = read_http_request(&mut stream)
        .unwrap()
        .expect("expected HTTP request");
      if let Ok(mut slot) = captured_req.lock() {
        *slot = String::from_utf8_lossy(&request).to_string();
      }

      let body = b"img";
      let headers = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Type: image/png\r\nContent-Length: {}\r\nContent-Range: bytes 0-2/3\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}/asset.png", addr);
    // Use a cross-origin referrer so the `Origin` header must come from the referrer (not the
    // target URL origin).
    let referrer = "http://a.test/page";
    let res = fetcher
      .fetch_partial_with_request(
        FetchRequest::new(&url, FetchDestination::ImageCors).with_referrer_url(referrer),
        2,
      )
      .expect("fetch image cors prefix");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"im");
    let req = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.contains("sec-fetch-mode: cors"),
      "expected Sec-Fetch-Mode cors for partial image, got: {req}"
    );
    assert!(
      req.contains("range: bytes=0-1"),
      "expected Range header for partial image, got: {req}"
    );
    assert!(
      req.contains("origin: http://a.test"),
      "expected Origin header derived from referrer for partial image, got: {req}"
    );
    assert!(
      req.contains("referer: http://a.test/"),
      "expected Referer header derived from referrer for partial image, got: {req}"
    );
  }

  #[test]
  fn http_fetcher_sets_stylesheet_request_headers() {
    let Some(listener) = try_bind_localhost("http_fetcher_sets_stylesheet_request_headers") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let request = read_http_request(&mut stream)
        .unwrap()
        .expect("expected HTTP request");
      if let Ok(mut slot) = captured_req.lock() {
        *slot = String::from_utf8_lossy(&request).to_string();
      }

      let body = b"body { color: red; }";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}/style.css", addr);
    let res = fetcher
      .fetch_with_context(FetchContextKind::Stylesheet, &url)
      .expect("fetch stylesheet");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"body { color: red; }");
    let req = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.contains(&format!("accept: {BROWSER_ACCEPT_STYLESHEET}").to_ascii_lowercase()),
      "expected stylesheet accept header, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-dest: style"),
      "expected Sec-Fetch-Dest style for stylesheet, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-mode: no-cors"),
      "expected Sec-Fetch-Mode no-cors for stylesheet, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-site: same-origin"),
      "expected Sec-Fetch-Site same-origin for stylesheet, got: {req}"
    );
  }

  #[test]
  fn http_fetcher_sets_stylesheet_cors_request_headers() {
    let Some(listener) = try_bind_localhost("http_fetcher_sets_stylesheet_cors_request_headers")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let request = read_http_request(&mut stream)
        .unwrap()
        .expect("expected HTTP request");
      if let Ok(mut slot) = captured_req.lock() {
        *slot = String::from_utf8_lossy(&request).to_string();
      }

      let body = b"body { color: red; }";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{addr}/style.css");
    let referrer = "http://a.test/page.html";
    let client_origin = origin_from_url(referrer).expect("origin");
    let res = fetcher
      .fetch_with_request(
        FetchRequest::new(&url, FetchDestination::StyleCors)
          .with_referrer_url(referrer)
          .with_client_origin(&client_origin)
          .with_credentials_mode(FetchCredentialsMode::SameOrigin),
      )
      .expect("fetch stylesheet cors");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"body { color: red; }");
    let req = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.contains(&format!("accept: {BROWSER_ACCEPT_STYLESHEET}").to_ascii_lowercase()),
      "expected stylesheet accept header, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-dest: style"),
      "expected Sec-Fetch-Dest style for stylesheet cors, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-mode: cors"),
      "expected Sec-Fetch-Mode cors for stylesheet cors, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-site: cross-site"),
      "expected Sec-Fetch-Site cross-site for stylesheet cors, got: {req}"
    );
    assert!(
      req.contains("origin: http://a.test"),
      "expected Origin header derived from client origin, got: {req}"
    );
    assert!(
      req.contains("referer: http://a.test/"),
      "expected Referer header derived from referrer policy, got: {req}"
    );
  }

  #[test]
  fn http_fetcher_sets_iframe_request_headers() {
    let Some(listener) = try_bind_localhost("http_fetcher_sets_iframe_request_headers") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let request = read_http_request(&mut stream)
        .unwrap()
        .expect("expected HTTP request");
      if let Ok(mut slot) = captured_req.lock() {
        *slot = String::from_utf8_lossy(&request).to_string();
      }

      let body = b"iframe";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}/frame.html", addr);
    let referrer = format!("http://{}/outer.html", addr);
    let res = fetcher
      .fetch_with_request(
        FetchRequest::new(&url, FetchDestination::Iframe).with_referrer_url(&referrer),
      )
      .expect("fetch iframe");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"iframe");
    let req = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.contains("sec-fetch-dest: iframe"),
      "expected Sec-Fetch-Dest iframe, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-mode: navigate"),
      "expected Sec-Fetch-Mode navigate for iframe, got: {req}"
    );
    assert!(
      !req.contains("sec-fetch-user:"),
      "iframe navigations should not set Sec-Fetch-User, got: {req}"
    );
  }

  #[test]
  fn http_fetcher_sets_document_no_user_request_headers() {
    let Some(listener) = try_bind_localhost("http_fetcher_sets_document_no_user_request_headers")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
      let request = read_http_request(&mut stream)
        .unwrap()
        .expect("expected HTTP request");
      if let Ok(mut slot) = captured_req.lock() {
        *slot = String::from_utf8_lossy(&request).to_string();
      }

      let body = b"doc";
      let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(headers.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}/redirect.html", addr);
    let referrer = format!("http://{}/origin.html", addr);
    let res = fetcher
      .fetch_with_request(FetchRequest::document_no_user(&url).with_referrer_url(&referrer))
      .expect("fetch document redirect");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"doc");
    let req = captured.lock().unwrap().to_ascii_lowercase();
    assert!(
      req.contains("sec-fetch-dest: document"),
      "expected Sec-Fetch-Dest document, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-mode: navigate"),
      "expected Sec-Fetch-Mode navigate for document, got: {req}"
    );
    assert!(
      req.contains("sec-fetch-site: same-origin"),
      "expected Sec-Fetch-Site same-origin for document redirect, got: {req}"
    );
    assert!(
      !req.contains("sec-fetch-user:"),
      "non-user document navigations should not set Sec-Fetch-User, got: {req}"
    );
  }

  #[test]
  fn http_fetcher_selects_headers_from_context_not_url() {
    let Some(listener) = try_bind_localhost("http_fetcher_selects_headers_from_context_not_url")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let barrier_server = Arc::clone(&barrier);

    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_req = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      barrier_server.wait();
      let start = Instant::now();
      let mut handled = 0usize;
      while handled < 2 && start.elapsed() < Duration::from_secs(3) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            stream
              .set_read_timeout(Some(Duration::from_millis(500)))
              .unwrap();
            let request = read_http_request(&mut stream)
              .unwrap()
              .expect("expected HTTP request");
            captured_req
              .lock()
              .unwrap()
              .push(String::from_utf8_lossy(&request).to_ascii_lowercase());

            let body = b"ok";
            let headers = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            stream.write_all(headers.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            handled += 1;
          }
          Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept http_fetcher_selects_headers_from_context_not_url: {err}"),
        }
      }
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let doc_url = format!("http://{}/font.woff2", addr);
    let font_url = format!("http://{}/index.html", addr);

    barrier.wait();
    let doc = fetcher
      .fetch_with_context(FetchContextKind::Document, &doc_url)
      .expect("document fetch");
    assert_eq!(doc.bytes, b"ok");
    let font = fetcher
      .fetch_with_context(FetchContextKind::Font, &font_url)
      .expect("font fetch");
    assert_eq!(font.bytes, b"ok");

    handle.join().unwrap();

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2, "expected two captured requests");

    let document_req = &captured[0];
    assert!(
      document_req.contains(&format!("accept: {DEFAULT_ACCEPT}").to_ascii_lowercase()),
      "expected document request to use HTML Accept header, got: {document_req}"
    );
    assert!(
      document_req.contains("sec-fetch-dest: document"),
      "expected document request sec-fetch-dest=document, got: {document_req}"
    );
    assert!(
      document_req.contains("sec-fetch-mode: navigate"),
      "expected document request sec-fetch-mode=navigate, got: {document_req}"
    );
    assert!(
      document_req.contains("sec-fetch-site: none"),
      "expected document request sec-fetch-site=none, got: {document_req}"
    );
    assert!(
      document_req.contains("sec-fetch-user: ?1"),
      "expected document request sec-fetch-user=?1, got: {document_req}"
    );
    assert!(
      document_req.contains("upgrade-insecure-requests: 1"),
      "expected document request upgrade-insecure-requests, got: {document_req}"
    );

    let font_req = &captured[1];
    assert!(
      font_req.contains("accept: */*"),
      "expected font request to use */* Accept header, got: {font_req}"
    );
    assert!(
      font_req.contains("sec-fetch-dest: font"),
      "expected font request sec-fetch-dest=font, got: {font_req}"
    );
    assert!(
      font_req.contains("sec-fetch-mode: cors"),
      "expected font request sec-fetch-mode=cors, got: {font_req}"
    );
    assert!(
      font_req.contains("sec-fetch-site: same-origin"),
      "expected font request sec-fetch-site=same-origin, got: {font_req}"
    );
    let origin = format!("http://127.0.0.1:{}", addr.port()).to_ascii_lowercase();
    assert!(
      font_req.contains(&format!("origin: {origin}")),
      "expected font request origin header, got: {font_req}"
    );
    assert!(
      font_req.contains(&format!("referer: {origin}/")),
      "expected font request referer header, got: {font_req}"
    );
  }

  #[test]
  fn http_fetcher_reuses_keep_alive_connections() {
    let Some(listener) = try_bind_localhost("http_fetcher_reuses_keep_alive_connections") else {
      return;
    };
    let addr = listener.local_addr().unwrap();

    let handle = thread::spawn(move || {
      let mut conn_count = 0;
      let mut handled = 0;

      while handled < 2 {
        let (mut stream, _) = listener.accept().unwrap();
        conn_count += 1;
        stream
          .set_read_timeout(Some(Duration::from_millis(500)))
          .unwrap();

        loop {
          match read_http_request(&mut stream) {
            Ok(Some(_)) => {
              let body = b"pong";
              let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                body.len()
              );
              stream.write_all(headers.as_bytes()).unwrap();
              stream.write_all(body).unwrap();
              handled += 1;
              if handled >= 2 {
                return conn_count;
              }
            }
            Ok(None) => break,
            Err(_) => break,
          }
        }
      }

      conn_count
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}/", addr);
    let first = fetcher.fetch(&url).expect("first fetch");
    assert_eq!(first.bytes, b"pong");
    let second = fetcher.fetch(&url).expect("second fetch");
    assert_eq!(second.bytes, b"pong");

    let conn_count = handle.join().unwrap();
    assert_eq!(
      conn_count, 1,
      "expected keep-alive reuse across fetches (got {conn_count})"
    );
  }

  fn read_http_request(stream: &mut TcpStream) -> io::Result<Option<Vec<u8>>> {
    let mut buf = [0u8; 1024];
    let mut request = Vec::new();
    loop {
      match stream.read(&mut buf) {
        Ok(0) => return Ok(None),
        Ok(n) => {
          request.extend_from_slice(&buf[..n]);
          if request.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(Some(request));
          }
        }
        Err(ref e)
          if matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
          ) =>
        {
          if request.is_empty() {
            return Ok(None);
          }
          return Err(io::Error::new(
            e.kind(),
            "incomplete HTTP request before timeout",
          ));
        }
        Err(e) => return Err(e),
      }
    }
  }

  #[test]
  fn http_fetch_does_not_run_past_render_deadline() {
    let Some(listener) = try_bind_localhost("http_fetch_does_not_run_past_render_deadline") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let barrier_server = Arc::clone(&barrier);

    let handle = thread::spawn(move || {
      barrier_server.wait();
      let start = Instant::now();
      while start.elapsed() < Duration::from_secs(2) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            stream
              .set_read_timeout(Some(Duration::from_millis(500)))
              .unwrap();
            let _ = read_http_request(&mut stream);

            // Delay the response well past the render deadline so the client must time out.
            thread::sleep(Duration::from_millis(200));

            let body = b"ok";
            let headers = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.write_all(body);
            return;
          }
          Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept http_fetch_does_not_run_past_render_deadline: {err}"),
        }
      }

      panic!(
        "server did not receive request within {:?}",
        start.elapsed()
      );
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(5));
    let url = format!("http://{}/", addr);

    barrier.wait();
    let deadline = render_control::RenderDeadline::new(Some(Duration::from_millis(50)), None);
    let start = Instant::now();
    let res = render_control::with_deadline(Some(&deadline), || fetcher.fetch(&url));
    let elapsed = start.elapsed();

    handle.join().unwrap();

    assert!(
      res.is_err(),
      "expected fetch to fail under render deadline, got: {res:?}"
    );
    assert!(
      elapsed < Duration::from_millis(150),
      "fetch should fail quickly under deadline (elapsed={elapsed:?})"
    );

    let err = res.unwrap_err().to_string().to_ascii_lowercase();
    assert!(
      err.contains("timeout") || err.contains("deadline"),
      "expected error to mention timeout/deadline, got: {err}"
    );
  }

  #[test]
  fn http_fetch_disables_retries_under_deadline() {
    let Some(listener) = try_bind_localhost("http_fetch_disables_retries_under_deadline") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let request_count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&request_count);

    let barrier = Arc::new(Barrier::new(2));
    let barrier_server = Arc::clone(&barrier);

    let handle = thread::spawn(move || {
      barrier_server.wait();
      let start = Instant::now();
      let mut last_request = None::<Instant>;

      while start.elapsed() < Duration::from_secs(2) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            seen.fetch_add(1, Ordering::SeqCst);
            last_request = Some(Instant::now());

            stream
              .set_read_timeout(Some(Duration::from_millis(500)))
              .unwrap();
            let _ = read_http_request(&mut stream);

            let body = b"unavailable";
            let headers = format!(
              "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.write_all(body);
          }
          Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
            if let Some(last) = last_request {
              if last.elapsed() > Duration::from_millis(250) {
                break;
              }
            }
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept http_fetch_disables_retries_under_deadline: {err}"),
        }
      }
    });

    let retry_policy = HttpRetryPolicy {
      max_attempts: 6,
      backoff_base: Duration::ZERO,
      backoff_cap: Duration::ZERO,
      respect_retry_after: false,
    };
    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(5))
      .with_retry_policy(retry_policy);
    let url = format!("http://{}/", addr);

    barrier.wait();
    let deadline = render_control::RenderDeadline::new(Some(Duration::from_millis(200)), None);
    let res = render_control::with_deadline(Some(&deadline), || fetcher.fetch(&url));
    assert!(
      res.is_err(),
      "expected 503 to surface as an error when retries are disabled, got: {res:?}"
    );

    handle.join().unwrap();

    assert_eq!(
      request_count.load(Ordering::SeqCst),
      1,
      "expected retries to be disabled under render deadline"
    );
  }

  #[test]
  fn http_fetch_cancel_callback_aborts_retries_and_backoff() {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    let Some(listener) =
      try_bind_localhost("http_fetch_cancel_callback_aborts_retries_and_backoff")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let request_count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&request_count);
    let (first_request_tx, first_request_rx) = mpsc::channel();

    let barrier = Arc::new(Barrier::new(2));
    let barrier_server = Arc::clone(&barrier);

    let handle = thread::spawn(move || {
      barrier_server.wait();
      let start = Instant::now();
      let mut last_request = None::<Instant>;

      while start.elapsed() < Duration::from_secs(2) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            let count = seen.fetch_add(1, Ordering::SeqCst).saturating_add(1);
            last_request = Some(Instant::now());

            stream
              .set_read_timeout(Some(Duration::from_millis(500)))
              .unwrap();
            let _ = read_http_request(&mut stream);

            let body = b"unavailable";
            let headers = format!(
              "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.write_all(body);

            if count == 1 {
              let _ = first_request_tx.send(());
            }
          }
          Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
            if let Some(last) = last_request {
              if last.elapsed() > Duration::from_millis(250) {
                break;
              }
            }
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept http_fetch_cancel_callback_aborts_retries_and_backoff: {err}"),
        }
      }
    });

    let retry_policy = HttpRetryPolicy {
      max_attempts: 2,
      backoff_base: Duration::from_secs(1),
      backoff_cap: Duration::from_secs(1),
      respect_retry_after: false,
    };
    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(5))
      .with_retry_policy(retry_policy);
    let url = format!("http://{addr}/style.css");

    barrier.wait();

    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_flag_worker = Arc::clone(&cancel_flag);
    let cancel: Arc<crate::render_control::CancelCallback> =
      Arc::new(move || cancel_flag_worker.load(Ordering::Relaxed));
    let deadline = render_control::RenderDeadline::new(None, Some(cancel));

    let canceller = {
      let cancel_flag = Arc::clone(&cancel_flag);
      thread::spawn(move || {
        let _ = first_request_rx.recv_timeout(Duration::from_secs(1));
        cancel_flag.store(true, Ordering::Relaxed);
      })
    };

    let start = Instant::now();
    let res = render_control::with_deadline(Some(&deadline), || {
      fetcher.fetch_with_context(FetchContextKind::Stylesheet, &url)
    });
    let elapsed = start.elapsed();

    canceller.join().unwrap();
    handle.join().unwrap();

    assert!(
      elapsed < Duration::from_millis(400),
      "fetch should abort quickly under cancel callback (elapsed={elapsed:?})"
    );

    let err = res.expect_err("expected fetch to fail under cancel callback");
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Css);
      }
      other => panic!("unexpected error: {other:?}"),
    }

    assert_eq!(
      request_count.load(Ordering::SeqCst),
      1,
      "expected cancellation to stop retries before starting another request"
    );
  }

  #[test]
  fn test_http_fetcher_defaults() {
    let fetcher = HttpFetcher::new();
    assert_eq!(fetcher.policy.request_timeout, Duration::from_secs(30));
    assert_eq!(fetcher.user_agent, DEFAULT_USER_AGENT);
  }

  #[test]
  fn test_http_fetcher_builder() {
    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(60))
      .with_user_agent("Test/1.0")
      .with_max_size(1024);

    assert_eq!(fetcher.policy.request_timeout, Duration::from_secs(60));
    assert_eq!(fetcher.user_agent, "Test/1.0");
    assert_eq!(fetcher.policy.max_response_bytes, 1024);
  }

  #[test]
  fn http_fetcher_rebuilds_agent_and_shares_across_clones() {
    let fetcher = HttpFetcher::new();
    let original_agent = Arc::clone(&fetcher.agent);

    let cloned = fetcher.clone();
    assert_eq!(Arc::as_ptr(&original_agent), Arc::as_ptr(&cloned.agent));

    let updated = fetcher.with_timeout(Duration::from_millis(50));
    assert_ne!(Arc::as_ptr(&original_agent), Arc::as_ptr(&updated.agent));
    assert_eq!(updated.policy.request_timeout, Duration::from_millis(50));

    let updated_clone = updated.clone();
    assert_eq!(
      Arc::as_ptr(&updated.agent),
      Arc::as_ptr(&updated_clone.agent)
    );

    let updated_agent = Arc::clone(&updated.agent);
    let policy = ResourcePolicy::default().with_request_timeout(Duration::from_millis(75));
    let rebuilt = updated.with_policy(policy);
    assert_eq!(rebuilt.policy.request_timeout, Duration::from_millis(75));
    assert_ne!(Arc::as_ptr(&updated_agent), Arc::as_ptr(&rebuilt.agent));
  }

  #[test]
  fn http_fetcher_is_send_and_sync() {
    fn assert_bounds<T: Send + Sync>() {}
    assert_bounds::<HttpFetcher>();
  }

  #[test]
  fn resource_policy_blocks_disallowed_scheme() {
    let fetcher = HttpFetcher::new().with_policy(
      ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    );
    let err = fetcher.fetch("http://example.test/").unwrap_err();
    assert!(
      err.to_string().contains("not allowed"),
      "unexpected error: {err:?}"
    );
  }

  #[test]
  fn resource_policy_rejects_empty_urls() {
    let fetcher = HttpFetcher::new();
    let err = fetcher.fetch("").unwrap_err();
    assert!(
      matches!(err, Error::Other(_)),
      "expected Error::Other for empty URL, got {err:?}"
    );
    assert!(
      err.to_string().contains("empty URL"),
      "unexpected error: {err:?}"
    );

    let err = fetcher.fetch(" \t ").unwrap_err();
    assert!(
      matches!(err, Error::Other(_)),
      "expected Error::Other for whitespace URL, got {err:?}"
    );
    assert!(
      err.to_string().contains("empty URL"),
      "unexpected error: {err:?}"
    );
  }

  #[test]
  fn resource_policy_blocks_denied_host() {
    let fetcher = HttpFetcher::new()
      .with_policy(ResourcePolicy::default().with_host_denylist(["example.test"]));
    let err = fetcher.fetch("http://example.test/blocked").unwrap_err();
    assert!(
      err.to_string().contains("denied"),
      "unexpected error: {err:?}"
    );
  }

  #[test]
  fn resource_policy_enforces_response_size_for_data_urls() {
    let fetcher =
      HttpFetcher::new().with_policy(ResourcePolicy::default().with_max_response_bytes(4));
    let err = fetcher
      .fetch("data:text/plain,hello")
      .expect_err("fetch should fail due to size limit");
    assert!(
      err.to_string().contains("response too large"),
      "unexpected error: {err:?}"
    );
  }

  #[test]
  fn fetch_partial_allows_oversized_data_url_prefixes() {
    let bytes = vec![42u8; 1024];
    let url =
      data_url::encode_base64_data_url("application/octet-stream", &bytes).expect("data url");
    let fetcher = HttpFetcher::new().with_max_size(64);
    assert!(
      fetcher.fetch(&url).is_err(),
      "full fetch should exceed max_size"
    );

    let res = fetcher
      .fetch_partial(&url, 16)
      .expect("partial fetch succeeds");
    assert_eq!(res.bytes, bytes[..16]);
  }

  #[test]
  fn fetch_partial_allows_oversized_files() {
    let bytes = vec![7u8; 1024];
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(&bytes).expect("write file");
    let url = format!("file://{}", file.path().display());

    let fetcher = HttpFetcher::new().with_max_size(64);
    assert!(
      fetcher.fetch(&url).is_err(),
      "full fetch should exceed max_size"
    );

    let res = fetcher
      .fetch_partial(&url, 16)
      .expect("partial fetch succeeds");
    assert_eq!(res.bytes, bytes[..16]);
  }

  #[test]
  fn file_fetch_ignores_query_and_fragment() {
    let bytes = b"hello".to_vec();
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(&bytes).expect("write file");
    let base = format!("file://{}", file.path().display());
    let url = format!("{base}?v=1#ignored");

    let fetcher = HttpFetcher::new();
    let res = fetcher.fetch(&url).expect("fetch with query/fragment");
    assert_eq!(res.bytes, bytes);

    let res = fetcher
      .fetch_partial(&url, 3)
      .expect("partial fetch with query/fragment");
    assert_eq!(res.bytes, bytes[..3]);
  }

  #[test]
  fn file_fetch_substitutes_placeholder_bytes_for_invalid_image_payloads() {
    let mut file = NamedTempFile::new().expect("temp file");
    file
      .write_all(b"<!DOCTYPE html><html><title>nope</title></html>")
      .expect("write html");
    let url = format!("file://{}", file.path().display());

    let fetcher = HttpFetcher::new();
    let res = fetcher
      .fetch_with_context(FetchContextKind::Image, &url)
      .expect("fetch image");
    assert_eq!(res.bytes, OFFLINE_FIXTURE_PLACEHOLDER_PNG);
    assert_eq!(
      res.content_type.as_deref(),
      Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME)
    );

    let partial = fetcher
      .fetch_partial_with_context(FetchContextKind::Image, &url, 8)
      .expect("fetch image prefix");
    assert_eq!(partial.bytes, OFFLINE_FIXTURE_PLACEHOLDER_PNG[..8]);
    assert_eq!(
      partial.content_type.as_deref(),
      Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME)
    );
  }

  #[test]
  fn data_url_fetch_substitutes_placeholder_bytes_for_empty_image_payloads() {
    let fetcher = HttpFetcher::new();
    let url = "data:image/gif;base64,";

    let res = fetcher
      .fetch_with_context(FetchContextKind::Image, url)
      .expect("fetch image data url");
    assert_eq!(res.bytes, OFFLINE_FIXTURE_PLACEHOLDER_PNG);
    assert_eq!(
      res.content_type.as_deref(),
      Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME)
    );

    let partial = fetcher
      .fetch_partial_with_context(FetchContextKind::Image, url, 8)
      .expect("fetch image prefix");
    assert_eq!(partial.bytes, OFFLINE_FIXTURE_PLACEHOLDER_PNG[..8]);
    assert_eq!(
      partial.content_type.as_deref(),
      Some(OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME)
    );
  }

  #[test]
  fn file_fetch_substitutes_placeholder_bytes_for_empty_font_payloads() {
    let file = NamedTempFile::new().expect("temp file");
    let url = format!("file://{}", file.path().display());

    let fetcher = HttpFetcher::new();
    let res = fetcher
      .fetch_with_context(FetchContextKind::Font, &url)
      .expect("fetch font");
    assert_eq!(res.bytes, OFFLINE_FIXTURE_PLACEHOLDER_WOFF2);
    assert_eq!(
      res.content_type.as_deref(),
      Some(OFFLINE_FIXTURE_PLACEHOLDER_WOFF2_MIME)
    );
  }

  #[test]
  fn resource_policy_tracks_total_budget() {
    let fetcher =
      HttpFetcher::new().with_policy(ResourcePolicy::default().with_total_bytes_budget(Some(5)));
    fetcher
      .fetch("data:text/plain,abc")
      .expect("first fetch within budget");
    let err = fetcher
      .fetch("data:text/plain,def")
      .expect_err("budget should be exhausted");
    assert!(
      err.to_string().contains("budget"),
      "unexpected error: {err:?}"
    );
  }

  #[test]
  fn caching_fetcher_counts_cached_hits_against_budget() {
    let policy = ResourcePolicy::default().with_total_bytes_budget(Some(5));
    let fetcher =
      CachingFetcher::new(HttpFetcher::new().with_policy(policy.clone())).with_policy(policy);
    let url = "data:text/plain,abcd";

    let first = fetcher.fetch(url).expect("first fetch within budget");
    assert_eq!(first.bytes, b"abcd");

    let err = fetcher
      .fetch(url)
      .expect_err("cached fetch should honor total budget");
    assert!(
      err.to_string().contains("budget"),
      "unexpected error: {err:?}"
    );
  }

  #[test]
  fn inflight_wait_respects_render_deadline() {
    use std::sync::mpsc;

    struct BlockingFetcher {
      started: Arc<Barrier>,
      release: Arc<Barrier>,
    }

    impl ResourceFetcher for BlockingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        if url == "https://example.com/blocked" {
          self.started.wait();
          self.release.wait();
          return Ok(FetchedResource::new(
            b"ok".to_vec(),
            Some("text/plain".to_string()),
          ));
        }

        Err(Error::Resource(ResourceError::new(
          url.to_string(),
          "unexpected url",
        )))
      }
    }

    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let url = "https://example.com/blocked".to_string();
    let fetcher = Arc::new(CachingFetcher::new(BlockingFetcher {
      started: Arc::clone(&started),
      release: Arc::clone(&release),
    }));

    let fetcher_owner = Arc::clone(&fetcher);
    let url_owner = url.clone();
    let owner_handle = thread::spawn(move || fetcher_owner.fetch(&url_owner));

    // Wait until the owner thread has started the inner fetch and registered the in-flight entry.
    started.wait();

    let fetcher_waiter = Arc::clone(&fetcher);
    let url_waiter = url.clone();
    let (tx, rx) = mpsc::channel();
    let waiter_handle = thread::spawn(move || {
      let deadline = render_control::RenderDeadline::new(Some(Duration::from_millis(50)), None);
      let start = Instant::now();
      let result =
        render_control::with_deadline(Some(&deadline), || fetcher_waiter.fetch(&url_waiter));
      let elapsed = start.elapsed();
      tx.send((result, elapsed)).unwrap();
    });

    let (result, elapsed) = match rx.recv_timeout(Duration::from_secs(1)) {
      Ok(value) => value,
      Err(err) => {
        // Unblock the owner thread so the test can terminate even if the waiter path is broken.
        release.wait();
        let _ = owner_handle.join();
        waiter_handle.join().unwrap();
        panic!("waiter fetch did not complete under deadline: {err}");
      }
    };

    let err = result.expect_err("waiter fetch should fail under deadline");
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Paint);
      }
      other => panic!("unexpected error after {elapsed:?}: {other:?}"),
    }

    // Let the owner thread finish so it can clean up the in-flight entry.
    release.wait();

    let owner_result = owner_handle.join().unwrap();
    assert!(
      owner_result.is_ok(),
      "owner fetch should complete after being released: {owner_result:?}"
    );
    waiter_handle.join().unwrap();
  }

  #[test]
  fn inflight_wait_respects_cancel_callback() {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    struct BlockingFetcher {
      started: Arc<Barrier>,
      release: Arc<Barrier>,
    }

    impl ResourceFetcher for BlockingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        if url == "https://example.com/blocked" {
          self.started.wait();
          self.release.wait();
          return Ok(FetchedResource::new(
            b"ok".to_vec(),
            Some("text/plain".to_string()),
          ));
        }

        Err(Error::Resource(ResourceError::new(
          url.to_string(),
          "unexpected url",
        )))
      }
    }

    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let url = "https://example.com/blocked".to_string();
    let fetcher = Arc::new(CachingFetcher::new(BlockingFetcher {
      started: Arc::clone(&started),
      release: Arc::clone(&release),
    }));

    let fetcher_owner = Arc::clone(&fetcher);
    let url_owner = url.clone();
    let owner_handle = thread::spawn(move || fetcher_owner.fetch(&url_owner));

    started.wait();

    let fetcher_waiter = Arc::clone(&fetcher);
    let url_waiter = url.clone();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_flag_worker = Arc::clone(&cancel_flag);
    let cancel: Arc<crate::render_control::CancelCallback> =
      Arc::new(move || cancel_flag_worker.load(Ordering::Relaxed));
    let (tx, rx) = mpsc::channel();
    let waiter_handle = thread::spawn(move || {
      let deadline = render_control::RenderDeadline::new(None, Some(cancel));
      let start = Instant::now();
      let result =
        render_control::with_deadline(Some(&deadline), || fetcher_waiter.fetch(&url_waiter));
      let elapsed = start.elapsed();
      tx.send((result, elapsed)).unwrap();
    });

    thread::sleep(Duration::from_millis(50));
    cancel_flag.store(true, Ordering::Relaxed);

    let (result, elapsed) = match rx.recv_timeout(Duration::from_secs(1)) {
      Ok(value) => value,
      Err(err) => {
        release.wait();
        let _ = owner_handle.join();
        waiter_handle.join().unwrap();
        panic!("waiter fetch did not complete under cancel: {err}");
      }
    };

    let err = result.expect_err("waiter fetch should fail under cancel callback");
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Paint);
      }
      other => panic!("unexpected error after {elapsed:?}: {other:?}"),
    }

    release.wait();

    let owner_result = owner_handle.join().unwrap();
    assert!(
      owner_result.is_ok(),
      "owner fetch should complete after being released: {owner_result:?}"
    );
    waiter_handle.join().unwrap();
  }

  #[test]
  fn http_fetch_deadline_exceeded_before_request_surfaces_as_timeout() {
    let fetcher = HttpFetcher::new();
    let url = "http://example.com/style.css";
    let deadline = render_control::RenderDeadline::new(Some(Duration::from_millis(0)), None);
    let err = render_control::with_deadline(Some(&deadline), || fetcher.fetch(url))
      .expect_err("expected deadline-exceeded fetch to fail");
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Css);
      }
      other => panic!("expected timeout error, got {other:?}"),
    }
  }

  #[test]
  fn http_fetch_cancel_callback_surfaces_as_timeout() {
    let fetcher = HttpFetcher::new();
    let url = "http://example.com/style.css";
    let cancel: Arc<crate::render_control::CancelCallback> = Arc::new(|| true);
    let deadline = render_control::RenderDeadline::new(None, Some(cancel));
    let err = render_control::with_deadline(Some(&deadline), || fetcher.fetch(url))
      .expect_err("expected cancelled fetch to fail");
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Css);
      }
      other => panic!("expected timeout error, got {other:?}"),
    }
  }

  #[test]
  fn test_fetch_data_url() {
    let fetcher = HttpFetcher::new();
    let resource = fetcher.fetch("data:text/plain,test").unwrap();
    assert_eq!(resource.bytes, b"test");
  }

  #[test]
  fn http_fetcher_retries_with_identity_accept_encoding_on_decompression_error() {
    use std::time::{Duration, Instant};

    let Some(listener) = try_bind_localhost(
      "http_fetcher_retries_with_identity_accept_encoding_on_decompression_error",
    ) else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let captured = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let captured_headers = Arc::clone(&captured);
    let handle = thread::spawn(move || {
      let mut handled = 0;
      let start = Instant::now();
      while handled < 2 && start.elapsed() < Duration::from_secs(5) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            let mut buf = [0u8; 1024];
            let mut req = Vec::new();
            loop {
              match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                  req.extend_from_slice(&buf[..n]);
                  if req.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                  }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                  thread::sleep(Duration::from_millis(10));
                  continue;
                }
                Err(_) => break,
              }
            }

            let req_str = String::from_utf8_lossy(&req);
            let headers: Vec<String> = req_str
              .lines()
              .filter(|line| line.to_ascii_lowercase().starts_with("accept-encoding:"))
              .map(|line| line.trim().to_ascii_lowercase())
              .collect();
            captured_headers.lock().unwrap().push(headers);

            if handled == 0 {
              let body = b"not brotli";
              let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Encoding: br\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
              );
              let _ = stream.write_all(response.as_bytes());
              let _ = stream.write_all(body);
            } else {
              let body = b"plain body";
              let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
              );
              let _ = stream.write_all(response.as_bytes());
              let _ = stream.write_all(body);
            }

            handled += 1;
          }
          Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(10));
          }
          Err(_) => break,
        }
      }
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}", addr);
    let res = fetcher
      .fetch(&url)
      .expect("fetch after decompression retry");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"plain body");

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.len(),
      2,
      "decompression error should trigger a retry with a second request: {:?}",
      *captured
    );

    let initial_headers = &captured[0];
    assert_eq!(
      initial_headers,
      &vec!["accept-encoding: gzip, deflate, br".to_string()],
      "default request should advertise gzip/deflate/br"
    );

    let retry_headers = &captured[1];
    assert_eq!(
      retry_headers.len(),
      1,
      "retry should send exactly one Accept-Encoding header: {:?}",
      retry_headers
    );
    assert_eq!(
      retry_headers[0], "accept-encoding: identity",
      "retry Accept-Encoding should be identity"
    );
  }

  #[test]
  fn fetch_http_decodes_brotli_body() {
    let Some(listener) = try_bind_localhost("fetch_http_decodes_brotli_body") else {
      return;
    };
    let addr = listener.local_addr().unwrap();

    let handle = std::thread::spawn(move || {
      if let Some(stream) = listener.incoming().next() {
        let mut stream = stream.unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);

        let body = b"hello brotli";
        let mut compressed = Vec::new();
        {
          let mut writer = CompressorWriter::new(&mut compressed, 4096, 5, 22);
          writer.write_all(body).unwrap();
          writer.flush().unwrap();
        }

        let headers = format!(
          "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Encoding: br\r\nContent-Length: {}\r\n\r\n",
          compressed.len()
        );
        let _ = stream.write_all(headers.as_bytes());
        let _ = stream.write_all(&compressed);
      }
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}", addr);
    let res = fetcher.fetch(&url).expect("brotli fetch");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"hello brotli");
  }

  #[test]
  fn fetch_http_decodes_gzip_body() {
    let Some(listener) = try_bind_localhost("fetch_http_decodes_gzip_body") else {
      return;
    };
    let addr = listener.local_addr().unwrap();

    let handle = std::thread::spawn(move || {
      if let Some(stream) = listener.incoming().next() {
        let mut stream = stream.unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);

        let body = b"hello gzip";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(body).unwrap();
        let compressed = encoder.finish().unwrap();

        let headers = format!(
          "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
          compressed.len()
        );
        let _ = stream.write_all(headers.as_bytes());
        let _ = stream.write_all(&compressed);
      }
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}", addr);
    let res = fetcher.fetch(&url).expect("gzip fetch");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"hello gzip");
  }

  #[test]
  fn fetch_http_retries_on_bad_gzip() {
    use std::io::Read;
    use std::io::Write;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

    let Some(listener) = try_bind_localhost("fetch_http_retries_on_bad_gzip") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let handle = thread::spawn(move || {
      let mut handled = 0;
      let start = Instant::now();
      while handled < 2 && start.elapsed() < Duration::from_secs(5) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            let mut buf = [0u8; 1024];
            let mut req = Vec::new();
            loop {
              match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                  req.extend_from_slice(&buf[..n]);
                  if req.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                  }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                  thread::sleep(Duration::from_millis(10));
                  continue;
                }
                Err(_) => break,
              }
            }

            let req_str = String::from_utf8_lossy(&req).to_lowercase();
            let body = b"hello world";
            let encoding_header = if req_str.contains("accept-encoding: identity") {
              ""
            } else {
              "Content-Encoding: gzip\r\n"
            };
            let response = format!(
                            "HTTP/1.1 200 OK\r\n{}Content-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
                            encoding_header,
                            body.len()
                        );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            handled += 1;
          }
          Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(10));
          }
          Err(_) => break,
        }
      }
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}", addr);
    let resource = fetcher.fetch(&url).unwrap();
    assert_eq!(resource.bytes, b"hello world");

    handle.join().unwrap();
  }

  #[test]
  fn fetch_http_retries_on_bad_brotli() {
    use std::io::Read;
    use std::io::Write;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

    let Some(listener) = try_bind_localhost("fetch_http_retries_on_bad_brotli") else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let handle = thread::spawn(move || {
      let mut handled = 0;
      let start = Instant::now();
      while handled < 2 && start.elapsed() < Duration::from_secs(5) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            let mut buf = [0u8; 1024];
            let mut req = Vec::new();
            loop {
              match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                  req.extend_from_slice(&buf[..n]);
                  if req.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                  }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                  thread::sleep(Duration::from_millis(10));
                  continue;
                }
                Err(_) => break,
              }
            }

            let req_str = String::from_utf8_lossy(&req).to_lowercase();
            let body = b"hello brotli";
            let encoding_header = if req_str.contains("accept-encoding: identity") {
              ""
            } else {
              "Content-Encoding: br\r\n"
            };
            let response = format!(
              "HTTP/1.1 200 OK\r\n{}Content-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
              encoding_header,
              body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            handled += 1;
          }
          Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(10));
          }
          Err(_) => break,
        }
      }
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}", addr);
    let resource = fetcher.fetch(&url).unwrap();
    assert_eq!(resource.bytes, b"hello brotli");

    handle.join().unwrap();
  }

  #[test]
  fn http_fetcher_allows_204_empty_body() {
    let Some(listener) = try_bind_localhost("http_fetcher_allows_204_empty_body") else {
      return;
    };
    let addr = listener.local_addr().unwrap();

    let handle = thread::spawn(move || {
      if let Some(stream) = listener.incoming().next() {
        let mut stream = stream.unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let headers = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(headers);
      }
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}", addr);
    let res = fetcher.fetch(&url).expect("fetch 204");
    handle.join().unwrap();

    assert!(res.bytes.is_empty());
    assert_eq!(res.status, Some(204));
  }

  #[test]
  fn http_fetcher_retries_202_empty_body_then_succeeds() {
    let Some(listener) = try_bind_localhost("http_fetcher_retries_202_empty_body_then_succeeds")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();

    let handle = thread::spawn(move || {
      for (idx, stream) in listener.incoming().take(2).enumerate() {
        let mut stream = stream.unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);

        if idx == 0 {
          let headers =
            b"HTTP/1.1 202 Accepted\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
          let _ = stream.write_all(headers);
        } else {
          let body = b"ok";
          let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
          );
          stream.write_all(headers.as_bytes()).unwrap();
          stream.write_all(body).unwrap();
        }
      }
    });

    let retry_policy = HttpRetryPolicy {
      max_attempts: 2,
      backoff_base: Duration::ZERO,
      backoff_cap: Duration::ZERO,
      respect_retry_after: true,
    };
    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(retry_policy);
    let url = format!("http://{}", addr);
    let res = fetcher.fetch(&url).expect("fetch after 202 retry");
    handle.join().unwrap();

    assert_eq!(res.bytes, b"ok");
    assert_eq!(res.status, Some(200));
  }

  #[test]
  fn http_fetcher_returns_ok_on_persistent_202_empty_body_after_retries() {
    let Some(listener) =
      try_bind_localhost("http_fetcher_returns_ok_on_persistent_202_empty_body_after_retries")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();

    let handle = thread::spawn(move || {
      for stream in listener.incoming().take(2) {
        let mut stream = stream.unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let headers =
          b"HTTP/1.1 202 Accepted\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(headers);
      }
    });

    let retry_policy = HttpRetryPolicy {
      max_attempts: 2,
      backoff_base: Duration::ZERO,
      backoff_cap: Duration::ZERO,
      respect_retry_after: true,
    };
    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(retry_policy);
    let url = format!("http://{}", addr);
    let res = fetcher
      .fetch(&url)
      .expect("persistent 202 empty body should return Ok");
    handle.join().unwrap();

    assert!(res.bytes.is_empty());
    assert_eq!(res.status, Some(202));
  }

  #[test]
  fn http_fetcher_returns_ok_on_persistent_429_after_retries() {
    let Some(listener) =
      try_bind_localhost("http_fetcher_returns_ok_on_persistent_429_after_retries")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();
    let body = b"too many";

    let handle = thread::spawn(move || {
      for stream in listener.incoming().take(2) {
        let mut stream = stream.unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let headers = format!(
          "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
          body.len()
        );
        let _ = stream.write_all(headers.as_bytes());
        let _ = stream.write_all(body);
      }
    });

    let retry_policy = HttpRetryPolicy {
      max_attempts: 2,
      backoff_base: Duration::ZERO,
      backoff_cap: Duration::ZERO,
      respect_retry_after: true,
    };
    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(retry_policy);
    let url = format!("http://{}/", addr);
    let res = fetcher
      .fetch(&url)
      .expect("persistent 429 should return Ok after retries are exhausted");
    handle.join().unwrap();

    assert_eq!(res.bytes, body);
    assert_eq!(res.status, Some(429));
  }

  #[test]
  fn fetch_http_errors_on_empty_body() {
    use std::io::Write;

    let Some(listener) = try_bind_localhost("fetch_http_errors_on_empty_body") else {
      return;
    };
    let addr = listener.local_addr().unwrap();

    let handle = std::thread::spawn(move || {
      if let Some(stream) = listener.incoming().next() {
        let mut stream = stream.unwrap();
        let headers = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 0\r\n\r\n";
        let _ = stream.write_all(headers);
      }
    });

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let url = format!("http://{}", addr);
    let res = fetcher.fetch(&url);
    assert!(res.is_err(), "expected empty response to error: {res:?}");

    handle.join().unwrap();
  }

  #[test]
  fn fetch_http_honors_retry_after_over_backoff_cap() {
    let Some(listener) = try_bind_localhost("fetch_http_honors_retry_after_over_backoff_cap")
    else {
      return;
    };
    let addr = listener.local_addr().unwrap();

    let handle = std::thread::spawn(move || {
      let mut iter = listener.incoming();

      // First response: 429 with Retry-After.
      if let Some(stream) = iter.next() {
        let mut stream = stream.unwrap();
        let _ = stream.read(&mut [0u8; 1024]);
        let body = b"too many";
        let headers = format!(
          "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
          body.len()
        );
        let _ = stream.write_all(headers.as_bytes());
        let _ = stream.write_all(body);
      }

      // Second response: OK.
      if let Some(stream) = iter.next() {
        let mut stream = stream.unwrap();
        let _ = stream.read(&mut [0u8; 1024]);
        let body = b"ok";
        let headers = format!(
          "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
          body.len()
        );
        let _ = stream.write_all(headers.as_bytes());
        let _ = stream.write_all(body);
      }
    });

    // Use a tiny exponential backoff so the only way to sleep ~1s is via Retry-After.
    let retry_policy = HttpRetryPolicy {
      max_attempts: 2,
      backoff_base: Duration::from_millis(1),
      backoff_cap: Duration::from_millis(5),
      respect_retry_after: true,
    };
    let fetcher = HttpFetcher::new()
      .with_timeout(Duration::from_secs(2))
      .with_retry_policy(retry_policy);

    let url = format!("http://{}/", addr);
    let start = Instant::now();
    let res = fetcher.fetch(&url).expect("fetch should retry after 429");
    let elapsed = start.elapsed();
    handle.join().unwrap();

    assert_eq!(res.bytes, b"ok");
    assert!(
      elapsed >= Duration::from_millis(900),
      "expected Retry-After delay to be respected (elapsed={elapsed:?})"
    );
    assert!(
      elapsed < Duration::from_secs(10),
      "unexpectedly slow retry-after test (elapsed={elapsed:?})"
    );
  }

  #[test]
  fn normalize_user_agent_for_log_strips_prefix() {
    assert_eq!(normalize_user_agent_for_log("User-Agent: Foo"), "Foo");
    assert_eq!(normalize_user_agent_for_log("Foo"), "Foo");
    assert_eq!(normalize_user_agent_for_log(""), "");
  }

  #[derive(Clone)]
  struct CountingFetcher {
    count: Arc<AtomicUsize>,
    body: Vec<u8>,
  }

  impl ResourceFetcher for CountingFetcher {
    fn fetch(&self, _url: &str) -> Result<FetchedResource> {
      self.count.fetch_add(1, Ordering::SeqCst);
      Ok(FetchedResource::new(
        self.body.clone(),
        Some("text/plain".to_string()),
      ))
    }
  }

  #[test]
  fn caching_fetcher_hits_cache_and_evicts() {
    let counter = Arc::new(AtomicUsize::new(0));
    let inner = CountingFetcher {
      count: Arc::clone(&counter),
      body: b"hello".to_vec(),
    };

    let cache = CachingFetcher::new(inner)
      .with_max_items(1)
      .with_max_bytes(8);

    let _ = cache.fetch("http://example.com/a").unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // Second fetch should hit cache
    let _ = cache.fetch("http://example.com/a").unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // Different URL should evict previous entry due to max_items = 1
    let _ = cache.fetch("http://example.com/b").unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    // First URL should require a re-fetch after eviction
    let _ = cache.fetch("http://example.com/a").unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 3);
  }

  #[test]
  fn caching_fetcher_coalesces_inflight_requests() {
    #[derive(Clone)]
    struct SlowFetcher {
      count: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for SlowFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        self.count.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(50));
        Ok(FetchedResource::new(
          b"ok".to_vec(),
          Some("text/plain".to_string()),
        ))
      }
    }

    let counter = Arc::new(AtomicUsize::new(0));
    let fetcher = SlowFetcher {
      count: Arc::clone(&counter),
    };
    let cache = Arc::new(CachingFetcher::new(fetcher));
    let barrier = Arc::new(Barrier::new(4));

    let mut handles = Vec::new();
    for _ in 0..4 {
      let f = Arc::clone(&cache);
      let b = Arc::clone(&barrier);
      handles.push(thread::spawn(move || {
        b.wait();
        f.fetch("http://example.com/shared").unwrap();
      }));
    }

    for handle in handles {
      handle.join().unwrap();
    }

    assert_eq!(counter.load(Ordering::SeqCst), 1);
  }

  #[test]
  fn caching_fetcher_can_cache_errors() {
    #[derive(Clone)]
    struct ErrorFetcher {
      count: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for ErrorFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Err(Error::Other("boom".to_string()))
      }

      fn request_header_value(&self, _req: FetchRequest<'_>, header_name: &str) -> Option<String> {
        if header_name.eq_ignore_ascii_case("origin")
          || header_name.eq_ignore_ascii_case("accept-encoding")
          || header_name.eq_ignore_ascii_case("accept-language")
          || header_name.eq_ignore_ascii_case("user-agent")
          || header_name.eq_ignore_ascii_case("referer")
        {
          return Some(String::new());
        }
        None
      }
    }

    let counter = Arc::new(AtomicUsize::new(0));
    let fetcher = ErrorFetcher {
      count: Arc::clone(&counter),
    };
    let cache = CachingFetcher::new(fetcher).with_cache_errors(true);

    assert!(cache.fetch("http://example.com/error").is_err());
    assert!(cache.fetch("http://example.com/error").is_err());
    assert_eq!(counter.load(Ordering::SeqCst), 1);
  }

  #[test]
  fn caching_fetcher_partitions_cached_errors_by_referrer_for_request_api() {
    #[derive(Clone)]
    struct ReferrerAwareFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for ReferrerAwareFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected request-aware fetch");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match req.referrer_url {
          Some("https://a.test/") => Err(Error::Other("blocked".to_string())),
          Some("https://b.test/") => {
            let mut res = FetchedResource::new(b"ok".to_vec(), Some("text/plain".to_string()));
            res.final_url = Some(req.url.to_string());
            Ok(res)
          }
          other => panic!("unexpected referrer in fetch_with_request: {other:?}"),
        }
      }

      fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
        if header_name.eq_ignore_ascii_case("referer") {
          return Some(req.referrer_url.unwrap_or("").to_string());
        }
        if header_name.eq_ignore_ascii_case("origin")
          || header_name.eq_ignore_ascii_case("accept-encoding")
          || header_name.eq_ignore_ascii_case("accept-language")
          || header_name.eq_ignore_ascii_case("user-agent")
        {
          return Some(String::new());
        }
        None
      }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let cache = CachingFetcher::new(ReferrerAwareFetcher {
      calls: Arc::clone(&calls),
    })
    .with_cache_errors(true);
    let url = "https://example.com/image.png";
    let req_a = FetchRequest::new(url, FetchDestination::Image).with_referrer_url("https://a.test/");
    let req_b = FetchRequest::new(url, FetchDestination::Image).with_referrer_url("https://b.test/");

    assert!(cache.fetch_with_request(req_a).is_err());
    assert!(cache.fetch_with_request(req_a).is_err(), "should hit cached error");
    assert_eq!(
      calls.load(Ordering::SeqCst),
      1,
      "expected repeated request to reuse cached error"
    );

    let res = cache
      .fetch_with_request(req_b)
      .expect("different referrer should not reuse cached error");
    assert_eq!(res.bytes, b"ok".to_vec());
    assert_eq!(
      calls.load(Ordering::SeqCst),
      2,
      "expected different referrer to re-fetch"
    );
  }

  #[test]
  fn caching_fetcher_partitions_cached_errors_by_referrer_for_context_api() {
    #[derive(Clone)]
    struct ContextReferrerAwareFetcher {
      calls: Arc<AtomicUsize>,
      referrer: Arc<Mutex<Option<String>>>,
    }

    impl ResourceFetcher for ContextReferrerAwareFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected context-aware fetch");
      }

      fn fetch_with_context(&self, _kind: FetchContextKind, url: &str) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let referrer = self.referrer.lock().unwrap().clone();
        match referrer.as_deref() {
          Some("https://a.test/") => Err(Error::Other("blocked".to_string())),
          Some("https://b.test/") => {
            let mut res = FetchedResource::new(b"ok".to_vec(), Some("text/plain".to_string()));
            res.final_url = Some(url.to_string());
            Ok(res)
          }
          other => panic!("unexpected referrer in fetch_with_context: {other:?}"),
        }
      }

      fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
        if header_name.eq_ignore_ascii_case("referer") {
          return Some(
            req
              .referrer_url
              .map(str::to_string)
              .or_else(|| self.referrer.lock().unwrap().clone())
              .unwrap_or_default(),
          );
        }
        if header_name.eq_ignore_ascii_case("origin")
          || header_name.eq_ignore_ascii_case("accept-encoding")
          || header_name.eq_ignore_ascii_case("accept-language")
          || header_name.eq_ignore_ascii_case("user-agent")
        {
          return Some(String::new());
        }
        None
      }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let referrer = Arc::new(Mutex::new(None::<String>));
    let cache = CachingFetcher::new(ContextReferrerAwareFetcher {
      calls: Arc::clone(&calls),
      referrer: Arc::clone(&referrer),
    })
    .with_cache_errors(true);
    let url = "https://example.com/image.png";

    *referrer.lock().unwrap() = Some("https://a.test/".to_string());
    assert!(cache
      .fetch_with_context(FetchContextKind::Image, url)
      .is_err());
    assert!(cache
      .fetch_with_context(FetchContextKind::Image, url)
      .is_err());
    assert_eq!(
      calls.load(Ordering::SeqCst),
      1,
      "expected repeated context request to reuse cached error"
    );

    *referrer.lock().unwrap() = Some("https://b.test/".to_string());
    let res = cache
      .fetch_with_context(FetchContextKind::Image, url)
      .expect("different referrer should not reuse cached error");
    assert_eq!(res.bytes, b"ok".to_vec());
    assert_eq!(
      calls.load(Ordering::SeqCst),
      2,
      "expected different context referrer to re-fetch"
    );
  }

  #[test]
  fn caching_fetcher_fetch_partial_delegates_to_inner_partial() {
    #[derive(Clone)]
    struct PartialOnlyFetcher;

    impl ResourceFetcher for PartialOnlyFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("fetch() should not be called for partial fetches");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        url: &str,
        max_bytes: usize,
      ) -> Result<FetchedResource> {
        let mut res = FetchedResource::new(b"0123456789".to_vec(), Some("text/plain".to_string()));
        res.final_url = Some(url.to_string());
        if res.bytes.len() > max_bytes {
          res.bytes.truncate(max_bytes);
        }
        Ok(res)
      }
    }

    let cache = CachingFetcher::new(PartialOnlyFetcher);
    let url = "http://example.com/partial";
    let res = cache
      .fetch_partial_with_context(FetchContextKind::Image, url, 4)
      .expect("partial fetch");
    assert_eq!(res.bytes, b"0123");
  }

  #[test]
  fn caching_fetcher_fetch_partial_does_not_populate_full_cache() {
    #[derive(Clone)]
    struct PartialCountingFetcher {
      fetch_calls: Arc<AtomicUsize>,
      partial_calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for PartialCountingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_calls.fetch_add(1, Ordering::SeqCst);
        let mut res = FetchedResource::new(b"full-body".to_vec(), Some("text/plain".to_string()));
        res.final_url = Some(url.to_string());
        Ok(res)
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        url: &str,
        max_bytes: usize,
      ) -> Result<FetchedResource> {
        self.partial_calls.fetch_add(1, Ordering::SeqCst);
        let mut res =
          FetchedResource::new(b"partial-body".to_vec(), Some("text/plain".to_string()));
        res.final_url = Some(url.to_string());
        if res.bytes.len() > max_bytes {
          res.bytes.truncate(max_bytes);
        }
        Ok(res)
      }
    }

    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let partial_calls = Arc::new(AtomicUsize::new(0));
    let cache = CachingFetcher::new(PartialCountingFetcher {
      fetch_calls: Arc::clone(&fetch_calls),
      partial_calls: Arc::clone(&partial_calls),
    });

    let url = "http://example.com/partial-cache";
    let partial = cache
      .fetch_partial_with_context(FetchContextKind::Image, url, 7)
      .expect("partial fetch");
    assert_eq!(partial.bytes, b"partial");
    assert_eq!(partial_calls.load(Ordering::SeqCst), 1);

    let full = cache
      .fetch_with_context(FetchContextKind::Image, url)
      .expect("full fetch");
    assert_eq!(full.bytes, b"full-body");
    assert_eq!(
      fetch_calls.load(Ordering::SeqCst),
      1,
      "full fetch should still hit inner fetcher after partial fetch"
    );
  }

  #[derive(Clone, Debug)]
  struct RecordedFetchRequestCall {
    destination: FetchDestination,
    referrer_url: Option<String>,
  }

  #[derive(Clone)]
  struct RequestRecordingFetcher {
    calls: Arc<Mutex<Vec<RecordedFetchRequestCall>>>,
  }

  impl ResourceFetcher for RequestRecordingFetcher {
    fn fetch(&self, _url: &str) -> Result<FetchedResource> {
      panic!("fetch() should not be called when using fetch_with_request()");
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
      self.calls.lock().unwrap().push(RecordedFetchRequestCall {
        destination: req.destination,
        referrer_url: req.referrer_url.map(|s| s.to_string()),
      });
      Ok(FetchedResource::new(
        b"ok".to_vec(),
        Some("text/plain".to_string()),
      ))
    }
  }

  #[test]
  fn caching_fetcher_forwards_fetch_request_context() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let inner = RequestRecordingFetcher {
      calls: Arc::clone(&calls),
    };
    let cache = CachingFetcher::new(inner);

    let url = "http://example.com/asset.css";
    let req =
      FetchRequest::new(url, FetchDestination::Style).with_referrer_url("http://example.com/");
    let res = cache.fetch_with_request(req).expect("fetch");
    assert_eq!(res.bytes, b"ok");

    let snapshot = calls.lock().unwrap().clone();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].destination, FetchDestination::Style);
    assert_eq!(
      snapshot[0].referrer_url.as_deref(),
      Some("http://example.com/")
    );

    // Cache key ignores referrer: a subsequent request with the same destination but a different
    // referrer should still hit the cache and avoid a second network call.
    let req2 =
      FetchRequest::new(url, FetchDestination::Style).with_referrer_url("http://other.example/");
    let res = cache.fetch_with_request(req2).expect("cached fetch");
    assert_eq!(res.bytes, b"ok");
    assert_eq!(
      calls.lock().unwrap().len(),
      1,
      "expected request fetches with differing referrers to share a cache entry"
    );

    // Cache key includes destination: a request for the same URL with a different destination is
    // fetched separately so differing header profiles (e.g. Accept/Sec-Fetch-*) can't poison each
    // other.
    let req3 =
      FetchRequest::new(url, FetchDestination::Image).with_referrer_url("http://other.example/");
    let res = cache.fetch_with_request(req3).expect("second fetch");
    assert_eq!(res.bytes, b"ok");
    let snapshot = calls.lock().unwrap().clone();
    assert_eq!(snapshot.len(), 2);
    assert_eq!(snapshot[1].destination, FetchDestination::Image);
    assert_eq!(
      snapshot[1].referrer_url.as_deref(),
      Some("http://other.example/")
    );
  }

  #[test]
  fn caching_fetcher_partitions_cors_mode_entries_by_origin_when_enforced() {
    #[derive(Clone)]
    struct OriginEchoFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for OriginEchoFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected CachingFetcher to use fetch_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let origin = cors_origin_key_from_client_origin(req.client_origin).unwrap_or_default();
        let mut res = FetchedResource::new(origin.into_bytes(), Some("text/plain".to_string()));
        res.final_url = Some(req.url.to_string());
        Ok(res)
      }
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_ENFORCE_CORS".to_string(),
      "1".to_string(),
    )])));

    runtime::with_thread_runtime_toggles(toggles, || {
      let calls = Arc::new(AtomicUsize::new(0));
      let cache = CachingFetcher::new(OriginEchoFetcher {
        calls: Arc::clone(&calls),
      });

      for (destination, url) in [
        (FetchDestination::Font, "http://example.com/font.woff2"),
        (FetchDestination::ImageCors, "http://example.com/image.png"),
      ] {
        let origin_a = origin_from_url("http://a.test/page").expect("origin");
        let origin_b = origin_from_url("http://b.test/page").expect("origin");
        let req_a = FetchRequest::new(url, destination)
          .with_client_origin(&origin_a)
          .with_referrer_url("http://a.test/page");
        let req_b = FetchRequest::new(url, destination)
          .with_client_origin(&origin_b)
          .with_referrer_url("http://b.test/page");

        let first_a = cache.fetch_with_request(req_a).expect("fetch A");
        assert_eq!(first_a.bytes, b"http://a.test");
        let first_b = cache.fetch_with_request(req_b).expect("fetch B");
        assert_eq!(first_b.bytes, b"http://b.test");

        // Ensure fetching `B` doesn't overwrite `A` and vice versa.
        let second_a = cache.fetch_with_request(req_a).expect("cache A");
        assert_eq!(second_a.bytes, b"http://a.test");
        let second_b = cache.fetch_with_request(req_b).expect("cache B");
        assert_eq!(second_b.bytes, b"http://b.test");
      }

      assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "expected differing referrer origins to produce distinct cache entries for CORS-mode requests when CORS enforcement is enabled"
      );
    });
  }

  #[test]
  fn caching_fetcher_fetch_partial_with_request_uses_origin_partitioned_cache_keys() {
    #[derive(Clone)]
    struct OriginEchoFetcher {
      full_calls: Arc<AtomicUsize>,
      partial_calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for OriginEchoFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected CachingFetcher to use fetch_with_request / fetch_partial_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.full_calls.fetch_add(1, Ordering::SeqCst);
        let origin = cors_origin_key_from_client_origin(req.client_origin).unwrap_or_default();
        let mut res = FetchedResource::new(
          format!("full:{origin}").into_bytes(),
          Some("text/plain".to_string()),
        );
        res.final_url = Some(req.url.to_string());
        Ok(res)
      }

      fn fetch_partial_with_request(
        &self,
        req: FetchRequest<'_>,
        max_bytes: usize,
      ) -> Result<FetchedResource> {
        self.partial_calls.fetch_add(1, Ordering::SeqCst);
        let origin = cors_origin_key_from_client_origin(req.client_origin).unwrap_or_default();
        let mut bytes = format!("partial:{origin}").into_bytes();
        bytes.truncate(max_bytes);
        let mut res = FetchedResource::new(bytes, Some("text/plain".to_string()));
        res.final_url = Some(req.url.to_string());
        Ok(res)
      }
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_ENFORCE_CORS".to_string(),
      "1".to_string(),
    )])));

    runtime::with_thread_runtime_toggles(toggles, || {
      let full_calls = Arc::new(AtomicUsize::new(0));
      let partial_calls = Arc::new(AtomicUsize::new(0));
      let cache = CachingFetcher::new(OriginEchoFetcher {
        full_calls: Arc::clone(&full_calls),
        partial_calls: Arc::clone(&partial_calls),
      });

      let url = "http://example.com/image.png";
      let origin_a = origin_from_url("http://a.test/page").expect("origin");
      let origin_b = origin_from_url("http://b.test/page").expect("origin");
      let req_a = FetchRequest::new(url, FetchDestination::ImageCors)
        .with_client_origin(&origin_a)
        .with_referrer_url("http://a.test/page");
      let req_b = FetchRequest::new(url, FetchDestination::ImageCors)
        .with_client_origin(&origin_b)
        .with_referrer_url("http://b.test/page");

      // Seed a full response for origin A.
      let full_a = cache.fetch_with_request(req_a).expect("fetch A");
      assert!(
        String::from_utf8_lossy(&full_a.bytes).starts_with("full:http://a.test"),
        "unexpected full response: {:?}",
        String::from_utf8_lossy(&full_a.bytes)
      );
      assert_eq!(full_calls.load(Ordering::SeqCst), 1);
      assert_eq!(partial_calls.load(Ordering::SeqCst), 0);

      // A partial fetch for the same origin should reuse the cached full response rather than
      // calling through to the inner partial fetcher.
      let prefix_a = cache
        .fetch_partial_with_request(req_a, 8)
        .expect("partial A");
      assert_eq!(prefix_a.bytes, b"full:htt");
      assert_eq!(full_calls.load(Ordering::SeqCst), 1);
      assert_eq!(
        partial_calls.load(Ordering::SeqCst),
        0,
        "expected partial fetch to reuse cached full response for same origin"
      );

      // A partial fetch from origin B should *not* reuse A's cached response.
      let prefix_b = cache
        .fetch_partial_with_request(req_b, 8)
        .expect("partial B");
      assert_eq!(prefix_b.bytes, b"partial:");
      assert_eq!(full_calls.load(Ordering::SeqCst), 1);
      assert_eq!(
        partial_calls.load(Ordering::SeqCst),
        1,
        "expected partial fetch from a different origin to call inner fetcher when CORS enforcement is enabled"
      );
    });
  }

  #[test]
  fn caching_fetcher_partitions_cors_mode_entries_by_origin_without_enforcement_by_default() {
    #[derive(Clone)]
    struct OriginEchoFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for OriginEchoFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected CachingFetcher to use fetch_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let origin = cors_origin_key_from_client_origin(req.client_origin).unwrap_or_default();
        let mut res = FetchedResource::new(origin.into_bytes(), Some("text/plain".to_string()));
        res.final_url = Some(req.url.to_string());
        Ok(res)
      }
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_ENFORCE_CORS".to_string(),
      "0".to_string(),
    )])));

    runtime::with_thread_runtime_toggles(toggles, || {
      let calls = Arc::new(AtomicUsize::new(0));
      let cache = CachingFetcher::new(OriginEchoFetcher {
        calls: Arc::clone(&calls),
      });

      for (destination, url) in [
        (FetchDestination::Font, "http://example.com/font.woff2"),
        (FetchDestination::ImageCors, "http://example.com/image.png"),
      ] {
        let origin_a = origin_from_url("http://a.test/page").expect("origin");
        let origin_b = origin_from_url("http://b.test/page").expect("origin");
        let req_a = FetchRequest::new(url, destination)
          .with_client_origin(&origin_a)
          .with_referrer_url("http://a.test/page");
        let req_b = FetchRequest::new(url, destination)
          .with_client_origin(&origin_b)
          .with_referrer_url("http://b.test/page");

        let first_a = cache.fetch_with_request(req_a).expect("fetch A");
        assert_eq!(first_a.bytes, b"http://a.test");
        let first_b = cache.fetch_with_request(req_b).expect("fetch B");
        assert_eq!(first_b.bytes, b"http://b.test");

        // Ensure fetching `B` doesn't overwrite `A` and vice versa.
        let second_a = cache.fetch_with_request(req_a).expect("cache A");
        assert_eq!(second_a.bytes, b"http://a.test");
        let second_b = cache.fetch_with_request(req_b).expect("cache B");
        assert_eq!(second_b.bytes, b"http://b.test");
      }

      assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "expected differing referrer origins to produce distinct cache entries for CORS-mode requests even when CORS enforcement is disabled"
      );
    });
  }

  #[test]
  fn caching_fetcher_partitions_cors_mode_entries_by_client_origin_independent_of_referrer() {
    #[derive(Clone)]
    struct OriginEchoFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for OriginEchoFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected CachingFetcher to use fetch_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let origin = cors_origin_key_from_client_origin(req.client_origin).unwrap_or_default();
        let mut res = FetchedResource::new(origin.into_bytes(), Some("text/plain".to_string()));
        res.final_url = Some(req.url.to_string());
        Ok(res)
      }
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_ENFORCE_CORS".to_string(),
      "0".to_string(),
    )])));

    runtime::with_thread_runtime_toggles(toggles, || {
      let calls = Arc::new(AtomicUsize::new(0));
      let cache = CachingFetcher::new(OriginEchoFetcher {
        calls: Arc::clone(&calls),
      });

      let url = "http://example.com/font.woff2";
      let referrer = "http://referrer.test/style.css";
      let origin_a = origin_from_url("http://a.test/page").expect("origin");
      let origin_b = origin_from_url("http://b.test/page").expect("origin");
      let req_a = FetchRequest::new(url, FetchDestination::Font)
        .with_client_origin(&origin_a)
        .with_referrer_url(referrer);
      let req_b = FetchRequest::new(url, FetchDestination::Font)
        .with_client_origin(&origin_b)
        .with_referrer_url(referrer);

      let first_a = cache.fetch_with_request(req_a).expect("fetch A");
      assert_eq!(first_a.bytes, b"http://a.test");
      let first_b = cache.fetch_with_request(req_b).expect("fetch B");
      assert_eq!(first_b.bytes, b"http://b.test");

      // Ensure fetching `B` doesn't overwrite `A` and vice versa.
      let second_a = cache.fetch_with_request(req_a).expect("cache A");
      assert_eq!(second_a.bytes, b"http://a.test");
      let second_b = cache.fetch_with_request(req_b).expect("cache B");
      assert_eq!(second_b.bytes, b"http://b.test");

      assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "expected distinct client origins to produce distinct cache entries even when referrer URL is identical"
      );
    });
  }

  #[test]
  fn caching_fetcher_does_not_partition_cors_mode_entries_when_partition_toggle_disabled() {
    #[derive(Clone)]
    struct OriginEchoFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for OriginEchoFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected CachingFetcher to use fetch_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let origin = cors_origin_key_from_client_origin(req.client_origin).unwrap_or_default();
        let mut res = FetchedResource::new(origin.into_bytes(), Some("text/plain".to_string()));
        res.final_url = Some(req.url.to_string());
        Ok(res)
      }
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([
      ("FASTR_FETCH_ENFORCE_CORS".to_string(), "0".to_string()),
      (
        "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
        "0".to_string(),
      ),
    ])));

    runtime::with_thread_runtime_toggles(toggles, || {
      let calls = Arc::new(AtomicUsize::new(0));
      let cache = CachingFetcher::new(OriginEchoFetcher {
        calls: Arc::clone(&calls),
      });

      let url = "http://example.com/font.woff2";
      let referrer = "http://referrer.test/style.css";
      let origin_a = origin_from_url("http://a.test/page").expect("origin");
      let origin_b = origin_from_url("http://b.test/page").expect("origin");
      let req_a = FetchRequest::new(url, FetchDestination::Font)
        .with_client_origin(&origin_a)
        .with_referrer_url(referrer);
      let req_b = FetchRequest::new(url, FetchDestination::Font)
        .with_client_origin(&origin_b)
        .with_referrer_url(referrer);

      let first_a = cache.fetch_with_request(req_a).expect("fetch A");
      assert_eq!(first_a.bytes, b"http://a.test");
      let first_b = cache.fetch_with_request(req_b).expect("fetch B");
      assert_eq!(
        first_b.bytes, b"http://a.test",
        "expected partitioning to be disabled"
      );

      // Ensure fetching `B` doesn't overwrite `A` and vice versa.
      let second_a = cache.fetch_with_request(req_a).expect("cache A");
      assert_eq!(second_a.bytes, b"http://a.test");
      let second_b = cache.fetch_with_request(req_b).expect("cache B");
      assert_eq!(second_b.bytes, b"http://a.test");

      assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "expected requests with differing client origins to share cache entries when partitioning is disabled"
      );
    });
  }

  #[test]
  fn caching_fetcher_partitions_cors_mode_entries_by_credentials_mode() {
    #[derive(Clone)]
    struct CredsEchoFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for CredsEchoFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected CachingFetcher to use fetch_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let bytes = match req.credentials_mode {
          FetchCredentialsMode::Omit => b"omit".to_vec(),
          FetchCredentialsMode::SameOrigin => b"same-origin".to_vec(),
          FetchCredentialsMode::Include => b"include".to_vec(),
        };
        let mut res = FetchedResource::new(bytes, Some("text/plain".to_string()));
        res.final_url = Some(req.url.to_string());
        Ok(res)
      }
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_ENFORCE_CORS".to_string(),
      "0".to_string(),
    )])));

    runtime::with_thread_runtime_toggles(toggles, || {
      let calls = Arc::new(AtomicUsize::new(0));
      let cache = CachingFetcher::new(CredsEchoFetcher {
        calls: Arc::clone(&calls),
      });

      let url = "http://example.com/font.woff2";
      let origin = origin_from_url("http://a.test/page").expect("origin");
      let referrer = "http://referrer.test/style.css";
      let base_req = FetchRequest::new(url, FetchDestination::Font)
        .with_client_origin(&origin)
        .with_referrer_url(referrer);
      let req_omit = base_req.with_credentials_mode(FetchCredentialsMode::Omit);
      let req_include = base_req.with_credentials_mode(FetchCredentialsMode::Include);

      let omit = cache.fetch_with_request(req_omit).expect("omit fetch");
      assert_eq!(omit.bytes, b"omit");
      let include = cache.fetch_with_request(req_include).expect("include fetch");
      assert_eq!(include.bytes, b"include");

      // Ensure cache hits are scoped to the credentials mode.
      let omit2 = cache.fetch_with_request(req_omit).expect("omit cache");
      assert_eq!(omit2.bytes, b"omit");
      let include2 = cache.fetch_with_request(req_include).expect("include cache");
      assert_eq!(include2.bytes, b"include");

      assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "expected cache to be partitioned by credentials mode"
      );
    });
  }

  #[derive(Clone, Debug)]
  struct RecordedValidationCall {
    destination: FetchDestination,
    referrer_url: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
  }

  #[derive(Clone)]
  struct ContextValidatingFetcher {
    calls: Arc<Mutex<Vec<RecordedValidationCall>>>,
    step: Arc<AtomicUsize>,
  }

  impl ResourceFetcher for ContextValidatingFetcher {
    fn fetch(&self, _url: &str) -> Result<FetchedResource> {
      panic!("fetch() should not be called by CachingFetcher::fetch_with_request");
    }

    fn fetch_with_request(&self, _req: FetchRequest<'_>) -> Result<FetchedResource> {
      let step = self.step.fetch_add(1, Ordering::SeqCst);
      assert_eq!(step, 0, "unexpected fetch_with_request call count");
      let mut resource = FetchedResource::new(b"cached".to_vec(), Some("text/plain".to_string()));
      resource.status = Some(200);
      resource.etag = Some("etag1".to_string());
      resource.last_modified = Some("lm1".to_string());
      Ok(resource)
    }

    fn fetch_with_validation(
      &self,
      _url: &str,
      _etag: Option<&str>,
      _last_modified: Option<&str>,
    ) -> Result<FetchedResource> {
      panic!(
        "expected CachingFetcher::fetch_with_request to use fetch_with_request_and_validation"
      );
    }

    fn fetch_with_request_and_validation(
      &self,
      req: FetchRequest<'_>,
      etag: Option<&str>,
      last_modified: Option<&str>,
    ) -> Result<FetchedResource> {
      let step = self.step.fetch_add(1, Ordering::SeqCst);
      assert_eq!(
        step, 1,
        "unexpected fetch_with_request_and_validation call count"
      );
      self.calls.lock().unwrap().push(RecordedValidationCall {
        destination: req.destination,
        referrer_url: req.referrer_url.map(|s| s.to_string()),
        etag: etag.map(|s| s.to_string()),
        last_modified: last_modified.map(|s| s.to_string()),
      });
      let mut res = FetchedResource::new(Vec::new(), Some("text/plain".to_string()));
      res.status = Some(304);
      Ok(res)
    }
  }

  #[test]
  fn caching_fetcher_preserves_request_context_for_conditional_requests() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let inner = ContextValidatingFetcher {
      calls: Arc::clone(&calls),
      step: Arc::new(AtomicUsize::new(0)),
    };
    let cache = CachingFetcher::new(inner);

    let url = "http://example.com/resource";
    let req =
      FetchRequest::new(url, FetchDestination::Style).with_referrer_url("http://example.com/");

    let first = cache.fetch_with_request(req).expect("initial fetch");
    assert_eq!(first.bytes, b"cached");

    let second = cache.fetch_with_request(req).expect("revalidation fetch");
    assert_eq!(second.bytes, b"cached", "should return cached body on 304");

    let snapshot = calls.lock().unwrap().clone();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].destination, FetchDestination::Style);
    assert_eq!(
      snapshot[0].referrer_url.as_deref(),
      Some("http://example.com/")
    );
    assert_eq!(snapshot[0].etag.as_deref(), Some("etag1"));
    assert_eq!(snapshot[0].last_modified.as_deref(), Some("lm1"));
  }

  #[derive(Clone, Debug)]
  struct MockResponse {
    status: u16,
    body: Vec<u8>,
    etag: Option<String>,
    last_modified: Option<String>,
    cache_policy: Option<HttpCachePolicy>,
  }

  #[derive(Clone, Debug)]
  struct MockCall {
    url: String,
    etag: Option<String>,
    last_modified: Option<String>,
  }

  #[derive(Clone)]
  struct ScriptedFetcher {
    responses: Arc<Mutex<VecDeque<MockResponse>>>,
    calls: Arc<Mutex<Vec<MockCall>>>,
  }

  impl ScriptedFetcher {
    fn new(responses: Vec<MockResponse>) -> Self {
      Self {
        responses: Arc::new(Mutex::new(VecDeque::from(responses))),
        calls: Arc::new(Mutex::new(Vec::new())),
      }
    }

    fn record_call(&self, url: &str, etag: Option<&str>, last_modified: Option<&str>) {
      let mut calls = self.calls.lock().unwrap();
      calls.push(MockCall {
        url: url.to_string(),
        etag: etag.map(|s| s.to_string()),
        last_modified: last_modified.map(|s| s.to_string()),
      });
    }

    fn next_response(&self) -> Result<FetchedResource> {
      let mut responses = self.responses.lock().unwrap();
      let resp = responses
        .pop_front()
        .expect("scripted fetcher ran out of responses");

      let mut resource = FetchedResource::new(resp.body.clone(), Some("text/plain".to_string()));
      resource.status = Some(resp.status);
      resource.etag = resp.etag.clone();
      resource.last_modified = resp.last_modified.clone();
      resource.cache_policy = resp.cache_policy.clone();
      Ok(resource)
    }

    fn calls(&self) -> Vec<MockCall> {
      self.calls.lock().unwrap().clone()
    }
  }

  impl ResourceFetcher for ScriptedFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      self.record_call(url, None, None);
      self.next_response()
    }

    fn fetch_with_validation(
      &self,
      url: &str,
      etag: Option<&str>,
      last_modified: Option<&str>,
    ) -> Result<FetchedResource> {
      self.record_call(url, etag, last_modified);
      self.next_response()
    }
  }

  #[test]
  fn caching_fetcher_uses_cached_body_on_not_modified() {
    let fetcher = ScriptedFetcher::new(vec![
      MockResponse {
        status: 200,
        body: b"cached".to_vec(),
        etag: Some("etag1".to_string()),
        last_modified: Some("lm1".to_string()),
        cache_policy: None,
      },
      MockResponse {
        status: 304,
        body: Vec::new(),
        etag: None,
        last_modified: None,
        cache_policy: None,
      },
    ]);

    let cache = CachingFetcher::new(fetcher.clone());
    let url = "http://example.com/resource";

    let first = cache.fetch(url).expect("initial fetch");
    assert_eq!(first.bytes, b"cached");

    let second = cache.fetch(url).expect("revalidated fetch");
    assert_eq!(second.bytes, b"cached", "should return cached body on 304");

    let calls = fetcher.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].etag, None);
    assert_eq!(calls[1].etag.as_deref(), Some("etag1"));
    assert_eq!(calls[1].last_modified.as_deref(), Some("lm1"));
  }

  #[test]
  fn caching_fetcher_updates_validators_on_not_modified() {
    let fetcher = ScriptedFetcher::new(vec![
      MockResponse {
        status: 200,
        body: b"cached".to_vec(),
        etag: Some("etag1".to_string()),
        last_modified: Some("lm1".to_string()),
        cache_policy: None,
      },
      MockResponse {
        status: 304,
        body: Vec::new(),
        etag: Some("etag2".to_string()),
        last_modified: Some("lm2".to_string()),
        cache_policy: None,
      },
      MockResponse {
        status: 304,
        body: Vec::new(),
        etag: None,
        last_modified: None,
        cache_policy: None,
      },
    ]);

    let cache = CachingFetcher::new(fetcher.clone());
    let url = "http://example.com/resource";

    let first = cache.fetch(url).expect("initial fetch");
    assert_eq!(first.bytes, b"cached");

    let second = cache.fetch(url).expect("first revalidation");
    assert_eq!(second.bytes, b"cached");

    let third = cache.fetch(url).expect("second revalidation");
    assert_eq!(third.bytes, b"cached");

    let calls = fetcher.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1].etag.as_deref(), Some("etag1"));
    assert_eq!(calls[1].last_modified.as_deref(), Some("lm1"));
    assert_eq!(
      calls[2].etag.as_deref(),
      Some("etag2"),
      "validators should update after 304 with new etag"
    );
    assert_eq!(calls[2].last_modified.as_deref(), Some("lm2"));
  }

  #[test]
  fn caching_fetcher_falls_back_on_transient_status() {
    let fetcher = ScriptedFetcher::new(vec![
      MockResponse {
        status: 200,
        body: b"cached".to_vec(),
        etag: Some("etag1".to_string()),
        last_modified: None,
        cache_policy: None,
      },
      MockResponse {
        status: 503,
        body: b"transient".to_vec(),
        etag: None,
        last_modified: None,
        cache_policy: None,
      },
      MockResponse {
        status: 200,
        body: b"fresh".to_vec(),
        etag: Some("etag2".to_string()),
        last_modified: None,
        cache_policy: None,
      },
    ]);

    let cache = CachingFetcher::new(fetcher.clone());
    let url = "http://example.com/resource";

    let first = cache.fetch(url).expect("initial fetch");
    assert_eq!(first.bytes, b"cached");

    let second = cache.fetch(url).expect("fallback fetch");
    assert_eq!(
      second.bytes, b"cached",
      "should fall back to cached bytes on transient status"
    );

    let third = cache.fetch(url).expect("fresh fetch");
    assert_eq!(third.bytes, b"fresh");

    assert_eq!(fetcher.calls().len(), 3, "fetcher should be called 3 times");
  }

  #[test]
  fn caching_fetcher_does_not_poison_successful_entry_on_http_error() {
    let fetcher = ScriptedFetcher::new(vec![
      MockResponse {
        status: 200,
        body: b"cached".to_vec(),
        etag: Some("etag1".to_string()),
        last_modified: Some("lm1".to_string()),
        cache_policy: Some(HttpCachePolicy {
          max_age: Some(3600),
          ..Default::default()
        }),
      },
      MockResponse {
        status: 403,
        body: b"forbidden".to_vec(),
        etag: Some("etag_forbidden".to_string()),
        last_modified: Some("lm_forbidden".to_string()),
        cache_policy: Some(HttpCachePolicy {
          no_store: true,
          ..Default::default()
        }),
      },
      MockResponse {
        status: 200,
        body: b"fresh".to_vec(),
        etag: Some("etag2".to_string()),
        last_modified: None,
        cache_policy: None,
      },
    ]);

    let cache = CachingFetcher::new(fetcher.clone());
    let url = "http://example.com/resource";

    let first = cache.fetch(url).expect("initial fetch");
    assert_eq!(first.bytes, b"cached");

    let second = cache
      .fetch(url)
      .expect("http error refresh should fall back");
    assert_eq!(
      second.bytes, b"cached",
      "expected cached bytes to be served instead of 403 body"
    );
    assert_eq!(
      second.status,
      Some(200),
      "fallback should return cached status/metadata, not the HTTP error response"
    );
    assert_eq!(
      second.etag.as_deref(),
      Some("etag1"),
      "fallback should return cached status/metadata, not the HTTP error response"
    );
    assert_eq!(
      second.last_modified.as_deref(),
      Some("lm1"),
      "fallback should return cached status/metadata, not the HTTP error response"
    );
    assert_eq!(
      second
        .cache_policy
        .as_ref()
        .and_then(|policy| policy.max_age),
      Some(3600),
      "fallback should return cached status/metadata, not the HTTP error response"
    );
    assert!(
      !second
        .cache_policy
        .as_ref()
        .is_some_and(|policy| policy.no_store),
      "fallback should return cached status/metadata, not the HTTP error response"
    );

    let cached = cache
      .cached_entry(&CacheKey::new(FetchContextKind::Other, url.to_string()), None)
      .expect("cache entry should remain after fallback");
    let cached_res = cached
      .value
      .as_result()
      .expect("cache entry should be a resource");
    assert_eq!(
      cached_res.bytes, b"cached",
      "cache entry must not be overwritten by HTTP error response"
    );
    assert_eq!(
      cached.etag.as_deref(),
      Some("etag1"),
      "cache entry validators must not be overwritten by HTTP error response"
    );
    assert_eq!(
      cached.last_modified.as_deref(),
      Some("lm1"),
      "cache entry validators must not be overwritten by HTTP error response"
    );
    let cache_meta = cached
      .http_cache
      .as_ref()
      .expect("seed response should have cache metadata");
    assert_eq!(
      cache_meta.max_age,
      Some(Duration::from_secs(3600)),
      "HTTP error refresh must not overwrite cache metadata"
    );
    assert!(
      !cache_meta.no_store,
      "HTTP error refresh must not overwrite cache metadata"
    );

    let third = cache.fetch(url).expect("fresh fetch should update cache");
    assert_eq!(third.bytes, b"fresh");

    let calls = fetcher.calls();
    assert_eq!(calls.len(), 3, "expected three network attempts");
    assert_eq!(calls[1].etag.as_deref(), Some("etag1"));
    assert_eq!(calls[1].last_modified.as_deref(), Some("lm1"));
    assert_eq!(
      calls[2].etag.as_deref(),
      Some("etag1"),
      "403 refresh should not overwrite cached validators"
    );
    assert_eq!(
      calls[2].last_modified.as_deref(),
      Some("lm1"),
      "403 refresh should not overwrite cached validators"
    );
  }

  #[test]
  fn caching_fetcher_does_not_update_aliases_on_http_error() {
    #[derive(Clone, Debug)]
    struct AliasingResponse {
      status: u16,
      body: Vec<u8>,
      etag: Option<String>,
      final_url: Option<String>,
    }

    #[derive(Clone)]
    struct AliasingFetcher {
      responses: Arc<Mutex<VecDeque<AliasingResponse>>>,
      calls: Arc<Mutex<Vec<MockCall>>>,
    }

    impl AliasingFetcher {
      fn new(responses: Vec<AliasingResponse>) -> Self {
        Self {
          responses: Arc::new(Mutex::new(VecDeque::from(responses))),
          calls: Arc::new(Mutex::new(Vec::new())),
        }
      }

      fn record_call(&self, url: &str, etag: Option<&str>, last_modified: Option<&str>) {
        self.calls.lock().unwrap().push(MockCall {
          url: url.to_string(),
          etag: etag.map(|s| s.to_string()),
          last_modified: last_modified.map(|s| s.to_string()),
        });
      }

      fn next_response(&self, url: &str) -> Result<FetchedResource> {
        let mut responses = self.responses.lock().unwrap();
        let resp = responses
          .pop_front()
          .expect("scripted fetcher ran out of responses");
        let mut resource = FetchedResource::new(resp.body, Some("text/plain".to_string()));
        resource.status = Some(resp.status);
        resource.etag = resp.etag;
        resource.final_url = resp.final_url.or_else(|| Some(url.to_string()));
        Ok(resource)
      }

      fn calls(&self) -> Vec<MockCall> {
        self.calls.lock().unwrap().clone()
      }
    }

    impl ResourceFetcher for AliasingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.record_call(url, None, None);
        self.next_response(url)
      }

      fn fetch_with_validation(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
      ) -> Result<FetchedResource> {
        self.record_call(url, etag, last_modified);
        self.next_response(url)
      }
    }

    let start_url = "http://example.com/start";
    let canonical_url = "http://example.com/canonical";
    let error_final_url = "http://example.com/error";

    let fetcher = AliasingFetcher::new(vec![
      AliasingResponse {
        status: 200,
        body: b"cached".to_vec(),
        etag: Some("etag1".to_string()),
        final_url: Some(canonical_url.to_string()),
      },
      AliasingResponse {
        status: 403,
        body: b"forbidden".to_vec(),
        etag: None,
        final_url: Some(error_final_url.to_string()),
      },
      AliasingResponse {
        status: 200,
        body: b"network".to_vec(),
        etag: None,
        final_url: None,
      },
    ]);

    let cache = CachingFetcher::new(fetcher.clone());

    let first = cache.fetch(start_url).expect("seed fetch");
    assert_eq!(first.bytes, b"cached");
    assert_eq!(first.final_url.as_deref(), Some(canonical_url));

    let second = cache.fetch(start_url).expect("fallback fetch");
    assert_eq!(second.bytes, b"cached");
    assert_eq!(
      second.final_url.as_deref(),
      Some(canonical_url),
      "fallback should not expose HTTP error response final_url"
    );

    let third = cache
      .fetch(error_final_url)
      .expect("error final url should not be aliased/cached");
    assert_eq!(third.bytes, b"network");

    let calls = fetcher.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].url, start_url);
    assert_eq!(calls[1].url, start_url);
    assert_eq!(calls[1].etag.as_deref(), Some("etag1"));
    assert_eq!(calls[2].url, error_final_url);
  }

  #[test]
  fn caching_fetcher_fetch_with_request_does_not_update_aliases_on_http_error() {
    #[derive(Clone, Debug)]
    struct AliasingResponse {
      status: u16,
      body: Vec<u8>,
      etag: Option<String>,
      final_url: Option<String>,
    }

    #[derive(Clone, Debug)]
    struct RecordedCall {
      url: String,
      destination: FetchDestination,
      etag: Option<String>,
      last_modified: Option<String>,
    }

    #[derive(Clone)]
    struct AliasingFetcher {
      responses: Arc<Mutex<VecDeque<AliasingResponse>>>,
      calls: Arc<Mutex<Vec<RecordedCall>>>,
    }

    impl AliasingFetcher {
      fn new(responses: Vec<AliasingResponse>) -> Self {
        Self {
          responses: Arc::new(Mutex::new(VecDeque::from(responses))),
          calls: Arc::new(Mutex::new(Vec::new())),
        }
      }

      fn record_call(
        &self,
        url: &str,
        destination: FetchDestination,
        etag: Option<&str>,
        last_modified: Option<&str>,
      ) {
        self.calls.lock().unwrap().push(RecordedCall {
          url: url.to_string(),
          destination,
          etag: etag.map(|s| s.to_string()),
          last_modified: last_modified.map(|s| s.to_string()),
        });
      }

      fn next_response(&self, url: &str) -> Result<FetchedResource> {
        let mut responses = self.responses.lock().unwrap();
        let resp = responses
          .pop_front()
          .expect("scripted fetcher ran out of responses");
        let mut resource = FetchedResource::new(resp.body, Some("text/plain".to_string()));
        resource.status = Some(resp.status);
        resource.etag = resp.etag;
        resource.final_url = resp.final_url.or_else(|| Some(url.to_string()));
        Ok(resource)
      }

      fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
      }
    }

    impl ResourceFetcher for AliasingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("fetch() should not be called by fetch_with_request alias tests");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.record_call(req.url, req.destination, None, None);
        self.next_response(req.url)
      }

      fn fetch_with_request_and_validation(
        &self,
        req: FetchRequest<'_>,
        etag: Option<&str>,
        last_modified: Option<&str>,
      ) -> Result<FetchedResource> {
        self.record_call(req.url, req.destination, etag, last_modified);
        self.next_response(req.url)
      }
    }

    let start_url = "http://example.com/start.css";
    let canonical_url = "http://example.com/canonical.css";
    let error_final_url = "http://example.com/error.css";

    let fetcher = AliasingFetcher::new(vec![
      AliasingResponse {
        status: 200,
        body: b"cached".to_vec(),
        etag: Some("etag1".to_string()),
        final_url: Some(canonical_url.to_string()),
      },
      AliasingResponse {
        status: 403,
        body: b"forbidden".to_vec(),
        etag: None,
        final_url: Some(error_final_url.to_string()),
      },
      AliasingResponse {
        status: 200,
        body: b"network".to_vec(),
        etag: None,
        final_url: None,
      },
    ]);

    let cache = CachingFetcher::new(fetcher.clone());
    let req = FetchRequest::new(start_url, FetchDestination::Style);

    let first = cache.fetch_with_request(req).expect("seed fetch");
    assert_eq!(first.bytes, b"cached");
    assert_eq!(first.final_url.as_deref(), Some(canonical_url));

    let start_key = CacheKey::new(FetchContextKind::Stylesheet, start_url.to_string());
    let canonical_key = CacheKey::new(FetchContextKind::Stylesheet, canonical_url.to_string());
    let request_sig = inflight_signature_for_request(&cache.inner, req);
    {
      let state = cache.state.lock().unwrap();
      assert_eq!(
        state
          .aliases
          .get(&start_key)
          .and_then(|targets| targets.get(&request_sig)),
        Some(&canonical_key),
        "expected alias to be created from the initial redirect"
      );
    }

    let second = cache.fetch_with_request(req).expect("fallback fetch");
    assert_eq!(second.bytes, b"cached");
    assert_eq!(
      second.final_url.as_deref(),
      Some(canonical_url),
      "fallback should not expose HTTP error response final_url"
    );
    {
      let state = cache.state.lock().unwrap();
      assert_eq!(
        state
          .aliases
          .get(&start_key)
          .and_then(|targets| targets.get(&request_sig)),
        Some(&canonical_key),
        "HTTP error refresh should not overwrite redirect aliases"
      );
    }

    let third = cache
      .fetch_with_request(FetchRequest::new(error_final_url, FetchDestination::Style))
      .expect("error final url should not be aliased/cached");
    assert_eq!(third.bytes, b"network");

    let calls = fetcher.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].url, start_url);
    assert_eq!(calls[0].destination, FetchDestination::Style);
    assert_eq!(calls[1].url, start_url);
    assert_eq!(calls[1].destination, FetchDestination::Style);
    assert_eq!(calls[1].etag.as_deref(), Some("etag1"));
    assert_eq!(calls[2].url, error_final_url);
    assert_eq!(calls[2].destination, FetchDestination::Style);
  }

  #[test]
  fn caching_fetcher_redirect_aliases_are_request_signature_aware() {
    #[derive(Clone)]
    struct ReferrerAliasingFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ReferrerAliasingFetcher {
      fn new() -> Self {
        Self {
          calls: Arc::new(AtomicUsize::new(0)),
        }
      }

      fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
      }
    }

    impl ResourceFetcher for ReferrerAliasingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("fetch() should not be called by fetch_with_request tests");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let referer = req.referrer_url.unwrap_or("");
        let (final_url, body) = if referer.contains("a.example") {
          (
            "https://example.com/canonical-a",
            b"canonical-a".to_vec(),
          )
        } else {
          (
            "https://example.com/canonical-b",
            b"canonical-b".to_vec(),
          )
        };
        let mut resource = FetchedResource::new(body, Some("text/plain".to_string()));
        resource.final_url = Some(final_url.to_string());
        Ok(resource)
      }

      fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
        match header_name {
          "accept-encoding" => Some("gzip".to_string()),
          "accept-language" => Some("en".to_string()),
          "origin" => Some(req.client_origin.map(ToString::to_string).unwrap_or_default()),
          "referer" => Some(req.referrer_url.unwrap_or("").to_string()),
          "user-agent" => Some("fastrender-test".to_string()),
          _ => None,
        }
      }
    }

    let fetcher = ReferrerAliasingFetcher::new();
    let cache = CachingFetcher::new(fetcher.clone());

    let start_url = "https://example.com/start";
    let canonical_a_url = "https://example.com/canonical-a";
    let canonical_b_url = "https://example.com/canonical-b";

    let req_a =
      FetchRequest::new(start_url, FetchDestination::Other).with_referrer_url("https://a.example/");
    let req_b =
      FetchRequest::new(start_url, FetchDestination::Other).with_referrer_url("https://b.example/");

    let first = cache.fetch_with_request(req_a).expect("seed fetch");
    assert_eq!(first.bytes, b"canonical-a");
    assert_eq!(first.final_url.as_deref(), Some(canonical_a_url));

    let second = cache.fetch_with_request(req_b).expect("second fetch");
    assert_eq!(second.bytes, b"canonical-b");
    assert_eq!(
      second.final_url.as_deref(),
      Some(canonical_b_url),
      "second request should not follow redirect alias from the first request signature",
    );

    let third = cache.fetch_with_request(req_a).expect("third fetch");
    assert_eq!(third.bytes, b"canonical-a");
    assert_eq!(third.final_url.as_deref(), Some(canonical_a_url));
    assert_eq!(
      fetcher.call_count(),
      2,
      "distinct request signatures should require distinct network fetches",
    );

    let start_key = CacheKey::new(FetchContextKind::Other, start_url.to_string());
    let canonical_a_key = CacheKey::new(FetchContextKind::Other, canonical_a_url.to_string());
    let canonical_b_key = CacheKey::new(FetchContextKind::Other, canonical_b_url.to_string());
    let request_sig_a = inflight_signature_for_request(&cache.inner, req_a);
    let request_sig_b = inflight_signature_for_request(&cache.inner, req_b);
    assert_ne!(request_sig_a, request_sig_b);

    let state = cache.state.lock().unwrap();
    assert_eq!(
      state
        .aliases
        .get(&start_key)
        .and_then(|targets| targets.get(&request_sig_a)),
      Some(&canonical_a_key),
    );
    assert_eq!(
      state
        .aliases
        .get(&start_key)
        .and_then(|targets| targets.get(&request_sig_b)),
      Some(&canonical_b_key),
    );
  }

  #[test]
  fn resource_cache_diagnostics_record_stale_hit_on_http_error_refresh() {
    let _lock = resource_cache_diagnostics_test_lock();
    let fetcher = ScriptedFetcher::new(vec![
      MockResponse {
        status: 200,
        body: b"cached".to_vec(),
        etag: Some("etag1".to_string()),
        last_modified: None,
        cache_policy: None,
      },
      MockResponse {
        status: 403,
        body: b"forbidden".to_vec(),
        etag: None,
        last_modified: None,
        cache_policy: None,
      },
    ]);
    let cache = CachingFetcher::new(fetcher);
    let url = "http://example.com/diagnostics-http-error";
    let seed = cache.fetch(url).expect("seed fetch");
    assert_eq!(seed.bytes, b"cached");

    enable_resource_cache_diagnostics();
    let second = cache.fetch(url).expect("fallback fetch");
    assert_eq!(second.bytes, b"cached");

    let stats = take_resource_cache_diagnostics().expect("diagnostics should be enabled");
    assert_eq!(stats.fresh_hits, 0);
    assert_eq!(stats.revalidated_hits, 0);
    assert_eq!(stats.misses, 0);
    assert_eq!(stats.stale_hits, 1);
    assert_eq!(stats.resource_cache_bytes, b"cached".len());
  }

  #[test]
  #[cfg(feature = "disk_cache")]
  fn resource_cache_diagnostics_record_stale_hit_on_http_error_refresh_disk_cache() {
    let _lock = resource_cache_diagnostics_test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let fetcher = ScriptedFetcher::new(vec![
      MockResponse {
        status: 200,
        body: b"cached".to_vec(),
        etag: Some("etag1".to_string()),
        last_modified: None,
        cache_policy: Some(HttpCachePolicy {
          max_age: Some(3600),
          ..Default::default()
        }),
      },
      MockResponse {
        status: 403,
        body: b"forbidden".to_vec(),
        etag: None,
        last_modified: None,
        cache_policy: None,
      },
    ]);
    let disk = DiskCachingFetcher::with_configs(
      fetcher,
      tmp.path(),
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        // Force stale planning so the second fetch attempts a refresh.
        max_age: Some(Duration::from_secs(0)),
        ..DiskCacheConfig::default()
      },
    );
    let url = "https://example.com/diagnostics-http-error";
    let seed = disk.fetch(url).expect("seed fetch");
    assert_eq!(seed.bytes, b"cached");

    enable_resource_cache_diagnostics();
    let second = disk.fetch(url).expect("fallback fetch");
    assert_eq!(second.bytes, b"cached");

    let stats = take_resource_cache_diagnostics().expect("diagnostics should be enabled");
    assert_eq!(stats.fresh_hits, 0);
    assert_eq!(stats.revalidated_hits, 0);
    assert_eq!(stats.misses, 0);
    assert_eq!(stats.stale_hits, 1);
    assert_eq!(stats.resource_cache_bytes, b"cached".len());
  }

  #[test]
  fn caching_fetcher_fetch_with_request_does_not_poison_successful_entry_on_http_error() {
    #[derive(Clone, Debug)]
    struct RecordedRequestCall {
      etag: Option<String>,
      last_modified: Option<String>,
    }

    #[derive(Clone, Debug)]
    struct RequestResponse {
      status: u16,
      body: Vec<u8>,
      etag: Option<String>,
      last_modified: Option<String>,
      cache_policy: Option<HttpCachePolicy>,
    }

    #[derive(Clone)]
    struct RequestScriptedFetcher {
      responses: Arc<Mutex<VecDeque<RequestResponse>>>,
      calls: Arc<Mutex<Vec<RecordedRequestCall>>>,
    }

    impl RequestScriptedFetcher {
      fn new(responses: Vec<RequestResponse>) -> Self {
        Self {
          responses: Arc::new(Mutex::new(VecDeque::from(responses))),
          calls: Arc::new(Mutex::new(Vec::new())),
        }
      }

      fn next_response(&self, url: &str) -> Result<FetchedResource> {
        let mut responses = self.responses.lock().unwrap();
        let resp = responses
          .pop_front()
          .expect("scripted fetcher ran out of responses");
        let mut resource = FetchedResource::new(resp.body, Some("text/plain".to_string()));
        resource.status = Some(resp.status);
        resource.final_url = Some(url.to_string());
        resource.etag = resp.etag;
        resource.last_modified = resp.last_modified;
        resource.cache_policy = resp.cache_policy;
        Ok(resource)
      }

      fn calls(&self) -> Vec<RecordedRequestCall> {
        self.calls.lock().unwrap().clone()
      }
    }

    impl ResourceFetcher for RequestScriptedFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("fetch() should not be called by fetch_with_request tests");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push(RecordedRequestCall {
          etag: None,
          last_modified: None,
        });
        self.next_response(req.url)
      }

      fn fetch_with_request_and_validation(
        &self,
        req: FetchRequest<'_>,
        etag: Option<&str>,
        last_modified: Option<&str>,
      ) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push(RecordedRequestCall {
          etag: etag.map(|s| s.to_string()),
          last_modified: last_modified.map(|s| s.to_string()),
        });
        self.next_response(req.url)
      }
    }

    let fetcher = RequestScriptedFetcher::new(vec![
      RequestResponse {
        status: 200,
        body: b"cached".to_vec(),
        etag: Some("etag1".to_string()),
        last_modified: Some("lm1".to_string()),
        cache_policy: Some(HttpCachePolicy {
          max_age: Some(3600),
          ..Default::default()
        }),
      },
      RequestResponse {
        status: 403,
        body: b"forbidden".to_vec(),
        etag: Some("etag_forbidden".to_string()),
        last_modified: Some("lm_forbidden".to_string()),
        cache_policy: Some(HttpCachePolicy {
          no_store: true,
          ..Default::default()
        }),
      },
      RequestResponse {
        status: 200,
        body: b"fresh".to_vec(),
        etag: Some("etag2".to_string()),
        last_modified: None,
        cache_policy: None,
      },
    ]);

    let cache = CachingFetcher::new(fetcher.clone());
    let url = "http://example.com/request";
    let req = FetchRequest::new(url, FetchDestination::Style);

    let first = cache.fetch_with_request(req).expect("seed fetch");
    assert_eq!(first.bytes, b"cached");

    let second = cache.fetch_with_request(req).expect("fallback fetch");
    assert_eq!(second.bytes, b"cached");
    assert_eq!(
      second.status,
      Some(200),
      "fallback should return cached status/metadata, not the HTTP error response"
    );
    assert_eq!(
      second.etag.as_deref(),
      Some("etag1"),
      "fallback should return cached status/metadata, not the HTTP error response"
    );
    assert_eq!(
      second.last_modified.as_deref(),
      Some("lm1"),
      "fallback should return cached status/metadata, not the HTTP error response"
    );
    assert_eq!(
      second
        .cache_policy
        .as_ref()
        .and_then(|policy| policy.max_age),
      Some(3600),
      "fallback should return cached status/metadata, not the HTTP error response"
    );
    assert!(
      !second
        .cache_policy
        .as_ref()
        .is_some_and(|policy| policy.no_store),
      "fallback should return cached status/metadata, not the HTTP error response"
    );

    let cached = cache
      .cached_entry(
        &CacheKey::new(FetchContextKind::Stylesheet, url.to_string()),
        None,
      )
      .expect("cache entry should remain after fallback");
    let cached_res = cached
      .value
      .as_result()
      .expect("cache entry should be a resource");
    assert_eq!(
      cached_res.bytes, b"cached",
      "cache entry must not be overwritten by HTTP error response"
    );
    assert_eq!(
      cached.etag.as_deref(),
      Some("etag1"),
      "cache entry validators must not be overwritten by HTTP error response"
    );
    assert_eq!(
      cached.last_modified.as_deref(),
      Some("lm1"),
      "cache entry validators must not be overwritten by HTTP error response"
    );
    let cache_meta = cached
      .http_cache
      .as_ref()
      .expect("seed response should have cache metadata");
    assert_eq!(
      cache_meta.max_age,
      Some(Duration::from_secs(3600)),
      "HTTP error refresh must not overwrite cache metadata"
    );
    assert!(
      !cache_meta.no_store,
      "HTTP error refresh must not overwrite cache metadata"
    );

    let third = cache.fetch_with_request(req).expect("fresh fetch");
    assert_eq!(third.bytes, b"fresh");

    let calls = fetcher.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1].etag.as_deref(), Some("etag1"));
    assert_eq!(calls[1].last_modified.as_deref(), Some("lm1"));
    assert_eq!(
      calls[2].etag.as_deref(),
      Some("etag1"),
      "403 refresh should not overwrite cached validators"
    );
    assert_eq!(
      calls[2].last_modified.as_deref(),
      Some("lm1"),
      "403 refresh should not overwrite cached validators"
    );
  }

  #[test]
  fn honors_fresh_cache_control_without_revalidating() {
    let fetcher = ScriptedFetcher::new(vec![
      MockResponse {
        status: 200,
        body: b"cached".to_vec(),
        etag: Some("etag1".to_string()),
        last_modified: Some("lm1".to_string()),
        cache_policy: Some(HttpCachePolicy {
          max_age: Some(3600),
          ..Default::default()
        }),
      },
      MockResponse {
        status: 304,
        body: b"stale".to_vec(),
        etag: None,
        last_modified: None,
        cache_policy: None,
      },
    ]);

    let cache = CachingFetcher::with_config(
      fetcher.clone(),
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
    );
    let url = "http://example.com/fresh";

    let first = cache.fetch(url).expect("initial fetch");
    assert_eq!(first.bytes, b"cached");

    let second = cache.fetch(url).expect("cached fetch");
    assert_eq!(second.bytes, b"cached");
    assert_eq!(
      fetcher.calls().len(),
      1,
      "fresh cached entries should not be revalidated"
    );
  }

  #[test]
  fn no_cache_policy_triggers_validation_even_when_fresh() {
    let fetcher = ScriptedFetcher::new(vec![
      MockResponse {
        status: 200,
        body: b"cached".to_vec(),
        etag: Some("etag1".to_string()),
        last_modified: Some("lm1".to_string()),
        cache_policy: Some(HttpCachePolicy {
          max_age: Some(3600),
          no_cache: true,
          ..Default::default()
        }),
      },
      MockResponse {
        status: 304,
        body: Vec::new(),
        etag: None,
        last_modified: None,
        cache_policy: None,
      },
    ]);

    let cache = CachingFetcher::with_config(
      fetcher.clone(),
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
    );
    let url = "http://example.com/no-cache";

    cache.fetch(url).expect("initial fetch");
    cache.fetch(url).expect("revalidated fetch");

    let calls = fetcher.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].etag, None);
    assert_eq!(
      calls[1].etag.as_deref(),
      Some("etag1"),
      "no-cache should still force conditional requests when validators exist"
    );
  }

  #[test]
  fn freshness_cap_limits_http_freshness() {
    let cache = CachingFetcher::with_config(
      ScriptedFetcher::new(vec![]),
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
    );

    let stored_at = SystemTime::now()
      .checked_sub(Duration::from_secs(2))
      .unwrap();
    let http_cache = CachedHttpMetadata {
      stored_at,
      max_age: Some(Duration::from_secs(3600)),
      expires: None,
      no_cache: false,
      no_store: false,
      must_revalidate: false,
    };
    let snapshot = CachedSnapshot {
      value: CacheValue::Resource(FetchedResource::new(
        b"cached".to_vec(),
        Some("text/plain".to_string()),
      )),
      etag: Some("etag1".to_string()),
      last_modified: Some("lm1".to_string()),
      http_cache: Some(http_cache),
    };

    let plan = cache.plan_cache_use(
      "http://example.com/capped",
      Some(snapshot),
      Some(Duration::from_secs(1)),
    );

    assert!(
      matches!(plan.action, CacheAction::Validate { .. }),
      "freshness cap should force validation even when HTTP max-age is longer"
    );
  }

  #[test]
  fn caching_fetcher_aliases_final_url() {
    #[derive(Clone)]
    struct RedirectingFetcher {
      calls: Arc<Mutex<Vec<String>>>,
      target: String,
    }

    impl ResourceFetcher for RedirectingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.calls.lock().unwrap().push(url.to_string());
        let mut res = FetchedResource::new(b"alias".to_vec(), Some("text/plain".to_string()));
        res.final_url = Some(self.target.clone());
        Ok(res)
      }
    }

    let calls = Arc::new(Mutex::new(Vec::new()));
    let target = "http://example.com/final".to_string();
    let fetcher = RedirectingFetcher {
      calls: Arc::clone(&calls),
      target: target.clone(),
    };
    let cache = CachingFetcher::new(fetcher);

    let initial = cache
      .fetch("http://example.com/start")
      .expect("first fetch");
    assert_eq!(initial.final_url.as_deref(), Some(target.as_str()));

    let second = cache.fetch(&target).expect("aliased fetch");
    assert_eq!(second.bytes, initial.bytes);
    assert_eq!(calls.lock().unwrap().len(), 1, "alias should hit cache");
  }

  #[test]
  fn parse_vary_headers_coalesces_sort_and_lowercases() {
    use http::header;

    let mut headers = HeaderMap::new();
    headers.append(header::VARY, http::HeaderValue::from_static("Origin"));
    headers.append(
      header::VARY,
      http::HeaderValue::from_static("Accept-Encoding, origin"),
    );
    headers.append(header::VARY, http::HeaderValue::from_static("ACCEPT-LANGUAGE"));

    let normalized = parse_vary_headers(&headers);
    assert_eq!(
      normalized.as_deref(),
      Some("accept-encoding, accept-language, origin")
    );
  }

  #[test]
  fn parse_vary_headers_returns_star() {
    use http::header;

    let mut headers = HeaderMap::new();
    headers.append(header::VARY, http::HeaderValue::from_static("Origin"));
    headers.append(header::VARY, http::HeaderValue::from_static("*"));
    assert_eq!(parse_vary_headers(&headers).as_deref(), Some("*"));
  }

  #[test]
  fn caching_fetcher_tracks_multiple_vary_variants_for_one_url() {
    #[derive(Clone)]
    struct VaryOriginFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for VaryOriginFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let origin = cors_origin_key_for_request(&req).unwrap_or_default();
        let mut resource =
          FetchedResource::new(origin.as_bytes().to_vec(), Some("text/plain".to_string()));
        resource.status = Some(200);
        resource.final_url = Some(req.url.to_string());
        resource.vary = Some("origin".to_string());
        resource.cache_policy = Some(HttpCachePolicy {
          max_age: Some(60),
          ..Default::default()
        });
        Ok(resource)
      }

      fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
        if header_name.eq_ignore_ascii_case("origin") {
          return cors_origin_key_for_request(&req);
        }
        None
      }
    }

    // Disable CORS cache partitioning so the cache key remains stable across differing Origin
    // values. This ensures the test exercises the `Vary` variant machinery rather than partition
    // keys.
    let _toggles_guard = runtime::set_runtime_toggles(Arc::new(runtime::RuntimeToggles::from_map(
      HashMap::from([(
        "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
        "0".to_string(),
      )]),
    )));

    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = CachingFetcher::new(VaryOriginFetcher {
      calls: Arc::clone(&calls),
    });
    let url = "https://example.com/font.woff2";

    let first_a = fetcher
      .fetch_with_request(
        FetchRequest::new(url, FetchDestination::Font)
          .with_referrer_url("https://a.test/page.html"),
      )
      .expect("fetch origin A");
    assert_eq!(first_a.bytes, b"https://a.test");

    let first_b = fetcher
      .fetch_with_request(
        FetchRequest::new(url, FetchDestination::Font)
          .with_referrer_url("https://b.test/page.html"),
      )
      .expect("fetch origin B");
    assert_eq!(first_b.bytes, b"https://b.test");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let second_a = fetcher
      .fetch_with_request(
        FetchRequest::new(url, FetchDestination::Font)
          .with_referrer_url("https://a.test/page.html"),
      )
      .expect("cached origin A");
    assert_eq!(second_a.bytes, b"https://a.test");
    assert_eq!(
      calls.load(Ordering::SeqCst),
      2,
      "cached fetch should not re-fetch the network"
    );
  }

  #[test]
  fn caching_fetcher_skips_vary_star_entries() {
    #[derive(Clone)]
    struct VaryStarFetcher {
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for VaryStarFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut res = FetchedResource::new(b"ok".to_vec(), Some("text/plain".to_string()));
        res.status = Some(200);
        res.final_url = Some(url.to_string());
        res.vary = Some("*".to_string());
        Ok(res)
      }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let cache = CachingFetcher::new(VaryStarFetcher {
      calls: Arc::clone(&calls),
    });
    let url = "https://example.com/vary-star";
    let _ = cache.fetch(url).expect("first");
    let _ = cache.fetch(url).expect("second");
    assert_eq!(
      calls.load(Ordering::SeqCst),
      2,
      "Vary:* responses must not be cached"
    );
  }
}
