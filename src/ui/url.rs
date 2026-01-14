use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use url::Url;

use crate::ui::messages::NavigationReason;

pub const DEFAULT_SEARCH_ENGINE_TEMPLATE: &str = "https://duckduckgo.com/?q={query}";

// -----------------------------------------------------------------------------
// Crash URL testing hooks
// -----------------------------------------------------------------------------
//
// The multiprocess/security workstream uses `crash://` URLs as deterministic smoke tests for
// renderer crash/unresponsive handling. These URLs are **disabled by default** so they are not
// reachable during normal browsing sessions.
//
// The windowed/headless `browser` binary can opt into allowing the scheme via CLI/env knobs and
// sets this flag at startup.
static ALLOW_CRASH_URLS: AtomicBool = AtomicBool::new(false);

/// Allow (or disallow) navigation to `crash://` URLs.
///
/// Disabled by default. Intended for CI/testing harnesses.
pub fn set_allow_crash_urls(enabled: bool) {
  ALLOW_CRASH_URLS.store(enabled, Ordering::Relaxed);
}

/// Returns `true` when navigation to `crash://` URLs is currently allowlisted.
///
/// This is intended primarily for tests/integration harnesses that temporarily override the
/// process-global allowlist and need to restore the previous value.
pub fn crash_urls_allowed() -> bool {
  ALLOW_CRASH_URLS.load(Ordering::Relaxed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OmniboxInputResolution {
  Url { url: String },
  Search { query: String, url: String },
}

impl OmniboxInputResolution {
  pub fn url(&self) -> &str {
    match self {
      Self::Url { url } | Self::Search { url, .. } => url,
    }
  }

  pub fn query(&self) -> Option<&str> {
    match self {
      Self::Url { .. } => None,
      Self::Search { query, .. } => Some(query),
    }
  }
}

/// Validate that a normalized, user-supplied URL uses a supported scheme.
///
/// This is intended for URLs originating from the address bar / command-line `browser [<url>]`
/// argument. It rejects opaque schemes like `javascript:` even though `url::Url` can parse them.
///
/// Note: this intentionally rejects privileged internal schemes reserved for renderer-chrome such
/// as `chrome://` (built-in UI assets), `chrome-action:` (chrome UI actions), and `chrome-dialog:`
/// (chrome modal/dialog result actions). See
/// `docs/renderer_chrome_schemes.md`.
///
/// For programmatic navigations (e.g. link clicks) the worker still validates schemes independently.
pub fn validate_user_navigation_url_scheme(url: &str) -> Result<(), String> {
  validate_navigation_url_scheme(url, false)
}

/// Validate that a URL uses a supported scheme for *trusted* chrome documents.
///
/// This is intended for internal browser-chrome pages (renderer-chrome workstream) that need to
/// navigate to other built-in pages and assets.
///
/// Security note: Do **not** use this for untrusted web content.
pub fn validate_trusted_chrome_navigation_url_scheme(url: &str) -> Result<(), String> {
  validate_navigation_url_scheme(url, true)
}

fn validate_navigation_url_scheme(url: &str, allow_chrome: bool) -> Result<(), String> {
  let parsed = Url::parse(url).map_err(|err| err.to_string())?;
  let scheme = parsed.scheme().to_ascii_lowercase();
  match scheme.as_str() {
    "http" | "https" | "file" | "about" => Ok(()),
    "chrome" if allow_chrome => Ok(()),
    "chrome" => Err("navigation to chrome:// URLs is restricted to trusted chrome pages".to_string()),
    "crash" if crash_urls_allowed() => Ok(()),
    "javascript" => Err("navigation to javascript: URLs is not supported".to_string()),
    _ => Err(format!("unsupported URL scheme: {scheme}")),
  }
}

/// Sanitize a URL string received from the render worker before storing it in browser UI state or
/// acting on it.
///
/// This is intended for *untrusted* renderer-supplied URLs that are displayed to the user (status
/// bar hover URL) or used by UI actions (page context menu open/download/copy/bookmark).
///
/// Policy:
/// - Enforces a hard maximum byte length ([`crate::ui::protocol_limits::MAX_URL_BYTES`]).
/// - Trims ASCII whitespace.
/// - Parses with `url::Url` (rejects invalid URLs).
/// - Allowlists schemes that the browser UI can safely display/act on: http, https, file, about.
///
/// Returns a normalized URL string (`Url::to_string()`) on success, otherwise `None`.
pub fn sanitize_worker_url_for_ui(raw: &str) -> Option<String> {
  if raw.as_bytes().len() > crate::ui::protocol_limits::MAX_URL_BYTES {
    return None;
  }
  let raw = trim_ascii_whitespace(raw);
  if raw.is_empty() {
    return None;
  }

  let parsed = Url::parse(raw).ok()?;
  match parsed.scheme().to_ascii_lowercase().as_str() {
    "http" | "https" | "file" | "about" => Some(parsed.to_string()),
    _ => None,
  }
}

/// Browser navigation policy for `file://` URLs.
///
/// This helper is intended to be consulted *only* when the navigation target is a `file://` URL.
/// It determines whether a navigation to a local file should be permitted based on:
/// - the navigation reason (typed URL vs. link click vs. back/forward/reload), and
/// - the current document URL (the "initiator") when available.
///
/// Rationale: in a multiprocess / sandbox model, allowing web content (`http`/`https`) to navigate a
/// tab to `file://...` is a security footgun (and differs from mainstream browsers).
pub(crate) fn navigation_to_file_is_allowed(
  reason: NavigationReason,
  current_url: Option<&str>,
) -> bool {
  match reason {
    // Explicit user input is allowed to open local files.
    NavigationReason::TypedUrl => true,
    // Browser chrome actions should allow revisiting already-existing history entries that happen
    // to be `file://` URLs.
    NavigationReason::BackForward | NavigationReason::Reload => true,
    // Link clicks from web pages must not be able to initiate file navigations.
    NavigationReason::LinkClick => {
      let Some(current_url) = current_url else {
        return true;
      };
      let current_url = trim_ascii_whitespace(current_url);
      if current_url.is_empty() {
        return true;
      }
      let scheme = current_url
        .split_once(':')
        .map(|(scheme, _rest)| scheme)
        .unwrap_or("");
      !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https")
    }
  }
}

/// Normalize a user-provided address bar input into a canonical URL string.
///
/// This function intentionally performs *minimal* normalization; see
/// `normalize_user_url`'s unit tests for the expected behavior.
///
/// Special-cases:
/// - Filesystem-looking paths are converted to `file://` URLs.
///
/// Note: `url::Url` accepts opaque/non-hierarchical schemes like `javascript:`.
/// We currently treat those as valid URLs and return them successfully; callers
/// should apply a scheme allowlist (e.g. [`validate_user_navigation_url_scheme`]) before attempting
/// navigation/fetch.
pub fn normalize_user_url(input: &str) -> Result<String, String> {
  let input = trim_ascii_whitespace(input);
  if input.is_empty() {
    return Err("empty URL".to_string());
  }

  if looks_like_file_path(input) {
    return file_url_from_user_input(input)
      .ok_or_else(|| format!("failed to convert path to file:// URL: {input:?}"));
  }

  match Url::parse(input) {
    Ok(url) => {
      // `url::Url` treats strings of the form `host:port` as opaque URLs with a custom scheme named
      // `host` (because RFC 3986 schemes can contain dots). In a browser address bar, users almost
      // always intend `localhost:3000` / `example.com:8080` to mean an HTTP(S) URL with an implied
      // scheme.
      //
      // Only apply this heuristic when the input does *not* already contain an explicit `://`
      // separator and the `:port` portion looks numeric. This preserves existing behaviour for
      // unsupported-but-explicit schemes like `foo:bar` (which should still round-trip through
      // normalization so callers can display a meaningful "unsupported scheme" error).
      if url.cannot_be_a_base() && looks_like_host_port_without_scheme(input) {
        let with_scheme = format!("https://{input}");
        return Url::parse(&with_scheme)
          .map(|url| url.to_string())
          .map_err(|parse_err| parse_err.to_string());
      }
      Ok(url.to_string())
    }
    Err(parse_err) => {
      if !input.contains("://") && !input.contains(' ') {
        let with_scheme = format!("https://{input}");
        match Url::parse(&with_scheme) {
          Ok(url) => Ok(url.to_string()),
          Err(parse_err) => Err(parse_err.to_string()),
        }
      } else {
        Err(parse_err.to_string())
      }
    }
  }
}

/// Resolve a link `href` attribute value against a base URL.
///
/// This is intended for DOM-driven navigations like link clicks and context menu actions.
///
/// - Empty/whitespace-only hrefs resolve to the base URL (`Url::join("")` semantics).
/// - Returns `None` when `href` cannot be resolved or resolves to a `javascript:` URL (the browser
///   UI does not execute JavaScript).
pub fn resolve_link_url(base_url: &str, href: &str) -> Option<String> {
  let href = trim_ascii_whitespace(href);

  // Fast path: avoid parsing if this is clearly a `javascript:` URL (common in legacy pages).
  if href
    .as_bytes()
    .get(.."javascript:".len())
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"javascript:"))
  {
    return None;
  }

  if let Ok(base) = Url::parse(base_url) {
    if let Ok(joined) = base.join(href) {
      if joined.scheme().eq_ignore_ascii_case("javascript") {
        return None;
      }
      return Some(joined.to_string());
    }

    // Empty hrefs are valid same-document navigations; if `Url::join` fails (e.g. for
    // cannot-be-a-base URLs), still treat the base as the resolved URL.
    if href.is_empty() && !base.scheme().eq_ignore_ascii_case("javascript") {
      let mut base = base;
      base.set_fragment(None);
      return Some(base.to_string());
    }
  }

  if href.is_empty() {
    return None;
  }

  let absolute = Url::parse(href).ok()?;
  (!absolute.scheme().eq_ignore_ascii_case("javascript")).then(|| absolute.to_string())
}

