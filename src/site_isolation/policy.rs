//! Site isolation policy decisions.
//!
//! This module centralizes the decision of when a child browsing context (e.g. an `<iframe>`) must
//! be hosted in a separate renderer process.
//!
//! See `instructions/multiprocess_security.md` **P2: Site isolation**:
//! - Cross-origin iframes get their own process
//! - Navigations to a new origin should swap renderer processes
//!
//! This helper intentionally covers only the *process assignment* decision. Actual security still
//! requires a working frame tree, process model, and IPC-enforced access control.

use crate::debug::runtime;

use super::site_key::{site_key_for_navigation, SiteKey};

/// Environment key controlling site isolation behavior.
///
/// Accepted values (case-insensitive):
/// - `off`, `0`, `false`, `no` → [`SiteIsolationMode::Off`]
/// - `per-origin`, `origin`, `on`, `1`, `true`, `yes` → [`SiteIsolationMode::PerOrigin`]
/// - `per-site`, `site` → [`SiteIsolationMode::PerSite`]
pub const ENV_SITE_ISOLATION_MODE: &str = "FASTR_SITE_ISOLATION_MODE";

/// Controls how renderer processes are partitioned across sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SiteIsolationMode {
  /// Disable site isolation (single-process or process-per-tab style behavior).
  Off,
  /// Isolate at the granularity of origins (scheme + host + port).
  PerOrigin,
  /// Isolate at a coarser site granularity (eTLD+1). Reserved for future use.
  ///
  /// Note: Today this mode behaves the same as [`SiteIsolationMode::PerOrigin`]. The variant is
  /// included so embedders can start plumbing configuration early.
  PerSite,
}

impl Default for SiteIsolationMode {
  fn default() -> Self {
    // Default to the secure configuration described in `instructions/multiprocess_security.md` P2.
    Self::PerOrigin
  }
}

impl SiteIsolationMode {
  /// Returns the currently configured site isolation mode (from `FASTR_*` runtime toggles).
  pub fn from_env() -> Self {
    let toggles = runtime::runtime_toggles();
    let raw = toggles.get(ENV_SITE_ISOLATION_MODE);
    raw.and_then(Self::parse).unwrap_or_default()
  }

  fn parse(raw: &str) -> Option<Self> {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.is_empty() {
      return None;
    }
    match lower.as_str() {
      "0" | "false" | "no" | "off" | "disabled" => Some(Self::Off),
      "1" | "true" | "yes" | "on" | "origin" | "perorigin" | "per-origin" | "per_origin" => {
        Some(Self::PerOrigin)
      }
      "site" | "persite" | "per-site" | "per_site" => Some(Self::PerSite),
      _ => None,
    }
  }

  fn isolates_cross_origin(self) -> bool {
    matches!(self, Self::PerOrigin | Self::PerSite)
  }
}

fn parent_scheme(parent: &SiteKey) -> Option<&str> {
  match parent {
    SiteKey::HttpSchemefulSite { scheme, .. } => Some(scheme.as_str()),
    SiteKey::OriginLike { scheme, .. } => Some(scheme.as_str()),
    SiteKey::Opaque(_) => None,
  }
}

/// Decide whether a child frame should be isolated into a separate renderer process.
///
/// This is the central decision point described by `instructions/multiprocess_security.md` P2.
///
/// Decision rules:
/// - `srcdoc` iframes inherit the parent origin → never isolate.
/// - `about:blank` iframes inherit the creator origin → never isolate.
/// - If the child resolves to the same [`SiteKey`] as the parent → do not isolate.
/// - Otherwise (cross-origin), isolate when the configured [`SiteIsolationMode`] is
///   [`SiteIsolationMode::PerOrigin`].
pub fn should_isolate_child_frame(
  parent: &SiteKey,
  child_url: &str,
  child_is_srcdoc: bool,
) -> bool {
  should_isolate_child_frame_with_force_opaque_origin(parent, child_url, child_is_srcdoc, false)
}

