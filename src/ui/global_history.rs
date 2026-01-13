//! Global (cross-tab) browsing history.
//!
//! This is a small, UI-owned store intended for chrome features like:
//! - the History panel (and profile autosave),
//! - `about:history` / "Recently visited" sections.
//!
//! # Model
//!
//! The store is a **per-URL summary**: entries are deduplicated by a normalized URL. Each committed
//! navigation:
//! - increments [`GlobalHistoryEntry::visit_count`]
//! - updates [`GlobalHistoryEntry::visited_at_ms`]
//! - updates [`GlobalHistoryEntry::title`] only when a non-empty title is provided
//! - moves the entry to the end of the list (most-recent)
//!
//! The store is bounded by a configurable capacity (default: [`DEFAULT_GLOBAL_HISTORY_CAPACITY`]),
//! evicting the oldest entries first.
//!
//! # URL normalization
//!
//! These rules are intentionally explicit and covered by regression tests so history stays stable
//! as the UI/worker protocol evolves:
//!
//! - Fragment navigations: URLs are normalized by stripping the fragment (`#...`) for history
//!   purposes. This avoids separate history entries for in-page anchor jumps.
//! - `about:` pages are not recorded (including `about:history` / `about:bookmarks`) to avoid
//!   recursive/self-referential noise and to keep internal pages out of user history.
//! - `file:` URLs are recorded.
//! - A conservative scheme allowlist is applied: only http/https/file are recorded.
//! - Titles are trimmed and empty titles are treated as missing.
//!
//! If these semantics change, update the tests in this module.

use crate::ui::about_pages;
use serde::{de, Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

use super::string_match::contains_ascii_case_insensitive;

/// Current on-disk schema version for [`GlobalHistoryStore`].
pub const GLOBAL_HISTORY_SCHEMA_VERSION: u32 = 1;

/// Default maximum number of global history entries stored in-memory and persisted to disk.
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
  ///
  /// This is required for all new entries. Deserialization is permissive so older persisted files
  /// that used `Option<u64>` (or omitted the field) can be migrated.
  #[serde(default, deserialize_with = "deserialize_u64_or_null", alias = "ts")]
  pub visited_at_ms: u64,
  /// Number of committed visits to this URL.
  #[serde(default = "default_visit_count")]
  pub visit_count: u64,
}

fn default_visit_count() -> u64 {
  1
}

#[derive(Debug, Clone, Serialize)]
pub struct GlobalHistoryStore {
  schema_version: u32,
  pub entries: Vec<GlobalHistoryEntry>,
  #[serde(skip)]
  capacity: usize,
  /// Monotonic revision counter incremented on every mutation of `entries`.
  ///
  /// This allows UI layers to cheaply detect whether cached search results are still valid.
  #[serde(skip)]
  revision: u64,
}

impl PartialEq for GlobalHistoryStore {
  fn eq(&self, other: &Self) -> bool {
    self.schema_version == other.schema_version && self.entries == other.entries
  }
}

impl Eq for GlobalHistoryStore {}

#[derive(Debug, Deserialize)]
struct GlobalHistoryStoreV1 {
  schema_version: u32,
  #[serde(default)]
  entries: Vec<GlobalHistoryEntry>,
}

