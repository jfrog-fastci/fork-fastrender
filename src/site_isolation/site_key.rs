use crate::resource::{origin_from_url, DocumentOrigin};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use url::Url;

/// How `file:` URLs are mapped into [`SiteKey`]s.
///
/// Real browsers treat `file:` documents as having opaque origins; for site isolation we need a
/// policy that avoids co-hosting unrelated local files in the same renderer process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FileUrlSiteIsolation {
  /// Legacy behaviour: all `file:` URLs share one site bucket.
  ///
  /// This is less secure but can reduce process churn in tests/harnesses that load many local
  /// fixtures.
  SingleSite,
  /// Derive an opaque key from the full absolute file path (stable for a given file URL).
  ///
  /// Same file URL ⇒ same `SiteKey`
  /// Different file paths ⇒ different `SiteKey`
  OpaquePerUrl,
  /// Derive an opaque key from the parent directory of the file path (stable per directory).
  ///
  /// Files in the same directory share a `SiteKey`; files in different directories do not.
  OpaquePerDirectory,
}

impl Default for FileUrlSiteIsolation {
  fn default() -> Self {
    Self::OpaquePerUrl
  }
}

/// Canonical key used to decide which renderer process hosts a document/frame.
///
/// For HTTP(S) documents this follows "schemeful site" semantics: scheme + registrable domain
/// (eTLD+1). This matches Chromium's schemeful same-site definition and allows sibling subdomains
/// to share a renderer process.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SiteKey {
  /// Schemeful site key for HTTP(S): scheme + registrable domain (eTLD+1).
  HttpSchemefulSite {
    scheme: String,
    site: String,
  },
  /// Conservative origin-like key: scheme + host + optional port.
  ///
  /// For HTTP(S) this is used only when the host has no registrable domain (IP literals, localhost,
  /// etc). The port is only present when it is non-default (80/443).
  ///
  /// For non-HTTP(S) schemes this is used as a best-effort origin-like key (scheme + host + port),
  /// and collapses to a scheme-only key when `host` is `None`.
  OriginLike {
    scheme: String,
    host: Option<String>,
    port: Option<u16>,
  },
  /// Unique (opaque) site key for documents with a unique origin (e.g. `data:`), as well as
  /// unparseable/unsupported navigations.
  Opaque(u64),
}

impl SiteKey {
  /// Compute a deterministic site key from a URL string.
  ///
  /// This is intended for pure classification (e.g. site grouping). It returns `None` only when the
  /// URL fails to parse.
  pub fn from_url(url: &str) -> Option<Self> {
    let origin = origin_from_url(url.trim())?;
    Some(Self::from_origin(&origin))
  }

  /// Compute a deterministic site key from a [`DocumentOrigin`].
  pub fn from_origin(origin: &DocumentOrigin) -> Self {
    let scheme = origin.scheme().to_ascii_lowercase();
    let host = origin.host().and_then(normalize_host);

    match scheme.as_str() {
      "http" | "https" => {
        let Some(host) = host else {
          return Self::OriginLike {
            scheme,
            host: None,
            port: None,
          };
        };

        // IP literals do not have registrable domains.
        if host.parse::<IpAddr>().is_err() {
          if let Some(site) = crate::resource::http_browser_registrable_domain(&host) {
            return Self::HttpSchemefulSite { scheme, site };
          }
        }

        let port = normalize_http_port(&scheme, origin.port());
        Self::OriginLike {
          scheme,
          host: Some(host),
          port,
        }
      }
      _ => Self::OriginLike {
        scheme,
        host,
        port: origin.port(),
      },
    }
  }

  pub fn same_site(&self, other: &SiteKey) -> bool {
    self == other
  }
}

impl fmt::Display for SiteKey {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      SiteKey::HttpSchemefulSite { scheme, site } => write!(f, "{scheme}://{site}"),
      SiteKey::OriginLike { scheme, host, port } => {
        let Some(host) = host.as_deref() else {
          if scheme == "file" {
            return write!(f, "file://");
          }
          return write!(f, "{scheme}:");
        };
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
      SiteKey::Opaque(id) => write!(f, "opaque:{id}"),
    }
  }
}

impl SiteKey {
  /// Convenience helper to derive a site key from a parsed URL, treating it as a top-level
  /// navigation with no parent inheritance.
  ///
  /// This is intentionally a thin wrapper over [`site_key_for_navigation`].
  pub fn from_parsed_url(url: &Url) -> SiteKey {
    site_key_for_navigation(url.as_str(), None, false)
  }
}

