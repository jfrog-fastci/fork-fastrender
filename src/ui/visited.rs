use std::collections::VecDeque;
use std::time::SystemTime;

use super::about_pages;
use super::GlobalHistoryStore;

/// Default maximum number of unique visited URLs stored in-memory.
///
/// This is intentionally bounded so the UI thread can offer omnibox suggestions without
/// unbounded memory growth.
pub const DEFAULT_VISITED_URL_CAPACITY: usize = 5000;

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
}

impl VisitedUrlStore {
  pub fn new() -> Self {
    Self::with_capacity(DEFAULT_VISITED_URL_CAPACITY)
  }

  pub fn with_capacity(capacity: usize) -> Self {
    Self {
      records: VecDeque::new(),
      capacity,
    }
  }

  pub fn len(&self) -> usize {
    self.records.len()
  }

  pub fn is_empty(&self) -> bool {
    self.records.is_empty()
  }

  pub fn clear(&mut self) {
    self.records.clear();
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

    let visit_count = visit_count.max(1);

    if let Some(idx) = self.records.iter().position(|r| r.url == url) {
      if let Some(mut existing) = self.records.remove(idx) {
        existing.last_visited = visited_at;
        existing.visit_count = existing.visit_count.saturating_add(visit_count);
        if title.is_some() {
          existing.title = title;
        }
        self.records.push_back(existing);
      }
      return;
    }

    self.records.push_back(VisitedUrlRecord {
      url,
      title,
      last_visited: visited_at,
      visit_count,
    });

    while self.records.len() > self.capacity {
      self.records.pop_front();
    }
  }

  /// Populate the visited URL store from a persisted [`GlobalHistoryStore`].
  ///
  /// This is intended for startup seeding so omnibox history suggestions survive browser restarts.
  ///
  /// Behaviour:
  /// - Orders history entries by `visited_at_ms` (oldest → newest), falling back to file order for
  ///   missing timestamps.
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
      .map(|(idx, entry)| (entry.visited_at_ms.unwrap_or(0), idx))
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

      let visited_at_ms = entry.visited_at_ms.unwrap_or(0);
      let visited_at = UNIX_EPOCH
        .checked_add(Duration::from_millis(visited_at_ms))
        .unwrap_or(UNIX_EPOCH);

      let visit_count = entry.visit_count.max(1);
      let visit_count = u32::try_from(visit_count).unwrap_or(u32::MAX);

      self.record_visit_at_with_count(url.to_string(), entry.title.clone(), visited_at, visit_count);
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

    let tokens: Vec<&str> = query.split_whitespace().filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() {
      return self.iter_recent().take(limit).collect();
    }

    let mut out = Vec::with_capacity(limit.min(self.len()));
    'records: for record in self.iter_recent() {
      for token in &tokens {
        let in_url = contains_case_insensitive(&record.url, token);
        let in_title = record
          .title
          .as_deref()
          .is_some_and(|t| contains_case_insensitive(t, token));
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

  pub fn get(&self, url: &str) -> Option<&VisitedUrlRecord> {
    self.records.iter().find(|r| r.url == url)
  }
}

impl Default for VisitedUrlStore {
  fn default() -> Self {
    Self::new()
  }
}

fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
  // For omnibox usage we want lightweight, allocation-free matching. We use ASCII-only
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
  use std::time::Duration;

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
    assert_eq!(a.visit_count, 2, "expected visit_count to increment on revisit");