/// Compute an HTTP fallback URL for a failed HTTPS navigation.
///
/// When `url` parses successfully as a `https://` URL, returns the same URL with the scheme
/// changed to `http://` while preserving host, port, path, query and fragment.
///
/// This is intended for explicit, user-triggered "Try HTTP" actions in the browser chrome. It does
/// **not** perform any automatic redirects.
pub fn http_fallback_url_for_failed_https(url: &str) -> Option<String> {
  let mut parsed = Url::parse(url).ok()?;
  if !parsed.scheme().eq_ignore_ascii_case("https") {
    return None;
  }
  parsed.set_scheme("http").ok()?;
  Some(parsed.to_string())
}

fn looks_like_file_path(input: &str) -> bool {
  input.starts_with('/')
    || input.starts_with("./")
    || input.starts_with("../")
    || looks_like_windows_drive_path(input)
    || looks_like_windows_unc_path(input)
}

fn looks_like_host_port_without_scheme(input: &str) -> bool {
  if input.contains("://") || input.as_bytes().iter().any(|b| b.is_ascii_whitespace()) {
    return false;
  }
  let Some((host, rest)) = input.split_once(':') else {
    return false;
  };

  let host = host.trim();
  if host.is_empty() {
    return false;
  }
  if host_is_likely_known_scheme(host) {
    // Preserve the interpretation of inputs like `mailto:123`, `tel:5551234`, or `javascript:1` as
    // explicit schemes instead of rewriting them to `https://...`.
    return false;
  }
  if !host_looks_like_hostname(host) {
    return false;
  }

  let rest = rest.trim();
  // A host:port candidate must have a numeric port immediately after the colon.
  let bytes = rest.as_bytes();
  let Some(first) = bytes.first() else {
    return false;
  };
  if !first.is_ascii_digit() {
    return false;
  }

  // Be conservative: require the port to be a valid u16 and to be followed by a URL delimiter (or
  // end-of-input). This avoids reinterpreting inputs like `foo:123bar` as a missing-scheme URL.
  let mut idx = 0usize;
  while idx < bytes.len() && bytes[idx].is_ascii_digit() {
    idx += 1;
  }
  let port_str = rest.get(..idx).unwrap_or("");
  if port_str.parse::<u16>().is_err() {
    return false;
  }
  match bytes.get(idx) {
    None | Some(b'/') | Some(b'?') | Some(b'#') => true,
    _ => false,
  }
}

