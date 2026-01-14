//! Visited URL history for omnibox/autocomplete.
//!
//! This is intentionally *not* the same as per-tab back/forward history (`ui::history::TabHistory`).
//! The omnibox wants a global list of "things the user has visited" that can be searched/filtered
//! for suggestions.
//!
//! ## `about:` URLs policy
//!
//! Internal `about:*` pages are a mix of:
//! - user-facing destinations that are useful to keep discoverable (`about:help`, `about:version`),
//! - and transient/internal pages that are created automatically (`about:newtab`, `about:error`) or
//!   exist only for tests (`about:test-*`).
//!
//! Recording all `about:*` pages quickly pollutes history and makes omnibox suggestions noisy.
//! We therefore keep a small allowlist of useful `about:` pages and ignore the rest.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::time::SystemTime;

use smallvec::SmallVec;

use super::about_pages;
use super::GlobalHistoryStore;
use super::string_match::contains_ascii_case_insensitive;

/// Default maximum number of unique visited URLs stored in-memory.
///
/// This is intentionally bounded so the UI thread can offer omnibox suggestions without unbounded
/// memory growth.
pub const DEFAULT_VISITED_URL_CAPACITY: usize = 5000;

/// Decide whether a committed navigation should be added to omnibox visited history.
///
/// Rationale:
/// - `about:newtab`, `about:blank`, and `about:error` are often created automatically and would
///   dominate the history list.
/// - `about:test-*` pages exist for deterministic UI/worker tests and should never leak into user
///   history.
/// - Some `about:` pages are genuinely useful to revisit/auto-complete (`about:help`,
///   `about:version`, `about:gpu`), so we keep them.
///
/// Unknown `about:` pages default to **not** being recorded: internal pages are more likely to be
/// transient than user-facing. If a new `about:` page should be discoverable via visited history,
/// add it to the allowlist below.
pub fn should_record_visit_in_history(url: &str) -> bool {
  let trimmed = url.trim();
  if trimmed.is_empty() {
    return false;
  }

  let lower = trimmed.to_ascii_lowercase();
  if !lower.starts_with("about:") {
    return true;
  }

  // `about:` pages may be used with query strings/fragments; the base identifier defines policy.
  let base = lower
    .split(|c| matches!(c, '?' | '#'))
    .next()
    .unwrap_or(lower.as_str());

  match base {
    about_pages::ABOUT_HELP | about_pages::ABOUT_VERSION | about_pages::ABOUT_GPU => true,
    about_pages::ABOUT_NEWTAB | about_pages::ABOUT_BLANK | about_pages::ABOUT_ERROR => false,
    _ if base.starts_with("about:test-") => false,
    _ => false,
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisitedUrlRecord {
  pub url: String,
  pub title: Option<String>,
  pub last_visited: SystemTime,
  pub visit_count: u32,
}

#[derive(Debug)]
pub struct VisitedUrlStore {
  records: VecDeque<VisitedUrlRecord>,
  capacity: usize,
  /// Monotonic revision counter incremented on every mutation of `records`.
  ///
  /// This allows UI callers to cheaply detect whether cached search results are still valid.
  revision: u64,
  /// Fast URL → record index lookup for [`VisitedUrlStore::get`] / [`VisitedUrlStore::record_visit`].
  ///
  /// This is derived state and is rebuilt by constructors/seeders.
  ///
  /// ## Index base
  ///
  /// `VecDeque` supports `pop_front()` without shifting elements, but logical indices still change
  /// (every remaining element's index is decremented). To avoid `O(n)` index rewrites on every
  /// capacity eviction, we store indices as `url_index_base + logical_index`, and update
  /// `url_index_base` when we pop from the front.
  url_index: HashMap<String, usize>,
  url_index_base: usize,
}

impl VisitedUrlStore {
  pub fn new() -> Self {
    Self::with_capacity(DEFAULT_VISITED_URL_CAPACITY)
  }

  pub fn with_capacity(capacity: usize) -> Self {
    Self {
      records: VecDeque::new(),
      capacity,
      revision: 0,
      url_index: HashMap::new(),
      url_index_base: 0,
    }
  }

  /// Monotonic revision counter incremented on every mutation of the store.
  pub fn revision(&self) -> u64 {
    self.revision
  }

  fn bump_revision(&mut self) {
    self.revision = self.revision.wrapping_add(1);
  }

  pub fn len(&self) -> usize {
    self.records.len()
  }

  pub fn is_empty(&self) -> bool {
    self.records.is_empty()
  }

  pub fn clear(&mut self) {
    if self.records.is_empty() {
      return;
    }
    self.records.clear();
    self.url_index.clear();
    self.url_index_base = 0;
    self.bump_revision();
  }

  pub fn iter_recent(&self) -> impl Iterator<Item = &VisitedUrlRecord> {
    self.records.iter().rev()
  }

  /// Record a visit to `url`.
  ///
  /// If the URL already exists in the store, it is deduplicated in-place:
  /// - `last_visited` is always refreshed
  /// - `title` is refreshed only when `title` is `Some(..)`
  pub fn record_visit(&mut self, url: String, title: Option<String>) {
    self.record_visit_at_with_count(url, title, SystemTime::now(), 1);
  }

  pub(crate) fn record_visit_at(
    &mut self,
    url: String,
    title: Option<String>,
    visited_at: SystemTime,
  ) {
    self.record_visit_at_with_count(url, title, visited_at, 1);
  }

  fn record_visit_at_with_count(
    &mut self,
    url: String,
    title: Option<String>,
    visited_at: SystemTime,
    visit_count: u32,
  ) {
    if self.capacity == 0 {
      return;
    }

    let trimmed = url.trim();
    if trimmed.is_empty() {
      return;
    }
    if !should_record_visit_in_history(trimmed) {
      return;
    }

    let url = if trimmed.len() == url.len() {
      url
    } else {
      trimmed.to_string()
    };
    let visit_count = visit_count.max(1);

    // Dedup/update existing entry when present.
    if let Some(idx) = self.lookup_index(url.as_str()) {
      // Fast path: the entry is already most-recent; update in place without shifting.
      if idx == self.records.len().saturating_sub(1)
        && self.records.get(idx).is_some_and(|r| r.url == url)
      {
        if let Some(existing) = self.records.get_mut(idx) {
          existing.last_visited = visited_at;
          existing.visit_count = existing.visit_count.saturating_add(visit_count);
          if title.is_some() {
            existing.title = title;
          }
          self.bump_revision();
          return;
        }
      }

      // Slow-ish path: shift the entry to the end to preserve recency ordering.
      if idx < self.records.len() && self.records.get(idx).is_some_and(|r| r.url == url) {
        if let Some(mut existing) = self.records.remove(idx) {
          existing.last_visited = visited_at;
          existing.visit_count = existing.visit_count.saturating_add(visit_count);
          if title.is_some() {
            existing.title = title;
          }
          self.records.push_back(existing);
          self.reindex_from(idx);
          self.bump_revision();
          return;
        }
      }

      // Defensive fallback: if the index got out of sync (e.g. someone mutated `records`
      // directly), rebuild and retry once.
      self.rebuild_url_index();
      if let Some(idx) = self.lookup_index(url.as_str()) {
        if idx == self.records.len().saturating_sub(1)
          && self.records.get(idx).is_some_and(|r| r.url == url)
        {
          if let Some(existing) = self.records.get_mut(idx) {
            existing.last_visited = visited_at;
            existing.visit_count = existing.visit_count.saturating_add(visit_count);
            if title.is_some() {
              existing.title = title;
            }
            self.bump_revision();
            return;
          }
        }

        if idx < self.records.len() && self.records.get(idx).is_some_and(|r| r.url == url) {
          if let Some(mut existing) = self.records.remove(idx) {
            existing.last_visited = visited_at;
            existing.visit_count = existing.visit_count.saturating_add(visit_count);
            if title.is_some() {
              existing.title = title;
            }
            self.records.push_back(existing);
            self.reindex_from(idx);
            self.bump_revision();
            return;
          }
        }
      }
    }

    self.records.push_back(VisitedUrlRecord {
      url: url.clone(),
      title,
      last_visited: visited_at,
      visit_count,
    });
    self
      .url_index
      .insert(url, self.url_index_base.wrapping_add(self.records.len() - 1));

    while self.records.len() > self.capacity {
      self.pop_front();
    }

    self.bump_revision();
  }

  /// Populate the visited URL store from a persisted [`GlobalHistoryStore`].
  ///
  /// This is intended for startup seeding so omnibox history suggestions survive browser restarts.
  ///
  /// Behaviour:
  /// - Orders history entries by `visited_at_ms` (oldest → newest), falling back to file order for
  ///   missing timestamps (`visited_at_ms == 0`).
  /// - Deduplicates by URL using the same logic as [`VisitedUrlStore::record_visit`].
  /// - Filters `about:` URLs so internal pages like `about:newtab` do not pollute omnibox history.
  /// - Enforces the store's configured capacity.
  pub fn seed_from_global_history(&mut self, history: &GlobalHistoryStore) {
    if self.capacity == 0 || history.entries.is_empty() {
      return;
    }

    use std::time::{Duration, UNIX_EPOCH};

    let mut ordered: Vec<(u64, usize)> = history
      .entries
      .iter()
      .enumerate()
      .map(|(idx, entry)| (entry.visited_at_ms, idx))
      .collect();

    // Sort by timestamp (ascending); include the original index to make ordering deterministic and
    // preserve file order for equal/missing timestamps.
    ordered.sort();

    for (_ts, idx) in ordered {
      let entry = &history.entries[idx];
      let url = entry.url.trim();
      if url.is_empty() {
        continue;
      }
      if about_pages::is_about_url(url) {
        continue;
      }

      let visited_at_ms = if entry.visited_at_ms == 0 {
        idx as u64
      } else {
        entry.visited_at_ms
      };
      let visited_at = UNIX_EPOCH
        .checked_add(Duration::from_millis(visited_at_ms))
        .unwrap_or(UNIX_EPOCH);

      let visit_count = entry.visit_count.max(1);
      let visit_count = u32::try_from(visit_count).unwrap_or(u32::MAX);

      self.record_visit_at_with_count(
        url.to_string(),
        entry.title.clone(),
        visited_at,
        visit_count,
      );
    }
  }

  /// Backwards-compatible alias for [`VisitedUrlStore::seed_from_global_history`].
  pub fn extend_from_global_history(&mut self, history: &GlobalHistoryStore) {
    self.seed_from_global_history(history);
  }

  /// Search visited URLs for omnibox suggestions.
  ///
  /// The returned records are ordered by recency (most recent first).
  pub fn search<'a>(&'a self, query: &str, limit: usize) -> Vec<&'a VisitedUrlRecord> {
    if limit == 0 {
      return Vec::new();
    }

    // Lowercase once so we can use the fast ASCII-only matcher (non-ASCII bytes compare exactly).
    // Most queries are already lowercase; avoid allocating unless needed.
    let query_lower: Cow<'_, str> = if query.as_bytes().iter().any(|b| b.is_ascii_uppercase()) {
      Cow::Owned(query.to_ascii_lowercase())
    } else {
      Cow::Borrowed(query)
    };
    let tokens: SmallVec<[&str; 4]> = query_lower.split_whitespace().collect();

    match tokens.as_slice() {
      [] => self.iter_recent().take(limit).collect(),
      [token] => {
        let mut out = Vec::with_capacity(limit.min(self.len()));
        for record in self.iter_recent() {
          let in_url = contains_ascii_case_insensitive(&record.url, token);
          let in_title = record
            .title
            .as_deref()
            .is_some_and(|t| contains_ascii_case_insensitive(t, token));
          if !in_url && !in_title {
            continue;
          }

          out.push(record);
          if out.len() >= limit {
            break;
          }
        }
        out
      }
      tokens => {
        let mut out = Vec::with_capacity(limit.min(self.len()));
        'records: for record in self.iter_recent() {
          for token in tokens {
            let in_url = contains_ascii_case_insensitive(&record.url, token);
            let in_title = record
              .title
              .as_deref()
              .is_some_and(|t| contains_ascii_case_insensitive(t, token));
            if !in_url && !in_title {
              continue 'records;
            }
          }

          out.push(record);
          if out.len() >= limit {
            break;
          }
        }

        out
      }
    }
  }

  pub fn get(&self, url: &str) -> Option<&VisitedUrlRecord> {
    let idx = self.lookup_index(url)?;
    self.records.get(idx).filter(|r| r.url == url)
  }

  fn lookup_index(&self, url: &str) -> Option<usize> {
    let abs = *self.url_index.get(url)?;
    Some(abs.wrapping_sub(self.url_index_base))
  }

  fn pop_front(&mut self) -> Option<VisitedUrlRecord> {
    let removed = self.records.pop_front()?;
    self.url_index.remove(removed.url.as_str());
    self.url_index_base = self.url_index_base.wrapping_add(1);
    Some(removed)
  }

  fn rebuild_url_index(&mut self) {
    self.url_index.clear();
    for (idx, record) in self.records.iter().enumerate() {
      self
        .url_index
        .insert(record.url.clone(), self.url_index_base.wrapping_add(idx));
    }
  }

  fn reindex_from(&mut self, start: usize) {
    for (idx, record) in self.records.iter().enumerate().skip(start) {
      let abs = self.url_index_base.wrapping_add(idx);
      if let Some(existing) = self.url_index.get_mut(record.url.as_str()) {
        *existing = abs;
      } else {
        self.url_index.insert(record.url.clone(), abs);
      }
    }
  }
}

