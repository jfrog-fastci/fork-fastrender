use crate::ui::browser_app::{BrowserTabState, ClosedTabState};
use crate::ui::messages::TabId;
use crate::ui::visited::VisitedUrlStore;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;

/// The input + state available to [`OmniboxProvider`] implementations.
///
/// This type intentionally avoids `egui` types so the suggestion engine can be unit tested and
/// reused by non-egui front-ends.
#[derive(Debug, Clone, Copy)]
pub struct OmniboxContext<'a> {
  /// All currently open tabs (including the active tab).
  pub open_tabs: &'a [BrowserTabState],
  /// Stack of recently closed tabs (see `BrowserAppState::closed_tabs`).
  pub closed_tabs: &'a [ClosedTabState],
  /// Global visited URL store (used for history-style suggestions).
  pub visited: &'a VisitedUrlStore,
  /// Optional active tab id; providers can use this to avoid suggesting "switch to tab" for the
  /// already-active tab.
  pub active_tab_id: Option<TabId>,
  /// Optional bookmark store. Bookmarks are not implemented yet, but the omnibox provider
  /// architecture expects to receive them via the context.
  pub bookmarks: Option<&'a BrowserBookmarks>,
  /// Optional cache of remote search suggestions.
  ///
  /// This is intentionally a *synchronous* interface: async-backed suggest providers are expected
  /// to maintain a background cache and expose the latest results here so the omnibox engine can
  /// stay deterministic and non-blocking.
  pub remote_search_suggest: Option<&'a RemoteSearchSuggestCache>,
}

/// A synchronous source of omnibox suggestions.
///
/// Providers should be pure functions of `ctx` + `input`: no side effects, no I/O, no blocking.
pub trait OmniboxProvider {
  fn suggestions(&self, ctx: &OmniboxContext<'_>, input: &str) -> Vec<OmniboxSuggestion>;
}

/// The action a suggestion represents (what happens when the user selects it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OmniboxAction {
  NavigateToUrl(String),
  ActivateTab(TabId),
  Search(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OmniboxUrlSource {
  Bookmark,
  OpenTab,
  ClosedTab,
  Visited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OmniboxSearchSource {
  RemoteSuggest,
}

/// A single omnibox suggestion row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmniboxSuggestion {
  pub action: OmniboxAction,
  /// Optional human-friendly title (tab title, page title, bookmark title, …).
  pub title: Option<String>,
  /// Optional URL associated with this suggestion (used for matching + display).
  pub url: Option<String>,
  /// Source hint used for scoring + UI presentation.
  pub source: OmniboxSuggestionSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OmniboxSuggestionSource {
  Url(OmniboxUrlSource),
  Search(OmniboxSearchSource),
}

impl OmniboxSuggestion {
  fn dedup_key_owned(&self) -> String {
    match &self.action {
      OmniboxAction::NavigateToUrl(url) => url.to_ascii_lowercase(),
      OmniboxAction::ActivateTab(_) => self
        .url
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase(),
      OmniboxAction::Search(query) => query.to_ascii_lowercase(),
    }
  }
}

// -----------------------------------------------------------------------------
// Data stores (stubs / minimal implementations)
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkEntry {
  pub url: String,
  pub title: Option<String>,
}

/// Placeholder for the future bookmarks implementation.
#[derive(Debug, Default)]
pub struct BrowserBookmarks {
  entries: Vec<BookmarkEntry>,
}

impl BrowserBookmarks {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn push(&mut self, entry: BookmarkEntry) {
    self.entries.push(entry);
  }

  pub fn entries(&self) -> &[BookmarkEntry] {
    &self.entries
  }
}

/// Placeholder for a cache of remote search suggestions.
#[derive(Debug, Default)]
pub struct RemoteSearchSuggestCache {
  // Using an `Arc` here makes it easy for async-updaters to replace the entire cache without
  // blocking the UI thread.
  latest: Arc<RemoteSearchSuggestSnapshot>,
}

#[derive(Debug, Default)]
pub struct RemoteSearchSuggestSnapshot {
  // In the future this can include per-engine state (e.g. Google vs DuckDuckGo), freshness
  // timestamps, debounced in-flight query, etc.
  //
  // For now it remains empty; the `RemoteSearchSuggestProvider` is a no-op.
  _private: (),
}

impl RemoteSearchSuggestCache {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn snapshot(&self) -> Arc<RemoteSearchSuggestSnapshot> {
    self.latest.clone()
  }
}

// -----------------------------------------------------------------------------
// Providers
// -----------------------------------------------------------------------------

pub struct OpenTabsProvider;

impl OmniboxProvider for OpenTabsProvider {
  fn suggestions(&self, ctx: &OmniboxContext<'_>, _input: &str) -> Vec<OmniboxSuggestion> {
    let mut out = Vec::new();
    for tab in ctx.open_tabs {
      if ctx.active_tab_id == Some(tab.id) {
        continue;
      }
      let Some(url) = tab.current_url.as_ref().filter(|u| !u.trim().is_empty()) else {
        continue;
      };
      let title = tab
        .title
        .as_ref()
        .or(tab.committed_title.as_ref())
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string());

      out.push(OmniboxSuggestion {
        action: OmniboxAction::ActivateTab(tab.id),
        title,
        url: Some(url.clone()),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab),
      });
    }
    out
  }
}

pub struct ClosedTabsProvider;

impl OmniboxProvider for ClosedTabsProvider {
  fn suggestions(&self, ctx: &OmniboxContext<'_>, _input: &str) -> Vec<OmniboxSuggestion> {
    let mut out = Vec::new();
    for closed in ctx.closed_tabs {
      if closed.url.trim().is_empty() {
        continue;
      }
      let title = closed
        .title
        .as_ref()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string());
      out.push(OmniboxSuggestion {
        action: OmniboxAction::NavigateToUrl(closed.url.clone()),
        title,
        url: Some(closed.url.clone()),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab),
      });
    }
    out
  }
}