/// Canonical origin key for origin-partitioned state.
///
/// This is intentionally distinct from [`SiteKey`]: `OriginKey` remains an origin-like key
/// (scheme + host + port), while `SiteKey` can group multiple origins into the same site bucket.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OriginKey {
  Origin(DocumentOrigin),
  Opaque(u64),
}

impl fmt::Display for OriginKey {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      OriginKey::Origin(origin) => origin.fmt(f),
      OriginKey::Opaque(id) => write!(f, "opaque:{id}"),
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

/// Generator for [`SiteKey::Opaque`] values.
///
/// A factory can be injected for deterministic tests (each test can start from a fixed seed
/// without depending on global state).
#[derive(Debug)]
pub struct SiteKeyFactory {
  next_opaque_id: AtomicU64,
  file_url_isolation: FileUrlSiteIsolation,
}

impl Default for SiteKeyFactory {
  fn default() -> Self {
    Self::new_with_seed(1)
  }
}

impl SiteKeyFactory {
  /// Create a new factory whose first generated opaque ID will be `seed`.
  pub const fn new_with_seed(seed: u64) -> Self {
    Self::new_with_seed_and_file_url_isolation(seed, FileUrlSiteIsolation::OpaquePerUrl)
  }

  /// Create a new factory with an explicit `file:` URL isolation mode.
  pub const fn new_with_seed_and_file_url_isolation(
    seed: u64,
    file_url_isolation: FileUrlSiteIsolation,
  ) -> Self {
    Self {
      next_opaque_id: AtomicU64::new(seed),
      file_url_isolation,
    }
  }

  fn new_opaque(&self) -> SiteKey {
    let id = self.next_opaque_id.fetch_add(1, Ordering::Relaxed);
    SiteKey::Opaque(id)
  }

  fn file_origin() -> Option<&'static DocumentOrigin> {
    static FILE_ORIGIN: OnceLock<Option<DocumentOrigin>> = OnceLock::new();
    FILE_ORIGIN.get_or_init(|| origin_from_url("file:///")).as_ref()
  }

  fn stable_file_hash_u64(&self, bytes: &[u8]) -> u64 {
    // Domain-separated hash so `file:`-derived opaque IDs don't accidentally collide with other
    // hashes should we introduce them in the future.
    let mut hasher = Sha256::new();
    hasher.update(b"fastrender:file-site-key:v1\0");
    hasher.update(bytes);
    let digest = hasher.finalize();

    // Use 64 bits (with a tag bit) since `SiteKey::Opaque` is a u64 today.
    //
    // Security note: collisions are still theoretically possible, but using a cryptographic hash and
    // tagging into a disjoint ID space keeps the risk negligible for realistic workloads.
    let mut first8 = [0u8; 8];
    first8.copy_from_slice(&digest[..8]);
    let raw = u64::from_le_bytes(first8);

    // Tag file-derived opaque IDs into the top half of the u64 space so they won't collide with the
    // sequential `new_opaque()` IDs unless a session performs ~2^63 opaque navigations.
    raw | (1u64 << 63)
  }

  fn stable_about_hash_u64(&self, bytes: &[u8]) -> u64 {
    // Domain-separated hash so `about:`-derived opaque IDs don't collide with other stable hashes.
    let mut hasher = Sha256::new();
    hasher.update(b"fastrender:about-site-key:v1\0");
    hasher.update(bytes);
    let digest = hasher.finalize();

    let mut first8 = [0u8; 8];
    first8.copy_from_slice(&digest[..8]);
    let raw = u64::from_le_bytes(first8);

    // Tag into the high bit so these stable IDs cannot collide with the sequential `new_opaque()`
    // IDs unless a session performs ~2^63 opaque navigations.
    raw | (1u64 << 63)
  }