/// Cached search helper for [`VisitedUrlStore`].
///
/// This is intended for UI callers (like omnibox) that re-run the same search query every frame.
/// When both the query string and the visited store revision are unchanged, the per-call work is
/// O(1) (returning a slice of cached match indices).
#[derive(Debug, Default, Clone)]
pub struct VisitedUrlSearcher {
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

impl VisitedUrlSearcher {
  pub fn new() -> Self {
    Self::default()
  }

  /// Search `store` for `query`, returning indices into the store ordered by recency (most recent
  /// first).
  ///
  /// - When `query` and `store.revision()` are unchanged, this returns cached indices without
  ///   re-tokenizing or rescanning the store.
  /// - `limit == 0` always returns an empty slice.
  pub fn search_indices<'a>(
    &'a mut self,
    store: &VisitedUrlStore,
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
      // Only re-tokenize when the query itself changes; if the store mutates while the query stays
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

      let (indices, complete) = compute_search_match_indices(store, &self.last_tokens_lower, limit);
      self.cached_match_indices = indices;
      self.cached_complete = complete;
      self.cached_limit = limit;
    }

    let n = limit.min(self.cached_match_indices.len());
    &self.cached_match_indices[..n]
  }
}

fn compute_search_match_indices(
  store: &VisitedUrlStore,
  tokens: &[String],
  limit: usize,
) -> (Vec<usize>, bool) {
  if limit == 0 {
    return (Vec::new(), true);
  }

  if tokens.is_empty() {
    let indices: Vec<usize> = store
      .records
      .iter()
      .enumerate()
      .rev()
      .take(limit)
      .map(|(idx, _)| idx)
      .collect();
    let complete = store.records.len() <= limit;
    return (indices, complete);
  }

  let mut out = Vec::with_capacity(limit.min(store.records.len()));
  'records: for (idx, record) in store.records.iter().enumerate().rev() {
    for token in tokens {
      let in_url = contains_ascii_case_insensitive(&record.url, token);
      let in_title = record
        .title
        .as_deref()
        .is_some_and(|t| contains_ascii_case_insensitive(t, token));
      if !in_url && !in_title {
        continue 'records;
      }
    }

    out.push(idx);
    if out.len() >= limit {
      break;
    }
  }

  let complete = out.len() < limit;
  (out, complete)
}