pub struct VisitedProvider;

impl OmniboxProvider for VisitedProvider {
  fn suggestions(&self, ctx: &OmniboxContext<'_>, input: &str) -> Vec<OmniboxSuggestion> {
    // Cap the number of visited records we consider per query so omnibox completion stays cheap.
    const VISITED_LIMIT: usize = 200;

    let mut out = Vec::new();
    for record in ctx.visited.search(input, VISITED_LIMIT) {
      if record.url.trim().is_empty() {
        continue;
      }
      let title = record
        .title
        .as_ref()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string());
      out.push(OmniboxSuggestion {
        action: OmniboxAction::NavigateToUrl(record.url.clone()),
        title,
        url: Some(record.url.clone()),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
      });
    }
    out
  }
}

/// Stub provider: bookmarks suggestions are not implemented yet.
pub struct BookmarksProvider;

impl OmniboxProvider for BookmarksProvider {
  fn suggestions(&self, _ctx: &OmniboxContext<'_>, _input: &str) -> Vec<OmniboxSuggestion> {
    // Future implementation will read from `ctx.bookmarks` and return URL suggestions with
    // `OmniboxUrlSource::Bookmark`.
    Vec::new()
  }
}

/// Stub provider: remote search suggestions are not implemented yet.
pub struct RemoteSearchSuggestProvider;

impl OmniboxProvider for RemoteSearchSuggestProvider {
  fn suggestions(&self, _ctx: &OmniboxContext<'_>, _input: &str) -> Vec<OmniboxSuggestion> {
    // Future implementation will read from `ctx.remote_search_suggest` and return
    // `OmniboxAction::Search` suggestions.
    Vec::new()
  }
}

// -----------------------------------------------------------------------------
// Engine
// -----------------------------------------------------------------------------

/// Build omnibox suggestions for `input` using the default provider set.
pub fn build_omnibox_suggestions(ctx: &OmniboxContext<'_>, input: &str) -> Vec<OmniboxSuggestion> {
  build_omnibox_suggestions_with_providers(ctx, input, default_providers())
}

fn default_providers() -> Vec<Box<dyn OmniboxProvider>> {
  vec![
    Box::new(OpenTabsProvider),
    Box::new(ClosedTabsProvider),
    Box::new(VisitedProvider),
    Box::new(BookmarksProvider),
    Box::new(RemoteSearchSuggestProvider),
  ]
}

