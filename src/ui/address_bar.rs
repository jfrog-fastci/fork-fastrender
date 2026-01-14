use publicsuffix::{List, Psl};
use std::sync::OnceLock;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AddressBarSecurityState {
  Https,
  Http,
  File,
  About,
  #[default]
  Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressBarDisplayParts {
  /// Weakly-emphasised host prefix (subdomains + trailing dot) for HTTP(S) URLs.
  ///
  /// Empty for non-host URLs (about/file/invalid) or when the host has no subdomain component.
  pub display_host_prefix: String,
  /// Primary host/domain portion of the URL.
  ///
  /// For HTTP(S) URLs, this is the registrable domain (eTLD+1) when possible, otherwise the full
  /// host (IPv4/IPv6/localhost/etc).
  ///
  /// For non-host URLs (about/file/invalid), this contains a best-effort display string.
  pub display_host_domain: String,
  /// Weakly-emphasised host suffix (currently `:port`) for HTTP(S) URLs.
  pub display_host_suffix: String,
  /// Optional remainder of the URL (path, query and fragment).
  pub display_path_query_fragment: Option<String>,
  pub security_state: AddressBarSecurityState,
}

pub fn format_address_bar_url(raw: &str) -> AddressBarDisplayParts {
  let raw = raw.trim();
  if raw.is_empty() {
    return AddressBarDisplayParts {
      display_host_prefix: String::new(),
      display_host_domain: String::new(),
      display_host_suffix: String::new(),
      display_path_query_fragment: None,
      security_state: AddressBarSecurityState::Other,
    };
  }

  match Url::parse(raw) {
    Ok(url) => format_parsed_url(&url, raw),
    Err(_) => AddressBarDisplayParts {
      display_host_prefix: String::new(),
      display_host_domain: raw.to_string(),
      display_host_suffix: String::new(),
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
      display_host_prefix: String::new(),
      display_host_domain: url.to_string(),
      display_host_suffix: String::new(),
      display_path_query_fragment: None,
      security_state: AddressBarSecurityState::About,
    };
  }
  if scheme.eq_ignore_ascii_case("file") {
    return AddressBarDisplayParts {
      display_host_prefix: String::new(),
      display_host_domain: format_file_url(url),
      display_host_suffix: String::new(),
      display_path_query_fragment: None,
      security_state: AddressBarSecurityState::File,
    };
  }

  // Unknown/unsupported scheme. Keep the string intact; this is just for display.
  AddressBarDisplayParts {
    display_host_prefix: String::new(),
    display_host_domain: url.to_string(),
    display_host_suffix: String::new(),
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
      display_host_prefix: String::new(),
      display_host_domain: raw.to_string(),
      display_host_suffix: String::new(),
      display_path_query_fragment: None,
      security_state: AddressBarSecurityState::Other,
    };
  };

  let registrable = registrable_domain(host);
  let (display_host_prefix, mut display_host_domain) = match registrable {
    Some(domain) => {
      if domain == host {
        (String::new(), domain)
      } else if host.len() > domain.len() && host.ends_with(&domain) {
        let prefix = &host[..host.len() - domain.len()];
        if prefix.ends_with('.') {
          (prefix.to_string(), domain)
        } else {
          // Defensive fallback: avoid rendering a misleading split if PSL output is unexpected.
          (String::new(), host.to_string())
        }
      } else {
        // Defensive fallback: avoid rendering a misleading split if PSL output is unexpected.
        (String::new(), host.to_string())
      }
    }
    None => (String::new(), host.to_string()),
  };

  if matches!(url.host(), Some(url::Host::Ipv6(_))) && !display_host_domain.starts_with('[') {
    display_host_domain = format!("[{display_host_domain}]");
  }

  let display_host_suffix = url
    .port()
    .map_or_else(String::new, |port| format!(":{port}"));

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
    display_host_prefix,
    display_host_domain,
    display_host_suffix,
    display_path_query_fragment: (!path_query_fragment.is_empty()).then_some(path_query_fragment),
    security_state,
  }
}