fn host_looks_like_hostname(host: &str) -> bool {
  // Accept common hostname forms:
  // - single-label dev/intranet hosts: `devbox`
  // - multi-label domains: `example.com`
  // This intentionally does *not* accept underscores or non-ASCII characters. Inputs that don't
  // match this heuristic should fall back to either explicit-scheme handling (`foo:bar`) or search.
  host.split('.').all(|label| {
    if label.is_empty() {
      return false;
    }
    let bytes = label.as_bytes();
    if bytes.first() == Some(&b'-') || bytes.last() == Some(&b'-') {
      return false;
    }
    bytes
      .iter()
      .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
  })
}

fn host_is_likely_known_scheme(host: &str) -> bool {
  // This list is intentionally small and conservative: it's here to avoid reinterpreting common
  // explicit scheme URLs that can reasonably start with digits in their opaque portion.
  //
  // Note: schemes are ASCII case-insensitive.
  host.eq_ignore_ascii_case("about")
    || host.eq_ignore_ascii_case("chrome")
    || host.eq_ignore_ascii_case("chrome-action")
    || host.eq_ignore_ascii_case("crash")
    || host.eq_ignore_ascii_case("data")
    || host.eq_ignore_ascii_case("file")
    || host.eq_ignore_ascii_case("javascript")
    || host.eq_ignore_ascii_case("mailto")
    || host.eq_ignore_ascii_case("sms")
    || host.eq_ignore_ascii_case("tel")
    || host.eq_ignore_ascii_case("view-source")
}

