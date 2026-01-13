use crate::ui::about_pages;
use crate::ui::browser_app::{BrowserTabState, ClosedTabState, RemoteSearchSuggestCache};
use crate::ui::messages::TabId;
use crate::ui::url::{resolve_omnibox_input, resolve_omnibox_search_query, OmniboxInputResolution};
use crate::ui::visited::{VisitedUrlRecord, VisitedUrlStore};
use crate::ui::{BookmarkNode, BookmarkStore};
use super::string_match::find_ascii_case_insensitive;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use publicsuffix::{List, Psl};
use url::Url;

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

static PRIMARY_ACTION_PROVIDER: PrimaryActionProvider = PrimaryActionProvider;
static REMOTE_SEARCH_SUGGEST_PROVIDER: RemoteSearchSuggestProvider = RemoteSearchSuggestProvider;
static OPEN_TABS_PROVIDER: OpenTabsProvider = OpenTabsProvider;
static ABOUT_PAGES_PROVIDER: AboutPagesProvider = AboutPagesProvider;
static CLOSED_TABS_PROVIDER: ClosedTabsProvider = ClosedTabsProvider;
static VISITED_PROVIDER: VisitedProvider = VisitedProvider;
static BOOKMARKS_PROVIDER: BookmarksProvider = BookmarksProvider;

/// Default provider set used by the omnibox suggestion engine.
///
/// Stored as a static slice so building suggestions (a hot per-keystroke path) does not allocate
/// trait objects each time.
static DEFAULT_PROVIDERS: [&(dyn OmniboxProvider + Sync); 7] = [
  &PRIMARY_ACTION_PROVIDER,
  &REMOTE_SEARCH_SUGGEST_PROVIDER,
  &OPEN_TABS_PROVIDER,
  &ABOUT_PAGES_PROVIDER,
  &CLOSED_TABS_PROVIDER,
  &VISITED_PROVIDER,
  &BOOKMARKS_PROVIDER,
];

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

    // Hot path: most omnibox input is a plain search query; avoid building a full search URL when
    // we only need the query string.
    let suggestion = if let Some(query) = resolve_omnibox_search_query(input) {
      let query = query.to_string();
      OmniboxSuggestion {
        // Store the raw query in the action; navigation code is responsible for resolving it into
        // a concrete search engine URL.
        action: OmniboxAction::Search(query.clone()),
        title: Some(query),
        url: None,
        source: OmniboxSuggestionSource::Primary,
      }
    } else {
      match resolve_omnibox_input(input) {
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
      }
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
      (about_pages::ABOUT_HISTORY, "History"),
      (about_pages::ABOUT_BOOKMARKS, "Bookmarks"),
      (about_pages::ABOUT_HELP, "Help"),
      (about_pages::ABOUT_VERSION, "Version"),
      (about_pages::ABOUT_GPU, "GPU"),
      (about_pages::ABOUT_PROCESSES, "Processes"),
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

    // Cap the number of bookmark entries we consider per query so omnibox completion stays cheap.
    const BOOKMARK_SCAN_LIMIT: usize = 500;

    let matches = bookmarks.search(input, BOOKMARK_SCAN_LIMIT);
    if matches.is_empty() {
      return Vec::new();
    }

    let mut out = Vec::new();
    let mut seen_urls: HashSet<String> = HashSet::new();

    for id in matches {
      let Some(BookmarkNode::Bookmark(entry)) = bookmarks.nodes.get(&id) else {
        continue;
      };

      let url = entry.url.trim();
      if url.is_empty() {
        continue;
      }

      // Avoid suggesting the same URL multiple times when the bookmark store contains duplicates
      // (possible via import).
      if !seen_urls.insert(url.to_ascii_lowercase()) {
        continue;
      }

      let title = entry
        .title
        .as_deref()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty());

      let url_owned = url.to_string();
      out.push(OmniboxSuggestion {
        action: OmniboxAction::NavigateToUrl(url_owned.clone()),
        title: title
          .map(|t| t.to_string())
          .or_else(|| Some(url_owned.clone())),
        url: Some(url_owned),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark),
      });
    }

    out
  }
}

/// Cached remote (network) search query suggestions.
pub struct RemoteSearchSuggestProvider;