#[derive(Debug, Deserialize)]
struct LegacyGlobalHistoryStore {
  #[serde(default)]
  entries: Vec<GlobalHistoryEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GlobalHistoryStoreFile {
  V1(GlobalHistoryStoreV1),
  Legacy(LegacyGlobalHistoryStore),
  LegacyVec(Vec<GlobalHistoryEntry>),
}

impl<'de> Deserialize<'de> for GlobalHistoryStore {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let file = GlobalHistoryStoreFile::deserialize(deserializer)?;
    let mut store = match file {
      GlobalHistoryStoreFile::V1(v1) => {
        if v1.schema_version != GLOBAL_HISTORY_SCHEMA_VERSION {
          return Err(de::Error::custom(format!(
            "unsupported global history schema_version {}; expected {}",
            v1.schema_version, GLOBAL_HISTORY_SCHEMA_VERSION
          )));
        }
        GlobalHistoryStore {
          schema_version: v1.schema_version,
          entries: v1.entries,
          capacity: DEFAULT_GLOBAL_HISTORY_CAPACITY,
          revision: 0,
        }
      }
      GlobalHistoryStoreFile::Legacy(legacy) => GlobalHistoryStore {
        schema_version: GLOBAL_HISTORY_SCHEMA_VERSION,
        entries: legacy.entries,
        capacity: DEFAULT_GLOBAL_HISTORY_CAPACITY,
        revision: 0,
      },
      GlobalHistoryStoreFile::LegacyVec(entries) => GlobalHistoryStore {
        schema_version: GLOBAL_HISTORY_SCHEMA_VERSION,
        entries,
        capacity: DEFAULT_GLOBAL_HISTORY_CAPACITY,
        revision: 0,
      },
    };

    store.normalize_after_load();
    Ok(store)
  }
}

impl Default for GlobalHistoryStore {
  fn default() -> Self {
    Self::with_capacity(DEFAULT_GLOBAL_HISTORY_CAPACITY)
  }
}

impl GlobalHistoryStore {
  pub fn with_capacity(capacity: usize) -> Self {
    Self {
      schema_version: GLOBAL_HISTORY_SCHEMA_VERSION,
      entries: Vec::new(),
      capacity,
      revision: 0,
    }
  }

  /// Monotonic revision counter incremented on every mutation of the history store.
  pub fn revision(&self) -> u64 {
    self.revision
  }