fn looks_like_windows_drive_path(input: &str) -> bool {
  let bytes = input.as_bytes();
  if bytes.len() < 3 {
    return false;
  }
  bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn looks_like_windows_unc_path(input: &str) -> bool {
  input.starts_with("\\\\")
}

fn file_url_from_user_input(input: &str) -> Option<String> {
  if looks_like_windows_unc_path(input) {
    return windows_unc_path_to_file_url(input);
  }
  if looks_like_windows_drive_path(input) {
    return windows_drive_path_to_file_url(input);
  }

  let path = Path::new(input);
  let abs = if path.is_absolute() {
    PathBuf::from(path)
  } else {
    std::env::current_dir().ok()?.join(path)
  };
  Url::from_file_path(abs).ok().map(|url| url.to_string())
}

fn windows_drive_path_to_file_url(input: &str) -> Option<String> {
  let bytes = input.as_bytes();
  if bytes.len() < 3 {
    return None;
  }
  if !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || (bytes[2] != b'\\' && bytes[2] != b'/')
  {
    return None;
  }

  // Treat both `C:\foo` and `C:/foo` as Windows drive paths and convert them into a file URL of the
  // form `file:///C:/foo`. This conversion is done manually (rather than via `Url::from_file_path`)
  // so it behaves consistently on non-Windows hosts too (where such strings would otherwise be
  // treated as relative POSIX paths like `./C:/foo`).
  let drive = bytes[0] as char;
  // Skip one or more path separators after the drive prefix so we also accept escaped strings like
  // `C:\\tmp\\a.html` (common when copying paths from JSON/debug output).
  let mut idx = 2usize; // after `C:`
  while idx < bytes.len() && (bytes[idx] == b'\\' || bytes[idx] == b'/') {
    idx += 1;
  }
  let rest = input.get(idx..)?.replace('\\', "/");
  // Collapse multiple consecutive separators for a cleaner file URL.
  let rest = rest
    .split('/')
    .filter(|segment| !segment.is_empty())
    .collect::<Vec<_>>()
    .join("/");

  let mut path = String::new();
  path.push('/');
  path.push(drive);
  path.push(':');
  path.push('/');
  path.push_str(&rest);

  let mut url = Url::parse("file:///").ok()?;
  url.set_path(&path);
  Some(url.to_string())
}

fn windows_unc_path_to_file_url(input: &str) -> Option<String> {
  // UNC paths look like `\\server\share\path`. Convert to `file://server/share/path`.
  // Accept extra leading backslashes so escaped strings like `\\\\server\\share` still work.
  let trimmed = input.trim_start_matches('\\');
  if trimmed.is_empty() {
    return None;
  }
  let normalized = trimmed.replace('\\', "/");
  let normalized = normalized
    .split('/')
    .filter(|segment| !segment.is_empty())
    .collect::<Vec<_>>()
    .join("/");
  let mut parts = normalized.splitn(3, '/');
  let server = parts.next()?.trim();
  let share = parts.next()?.trim();
  let rest = parts.next().unwrap_or("").trim();
  if server.is_empty() || share.is_empty() {
    return None;
  }

  let mut url = Url::parse(&format!("file://{server}/")).ok()?;
  let mut path = String::new();
  path.push('/');
  path.push_str(share);
  if !rest.is_empty() {
    path.push('/');
    path.push_str(rest);
  }
  url.set_path(&path);
  Some(url.to_string())
}

pub(crate) fn trim_ascii_whitespace(value: &str) -> &str {
  // This is a hot helper for omnibox/search-suggest paths. Implement it with byte scanning instead
  // of `trim_matches` to avoid per-char closure overhead.
  //
  // Note: this intentionally matches the HTML definition of ASCII whitespace (and mirrors the
  // previous `trim_matches` implementation).
  let bytes = value.as_bytes();
  let mut start = 0usize;
  let mut end = bytes.len();
  while start < end && matches!(bytes[start], b'\t' | b'\n' | b'\x0C' | b'\r' | b' ') {
    start += 1;
  }
  while end > start && matches!(bytes[end - 1], b'\t' | b'\n' | b'\x0C' | b'\r' | b' ') {
    end -= 1;
  }
  // `start`/`end` only move over ASCII bytes (1-byte UTF-8 chars), so slicing is safe.
  &value[start..end]
}

fn omnibox_input_looks_like_url_trimmed(input: &str) -> bool {
  if input.is_empty() {
    return false;
  }

  if input.as_bytes().iter().any(|b| b.is_ascii_whitespace()) {
    return false;
  }
  if input.contains("://") {
    return true;
  }
  if input
    .as_bytes()
    .get(.."about:".len())
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"about:"))
  {
    return true;
  }
  if looks_like_file_path(input) {
    return true;
  }
  if input.eq_ignore_ascii_case("localhost") {
    return true;
  }

  // Fast path: a dot almost always means a domain-like input (or an IPv4 literal), which the
  // omnibox should treat as a URL.
  if input.contains('.') {
    return true;
  }

  // Without a dot, the remaining URL-like forms we care about are host:port and IPv6 literals.
  // If there is no colon, there's nothing left to classify as a URL.
  if !input.contains(':') && !input.starts_with('[') {
    return false;
  }

  // Host:port (including bracketed IPv6 host:port).
  if looks_like_host_port_without_scheme(input)
    || looks_like_bracketed_ipv6_host_port_without_scheme(input)
  {
    return true;
  }

  // Finally, treat bare IPv6 literals as URLs.
  parse_ip_literal_trimmed(input).is_some()
}

