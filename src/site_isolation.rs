use crate::resource::DocumentOrigin;
use std::net::IpAddr;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SiteKey(SiteKeyInner);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum SiteKeyInner {
  /// "Schemeful site" for HTTP(S): scheme + registrable domain (eTLD+1).
  SchemefulSite { scheme: String, site: String },
  /// Conservative origin-like fallback: scheme + host + (optional) port.
  ///
  /// For HTTP(S), the port is only present when it is non-default.
  OriginLike {
    scheme: String,
    host: String,
    port: Option<u16>,
  },
  /// Opaque/no-host schemes (e.g. `file:`, `data:`).
  SchemeOnly { scheme: String },
}

impl SiteKey {
  /// Compute a [`SiteKey`] from a URL string.
  pub fn from_url(url: &str) -> Option<Self> {
    let origin = crate::resource::origin_from_url(url.trim())?;
    Some(Self::from_origin(&origin))
  }

  /// Compute a [`SiteKey`] from a browser document origin.
  pub fn from_origin(origin: &DocumentOrigin) -> Self {
    let scheme = origin.scheme().to_ascii_lowercase();
    let host = origin.host().and_then(normalize_host);

    match scheme.as_str() {
      "http" | "https" => {
        let Some(host) = host else {
          return Self(SiteKeyInner::SchemeOnly { scheme });
        };

        // Try to compute a schemeful site using the PSL (eTLD+1). For IP literals (or hosts that
        // don't have a registrable domain), fall back to an origin-like key.
        if host.parse::<IpAddr>().is_err() {
          if let Some(site) = crate::resource::http_browser_registrable_domain(&host) {
            return Self(SiteKeyInner::SchemefulSite { scheme, site });
          }
        }

        let port = normalize_http_port(&scheme, origin.port());
        Self(SiteKeyInner::OriginLike { scheme, host, port })
      }
      _ => {
        let Some(host) = host else {
          return Self(SiteKeyInner::SchemeOnly { scheme });
        };
        // For non-HTTP(S) schemes, use a conservative origin-like key.
        let port = origin.port();
        Self(SiteKeyInner::OriginLike { scheme, host, port })
      }
    }
  }
}

/// Returns true when the two site keys are considered same-site.
pub fn same_site(a: &SiteKey, b: &SiteKey) -> bool {
  a == b
}

impl std::fmt::Display for SiteKey {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match &self.0 {
      SiteKeyInner::SchemefulSite { scheme, site } => write!(f, "{scheme}://{site}"),
      SiteKeyInner::OriginLike { scheme, host, port } => {
        let needs_brackets = host.contains(':') && !host.starts_with('[');
        if needs_brackets {
          write!(f, "{scheme}://[{host}]")?;
        } else {
          write!(f, "{scheme}://{host}")?;
        }
        if let Some(port) = port {
          write!(f, ":{port}")?;
        }
        Ok(())
      }
      SiteKeyInner::SchemeOnly { scheme } => write!(f, "{scheme}:"),
    }
  }
}

fn normalize_host(host: &str) -> Option<String> {
  let trimmed = host.trim_end_matches('.').to_ascii_lowercase();
  if trimmed.is_empty() {
    None
  } else {
    Some(trimmed)
  }
}

fn normalize_http_port(scheme: &str, port: Option<u16>) -> Option<u16> {
  let default = match scheme {
    "http" => 80,
    "https" => 443,
    _ => return port,
  };
  let effective = port.unwrap_or(default);
  if effective == default {
    None
  } else {
    Some(effective)
  }
}

#[cfg(test)]
mod tests {
  use super::SiteKey;

  #[test]
  fn schemeful_site_groups_registrable_domains() {
    let a = SiteKey::from_url("https://a.example.com").unwrap();
    let b = SiteKey::from_url("https://b.example.com").unwrap();
    assert_eq!(a, b);
  }

  #[test]
  fn schemeful_site_is_schemeful() {
    let https = SiteKey::from_url("https://example.com").unwrap();
    let http = SiteKey::from_url("http://example.com").unwrap();
    assert_ne!(https, http);
  }

  #[test]
  fn default_ports_do_not_affect_fallback_keys() {
    let a = SiteKey::from_url("https://127.0.0.1").unwrap();
    let b = SiteKey::from_url("https://127.0.0.1:443").unwrap();
    assert_eq!(a, b);
  }

  #[test]
  fn non_default_ports_affect_fallback_keys() {
    let a = SiteKey::from_url("https://127.0.0.1").unwrap();
    let b = SiteKey::from_url("https://127.0.0.1:444").unwrap();
    assert_ne!(a, b);
  }

  #[test]
  fn ip_literal_hosts_are_cross_site_unless_identical() {
    let a = SiteKey::from_url("http://127.0.0.1").unwrap();
    let b = SiteKey::from_url("http://127.0.0.2").unwrap();
    assert_ne!(a, b);

    // Same IP, default port normalization (http://:80) matches.
    let c = SiteKey::from_url("http://127.0.0.1:80").unwrap();
    assert_eq!(a, c);
  }
}

