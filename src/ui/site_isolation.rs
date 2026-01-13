//! Canonical “site key” extraction used for site isolation / renderer process assignment.
//!
//! The browser needs a deterministic key to decide whether two navigations can share the same
//! renderer process. For the purposes of this project we implement a conservative policy:
//!
//! - `http`/`https` URLs with a host are keyed by their *origin*:
//!   `(scheme, host, port)` with scheme + host lowercased and the port normalized via
//!   [`url::Url::port_or_known_default`]. This ensures default ports coalesce (e.g. `:443` for
//!   `https`).
//! - Everything else is treated as *opaque* and keyed by the full normalized URL string produced by
//!   `url::Url` (`Url::to_string()`).
//!
//! Treating non-HTTP(S) schemes as opaque avoids accidentally coalescing unrelated documents (e.g.
//! `about:newtab` and `about:history`) into the same renderer process.
//!
//! ## `file:` URLs
//!
//! `file:` URLs are deliberately treated as opaque *including the full path*, so
//! `file:///tmp/a.html` and `file:///tmp/b.html` do **not** share a `SiteKey`. This is the most
//! conservative choice for security: different local files should not share a renderer process
//! unless we have a well-specified policy for what “same site” means for local filesystem
//! resources.
//!
//! (Note that this may create more renderer processes than a production browser, but is safer than
//! guessing.)
use std::fmt;
use url::Url;

/// Canonical site identifier used for renderer process assignment.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SiteKey {
  /// An HTTP(S) origin `(scheme, host, port)`.
  Origin { scheme: String, host: String, port: u16 },
  /// A non-HTTP(S) or hostless URL keyed by its full normalized string representation.
  Opaque { scheme: String, url: String },
}

impl SiteKey {
  /// Parse `url` and convert it into a canonical [`SiteKey`].
  ///
  /// This function is strict about parse errors: invalid URLs return `Err` instead of producing a
  /// fallback key.
  pub fn from_url(url: &str) -> Result<Self, String> {
    let parsed = Url::parse(url).map_err(|err| err.to_string())?;
    Ok(Self::from_parsed_url(&parsed))
  }

  fn from_parsed_url(parsed: &Url) -> Self {
    let scheme = parsed.scheme().to_ascii_lowercase();
    if matches!(scheme.as_str(), "http" | "https") {
      if let Some(host) = parsed.host_str().filter(|host| !host.is_empty()) {
        let host = host.to_ascii_lowercase();
        let port = parsed
          .port_or_known_default()
          .expect("http/https URLs always have a known default port");
        return Self::Origin { scheme, host, port };
      }
    }
    Self::Opaque {
      scheme,
      url: parsed.to_string(),
    }
  }
}

impl fmt::Display for SiteKey {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Origin { scheme, host, port } => {
        let host = if host.contains(':') && !host.starts_with('[') {
          // `url::Url::host_str()` returns IPv6 hosts without brackets.
          format!("[{host}]")
        } else {
          host.clone()
        };
        let default_port = match scheme.as_str() {
          "http" => 80,
          "https" => 443,
          _ => *port,
        };
        if *port == default_port {
          write!(f, "{scheme}://{host}")
        } else {
          write!(f, "{scheme}://{host}:{port}")
        }
      }
      Self::Opaque { url, .. } => f.write_str(url),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::SiteKey;
  use std::collections::HashSet;

  #[test]
  fn host_casing_is_normalized() {
    let a = SiteKey::from_url("https://Example.COM/a").unwrap();
    let b = SiteKey::from_url("https://example.com/b").unwrap();
    assert_eq!(a, b);
  }

  #[test]
  fn default_ports_are_normalized() {
    assert_eq!(
      SiteKey::from_url("https://example.com/").unwrap(),
      SiteKey::from_url("https://example.com:443/").unwrap()
    );
    assert_eq!(
      SiteKey::from_url("http://example.com/").unwrap(),
      SiteKey::from_url("http://example.com:80/").unwrap()
    );
  }

  #[test]
  fn http_display_is_origin_only() {
    let key = SiteKey::from_url("https://example.com/path?query#frag").unwrap();
    assert_eq!(key.to_string(), "https://example.com");
  }

  #[test]
  fn ipv6_origin_is_canonical_and_displays_stably() {
    let key = SiteKey::from_url("https://[::1]/a?b#c").unwrap();
    assert_eq!(key.to_string(), "https://[::1]");
    assert_eq!(
      key,
      SiteKey::from_url("https://[::1]:443/other").unwrap()
    );
  }

  #[test]
  fn about_pages_are_distinct() {
    let newtab = SiteKey::from_url("about:newtab").unwrap();
    let history = SiteKey::from_url("about:history").unwrap();
    assert_ne!(newtab, history);
  }

  #[test]
  fn file_urls_do_not_collapse_by_default() {
    let a = SiteKey::from_url("file:///tmp/a.html").unwrap();
    let b = SiteKey::from_url("file:///tmp/b.html").unwrap();
    assert_ne!(a, b);

    let mut set = HashSet::new();
    set.insert(a);
    assert!(set.insert(b));
  }

  #[test]
  fn parse_errors_are_reported() {
    assert!(SiteKey::from_url("not a url").is_err());
  }
}

