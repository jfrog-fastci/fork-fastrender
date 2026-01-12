use publicsuffix::{List, Psl};
use std::sync::OnceLock;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressBarSecurityState {
  Https,
  Http,
  File,
  About,
  Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressBarDisplayParts {
  /// Primary host/domain portion of the URL.
  ///
  /// For HTTP(S) URLs, this is a registrable domain when possible.
  ///
  /// For non-host URLs (about/file/invalid), this contains a best-effort display string.
  pub display_host: String,
  /// Optional remainder of the URL (path, query and fragment).
  pub display_path_query_fragment: Option<String>,
  pub security_state: AddressBarSecurityState,
}

pub fn format_address_bar_url(raw: &str) -> AddressBarDisplayParts {
  let raw = raw.trim();
  if raw.is_empty() {
    return AddressBarDisplayParts {
      display_host: String::new(),
      display_path_query_fragment: None,
      security_state: AddressBarSecurityState::Other,
    };
  }

  match Url::parse(raw) {
    Ok(url) => format_parsed_url(&url, raw),
    Err(_) => AddressBarDisplayParts {
      display_host: raw.to_string(),
      display_path_query_fragment: None,
      security_state: AddressBarSecurityState::Other,
    },
  }
}

fn format_parsed_url(url: &Url, raw: &str) -> AddressBarDisplayParts {
  let scheme = url.scheme();
  if scheme.eq_ignore_ascii_case("https") {
    return format_http_like_url(url, AddressBarSecurityState::Https, raw);
  }
  if scheme.eq_ignore_ascii_case("http") {
    return format_http_like_url(url, AddressBarSecurityState::Http, raw);
  }
  if scheme.eq_ignore_ascii_case("about") {
    return AddressBarDisplayParts {
      display_host: url.to_string(),
      display_path_query_fragment: None,
      security_state: AddressBarSecurityState::About,
    };
  }
  if scheme.eq_ignore_ascii_case("file") {
    return AddressBarDisplayParts {
      display_host: format_file_url(url),
      display_path_query_fragment: None,
      security_state: AddressBarSecurityState::File,
    };
  }

  // Unknown/unsupported scheme. Keep the string intact; this is just for display.
  AddressBarDisplayParts {
    display_host: url.to_string(),
    display_path_query_fragment: None,
    security_state: AddressBarSecurityState::Other,
  }
}

fn format_http_like_url(
  url: &Url,
  security_state: AddressBarSecurityState,
  raw: &str,
) -> AddressBarDisplayParts {
  let Some(host) = url.host_str() else {
    return AddressBarDisplayParts {
      display_host: raw.to_string(),
      display_path_query_fragment: None,
      security_state: AddressBarSecurityState::Other,
    };
  };

  let mut display_host = registrable_domain(host).unwrap_or_else(|| host.to_string());

  if display_host.contains(':') && !display_host.starts_with('[') {
    // Likely an IPv6 literal.
    display_host = format!("[{display_host}]");
  }

  if let Some(port) = url.port() {
    display_host.push(':');
    display_host.push_str(&port.to_string());
  }

  let path = url.path();
  let query = url.query();
  let fragment = url.fragment();

  let mut path_query_fragment = String::new();
  // Only show a bare `/` when it is necessary to anchor query/fragment display.
  if path != "/" || query.is_some() || fragment.is_some() {
    path_query_fragment.push_str(path);
  }
  if let Some(q) = query {
    path_query_fragment.push('?');
    path_query_fragment.push_str(q);
  }
  if let Some(f) = fragment {
    path_query_fragment.push('#');
    path_query_fragment.push_str(f);
  }

  AddressBarDisplayParts {
    display_host,
    display_path_query_fragment: (!path_query_fragment.is_empty()).then_some(path_query_fragment),
    security_state,
  }
}

fn registrable_domain(host: &str) -> Option<String> {
  static PSL: OnceLock<List> = OnceLock::new();
  let list = PSL.get_or_init(List::default);
  let domain = list.domain(host.as_bytes())?;
  std::str::from_utf8(domain.as_bytes()).ok().map(str::to_string)
}

fn format_file_url(url: &Url) -> String {
  let mut out = String::new();
  out.push_str("file://");

  if let Some(host) = url.host_str().filter(|h| !h.is_empty()) {
    out.push_str(host);
  }

  out.push_str(&elide_file_path(url.path()));

  if let Some(q) = url.query() {
    out.push('?');
    out.push_str(q);
  }
  if let Some(f) = url.fragment() {
    out.push('#');
    out.push_str(f);
  }

  out
}

fn elide_file_path(path: &str) -> String {
  let mut segments = path
    .split('/')
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>();

  // Keep file URLs readable by eliding very deep paths:
  // `/a/b/c/d/e` -> `/a/b/…/d/e`
  const MAX_SEGMENTS: usize = 4;
  if segments.len() > MAX_SEGMENTS {
    let keep_head = 2usize;
    let keep_tail = 2usize;
    if segments.len() > keep_head + keep_tail {
      let mut elided = Vec::with_capacity(keep_head + 1 + keep_tail);
      elided.extend_from_slice(&segments[..keep_head]);
      elided.push("…");
      elided.extend_from_slice(&segments[segments.len() - keep_tail..]);
      segments = elided;
    }
  }

  let mut out = String::new();
  out.push('/');
  out.push_str(&segments.join("/"));
  out
}

#[cfg(test)]
mod tests {
  use super::{format_address_bar_url, AddressBarSecurityState};

  #[test]
  fn https_url_is_split_into_host_and_path_query_fragment() {
    let formatted = format_address_bar_url("https://example.com/path?x=1#y");
    assert_eq!(formatted.security_state, AddressBarSecurityState::Https);
    assert_eq!(formatted.display_host, "example.com");
    assert_eq!(
      formatted.display_path_query_fragment.as_deref(),
      Some("/path?x=1#y")
    );
  }

  #[test]
  fn http_url_shows_not_secure_state_and_hides_trivial_path() {
    let formatted = format_address_bar_url("http://example.com/");
    assert_eq!(formatted.security_state, AddressBarSecurityState::Http);
    assert_eq!(formatted.display_host, "example.com");
    assert_eq!(formatted.display_path_query_fragment, None);
  }

  #[test]
  fn about_url_is_displayed_as_is() {
    let formatted = format_address_bar_url("about:newtab");
    assert_eq!(formatted.security_state, AddressBarSecurityState::About);
    assert_eq!(formatted.display_host, "about:newtab");
    assert_eq!(formatted.display_path_query_fragment, None);
  }

  #[test]
  fn file_url_windows_drive_is_preserved() {
    let formatted = format_address_bar_url("file:///C:/tmp/a.html");
    assert_eq!(formatted.security_state, AddressBarSecurityState::File);
    assert_eq!(formatted.display_host, "file:///C:/tmp/a.html");
  }

  #[test]
  fn file_url_posix_is_preserved() {
    let formatted = format_address_bar_url("file:///tmp/a.html");
    assert_eq!(formatted.security_state, AddressBarSecurityState::File);
    assert_eq!(formatted.display_host, "file:///tmp/a.html");
  }

  #[test]
  fn invalid_url_falls_back_to_raw_string() {
    let formatted = format_address_bar_url("not a url");
    assert_eq!(formatted.security_state, AddressBarSecurityState::Other);
    assert_eq!(formatted.display_host, "not a url");
    assert_eq!(formatted.display_path_query_fragment, None);
  }
}
