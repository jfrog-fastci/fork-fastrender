use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::str::FromStr;
use std::path::{Path, PathBuf};
use url::Url;

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

fn crash_urls_allowed() -> bool {
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
/// as `chrome://` (built-in UI assets) and `chrome-action:` (chrome UI actions). See
/// `docs/renderer_chrome_schemes.md`.
///
/// For programmatic navigations (e.g. link clicks) the worker still validates schemes independently.
pub fn validate_user_navigation_url_scheme(url: &str) -> Result<(), String> {
  let parsed = Url::parse(url).map_err(|err| err.to_string())?;
  let scheme = parsed.scheme().to_ascii_lowercase();
  match scheme.as_str() {
    "http" | "https" | "file" | "about" => Ok(()),
    "crash" if crash_urls_allowed() => Ok(()),
    "javascript" => Err("navigation to javascript: URLs is not supported".to_string()),
    _ => Err(format!("unsupported URL scheme: {scheme}")),
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
/// Returns `None` when `href` is empty, cannot be resolved, or resolves to a `javascript:` URL (the
/// browser UI does not execute JavaScript).
pub fn resolve_link_url(base_url: &str, href: &str) -> Option<String> {
  let href = trim_ascii_whitespace(href);
  if href.is_empty() {
    return None;
  }

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
  }

  let absolute = Url::parse(href).ok()?;
  (!absolute.scheme().eq_ignore_ascii_case("javascript")).then(|| absolute.to_string())
}

fn looks_like_file_path(input: &str) -> bool {
  input.starts_with('/')
    || input.starts_with("./")
    || input.starts_with("../")
    || looks_like_windows_drive_path(input)
    || looks_like_windows_unc_path(input)
}

fn looks_like_host_port_without_scheme(input: &str) -> bool {
  if input.contains("://") || input.contains(' ') {
    return false;
  }
  let Some((host, rest)) = input.split_once(':') else {
    return false;
  };

  // Be conservative: only treat obvious hostname inputs as host:port. This covers the common dev
  // cases (`localhost:3000`, `example.com:8080`) without misclassifying arbitrary `foo:bar` opaque
  // URLs as missing-scheme HTTPS URLs.
  let host = host.trim();
  if !(host.eq_ignore_ascii_case("localhost") || host.contains('.')) {
    return false;
  }

  let rest = rest.trim();
  // A host:port candidate must have a numeric port immediately after the colon.
  rest
    .as_bytes()
    .first()
    .is_some_and(|ch| ch.is_ascii_digit())
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
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
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
/// - If input parses as an IPv4/IPv6 literal → URL
/// - If input looks like `host:port` (without scheme) → URL
/// - If input contains a dot (`.`) → URL
/// - Otherwise → Search
pub fn omnibox_input_looks_like_url(input: &str) -> bool {
  let input = trim_ascii_whitespace(input);
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
  if parse_ip_literal(input).is_some() {
    return true;
  }
  if looks_like_host_port_without_scheme(input) || looks_like_bracketed_ipv6_host_port_without_scheme(input) {
    return true;
  }
  input.contains('.')
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

  if omnibox_input_looks_like_url(input) {
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

  if let Some(ip) = parse_ip_literal(input) {
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

fn parse_ip_literal(input: &str) -> Option<IpAddr> {
  let trimmed = trim_ascii_whitespace(input);
  if trimmed.is_empty() {
    return None;
  }

  let candidate = trimmed
    .strip_prefix('[')
    .and_then(|s| s.strip_suffix(']'))
    .unwrap_or(trimmed);
  IpAddr::from_str(candidate).ok()
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
  // Treat obvious host:port values and IP literals as non-explicit-scheme inputs so they can be
  // handled like normal URLs in an omnibox.
  if looks_like_host_port_without_scheme(input)
    || looks_like_bracketed_ipv6_host_port_without_scheme(input)
    || parse_ip_literal(input).is_some()
    || looks_like_file_path(input)
  {
    return false;
  }

  let Some((scheme, _rest)) = input.split_once(':') else {
    return false;
  };
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
    normalize_user_url, omnibox_input_looks_like_url, resolve_link_url, resolve_omnibox_input,
    resolve_omnibox_search_query, validate_user_navigation_url_scheme, OmniboxInputResolution,
  };

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
  fn user_navigation_scheme_validation_allows_about_and_https() {
    assert!(validate_user_navigation_url_scheme("about:newtab").is_ok());
    assert!(validate_user_navigation_url_scheme("https://example.com/").is_ok());
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
  fn resolve_link_url_rejects_javascript_scheme() {
    assert_eq!(
      resolve_link_url("https://example.com/dir/page.html", "javascript:alert(1)"),
      None
    );
  }

  #[test]
  fn host_port_is_treated_as_https_url() {
    assert_eq!(
      normalize_user_url("localhost:3000").unwrap(),
      "https://localhost:3000/"
    );
    assert_eq!(
      normalize_user_url("example.com:8080/path").unwrap(),
      "https://example.com:8080/path"
    );
  }

  #[test]
  fn omnibox_heuristic_treats_words_as_search_and_domains_as_urls() {
    assert!(!omnibox_input_looks_like_url("cats"));
    assert!(!omnibox_input_looks_like_url("cats dogs"));
    assert!(omnibox_input_looks_like_url("example.com"));
    assert!(omnibox_input_looks_like_url("localhost"));
    assert!(omnibox_input_looks_like_url("127.0.0.1"));
    assert!(omnibox_input_looks_like_url("localhost:3000"));
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
}
