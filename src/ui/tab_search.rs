//! Tab search / quick switcher helpers shared by browser UIs.

use crate::ui::browser_app::BrowserTabState;
use crate::ui::string_match::find_ascii_case_insensitive;
use crate::ui::TabId;
use memchr::{memchr, memchr3, memrchr};
use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabSearchMatch {
  pub tab_id: TabId,
  /// Index into the current `BrowserAppState::tabs` slice.
  pub tab_index: usize,
  /// Lower is better.
  pub score: u8,
}

/// Populate `out` with ranked tab matches for `query`.
///
/// Matching is ASCII case-insensitive (non-ASCII compared exactly) and does not allocate per tab.
pub fn ranked_matches_into(query: &str, tabs: &[BrowserTabState], out: &mut Vec<TabSearchMatch>) {
  out.clear();

  let query = query.trim();
  if query.is_empty() {
    out.reserve(tabs.len());
    out.extend(tabs.iter().enumerate().map(|(idx, tab)| TabSearchMatch {
      tab_id: tab.id,
      tab_index: idx,
      score: 0,
    }));
    return;
  }

  // Most queries are already lowercase; avoid allocating unless needed.
  let needle_lower: Cow<'_, str> = if query.as_bytes().iter().any(|b| b.is_ascii_uppercase()) {
    Cow::Owned(query.to_ascii_lowercase())
  } else {
    Cow::Borrowed(query)
  };

  // Over-reserve to avoid repeated growth while typing; `out` is cached by the chrome overlay.
  out.reserve(tabs.len());

  for (idx, tab) in tabs.iter().enumerate() {
    let title = tab
      .title
      .as_deref()
      .map(str::trim)
      .filter(|s| !s.is_empty())
      .or_else(|| {
        tab
          .committed_title
          .as_deref()
          .map(str::trim)
          .filter(|s| !s.is_empty())
      })
      .unwrap_or("");
    let url = tab
      .committed_url
      .as_deref()
      .or_else(|| tab.current_url.as_deref())
      .unwrap_or("");

    let mut best: Option<u8> = None;
    if let Some(pos) = find_ascii_case_insensitive(title, needle_lower.as_ref()) {
      best = Some(if pos == 0 { 0 } else { 2 });
    }
    // If the title already matches at the best possible score (prefix), skip scanning the URL.
    if best != Some(0) {
      if let Some(pos) = find_ascii_case_insensitive(url, needle_lower.as_ref()) {
        let score = if pos == 0 { 1 } else { 3 };
        best = Some(best.map_or(score, |existing| existing.min(score)));
      }
    }

    if let Some(score) = best {
      out.push(TabSearchMatch {
        tab_id: tab.id,
        tab_index: idx,
        score,
      });
    }
  }

  // Stable sort preserves original tab order for ties.
  out.sort_by_key(|m| m.score);
}

/// Compute ranked tab matches for `query`.
pub fn ranked_matches(query: &str, tabs: &[BrowserTabState]) -> Vec<TabSearchMatch> {
  let mut out = Vec::new();
  ranked_matches_into(query, tabs, &mut out);
  out
}

/// Best-effort host extraction for `http`/`https` URLs.
///
/// Returns a borrowed slice of `url` (no allocations) containing the host name, without
/// userinfo/port/brackets.
pub fn http_host(url: &str) -> Option<&str> {
  let url = url.trim();
  if url.is_empty() {
    return None;
  }

  let rest = if url
    .get(..7)
    .is_some_and(|head| head.eq_ignore_ascii_case("http://"))
  {
    &url[7..]
  } else if url
    .get(..8)
    .is_some_and(|head| head.eq_ignore_ascii_case("https://"))
  {
    &url[8..]
  } else {
    return None;
  };

  // Authority ends at the first path/query/fragment delimiter.
  let authority_end = memchr3(b'/', b'?', b'#', rest.as_bytes()).unwrap_or(rest.len());
  let authority = &rest[..authority_end];

  // Strip userinfo if present.
  let hostport = match memrchr(b'@', authority.as_bytes()) {
    Some(at) => &authority[at + 1..],
    None => authority,
  };

  if hostport.starts_with('[') {
    let end = memchr(b']', hostport.as_bytes())?;
    let host = &hostport[1..end];
    let host = host.trim();
    (!host.is_empty()).then_some(host)
  } else {
    let host = match memchr(b':', hostport.as_bytes()) {
      Some(colon) => &hostport[..colon],
      None => hostport,
    }
    .trim();
    (!host.is_empty()).then_some(host)
  }
}

#[cfg(test)]
mod tests {
  use super::{http_host, ranked_matches};
  use crate::ui::browser_app::BrowserTabState;
  use crate::ui::TabId;

  #[test]
  fn http_host_extracts_simple_host() {
    assert_eq!(http_host("https://example.com/path"), Some("example.com"));
    assert_eq!(http_host("http://example.com"), Some("example.com"));
  }

  #[test]
  fn http_host_strips_port_and_userinfo() {
    assert_eq!(
      http_host("https://user:pass@example.com:8443/foo"),
      Some("example.com")
    );
  }

  #[test]
  fn http_host_handles_ipv6_literal() {
    assert_eq!(http_host("https://[::1]:8080/"), Some("::1"));
  }

  #[test]
  fn ranked_matches_is_case_insensitive_ascii() {
    let tab_a = TabId(1);
    let mut a = BrowserTabState::new(tab_a, "https://example.com/".to_string());
    a.title = Some("GitHub".to_string());

    let matches = ranked_matches("git", &[a]);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].tab_id, tab_a);
  }
}
