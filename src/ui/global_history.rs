//! Global (cross-tab) browsing history.
//!
//! This is a small, UI-owned store intended for chrome features like:
//! - the History panel (and profile autosave),
//! - a future `about:history` page.
//!
//! # Recording semantics (what counts as a visit)
//!
//! These rules are intentionally explicit and covered by regression tests so history stays stable
//! as the UI/worker protocol evolves:
//!
//! - Visits are recorded **only** on `WorkerToUi::NavigationCommitted`.
//!   - `NavigationStarted` and `NavigationFailed` do **not** create history entries.
//! - Redirects: `NavigationCommitted` already carries the *final* URL, so the store records exactly
//!   that (no special-case required).
//! - Fragment navigations: URLs are **normalized by stripping the fragment** (`#...`) for history
//!   purposes. This avoids separate history entries for in-page anchor jumps.
//! - `about:` pages are **not** recorded (including `about:history` / `about:bookmarks`) to avoid
//!   recursive/self-referential noise and to keep internal pages out of user history.
//! - `file:` URLs **are** recorded.
//! - History is **deduped by normalized URL**. Every committed navigation increments
//!   [`GlobalHistoryEntry::visit_count`] and updates [`GlobalHistoryEntry::visited_at_ms`]
//!   (including back/forward/reload).
//!
//! If these semantics change, update the tests in this module.

use crate::ui::about_pages;
use serde::{Deserialize, Serialize};
use url::Url;

/// Default maximum number of global history entries stored.
///
/// This must remain bounded: history is stored on the UI side and is also used for omnibox/history
/// search results, so unbounded growth would cause memory issues and slow UI operations.
pub const DEFAULT_GLOBAL_HISTORY_CAPACITY: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearBrowsingDataRange {
  LastHour,
  Last24Hours,
  Last7Days,
  AllTime,
}

impl ClearBrowsingDataRange {
  pub fn label(self) -> &'static str {
    match self {
      Self::LastHour => "Last hour",
      Self::Last24Hours => "Last 24 hours",
      Self::Last7Days => "Last 7 days",
      Self::AllTime => "All time",
    }
  }
}