  fn site_key_for_file_url(&self, parsed: &Url) -> SiteKey {
    match self.file_url_isolation {
      FileUrlSiteIsolation::SingleSite => Self::file_origin()
        .map(SiteKey::from_origin)
        .unwrap_or_else(|| self.new_opaque()),
      FileUrlSiteIsolation::OpaquePerUrl => {
        if let Ok(path) = parsed.to_file_path() {
          let id = self.stable_file_hash_u64(path.as_os_str().to_string_lossy().as_bytes());
          SiteKey::Opaque(id)
        } else {
          let mut normalized = parsed.clone();
          normalized.set_query(None);
          normalized.set_fragment(None);
          let id = self.stable_file_hash_u64(normalized.as_str().as_bytes());
          SiteKey::Opaque(id)
        }
      }
      FileUrlSiteIsolation::OpaquePerDirectory => {
        if let Ok(path) = parsed.to_file_path() {
          let dir = path.parent().unwrap_or(Path::new(""));
          let id = self.stable_file_hash_u64(dir.as_os_str().to_string_lossy().as_bytes());
          SiteKey::Opaque(id)
        } else {
          // If we can't map the URL into a platform file path, fall back to per-URL hashing.
          let mut normalized = parsed.clone();
          normalized.set_query(None);
          normalized.set_fragment(None);
          let id = self.stable_file_hash_u64(normalized.as_str().as_bytes());
          SiteKey::Opaque(id)
        }
      }
    }
  }

  /// Derive the site key for a navigation, optionally inheriting from a parent.
  ///
  /// Rules:
  /// - HTTP(S): schemeful-site key (scheme + registrable domain / eTLD+1), with a conservative
  ///   origin-like fallback for IP literals / non-registrable hosts.
  /// - `about:blank` / `about:srcdoc`: inherit `parent` when provided; otherwise create a new
  ///   opaque key.
  /// - `data:`: always opaque.
  /// - When `force_opaque_origin` is true (e.g. sandboxed iframes without `allow-same-origin`),
  ///   always return a fresh opaque key regardless of URL or parent.
  /// - Unparseable/unsupported URLs: opaque.
  pub fn site_key_for_navigation(
    &self,
    url: &str,
    parent: Option<&SiteKey>,
    force_opaque_origin: bool,
  ) -> SiteKey {
    if force_opaque_origin {
      return self.new_opaque();
    }

    // `blob:` URLs (e.g. `blob:https://example.com/uuid`) inherit their origin from the embedded
    // URL. Treat same-origin blob navigations as the same `SiteKey` to avoid unnecessary process
    // swaps/churn for `URL.createObjectURL()` results.
    if url
      .as_bytes()
      .get(..5)
      .is_some_and(|head| head.eq_ignore_ascii_case(b"blob:"))
    {
      // Per the URL Standard's "blob URL parser", the origin of a blob URL is the origin of the
      // parsed embedded URL. If the embedded URL is `null` or fails to parse (e.g. `blob:null/...`)
      // treat it as opaque.
      let embedded = &url[5..];
      let Ok(parsed_embedded) = Url::parse(embedded) else {
        return self.new_opaque();
      };

      // Blob origins do not inherit from the navigating frame; only `about:blank` / `about:srcdoc`
      // do. Derive the site key solely from the embedded URL.
      let scheme = parsed_embedded.scheme();
      return match scheme {
        "http" | "https" => {
          if parsed_embedded.host_str().is_none() {
            self.new_opaque()
          } else {
            origin_from_url(parsed_embedded.as_str())
              .map(|origin| SiteKey::from_origin(&origin))
              .unwrap_or_else(|| self.new_opaque())
          }
        }
        "file" => self.site_key_for_file_url(&parsed_embedded),
        "about" | "data" => self.new_opaque(),
        _ => self.new_opaque(),
      };
    }

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
        origin_from_url(url)
          .map(|origin| SiteKey::from_origin(&origin))
          .unwrap_or_else(|| self.new_opaque())
      }
      "file" => self.site_key_for_file_url(&parsed),
      "about" => {
        let page = parsed.path().trim_start_matches('/');
        if page.eq_ignore_ascii_case("blank") || page.eq_ignore_ascii_case("srcdoc") {
          parent.cloned().unwrap_or_else(|| self.new_opaque())
        } else {
          // Treat internal `about:*` pages as opaque but stable per page identifier (case
          // insensitive) so query/fragment state does not cause process churn.
          let page = page.to_ascii_lowercase();
          let id = self.stable_about_hash_u64(page.as_bytes());
          SiteKey::Opaque(id)
        }
      }
      "data" => self.new_opaque(),
      _ => self.new_opaque(),
    }
  }
}