impl OmniboxProvider for RemoteSearchSuggestProvider {
  fn suggestions(&self, ctx: &OmniboxContext<'_>, input: &str) -> Vec<OmniboxSuggestion> {
    let Some(query) = resolve_omnibox_search_query(input) else {
      return Vec::new();
    };

    let Some(cache) = ctx.remote_search_suggest else {
      return Vec::new();
    };
    if cache.query != query {
      return Vec::new();
    }

    let mut out = Vec::new();
    for s in &cache.suggestions {
      let trimmed = s.trim();
      if trimmed.is_empty() {
        continue;
      }
      if trimmed.eq_ignore_ascii_case(query) {
        continue;
      }

      // Best-effort de-dupe (case-insensitive).
      if out
        .iter()
        .any(|existing: &OmniboxSuggestion| match &existing.action {
          OmniboxAction::Search(existing_q) => existing_q.eq_ignore_ascii_case(trimmed),
          _ => false,
        })
      {
        continue;
      }

      out.push(OmniboxSuggestion {
        action: OmniboxAction::Search(trimmed.to_string()),
        title: None,
        url: None,
        source: OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest),
      });
    }
    out
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
  build_omnibox_suggestions_with_provider_iter_at_time(
    ctx,
    input,
    limit,
    DEFAULT_PROVIDERS.iter().copied(),
    SystemTime::now(),
  )
}

/// Build omnibox suggestions using [`DEFAULT_OMNIBOX_LIMIT`].
pub fn build_omnibox_suggestions_default_limit(
  ctx: &OmniboxContext<'_>,
  input: &str,
) -> Vec<OmniboxSuggestion> {
  build_omnibox_suggestions(ctx, input, DEFAULT_OMNIBOX_LIMIT)
}

#[cfg(test)]
fn default_providers_boxed() -> Vec<Box<dyn OmniboxProvider + Sync>> {
  vec![
    Box::new(PrimaryActionProvider),
    Box::new(RemoteSearchSuggestProvider),
    Box::new(OpenTabsProvider),
    Box::new(AboutPagesProvider),
    Box::new(ClosedTabsProvider),
    Box::new(VisitedProvider),
    Box::new(BookmarksProvider),
  ]
}

#[cfg(test)]
fn build_omnibox_suggestions_with_providers(
  ctx: &OmniboxContext<'_>,
  input: &str,
  limit: usize,
  providers: Vec<Box<dyn OmniboxProvider + Sync>>,
) -> Vec<OmniboxSuggestion> {
  build_omnibox_suggestions_with_providers_at_time(ctx, input, limit, providers, SystemTime::now())
}

#[cfg(test)]
fn build_omnibox_suggestions_with_providers_at_time(
  ctx: &OmniboxContext<'_>,
  input: &str,
  limit: usize,
  providers: Vec<Box<dyn OmniboxProvider + Sync>>,
  now: SystemTime,
) -> Vec<OmniboxSuggestion> {
  build_omnibox_suggestions_with_provider_iter_at_time(
    ctx,
    input,
    limit,
    providers.iter().map(|p| p.as_ref()),
    now,
  )
}

fn build_omnibox_suggestions_with_provider_iter_at_time<'a>(
  ctx: &OmniboxContext<'_>,
  input: &str,
  limit: usize,
  providers: impl IntoIterator<Item = &'a (dyn OmniboxProvider + Sync)>,
  now: SystemTime,
) -> Vec<OmniboxSuggestion> {
  let input = input.trim();
  if input.is_empty() || limit == 0 {
    return Vec::new();
  }
  let tokens_lower = tokenize_lower(input);
  if tokens_lower.is_empty() {
    return Vec::new();
  }

  // We only ever return the top `limit` suggestions. Keeping a bounded working set avoids the
  // `O(n log n)` sort of potentially large provider outputs (visited/history/bookmarks), reducing
  // per-keystroke omnibox overhead.
  let mut selected = Vec::<ScoredSuggestion>::new();
  for provider in providers {
    for suggestion in provider.suggestions(ctx, input) {
      let Some(score) = score_suggestion(ctx, now, &suggestion, &tokens_lower) else {
        continue;
      };

      // Fast prune: once we have `limit` suggestions, any candidate with a lower score than the
      // current minimum cannot be part of the top set (score is the primary sort key).
      if selected.len() == limit {
        if let Some(min_score) = selected.iter().map(|s| s.score).min() {
          if score < min_score {
            continue;
          }
        }
      }

      let primary_raw = suggestion_primary_key_raw(&suggestion);

      // De-dupe by primary key (case-insensitive), matching the previous behaviour of using
      // `to_ascii_lowercase` keys.
      if let Some(existing_idx) = selected
        .iter()
        .position(|s| primary_raw.eq_ignore_ascii_case(&s.sort_key.primary))
      {
        if score < selected[existing_idx].score {
          continue;
        }

        let candidate = ScoredSuggestion {
          sort_key: suggestion_sort_key(&suggestion),
          suggestion,
          score,
        };
        if compare_scored_suggestions(&candidate, &selected[existing_idx]) == Ordering::Less {
          selected[existing_idx] = candidate;
        }
        continue;
      }

      let candidate = ScoredSuggestion {
        sort_key: suggestion_sort_key(&suggestion),
        suggestion,
        score,
      };

      if selected.len() < limit {
        selected.push(candidate);
        continue;
      }

      let worst_idx = worst_scored_suggestion_index(&selected);
      if compare_scored_suggestions(&candidate, &selected[worst_idx]) == Ordering::Less {
        selected[worst_idx] = candidate;
      }
    }
  }
  selected.sort_by(compare_scored_suggestions);
  selected.into_iter().map(|s| s.suggestion).collect()
}