/// Heuristic for deciding whether a user-typed omnibox string should be treated as a URL
/// (navigate) or a search query (search fallback).
///
/// The input is first trimmed of ASCII whitespace (space, tab, newlines). After trimming:
///
/// - If input contains any ASCII whitespace → Search
/// - If input contains `://` → URL
/// - If input begins with `about:` (case-insensitive) → URL
/// - If input looks like a filesystem path → URL
/// - If input is `localhost` (case-insensitive) → URL
/// - If input contains a dot (`.`) → URL (domain-like input or IPv4 literal)
/// - If input looks like `host:port` (without scheme) → URL
/// - If input parses as an IPv6 literal → URL
/// - Otherwise → Search
pub fn omnibox_input_looks_like_url(input: &str) -> bool {
  omnibox_input_looks_like_url_trimmed(trim_ascii_whitespace(input))
}

pub fn resolve_omnibox_input(input: &str) -> Result<OmniboxInputResolution, String> {
  resolve_omnibox_input_with_search_template(input, DEFAULT_SEARCH_ENGINE_TEMPLATE)
}

/// Fast path for omnibox/search-suggest code paths that only need to know whether the user input
/// should be treated as a search query.
///
/// This avoids constructing a full search-engine URL (percent encoding, `Url::parse`, etc.) which
/// is relatively expensive on hot per-keystroke UI paths (omnibox suggestion building, remote
/// suggest request scheduling).
///
/// Returns the trimmed query as a borrowed `&str` when the input is a search, or `None` when the
/// input should be treated as a URL.
pub fn resolve_omnibox_search_query(input: &str) -> Option<&str> {
  let input = trim_ascii_whitespace(input);
  if input.is_empty() {
    return None;
  }

  if has_explicit_scheme(input) {
    return None;
  }

  if omnibox_input_looks_like_url_trimmed(input) {
    return None;
  }

  Some(input)
}

pub fn resolve_omnibox_input_with_search_template(
  input: &str,
  search_engine_template: &str,
) -> Result<OmniboxInputResolution, String> {
  let input = trim_ascii_whitespace(input);
  if input.is_empty() {
    return Err("empty URL".to_string());
  }

  if has_explicit_scheme(input) {
    return Ok(OmniboxInputResolution::Url {
      url: normalize_user_url(input)?,
    });
  }

  if let Some(ip) = parse_ip_literal_trimmed(input) {
    return Ok(OmniboxInputResolution::Url {
      url: https_url_from_ip_literal(ip)?,
    });
  }

  if omnibox_input_looks_like_url(input) {
    return Ok(OmniboxInputResolution::Url {
      url: normalize_user_url(input)?,
    });
  }

  let query = input.to_string();
  let url = search_url_for_query(&query, search_engine_template)?;
  Ok(OmniboxInputResolution::Search { query, url })
}

pub fn search_url_for_query(query: &str, template: &str) -> Result<String, String> {
  if !template.contains("{query}") {
    return Err("search engine template missing `{query}` placeholder".to_string());
  }
  let encoded = urlencoding::encode(query);
  let url_str = template.replace("{query}", encoded.as_ref());
  Url::parse(&url_str)
    .map(|url| url.to_string())
    .map_err(|err| err.to_string())
}

fn parse_ip_literal_trimmed(trimmed: &str) -> Option<IpAddr> {
  if trimmed.is_empty() {
    return None;
  }

  let candidate = trimmed
    .strip_prefix('[')
    .and_then(|s| s.strip_suffix(']'))
    .unwrap_or(trimmed);
  IpAddr::from_str(candidate).ok()
}

fn parse_ip_literal(input: &str) -> Option<IpAddr> {
  parse_ip_literal_trimmed(trim_ascii_whitespace(input))
}

fn looks_like_bracketed_ipv6_host_port_without_scheme(input: &str) -> bool {
  if input.contains("://") || input.as_bytes().iter().any(|b| b.is_ascii_whitespace()) {
    return false;
  }
  let Some((host, rest)) = input.split_once("]:") else {
    return false;
  };
  if !host.starts_with('[') {
    return false;
  }
  let host = host.trim_start_matches('[');
  if IpAddr::from_str(host).is_err() {
    return false;
  }
  rest
    .as_bytes()
    .first()
    .is_some_and(|ch| ch.is_ascii_digit())
}

fn https_url_from_ip_literal(ip: IpAddr) -> Result<String, String> {
  let host = match ip {
    IpAddr::V4(v4) => v4.to_string(),
    IpAddr::V6(v6) => format!("[{v6}]"),
  };
  let url_str = format!("https://{host}/");
  Url::parse(&url_str)
    .map(|url| url.to_string())
    .map_err(|err| err.to_string())
}