impl Default for VisitedUrlStore {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  fn assert_url_index_consistent(store: &VisitedUrlStore) {
    assert_eq!(
      store.url_index.len(),
      store.records.len(),
      "url index should track every record"
    );
    for (idx, record) in store.records.iter().enumerate() {
      let expected_abs = store.url_index_base.wrapping_add(idx);
      assert_eq!(
        store.url_index.get(record.url.as_str()),
        Some(&expected_abs),
        "url index should map {} to {} (abs)",
        record.url,
        expected_abs
      );
      assert_eq!(
        store.lookup_index(record.url.as_str()),
        Some(idx),
        "lookup_index should map {} to {} (logical)",
        record.url,
        idx
      );
    }
  }

  #[test]
  fn dedup_refreshes_last_visited_and_preserves_title_when_none() {
    let mut store = VisitedUrlStore::with_capacity(10);

    let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
    let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(2);
    let t3 = SystemTime::UNIX_EPOCH + Duration::from_secs(3);

    store.record_visit_at("https://a.example/".to_string(), Some("A".to_string()), t1);
    store.record_visit_at("https://b.example/".to_string(), Some("B".to_string()), t2);

    // Visiting an existing URL should dedup it and refresh the timestamp, without clobbering the
    // title when the new title is `None`.
    store.record_visit_at("https://a.example/".to_string(), None, t3);

    assert_eq!(store.len(), 2);
    let mut it = store.iter_recent();
    let a = it.next().unwrap();
    assert_eq!(a.url, "https://a.example/");
    assert_eq!(a.title.as_deref(), Some("A"));
    assert_eq!(a.last_visited, t3);
    assert_eq!(
      a.visit_count, 2,
      "expected visit_count to increment on revisit"
    );

    let b = it.next().unwrap();
    assert_eq!(b.url, "https://b.example/");
    assert_eq!(b.last_visited, t2);
    assert_eq!(b.visit_count, 1);
    assert_url_index_consistent(&store);
  }

