use crate::ui::browser_app::{BrowserTabState, ClosedTabState};
use crate::ui::url::{resolve_omnibox_input, OmniboxInputResolution};
use crate::ui::messages::TabId;
use crate::ui::about_pages;
use crate::ui::bookmarks::{BookmarkNode, BookmarkStore};
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
  /// Optional bookmark store.
  pub bookmarks: Option<&'a BookmarkStore>,
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
  About,
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
  Primary,
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

pub struct PrimaryActionProvider;

impl OmniboxProvider for PrimaryActionProvider {
  fn suggestions(&self, _ctx: &OmniboxContext<'_>, input: &str) -> Vec<OmniboxSuggestion> {
    let input = input.trim();
    if input.is_empty() {
      return Vec::new();
    }

    let suggestion = match resolve_omnibox_input(input) {
      Ok(OmniboxInputResolution::Url { url }) => OmniboxSuggestion {
        action: OmniboxAction::NavigateToUrl(url.clone()),
        title: None,
        url: Some(url),
        source: OmniboxSuggestionSource::Primary,
      },
      Ok(OmniboxInputResolution::Search { query, .. }) => OmniboxSuggestion {
        // Store the raw query in the action; navigation code is responsible for resolving it into
        // a concrete search engine URL.
        action: OmniboxAction::Search(query.clone()),
        title: Some(query),
        url: None,
        source: OmniboxSuggestionSource::Primary,
      },
      Err(_) => OmniboxSuggestion {
        action: OmniboxAction::Search(input.to_string()),
        title: Some(input.to_string()),
        url: None,
        source: OmniboxSuggestionSource::Primary,
      },
    };

    vec![suggestion]
  }
}

pub struct AboutPagesProvider;

impl OmniboxProvider for AboutPagesProvider {
  fn suggestions(&self, _ctx: &OmniboxContext<'_>, input: &str) -> Vec<OmniboxSuggestion> {
    let input = input.trim();
    if input.is_empty() {
      return Vec::new();
    }
    let input_lower = input.to_ascii_lowercase();

    const PAGES: &[(&str, &str)] = &[
      (about_pages::ABOUT_NEWTAB, "New Tab"),
      (about_pages::ABOUT_HELP, "Help"),
      (about_pages::ABOUT_VERSION, "Version"),
      (about_pages::ABOUT_GPU, "GPU"),
    ];

    let mut out = Vec::new();
    for (url, title) in PAGES {
      let url_lower = url.to_ascii_lowercase();
      let title_lower = title.to_ascii_lowercase();

      if !url_lower.contains(&input_lower) && !title_lower.contains(&input_lower) {
        continue;
      }

      out.push(OmniboxSuggestion {
        action: OmniboxAction::NavigateToUrl((*url).to_string()),
        title: Some((*title).to_string()),
        url: Some((*url).to_string()),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::About),
      });
    }

    out
  }
}

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

/// Bookmark URL suggestions from the persisted [`BookmarkStore`].
pub struct BookmarksProvider;

impl OmniboxProvider for BookmarksProvider {
  fn suggestions(&self, ctx: &OmniboxContext<'_>, input: &str) -> Vec<OmniboxSuggestion> {
    let Some(bookmarks) = ctx.bookmarks else {
      return Vec::new();
    };

    let tokens: Vec<&str> = input.split_whitespace().filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() {
      return Vec::new();
    }

    let mut out = Vec::new();
    'nodes: for node in bookmarks.nodes.values() {
      let BookmarkNode::Bookmark(entry) = node else {
        continue;
      };

      let url = entry.url.trim();
      if url.is_empty() {
        continue;
      }

      let title = entry
        .title
        .as_deref()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty());

      for token in &tokens {
        if !contains_case_insensitive(url, token) && !title.is_some_and(|t| contains_case_insensitive(t, token)) {
          continue 'nodes;
        }
      }

      let url_owned = url.to_string();
      out.push(OmniboxSuggestion {
        action: OmniboxAction::NavigateToUrl(url_owned.clone()),
        title: title.map(|t| t.to_string()),
        url: Some(url_owned),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark),
      });
    }
    out
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
pub const DEFAULT_OMNIBOX_LIMIT: usize = 10;

/// Build omnibox suggestions for `input` using the default provider set, capped at `limit`.
pub fn build_omnibox_suggestions(
  ctx: &OmniboxContext<'_>,
  input: &str,
  limit: usize,
) -> Vec<OmniboxSuggestion> {
  build_omnibox_suggestions_with_providers(ctx, input, limit, default_providers())
}