fn build_omnibox_suggestions_with_providers(
  ctx: &OmniboxContext<'_>,
  input: &str,
  providers: Vec<Box<dyn OmniboxProvider>>,
) -> Vec<OmniboxSuggestion> {
  let input = input.trim();
  if input.is_empty() {
    return Vec::new();
  }
  let input_lower = input.to_ascii_lowercase();

  let mut scored = Vec::<ScoredSuggestion>::new();
  for provider in providers {
    for suggestion in provider.suggestions(ctx, input) {
      let Some(score) = score_suggestion(&suggestion, &input_lower) else {
        continue;
      };
      scored.push(ScoredSuggestion { suggestion, score });
    }
  }

  scored.sort_by(compare_scored_suggestions);

  let mut seen: HashSet<String> = HashSet::new();
  let mut out = Vec::new();
  for scored in scored {
    if seen.insert(scored.suggestion.dedup_key_owned()) {
      out.push(scored.suggestion);
    }
  }
  out
}

#[derive(Debug)]
struct ScoredSuggestion {
  suggestion: OmniboxSuggestion,
  score: i64,
}

fn score_suggestion(suggestion: &OmniboxSuggestion, input_lower: &str) -> Option<i64> {
  let base = match suggestion.source {
    OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab) => 3_000,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark) => 2_500,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab) => 2_000,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited) => 1_000,
    OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest) => 500,
  };

  let mut best_match = None::<i64>;

  if let Some(url) = suggestion.url.as_deref() {
    best_match = best_match.max(match_score(url, input_lower));
  }
  if let Some(title) = suggestion.title.as_deref() {
    best_match = best_match.max(match_score(title, input_lower));
  }

  // Search suggestions match against the query text.
  if let OmniboxAction::Search(query) = &suggestion.action {
    best_match = best_match.max(match_score(query, input_lower));
  }

  best_match.map(|m| base + m)
}

/// Returns a score for `needle` in `haystack`, where larger is better.
fn match_score(haystack: &str, needle_lower: &str) -> Option<i64> {
  let haystack_lower = haystack.to_ascii_lowercase();
  let idx = haystack_lower.find(needle_lower)? as i64;

  // Prefer prefix matches and earlier matches. Clamp so long strings don't overflow.
  let prefix_bonus = if idx == 0 { 1_000 } else { 0 };
  let position_bonus = (200 - idx).max(0);
  Some(prefix_bonus + position_bonus)
}

fn compare_scored_suggestions(a: &ScoredSuggestion, b: &ScoredSuggestion) -> Ordering {
  // Primary sort: score descending.
  match b.score.cmp(&a.score) {
    Ordering::Equal => {}
    ord => return ord,
  }

  // Secondary: source (OpenTab > Bookmark > ClosedTab > Visited > Search), consistent with base.
  match suggestion_source_rank(b.suggestion.source).cmp(&suggestion_source_rank(a.suggestion.source)) {
    Ordering::Equal => {}
    ord => return ord,
  }

  // Tertiary: prefer URL/title lexicographically for deterministic ordering (independent of
  // provider order).
  let a_key = suggestion_sort_key(&a.suggestion);
  let b_key = suggestion_sort_key(&b.suggestion);
  a_key.cmp(&b_key)
}

fn suggestion_source_rank(source: OmniboxSuggestionSource) -> i64 {
  match source {
    OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab) => 5,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark) => 4,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab) => 3,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited) => 2,
    OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest) => 1,
  }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SuggestionSortKey {
  // Lowercase for deterministic, case-insensitive ordering.
  primary: String,
  secondary: String,
  // Include TabId when relevant so multiple open-tab suggestions for the same URL are stable.
  tab_id: u64,
}