  fn bump_revision(&mut self) {
    self.revision = self.revision.wrapping_add(1);
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// Iterate history entries ordered by recency (most recent first).
  pub fn iter_recent(&self) -> impl Iterator<Item = (usize, &GlobalHistoryEntry)> {
    self.entries.iter().enumerate().rev()
  }

  /// Record a committed visit to `url`.
  ///
  /// Returns `true` if the store was mutated.
  pub fn record(&mut self, url: String, title: Option<String>) -> bool {
    self.record_at_ms(url, title, now_unix_ms())
  }

  fn record_at_ms(&mut self, url: String, title: Option<String>, visited_at_ms: u64) -> bool {
    if self.capacity == 0 {
      return false;
    }

    let Some(normalized) = normalize_url_for_history(&url) else {
      return false;
    };
    let title = normalize_title(title);

    if let Some(idx) = self.entries.iter().position(|e| e.url == normalized) {
      let mut existing = self.entries.remove(idx);
      existing.visit_count = existing.visit_count.max(1).saturating_add(1);
      existing.visited_at_ms = visited_at_ms;
      if title.is_some() {
        existing.title = title;
      }
      self.entries.push(existing);
      // Existing entries do not increase store length; no capacity trim required.
      self.bump_revision();
      return true;
    }

    self.entries.push(GlobalHistoryEntry {
      url: normalized,
      title,
      visited_at_ms,
      visit_count: 1,
    });
    self.enforce_capacity();
    self.bump_revision();
    true
  }

  /// Look up an entry by URL, applying the same normalization used for recording.
  pub fn get(&self, url: &str) -> Option<&GlobalHistoryEntry> {
    let key = normalize_url_for_history(url)?;
    self.entries.iter().find(|e| e.url == key)
  }

  /// Search history entries, ordered by recency (most recent first).
  pub fn search<'a>(&'a self, query: &str, limit: usize) -> Vec<(usize, &'a GlobalHistoryEntry)> {
    if limit == 0 {
      return Vec::new();
    }

    // Lowercase once so we can use the fast ASCII-only matcher (non-ASCII bytes compare exactly).
    let query_lower = query.to_ascii_lowercase();
    let tokens: Vec<&str> = query_lower
      .split_whitespace()
      .filter(|t| !t.is_empty())
      .collect();
    if tokens.is_empty() {
      return self.iter_recent().take(limit).collect();
    }

    let mut out = Vec::with_capacity(limit.min(self.entries.len()));
    'entries: for (idx, entry) in self.iter_recent() {
      for token in &tokens {
        let in_url = contains_ascii_case_insensitive(&entry.url, token);
        let in_title = entry
          .title
          .as_deref()
          .is_some_and(|t| contains_ascii_case_insensitive(t, token));
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

  /// Delete a single history entry by URL.
  ///
  /// Returns `true` when an entry was removed.
  pub fn delete_entry(&mut self, url: &str) -> bool {
    let Some(key) = normalize_url_for_history(url) else {
      return false;
    };
    let Some(idx) = self.entries.iter().position(|e| e.url == key) else {
      return false;
    };
    self.entries.remove(idx);
    self.bump_revision();
    true
  }

  /// Delete an entry by its index in [`GlobalHistoryStore::entries`].
  pub fn remove_at(&mut self, index: usize) -> Option<GlobalHistoryEntry> {
    if index < self.entries.len() {
      let removed = self.entries.remove(index);
      self.bump_revision();
      Some(removed)
    } else {
      None
    }
  }

  /// Remove all global history entries.
  pub fn clear_all(&mut self) {
    if self.entries.is_empty() {
      return;
    }
    self.entries.clear();
    self.bump_revision();
  }

  /// Legacy alias for [`GlobalHistoryStore::clear_all`].
  pub fn clear(&mut self) {
    self.clear_all();
  }

  /// Clear entries visited at or after `since_ms`.
  ///
  /// Entries with unknown timestamps (`visited_at_ms == 0`) are preserved unless `since_ms == 0`.
  pub fn clear_since(&mut self, since_ms: u64) {
    if since_ms == 0 {
      if self.entries.is_empty() {
        return;
      }
      self.entries.clear();
      self.bump_revision();
      return;
    }
    let before = self.entries.len();
    self
      .entries
      .retain(|e| e.visited_at_ms == 0 || e.visited_at_ms < since_ms);
    if self.entries.len() != before {
      self.bump_revision();
    }
  }

  /// Clear entries visited within `range` (`start_ms..end_ms`, end-exclusive).
  ///
  /// Entries with unknown timestamps (`visited_at_ms == 0`) are preserved unless the range starts
  /// at `0`.
  pub fn clear_range(&mut self, range: Range<u64>) {
    if range.start >= range.end {
      return;
    }
    let before = self.entries.len();
    self.entries.retain(|e| {
      if e.visited_at_ms == 0 && range.start > 0 {
        return true;
      }
      e.visited_at_ms < range.start || e.visited_at_ms >= range.end
    });
    if self.entries.len() != before {
      self.bump_revision();
    }
  }

  pub fn clear_browsing_data_range(&mut self, range: ClearBrowsingDataRange) {
    self.clear_browsing_data_range_at_ms(range, now_unix_ms());
  }

  pub fn clear_browsing_data_range_at_ms(&mut self, range: ClearBrowsingDataRange, now_ms: u64) {
    match range {
      ClearBrowsingDataRange::AllTime => {
        self.clear_all();
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
        self.clear_since(cutoff_ms);
      }
    }
  }

  /// Normalize + deduplicate entries in-place.
  ///
  /// This is intended as a best-effort migration step for history snapshots loaded from disk:
  /// older versions of the browser stored one entry per visit, potentially including fragments.
  pub fn normalize_in_place(&mut self) {
    self.schema_version = GLOBAL_HISTORY_SCHEMA_VERSION;
    // This is a potentially large mutation (dedupe + reorder). It's also called on load, so treat
    // it as a logical revision bump for cache invalidation even if the end result happens to match
    // the existing ordering.
    self.bump_revision();

    if self.capacity == 0 {
      self.entries.clear();
      return;
    }

    #[derive(Debug, Default, Clone)]
    struct Agg {
      title: Option<String>,
      visited_at_ms: u64,
      visit_count: u64,
      last_seen_idx: usize,
    }

    let mut by_url: HashMap<String, Agg> = HashMap::with_capacity(self.entries.len());
    for (idx, entry) in std::mem::take(&mut self.entries).into_iter().enumerate() {
      let Some(url) = normalize_url_for_history(&entry.url) else {
        continue;
      };

      let title = normalize_title(entry.title);
      let visited_at_ms = entry.visited_at_ms;
      let visit_count = entry.visit_count.max(1);

      let agg = by_url.entry(url).or_insert_with(|| Agg {
        last_seen_idx: idx,
        ..Default::default()
      });

      agg.visit_count = agg.visit_count.saturating_add(visit_count);

      // Prefer the newest timestamp; break ties using file order so migration is deterministic even
      // when timestamps are missing (legacy stores).
      let is_newer = visited_at_ms > agg.visited_at_ms
        || (visited_at_ms == agg.visited_at_ms && idx >= agg.last_seen_idx);

      if is_newer {
        agg.visited_at_ms = visited_at_ms;
        agg.last_seen_idx = idx;
        if title.is_some() {
          agg.title = title;
        }
      } else if agg.title.is_none() && title.is_some() {
        // Best-effort: if the most-recent entry is missing a title but an older one has it, keep
        // the known title instead of falling back to the raw URL.
        agg.title = title;
      }
    }

    let mut entries: Vec<(u64, usize, GlobalHistoryEntry)> = by_url
      .into_iter()
      .map(|(url, agg)| {
        (
          agg.visited_at_ms,
          agg.last_seen_idx,
          GlobalHistoryEntry {
            url,
            title: agg.title,
            visited_at_ms: agg.visited_at_ms,
            visit_count: agg.visit_count.max(1),
          },
        )
      })
      .collect();

    // Ensure deterministic ordering and recency semantics after migration: oldest → newest.
    entries.sort_by(|a, b| {
      a.0
        .cmp(&b.0)
        .then_with(|| a.1.cmp(&b.1))
        .then_with(|| a.2.url.cmp(&b.2.url))
    });

    self.entries = entries.into_iter().map(|(_, _, e)| e).collect();
    self.enforce_capacity();
  }

  fn normalize_after_load(&mut self) {
    // Ensure we always persist the current version on the next save.
    self.schema_version = GLOBAL_HISTORY_SCHEMA_VERSION;
    self.normalize_in_place();
  }

  fn enforce_capacity(&mut self) {
    if self.capacity == 0 {
      self.entries.clear();
      return;
    }

    if self.entries.len() > self.capacity {
      let excess = self.entries.len() - self.capacity;
      self.entries.drain(0..excess);
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
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| {
      let ms = d.as_millis();
      if ms > u64::MAX as u128 {
        u64::MAX
      } else {
        ms as u64
      }
    })
    .unwrap_or(0)
}

fn deserialize_u64_or_null<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
  D: Deserializer<'de>,
{
  Ok(Option::<u64>::deserialize(deserializer)?.unwrap_or(0))
}

/// Cached search helper for [`GlobalHistoryStore`].
///
/// This is intended for UI callers (like the History panel) that re-run the same search query every
/// frame. When both the query string and the history store revision are unchanged, the per-call
/// work is O(1) (returning a slice of cached match indices).
#[derive(Debug, Default, Clone)]
pub struct GlobalHistorySearcher {
  last_query: String,
  /// Cached lowercase tokens for `last_query`.
  ///
  /// Tokens are ASCII-lowercased so they can be passed directly to
  /// [`contains_ascii_case_insensitive`].
  last_tokens_lower: Vec<String>,
  last_revision: u64,
  cached_limit: usize,
  cached_complete: bool,
  cached_match_indices: Vec<usize>,
}

impl GlobalHistorySearcher {
  pub fn new() -> Self {
    Self::default()
  }

  /// Search `store` for `query`, returning indices into [`GlobalHistoryStore::entries`] ordered by
  /// recency (most recent first).
  ///
  /// - When `query` and `store.revision()` are unchanged, this returns cached indices without
  ///   re-tokenizing or rescanning the history store.
  /// - `limit == 0` always returns an empty slice.
  pub fn search_indices<'a>(
    &'a mut self,
    store: &GlobalHistoryStore,
    query: &str,
    limit: usize,
  ) -> &'a [usize] {
    if limit == 0 {
      self.cached_match_indices.clear();
      self.cached_limit = 0;
      self.cached_complete = true;
      // Keep `last_query`/`last_revision` as-is; `limit == 0` is not a meaningful cache state.
      return &self.cached_match_indices;
    }

    let store_revision = store.revision();
    let query_changed = query != self.last_query;
    let store_changed = store_revision != self.last_revision;
    let needs_more = limit > self.cached_limit && !self.cached_complete;
    if query_changed || store_changed || needs_more {
      // Only re-tokenize when the query itself changes; if history mutates while the query stays
      // stable we can reuse the cached tokens.
      if query_changed {
        self.last_query = query.to_string();
        let query_lower = query.to_ascii_lowercase();
        self.last_tokens_lower = query_lower
          .split_whitespace()
          .filter(|t| !t.is_empty())
          .map(|t| t.to_string())
          .collect();
      }
      self.last_revision = store_revision;

      let (indices, complete) =
        compute_search_match_indices(store, &self.last_tokens_lower, limit);
      self.cached_match_indices = indices;
      self.cached_complete = complete;
      self.cached_limit = limit;
    }

    let n = limit.min(self.cached_match_indices.len());
    &self.cached_match_indices[..n]
  }
}

fn compute_search_match_indices(
  store: &GlobalHistoryStore,
  tokens: &[String],
  limit: usize,
) -> (Vec<usize>, bool) {
  if limit == 0 {
    return (Vec::new(), true);
  }

  if tokens.is_empty() {
    let indices: Vec<usize> = store
      .iter_recent()
      .take(limit)
      .map(|(idx, _)| idx)
      .collect();
    let complete = store.entries.len() <= limit;
    return (indices, complete);
  }

  let mut out = Vec::with_capacity(limit.min(store.entries.len()));
  'entries: for (idx, entry) in store.iter_recent() {
    for token in tokens {
      let in_url = contains_ascii_case_insensitive(&entry.url, token);
      let in_title = entry
        .title
        .as_deref()
        .is_some_and(|t| contains_ascii_case_insensitive(t, token));
      if !in_url && !in_title {
        continue 'entries;
      }
    }

    out.push(idx);
    if out.len() >= limit {
      return (out, false);
    }
  }