#[derive(Debug)]
struct ScoredSuggestion {
  sort_key: SuggestionSortKey,
  suggestion: OmniboxSuggestion,
  score: i64,
}

fn score_suggestion(
  ctx: &OmniboxContext<'_>,
  now: SystemTime,
  suggestion: &OmniboxSuggestion,
  tokens_lower: &[String],
) -> Option<i64> {
  let base = match suggestion.source {
    OmniboxSuggestionSource::Primary => 1_000_000,
    OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest) => 10_000,
    // NOTE: Base scores are spaced far enough apart that match bonuses (see `match_score`) cannot
    // reorder the key source group we care about.
    OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab) => 5_000,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::About) => 3_700,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark) => 2_400,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab) => 2_000,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited) => 1_000,
  };

  // Non-blocking local frecency bonus for URL suggestions.
  //
  // This is intentionally capped so the base score ordering remains stable:
  // OpenTab > About > Bookmark > Visited.
  let frecency = match suggestion.source {
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited)
    | OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark) => suggestion
      .url
      .as_deref()
      .and_then(|url| ctx.visited.get(url))
      .map(|record| frecency_bonus(suggestion.source, record, now))
      .unwrap_or(0),
    _ => 0,
  };

  let mut match_total = 0i64;
  let parsed_url = suggestion.url.as_deref().and_then(|u| Url::parse(u).ok());

  for token_lower in tokens_lower {
    let mut best_token_match = None::<i64>;

    if let Some(url) = suggestion.url.as_deref() {
      best_token_match =
        best_token_match.max(match_score_url(parsed_url.as_ref(), url, token_lower));
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

  Some(base + match_total + frecency)
}

/// Returns a score for `needle` in `haystack`, where larger is better.
fn match_score(haystack: &str, needle_lower: &str) -> Option<i64> {
  let idx = find_ascii_case_insensitive(haystack, needle_lower)? as i64;

  // Prefer prefix matches and earlier matches. Clamp so long strings don't overflow.
  let prefix_bonus = if idx == 0 { 1_000 } else { 0 };
  let position_bonus = (200 - idx).max(0);
  Some(prefix_bonus + position_bonus)
}

fn match_score_url(parsed: Option<&Url>, raw: &str, needle_lower: &str) -> Option<i64> {
  let raw_score = match_score(raw, needle_lower);

  let Some(url) = parsed else {
    return raw_score;
  };
  if !matches!(url.scheme(), "http" | "https") {
    return raw_score;
  }

  let Some(host) = url.host_str() else {
    return raw_score;
  };

  let host_score = match_score_http_host(host, needle_lower);

  // Score path + query, but keep it lower than host matches.
  let path_score = match_score_pathish(url.path(), needle_lower);
  let query_score = url
    .query()
    .and_then(|q| match_score_pathish(q, needle_lower));
  let path_query_score = path_score.max(query_score);

  raw_score.max(host_score).max(path_query_score)
}

fn match_score_http_host(host: &str, needle_lower: &str) -> Option<i64> {
  let idx = find_ascii_case_insensitive(host, needle_lower)? as i64;

  let domain_start = registrable_domain(host)
    .and_then(|domain| host.len().checked_sub(domain.len()))
    .unwrap_or(0) as i64;

  let boundary_bonus = if idx == 0 || idx == domain_start {
    300
  } else if idx > 0 && host.as_bytes().get(idx as usize - 1) == Some(&b'.') {
    250
  } else {
    0
  };
  let position_bonus = (200 - idx).max(0);

  // Ensure host matches always outrank path/query matches.
  const HOST_BASE: i64 = 700;
  Some((HOST_BASE + boundary_bonus + position_bonus).min(1_200))
}

fn match_score_pathish(haystack: &str, needle_lower: &str) -> Option<i64> {
  let idx = find_ascii_case_insensitive(haystack, needle_lower)? as i64;
  let boundary_bonus = if idx == 0 {
    200
  } else {
    match haystack.as_bytes().get(idx as usize - 1) {
      Some(b'/') | Some(b'?') | Some(b'&') | Some(b'=') | Some(b'.') | Some(b'-') | Some(b'_') => {
        200
      }
      _ => 0,
    }
  };
  let position_bonus = (200 - idx).max(0);
  Some((boundary_bonus + position_bonus).min(600))
}

fn frecency_bonus(
  source: OmniboxSuggestionSource,
  record: &VisitedUrlRecord,
  now: SystemTime,
) -> i64 {
  let age = now
    .duration_since(record.last_visited)
    .unwrap_or(Duration::ZERO);

  let recency_bonus = if age < Duration::from_secs(60 * 60) {
    80
  } else if age < Duration::from_secs(60 * 60 * 24) {
    50
  } else if age < Duration::from_secs(60 * 60 * 24 * 7) {
    20
  } else {
    0
  };

  let frequency_bonus = if record.visit_count <= 1 {
    0
  } else {
    let log2 = 31 - record.visit_count.leading_zeros() as i64;
    (log2 * 10).min(70)
  };

  let bonus = recency_bonus + frequency_bonus;
  let cap = match source {
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark) => 90,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited) => 150,
    _ => 0,
  };
  bonus.min(cap)
}