fn registrable_domain(host: &str) -> Option<String> {
  static PSL: OnceLock<List> = OnceLock::new();
  let list = PSL.get_or_init(List::default);
  let domain = list.domain(host.as_bytes())?;
  std::str::from_utf8(domain.as_bytes())
    .ok()
    .map(str::to_string)
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
    assert_eq!(formatted.display_host_prefix, "");
    assert_eq!(formatted.display_host_domain, "example.com");
    assert_eq!(formatted.display_host_suffix, "");
    assert_eq!(
      formatted.display_path_query_fragment.as_deref(),
      Some("/path?x=1#y")
    );
  }

  #[test]
  fn http_url_shows_not_secure_state_and_hides_trivial_path() {
    let formatted = format_address_bar_url("http://example.com/");
    assert_eq!(formatted.security_state, AddressBarSecurityState::Http);
    assert_eq!(formatted.display_host_prefix, "");
    assert_eq!(formatted.display_host_domain, "example.com");
    assert_eq!(formatted.display_host_suffix, "");
    assert_eq!(formatted.display_path_query_fragment, None);
  }

  #[test]
  fn about_url_is_displayed_as_is() {
    let formatted = format_address_bar_url("about:newtab");
    assert_eq!(formatted.security_state, AddressBarSecurityState::About);
    assert_eq!(formatted.display_host_prefix, "");
    assert_eq!(formatted.display_host_domain, "about:newtab");
    assert_eq!(formatted.display_host_suffix, "");
    assert_eq!(formatted.display_path_query_fragment, None);
  }

  #[test]
  fn file_url_windows_drive_is_preserved() {
    let formatted = format_address_bar_url("file:///C:/tmp/a.html");
    assert_eq!(formatted.security_state, AddressBarSecurityState::File);
    assert_eq!(formatted.display_host_prefix, "");
    assert_eq!(formatted.display_host_domain, "file:///C:/tmp/a.html");
    assert_eq!(formatted.display_host_suffix, "");
  }

  #[test]
  fn file_url_posix_is_preserved() {
    let formatted = format_address_bar_url("file:///tmp/a.html");
    assert_eq!(formatted.security_state, AddressBarSecurityState::File);
    assert_eq!(formatted.display_host_prefix, "");
    assert_eq!(formatted.display_host_domain, "file:///tmp/a.html");
    assert_eq!(formatted.display_host_suffix, "");
  }

  #[test]
  fn invalid_url_falls_back_to_raw_string() {
    let formatted = format_address_bar_url("not a url");
    assert_eq!(formatted.security_state, AddressBarSecurityState::Other);
    assert_eq!(formatted.display_host_prefix, "");
    assert_eq!(formatted.display_host_domain, "not a url");
    assert_eq!(formatted.display_host_suffix, "");
    assert_eq!(formatted.display_path_query_fragment, None);
  }

  #[test]
  fn https_url_preserves_subdomain_and_emphasises_registrable_domain() {
    let formatted = format_address_bar_url("https://accounts.google.com/path");
    assert_eq!(formatted.security_state, AddressBarSecurityState::Https);
    assert_eq!(formatted.display_host_prefix, "accounts.");
    assert_eq!(formatted.display_host_domain, "google.com");
    assert_eq!(formatted.display_host_suffix, "");
    assert_eq!(
      formatted.display_path_query_fragment.as_deref(),
      Some("/path")
    );
  }

  #[test]
  fn https_url_preserves_multi_level_subdomain_and_port() {
    let formatted = format_address_bar_url("https://a.b.example.co.uk:8443/path");
    assert_eq!(formatted.security_state, AddressBarSecurityState::Https);
    assert_eq!(formatted.display_host_prefix, "a.b.");
    assert_eq!(formatted.display_host_domain, "example.co.uk");
    assert_eq!(formatted.display_host_suffix, ":8443");
    assert_eq!(
      formatted.display_path_query_fragment.as_deref(),
      Some("/path")
    );
  }

  #[test]
  fn https_url_ipv6_literal_and_port_are_preserved_with_brackets() {
    let formatted = format_address_bar_url("https://[2001:db8::1]:8443/path");
    assert_eq!(formatted.security_state, AddressBarSecurityState::Https);
    assert_eq!(formatted.display_host_prefix, "");
    assert_eq!(formatted.display_host_domain, "[2001:db8::1]");
    assert_eq!(formatted.display_host_suffix, ":8443");
    assert_eq!(
      formatted.display_path_query_fragment.as_deref(),
      Some("/path")
    );
  }
}
