//! Canonical “site key” extraction used for site isolation / renderer process assignment.
//!
//! The browser needs a deterministic key to decide whether two navigations can share the same
//! renderer process. For the purposes of this project we implement a conservative policy:
//!
//! - `http`/`https` URLs with a host are keyed by their *origin*:
//!   `(scheme, host, port)` with scheme + host lowercased and the port normalized via
//!   [`url::Url::port_or_known_default`]. This ensures default ports coalesce (e.g. `:443` for
//!   `https`).
//! - `blob:` URLs are keyed by the origin of their embedded URL when it is `http`/`https`
//!   (e.g. `blob:https://example.com/<uuid>` is treated as same-site with `https://example.com/`),
//!   matching blob URL origin semantics.
//! - `about:` URLs are treated as opaque, but keyed by the about page identifier only (e.g.
//!   `about:history`, `about:newtab`). Query/fragment components are ignored because internal pages
//!   frequently use them for in-page state.
//! - Everything else is treated as *opaque* and keyed by the normalized URL string produced by
//!   `url::Url` (`Url::to_string()`), with the fragment removed so same-document navigations do not
//!   trigger process swaps.
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
use crate::ui::protocol_limits::MAX_URL_BYTES;
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
  ///
  /// For safety, URLs longer than [`MAX_URL_BYTES`] are rejected before parsing to avoid expensive
  /// work on attacker-controlled strings.
  pub fn from_url(url: &str) -> Result<Self, String> {
    if url.len() > MAX_URL_BYTES {
      return Err(format!(
        "URL too long for site key extraction ({} bytes; limit is {} bytes)",
        url.len(),
        MAX_URL_BYTES
      ));
    }

    // `blob:` URLs embed an inner URL whose origin is the blob's origin. Treat `blob:https://...`
    // as same-site with the embedded HTTP(S) origin.
    if url
      .as_bytes()
      .get(.."blob:".len())
      .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"blob:"))
    {
      let inner = &url["blob:".len()..];
      if let Ok(inner_url) = Url::parse(inner) {
        if matches!(inner_url.scheme(), "http" | "https") {
          return Ok(Self::from_parsed_url(&inner_url));
        }
      }
    }

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
          .expect("http/https URLs always have a known default port"); // fastrender-allow-unwrap
        return Self::Origin { scheme, host, port };
      }
    }

    if scheme == "about" {
      // Key `about:` URLs by their page identifier only (e.g. `about:history`), ignoring any
      // query/fragment used for in-page state.
      let page = parsed.path().trim_start_matches('/').to_ascii_lowercase();
      return Self::Opaque {
        scheme,
        url: format!("about:{page}"),
      };
    }

    // For opaque schemes, ignore the fragment so same-document navigations do not force a process
    // swap. (A stricter policy may still treat query changes as cross-site for some schemes, but
    // fragments are always same-document.)
    let mut normalized = parsed.clone();
    normalized.set_fragment(None);

    // `file:` URLs often use `?query` for in-document state; treat file navigations that only
    // change query/fragment as same-site to avoid process churn.
    if scheme == "file" {
      normalized.set_query(None);
    }

    Self::Opaque {
      scheme,
      url: normalized.to_string(),
    }
  }
}

/// Returns `Ok(true)` when navigating from `current` to `target_url` crosses the current site key.
///
/// This is a pure policy primitive intended for future navigation/process-swap logic.
pub fn is_cross_site_navigation(current: &SiteKey, target_url: &str) -> Result<bool, String> {
  let target = SiteKey::from_url(target_url)?;
  Ok(&target != current)
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
  use super::{is_cross_site_navigation, SiteKey};
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
    assert_eq!(key, SiteKey::from_url("https://[::1]:443/other").unwrap());
  }

  #[test]
  fn about_pages_are_distinct() {
    let newtab = SiteKey::from_url("about:newtab").unwrap();
    let history = SiteKey::from_url("about:history").unwrap();
    assert_ne!(newtab, history);
  }

  #[test]
  fn about_pages_ignore_query_and_fragment() {
    let base = SiteKey::from_url("about:history").unwrap();
    assert_eq!(base, SiteKey::from_url("about:history?q=rust").unwrap());
    assert_eq!(base, SiteKey::from_url("about:history#foo").unwrap());

    // Page identifiers are case-insensitive.
    assert_eq!(base, SiteKey::from_url("ABOUT:History?q=ignored").unwrap());
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
  fn file_urls_ignore_fragments() {
    assert_eq!(
      SiteKey::from_url("file:///tmp/a.html#x").unwrap(),
      SiteKey::from_url("file:///tmp/a.html#y").unwrap()
    );

    // Query is also ignored for file URLs so in-document state does not churn processes.
    assert_eq!(
      SiteKey::from_url("file:///tmp/a.html?q=1").unwrap(),
      SiteKey::from_url("file:///tmp/a.html?q=2").unwrap()
    );
  }

  #[test]
  fn opaque_schemes_ignore_fragments() {
    assert_eq!(
      SiteKey::from_url("foo:bar#x").unwrap(),
      SiteKey::from_url("foo:bar#y").unwrap()
    );
  }

  #[test]
  fn parse_errors_are_reported() {
    assert!(SiteKey::from_url("not a url").is_err());
  }

  #[test]
  fn blob_urls_use_their_inner_origin() {
    let blob = SiteKey::from_url("blob:https://example.com/123").unwrap();
    let origin = SiteKey::from_url("https://example.com/").unwrap();
    assert_eq!(blob, origin);
  }

  #[test]
  fn blob_urls_normalize_host_case_and_default_ports() {
    let blob = SiteKey::from_url("blob:https://Example.com:443/123").unwrap();
    let origin = SiteKey::from_url("https://example.com/").unwrap();
    assert_eq!(blob, origin);
  }

  #[test]
  fn blob_null_is_opaque() {
    let key = SiteKey::from_url("blob:null/123").unwrap();
    assert!(matches!(key, SiteKey::Opaque { scheme, .. } if scheme == "blob"));
  }

  #[test]
  fn cross_site_navigation_helper_detects_same_origin() {
    let current = SiteKey::from_url("https://example.com/a").unwrap();
    assert!(!is_cross_site_navigation(&current, "https://example.com/b").unwrap());
  }

  #[test]
  fn cross_site_navigation_helper_detects_cross_origin() {
    let current = SiteKey::from_url("https://example.com").unwrap();
    assert!(is_cross_site_navigation(&current, "https://evil.com").unwrap());
  }

  #[test]
  fn cross_site_navigation_helper_detects_scheme_change() {
    let current = SiteKey::from_url("http://example.com").unwrap();
    assert!(is_cross_site_navigation(&current, "https://example.com").unwrap());
  }

  #[test]
  fn cross_site_navigation_helper_respects_opaque_scheme_semantics() {
    let current = SiteKey::from_url("about:blank").unwrap();
    assert!(is_cross_site_navigation(&current, "about:newtab").unwrap());

    let current = SiteKey::from_url("file:///tmp/a.html").unwrap();
    assert!(is_cross_site_navigation(&current, "file:///tmp/b.html").unwrap());
  }

  #[test]
  fn cross_site_navigation_helper_propagates_parse_errors() {
    let current = SiteKey::from_url("https://example.com").unwrap();
    assert!(is_cross_site_navigation(&current, "not a url").is_err());
  }
} 
