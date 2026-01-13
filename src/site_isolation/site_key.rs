use crate::resource::{origin_from_url, DocumentOrigin};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use url::Url;

/// Canonical key used to decide which renderer process hosts a document/frame.
///
/// This is intentionally a "site-ish" key rather than a full URL: for HTTP(S) documents we group
/// all paths on the same origin together.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SiteKey {
  /// Regular origin-based site key (HTTP/HTTPS/File).
  Origin(DocumentOrigin),
  /// Unique (opaque) site key for documents with a unique origin (e.g. `data:`), as well as
  /// unparseable/unsupported navigations.
  Opaque(u64),
}

/// Generator for [`SiteKey::Opaque`] values.
///
/// A factory can be injected for deterministic tests (each test can start from a fixed seed
/// without depending on global state).
#[derive(Debug)]
pub struct SiteKeyFactory {
  next_opaque_id: AtomicU64,
}

impl Default for SiteKeyFactory {
  fn default() -> Self {
    Self::new_with_seed(1)
  }
}

impl SiteKeyFactory {
  /// Create a new factory whose first generated opaque ID will be `seed`.
  pub const fn new_with_seed(seed: u64) -> Self {
    Self {
      next_opaque_id: AtomicU64::new(seed),
    }
  }

  fn new_opaque(&self) -> SiteKey {
    let id = self.next_opaque_id.fetch_add(1, Ordering::Relaxed);
    SiteKey::Opaque(id)
  }

  fn file_origin() -> &'static DocumentOrigin {
    static FILE_ORIGIN: OnceLock<DocumentOrigin> = OnceLock::new();
    FILE_ORIGIN.get_or_init(|| {
      origin_from_url("file:///").expect("file:/// must be a parseable URL")
    })
  }

  /// Derive the site key for a navigation, optionally inheriting from a parent.
  ///
  /// Rules:
  /// - HTTP(S): key by [`DocumentOrigin`] (case-insensitive host, default port normalization).
  /// - `about:blank` / `about:srcdoc`: inherit `parent` when provided; otherwise create a new
  ///   opaque key.
  /// - `data:`: always opaque.
  /// - Unparseable/unsupported URLs: opaque.
  pub fn site_key_for_navigation(&self, url: &str, parent: Option<&SiteKey>) -> SiteKey {
    let parsed = match Url::parse(url) {
      Ok(parsed) => parsed,
      Err(_) => return self.new_opaque(),
    };

    match parsed.scheme() {
      "http" | "https" => {
        // Guard against odd-but-parseable inputs like `http:foo` that have no authority component.
        if parsed.host_str().is_none() {
          return self.new_opaque();
        }
        origin_from_url(url).map_or_else(|| self.new_opaque(), SiteKey::Origin)
      }
      "file" => SiteKey::Origin(Self::file_origin().clone()),
      "about" => {
        let path = parsed.path();
        if path.eq_ignore_ascii_case("blank") || path.eq_ignore_ascii_case("srcdoc") {
          parent.cloned().unwrap_or_else(|| self.new_opaque())
        } else {
          self.new_opaque()
        }
      }
      "data" => self.new_opaque(),
      _ => self.new_opaque(),
    }
  }
}

/// Derive the site key for a navigation using a shared global factory.
pub fn site_key_for_navigation(url: &str, parent: Option<&SiteKey>) -> SiteKey {
  static FACTORY: OnceLock<SiteKeyFactory> = OnceLock::new();
  FACTORY
    .get_or_init(SiteKeyFactory::default)
    .site_key_for_navigation(url, parent)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn http_https_normalizes_host_case_and_default_ports() {
    let factory = SiteKeyFactory::new_with_seed(1);

    let a = factory.site_key_for_navigation("https://EXAMPLE.com", None);
    let b = factory.site_key_for_navigation("https://example.com:443/path?q=1", None);
    assert_eq!(a, b);

    let c = factory.site_key_for_navigation("http://Example.COM", None);
    let d = factory.site_key_for_navigation("http://example.com:80/other", None);
    assert_eq!(c, d);
  }

  #[test]
  fn cross_origin_urls_produce_different_site_keys() {
    let factory = SiteKeyFactory::new_with_seed(1);

    let a = factory.site_key_for_navigation("https://example.com", None);
    let b = factory.site_key_for_navigation("https://example.org", None);
    assert_ne!(a, b);

    let c = factory.site_key_for_navigation("https://example.com", None);
    let d = factory.site_key_for_navigation("http://example.com", None);
    assert_ne!(c, d);

    let e = factory.site_key_for_navigation("http://example.com:8080", None);
    let f = factory.site_key_for_navigation("http://example.com", None);
    assert_ne!(e, f);
  }

  #[test]
  fn file_urls_map_to_a_single_origin_key() {
    let factory = SiteKeyFactory::new_with_seed(1);

    let a = factory.site_key_for_navigation("file:///tmp/a.txt", None);
    let b = factory.site_key_for_navigation("file:///home/user/b.txt", None);
    assert_eq!(a, b);
  }

  #[test]
  fn about_blank_inherits_parent_when_provided() {
    let factory = SiteKeyFactory::new_with_seed(1);

    let parent = factory.site_key_for_navigation("https://example.com", None);
    let child = factory.site_key_for_navigation("about:blank", Some(&parent));
    assert_eq!(child, parent);

    let srcdoc = factory.site_key_for_navigation("about:srcdoc", Some(&parent));
    assert_eq!(srcdoc, parent);
  }

  #[test]
  fn about_blank_without_parent_is_unique_per_navigation() {
    let factory = SiteKeyFactory::new_with_seed(100);

    let a = factory.site_key_for_navigation("about:blank", None);
    let b = factory.site_key_for_navigation("about:blank", None);
    assert_ne!(a, b);

    assert!(matches!(a, SiteKey::Opaque(100)));
    assert!(matches!(b, SiteKey::Opaque(101)));
  }

  #[test]
  fn data_urls_are_always_opaque() {
    let factory = SiteKeyFactory::new_with_seed(5);

    let a = factory.site_key_for_navigation("data:text/plain,Hello", None);
    let b = factory.site_key_for_navigation("data:text/plain,Hello", None);
    assert_ne!(a, b);

    assert!(matches!(a, SiteKey::Opaque(5)));
    assert!(matches!(b, SiteKey::Opaque(6)));
  }

  #[test]
  fn unparseable_urls_are_opaque() {
    let factory = SiteKeyFactory::new_with_seed(9);

    let key = factory.site_key_for_navigation("not a url", None);
    assert_eq!(key, SiteKey::Opaque(9));
  }
}

