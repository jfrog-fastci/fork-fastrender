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

use serde::{de, Deserialize, Deserializer, Serialize};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

use super::string_match::contains_ascii_case_insensitive;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllowedScheme {
  Http,
  Https,
  File,
}

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

/// Incremental "visit committed" delta for synchronizing history across windows.
///
/// This delta is intentionally small and self-contained so the windowed browser can merge history
/// updates from multiple windows without cloning/reseeding the full [`GlobalHistoryStore`] on every
/// navigation.
///
/// Notes:
/// - This currently models *only* additive visit recording (`GlobalHistoryStore::record*`).
/// - Destructive history actions (clear/delete) may fall back to full-store sync for correctness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryVisitDelta {
  /// Normalized URL key that is actually stored in [`GlobalHistoryStore::entries`].
  pub url: String,
  /// Optional title update for the visit. `None` means "do not clobber an existing title".
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub title: Option<String>,
  /// Unix epoch milliseconds timestamp recorded for this visit.
  pub visited_at_ms: u64,
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
  /// Fast URL → entry index lookup for [`GlobalHistoryStore::get`] / [`GlobalHistoryStore::record`].
  ///
  /// This is derived state and is rebuilt on load (see `Deserialize` impl).
  #[serde(skip)]
  url_index: HashMap<String, usize>,
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
          url_index: HashMap::new(),
        }
      }
      GlobalHistoryStoreFile::Legacy(legacy) => GlobalHistoryStore {
        schema_version: GLOBAL_HISTORY_SCHEMA_VERSION,
        entries: legacy.entries,
        capacity: DEFAULT_GLOBAL_HISTORY_CAPACITY,
        revision: 0,
        url_index: HashMap::new(),
      },
      GlobalHistoryStoreFile::LegacyVec(entries) => GlobalHistoryStore {
        schema_version: GLOBAL_HISTORY_SCHEMA_VERSION,
        entries,
        capacity: DEFAULT_GLOBAL_HISTORY_CAPACITY,
        revision: 0,
        url_index: HashMap::new(),
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
      url_index: HashMap::new(),
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
    self.record_at_ms(url, title, now_unix_ms()).is_some()
  }

  /// Record a committed visit to `url` and return a delta describing the mutation.
  ///
  /// This is intended for UI layers that synchronize history across windows without cloning the
  /// entire history store on every navigation.
  pub fn record_with_delta(
    &mut self,
    url: String,
    title: Option<String>,
  ) -> Option<HistoryVisitDelta> {
    self.record_at_ms(url, title, now_unix_ms())
  }

  fn record_at_ms(
    &mut self,
    url: String,
    title: Option<String>,
    visited_at_ms: u64,
  ) -> Option<HistoryVisitDelta> {
    if self.capacity == 0 {
      return None;
    }

    let Some(normalized) = normalize_url_for_history(&url) else {
      return None;
    };
    let title = normalize_title(title);

    let changed =
      self.record_normalized_at_ms(normalized.as_str(), title.as_deref(), visited_at_ms);
    changed.then_some(HistoryVisitDelta {
      url: normalized,
      title,
      visited_at_ms,
    })
  }

  /// Apply an incremental visit delta produced by [`GlobalHistoryStore::record_with_delta`].
  ///
  /// This is O(delta) and avoids re-parsing the URL because the delta carries the normalized URL
  /// that acts as the store key.
  pub fn apply_visit_delta(&mut self, delta: &HistoryVisitDelta) -> bool {
    self.record_normalized_at_ms(
      delta.url.as_str(),
      delta.title.as_deref(),
      delta.visited_at_ms,
    )
  }

  pub fn apply_visit_deltas(&mut self, deltas: &[HistoryVisitDelta]) -> bool {
    let mut changed = false;
    for delta in deltas {
      changed |= self.apply_visit_delta(delta);
    }
    changed
  }

  fn record_normalized_at_ms(
    &mut self,
    normalized: &str,
    title: Option<&str>,
    visited_at_ms: u64,
  ) -> bool {
    if self.capacity == 0 {
      return false;
    }

    let normalized = normalized.trim();
    if normalized.is_empty() {
      return false;
    }

    // Defensive: deltas are expected to contain already-normalized non-`about:` URLs, but avoid
    // accidentally recording internal pages if a caller violates the contract.
    if normalized
      .as_bytes()
      .get(..6)
      .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"about:"))
    {
      return false;
    }

    if let Some(&idx) = self.url_index.get(normalized) {
      // Fast path: the entry is already the most-recent; update in place without shifting.
      if idx == self.entries.len().saturating_sub(1)
        && self.entries.get(idx).is_some_and(|e| e.url == normalized)
      {
        let existing = &mut self.entries[idx];
        existing.visit_count = existing.visit_count.max(1).saturating_add(1);
        existing.visited_at_ms = visited_at_ms;
        if let Some(title) = title {
          existing.title = Some(title.to_string());
        }
        self.bump_revision();
        return true;
      }

      // Slow-ish path: shift the entry to the end to preserve recency ordering.
      if idx < self.entries.len() && self.entries[idx].url == normalized {
        let mut existing = self.entries.remove(idx);
        existing.visit_count = existing.visit_count.max(1).saturating_add(1);
        existing.visited_at_ms = visited_at_ms;
        if let Some(title) = title {
          existing.title = Some(title.to_string());
        }
        self.entries.push(existing);

        // Indices for entries after `idx` changed due to `remove`; rebuild that suffix.
        self.reindex_from(idx);
        self.bump_revision();
        return true;
      }

      // Defensive fallback: if the index got out of sync (e.g. someone mutated `entries`
      // directly), rebuild and retry once.
      self.rebuild_url_index();
      if let Some(&idx) = self.url_index.get(normalized) {
        if idx == self.entries.len().saturating_sub(1)
          && self.entries.get(idx).is_some_and(|e| e.url == normalized)
        {
          let existing = &mut self.entries[idx];
          existing.visit_count = existing.visit_count.max(1).saturating_add(1);
          existing.visited_at_ms = visited_at_ms;
          if let Some(title) = title {
            existing.title = Some(title.to_string());
          }
          self.bump_revision();
          return true;
        }

        if idx < self.entries.len() && self.entries[idx].url == normalized {
          let mut existing = self.entries.remove(idx);
          existing.visit_count = existing.visit_count.max(1).saturating_add(1);
          existing.visited_at_ms = visited_at_ms;
          if let Some(title) = title {
            existing.title = Some(title.to_string());
          }
          self.entries.push(existing);
          self.reindex_from(idx);
          self.bump_revision();
          return true;
        }
      }
    }

    let url = normalized.to_string();
    self.entries.push(GlobalHistoryEntry {
      url: url.clone(),
      title: title.map(|t| t.to_string()),
      visited_at_ms,
      visit_count: 1,
    });
    self.url_index.insert(url, self.entries.len() - 1);
    self.enforce_capacity();
    self.bump_revision();
    true
  }

  /// Look up an entry by URL, applying the same normalization used for recording.
  pub fn get(&self, url: &str) -> Option<&GlobalHistoryEntry> {
    let key = normalize_url_for_history(url)?;
    let idx = *self.url_index.get(key.as_str())?;
    self.entries.get(idx).filter(|e| e.url == key)
  }

  /// Search history entries, ordered by recency (most recent first).
  pub fn search<'a>(&'a self, query: &str, limit: usize) -> Vec<(usize, &'a GlobalHistoryEntry)> {
    if limit == 0 {
      return Vec::new();
    }

    // Lowercase once so we can use the fast ASCII-only matcher (non-ASCII bytes compare exactly).
    // Most user queries are already lowercase; avoid allocating unless needed.
    let query_lower: Cow<'_, str> = if query.as_bytes().iter().any(|b| b.is_ascii_uppercase()) {
      Cow::Owned(query.to_ascii_lowercase())
    } else {
      Cow::Borrowed(query)
    };
    let tokens: SmallVec<[&str; 4]> = query_lower.split_whitespace().collect();

    match tokens.as_slice() {
      [] => self.iter_recent().take(limit).collect(),
      [token] => {
        let mut out = Vec::with_capacity(limit.min(self.entries.len()));
        for (idx, entry) in self.iter_recent() {
          let in_url = contains_ascii_case_insensitive(&entry.url, token);
          let in_title = entry
            .title
            .as_deref()
            .is_some_and(|t| contains_ascii_case_insensitive(t, token));
          if !in_url && !in_title {
            continue;
          }

          out.push((idx, entry));
          if out.len() >= limit {
            break;
          }
        }

        out
      }
      tokens => {
        let mut out = Vec::with_capacity(limit.min(self.entries.len()));
        'entries: for (idx, entry) in self.iter_recent() {
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

          out.push((idx, entry));
          if out.len() >= limit {
            break;
          }
        }

        out
      }
    }
  }

  /// Delete a single history entry by URL.
  ///
  /// Returns `true` when an entry was removed.
  pub fn delete_entry(&mut self, url: &str) -> bool {
    let Some(key) = normalize_url_for_history(url) else {
      return false;
    };
    let Some(&idx) = self.url_index.get(key.as_str()) else {
      return false;
    };
    if idx >= self.entries.len() || self.entries[idx].url != key {
      self.rebuild_url_index();
      let Some(&idx) = self.url_index.get(key.as_str()) else {
        return false;
      };
      if idx >= self.entries.len() || self.entries[idx].url != key {
        return false;
      }
    }
    let removed = self.entries.remove(idx);
    self.url_index.remove(removed.url.as_str());
    self.reindex_from(idx);
    self.bump_revision();
    true
  }

  /// Delete an entry by its index in [`GlobalHistoryStore::entries`].
  pub fn remove_at(&mut self, index: usize) -> Option<GlobalHistoryEntry> {
    if index < self.entries.len() {
      let removed = self.entries.remove(index);
      self.url_index.remove(removed.url.as_str());
      self.reindex_from(index);
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
    self.url_index.clear();
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
      self.url_index.clear();
      self.bump_revision();
      return;
    }
    let before = self.entries.len();
    self
      .entries
      .retain(|e| e.visited_at_ms == 0 || e.visited_at_ms < since_ms);
    if self.entries.len() != before {
      self.rebuild_url_index();
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
      self.rebuild_url_index();
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
      self.url_index.clear();
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
    self.rebuild_url_index();
  }

  fn normalize_after_load(&mut self) {
    // Ensure we always persist the current version on the next save.
    self.schema_version = GLOBAL_HISTORY_SCHEMA_VERSION;
    self.normalize_in_place();
  }

  fn enforce_capacity(&mut self) {
    if self.capacity == 0 {
      self.entries.clear();
      self.url_index.clear();
      return;
    }

    if self.entries.len() > self.capacity {
      let excess = self.entries.len() - self.capacity;
      // Remove evicted entries from the index before shifting.
      for e in &self.entries[..excess] {
        self.url_index.remove(e.url.as_str());
      }
      self.entries.drain(0..excess);
      // Remaining entries shifted down by `excess`.
      for idx in self.url_index.values_mut() {
        *idx = idx.saturating_sub(excess);
      }
    }
  }

  fn rebuild_url_index(&mut self) {
    let mut index = HashMap::with_capacity(self.entries.len());
    for (idx, entry) in self.entries.iter().enumerate() {
      index.insert(entry.url.clone(), idx);
    }
    self.url_index = index;
  }

  fn reindex_from(&mut self, start: usize) {
    for idx in start..self.entries.len() {
      let url = self.entries[idx].url.as_str();
      if let Some(slot) = self.url_index.get_mut(url) {
        *slot = idx;
      } else {
        // Best-effort recovery if the index got out of sync.
        self.url_index.insert(self.entries[idx].url.clone(), idx);
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

  // Split out the scheme up-front so we can:
  // - reject `about:` without allocating,
  // - reject unsupported schemes without paying the cost of `Url::parse` (the common case is
  //   already-normalized absolute http/https/file URLs emitted by the renderer worker).
  let scheme_end = trimmed.find(':')?;
  let scheme_raw = &trimmed[..scheme_end];
  let scheme = if scheme_raw.eq_ignore_ascii_case("http") {
    AllowedScheme::Http
  } else if scheme_raw.eq_ignore_ascii_case("https") {
    AllowedScheme::Https
  } else if scheme_raw.eq_ignore_ascii_case("file") {
    AllowedScheme::File
  } else if scheme_raw.eq_ignore_ascii_case("about") {
    return None;
  } else {
    return None;
  };

  // Fast path: most navigations come from the renderer as already-normalized absolute URLs.
  //
  // We conservatively skip `Url::parse` only when the URL:
  // - has no fragment (history never includes fragments),
  // - uses a lowercase allowed scheme (`http`/`https`/`file`),
  // - has an explicit path (`/` …) after the authority (Url adds `/` when missing),
  // - has a lowercase/canonical authority (no userinfo, no default ports, canonical IP literals),
  // - has canonical percent-encoding (valid, uppercase hex, and not encoding unreserved bytes),
  // - contains no dot-segments (`/./`, `/../`, `/.`, `/..`) that Url would remove.
  //
  // If any check fails, we fall back to `Url::parse` to preserve canonicalization semantics.
  if is_url_normalized_enough_for_history_fast_path(trimmed, scheme, scheme_end) {
    return Some(trimmed.to_string());
  }

  // Canonical path: `Url` handles normalization (lowercasing scheme/host, default port stripping,
  // path normalization, etc).
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
  Some(trimmed.split('#').next().unwrap_or(trimmed).to_string())
}

fn is_url_normalized_enough_for_history_fast_path(
  trimmed: &str,
  scheme: AllowedScheme,
  scheme_end: usize,
) -> bool {
  // Fast-path only applies to fragment-less URLs (history strips fragments).
  // Use a byte check to avoid UTF-8 scanning (URLs are expected to be ASCII at this stage anyway).
  if trimmed.as_bytes().contains(&b'#') {
    return false;
  }

  // Require lowercase scheme: `Url::parse` lowercases it, so mixed-case inputs need the slow path.
  match scheme {
    AllowedScheme::Http => {
      if &trimmed[..scheme_end] != "http" {
        return false;
      }
    }
    AllowedScheme::Https => {
      if &trimmed[..scheme_end] != "https" {
        return false;
      }
    }
    AllowedScheme::File => {
      if &trimmed[..scheme_end] != "file" {
        return false;
      }
    }
  }

  // Url serialization is ASCII-only; if non-ASCII appears here we need the full parser.
  if !trimmed.is_ascii() {
    return false;
  }

  let bytes = trimmed.as_bytes();
  // Require "://": these are absolute URLs with an authority component.
  if bytes.get(scheme_end..scheme_end + 3) != Some(&b"://"[..]) {
    return false;
  }

  // Validate characters + percent-encoding in a single pass.
  let mut i = 0;
  while i < bytes.len() {
    let b = bytes[i];
    // Reject ASCII whitespace/control bytes. `trim()` already handled leading/trailing space, but
    // internal whitespace would be percent-encoded by `Url`.
    if b <= 0x20 || b == 0x7F {
      return false;
    }
    // WHATWG URL treats backslash as a path separator for special schemes; `Url` normalizes it.
    if b == b'\\' {
      return false;
    }
    if b == b'%' {
      // Must be valid percent-encoding with uppercase hex digits.
      if i + 2 >= bytes.len() {
        return false;
      }
      let h1 = bytes[i + 1];
      let h2 = bytes[i + 2];
      let val = match (hex_val_upper(h1), hex_val_upper(h2)) {
        (Some(a), Some(b)) => (a << 4) | b,
        _ => return false,
      };
      // Be conservative: don't accept percent-encoded unreserved bytes (`Url` will serialize these
      // as the literal character).
      if matches!(
        val,
        b'a'..=b'z'
          | b'A'..=b'Z'
          | b'0'..=b'9'
          | b'-'
          | b'.'
          | b'_'
          | b'~'
      ) {
        return false;
      }
      i += 3;
      continue;
    }
    i += 1;
  }

  let authority_start = scheme_end + 3;
  if authority_start >= trimmed.len() {
    return false;
  }

  // Split authority + path (and ensure an explicit path is present).
  let after_scheme = &trimmed[authority_start..];
  let Some(rel_delim) = after_scheme
    .as_bytes()
    .iter()
    .position(|&b| b == b'/' || b == b'?')
  else {
    return false;
  };
  let delim = authority_start + rel_delim;
  if bytes[delim] != b'/' {
    // Canonical Url serialization always includes a path segment; query-only URLs get `/?`.
    return false;
  }

  let authority = &trimmed[authority_start..delim];

  // `file:` URLs are canonicalized with an empty host (`file:///...`). Allow only that common case.
  if scheme == AllowedScheme::File {
    if !authority.is_empty() {
      return false;
    }
  } else if authority.is_empty() {
    return false;
  }

  // Reject userinfo and other authority forms the worker shouldn't emit (and that `Url` may
  // normalize).
  if authority.as_bytes().contains(&b'@') {
    return false;
  }
  // Reject percent-encoded bytes in the authority (e.g. IPv6 zone identifiers) to keep this check
  // simple and conservative.
  if authority.as_bytes().contains(&b'%') {
    return false;
  }

  // Ensure the authority is lowercase (host and scheme are lowercased by `Url`).
  if authority.bytes().any(|b| matches!(b, b'A'..=b'Z')) {
    return false;
  }

  // Validate host/port normalization.
  if !authority_is_canon(scheme, authority) {
    return false;
  }

  // Ensure the path contains no dot-segments that `Url` would remove.
  let path_and_more = &trimmed[delim..];
  let path_end = path_and_more
    .as_bytes()
    .iter()
    .position(|&b| b == b'?')
    .map(|o| delim + o)
    .unwrap_or(trimmed.len());
  let path = &trimmed[delim..path_end];
  if path.contains("/./") || path.contains("/../") || path.ends_with("/.") || path.ends_with("/..")
  {
    return false;
  }

  true
}

fn hex_val_upper(b: u8) -> Option<u8> {
  match b {
    b'0'..=b'9' => Some(b - b'0'),
    b'A'..=b'F' => Some(b - b'A' + 10),
    _ => None,
  }
}

fn authority_is_canon(scheme: AllowedScheme, authority: &str) -> bool {
  // Split host and optional port.
  let (host, port) = if authority.starts_with('[') {
    let Some(end) = authority.find(']') else {
      return false;
    };
    let host_inner = &authority[1..end];
    let Ok(addr) = host_inner.parse::<Ipv6Addr>() else {
      return false;
    };
    if addr.to_string() != host_inner {
      return false;
    }
    let after = &authority[end + 1..];
    if after.is_empty() {
      (&authority[..end + 1], None)
    } else if let Some(port_str) = after.strip_prefix(':') {
      (&authority[..end + 1], Some(port_str))
    } else {
      return false;
    }
  } else if let Some(colon) = authority.rfind(':') {
    let port_str = &authority[colon + 1..];
    if port_str.is_empty() {
      return false;
    }
    if !port_str.bytes().all(|b| b.is_ascii_digit()) {
      return false;
    }
    (&authority[..colon], Some(port_str))
  } else {
    (authority, None)
  };

  // Validate host characters are in a conservative set for already-normalized URLs.
  if !host.is_empty()
    && !host.starts_with('[')
    && !host
      .bytes()
      .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-'))
  {
    return false;
  }

  // Avoid fast-pathing potential "IPv4 number" host representations (`Url` normalizes these).
  if !host.starts_with('[') {
    let host_no_brackets = host;
    if !host_no_brackets.is_empty() && host_no_brackets.bytes().all(|b| b.is_ascii_digit()) {
      // A purely-numeric host would be parsed as IPv4 and normalized.
      return false;
    }
    if host_no_brackets.starts_with("0x") {
      return false;
    }
    if !host_no_brackets.is_empty()
      && host_no_brackets
        .bytes()
        .all(|b| b.is_ascii_digit() || b == b'.')
    {
      // Canonicalize dotted-quad IPv4.
      let Ok(addr) = host_no_brackets.parse::<Ipv4Addr>() else {
        return false;
      };
      if addr.to_string() != host_no_brackets {
        return false;
      }
    }
  }

  if let Some(port_str) = port {
    // No leading zeros in the canonical serialization.
    if port_str.len() > 1 && port_str.as_bytes()[0] == b'0' {
      return false;
    }
    // Valid u16 range.
    let Ok(port_num) = port_str.parse::<u16>() else {
      return false;
    };
    let default = match scheme {
      AllowedScheme::Http => 80,
      AllowedScheme::Https => 443,
      AllowedScheme::File => {
        // `file:` URLs in the worker should never have ports.
        return false;
      }
    };
    if port_num == default {
      // Default ports are stripped by `Url` serialization.
      return false;
    }
  }

  // Ensure we never accept "scheme://?query" style URLs.
  if matches!(scheme, AllowedScheme::Http | AllowedScheme::Https) && host.is_empty() {
    return false;
  }

  true
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
  last_query_lower: String,
  /// Cached lowercase token byte ranges for `last_query_lower`.
  ///
  /// Tokens are stored as byte ranges (rather than `&str` slices) so the searcher remains
  /// self-contained and allocation-light without requiring self-referential borrows.
  last_token_ranges: SmallVec<[Range<usize>; 4]>,
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
      // Keep `last_query_lower`/`last_revision` as-is; `limit == 0` is not a meaningful cache state.
      return &self.cached_match_indices;
    }

    let store_revision = store.revision();
    // Query matching is ASCII case-insensitive; treat ASCII-only case changes as cache hits.
    let query_changed = !query.eq_ignore_ascii_case(self.last_query_lower.as_str());
    let store_changed = store_revision != self.last_revision;
    let needs_more = limit > self.cached_limit && !self.cached_complete;
    if query_changed || store_changed || needs_more {
      // Only re-tokenize when the query itself changes; if history mutates while the query stays
      // stable we can reuse the cached tokens.
      if query_changed {
        self.last_query_lower.clear();
        self.last_query_lower.push_str(query);
        self.last_query_lower.make_ascii_lowercase();

        self.last_token_ranges.clear();
        let base = self.last_query_lower.as_ptr() as usize;
        for token in self.last_query_lower.split_whitespace() {
          let start = token.as_ptr() as usize - base;
          self.last_token_ranges.push(start..start + token.len());
        }
      }
      self.last_revision = store_revision;

      let (indices, complete) = compute_search_match_indices(
        store,
        self.last_query_lower.as_str(),
        &self.last_token_ranges,
        limit,
      );
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
  query_lower: &str,
  token_ranges: &[Range<usize>],
  limit: usize,
) -> (Vec<usize>, bool) {
  if limit == 0 {
    return (Vec::new(), true);
  }

  if token_ranges.is_empty() {
    let indices: Vec<usize> = store
      .iter_recent()
      .take(limit)
      .map(|(idx, _)| idx)
      .collect();
    let complete = store.entries.len() <= limit;
    return (indices, complete);
  }

  if token_ranges.len() == 1 {
    let token = &query_lower[token_ranges[0].clone()];
    let mut out = Vec::with_capacity(limit.min(store.entries.len()));
    for (idx, entry) in store.iter_recent() {
      let in_url = contains_ascii_case_insensitive(&entry.url, token);
      let in_title = entry
        .title
        .as_deref()
        .is_some_and(|t| contains_ascii_case_insensitive(t, token));
      if !in_url && !in_title {
        continue;
      }

      out.push(idx);
      if out.len() >= limit {
        return (out, false);
      }
    }

    return (out, true);
  }

  let mut out = Vec::with_capacity(limit.min(store.entries.len()));
  'entries: for (idx, entry) in store.iter_recent() {
    for range in token_ranges {
      let token = &query_lower[range.clone()];
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

  fn normalize_url_for_history_slow(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
      return None;
    }
    if crate::ui::about_pages::is_about_url(trimmed) {
      return None;
    }

    if let Ok(mut parsed) = Url::parse(trimmed) {
      let scheme = parsed.scheme();
      if !matches!(scheme, "http" | "https" | "file") {
        return None;
      }
      parsed.set_fragment(None);
      return Some(parsed.to_string());
    }

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

  fn assert_url_index_consistent(store: &GlobalHistoryStore) {
    assert_eq!(
      store.url_index.len(),
      store.entries.len(),
      "url index should track every entry"
    );
    for (idx, entry) in store.entries.iter().enumerate() {
      assert_eq!(
        store.url_index.get(entry.url.as_str()),
        Some(&idx),
        "url index should map {} to {}",
        entry.url,
        idx
      );
    }
  }

  #[test]
  fn strips_fragments_and_dedupes_by_normalized_url() {
    let mut store = GlobalHistoryStore::with_capacity(10);

    assert!(store
      .record_at_ms(
        "https://example.test/a#one".to_string(),
        Some("A1".to_string()),
        1
      )
      .is_some());
    assert_eq!(store.entries.len(), 1);
    let entry = store.entries.last().unwrap();
    assert_eq!(entry.url, "https://example.test/a");
    assert_eq!(entry.title.as_deref(), Some("A1"));
    assert_eq!(entry.visited_at_ms, 1);
    assert_eq!(entry.visit_count, 1);

    assert!(store
      .record_at_ms(
        "https://example.test/a#two".to_string(),
        Some("A2".to_string()),
        2
      )
      .is_some());
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

    let _ = store.record_at_ms("https://a.example/".to_string(), Some("A".to_string()), 1);
    let _ = store.record_at_ms("https://b.example/".to_string(), Some("B".to_string()), 2);
    let _ = store.record_at_ms("https://a.example/".to_string(), None, 3);

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
  fn apply_visit_delta_matches_record_in_same_order() {
    let mut recorded = GlobalHistoryStore::with_capacity(10);
    let mut applied = GlobalHistoryStore::with_capacity(10);
    let mut deltas = Vec::new();

    for (url, title, ts) in [
      ("https://example.test/a#one", Some("A1"), 1_u64),
      ("https://example.test/b", Some("B"), 2),
      ("https://example.test/a#two", None, 3),
    ] {
      let delta = recorded
        .record_at_ms(url.to_string(), title.map(|t| t.to_string()), ts)
        .expect("expected visit to be recorded");
      deltas.push(delta);
    }

    for delta in &deltas {
      assert!(applied.apply_visit_delta(delta));
    }

    assert_eq!(applied, recorded);
    assert_url_index_consistent(&applied);
  }

  #[test]
  fn apply_visit_delta_increments_visit_count_and_moves_to_most_recent() {
    let mut store = GlobalHistoryStore::with_capacity(10);

    let a1 = HistoryVisitDelta {
      url: "https://a.example/".to_string(),
      title: None,
      visited_at_ms: 1,
    };
    let b1 = HistoryVisitDelta {
      url: "https://b.example/".to_string(),
      title: None,
      visited_at_ms: 2,
    };
    let a2 = HistoryVisitDelta {
      url: "https://a.example/".to_string(),
      title: None,
      visited_at_ms: 3,
    };

    assert!(store.apply_visit_delta(&a1));
    assert!(store.apply_visit_delta(&b1));
    assert!(store.apply_visit_delta(&a2));

    assert_eq!(store.entries.len(), 2);
    assert_eq!(
      store
        .entries
        .iter()
        .map(|e| e.url.as_str())
        .collect::<Vec<_>>(),
      vec!["https://b.example/", "https://a.example/"]
    );
    assert_eq!(store.get("https://a.example/").unwrap().visit_count, 2);
    assert_eq!(store.get("https://a.example/").unwrap().visited_at_ms, 3);
  }

  #[test]
  fn concurrent_window_deltas_merge_without_loss() {
    let mut global = GlobalHistoryStore::with_capacity(10);
    let mut win_a = global.clone();
    let mut win_b = global.clone();

    // Window A commits a visit and publishes a delta.
    let delta_a = win_a
      .record_at_ms("https://a.example/".to_string(), None, 1)
      .expect("expected delta");
    assert!(global.apply_visit_delta(&delta_a));
    // Propagate to the other window before it records its own visit (matches the browser's
    // per-window drain+propagate logic).
    assert!(win_b.apply_visit_delta(&delta_a));

    // Window B commits a different visit in the same wake batch.
    let delta_b = win_b
      .record_at_ms("https://b.example/".to_string(), None, 2)
      .expect("expected delta");
    assert!(global.apply_visit_delta(&delta_b));
    assert!(win_a.apply_visit_delta(&delta_b));

    assert_eq!(global.len(), 2);
    assert!(global.get("https://a.example/").is_some());
    assert!(global.get("https://b.example/").is_some());
    assert_eq!(win_a, global);
    assert_eq!(win_b, global);
  }

  #[test]
  fn url_index_tracks_record_updates_and_recency_ordering() {
    let mut store = GlobalHistoryStore::with_capacity(10);

    let _ = store.record_at_ms("https://a.example/".to_string(), Some("A".to_string()), 1);
    let _ = store.record_at_ms("https://b.example/".to_string(), Some("B".to_string()), 2);
    let _ = store.record_at_ms("https://c.example/".to_string(), Some("C".to_string()), 3);
    assert_eq!(
      store
        .entries
        .iter()
        .map(|e| e.url.as_str())
        .collect::<Vec<_>>(),
      vec![
        "https://a.example/",
        "https://b.example/",
        "https://c.example/"
      ]
    );
    assert_url_index_consistent(&store);

    // Update a middle entry; it should move to the end and all shifted indices should update.
    let _ = store.record_at_ms("https://b.example/".to_string(), None, 4);
    assert_eq!(
      store
        .entries
        .iter()
        .map(|e| e.url.as_str())
        .collect::<Vec<_>>(),
      vec![
        "https://a.example/",
        "https://c.example/",
        "https://b.example/"
      ]
    );
    assert_eq!(store.get("https://c.example/").unwrap().visited_at_ms, 3);
    assert_eq!(store.get("https://b.example/").unwrap().visited_at_ms, 4);
    assert_url_index_consistent(&store);

    // Update the oldest entry; it should move to the end.
    let _ = store.record_at_ms("https://a.example/".to_string(), None, 5);
    assert_eq!(
      store
        .entries
        .iter()
        .map(|e| e.url.as_str())
        .collect::<Vec<_>>(),
      vec![
        "https://c.example/",
        "https://b.example/",
        "https://a.example/"
      ]
    );
    assert_url_index_consistent(&store);

    // Updating the most-recent entry should not reorder.
    let _ = store.record_at_ms("https://a.example/#fragment".to_string(), None, 6);
    assert_eq!(
      store
        .entries
        .iter()
        .map(|e| e.url.as_str())
        .collect::<Vec<_>>(),
      vec![
        "https://c.example/",
        "https://b.example/",
        "https://a.example/"
      ]
    );
    assert_eq!(store.get("https://a.example/").unwrap().visited_at_ms, 6);
    assert_url_index_consistent(&store);
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
      assert!(store.record_at_ms(url.to_string(), None, 1).is_none());
    }

    assert!(store.entries.is_empty());
  }

  #[test]
  fn records_file_urls() {
    let mut store = GlobalHistoryStore::with_capacity(10);

    assert!(store
      .record_at_ms("file:///tmp/a.html#section".to_string(), None, 10)
      .is_some());
    assert_eq!(store.entries.len(), 1);
    assert_eq!(store.entries[0].url, "file:///tmp/a.html");
    assert_eq!(store.entries[0].visit_count, 1);
  }

  #[test]
  fn normalize_url_fast_path_matches_previous_implementation_for_common_urls() {
    // These should cover the common "already-normalized absolute URL" case emitted by the worker,
    // plus nearby variants that still require parsing/canonicalization.
    for url in [
      // https
      "https://example.test/",
      "https://example.test/a",
      "https://example.test/a?b=c",
      "https://example.test/?q=test",
      "https://example.test/a#frag",
      "https://example.test/?q=test#frag",
      // http
      "http://example.test/",
      "http://example.test/a?b=c#frag",
      // file
      "file:///tmp/a.html",
      "file:///tmp/a.html?x=1",
      "file:///tmp/a.html#section",
      // Canonicalization-required inputs (should still match the old output).
      "https://example.test",         // Url adds trailing slash
      "https://example.test?x=1",     // Url inserts `/` before `?`
      "HTTP://Example.TEST/",         // scheme/host lowercasing
      "http://example.test:80/",      // default port stripping
      "https://example.test/a/../b",  // dot-segment removal
      "https://example.test/%7euser", // percent-encoding normalization
    ] {
      assert_eq!(
        normalize_url_for_history(url),
        normalize_url_for_history_slow(url),
        "normalization mismatch for {url}"
      );
    }
  }

  #[test]
  fn normalize_url_weird_unparseable_urls_preserve_previous_behavior() {
    for url in [
      // Invalid IPv6 literals (missing closing bracket) should hit the best-effort fallback.
      "https://[::1",
      "http://[::1",
      "https://[::1#frag",
      // Invalid percent encoding.
      "https://example.test/%",
      "https://example.test/%GG",
      // Invalid ports.
      "http://example.test:/",
      "http://example.test:abc/",
    ] {
      assert!(
        Url::parse(url).is_err(),
        "test URL should be unparseable by Url::parse: {url}"
      );
      assert_eq!(
        normalize_url_for_history(url),
        normalize_url_for_history_slow(url),
        "fallback mismatch for {url}"
      );
    }
  }

  #[test]
  fn normalize_url_rejects_unsupported_schemes() {
    for url in [
      "about:blank",
      "javascript:alert(1)",
      "data:text/plain,hello",
      "ftp://example.test/",
      "chrome://version",
    ] {
      assert_eq!(
        normalize_url_for_history(url),
        None,
        "unsupported scheme should be rejected: {url}"
      );
    }
  }

  #[test]
  fn every_committed_navigation_increments_visit_count_and_updates_last_visited() {
    let mut store = GlobalHistoryStore::with_capacity(10);

    let _ = store.record_at_ms("https://example.test/a".to_string(), None, 1);
    let _ = store.record_at_ms("https://example.test/a".to_string(), None, 2);
    let _ = store.record_at_ms("https://example.test/a".to_string(), None, 3);

    let entry = store.get("https://example.test/a").unwrap();
    assert_eq!(entry.visit_count, 3);
    assert_eq!(entry.visited_at_ms, 3);
  }

  #[test]
  fn title_is_updated_only_when_some_non_empty() {
    let mut store = GlobalHistoryStore::with_capacity(10);

    let _ = store.record_at_ms(
      "https://example.test/".to_string(),
      Some("Title".to_string()),
      1,
    );
    let _ = store.record_at_ms("https://example.test/".to_string(), None, 2);
    let _ = store.record_at_ms(
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
    let _ = store.record_at_ms("https://a.example/".to_string(), None, 1);
    let _ = store.record_at_ms("https://b.example/".to_string(), None, 2);
    let _ = store.record_at_ms("https://c.example/".to_string(), None, 3);

    assert_eq!(store.len(), 2);
    let urls: Vec<&str> = store.iter_recent().map(|(_, e)| e.url.as_str()).collect();
    assert_eq!(urls, vec!["https://c.example/", "https://b.example/"]);
  }

  #[test]
  fn url_index_is_correct_after_capacity_eviction() {
    let mut store = GlobalHistoryStore::with_capacity(3);
    let _ = store.record_at_ms("https://a.example/".to_string(), None, 1);
    let _ = store.record_at_ms("https://b.example/".to_string(), None, 2);
    let _ = store.record_at_ms("https://c.example/".to_string(), None, 3);
    assert_url_index_consistent(&store);

    // Add a new entry at capacity; oldest should be evicted and indices rewritten.
    let _ = store.record_at_ms("https://d.example/".to_string(), None, 4);
    assert_eq!(
      store
        .entries
        .iter()
        .map(|e| e.url.as_str())
        .collect::<Vec<_>>(),
      vec![
        "https://b.example/",
        "https://c.example/",
        "https://d.example/"
      ]
    );
    assert!(store.get("https://a.example/").is_none());
    assert!(store.get("https://b.example/").is_some());
    assert!(store.get("https://c.example/").is_some());
    assert!(store.get("https://d.example/").is_some());
    assert_url_index_consistent(&store);

    // Moving an entry after eviction should still preserve index consistency.
    let _ = store.record_at_ms("https://b.example/".to_string(), None, 5);
    assert_eq!(
      store
        .entries
        .iter()
        .map(|e| e.url.as_str())
        .collect::<Vec<_>>(),
      vec![
        "https://c.example/",
        "https://d.example/",
        "https://b.example/"
      ]
    );
    assert_url_index_consistent(&store);

    // Another eviction should drop the new oldest.
    let _ = store.record_at_ms("https://e.example/".to_string(), None, 6);
    assert_eq!(
      store
        .entries
        .iter()
        .map(|e| e.url.as_str())
        .collect::<Vec<_>>(),
      vec![
        "https://d.example/",
        "https://b.example/",
        "https://e.example/"
      ]
    );
    assert!(store.get("https://c.example/").is_none());
    assert_url_index_consistent(&store);
  }

  #[test]
  fn search_matches_all_tokens_in_url_or_title_and_is_recency_first() {
    let mut history = GlobalHistoryStore::with_capacity(10);

    let _ = history.record_at_ms(
      "https://example.com/one".to_string(),
      Some("First Page".to_string()),
      1,
    );
    let _ = history.record_at_ms(
      "https://example.com/two".to_string(),
      Some("Second Page".to_string()),
      2,
    );
    let _ = history.record_at_ms(
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
    let _ = history.record_at_ms("https://a.example/".to_string(), None, 1);
    let _ = history.record_at_ms("https://b.example/".to_string(), None, 2);
    let _ = history.record_at_ms("https://c.example/".to_string(), None, 3);

    assert!(history.delete_entry("https://b.example/"));
    assert!(!history.delete_entry("https://b.example/"));

    let urls: Vec<&str> = history.iter_recent().map(|(_, e)| e.url.as_str()).collect();
    assert_eq!(urls, vec!["https://c.example/", "https://a.example/"]);
  }

  #[test]
  fn remove_at_removes_entries_by_index() {
    let mut history = GlobalHistoryStore::with_capacity(10);
    let _ = history.record_at_ms("https://a.example/".to_string(), None, 1);
    let _ = history.record_at_ms("https://b.example/".to_string(), None, 2);
    let _ = history.record_at_ms("https://c.example/".to_string(), None, 3);

    assert_eq!(history.remove_at(99), None);

    let removed = history.remove_at(1).expect("should remove b");
    assert_eq!(removed.url, "https://b.example/");

    let urls: Vec<&str> = history.iter_recent().map(|(_, e)| e.url.as_str()).collect();
    assert_eq!(urls, vec!["https://c.example/", "https://a.example/"]);
  }

  #[test]
  fn clear_all_since_and_range() {
    let mut history = GlobalHistoryStore::with_capacity(10);
    let _ = history.record_at_ms("https://a.example/".to_string(), None, 10);
    let _ = history.record_at_ms("https://b.example/".to_string(), None, 20);
    let _ = history.record_at_ms("https://c.example/".to_string(), None, 30);

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
  fn url_index_is_rebuilt_after_load_normalization() {
    let raw = r#"{
      "entries": [
        { "url": "https://a.example/#one", "title": "Old", "visited_at_ms": 1 },
        { "url": "https://b.example/", "title": "Bee", "visited_at_ms": 2 },
        { "url": "about:newtab", "title": "New Tab", "visited_at_ms": 3 },
        { "url": "https://a.example/#two", "title": "New", "visited_at_ms": 4 }
      ]
    }"#;

    let mut store: GlobalHistoryStore =
      serde_json::from_str(raw).expect("legacy JSON should parse");
    assert_eq!(store.entries.len(), 2);
    assert_url_index_consistent(&store);

    assert_eq!(
      store
        .get("https://a.example/#frag")
        .unwrap()
        .title
        .as_deref(),
      Some("New")
    );
    assert_eq!(
      store.get("https://b.example/").unwrap().title.as_deref(),
      Some("Bee")
    );

    // Recording a URL that already exists after load should update in place (no duplicate entry).
    let _ = store.record_at_ms(
      "https://a.example/#three".to_string(),
      Some("Newest".to_string()),
      10,
    );
    assert_eq!(store.entries.len(), 2);
    let a = store.get("https://a.example/").unwrap();
    assert_eq!(a.visit_count, 3);
    assert_eq!(a.visited_at_ms, 10);
    assert_eq!(a.title.as_deref(), Some("Newest"));
    assert_url_index_consistent(&store);
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
    let _ = store.record_at_ms("https://a.example/".to_string(), Some("A".to_string()), 1);

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
    let _ = history.record_at_ms(
      "https://old.example/".to_string(),
      None,
      now_ms - 8 * DAY_MS,
    );
    let _ = history.record_at_ms(
      "https://days.example/".to_string(),
      None,
      now_ms - 2 * DAY_MS,
    );
    let _ = history.record_at_ms(
      "https://hours.example/".to_string(),
      None,
      now_ms - 2 * HOUR_MS,
    );
    let _ = history.record_at_ms(
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
  fn clear_browsing_data_range_len_diff_matches_expected_removed_counts() {
    const HOUR_MS: u64 = 60 * 60 * 1000;
    const DAY_MS: u64 = 24 * HOUR_MS;
    let now_ms = 1_000_000_000_000_u64;

    let mut baseline = GlobalHistoryStore::default();
    let _ = baseline.record_at_ms(
      "https://old.example/".to_string(),
      None,
      now_ms - 8 * DAY_MS,
    );
    let _ = baseline.record_at_ms(
      "https://days.example/".to_string(),
      None,
      now_ms - 2 * DAY_MS,
    );
    let _ = baseline.record_at_ms(
      "https://hours.example/".to_string(),
      None,
      now_ms - 2 * HOUR_MS,
    );
    let _ = baseline.record_at_ms(
      "https://recent.example/".to_string(),
      None,
      now_ms - 10 * 60 * 1000,
    );
    baseline.entries.push(GlobalHistoryEntry {
      url: "https://unknown.example/".to_string(),
      title: None,
      visited_at_ms: 0,
      visit_count: 1,
    });

    for (range, expected_removed) in [
      (ClearBrowsingDataRange::LastHour, 1usize),
      (ClearBrowsingDataRange::Last24Hours, 2),
      (ClearBrowsingDataRange::Last7Days, 3),
      (ClearBrowsingDataRange::AllTime, 5),
    ] {
      let mut history = baseline.clone();
      let before = history.len();
      history.clear_browsing_data_range_at_ms(range, now_ms);
      let removed = before.saturating_sub(history.len());
      assert_eq!(
        removed, expected_removed,
        "unexpected removed count for range {:?}",
        range
      );
    }
  }

  #[test]
  fn search_returns_results_ordered_by_recency() {
    let mut history = GlobalHistoryStore::default();
    let _ = history.record_at_ms(
      "https://example.com/a".to_string(),
      Some("First".to_string()),
      1,
    );
    let _ = history.record_at_ms(
      "https://example.com/b".to_string(),
      Some("Second".to_string()),
      2,
    );
    let _ = history.record_at_ms(
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
    let _ = history.record_at_ms(
      "https://example.com/one".to_string(),
      Some("Example One".to_string()),
      1,
    );
    let _ = history.record_at_ms(
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
    let _ = history.record_at_ms(
      "https://example.com/one".to_string(),
      Some("One".to_string()),
      1,
    );
    let _ = history.record_at_ms(
      "https://example.com/two".to_string(),
      Some("Two".to_string()),
      2,
    );

    let mut searcher = GlobalHistorySearcher::default();
    let before = searcher.search_indices(&history, "example", 10).to_vec();
    assert_eq!(before.len(), 2);
    assert_eq!(history.entries[before[0]].url, "https://example.com/two");

    // Mutation: add a newer matching entry.
    let _ = history.record_at_ms(
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

  #[test]
  #[ignore]
  fn record_benchmark_does_not_linear_scan_urls() {
    use std::time::Instant;

    let mut store = GlobalHistoryStore::with_capacity(10_000);
    for i in 0..10_000_u32 {
      let _ = store.record_at_ms(format!("https://example.test/{i}"), None, i as u64);
    }

    let t0 = Instant::now();
    // Update a middle entry repeatedly; with a URL index this should avoid O(n) URL comparisons.
    for i in 0..1000_u32 {
      let _ = store.record_at_ms(
        "https://example.test/5000".to_string(),
        None,
        10_000 + i as u64,
      );
    }
    let dt = t0.elapsed();
    eprintln!("record 1000 updates into 10k-entry store: {dt:?}");
  }
}