  (out, true)
}
#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn strips_fragments_and_dedupes_by_normalized_url() {
    let mut store = GlobalHistoryStore::with_capacity(10);

    assert!(store.record_at_ms(
      "https://example.test/a#one".to_string(),
      Some("A1".to_string()),
      1
    ));
    assert_eq!(store.entries.len(), 1);
    let entry = store.entries.last().unwrap();
    assert_eq!(entry.url, "https://example.test/a");
    assert_eq!(entry.title.as_deref(), Some("A1"));
    assert_eq!(entry.visited_at_ms, 1);
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
    assert_eq!(entry.visited_at_ms, 2);
    assert_eq!(entry.visit_count, 2);
  }

  #[test]
  fn dedupes_non_consecutive_and_moves_to_end() {
    let mut store = GlobalHistoryStore::with_capacity(10);

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
      "title should not be clobbered"
    );
    assert_eq!(a.visited_at_ms, 3);
  }

  #[test]
  fn ignores_about_pages() {
    let mut store = GlobalHistoryStore::with_capacity(10);

    for url in [
      "about:newtab",
      "about:help",
      "about:history",
      "ABOUT:BOOKMARKS",
    ] {
      assert!(!store.record_at_ms(url.to_string(), None, 1));
    }

    assert!(store.entries.is_empty());
  }

  #[test]
  fn records_file_urls() {
    let mut store = GlobalHistoryStore::with_capacity(10);

    assert!(store.record_at_ms("file:///tmp/a.html#section".to_string(), None, 10));
    assert_eq!(store.entries.len(), 1);
    assert_eq!(store.entries[0].url, "file:///tmp/a.html");
    assert_eq!(store.entries[0].visit_count, 1);
  }

  #[test]
  fn every_committed_navigation_increments_visit_count_and_updates_last_visited() {
    let mut store = GlobalHistoryStore::with_capacity(10);

    store.record_at_ms("https://example.test/a".to_string(), None, 1);
    store.record_at_ms("https://example.test/a".to_string(), None, 2);
    store.record_at_ms("https://example.test/a".to_string(), None, 3);

    let entry = store.get("https://example.test/a").unwrap();
    assert_eq!(entry.visit_count, 3);
    assert_eq!(entry.visited_at_ms, 3);
  }

  #[test]
  fn title_is_updated_only_when_some_non_empty() {
    let mut store = GlobalHistoryStore::with_capacity(10);

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
    assert_eq!(entry.visited_at_ms, 3);
  }

  #[test]
  fn capacity_is_enforced_by_dropping_oldest_entries_first() {
    let mut store = GlobalHistoryStore::with_capacity(2);
    store.record_at_ms("https://a.example/".to_string(), None, 1);
    store.record_at_ms("https://b.example/".to_string(), None, 2);
    store.record_at_ms("https://c.example/".to_string(), None, 3);

    assert_eq!(store.len(), 2);
    let urls: Vec<&str> = store.iter_recent().map(|(_, e)| e.url.as_str()).collect();
    assert_eq!(urls, vec!["https://c.example/", "https://b.example/"]);
  }

  #[test]
  fn search_matches_all_tokens_in_url_or_title_and_is_recency_first() {
    let mut history = GlobalHistoryStore::with_capacity(10);

    history.record_at_ms(
      "https://example.com/one".to_string(),
      Some("First Page".to_string()),
      1,
    );
    history.record_at_ms(
      "https://example.com/two".to_string(),
      Some("Second Page".to_string()),
      2,
    );
    history.record_at_ms(
      "https://rust-lang.org/".to_string(),
      Some("Rust Language".to_string()),
      3,
    );

    let out = history.search("page example", 10);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].1.url, "https://example.com/two");
    assert_eq!(out[1].1.url, "https://example.com/one");

    // Case-insensitive.
    let out = history.search("RUST", 10);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].1.url, "https://rust-lang.org/");

    // Empty query returns recent entries.
    let out = history.search("   ", 2);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].1.url, "https://rust-lang.org/");
    assert_eq!(out[1].1.url, "https://example.com/two");
  }

  #[test]
  fn delete_entry_removes_single_matching_url() {
    let mut history = GlobalHistoryStore::with_capacity(10);
    history.record_at_ms("https://a.example/".to_string(), None, 1);
    history.record_at_ms("https://b.example/".to_string(), None, 2);
    history.record_at_ms("https://c.example/".to_string(), None, 3);

    assert!(history.delete_entry("https://b.example/"));
    assert!(!history.delete_entry("https://b.example/"));

    let urls: Vec<&str> = history.iter_recent().map(|(_, e)| e.url.as_str()).collect();
    assert_eq!(urls, vec!["https://c.example/", "https://a.example/"]);
  }

  #[test]
  fn remove_at_removes_entries_by_index() {
    let mut history = GlobalHistoryStore::with_capacity(10);
    history.record_at_ms("https://a.example/".to_string(), None, 1);
    history.record_at_ms("https://b.example/".to_string(), None, 2);
    history.record_at_ms("https://c.example/".to_string(), None, 3);

    assert_eq!(history.remove_at(99), None);

    let removed = history.remove_at(1).expect("should remove b");
    assert_eq!(removed.url, "https://b.example/");

    let urls: Vec<&str> = history.iter_recent().map(|(_, e)| e.url.as_str()).collect();
    assert_eq!(urls, vec!["https://c.example/", "https://a.example/"]);
  }

  #[test]
  fn clear_all_since_and_range() {
    let mut history = GlobalHistoryStore::with_capacity(10);
    history.record_at_ms("https://a.example/".to_string(), None, 10);
    history.record_at_ms("https://b.example/".to_string(), None, 20);
    history.record_at_ms("https://c.example/".to_string(), None, 30);

    history.clear_since(25);
    let urls: Vec<&str> = history.iter_recent().map(|(_, e)| e.url.as_str()).collect();
    assert_eq!(urls, vec!["https://b.example/", "https://a.example/"]);

    history.clear_range(15..25);
    let urls: Vec<&str> = history.iter_recent().map(|(_, e)| e.url.as_str()).collect();
    assert_eq!(urls, vec!["https://a.example/"]);

    history.clear_range(5..5); // empty range
    assert_eq!(history.len(), 1);

    history.clear_all();
    assert!(history.is_empty());
  }

  #[test]
  fn legacy_json_is_migrated_deduped_and_normalized() {
    let raw = r#"{
      "entries": [
        { "url": "https://a.example/#one", "title": "Old", "visited_at_ms": 1 },
        { "url": "about:newtab", "title": "New Tab", "visited_at_ms": 2 },
        { "url": "https://a.example/#two", "title": "New", "visited_at_ms": 3 }
      ]
    }"#;

    let store: GlobalHistoryStore = serde_json::from_str(raw).expect("legacy JSON should parse");
    assert_eq!(store.schema_version, GLOBAL_HISTORY_SCHEMA_VERSION);
    assert_eq!(store.entries.len(), 1);
    let entry = store.entries.first().unwrap();
    assert_eq!(entry.url, "https://a.example/");
    assert_eq!(entry.title.as_deref(), Some("New"));
    assert_eq!(entry.visit_count, 2);
    assert_eq!(entry.visited_at_ms, 3);
  }

  #[test]
  fn legacy_ts_field_is_migrated() {
    let raw = r#"[{ "url": "https://example.com", "title": "Example", "ts": 123 }]"#;
    let store: GlobalHistoryStore = serde_json::from_str(raw).expect("legacy JSON should parse");
    assert_eq!(store.entries.len(), 1);
    let entry = store.entries.first().unwrap();
    assert_eq!(entry.url, "https://example.com/");
    assert_eq!(entry.title.as_deref(), Some("Example"));
    assert_eq!(entry.visited_at_ms, 123);
    assert_eq!(entry.visit_count, 1);
  }

  #[test]
  fn serialization_is_versioned() {
    let mut store = GlobalHistoryStore::with_capacity(10);
    store.record_at_ms("https://a.example/".to_string(), Some("A".to_string()), 1);

    let v = serde_json::to_value(&store).unwrap();
    assert_eq!(
      v.get("schema_version").and_then(|v| v.as_u64()),
      Some(GLOBAL_HISTORY_SCHEMA_VERSION as u64)
    );
    assert!(v
      .get("entries")
      .and_then(|v| v.as_array())
      .is_some_and(|arr| !arr.is_empty()));
  }

  #[test]
  fn clear_browsing_data_range_removes_entries_within_cutoff() {
    const HOUR_MS: u64 = 60 * 60 * 1000;
    const DAY_MS: u64 = 24 * HOUR_MS;
    let now_ms = 1_000_000_000_000_u64;

    let mut history = GlobalHistoryStore::default();
    history.record_at_ms(
      "https://old.example/".to_string(),
      None,
      now_ms - 8 * DAY_MS,
    );
    history.record_at_ms(
      "https://days.example/".to_string(),
      None,
      now_ms - 2 * DAY_MS,
    );
    history.record_at_ms(
      "https://hours.example/".to_string(),
      None,
      now_ms - 2 * HOUR_MS,
    );
    history.record_at_ms(
      "https://recent.example/".to_string(),
      None,
      now_ms - 10 * 60 * 1000,
    );

    // Legacy entry with unknown timestamp should be preserved for partial clears.
    history.entries.push(GlobalHistoryEntry {
      url: "https://unknown.example/".to_string(),
      title: None,
      visited_at_ms: 0,
      visit_count: 1,
    });

    history.clear_browsing_data_range_at_ms(ClearBrowsingDataRange::LastHour, now_ms);
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

    history.clear_browsing_data_range_at_ms(ClearBrowsingDataRange::Last24Hours, now_ms);
    let urls: Vec<&str> = history.entries.iter().map(|e| e.url.as_str()).collect();
    assert_eq!(
      urls,
      vec![
        "https://old.example/",
        "https://days.example/",
        "https://unknown.example/",
      ]
    );

    history.clear_browsing_data_range_at_ms(ClearBrowsingDataRange::Last7Days, now_ms);
    let urls: Vec<&str> = history.entries.iter().map(|e| e.url.as_str()).collect();
    assert_eq!(
      urls,
      vec!["https://old.example/", "https://unknown.example/"]
    );

    history.clear_browsing_data_range_at_ms(ClearBrowsingDataRange::AllTime, now_ms);
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

  #[test]
  fn searcher_invalidates_cache_on_query_change() {
    let mut history = GlobalHistoryStore::with_capacity(10);
    history.record_at_ms(
      "https://example.com/one".to_string(),
      Some("Example One".to_string()),
      1,
    );
    history.record_at_ms(
      "https://rust-lang.org/".to_string(),
      Some("Rust".to_string()),
      2,
    );

    let mut searcher = GlobalHistorySearcher::default();
    let example = searcher.search_indices(&history, "example", 10).to_vec();
    assert_eq!(example.len(), 1);
    assert_eq!(history.entries[example[0]].url, "https://example.com/one");

    let rust = searcher.search_indices(&history, "rust", 10).to_vec();
    assert_eq!(rust.len(), 1);
    assert_eq!(history.entries[rust[0]].url, "https://rust-lang.org/");
  }

  #[test]
  fn searcher_invalidates_cache_on_history_mutation() {
    let mut history = GlobalHistoryStore::with_capacity(10);
    history.record_at_ms(
      "https://example.com/one".to_string(),
      Some("One".to_string()),
      1,
    );
    history.record_at_ms(
      "https://example.com/two".to_string(),
      Some("Two".to_string()),
      2,
    );

    let mut searcher = GlobalHistorySearcher::default();
    let before = searcher.search_indices(&history, "example", 10).to_vec();
    assert_eq!(before.len(), 2);
    assert_eq!(history.entries[before[0]].url, "https://example.com/two");

    // Mutation: add a newer matching entry.
    history.record_at_ms(
      "https://example.com/three".to_string(),
      Some("Three".to_string()),
      3,
    );

    let after_add = searcher.search_indices(&history, "example", 10).to_vec();
    assert_eq!(after_add.len(), 3);
    assert_eq!(
      history.entries[after_add[0]].url,
      "https://example.com/three"
    );

    // Mutation: delete a matching entry.
    assert!(history.delete_entry("https://example.com/two"));
    let after_delete = searcher.search_indices(&history, "example", 10).to_vec();
    assert_eq!(after_delete.len(), 2);
    assert!(after_delete
      .iter()
      .all(|idx| history.entries[*idx].url != "https://example.com/two"));
  }
}