impl Default for ClearBrowsingDataRange {
  fn default() -> Self {
    // Safer default when the dialog is opened from a shortcut (matches common browser defaults).
    Self::LastHour
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GlobalHistoryEntry {
  pub url: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub title: Option<String>,
  /// Unix epoch milliseconds for the most recent committed visit to this URL.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub visited_at_ms: Option<u64>,
  /// Number of committed visits to this URL.
  #[serde(default = "default_visit_count")]
  pub visit_count: u64,
}

fn default_visit_count() -> u64 {
  1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct GlobalHistoryStore {
  #[serde(default)]
  pub entries: Vec<GlobalHistoryEntry>,
}

impl GlobalHistoryStore {
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// Record a committed visit to `url`.
  ///
  /// Returns `true` if the store was mutated.
  pub fn record(&mut self, url: String, title: Option<String>) -> bool {
    self.record_at_ms(url, title, now_unix_ms())
  }

  fn record_at_ms(&mut self, url: String, title: Option<String>, visited_at_ms: u64) -> bool {
    if DEFAULT_GLOBAL_HISTORY_CAPACITY == 0 {
      return false;
    }

    let Some(normalized) = normalize_url_for_history(&url) else {
      return false;
    };

    let title = normalize_title(title);

    if let Some(idx) = self.entries.iter().position(|e| e.url == normalized) {
      let mut existing = self.entries.remove(idx);
      existing.visit_count = existing.visit_count.saturating_add(1);
      existing.visited_at_ms = Some(visited_at_ms);
      if title.is_some() {
        existing.title = title;
      }
      self.entries.push(existing);
      // Existing entries do not increase store length; no capacity trim required.
      return true;
    }

    self.entries.push(GlobalHistoryEntry {
      url: normalized,
      title,
      visited_at_ms: Some(visited_at_ms),
      visit_count: 1,
    });
    trim_to_capacity(&mut self.entries);
    true
  }

  /// Iterate history entries ordered by recency (most recent first).
  pub fn iter_recent(&self) -> impl Iterator<Item = (usize, &GlobalHistoryEntry)> {
    self.entries.iter().enumerate().rev()
  }

  /// Search history entries, ordered by recency (most recent first).
  pub fn search<'a>(
    &'a self,
    query: &str,
    limit: usize,
  ) -> Vec<(usize, &'a GlobalHistoryEntry)> {
    if limit == 0 {
      return Vec::new();
    }

    let tokens: Vec<&str> = query.split_whitespace().filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() {
      return self.iter_recent().take(limit).collect();
    }

    let mut out = Vec::with_capacity(limit.min(self.entries.len()));
    'entries: for (idx, entry) in self.iter_recent() {
      for token in &tokens {
        let in_url = contains_case_insensitive(&entry.url, token);
        let in_title = entry
          .title
          .as_deref()
          .is_some_and(|t| contains_case_insensitive(t, token));
        if !in_url && !in_title {
          continue 'entries;
        }
      }

      out.push((idx, entry));
      if out.len() >= limit {
        break;
      }
    }

    out
  }

  /// Look up an entry by URL, applying the same normalization used for recording.
  pub fn get(&self, url: &str) -> Option<&GlobalHistoryEntry> {
    let key = normalize_url_for_history(url)?;
    self.entries.iter().find(|e| e.url == key)
  }

  /// Normalize + deduplicate entries in-place.
  ///
  /// This is intended as a best-effort migration step for history snapshots loaded from disk:
  /// older versions of the browser stored one entry per visit, potentially including fragments.
  pub fn normalize_in_place(&mut self) {
    let mut out: Vec<GlobalHistoryEntry> = Vec::with_capacity(self.entries.len());
    for entry in std::mem::take(&mut self.entries) {
      let Some(url) = normalize_url_for_history(&entry.url) else {
        continue;
      };

      let visit_count = entry.visit_count.max(1);
      let title = normalize_title(entry.title);
      let visited_at_ms = entry.visited_at_ms;

      if let Some(idx) = out.iter().position(|e| e.url == url) {
        let mut existing = out.remove(idx);
        existing.visit_count = existing.visit_count.saturating_add(visit_count);
        if visited_at_ms.is_some() {
          existing.visited_at_ms = visited_at_ms;
        }
        if title.is_some() {
          existing.title = title;
        }
        out.push(existing);
      } else {
        out.push(GlobalHistoryEntry {
          url,
          title,
          visited_at_ms,
          visit_count,
        });
      }
    }

    self.entries = out;
    trim_to_capacity(&mut self.entries);
  }

  pub fn remove_at(&mut self, index: usize) -> Option<GlobalHistoryEntry> {
    if index < self.entries.len() {
      Some(self.entries.remove(index))
    } else {
      None
    }
  }

  pub fn clear(&mut self) {
    self.entries.clear();
  }

  pub fn clear_range(&mut self, range: ClearBrowsingDataRange) {
    self.clear_range_at_ms(range, now_unix_ms());
  }

  pub fn clear_range_at_ms(&mut self, range: ClearBrowsingDataRange, now_ms: u64) {
    match range {
      ClearBrowsingDataRange::AllTime => {
        self.entries.clear();
      }
      ClearBrowsingDataRange::LastHour
      | ClearBrowsingDataRange::Last24Hours
      | ClearBrowsingDataRange::Last7Days => {
        let duration_ms = match range {
          ClearBrowsingDataRange::LastHour => 60 * 60 * 1000,
          ClearBrowsingDataRange::Last24Hours => 24 * 60 * 60 * 1000,
          ClearBrowsingDataRange::Last7Days => 7 * 24 * 60 * 60 * 1000,
          ClearBrowsingDataRange::AllTime => 0,
        };
        let cutoff_ms = now_ms.saturating_sub(duration_ms);
        // Keep entries that are older than the cutoff. If a timestamp is missing (legacy data),
        // keep it for partial clears so we don't accidentally delete entries we cannot classify.
        self
          .entries
          .retain(|entry| entry.visited_at_ms.map_or(true, |ms| ms < cutoff_ms));
      }
    }
  }
}

fn normalize_title(title: Option<String>) -> Option<String> {
  title
    .map(|t| t.trim().to_string())
    .filter(|t| !t.is_empty())
}

/// Normalize a URL for use in `GlobalHistoryStore` and `VisitedUrlStore`.
///
/// Semantics:
/// - Reject empty input
/// - Reject `about:` pages
/// - Allow only `http`, `https`, `file`
/// - Strip the fragment (`#...`)
pub fn normalize_url_for_history(url: &str) -> Option<String> {
  let trimmed = url.trim();
  if trimmed.is_empty() {
    return None;
  }
  if about_pages::is_about_url(trimmed) {
    return None;
  }

  if let Ok(mut parsed) = Url::parse(trimmed) {
    // `Url` normalizes the scheme to lowercase.
    let scheme = parsed.scheme();

    // Keep this allowlist conservative: the UI only allows these schemes for typed navigations,
    // and recording other schemes can produce surprising/noisy history entries.
    if !matches!(scheme, "http" | "https" | "file") {
      return None;
    }

    parsed.set_fragment(None);
    return Some(parsed.to_string());
  }

  // Best-effort fallback for weird/unparseable URLs. This should be rare (the worker generally
  // emits fully normalized absolute URLs), but keep history robust.
  let lower = trimmed.trim_start().to_ascii_lowercase();
  let scheme_end = lower.find(':')?;
  let scheme = &lower[..scheme_end];
  if scheme == "about" {
    return None;
  }
  if !matches!(scheme, "http" | "https" | "file") {
    return None;
  }

  Some(trimmed.split('#').next().unwrap_or(trimmed).to_string())
}

fn now_unix_ms() -> u64 {
  use std::time::{SystemTime, UNIX_EPOCH};

  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_millis() as u64)
    .unwrap_or(0)
}