fn registrable_domain(host: &str) -> Option<&str> {
  static PSL: OnceLock<List> = OnceLock::new();
  let list = PSL.get_or_init(List::default);
  let domain = list.domain(host.as_bytes())?;
  std::str::from_utf8(domain.as_bytes()).ok()
}

fn compare_scored_suggestions(a: &ScoredSuggestion, b: &ScoredSuggestion) -> Ordering {
  // Primary sort: score descending.
  match b.score.cmp(&a.score) {
    Ordering::Equal => {}
    ord => return ord,
  }

  // Secondary: source, consistent with base score
  // (Primary > RemoteSuggest > OpenTab > About > Bookmark > ClosedTab > Visited).
  match suggestion_source_rank(b.suggestion.source)
    .cmp(&suggestion_source_rank(a.suggestion.source))
  {
    Ordering::Equal => {}
    ord => return ord,
  }

  // Tertiary: prefer URL/title lexicographically for deterministic ordering (independent of
  // provider order).
  a.sort_key.cmp(&b.sort_key)
}

fn worst_scored_suggestion_index(scored: &[ScoredSuggestion]) -> usize {
  debug_assert!(!scored.is_empty());
  let mut worst_idx = 0usize;
  for i in 1..scored.len() {
    if compare_scored_suggestions(&scored[i], &scored[worst_idx]) == Ordering::Greater {
      worst_idx = i;
    }
  }
  worst_idx
}

fn suggestion_source_rank(source: OmniboxSuggestionSource) -> i64 {
  match source {
    OmniboxSuggestionSource::Primary => 6,
    OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest) => 5,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab) => 4,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::About) => 3,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark) => 2,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab) => 1,
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited) => 0,
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