/// Build omnibox suggestions using [`DEFAULT_OMNIBOX_LIMIT`].
pub fn build_omnibox_suggestions_default_limit(
  ctx: &OmniboxContext<'_>,
  input: &str,
) -> Vec<OmniboxSuggestion> {
  build_omnibox_suggestions(ctx, input, DEFAULT_OMNIBOX_LIMIT)
}

fn default_providers() -> Vec<Box<dyn OmniboxProvider>> {
  vec![
    Box::new(PrimaryActionProvider),
    Box::new(OpenTabsProvider),
    Box::new(AboutPagesProvider),
    Box::new(ClosedTabsProvider),
    Box::new(VisitedProvider),
    Box::new(BookmarksProvider),
    Box::new(RemoteSearchSuggestProvider),
  ]
}

fn build_omnibox_suggestions_with_providers(
  ctx: &OmniboxContext<'_>,
  input: &str,
  limit: usize,
  providers: Vec<Box<dyn OmniboxProvider>>,
) -> Vec<OmniboxSuggestion> {
  let input = input.trim();
  if input.is_empty() {
    return Vec::new();
  }
  let tokens_lower = tokenize_lower(input);
  if tokens_lower.is_empty() {
    return Vec::new();
  }

  let mut scored = Vec::<ScoredSuggestion>::new();
  for provider in providers {
    for suggestion in provider.suggestions(ctx, input) {
      let Some(score) = score_suggestion(&suggestion, &tokens_lower) else {
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

  if out.len() > limit {
    out.truncate(limit);
  }
  out
}

#[derive(Debug)]
struct ScoredSuggestion {
  suggestion: OmniboxSuggestion,
  score: i64,
}

fn score_suggestion(suggestion: &OmniboxSuggestion, tokens_lower: &[String]) -> Option<i64> {
  let base = match suggestion.source {
    OmniboxSuggestionSource::Primary => 1_000_000,
    // NOTE: Base scores are spaced far enough apart that match bonuses (see `match_score`) cannot
    // reorder the key source group we care about:
    // OpenTab > About > Bookmark > Visited.
    OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab) => 5_000,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::About) => 3_700,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark) => 2_400,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab) => 2_000,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited) => 1_000,
    OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest) => 500,
  };

  let mut match_total = 0i64;
  for token_lower in tokens_lower {
    let mut best_token_match = None::<i64>;

    if let Some(url) = suggestion.url.as_deref() {
      best_token_match = best_token_match.max(match_score(url, token_lower));
    }
    if let Some(title) = suggestion.title.as_deref() {
      best_token_match = best_token_match.max(match_score(title, token_lower));
    }
    // Search suggestions match against the query text.
    if let OmniboxAction::Search(query) = &suggestion.action {
      best_token_match = best_token_match.max(match_score(query, token_lower));
    }

    match_total += best_token_match?;
  }

  Some(base + match_total)
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

fn compare_scored_suggestions(a: &ScoredSuggestion, b: &ScoredSuggestion) -> Ordering {
  // Primary sort: score descending.
  match b.score.cmp(&a.score) {
    Ordering::Equal => {}
    ord => return ord,
  }

  // Secondary: source, consistent with base score (Primary > OpenTab > About > Bookmark >
  // ClosedTab > Visited > Search).
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
    OmniboxSuggestionSource::Primary => 6,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab) => 5,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::About) => 4,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark) => 3,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab) => 2,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited) => 1,
    OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest) => 0,
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