/// Derive the site key for a navigation using a shared global factory.
pub fn site_key_for_navigation(
  url: &str,
  parent: Option<&SiteKey>,
  force_opaque_origin: bool,
) -> SiteKey {
  static FACTORY: OnceLock<SiteKeyFactory> = OnceLock::new();
  FACTORY
    .get_or_init(SiteKeyFactory::default)
    .site_key_for_navigation(url, parent, force_opaque_origin)
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::tempdir;

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
  fn http_https_normalizes_host_case_and_default_ports() {
    let a = SiteKey::from_url("https://EXAMPLE.com").unwrap();
    let b = SiteKey::from_url("https://example.com:443/path?q=1").unwrap();
    assert_eq!(a, b);

    let c = SiteKey::from_url("http://Example.COM").unwrap();
    let d = SiteKey::from_url("http://example.com:80/other").unwrap();
    assert_eq!(c, d);
  }

  #[test]
  fn cross_site_urls_produce_different_site_keys() {
    let a = SiteKey::from_url("https://example.com").unwrap();
    let b = SiteKey::from_url("https://example.org").unwrap();
    assert_ne!(a, b);

    let c = SiteKey::from_url("https://example.com").unwrap();
    let d = SiteKey::from_url("http://example.com").unwrap();
    assert_ne!(c, d);
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

    let c = SiteKey::from_url("http://127.0.0.1:80").unwrap();
    assert_eq!(a, c);
  }

  #[test]
  fn blob_urls_inherit_embedded_origin() {
    let factory = SiteKeyFactory::new_with_seed(1);
    let blob = factory.site_key_for_navigation("blob:https://a.test/1", None, false);
    let https = factory.site_key_for_navigation("https://a.test/", None, false);
    assert_eq!(blob, https);

    let other = factory.site_key_for_navigation("https://b.test/", None, false);
    assert_ne!(blob, other);
  }

  #[test]
  fn blob_null_urls_are_opaque() {
    let factory = SiteKeyFactory::new_with_seed(9);

    let key = factory.site_key_for_navigation("blob:null/1", None, false);
    assert_eq!(key, SiteKey::Opaque(9));
  }

  #[test]
  fn different_file_paths_do_not_share_site_key_in_opaque_per_url_mode() {
    let dir = tempdir().expect("temp dir");
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, "a").unwrap();
    std::fs::write(&b, "b").unwrap();

    let url_a = Url::from_file_path(&a).unwrap();
    let url_b = Url::from_file_path(&b).unwrap();

    let factory = SiteKeyFactory::new_with_seed_and_file_url_isolation(
      1,
      FileUrlSiteIsolation::OpaquePerUrl,
    );

    let key_a = factory.site_key_for_navigation(url_a.as_str(), None, false);
    let key_b = factory.site_key_for_navigation(url_b.as_str(), None, false);
    assert_ne!(key_a, key_b);
    assert!(matches!(key_a, SiteKey::Opaque(_)));
    assert!(matches!(key_b, SiteKey::Opaque(_)));
  }

  #[test]
  fn same_file_url_maps_to_same_site_key_in_opaque_per_url_mode() {
    let dir = tempdir().expect("temp dir");
    let path = dir.path().join("index.html");
    std::fs::write(&path, "<!doctype html>").unwrap();
    let url = Url::from_file_path(&path).unwrap();

    let factory = SiteKeyFactory::new_with_seed_and_file_url_isolation(
      1,
      FileUrlSiteIsolation::OpaquePerUrl,
    );
    let a = factory.site_key_for_navigation(url.as_str(), None, false);
    let b = factory.site_key_for_navigation(url.as_str(), None, false);
    assert_eq!(a, b);
  }

  #[test]
  fn about_blank_inherits_parent_when_provided() {
    let factory = SiteKeyFactory::new_with_seed(1);

    let parent = factory.site_key_for_navigation("https://example.com", None, false);
    let child = factory.site_key_for_navigation("about:blank", Some(&parent), false);
    assert_eq!(child, parent);

    let srcdoc = factory.site_key_for_navigation("about:srcdoc", Some(&parent), false);
    assert_eq!(srcdoc, parent);
  }

  #[test]
  fn internal_about_pages_are_stable_and_ignore_query_and_fragment() {
    let factory = SiteKeyFactory::new_with_seed(1);

    let base = factory.site_key_for_navigation("about:history", None, false);
    let query = factory.site_key_for_navigation("about:history?q=rust", None, false);
    let frag = factory.site_key_for_navigation("about:history#foo", None, false);
    let mixed_case = factory.site_key_for_navigation("ABOUT:History?q=ignored#bar", None, false);

    assert_eq!(base, query);
    assert_eq!(base, frag);
    assert_eq!(base, mixed_case);

    let other = factory.site_key_for_navigation("about:newtab", None, false);
    assert_ne!(base, other);
  }

  #[test]
  fn blob_child_frame_site_key_matches_embedded_origin() {
    let factory = SiteKeyFactory::new_with_seed(1);

    let parent = factory.site_key_for_navigation("https://a.test/", None, false);
    let same_origin_blob =
      factory.site_key_for_navigation("blob:https://a.test/1", Some(&parent), false);
    assert_eq!(same_origin_blob, parent);

    let cross_origin_blob =
      factory.site_key_for_navigation("blob:https://b.test/1", Some(&parent), false);
    assert_ne!(cross_origin_blob, parent);

    let opaque_blob = factory.site_key_for_navigation("blob:null/1", Some(&parent), false);
    assert_ne!(opaque_blob, parent);
    assert!(matches!(opaque_blob, SiteKey::Opaque(_)));
  }

  #[test]
  fn about_blank_without_parent_is_unique_per_navigation() {
    let factory = SiteKeyFactory::new_with_seed(100);

    let a = factory.site_key_for_navigation("about:blank", None, false);
    let b = factory.site_key_for_navigation("about:blank", None, false);
    assert_ne!(a, b);

    assert!(matches!(a, SiteKey::Opaque(100)));
    assert!(matches!(b, SiteKey::Opaque(101)));
  }

  #[test]
  fn data_urls_are_always_opaque() {
    let factory = SiteKeyFactory::new_with_seed(5);

    let a = factory.site_key_for_navigation("data:text/plain,Hello", None, false);
    let b = factory.site_key_for_navigation("data:text/plain,Hello", None, false);
    assert_ne!(a, b);

    assert!(matches!(a, SiteKey::Opaque(5)));
    assert!(matches!(b, SiteKey::Opaque(6)));
  }

  #[test]
  fn file_url_site_keys_ignore_fragment_and_query_in_fallback_hash() {
    let factory = SiteKeyFactory::new_with_seed_and_file_url_isolation(
      1,
      FileUrlSiteIsolation::OpaquePerUrl,
    );

    // Use a non-local file URL host to force `Url::to_file_path()` to fail so the implementation
    // falls back to hashing the URL string. Fragment/query differences must not change the key.
    let base = factory.site_key_for_navigation("file://example.com/tmp/a.html", None, false);
    let frag = factory.site_key_for_navigation("file://example.com/tmp/a.html#x", None, false);
    let query = factory.site_key_for_navigation("file://example.com/tmp/a.html?q=1", None, false);
    let both =
      factory.site_key_for_navigation("file://example.com/tmp/a.html?q=1#y", None, false);

    assert_eq!(base, frag);
    assert_eq!(base, query);
    assert_eq!(base, both);
  }

  #[test]
  fn unparseable_urls_are_opaque() {
    let factory = SiteKeyFactory::new_with_seed(9);

    let key = factory.site_key_for_navigation("not a url", None, false);
    assert_eq!(key, SiteKey::Opaque(9));
  }

  #[test]
  fn sandboxed_iframe_without_allow_same_origin_is_always_opaque() {
    let factory = SiteKeyFactory::new_with_seed(1);

    let parent = factory.site_key_for_navigation("https://example.com", None, false);
    // `about:srcdoc` would normally inherit the parent's site, but sandbox without
    // `allow-same-origin` forces a unique opaque origin.
    let child = factory.site_key_for_navigation("about:srcdoc", Some(&parent), true);
    assert_ne!(child, parent);
    assert!(matches!(child, SiteKey::Opaque(_)));

    // Even same-origin URLs must not share when the sandbox forces an opaque origin.
    let same_origin = factory.site_key_for_navigation(
      "https://example.com/path",
      Some(&parent),
      true,
    );
    assert_ne!(same_origin, parent);
    assert!(matches!(same_origin, SiteKey::Opaque(_)));
  }

  #[test]
  fn sandboxed_iframe_with_allow_same_origin_uses_normal_inheritance() {
    let factory = SiteKeyFactory::new_with_seed(1);

    let parent = factory.site_key_for_navigation("https://example.com", None, false);
    let child = factory.site_key_for_navigation("about:srcdoc", Some(&parent), false);
    assert_eq!(child, parent);
  }
}