fn trim_to_capacity(entries: &mut Vec<GlobalHistoryEntry>) {
  if DEFAULT_GLOBAL_HISTORY_CAPACITY == 0 {
    entries.clear();
    return;
  }

  if entries.len() > DEFAULT_GLOBAL_HISTORY_CAPACITY {
    let excess = entries.len() - DEFAULT_GLOBAL_HISTORY_CAPACITY;
    entries.drain(0..excess);
  }
}

fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
  // For history/omnibox usage we want lightweight, allocation-free matching. We use ASCII-only
  // case-insensitivity: non-ASCII bytes are compared exactly.
  if needle.is_empty() {
    return true;
  }

  let hay = haystack.as_bytes();
  let needle = needle.as_bytes();
  if needle.len() > hay.len() {
    return false;
  }

  for i in 0..=(hay.len() - needle.len()) {
    let mut ok = true;
    for j in 0..needle.len() {
      if hay[i + j].to_ascii_lowercase() != needle[j].to_ascii_lowercase() {
        ok = false;
        break;
      }
    }
    if ok {
      return true;
    }
  }

  false
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn strips_fragments_and_dedupes_by_normalized_url() {
    let mut store = GlobalHistoryStore::default();

    assert!(store.record_at_ms(
      "https://example.test/a#one".to_string(),
      Some("A1".to_string()),
      1
    ));
    assert_eq!(store.entries.len(), 1);
    let entry = store.entries.last().unwrap();
    assert_eq!(entry.url, "https://example.test/a");
    assert_eq!(entry.title.as_deref(), Some("A1"));
    assert_eq!(entry.visited_at_ms, Some(1));
    assert_eq!(entry.visit_count, 1);

    assert!(store.record_at_ms(
      "https://example.test/a#two".to_string(),
      Some("A2".to_string()),
      2
    ));
    assert_eq!(store.entries.len(), 1);
    let entry = store.entries.last().unwrap();
    assert_eq!(entry.url, "https://example.test/a");
    assert_eq!(entry.title.as_deref(), Some("A2"));
    assert_eq!(entry.visited_at_ms, Some(2));
    assert_eq!(entry.visit_count, 2);
  }

  #[test]
  fn dedupes_non_consecutive_and_moves_to_end() {
    let mut store = GlobalHistoryStore::default();

    store.record_at_ms("https://a.example/".to_string(), Some("A".to_string()), 1);
    store.record_at_ms("https://b.example/".to_string(), Some("B".to_string()), 2);
    store.record_at_ms("https://a.example/".to_string(), None, 3);

    assert_eq!(store.entries.len(), 2);
    assert_eq!(store.entries[0].url, "https://b.example/");
    assert_eq!(store.entries[1].url, "https://a.example/");

    let a = store.get("https://a.example/").unwrap();
    assert_eq!(a.visit_count, 2);
    assert_eq!(
      a.title.as_deref(),
      Some("A"),
      "title should not be clobbered by None"
    );
    assert_eq!(a.visited_at_ms, Some(3));
  }

  #[test]
  fn ignores_about_pages() {
    let mut store = GlobalHistoryStore::default();

    for url in ["about:newtab", "about:help", "about:history", "ABOUT:BOOKMARKS"] {
      assert!(!store.record_at_ms(url.to_string(), None, 1));
    }

    assert!(store.entries.is_empty());
  }

  #[test]
  fn records_file_urls() {
    let mut store = GlobalHistoryStore::default();

    assert!(store.record_at_ms(
      "file:///tmp/a.html#section".to_string(),
      None,
      10
    ));
    assert_eq!(store.entries.len(), 1);
    assert_eq!(store.entries[0].url, "file:///tmp/a.html");
    assert_eq!(store.entries[0].visit_count, 1);
  }

  #[test]
  fn every_committed_navigation_increments_visit_count_and_updates_last_visited() {
    let mut store = GlobalHistoryStore::default();

    store.record_at_ms("https://example.test/a".to_string(), None, 1);
    store.record_at_ms("https://example.test/a".to_string(), None, 2);
    store.record_at_ms("https://example.test/a".to_string(), None, 3);

    let entry = store.get("https://example.test/a").unwrap();
    assert_eq!(entry.visit_count, 3);
    assert_eq!(entry.visited_at_ms, Some(3));
  }

  #[test]
  fn title_is_updated_only_when_some_non_empty() {
    let mut store = GlobalHistoryStore::default();

    store.record_at_ms(
      "https://example.test/".to_string(),
      Some("Title".to_string()),
      1,
    );
    store.record_at_ms("https://example.test/".to_string(), None, 2);
    store.record_at_ms(
      "https://example.test/".to_string(),
      Some("   ".to_string()),
      3,
    );

    let entry = store.get("https://example.test/").unwrap();
    assert_eq!(entry.title.as_deref(), Some("Title"));
    assert_eq!(entry.visit_count, 3);
    assert_eq!(entry.visited_at_ms, Some(3));
  }

  #[test]
  fn normalize_in_place_merges_duplicates_and_filters_invalid_entries() {
    let mut store = GlobalHistoryStore {
      entries: vec![
        GlobalHistoryEntry {
          url: "https://a.example/#one".to_string(),
          title: Some("Old".to_string()),
          visited_at_ms: Some(1),
          visit_count: 1,
        },
        GlobalHistoryEntry {
          url: "about:newtab".to_string(),
          title: Some("New Tab".to_string()),
          visited_at_ms: Some(2),
          visit_count: 1,
        },
        GlobalHistoryEntry {
          url: "https://a.example/#two".to_string(),
          title: Some("New".to_string()),
          visited_at_ms: Some(3),
          visit_count: 1,
        },
      ],
    };

    store.normalize_in_place();

    assert_eq!(store.entries.len(), 1);
    let entry = store.entries.first().unwrap();
    assert_eq!(entry.url, "https://a.example/");
    assert_eq!(entry.title.as_deref(), Some("New"));
    assert_eq!(entry.visit_count, 2);
    assert_eq!(entry.visited_at_ms, Some(3));
  }

  #[test]
  fn clear_range_removes_entries_within_cutoff() {
    const HOUR_MS: u64 = 60 * 60 * 1000;
    const DAY_MS: u64 = 24 * HOUR_MS;
    let now_ms = 1_000_000_000_000_u64;

    let mut history = GlobalHistoryStore::default();
    history.record_at_ms("https://old.example/".to_string(), None, now_ms - 8 * DAY_MS);
    history.record_at_ms("https://days.example/".to_string(), None, now_ms - 2 * DAY_MS);
    history.record_at_ms("https://hours.example/".to_string(), None, now_ms - 2 * HOUR_MS);
    history.record_at_ms(
      "https://recent.example/".to_string(),
      None,
      now_ms - 10 * 60 * 1000,
    );

    // Legacy entry with unknown timestamp should be preserved for partial clears.
    history.entries.push(GlobalHistoryEntry {
      url: "https://unknown.example/".to_string(),
      title: None,
      visited_at_ms: None,
      visit_count: 1,
    });

    history.clear_range_at_ms(ClearBrowsingDataRange::LastHour, now_ms);
    let urls: Vec<&str> = history.entries.iter().map(|e| e.url.as_str()).collect();
    assert_eq!(
      urls,
      vec![
        "https://old.example/",
        "https://days.example/",
        "https://hours.example/",
        "https://unknown.example/",
      ]
    );

    history.clear_range_at_ms(ClearBrowsingDataRange::Last24Hours, now_ms);
    let urls: Vec<&str> = history.entries.iter().map(|e| e.url.as_str()).collect();
    assert_eq!(
      urls,
      vec![
        "https://old.example/",
        "https://days.example/",
        "https://unknown.example/",
      ]
    );

    history.clear_range_at_ms(ClearBrowsingDataRange::Last7Days, now_ms);
    let urls: Vec<&str> = history.entries.iter().map(|e| e.url.as_str()).collect();
    assert_eq!(urls, vec!["https://old.example/", "https://unknown.example/"]);

    history.clear_range_at_ms(ClearBrowsingDataRange::AllTime, now_ms);
    assert!(history.entries.is_empty());
  }

  #[test]
  fn search_returns_results_ordered_by_recency() {
    let mut history = GlobalHistoryStore::default();
    history.record_at_ms(
      "https://example.com/a".to_string(),
      Some("First".to_string()),
      1,
    );
    history.record_at_ms(
      "https://example.com/b".to_string(),
      Some("Second".to_string()),
      2,
    );
    history.record_at_ms(
      "https://example.com/a".to_string(),
      Some("Third".to_string()),
      3,
    );

    let results = history.search("example", 10);
    let titles: Vec<Option<&str>> = results.iter().map(|(_, e)| e.title.as_deref()).collect();
    assert_eq!(titles, vec![Some("Third"), Some("Second")]);
  }
}