  #[test]
  fn capacity_is_enforced_by_dropping_oldest_entries_first() {
    let mut store = VisitedUrlStore::with_capacity(2);

    let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
    let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(2);
    let t3 = SystemTime::UNIX_EPOCH + Duration::from_secs(3);

    store.record_visit_at("a".to_string(), None, t1);
    store.record_visit_at("b".to_string(), None, t2);
    assert_eq!(store.len(), 2);

    // Add a third unique URL; "a" is the oldest and should be dropped.
    store.record_visit_at("c".to_string(), None, t3);
    assert_eq!(store.len(), 2);

    let urls: Vec<&str> = store.iter_recent().map(|r| r.url.as_str()).collect();
    assert_eq!(urls, vec!["c", "b"]);
    assert_url_index_consistent(&store);
  }

  #[test]
  fn seed_from_global_history_dedups_orders_by_timestamp_and_preserves_titles() {
    let mut history = GlobalHistoryStore::default();
    history.entries = vec![
      super::super::GlobalHistoryEntry {
        url: "https://a.example/".to_string(),
        title: Some("A".to_string()),
        visited_at_ms: 2_000,
        visit_count: 2,
      },
      // More recent, but missing title; should not clobber previous title for the same URL.
      super::super::GlobalHistoryEntry {
        url: "https://a.example/".to_string(),
        title: None,
        visited_at_ms: 6_000,
        visit_count: 3,
      },
      // Out-of-order timestamp compared to file order: this should still be newer than `c`.
      super::super::GlobalHistoryEntry {
        url: "https://b.example/".to_string(),
        title: Some("B".to_string()),
        visited_at_ms: 5_000,
        visit_count: 1,
      },
      super::super::GlobalHistoryEntry {
        url: "https://c.example/".to_string(),
        title: Some("C".to_string()),
        visited_at_ms: 3_000,
        visit_count: 1,
      },
    ];

    let mut store = VisitedUrlStore::new();
    store.seed_from_global_history(&history);

    assert_eq!(store.len(), 3);

    let urls: Vec<&str> = store.iter_recent().map(|r| r.url.as_str()).collect();
    assert_eq!(
      urls,
      vec![
        "https://a.example/",
        "https://b.example/",
        "https://c.example/"
      ]
    );

    let a = store.iter_recent().next().unwrap();
    assert_eq!(a.title.as_deref(), Some("A"));
    assert_eq!(
      a.last_visited,
      std::time::UNIX_EPOCH + Duration::from_millis(6_000)
    );
    assert_eq!(
      a.visit_count, 5,
      "expected visit_count to be accumulated across duplicate history entries"
    );
    assert_url_index_consistent(&store);
  }