fn tokenize_lower(input: &str) -> Vec<String> {
  input
    .split_whitespace()
    .filter(|t| !t.is_empty())
    .map(|t| t.to_ascii_lowercase())
    .collect()
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
      pinned: false,
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
    let suggestions = build_omnibox_suggestions_default_limit(&ctx, "example");

    assert_eq!(
      suggestions,
      vec![
        OmniboxSuggestion {
          action: OmniboxAction::Search("example".to_string()),
          title: Some("example".to_string()),
          url: None,
          source: OmniboxSuggestionSource::Primary,
        },
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
      100,
      vec![
        Box::new(ProviderVisited),
        Box::new(PrimaryActionProvider),
        Box::new(ProviderOpenTab),
      ],
    );
    let b_first = build_omnibox_suggestions_with_providers(
      &ctx,
      "example",
      100,
      vec![
        Box::new(ProviderOpenTab),
        Box::new(ProviderVisited),
        Box::new(PrimaryActionProvider),
      ],
    );

    assert_eq!(a_first, b_first);
    assert_eq!(
      a_first,
      vec![
        OmniboxSuggestion {
          action: OmniboxAction::Search("example".to_string()),
          title: Some("example".to_string()),
          url: None,
          source: OmniboxSuggestionSource::Primary,
        },
        OmniboxSuggestion {
          action: OmniboxAction::ActivateTab(TabId(99)),
          title: Some("Example (tab)".to_string()),
          url: Some("https://example.com/".to_string()),
          source: OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab),
        }
      ]
    );
  }

  #[test]
  fn primary_action_is_first_and_uses_resolver() {
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

    let cats = build_omnibox_suggestions(&ctx, "cats", 10);
    assert!(
      cats.first().is_some_and(|s| s.source == OmniboxSuggestionSource::Primary),
      "expected a primary suggestion for non-empty input"
    );
    assert!(
      cats.iter().filter(|s| s.source == OmniboxSuggestionSource::Primary).count() == 1,
      "expected exactly one primary suggestion"
    );
    assert!(
      matches!(cats[0].action, OmniboxAction::Search(ref q) if q == "cats"),
      "expected primary action for `cats` to be Search"
    );

    let example = build_omnibox_suggestions(&ctx, "example.com", 10);
    assert!(
      matches!(example[0].action, OmniboxAction::NavigateToUrl(ref url) if url == "https://example.com/"),
      "expected primary action for `example.com` to be a navigation"
    );
  }

  #[test]
  fn about_pages_are_suggested() {
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

    let suggestions = build_omnibox_suggestions(&ctx, "about", 10);
    for url in [
      about_pages::ABOUT_NEWTAB,
      about_pages::ABOUT_HELP,
      about_pages::ABOUT_VERSION,
      about_pages::ABOUT_GPU,
    ] {
      assert!(
        suggestions.iter().any(|s| matches!(&s.action, OmniboxAction::NavigateToUrl(u) if u == url)),
        "expected suggestions for {url}"
      );
    }

    let suggestions = build_omnibox_suggestions(&ctx, "help", 10);
    assert!(
      suggestions
        .iter()
        .any(|s| matches!(&s.action, OmniboxAction::NavigateToUrl(u) if u == about_pages::ABOUT_HELP)),
      "expected about:help suggestion for input `help`"
    );
  }

  #[test]
  fn limit_is_enforced_after_dedup_and_deterministic() {
    struct ProviderA;
    impl OmniboxProvider for ProviderA {
      fn suggestions(&self, _ctx: &OmniboxContext<'_>, _input: &str) -> Vec<OmniboxSuggestion> {
        vec![
          // Duplicate URL should be deduped *before* limiting.
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl("https://a.com/".to_string()),
            title: Some("A2".to_string()),
            url: Some("https://a.com/".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl("https://a.com/".to_string()),
            title: Some("A1".to_string()),
            url: Some("https://a.com/".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl("https://b.com/".to_string()),
            title: None,
            url: Some("https://b.com/".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl("https://c.com/".to_string()),
            title: None,
            url: Some("https://c.com/".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
        ]
      }
    }

    struct ProviderB;
    impl OmniboxProvider for ProviderB {
      fn suggestions(&self, _ctx: &OmniboxContext<'_>, _input: &str) -> Vec<OmniboxSuggestion> {
        vec![
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl("https://c.com/".to_string()),
            title: None,
            url: Some("https://c.com/".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl("https://b.com/".to_string()),
            title: None,
            url: Some("https://b.com/".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl("https://a.com/".to_string()),
            title: Some("A1".to_string()),
            url: Some("https://a.com/".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
        ]
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

    // Input chosen so all URLs match with the same match score (prefix match on "https").
    let a = build_omnibox_suggestions_with_providers(
      &ctx,
      "https",
      3,
      vec![Box::new(PrimaryActionProvider), Box::new(ProviderA)],
    );
    let b = build_omnibox_suggestions_with_providers(
      &ctx,
      "https",
      3,
      vec![Box::new(PrimaryActionProvider), Box::new(ProviderB)],
    );

    assert_eq!(a, b);
    assert_eq!(a.len(), 3, "expected hard limit to be enforced");
    assert_eq!(a[0].source, OmniboxSuggestionSource::Primary);
    assert_eq!(
      a[1].url.as_deref(),
      Some("https://a.com/"),
      "expected lexicographic ordering for stable suggestions"
    );
    assert_eq!(
      a[2].url.as_deref(),
      Some("https://b.com/"),
      "expected dedup to run before truncation"
    );
  }

  #[test]
  fn bookmarks_are_suggested_and_ranked_above_visited_below_open_tabs() {
    let tab_id = TabId(1);
    let open_tabs = vec![BrowserTabState::new(
      tab_id,
      format!("https://{}/needle", "a".repeat(260)),
    )];
    let closed_tabs = Vec::new();

    let mut visited = VisitedUrlStore::new();
    visited.record_visit(
      "https://visited.example/".to_string(),
      Some("Needle Title".to_string()),
    );

    let bookmarks = BookmarkStore {
      urls: ["https://needle.example/".to_string()].into_iter().collect(),
    };
    let ctx = OmniboxContext {
      open_tabs: &open_tabs,
      closed_tabs: &closed_tabs,
      visited: &visited,
      active_tab_id: None,
      bookmarks: Some(&bookmarks),
      remote_search_suggest: None,
    };

    let suggestions = build_omnibox_suggestions(&ctx, "Needle", 10);

    assert_eq!(suggestions.len(), 4, "unexpected suggestions: {suggestions:?}");
    assert_eq!(suggestions[0].source, OmniboxSuggestionSource::Primary);
    assert_eq!(
      suggestions[1].source,
      OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab)
    );
    assert_eq!(
      suggestions[2].source,
      OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark)
    );
    assert_eq!(
      suggestions[2].url.as_deref(),
      Some("https://needle.example/")
    );
    assert_eq!(
      suggestions[3].source,
      OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited)
    );
  }

  #[test]
  fn bookmarks_are_suggested_for_matching_input() {
    let open_tabs = Vec::new();
    let closed_tabs = Vec::new();
    let visited = VisitedUrlStore::new();
    let bookmarks = BookmarkStore {
      urls: ["https://example.com/bookmark".to_string()]
        .into_iter()
        .collect(),
    };
    let ctx = OmniboxContext {
      open_tabs: &open_tabs,
      closed_tabs: &closed_tabs,
      visited: &visited,
      active_tab_id: None,
      bookmarks: Some(&bookmarks),
      remote_search_suggest: None,
    };

    let suggestions = build_omnibox_suggestions_default_limit(&ctx, "exam");
    assert!(
      suggestions.iter().any(|s| {
        s.source == OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark)
          && s.url.as_deref() == Some("https://example.com/bookmark")
          && matches!(s.action, OmniboxAction::NavigateToUrl(ref u) if u == "https://example.com/bookmark")
      }),
      "expected bookmark suggestion, got {suggestions:?}"
    );
  }

  #[test]
  fn bookmark_matching_is_tokenized_and_case_insensitive() {
    let open_tabs = Vec::new();
    let closed_tabs = Vec::new();
    let visited = VisitedUrlStore::new();
    let bookmarks = BookmarkStore {
      urls: [
        "https://www.rust-lang.org/learn".to_string(),
        "https://example.com/only-one-token".to_string(),
      ]
      .into_iter()
      .collect(),
    };
    let ctx = OmniboxContext {
      open_tabs: &open_tabs,
      closed_tabs: &closed_tabs,
      visited: &visited,
      active_tab_id: None,
      bookmarks: Some(&bookmarks),
      remote_search_suggest: None,
    };

    let suggestions = build_omnibox_suggestions_default_limit(&ctx, "RUST lang");
    assert!(
      suggestions.iter().any(|s| {
        s.source == OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark)
          && s.url.as_deref() == Some("https://www.rust-lang.org/learn")
      }),
      "expected rust-lang bookmark suggestion, got {suggestions:?}"
    );

    assert!(
      !suggestions.iter().any(|s| {
        s.source == OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark)
          && s.url.as_deref() == Some("https://example.com/only-one-token")
      }),
      "expected tokenized match to filter out non-matching bookmarks, got {suggestions:?}"
    );
  }

  #[test]
  fn engine_dedupes_bookmark_and_open_tab_suggestions() {
    let tab_id = TabId(1);
    let open_tabs = vec![BrowserTabState::new(
      tab_id,
      "https://example.com/".to_string(),
    )];
    let closed_tabs = Vec::new();
    let visited = VisitedUrlStore::new();
    let bookmarks = BookmarkStore {
      urls: ["https://example.com/".to_string()].into_iter().collect(),
    };
    let ctx = OmniboxContext {
      open_tabs: &open_tabs,
      closed_tabs: &closed_tabs,
      visited: &visited,
      active_tab_id: None,
      bookmarks: Some(&bookmarks),
      remote_search_suggest: None,
    };

    let suggestions = build_omnibox_suggestions_with_providers(
      &ctx,
      "example",
      50,
      vec![
        Box::new(PrimaryActionProvider),
        Box::new(OpenTabsProvider),
        Box::new(BookmarksProvider),
      ],
    );

    let matching = suggestions
      .iter()
      .filter(|s| s.url.as_deref() == Some("https://example.com/"))
      .collect::<Vec<_>>();
    assert_eq!(
      matching.len(),
      1,
      "expected exactly one suggestion for https://example.com/, got {suggestions:?}"
    );
    assert_eq!(
      matching[0].source,
      OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab),
      "expected open-tab suggestion to win dedup over bookmark, got {suggestions:?}"
    );
  }
}