fn has_explicit_scheme(input: &str) -> bool {
  // Fast path: without a colon, there is no scheme separator.
  let Some((scheme, _rest)) = input.split_once(':') else {
    return false;
  };

  // Treat obvious host:port values and IP literals as non-explicit-scheme inputs so they can be
  // handled like normal URLs in an omnibox.
  if looks_like_host_port_without_scheme(input)
    || looks_like_bracketed_ipv6_host_port_without_scheme(input)
    || parse_ip_literal_trimmed(input).is_some()
    || looks_like_file_path(input)
  {
    return false;
  }

  let bytes = scheme.as_bytes();
  if bytes.is_empty() {
    return false;
  }
  if !bytes[0].is_ascii_alphabetic() {
    return false;
  }
  bytes
    .iter()
    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
  use super::{
    http_fallback_url_for_failed_https, navigation_to_file_is_allowed, normalize_user_url,
    omnibox_input_looks_like_url, resolve_link_url, resolve_omnibox_input,
    resolve_omnibox_search_query, sanitize_worker_url_for_ui,
    validate_trusted_chrome_navigation_url_scheme, validate_user_navigation_url_scheme,
    OmniboxInputResolution,
  };
  use crate::ui::NavigationReason;

  #[test]
  fn bare_domain_defaults_to_https() {
    assert_eq!(
      normalize_user_url("example.com").unwrap(),
      "https://example.com/"
    );
  }

  #[test]
  fn scheme_is_preserved() {
    assert_eq!(
      normalize_user_url("https://example.com").unwrap(),
      "https://example.com/"
    );
  }

  #[test]
  fn trims_ascii_whitespace() {
    assert_eq!(
      normalize_user_url("  https://example.com  ").unwrap(),
      "https://example.com/"
    );
  }

  #[test]
  fn javascript_scheme_is_not_rejected_by_normalization() {
    assert_eq!(
      normalize_user_url("javascript:alert(1)").unwrap(),
      "javascript:alert(1)"
    );
  }

  #[test]
  fn about_urls_are_preserved() {
    assert_eq!(normalize_user_url("about:blank").unwrap(), "about:blank");
  }

  #[test]
  fn user_navigation_scheme_validation_rejects_javascript() {
    assert!(validate_user_navigation_url_scheme("javascript:alert(1)").is_err());
  }

  #[test]
  fn user_navigation_scheme_validation_rejects_unknown_scheme() {
    assert!(validate_user_navigation_url_scheme("foo:bar").is_err());
  }

  #[test]
  fn user_navigation_scheme_validation_rejects_privileged_renderer_chrome_schemes() {
    // These schemes are reserved for the trusted browser-process chrome renderer and must not be
    // accepted for user/content navigations.
    let err = validate_user_navigation_url_scheme("chrome://styles/chrome.css").unwrap_err();
    assert!(
      err.to_ascii_lowercase().contains("chrome://"),
      "expected error to mention chrome://, got: {err}"
    );

    let err = validate_user_navigation_url_scheme("chrome-action:new-tab").unwrap_err();
    assert!(
      err.to_ascii_lowercase().contains("unsupported") && err.contains("chrome-action"),
      "unexpected error for chrome-action:: {err}"
    );

    let err = validate_user_navigation_url_scheme("chrome-dialog:accept").unwrap_err();
    assert!(
      err.to_ascii_lowercase().contains("unsupported") && err.contains("chrome-dialog"),
      "unexpected error for chrome-dialog:: {err}"
    );
  }

  #[test]
  fn user_navigation_scheme_validation_allows_about_and_https() {
    assert!(validate_user_navigation_url_scheme("about:newtab").is_ok());
    assert!(validate_user_navigation_url_scheme("https://example.com/").is_ok());
  }

  #[test]
  fn open_in_new_tab_ipc_rejects_chrome_scheme_for_untrusted_content() {
    // `WorkerToUi::RequestOpenInNewTab` (renderer → browser IPC) must not be able to ask the browser
    // to open a privileged `chrome://` page.
    assert!(validate_user_navigation_url_scheme("chrome://icons/back.svg").is_err());
  }

  #[test]
  fn trusted_chrome_navigation_scheme_validation_allows_chrome_scheme() {
    assert!(
      validate_trusted_chrome_navigation_url_scheme("chrome://styles/chrome.css").is_ok(),
      "trusted chrome navigations should allow chrome://"
    );
  }

  #[test]
  fn sanitize_worker_url_for_ui_allows_http_https_file_about() {
    assert_eq!(
      sanitize_worker_url_for_ui("https://example.com/path").as_deref(),
      Some("https://example.com/path")
    );
    assert_eq!(
      sanitize_worker_url_for_ui("http://example.com").as_deref(),
      Some("http://example.com/")
    );
    assert_eq!(
      sanitize_worker_url_for_ui("about:newtab").as_deref(),
      Some("about:newtab")
    );
    assert_eq!(
      sanitize_worker_url_for_ui("file:///tmp/a.html").as_deref(),
      Some("file:///tmp/a.html")
    );
  }

  #[test]
  fn sanitize_worker_url_for_ui_rejects_disallowed_schemes_and_invalid_urls() {
    assert_eq!(
      sanitize_worker_url_for_ui("javascript:alert(1)"),
      None,
      "javascript: must be rejected"
    );
    assert_eq!(
      sanitize_worker_url_for_ui("data:text/plain,hi"),
      None,
      "data: must be rejected"
    );
    assert_eq!(
      sanitize_worker_url_for_ui("http://[::1"),
      None,
      "invalid URL must be rejected"
    );
  }

  #[test]
  fn sanitize_worker_url_for_ui_enforces_max_length() {
    let max = crate::ui::protocol_limits::MAX_URL_BYTES;
    let mut s = String::new();
    // Ensure we exceed the byte limit even on non-ASCII hosts by using ASCII.
    s.push_str("https://example.com/");
    while s.as_bytes().len() <= max {
      s.push('a');
    }
    assert!(sanitize_worker_url_for_ui(&s).is_none());
  }

  #[test]
  fn windows_drive_paths_are_converted_to_file_urls() {
    assert_eq!(
      normalize_user_url(r"C:\tmp\a.html").unwrap(),
      "file:///C:/tmp/a.html"
    );
    assert_eq!(
      normalize_user_url(r"C:\\tmp\\a.html").unwrap(),
      "file:///C:/tmp/a.html"
    );
    assert_eq!(
      normalize_user_url("C:/tmp/a.html").unwrap(),
      "file:///C:/tmp/a.html"
    );
  }

  #[test]
  fn windows_unc_paths_are_converted_to_file_urls() {
    assert_eq!(
      normalize_user_url(r"\\server\share\a.html").unwrap(),
      "file://server/share/a.html"
    );
    assert_eq!(
      normalize_user_url(r"\\\\server\\share\\a.html").unwrap(),
      "file://server/share/a.html"
    );
  }

  #[cfg(unix)]
  #[test]
  fn posix_paths_are_converted_to_file_urls() {
    assert_eq!(
      normalize_user_url("/tmp/a.html").unwrap(),
      "file:///tmp/a.html"
    );
  }

  #[test]
  fn resolve_link_url_resolves_relative_against_base() {
    assert_eq!(
      resolve_link_url("https://example.com/dir/page.html", "other.html").as_deref(),
      Some("https://example.com/dir/other.html")
    );
  }

  #[test]
  fn resolve_link_url_preserves_fragment_only_links() {
    assert_eq!(
      resolve_link_url("https://example.com/dir/page.html", "#frag").as_deref(),
      Some("https://example.com/dir/page.html#frag")
    );
  }

  #[test]
  fn resolve_link_url_empty_href_resolves_to_base_url() {
    assert_eq!(
      resolve_link_url("https://example.com/dir/page.html", "").as_deref(),
      Some("https://example.com/dir/page.html")
    );
    assert_eq!(
      resolve_link_url("https://example.com/dir/page.html", "   ").as_deref(),
      Some("https://example.com/dir/page.html")
    );
  }

  #[test]
  fn resolve_link_url_rejects_javascript_scheme() {
    assert_eq!(
      resolve_link_url("https://example.com/dir/page.html", "javascript:alert(1)"),
      None
    );
  }

  #[test]
  fn http_fallback_preserves_url_components() {
    assert_eq!(
      http_fallback_url_for_failed_https("https://example.com/path?x=1#y").as_deref(),
      Some("http://example.com/path?x=1#y")
    );
  }

  #[test]
  fn host_port_is_treated_as_https_url() {
    assert_eq!(
      normalize_user_url("localhost:3000").unwrap(),
      "https://localhost:3000/"
    );
    assert_eq!(
      normalize_user_url("devbox:3000").unwrap(),
      "https://devbox:3000/"
    );
    assert_eq!(
      normalize_user_url("example.com:8080/path").unwrap(),
      "https://example.com:8080/path"
    );
  }

  #[test]
  fn opaque_scheme_like_inputs_are_not_rewritten_as_host_port() {
    // `foo:bar` should remain an opaque URL so callers can provide a meaningful unsupported-scheme
    // error message.
    assert_eq!(normalize_user_url("foo:bar").unwrap(), "foo:bar");

    // Explicit schemes should not be mistaken for missing-scheme host:port values.
    let mailto = normalize_user_url("mailto:someone@example.com").unwrap();
    assert_eq!(mailto, "mailto:someone@example.com");
    assert!(validate_user_navigation_url_scheme(&mailto).is_err());

    let javascript = normalize_user_url("javascript:1").unwrap();
    assert_eq!(javascript, "javascript:1");
    assert!(validate_user_navigation_url_scheme(&javascript).is_err());
  }

  #[test]
  fn omnibox_heuristic_treats_words_as_search_and_domains_as_urls() {
    assert!(!omnibox_input_looks_like_url("cats"));
    assert!(!omnibox_input_looks_like_url("cats dogs"));
    assert!(omnibox_input_looks_like_url("example.com"));
    assert!(omnibox_input_looks_like_url("localhost"));
    assert!(omnibox_input_looks_like_url("127.0.0.1"));
    assert!(omnibox_input_looks_like_url("localhost:3000"));
    assert!(omnibox_input_looks_like_url("devbox:3000"));
    assert!(omnibox_input_looks_like_url("about:help"));
  }

  #[test]
  fn omnibox_resolves_search_queries_to_duckduckgo() {
    let resolved = resolve_omnibox_input("cats").unwrap();
    assert_eq!(resolved.query(), Some("cats"));
    assert_eq!(resolved.url(), "https://duckduckgo.com/?q=cats");
  }

  #[test]
  fn omnibox_resolves_queries_with_whitespace_to_search() {
    match resolve_omnibox_input("cats dogs").unwrap() {
      OmniboxInputResolution::Search { query, url } => {
        assert_eq!(query, "cats dogs");
        assert_eq!(url, "https://duckduckgo.com/?q=cats%20dogs");
      }
      other => panic!("expected Search, got {other:?}"),
    }
  }

  #[test]
  fn omnibox_resolves_domains_and_hosts_to_urls() {
    match resolve_omnibox_input("example.com").unwrap() {
      OmniboxInputResolution::Url { url } => assert_eq!(url, "https://example.com/"),
      other => panic!("expected Url, got {other:?}"),
    }
    match resolve_omnibox_input("localhost").unwrap() {
      OmniboxInputResolution::Url { url } => assert_eq!(url, "https://localhost/"),
      other => panic!("expected Url, got {other:?}"),
    }
    match resolve_omnibox_input("127.0.0.1").unwrap() {
      OmniboxInputResolution::Url { url } => assert_eq!(url, "https://127.0.0.1/"),
      other => panic!("expected Url, got {other:?}"),
    }
    match resolve_omnibox_input("localhost:3000").unwrap() {
      OmniboxInputResolution::Url { url } => assert_eq!(url, "https://localhost:3000/"),
      other => panic!("expected Url, got {other:?}"),
    }
    match resolve_omnibox_input("about:help").unwrap() {
      OmniboxInputResolution::Url { url } => assert_eq!(url, "about:help"),
      other => panic!("expected Url, got {other:?}"),
    }
  }

  #[test]
  fn omnibox_does_not_search_explicit_schemes() {
    let resolved = resolve_omnibox_input("javascript:alert(1)").unwrap();
    assert_eq!(resolved.query(), None);
    assert!(validate_user_navigation_url_scheme(resolved.url()).is_err());

    let resolved = resolve_omnibox_input("foo:bar").unwrap();
    assert_eq!(resolved.query(), None);
    assert!(validate_user_navigation_url_scheme(resolved.url()).is_err());
  }

  #[test]
  fn omnibox_search_percent_encodes_special_chars() {
    match resolve_omnibox_input("a&b").unwrap() {
      OmniboxInputResolution::Search { query, url } => {
        assert_eq!(query, "a&b");
        assert_eq!(url, "https://duckduckgo.com/?q=a%26b");
      }
      other => panic!("expected Search, got {other:?}"),
    }
  }

  #[test]
  fn omnibox_search_query_helper_classifies_search_without_building_url() {
    assert_eq!(resolve_omnibox_search_query("cats"), Some("cats"));
    assert_eq!(
      resolve_omnibox_search_query("  cats dogs  "),
      Some("cats dogs")
    );
    assert_eq!(resolve_omnibox_search_query("example.com"), None);
    assert_eq!(resolve_omnibox_search_query("localhost"), None);
    assert_eq!(resolve_omnibox_search_query("http://localhost"), None);
    assert_eq!(resolve_omnibox_search_query("about:help"), None);
  }

  #[test]
  fn file_navigation_is_blocked_from_http_link_clicks() {
    assert!(
      !navigation_to_file_is_allowed(
        NavigationReason::LinkClick,
        Some("https://example.com/")
      ),
      "expected file:// navigation to be blocked when initiated by a link click from an https page"
    );
  }

  #[test]
  fn file_navigation_is_allowed_for_typed_urls() {
    assert!(
      navigation_to_file_is_allowed(
        NavigationReason::TypedUrl,
        Some("https://example.com/")
      ),
      "expected file:// navigation to be allowed when explicitly typed by the user"
    );
  }

  #[test]
  fn file_navigation_is_allowed_from_file_pages() {
    assert!(
      navigation_to_file_is_allowed(
        NavigationReason::LinkClick,
        Some("file:///tmp/a.html")
      ),
      "expected file:// -> file:// navigations via link click to be allowed"
    );
  }
}