  #[test]
  fn seed_from_global_history_filters_about_urls() {
    let mut history = GlobalHistoryStore::default();
    history.entries = vec![
      super::super::GlobalHistoryEntry {
        url: "about:newtab".to_string(),
        title: Some("New Tab".to_string()),
        visited_at_ms: 10_000,
        visit_count: 1,
      },
      super::super::GlobalHistoryEntry {
        url: "https://example.com/".to_string(),
        title: Some("Example".to_string()),
        visited_at_ms: 11_000,
        visit_count: 1,
      },
    ];

    let mut store = VisitedUrlStore::new();
    store.seed_from_global_history(&history);

    assert_eq!(store.len(), 1);
    let record = store.iter_recent().next().unwrap();
    assert_eq!(record.url, "https://example.com/");
    assert_url_index_consistent(&store);
  }

  #[test]
  fn seed_from_global_history_populates_search_results() {
    let mut history = GlobalHistoryStore::default();
    history.entries = vec![
      super::super::GlobalHistoryEntry {
        url: "https://example.com/".to_string(),
        title: Some("Example Domain".to_string()),
        visited_at_ms: 1_000,
        visit_count: 1,
      },
      super::super::GlobalHistoryEntry {
        url: "https://www.rust-lang.org/".to_string(),
        title: Some("Rust".to_string()),
        visited_at_ms: 2_000,
        visit_count: 1,
      },
      super::super::GlobalHistoryEntry {
        url: "https://example.org/other".to_string(),
        title: None,
        visited_at_ms: 3_000,
        visit_count: 1,
      },
    ];

    let mut store = VisitedUrlStore::new();
    store.seed_from_global_history(&history);

    let urls: Vec<&str> = store.iter_recent().map(|r| r.url.as_str()).collect();
    assert_eq!(
      urls,
      vec![
        "https://example.org/other",
        "https://www.rust-lang.org/",
        "https://example.com/"
      ]
    );

    let hits = store.search("example", 10);
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].url, "https://example.org/other");
    assert_eq!(hits[1].url, "https://example.com/");

    let rust = store.search("rust", 10);
    assert_eq!(rust.len(), 1);
    assert_eq!(rust[0].url, "https://www.rust-lang.org/");

    let example_com = store
      .iter_recent()
      .find(|r| r.url == "https://example.com/")
      .unwrap();
    assert_eq!(
      example_com.last_visited,
      std::time::UNIX_EPOCH + Duration::from_millis(1_000)
    );
    assert_url_index_consistent(&store);
  }

  #[test]
  fn seed_from_global_history_skips_empty_urls() {
    let mut history = GlobalHistoryStore::default();
    history.entries = vec![
      super::super::GlobalHistoryEntry {
        url: "   ".to_string(),
        title: Some("Whitespace".to_string()),
        visited_at_ms: 1_000,
        visit_count: 1,
      },
      super::super::GlobalHistoryEntry {
        url: "".to_string(),
        title: Some("Empty".to_string()),
        visited_at_ms: 2_000,
        visit_count: 1,
      },
      super::super::GlobalHistoryEntry {
        url: "https://example.com/".to_string(),
        title: None,
        visited_at_ms: 3_000,
        visit_count: 1,
      },
    ];

    let mut store = VisitedUrlStore::new();
    store.seed_from_global_history(&history);

    assert_eq!(store.len(), 1);
    assert_eq!(
      store.iter_recent().next().unwrap().url,
      "https://example.com/"
    );
    assert_url_index_consistent(&store);
  }

  #[test]
  fn seed_from_global_history_respects_capacity() {
    let mut history = GlobalHistoryStore::default();
    history.entries = vec![
      super::super::GlobalHistoryEntry {
        url: "https://a.example/".to_string(),
        title: None,
        visited_at_ms: 1_000,
        visit_count: 1,
      },
      super::super::GlobalHistoryEntry {
        url: "https://b.example/".to_string(),
        title: None,
        visited_at_ms: 2_000,
        visit_count: 1,
      },
      super::super::GlobalHistoryEntry {
        url: "https://c.example/".to_string(),
        title: None,
        visited_at_ms: 3_000,
        visit_count: 1,
      },
    ];

    let mut store = VisitedUrlStore::with_capacity(2);
    store.seed_from_global_history(&history);

    let urls: Vec<&str> = store.iter_recent().map(|r| r.url.as_str()).collect();
    assert_eq!(urls, vec!["https://c.example/", "https://b.example/"]);
    assert_url_index_consistent(&store);
  }

  #[test]
  fn url_index_tracks_record_updates_and_recency_ordering() {
    let mut store = VisitedUrlStore::with_capacity(10);
    let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
    let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(2);
    let t3 = SystemTime::UNIX_EPOCH + Duration::from_secs(3);
    let t4 = SystemTime::UNIX_EPOCH + Duration::from_secs(4);
    let t5 = SystemTime::UNIX_EPOCH + Duration::from_secs(5);

    store.record_visit_at("https://a.example/".to_string(), Some("A".to_string()), t1);
    store.record_visit_at("https://b.example/".to_string(), Some("B".to_string()), t2);
    store.record_visit_at("https://c.example/".to_string(), Some("C".to_string()), t3);
    assert_eq!(
      store.records.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
      vec!["https://a.example/", "https://b.example/", "https://c.example/"]
    );
    assert_url_index_consistent(&store);

    // Updating a middle entry should move it to the back and rewrite shifted indices.
    store.record_visit_at("https://b.example/".to_string(), None, t4);
    assert_eq!(
      store.records.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
      vec!["https://a.example/", "https://c.example/", "https://b.example/"]
    );
    assert_eq!(store.get("https://b.example/").unwrap().last_visited, t4);
    assert_url_index_consistent(&store);

    // Updating the most-recent entry should not reorder.
    store.record_visit_at("https://b.example/".to_string(), None, t5);
    assert_eq!(
      store.records.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
      vec!["https://a.example/", "https://c.example/", "https://b.example/"]
    );
    assert_eq!(store.get("https://b.example/").unwrap().last_visited, t5);
    assert_url_index_consistent(&store);
  }

  #[test]
  fn url_index_is_correct_after_capacity_eviction() {
    let mut store = VisitedUrlStore::with_capacity(3);
    let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
    let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(2);
    let t3 = SystemTime::UNIX_EPOCH + Duration::from_secs(3);
    let t4 = SystemTime::UNIX_EPOCH + Duration::from_secs(4);
    let t5 = SystemTime::UNIX_EPOCH + Duration::from_secs(5);
    let t6 = SystemTime::UNIX_EPOCH + Duration::from_secs(6);

    store.record_visit_at("a".to_string(), None, t1);
    store.record_visit_at("b".to_string(), None, t2);
    store.record_visit_at("c".to_string(), None, t3);
    assert_url_index_consistent(&store);

    // Add a new entry at capacity; oldest should be evicted.
    store.record_visit_at("d".to_string(), None, t4);
    assert_eq!(
      store.records.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
      vec!["b", "c", "d"]
    );
    assert!(store.get("a").is_none());
    assert!(store.get("b").is_some());
    assert!(store.get("c").is_some());
    assert!(store.get("d").is_some());
    assert_url_index_consistent(&store);

    // Moving an entry after eviction should still preserve index consistency.
    store.record_visit_at("b".to_string(), None, t5);
    assert_eq!(
      store.records.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
      vec!["c", "d", "b"]
    );
    assert_url_index_consistent(&store);

    // Another eviction should drop the new oldest.
    store.record_visit_at("e".to_string(), None, t6);
    assert_eq!(
      store.records.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
      vec!["d", "b", "e"]
    );
    assert!(store.get("c").is_none());
    assert_url_index_consistent(&store);
  }

  #[test]
  fn cached_search_matches_uncached_for_repeated_calls() {
    let mut store = VisitedUrlStore::with_capacity(10);
    let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
    let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(2);
    let t3 = SystemTime::UNIX_EPOCH + Duration::from_secs(3);

    store.record_visit_at(
      "https://example.com/".to_string(),
      Some("Example Domain".to_string()),
      t1,
    );
    store.record_visit_at("https://www.rust-lang.org/".to_string(), Some("Rust".to_string()), t2);
    store.record_visit_at("https://example.org/other".to_string(), None, t3);

    let uncached: Vec<&str> = store
      .search("example", 10)
      .into_iter()
      .map(|r| r.url.as_str())
      .collect();

    let mut searcher = VisitedUrlSearcher::default();
    let cached1 = searcher.search_indices(&store, "example", 10).to_vec();
    let cached_urls1: Vec<&str> = cached1
      .iter()
      .filter_map(|idx| store.records.get(*idx))
      .map(|r| r.url.as_str())
      .collect();
    assert_eq!(cached_urls1, uncached);

    let cached2 = searcher.search_indices(&store, "example", 10).to_vec();
    assert_eq!(cached2, cached1);
  }

  #[test]
  fn should_record_visit_in_history_filters_noisy_about_pages_and_allows_useful_ones() {
    for url in [
      "about:newtab",
      "about:blank",
      "about:error",
      "about:test-scroll",
      "about:test-heavy",
      "about:test-layout-stress",
      "about:test-form",
      // Query/fragment variants should behave the same.
      "about:newtab#foo",
      "about:test-heavy?q=1",
    ] {
      assert!(
        !should_record_visit_in_history(url),
        "expected {url} not to be recorded"
      );
    }

    for url in [
      "about:help",
      "about:version",
      "about:gpu",
      "about:help#shortcuts",
      "about:version?q=1",
      // Case-insensitive.
      "ABOUT:HELP",
    ] {
      assert!(
        should_record_visit_in_history(url),
        "expected {url} to be recorded"
      );
    }

    // Non-about URLs should be recorded normally.
    assert!(should_record_visit_in_history("https://example.com/"));
    assert!(should_record_visit_in_history("file:///tmp/a.html"));
  }

  #[test]
  #[ignore]
  fn record_benchmark_does_not_linear_scan_urls() {
    use std::time::Instant;

    let mut store = VisitedUrlStore::with_capacity(10_000);
    let t0 = SystemTime::UNIX_EPOCH;
    for i in 0..10_000_u32 {
      store.record_visit_at(format!("https://example.test/{i}"), None, t0);
    }

    let start = Instant::now();
    // Update a middle entry repeatedly; with a URL index this should avoid O(n) URL comparisons.
    for i in 0..1000_u32 {
      store.record_visit_at(
        "https://example.test/5000".to_string(),
        None,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1 + i as u64),
      );
    }
    let dt = start.elapsed();
    eprintln!("record 1000 updates into 10k-entry store: {dt:?}");
  }
}
