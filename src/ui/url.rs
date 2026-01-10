use std::path::{Path, PathBuf};
use url::Url;

/// Validate that a normalized, user-supplied URL uses a supported scheme.
///
/// This is intended for URLs originating from the address bar / command-line `browser [<url>]`
/// argument. It rejects opaque schemes like `javascript:` even though `url::Url` can parse them.
///
/// For programmatic navigations (e.g. link clicks) the worker still validates schemes independently.
pub fn validate_user_navigation_url_scheme(url: &str) -> Result<(), String> {
  let parsed = Url::parse(url).map_err(|err| err.to_string())?;
  let scheme = parsed.scheme().to_ascii_lowercase();
  match scheme.as_str() {
    "http" | "https" | "file" | "about" => Ok(()),
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
    Ok(url) => Ok(url.to_string()),
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
  if !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || (bytes[2] != b'\\' && bytes[2] != b'/') {
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

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

#[cfg(test)]
mod tests {
  use super::{normalize_user_url, resolve_link_url, validate_user_navigation_url_scheme};

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
}