fn suggestion_sort_key(s: &OmniboxSuggestion) -> SuggestionSortKey {
  let (primary, secondary, tab_id) = match &s.action {
    OmniboxAction::ActivateTab(tab_id) => (
      s.url.as_deref().unwrap_or_default(),
      s.title.as_deref().unwrap_or_default(),
      tab_id.0,
    ),
    OmniboxAction::NavigateToUrl(url) => (url.as_str(), s.title.as_deref().unwrap_or_default(), 0),
    OmniboxAction::Search(query) => (query.as_str(), "", 0),
  };

  SuggestionSortKey {
    primary: primary.to_ascii_lowercase(),
    secondary: secondary.to_ascii_lowercase(),
    tab_id,
  }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
  mod tests {
  use super::*;

  #[test]
  fn engine_produces_expected_local_suggestions() {
    let tab_a = TabId(1);
    let tab_b = TabId(2);

    let mut open_tabs = Vec::new();
    let mut a = BrowserTabState::new(tab_a, "https://example.com/".to_string());
    a.title = Some("Example Domain".to_string());
    open_tabs.push(a);
    let mut b = BrowserTabState::new(tab_b, "https://rust-lang.org/".to_string());
    b.title = Some("Rust".to_string());
    open_tabs.push(b);

    let closed_tabs = vec![ClosedTabState {
      url: "https://example.org/".to_string(),
      title: Some("Example Org".to_string()),
    }];

    let mut visited = VisitedUrlStore::with_capacity(10);
    visited.record_visit(
      "https://example.net/".to_string(),
      Some("Example Net".to_string()),
    );
    // Duplicate URL from open tab should be deduped in favour of the open-tab suggestion.
    visited.record_visit(
      "https://example.com/".to_string(),
      Some("Example Domain (history)".to_string()),
    );

    let ctx = OmniboxContext {
      open_tabs: &open_tabs,
      closed_tabs: &closed_tabs,
      visited: &visited,
      active_tab_id: Some(tab_b),
      bookmarks: None,
      remote_search_suggest: None,
    };
    let suggestions = build_omnibox_suggestions(&ctx, "example");

    assert_eq!(
      suggestions,
      vec![
        OmniboxSuggestion {
          action: OmniboxAction::ActivateTab(tab_a),
          title: Some("Example Domain".to_string()),
          url: Some("https://example.com/".to_string()),
          source: OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab),
        },
        OmniboxSuggestion {
          action: OmniboxAction::NavigateToUrl("https://example.org/".to_string()),
          title: Some("Example Org".to_string()),
          url: Some("https://example.org/".to_string()),
          source: OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab),
        },
        OmniboxSuggestion {
          action: OmniboxAction::NavigateToUrl("https://example.net/".to_string()),
          title: Some("Example Net".to_string()),
          url: Some("https://example.net/".to_string()),
          source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
        },
      ]
    );
  }

  #[test]
  fn provider_order_does_not_affect_final_output() {
    struct ProviderVisited;
    impl OmniboxProvider for ProviderVisited {
      fn suggestions(&self, _ctx: &OmniboxContext<'_>, _input: &str) -> Vec<OmniboxSuggestion> {
        vec![OmniboxSuggestion {
          action: OmniboxAction::NavigateToUrl("https://example.com/".to_string()),
          title: Some("Example (history)".to_string()),
          url: Some("https://example.com/".to_string()),
          source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
        }]
      }
    }

    struct ProviderOpenTab;
    impl OmniboxProvider for ProviderOpenTab {
      fn suggestions(&self, _ctx: &OmniboxContext<'_>, _input: &str) -> Vec<OmniboxSuggestion> {
        vec![OmniboxSuggestion {
          action: OmniboxAction::ActivateTab(TabId(99)),
          title: Some("Example (tab)".to_string()),
          url: Some("https://example.com/".to_string()),
          source: OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab),
        }]
      }
    }

    let open_tabs = Vec::new();
    let closed_tabs = Vec::new();
    let visited = VisitedUrlStore::new();
    let ctx = OmniboxContext {
      open_tabs: &open_tabs,
      closed_tabs: &closed_tabs,
      visited: &visited,
      active_tab_id: None,
      bookmarks: None,
      remote_search_suggest: None,
    };

    let a_first = build_omnibox_suggestions_with_providers(
      &ctx,
      "example",
      vec![Box::new(ProviderVisited), Box::new(ProviderOpenTab)],
    );
    let b_first = build_omnibox_suggestions_with_providers(
      &ctx,
      "example",
      vec![Box::new(ProviderOpenTab), Box::new(ProviderVisited)],
    );

    assert_eq!(a_first, b_first);
    assert_eq!(
      a_first,
      vec![OmniboxSuggestion {
        action: OmniboxAction::ActivateTab(TabId(99)),
        title: Some("Example (tab)".to_string()),
        url: Some("https://example.com/".to_string()),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab),
      }]
    );
  }
}