    let b = it.next().unwrap();
    assert_eq!(b.url, "https://b.example/");
    assert_eq!(b.last_visited, t2);
    assert_eq!(b.visit_count, 1);
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
  }

  #[test]
  fn seed_from_global_history_dedups_orders_by_timestamp_and_preserves_titles() {
    let history = GlobalHistoryStore {
      entries: vec![
        super::super::GlobalHistoryEntry {
          url: "https://a.example/".to_string(),
          title: Some("A".to_string()),
          visited_at_ms: Some(2_000),
          visit_count: 2,
        },
        // More recent, but missing title; should not clobber previous title for the same URL.
        super::super::GlobalHistoryEntry {
          url: "https://a.example/".to_string(),
          title: None,
          visited_at_ms: Some(6_000),
          visit_count: 3,
        },
        // Out-of-order timestamp compared to file order: this should still be newer than `c`.
        super::super::GlobalHistoryEntry {
          url: "https://b.example/".to_string(),
          title: Some("B".to_string()),
          visited_at_ms: Some(5_000),
          visit_count: 1,
        },
        super::super::GlobalHistoryEntry {
          url: "https://c.example/".to_string(),
          title: Some("C".to_string()),
          visited_at_ms: Some(3_000),
          visit_count: 1,
        },
      ],
    };

    let mut store = VisitedUrlStore::new();
    store.seed_from_global_history(&history);

    assert_eq!(store.len(), 3);

    let urls: Vec<&str> = store.iter_recent().map(|r| r.url.as_str()).collect();
    assert_eq!(
      urls,
      vec!["https://a.example/", "https://b.example/", "https://c.example/"]
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
  }

  #[test]
  fn seed_from_global_history_filters_about_urls() {
    let history = GlobalHistoryStore {
      entries: vec![
        super::super::GlobalHistoryEntry {
          url: "about:newtab".to_string(),
          title: Some("New Tab".to_string()),
          visited_at_ms: Some(10_000),
          visit_count: 1,
        },
        super::super::GlobalHistoryEntry {
          url: "https://example.com/".to_string(),
          title: Some("Example".to_string()),
          visited_at_ms: Some(11_000),
          visit_count: 1,
        },
      ],
    };

    let mut store = VisitedUrlStore::new();
    store.seed_from_global_history(&history);

    assert_eq!(store.len(), 1);
    let record = store.iter_recent().next().unwrap();
    assert_eq!(record.url, "https://example.com/");
  }

  #[test]
  fn seed_from_global_history_populates_search_results() {
    let history = GlobalHistoryStore {
      entries: vec![
        super::super::GlobalHistoryEntry {
          url: "https://example.com/".to_string(),
          title: Some("Example Domain".to_string()),
          visited_at_ms: Some(1_000),
          visit_count: 1,
        },
        super::super::GlobalHistoryEntry {
          url: "https://www.rust-lang.org/".to_string(),
          title: Some("Rust".to_string()),
          visited_at_ms: Some(2_000),
          visit_count: 1,
        },
        super::super::GlobalHistoryEntry {
          url: "https://example.org/other".to_string(),
          title: None,
          visited_at_ms: Some(3_000),
          visit_count: 1,
        },
      ],
    };

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
  }

  #[test]
  fn seed_from_global_history_skips_empty_urls() {
    let history = GlobalHistoryStore {
      entries: vec![
        super::super::GlobalHistoryEntry {
          url: "   ".to_string(),
          title: Some("Whitespace".to_string()),
          visited_at_ms: Some(1_000),
          visit_count: 1,
        },
        super::super::GlobalHistoryEntry {
          url: "".to_string(),
          title: Some("Empty".to_string()),
          visited_at_ms: Some(2_000),
          visit_count: 1,
        },
        super::super::GlobalHistoryEntry {
          url: "https://example.com/".to_string(),
          title: None,
          visited_at_ms: Some(3_000),
          visit_count: 1,
        },
      ],
    };

    let mut store = VisitedUrlStore::new();
    store.seed_from_global_history(&history);

    assert_eq!(store.len(), 1);
    assert_eq!(store.iter_recent().next().unwrap().url, "https://example.com/");
  }

  #[test]
  fn seed_from_global_history_respects_capacity() {
    let history = GlobalHistoryStore {
      entries: vec![
        super::super::GlobalHistoryEntry {
          url: "https://a.example/".to_string(),
          title: None,
          visited_at_ms: Some(1_000),
          visit_count: 1,
        },
        super::super::GlobalHistoryEntry {
          url: "https://b.example/".to_string(),
          title: None,
          visited_at_ms: Some(2_000),
          visit_count: 1,
        },
        super::super::GlobalHistoryEntry {
          url: "https://c.example/".to_string(),
          title: None,
          visited_at_ms: Some(3_000),
          visit_count: 1,
        },
      ],
    };

    let mut store = VisitedUrlStore::with_capacity(2);
    store.seed_from_global_history(&history);

    let urls: Vec<&str> = store.iter_recent().map(|r| r.url.as_str()).collect();
    assert_eq!(urls, vec!["https://c.example/", "https://b.example/"]);
  }
}