/// Like [`should_isolate_child_frame`], but allows the caller to force the child into a unique
/// opaque origin.
///
/// This is primarily used for `<iframe sandbox>` when the sandbox token list does **not** contain
/// `allow-same-origin`, causing the child document to have an opaque origin even for `srcdoc` /
/// `about:blank` navigations.
pub fn should_isolate_child_frame_with_force_opaque_origin(
  parent: &SiteKey,
  child_url: &str,
  child_is_srcdoc: bool,
  force_opaque_origin: bool,
) -> bool {
  should_isolate_child_frame_with_mode(
    parent,
    child_url,
    child_is_srcdoc,
    force_opaque_origin,
    SiteIsolationMode::from_env(),
  )
}

fn should_isolate_child_frame_with_mode(
  parent: &SiteKey,
  child_url: &str,
  child_is_srcdoc: bool,
  force_opaque_origin: bool,
  mode: SiteIsolationMode,
) -> bool {
  if child_is_srcdoc && !force_opaque_origin {
    return false;
  }

  let trimmed = child_url.trim();
  if trimmed.is_empty() && !force_opaque_origin {
    // HTML: missing/empty iframe `src` behaves like `about:blank` and inherits the creator origin.
    return false;
  }

  // Relative URLs resolve against the parent document base URL. `should_isolate_child_frame` is
  // typically called with an already-resolved absolute URL; for unresolved relative inputs we
  // conservatively assume the navigation stays within the parent site.
  if !trimmed.contains(':') && !trimmed.starts_with("//") && !force_opaque_origin {
    return false;
  }

  let resolved_url;
  let child_url = if let Some(after) = trimmed.strip_prefix("//") {
    // Scheme-relative URLs inherit the parent's scheme.
    if let Some(scheme) = parent_scheme(parent) {
      resolved_url = format!("{scheme}://{after}");
      resolved_url.as_str()
    } else {
      // If the parent is an opaque site, we cannot reliably resolve `//host/...`. Treat it as
      // cross-site so isolation remains conservative.
      return mode.isolates_cross_origin();
    }
  } else {
    trimmed
  };

  let child_site = site_key_for_navigation(child_url, Some(parent), force_opaque_origin);

  if &child_site == parent {
    return false;
  }

  mode.isolates_cross_origin()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn same_origin_iframe_not_isolated() {
    let parent = site_key_for_navigation("https://example.com/", None, false);
    assert!(!should_isolate_child_frame_with_mode(
      &parent,
      "https://example.com/child",
      false,
      false,
      SiteIsolationMode::PerOrigin
    ));
  }

  #[test]
  fn cross_origin_iframe_isolated() {
    let parent = site_key_for_navigation("https://example.com/", None, false);
    assert!(should_isolate_child_frame_with_mode(
      &parent,
      "https://other.com/",
      false,
      false,
      SiteIsolationMode::PerOrigin
    ));
  }

  #[test]
  fn srcdoc_iframe_not_isolated() {
    let parent = site_key_for_navigation("https://example.com/", None, false);
    assert!(!should_isolate_child_frame_with_mode(
      &parent,
      "https://other.com/",
      true,
      false,
      SiteIsolationMode::PerOrigin
    ));
  }

  #[test]
  fn about_blank_iframe_not_isolated() {
    let parent = site_key_for_navigation("https://example.com/", None, false);
    assert!(!should_isolate_child_frame_with_mode(
      &parent,
      "about:blank",
      false,
      false,
      SiteIsolationMode::PerOrigin
    ));
  }

  #[test]
  fn data_iframe_isolated() {
    let parent = site_key_for_navigation("https://example.com/", None, false);
    assert!(should_isolate_child_frame_with_mode(
      &parent,
      "data:text/html,hello",
      false,
      false,
      SiteIsolationMode::PerOrigin
    ));
  }

  #[test]
  fn sandboxed_srcdoc_iframe_isolated() {
    let parent = site_key_for_navigation("https://example.com/", None, false);
    assert!(should_isolate_child_frame_with_mode(
      &parent,
      "about:srcdoc",
      true,
      true,
      SiteIsolationMode::PerOrigin
    ));
  }
}
