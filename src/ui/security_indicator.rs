/// Scheme → security indicator mapping for the address bar.
///
/// This is *visual only* and must not affect navigation logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityIndicator {
  /// HTTPS.
  Secure,
  /// HTTP (insecure).
  Insecure,
  /// Local/internal (file/about/unknown).
  Neutral,
}

impl SecurityIndicator {
  pub fn icon(self) -> &'static str {
    match self {
      SecurityIndicator::Secure => "🔒",
      SecurityIndicator::Insecure => "⚠",
      SecurityIndicator::Neutral => "ⓘ",
    }
  }

  pub fn tooltip(self) -> &'static str {
    match self {
      SecurityIndicator::Secure => "Secure (https)",
      SecurityIndicator::Insecure => "Not secure (http)",
      SecurityIndicator::Neutral => "Local or internal page",
    }
  }
}

pub fn indicator_for_url(url: &str) -> SecurityIndicator {
  let scheme = url
    .split_once(':')
    .map(|(scheme, _rest)| scheme)
    .unwrap_or("");

  match scheme.to_ascii_lowercase().as_str() {
    "https" => SecurityIndicator::Secure,
    "http" => SecurityIndicator::Insecure,
    "file" | "about" => SecurityIndicator::Neutral,
    _ => SecurityIndicator::Neutral,
  }
}

#[cfg(test)]
mod tests {
  use super::{indicator_for_url, SecurityIndicator};

  #[test]
  fn https_is_secure() {
    assert_eq!(
      indicator_for_url("https://example.com/"),
      SecurityIndicator::Secure
    );
  }

  #[test]
  fn http_is_insecure() {
    assert_eq!(
      indicator_for_url("http://example.com/"),
      SecurityIndicator::Insecure
    );
  }

  #[test]
  fn file_and_about_are_neutral() {
    assert_eq!(
      indicator_for_url("file:///tmp/a.html"),
      SecurityIndicator::Neutral
    );
    assert_eq!(indicator_for_url("about:newtab"), SecurityIndicator::Neutral);
  }

  #[test]
  fn unknown_schemes_are_neutral() {
    assert_eq!(indicator_for_url("foo:bar"), SecurityIndicator::Neutral);
  }
}

