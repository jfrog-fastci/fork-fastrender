use std::path::{Path, PathBuf};
use url::Url;

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
/// should apply a scheme allowlist before attempting navigation/fetch.
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

fn looks_like_file_path(input: &str) -> bool {
  input.starts_with('/') || input.starts_with("./") || input.starts_with("../") || looks_like_windows_drive_path(input)
}

fn looks_like_windows_drive_path(input: &str) -> bool {
  let bytes = input.as_bytes();
  if bytes.len() < 3 {
    return false;
  }
  bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn file_url_from_user_input(input: &str) -> Option<String> {
  let path = Path::new(input);
  let abs = if path.is_absolute() {
    PathBuf::from(path)
  } else {
    std::env::current_dir().ok()?.join(path)
  };
  Url::from_file_path(abs).ok().map(|url| url.to_string())
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

#[cfg(test)]
mod tests {
  use super::normalize_user_url;

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

  #[cfg(unix)]
  #[test]
  fn posix_paths_are_converted_to_file_urls() {
    assert_eq!(
      normalize_user_url("/tmp/a.html").unwrap(),
      "file:///tmp/a.html"
    );
  }
}