fn suggestion_primary_key_raw(s: &OmniboxSuggestion) -> &str {
  match &s.action {
    OmniboxAction::ActivateTab(_) => s.url.as_deref().unwrap_or_default(),
    OmniboxAction::NavigateToUrl(url) => url.as_str(),
    OmniboxAction::Search(query) => query.as_str(),
  }
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
  use std::time::SystemTime;

  #[test]
  fn engine_produces_expected_local_suggestions() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);

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
    let mut history = crate::ui::GlobalHistoryStore::default();
    history.entries = vec![
      crate::ui::GlobalHistoryEntry {
        url: "https://example.net/".to_string(),
        title: Some("Example Net".to_string()),
        visited_at_ms: 1_000,
        visit_count: 1,
      },
      // Duplicate URL from open tab should be deduped in favour of the open-tab suggestion.
      crate::ui::GlobalHistoryEntry {
        url: "https://example.com/".to_string(),
        title: Some("Example Domain (history)".to_string()),
        visited_at_ms: 2_000,
        visit_count: 1,
      },
    ];
    visited.seed_from_global_history(&history);

    let ctx = OmniboxContext {
      open_tabs: &open_tabs,
      closed_tabs: &closed_tabs,
      visited: &visited,
      active_tab_id: Some(tab_b),
      bookmarks: None,
      remote_search_suggest: None,
    };
    let suggestions = build_omnibox_suggestions_with_providers_at_time(
      &ctx,
      "example",
      DEFAULT_OMNIBOX_LIMIT,
      default_providers_boxed(),
      now,
    );

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
  fn remote_suggestions_are_ranked_between_primary_and_local_matches() {
    let tab_a = TabId(1);
    let mut open_tabs = Vec::new();
    let mut a = BrowserTabState::new(tab_a, "https://example.com/".to_string());
    a.title = Some("Example Domain".to_string());
    open_tabs.push(a);

    let closed_tabs = Vec::new();
    let visited = VisitedUrlStore::new();

    let remote_cache = RemoteSearchSuggestCache {
      query: "example".to_string(),
      // Include the raw query; provider should filter it out.
      suggestions: vec![
        "example".to_string(),
        "example one".to_string(),
        "example two".to_string(),
      ],
      fetched_at: SystemTime::UNIX_EPOCH,
    };

    let ctx = OmniboxContext {
      open_tabs: &open_tabs,
      closed_tabs: &closed_tabs,
      visited: &visited,
      active_tab_id: None,
      bookmarks: None,
      remote_search_suggest: Some(&remote_cache),
    };

    let suggestions = build_omnibox_suggestions(&ctx, "example", 10);
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
          action: OmniboxAction::Search("example one".to_string()),
          title: None,
          url: None,
          source: OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest),
        },
        OmniboxSuggestion {
          action: OmniboxAction::Search("example two".to_string()),
          title: None,
          url: None,
          source: OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest),
        },
        OmniboxSuggestion {
          action: OmniboxAction::ActivateTab(tab_a),
          title: Some("Example Domain".to_string()),
          url: Some("https://example.com/".to_string()),
          source: OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab),
        },
      ]
    );
  }

  #[test]
  fn remote_suggestions_are_ignored_when_cache_query_does_not_match() {
    let open_tabs = Vec::new();
    let closed_tabs = Vec::new();
    let visited = VisitedUrlStore::new();

    let remote_cache = RemoteSearchSuggestCache {
      query: "other".to_string(),
      suggestions: vec!["other one".to_string()],
      fetched_at: SystemTime::UNIX_EPOCH,
    };

    let ctx = OmniboxContext {
      open_tabs: &open_tabs,
      closed_tabs: &closed_tabs,
      visited: &visited,
      active_tab_id: None,
      bookmarks: None,
      remote_search_suggest: Some(&remote_cache),
    };

    let suggestions = build_omnibox_suggestions(&ctx, "example", 10);
    assert!(
      !suggestions
        .iter()
        .any(|s| s.source == OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest)),
      "expected no remote suggestions when cache query mismatches; got {suggestions:?}"
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
      cats
        .first()
        .is_some_and(|s| s.source == OmniboxSuggestionSource::Primary),
      "expected a primary suggestion for non-empty input"
    );
    assert!(
      cats
        .iter()
        .filter(|s| s.source == OmniboxSuggestionSource::Primary)
        .count()
        == 1,
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
      about_pages::ABOUT_HISTORY,
      about_pages::ABOUT_BOOKMARKS,
      about_pages::ABOUT_HELP,
      about_pages::ABOUT_VERSION,
      about_pages::ABOUT_GPU,
    ] {
      assert!(
        suggestions
          .iter()
          .any(|s| matches!(&s.action, OmniboxAction::NavigateToUrl(u) if u == url)),
        "expected suggestions for {url}"
      );
    }

    let suggestions = build_omnibox_suggestions(&ctx, "help", 10);
    assert!(
      suggestions.iter().any(
        |s| matches!(&s.action, OmniboxAction::NavigateToUrl(u) if u == about_pages::ABOUT_HELP)
      ),
      "expected about:help suggestion for input `help`"
    );
  }

  #[test]
  fn about_pages_are_suggested_even_if_not_recorded_in_visited_history() {
    let open_tabs = Vec::new();
    let closed_tabs = Vec::new();
    let mut visited = VisitedUrlStore::with_capacity(10);

    // Transient about:newtab should be filtered out of visited history…
    visited.record_visit(
      about_pages::ABOUT_NEWTAB.to_string(),
      Some("New Tab".to_string()),
    );
    assert_eq!(visited.len(), 0);

    let ctx = OmniboxContext {
      open_tabs: &open_tabs,
      closed_tabs: &closed_tabs,
      visited: &visited,
      active_tab_id: None,
      bookmarks: None,
      remote_search_suggest: None,
    };

    // …but it should still be suggested by the about-pages provider (independent of history).
    let suggestions = build_omnibox_suggestions(&ctx, "about:n", 10);
    assert!(
      suggestions.iter().any(|s| {
        matches!(&s.action, OmniboxAction::NavigateToUrl(u) if u == about_pages::ABOUT_NEWTAB)
          && s.source == OmniboxSuggestionSource::Url(OmniboxUrlSource::About)
      }),
      "expected about:newtab suggestion, got {suggestions:?}"
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
  fn visited_suggestions_prefer_frecency_over_lexicographic_order_when_scores_tie() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000);
    let now_ms = now
      .duration_since(SystemTime::UNIX_EPOCH)
      .unwrap()
      .as_millis() as u64;

    let mut history = crate::ui::GlobalHistoryStore::default();
    history.entries = vec![
      // Oldest, low frequency, but lexicographically earlier.
      crate::ui::GlobalHistoryEntry {
        url: "https://a.example.com/".to_string(),
        title: None,
        visited_at_ms: now_ms - 9 * 24 * 60 * 60 * 1_000,
        visit_count: 1,
      },
      // Recent and frequently visited, but lexicographically later.
      crate::ui::GlobalHistoryEntry {
        url: "https://b.example.com/".to_string(),
        title: None,
        visited_at_ms: now_ms - 30 * 60 * 1_000,
        visit_count: 64,
      },
    ];

    let mut visited = VisitedUrlStore::with_capacity(10);
    visited.seed_from_global_history(&history);

    let open_tabs = Vec::new();
    let closed_tabs = Vec::new();
    let ctx = OmniboxContext {
      open_tabs: &open_tabs,
      closed_tabs: &closed_tabs,
      visited: &visited,
      active_tab_id: None,
      bookmarks: None,
      remote_search_suggest: None,
    };

    let suggestions = build_omnibox_suggestions_with_providers_at_time(
      &ctx,
      "example",
      10,
      vec![Box::new(PrimaryActionProvider), Box::new(VisitedProvider)],
      now,
    );

    let visited_urls: Vec<&str> = suggestions
      .iter()
      .filter(|s| s.source == OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited))
      .filter_map(|s| s.url.as_deref())
      .collect();
    assert_eq!(
      visited_urls,
      vec!["https://b.example.com/", "https://a.example.com/"]
    );
  }

  #[test]
  fn host_matches_are_weighted_above_path_matches_for_urls() {
    struct Provider;
    impl OmniboxProvider for Provider {
      fn suggestions(&self, _ctx: &OmniboxContext<'_>, _input: &str) -> Vec<OmniboxSuggestion> {
        vec![
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl("https://example.com/path/git/one".to_string()),
            title: None,
            url: Some("https://example.com/path/git/one".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl("https://github.com/rust-lang/rust".to_string()),
            title: None,
            url: Some("https://github.com/rust-lang/rust".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
        ]
      }
    }

    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000);
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

    let suggestions = build_omnibox_suggestions_with_providers_at_time(
      &ctx,
      "git",
      10,
      vec![Box::new(Provider)],
      now,
    );

    let urls: Vec<&str> = suggestions
      .iter()
      .filter_map(|s| s.url.as_deref())
      .collect();
    assert_eq!(
      urls,
      vec![
        "https://github.com/rust-lang/rust",
        "https://example.com/path/git/one"
      ]
    );
  }

  #[test]
  fn bookmarks_are_suggested_and_ranked_above_visited_below_open_tabs() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);

    let tab_id = TabId(1);
    let open_tabs = vec![BrowserTabState::new(
      tab_id,
      format!("https://{}/needle", "a".repeat(260)),
    )];
    let closed_tabs = Vec::new();

    let mut visited = VisitedUrlStore::new();
    let mut history = crate::ui::GlobalHistoryStore::default();
    history.entries = vec![crate::ui::GlobalHistoryEntry {
      url: "https://visited.example/".to_string(),
      title: Some("Needle Title".to_string()),
      visited_at_ms: 1_000,
      visit_count: 1,
    }];
    visited.seed_from_global_history(&history);

    let mut bookmarks = BookmarkStore::default();
    bookmarks
      .add("https://needle.example/".to_string(), None, None)
      .expect("add bookmark");
    let ctx = OmniboxContext {
      open_tabs: &open_tabs,
      closed_tabs: &closed_tabs,
      visited: &visited,
      active_tab_id: None,
      bookmarks: Some(&bookmarks),
      remote_search_suggest: None,
    };

    let suggestions = build_omnibox_suggestions_with_providers_at_time(
      &ctx,
      "Needle",
      10,
      default_providers_boxed(),
      now,
    );

    assert_eq!(
      suggestions.len(),
      4,
      "unexpected suggestions: {suggestions:?}"
    );
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
    let mut bookmarks = BookmarkStore::default();
    bookmarks
      .add("https://example.com/bookmark".to_string(), None, None)
      .expect("add bookmark");
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
  fn bookmarks_match_on_title_and_use_title_in_suggestion() {
    let open_tabs = Vec::new();
    let closed_tabs = Vec::new();
    let visited = VisitedUrlStore::new();
    let mut bookmarks = BookmarkStore::default();
    bookmarks
      .add(
        "https://example.com/opaque".to_string(),
        Some("Learn Rust".to_string()),
        None,
      )
      .unwrap();
    let ctx = OmniboxContext {
      open_tabs: &open_tabs,
      closed_tabs: &closed_tabs,
      visited: &visited,
      active_tab_id: None,
      bookmarks: Some(&bookmarks),
      remote_search_suggest: None,
    };

    let suggestions = build_omnibox_suggestions_default_limit(&ctx, "rust");
    let bookmark = suggestions
      .iter()
      .find(|s| s.url.as_deref() == Some("https://example.com/opaque"))
      .expect("expected bookmark suggestion for title match");
    assert_eq!(
      bookmark.source,
      OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark)
    );
    assert_eq!(bookmark.title.as_deref(), Some("Learn Rust"));
  }

  #[test]
  fn bookmarks_inside_folders_are_suggested() {
    let open_tabs = Vec::new();
    let closed_tabs = Vec::new();
    let visited = VisitedUrlStore::new();
    let mut bookmarks = BookmarkStore::default();
    let folder = bookmarks.create_folder("Folder".to_string(), None).unwrap();
    bookmarks
      .add(
        "https://example.com/nested".to_string(),
        Some("Nested Bookmark".to_string()),
        Some(folder),
      )
      .unwrap();
    let ctx = OmniboxContext {
      open_tabs: &open_tabs,
      closed_tabs: &closed_tabs,
      visited: &visited,
      active_tab_id: None,
      bookmarks: Some(&bookmarks),
      remote_search_suggest: None,
    };

    let suggestions = build_omnibox_suggestions_default_limit(&ctx, "nested");
    assert!(
      suggestions.iter().any(|s| {
        s.source == OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark)
          && s.url.as_deref() == Some("https://example.com/nested")
      }),
      "expected nested bookmark suggestion, got {suggestions:?}"
    );
  }

  #[test]
  fn bookmark_matching_is_tokenized_and_case_insensitive() {
    let open_tabs = Vec::new();
    let closed_tabs = Vec::new();
    let visited = VisitedUrlStore::new();
    let mut bookmarks = BookmarkStore::default();
    bookmarks
      .add("https://www.rust-lang.org/learn".to_string(), None, None)
      .expect("add bookmark rust-lang");
    bookmarks
      .add("https://example.com/only-one-token".to_string(), None, None)
      .expect("add bookmark example");
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
    let mut bookmarks = BookmarkStore::default();
    bookmarks
      .add("https://example.com/".to_string(), None, None)
      .expect("add bookmark");
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
