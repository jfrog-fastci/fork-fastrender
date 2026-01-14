use super::string_match::{
  contains_ascii_case_insensitive, find_ascii_case_insensitive, AsciiCaseInsensitive,
<<<<<<< HEAD
=======
  AsciiCaseInsensitiveStr,
>>>>>>> ac5c2202c (fix: remove merge artifacts and restore build)
};
use crate::ui::about_pages;
use crate::ui::browser_app::{BrowserTabState, ClosedTabState, RemoteSearchSuggestCache};
use crate::ui::messages::TabId;
use crate::ui::url::{resolve_omnibox_input, resolve_omnibox_search_query, OmniboxInputResolution};
use crate::ui::visited::{VisitedUrlRecord, VisitedUrlStore};
use crate::ui::{BookmarkNode, BookmarkStore};
use memchr::{memchr, memchr2, memchr3, memrchr};
use rustc_hash::FxBuildHasher;
use smallvec::SmallVec;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use publicsuffix::{List, Psl};

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

  /// Upper bound on the best score this provider can produce for the current token count.
  ///
  /// This enables the omnibox engine to skip expensive providers (visited/bookmarks) once it has
  /// already found enough higher-scoring candidates to satisfy the output limit.
  fn max_score_upper_bound(&self, _token_count: usize) -> i64 {
    i64::MAX
  }
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
  NavigateToUrl,
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
          action: OmniboxAction::NavigateToUrl,
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

  fn max_score_upper_bound(&self, token_count: usize) -> i64 {
    1_000_000 + 1_200 * token_count as i64
  }
}

pub struct AboutPagesProvider;

impl OmniboxProvider for AboutPagesProvider {
  fn suggestions(&self, _ctx: &OmniboxContext<'_>, input: &str) -> Vec<OmniboxSuggestion> {
    let input = input.trim();
    if input.is_empty() {
      return Vec::new();
    }
    let input_lower: Cow<'_, str> = if input.as_bytes().iter().any(|b| b.is_ascii_uppercase()) {
      Cow::Owned(input.to_ascii_lowercase())
    } else {
      Cow::Borrowed(input)
    };

    let mut out = Vec::with_capacity(
      about_pages::user_facing_about_pages().len()
        + if input_lower.starts_with("about:test") {
          4
        } else {
          0
        },
    );
    for (url, title) in about_pages::user_facing_about_pages() {
      if !contains_ascii_case_insensitive(url, input_lower.as_ref())
        && !contains_ascii_case_insensitive(title, input_lower.as_ref())
      {
        continue;
      }

      out.push(OmniboxSuggestion {
        action: OmniboxAction::NavigateToUrl,
        title: Some((*title).to_string()),
        url: Some((*url).to_string()),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::About),
      });
    }

    // Test-only pages (`about:test-*`) are intentionally excluded from generic matching so that
    // typing "test" doesn't surface internal debug pages. If the user explicitly starts typing an
    // `about:test` URL, surface them as completions.
    if input_lower.starts_with("about:test") {
      const TEST_PAGES: &[(&str, &str)] = &[
        (about_pages::ABOUT_TEST_SCROLL, "Test Scroll"),
        (about_pages::ABOUT_TEST_HEAVY, "Test Heavy"),
        (about_pages::ABOUT_TEST_LAYOUT_STRESS, "Test Layout Stress"),
        (about_pages::ABOUT_TEST_FORM, "Test Form"),
      ];
      for (url, title) in TEST_PAGES {
        if !contains_ascii_case_insensitive(url, input_lower.as_ref())
          && !contains_ascii_case_insensitive(title, input_lower.as_ref())
        {
          continue;
        }

        out.push(OmniboxSuggestion {
          action: OmniboxAction::NavigateToUrl,
          title: Some((*title).to_string()),
          url: Some((*url).to_string()),
          source: OmniboxSuggestionSource::Url(OmniboxUrlSource::About),
        });
      }
    }

    out
  }

  fn max_score_upper_bound(&self, token_count: usize) -> i64 {
    3_700 + 1_200 * token_count as i64
  }
}

pub struct OpenTabsProvider;

impl OmniboxProvider for OpenTabsProvider {
  fn suggestions(&self, ctx: &OmniboxContext<'_>, _input: &str) -> Vec<OmniboxSuggestion> {
    let mut out = Vec::with_capacity(ctx.open_tabs.len());
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

  fn max_score_upper_bound(&self, token_count: usize) -> i64 {
    5_000 + 1_200 * token_count as i64
  }
}

pub struct ClosedTabsProvider;

impl OmniboxProvider for ClosedTabsProvider {
  fn suggestions(&self, ctx: &OmniboxContext<'_>, _input: &str) -> Vec<OmniboxSuggestion> {
    let mut out = Vec::with_capacity(ctx.closed_tabs.len());
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
        action: OmniboxAction::NavigateToUrl,
        title,
        url: Some(closed.url.clone()),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab),
      });
    }
    out
  }

  fn max_score_upper_bound(&self, token_count: usize) -> i64 {
    2_000 + 1_200 * token_count as i64
  }
}

pub struct VisitedProvider;

impl OmniboxProvider for VisitedProvider {
  fn suggestions(&self, ctx: &OmniboxContext<'_>, input: &str) -> Vec<OmniboxSuggestion> {
    // Cap the number of visited records we consider per query so omnibox completion stays cheap.
    const VISITED_LIMIT: usize = 200;

    let matches = ctx.visited.search(input, VISITED_LIMIT);
    let mut out = Vec::with_capacity(matches.len());
    for record in matches {
      let title = record
        .title
        .as_ref()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string());
      out.push(OmniboxSuggestion {
        action: OmniboxAction::NavigateToUrl,
        title,
        url: Some(record.url.clone()),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
      });
    }
    out
  }

  fn max_score_upper_bound(&self, token_count: usize) -> i64 {
    // Score = base (visited) + match_total + frecency (capped at 150).
    1_000 + 1_200 * token_count as i64 + 150
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

    let mut out = Vec::with_capacity(matches.len());
    let mut seen_urls: HashSet<AsciiCaseInsensitive<'_>, FxBuildHasher> =
      HashSet::with_capacity_and_hasher(matches.len(), FxBuildHasher::default());

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
      if !seen_urls.insert(AsciiCaseInsensitive(url)) {
        continue;
      }

      let title = entry
        .title
        .as_deref()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        // Avoid rendering the same URL twice (as both the title and the secondary URL text).
        .filter(|t| !t.eq_ignore_ascii_case(url));

      let url_owned = url.to_string();
      out.push(OmniboxSuggestion {
        action: OmniboxAction::NavigateToUrl,
        title: title.map(|t| t.to_string()),
        url: Some(url_owned),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark),
      });
    }

    out
  }

  fn max_score_upper_bound(&self, token_count: usize) -> i64 {
    // Score = base (bookmark) + match_total + frecency (capped at 90).
    2_400 + 1_200 * token_count as i64 + 90
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

    let mut out = Vec::with_capacity(cache.suggestions.len());
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

  fn max_score_upper_bound(&self, token_count: usize) -> i64 {
    10_000 + 1_200 * token_count as i64
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
  // Lowercase once and keep tokens as slices into the lowercased buffer so we don't allocate a
  // separate `String` per token on the hot per-keystroke path.
  //
  // Most omnibox input is already lowercase; avoid allocating unless needed.
  let input_lower: Cow<'_, str> = if input.as_bytes().iter().any(|b| b.is_ascii_uppercase()) {
    Cow::Owned(input.to_ascii_lowercase())
  } else {
    Cow::Borrowed(input)
  };
  let tokens_lower = tokenize_lower(input_lower.as_ref());
  if tokens_lower.is_empty() {
    return Vec::new();
  }
  let token_count = tokens_lower.len();

  // We only ever return the top `limit` suggestions. Keeping a bounded working set avoids the
  // `O(n log n)` sort of potentially large provider outputs (visited/history/bookmarks), reducing
  // per-keystroke omnibox overhead.
  let mut selected = Vec::<ScoredSuggestion>::with_capacity(limit);
  for provider in providers {
    // If we already have enough suggestions, and even the theoretical max score for this provider
    // cannot beat our current worst score, skip it entirely.
    if selected.len() == limit {
      if let Some(min_score) = selected.iter().map(|s| s.score).min() {
        if min_score > provider.max_score_upper_bound(token_count) {
          continue;
        }
      }
    }

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
        .position(|s| primary_raw.eq_ignore_ascii_case(suggestion_primary_key_raw(&s.suggestion)))
      {
        if score < selected[existing_idx].score {
          continue;
        }

        let candidate = ScoredSuggestion { suggestion, score };
        if compare_scored_suggestions(&candidate, &selected[existing_idx]) == Ordering::Less {
          selected[existing_idx] = candidate;
        }
        continue;
      }

      let candidate = ScoredSuggestion { suggestion, score };

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
  suggestion: OmniboxSuggestion,
  score: i64,
}

fn score_suggestion(
  ctx: &OmniboxContext<'_>,
  now: SystemTime,
  suggestion: &OmniboxSuggestion,
  tokens_lower: &[&str],
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
  const MAX_TOKEN_MATCH_SCORE: i64 = 1_200;
  let parsed_url = suggestion
    .url
    .as_deref()
    .and_then(parse_http_url_for_scoring);

  for &token_lower in tokens_lower {
    let mut best_token_match = None::<i64>;

    if let Some(url) = suggestion.url.as_deref() {
      best_token_match =
        best_token_match.max(match_score_url(parsed_url.as_ref(), url, token_lower));
      if best_token_match == Some(MAX_TOKEN_MATCH_SCORE) {
        match_total += MAX_TOKEN_MATCH_SCORE;
        continue;
      }
    }
    if let Some(title) = suggestion.title.as_deref() {
      best_token_match = best_token_match.max(match_score(title, token_lower));
      if best_token_match == Some(MAX_TOKEN_MATCH_SCORE) {
        match_total += MAX_TOKEN_MATCH_SCORE;
        continue;
      }
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

#[derive(Debug, Clone, Copy)]
struct OmniboxHttpUrl<'a> {
  host: &'a str,
  // Always has a leading `/`. If the raw URL has no explicit path, this is `/`.
  path: &'a str,
  // Excludes the leading `?`.
  query: Option<&'a str>,
}

/// Lightweight, allocation-free URL parser for omnibox scoring.
///
/// We only care about http(s) URLs, and we only need to extract the host, path, and query string
/// (for match weighting). Full RFC-compliant parsing is unnecessary here and `url::Url::parse`
/// is relatively expensive when the omnibox fanout is large (e.g. visited history provider).
fn parse_http_url_for_scoring(raw: &str) -> Option<OmniboxHttpUrl<'_>> {
  // Fast scheme classifier: only treat `http://` and `https://` as eligible for structured
  // scoring. Everything else falls back to raw substring scoring.
  let bytes = raw.as_bytes();
  let scheme_len = if bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"http://") {
    7
  } else if bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"https://") {
    8
  } else {
    return None;
  };

  let rest = &raw[scheme_len..];

  // Authority ends at the first `/`, `?`, or `#` (or end of string).
  let authority_end = memchr3(b'/', b'?', b'#', rest.as_bytes()).unwrap_or(rest.len());
  let authority = &rest[..authority_end];
  if authority.is_empty() {
    return None;
  }

  // Strip userinfo (`user:pass@`).
  let hostport = match memrchr(b'@', authority.as_bytes()) {
    Some(at) => &authority[at + 1..],
    None => authority,
  };
  if hostport.is_empty() {
    return None;
  }

  // Extract host and validate optional port.
  let host = if hostport.starts_with('[') {
    // IPv6 literal. Format: `[host]` or `[host]:port`.
    //
    // Note: `url::Url::host_str()` returns the bracketed form (`[::1]`), so we keep the brackets
    // here for scoring parity.
    let close = memchr(b']', hostport.as_bytes())?;
    if close <= 1 {
      return None;
    }

    let after = &hostport[close + 1..];
    if !after.is_empty() {
      if !after.starts_with(':') {
        return None;
      }
      if !is_valid_url_port(&after[1..]) {
        return None;
      }
    }

    &hostport[..(close + 1)]
  } else {
    match memchr(b':', hostport.as_bytes()) {
      Some(colon) => {
        let host = &hostport[..colon];
        let port = &hostport[colon + 1..];
        if host.is_empty() || !is_valid_url_port(port) {
          return None;
        }
        host
      }
      None => hostport,
    }
  };

  if host.is_empty() {
    return None;
  }

  // Remaining part begins with `/`, `?`, `#`, or is empty.
  let after_auth = &rest[authority_end..];

  // For `http(s)://host` URLs, `url::Url::path()` returns `/` even when no explicit path is
  // present. Mirror that behavior so scoring stays comparable.
  let mut path: &str = "/";
  let mut query: Option<&str> = None;

  if after_auth.starts_with('/') {
    // Path ends at `?` or `#`.
    let path_end = memchr2(b'?', b'#', after_auth.as_bytes()).unwrap_or(after_auth.len());
    path = &after_auth[..path_end];

    // Query (optional) begins after `?` and ends at `#` (or end).
    if after_auth.as_bytes().get(path_end) == Some(&b'?') {
      let query_start = path_end + 1;
      let query_end = query_start
        + memchr(b'#', &after_auth.as_bytes()[query_start..])
          .unwrap_or(after_auth.len() - query_start);
      query = Some(&after_auth[query_start..query_end]);
    }
  } else if after_auth.starts_with('?') {
    let query_start = 1;
    let query_end = query_start
      + memchr(b'#', &after_auth.as_bytes()[query_start..])
        .unwrap_or(after_auth.len() - query_start);
    query = Some(&after_auth[query_start..query_end]);
  }

  Some(OmniboxHttpUrl { host, path, query })
}

fn is_valid_url_port(port: &str) -> bool {
  // `url::Url` parses ports as `u16` (0-65535). Keep the validation lightweight and avoid
  // allocating or calling `str::parse`.
  if port.is_empty() {
    return false;
  }
  let mut value: u32 = 0;
  for b in port.as_bytes() {
    if !b.is_ascii_digit() {
      return false;
    }
    value = value * 10 + (*b - b'0') as u32;
    if value > 65_535 {
      return false;
    }
  }
  true
}

fn match_score_url(
  parsed: Option<&OmniboxHttpUrl<'_>>,
  raw: &str,
  needle_lower: &str,
) -> Option<i64> {
  let Some(url) = parsed else {
    return match_score(raw, needle_lower);
  };
  if raw
    .as_bytes()
    .get(..needle_lower.len())
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(needle_lower.as_bytes()))
  {
    // Prefix match is always the maximum possible score.
    return Some(1_200);
  }
  let host_score = match_score_http_host(url.host, needle_lower);
  if host_score == Some(1_200) {
    // Host-prefix matches are also the maximum possible score. Skip scanning the path/query.
    return host_score;
  }

  // Score path + query, but keep it lower than host matches.
  let path_score = match_score_pathish(url.path, needle_lower);
  let query_score = url.query.and_then(|q| match_score_pathish(q, needle_lower));
  let path_query_score = path_score.max(query_score);

  let best_structured = host_score.max(path_query_score);
  // If we have any structured match >= 199, a non-prefix raw match cannot beat it:
  // `match_score` without the prefix bonus is at most `200 - 1 = 199`.
  if matches!(best_structured, Some(score) if score >= 199) {
    return best_structured;
  }

  best_structured.max(match_score(raw, needle_lower))
}

fn match_score_http_host(host: &str, needle_lower: &str) -> Option<i64> {
  let idx = find_ascii_case_insensitive(host, needle_lower)? as i64;
  let boundary_bonus = if idx == 0 {
    300
  } else if host.as_bytes().get(idx as usize - 1) == Some(&b'.') {
    let bytes = host.as_bytes();
    // Matches at the start of the TLD label (after the last `.`) cannot be at the registrable-domain
    // boundary, so the PSL lookup is wasted work.
    let last_dot = memrchr(b'.', bytes).unwrap_or(idx as usize - 1);
    if idx as usize == last_dot + 1 {
      250
    } else {
      // Only compute the registrable-domain boundary when the match starts at a host label boundary.
      // For non-boundary matches, we cannot be at the domain boundary and the PSL lookup is wasted
      // work.
      let domain_start = registrable_domain(host)
        .and_then(|domain| host.len().checked_sub(domain.len()))
        .unwrap_or(0) as i64;
      if idx == domain_start {
        300
      } else {
        250
      }
    }
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
  compare_suggestion_sort_keys(&a.suggestion, &b.suggestion)
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

fn suggestion_primary_key_raw(s: &OmniboxSuggestion) -> &str {
  match &s.action {
    OmniboxAction::ActivateTab(_) | OmniboxAction::NavigateToUrl => {
      s.url.as_deref().unwrap_or_default()
    }
    OmniboxAction::Search(query) => query.as_str(),
  }
}

fn compare_suggestion_sort_keys(a: &OmniboxSuggestion, b: &OmniboxSuggestion) -> Ordering {
  let (a_primary, a_secondary, a_tab_id) = suggestion_sort_key_parts(a);
  let (b_primary, b_secondary, b_tab_id) = suggestion_sort_key_parts(b);

  match cmp_ascii_lowercase(a_primary, b_primary) {
    Ordering::Equal => {}
    ord => return ord,
  }
  match cmp_ascii_lowercase(a_secondary, b_secondary) {
    Ordering::Equal => {}
    ord => return ord,
  }
  a_tab_id.cmp(&b_tab_id)
}

fn suggestion_sort_key_parts(s: &OmniboxSuggestion) -> (&str, &str, u64) {
  let (primary, secondary, tab_id) = match &s.action {
    OmniboxAction::ActivateTab(tab_id) => (
      s.url.as_deref().unwrap_or_default(),
      s.title.as_deref().unwrap_or_default(),
      tab_id.0,
    ),
    OmniboxAction::NavigateToUrl => (
      s.url.as_deref().unwrap_or_default(),
      s.title.as_deref().unwrap_or_default(),
      0,
    ),
    OmniboxAction::Search(query) => (query.as_str(), "", 0),
  };

  (primary, secondary, tab_id)
}

fn cmp_ascii_lowercase(a: &str, b: &str) -> Ordering {
  for (&a_byte, &b_byte) in a.as_bytes().iter().zip(b.as_bytes().iter()) {
    let a_lower = a_byte.to_ascii_lowercase();
    let b_lower = b_byte.to_ascii_lowercase();
    match a_lower.cmp(&b_lower) {
      Ordering::Equal => {}
      ord => return ord,
    }
  }
  a.len().cmp(&b.len())
}

fn tokenize_lower<'a>(input_lower: &'a str) -> SmallVec<[&'a str; 4]> {
  input_lower.split_whitespace().collect()
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::SystemTime;
  use url::Url;

  fn match_score_url_via_url_crate(raw: &str, needle_lower: &str) -> Option<i64> {
    let raw_score = match_score(raw, needle_lower);
    let parsed = Url::parse(raw).ok();
    let Some(url) = parsed.as_ref() else {
      return raw_score;
    };
    if !matches!(url.scheme(), "http" | "https") {
      return raw_score;
    }
    let Some(host) = url.host_str() else {
      return raw_score;
    };

    let host_score = match_score_http_host(host, needle_lower);
    let path_score = match_score_pathish(url.path(), needle_lower);
    let query_score = url
      .query()
      .and_then(|q| match_score_pathish(q, needle_lower));
    let path_query_score = path_score.max(query_score);

    raw_score.max(host_score).max(path_query_score)
  }

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
          action: OmniboxAction::NavigateToUrl,
          title: Some("Example Org".to_string()),
          url: Some("https://example.org/".to_string()),
          source: OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab),
        },
        OmniboxSuggestion {
          action: OmniboxAction::NavigateToUrl,
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
          action: OmniboxAction::NavigateToUrl,
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
      matches!(example[0].action, OmniboxAction::NavigateToUrl)
        && example[0].url.as_deref() == Some("https://example.com/"),
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
      about_pages::ABOUT_SETTINGS,
      about_pages::ABOUT_HELP,
      about_pages::ABOUT_VERSION,
      about_pages::ABOUT_GPU,
      about_pages::ABOUT_PROCESSES,
    ] {
      assert!(
        suggestions.iter().any(
          |s| matches!(s.action, OmniboxAction::NavigateToUrl) && s.url.as_deref() == Some(url)
        ),
        "expected suggestions for {url}"
      );
    }

    let suggestions = build_omnibox_suggestions(&ctx, "help", 10);
    assert!(
      suggestions
        .iter()
        .any(|s| matches!(s.action, OmniboxAction::NavigateToUrl)
          && s.url.as_deref() == Some(about_pages::ABOUT_HELP)),
      "expected about:help suggestion for input `help`"
    );
  }

  #[test]
  fn about_settings_is_suggested_and_matching_is_case_insensitive() {
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

    let provider = AboutPagesProvider;
    let suggestions = provider.suggestions(&ctx, "settings");
    assert!(
      suggestions
        .iter()
        .any(|s| s.url.as_deref() == Some(about_pages::ABOUT_SETTINGS)),
      "expected about:settings suggestion, got {suggestions:?}"
    );

    let suggestions = provider.suggestions(&ctx, "SeTtInGs");
    assert!(
      suggestions
        .iter()
        .any(|s| s.url.as_deref() == Some(about_pages::ABOUT_SETTINGS)),
      "expected case-insensitive about:settings suggestion, got {suggestions:?}"
    );
  }

  #[test]
  fn about_test_pages_are_not_suggested_for_generic_queries() {
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

    let provider = AboutPagesProvider;
    let suggestions = provider.suggestions(&ctx, "test");
    assert!(
      !suggestions.iter().any(|s| s
        .url
        .as_deref()
        .is_some_and(|u| u.starts_with("about:test"))),
      "expected no about:test-* suggestions for input `test`, got {suggestions:?}"
    );

    // But if the user explicitly starts typing an `about:test` URL, include them as completions.
    let suggestions = provider.suggestions(&ctx, "about:test");
    assert!(
      suggestions
        .iter()
        .any(|s| s.url.as_deref() == Some(about_pages::ABOUT_TEST_SCROLL)),
      "expected about:test-* suggestions for input `about:test`, got {suggestions:?}"
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
        matches!(s.action, OmniboxAction::NavigateToUrl)
          && s.url.as_deref() == Some(about_pages::ABOUT_NEWTAB)
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
            action: OmniboxAction::NavigateToUrl,
            title: Some("A2".to_string()),
            url: Some("https://a.com/".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl,
            title: Some("A1".to_string()),
            url: Some("https://a.com/".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl,
            title: None,
            url: Some("https://b.com/".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl,
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
            action: OmniboxAction::NavigateToUrl,
            title: None,
            url: Some("https://c.com/".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl,
            title: None,
            url: Some("https://b.com/".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl,
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
            action: OmniboxAction::NavigateToUrl,
            title: None,
            url: Some("https://example.com/path/git/one".to_string()),
            source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
          },
          OmniboxSuggestion {
            action: OmniboxAction::NavigateToUrl,
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
  fn url_scoring_fast_path_matches_url_crate_for_common_urls() {
    // We intentionally avoid `url::Url::parse` in omnibox scoring for performance. This test keeps
    // the lightweight parser honest by comparing its scoring behavior to the previous
    // `Url::parse`-based implementation for a representative set of URLs.
    let cases: &[(&str, &[&str])] = &[
      (
        "https://example.com/path/to/page?query=one&two=three",
        &["example", "path", "query=one", "two=three", "/path", "/"],
      ),
      (
        "HTTP://Sub.Example.co.uk/a/b?c=d#frag",
        &["sub", "example", "co.uk", "/a", "c=d", "frag"],
      ),
      (
        "https://user:pass@example.com:8080/secure/path?token=ABC#ignored",
        &["user", "pass", "example.com", "8080", "secure", "token=abc"],
      ),
      (
        "https://[2001:db8::1]:443/ipv6/path?x=y",
        &["2001:db8::1", "443", "ipv6", "x=y"],
      ),
      ("https://example.com", &["example", "com", "/"]),
      ("https://example.com?only_query=1", &["only_query", "/"]),
      (
        "https://example.com/%7Euser?foo=bar%2Fbaz",
        &["%7euser", "bar%2fbaz"],
      ),
      // Non-http(s) schemes should fall back to raw scoring.
      ("ftp://example.com/path", &["example", "path"]),
      ("about:help", &["help"]),
      ("file:///Users/alice/test.txt", &["users", "test.txt"]),
      // Invalid ports should behave like the `url::Url::parse` fallback (raw scoring only).
      (
        "https://example.com:99999/path",
        &["example", "path", "99999"],
      ),
      ("https://[::1]bad/path", &["::1", "bad", "path"]),
    ];

    for (raw, needles) in cases {
      for needle in *needles {
        let needle_lower = needle.to_ascii_lowercase();

        let old_score = match_score_url_via_url_crate(raw, &needle_lower);

        let parsed = parse_http_url_for_scoring(raw);
        let new_score = match_score_url(parsed.as_ref(), raw, &needle_lower);

        assert_eq!(
          new_score, old_score,
          "score mismatch for raw={raw:?} needle={needle:?}: new={new_score:?} old={old_score:?}"
        );
      }
    }
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
          && matches!(s.action, OmniboxAction::NavigateToUrl)
      }),
      "expected bookmark suggestion, got {suggestions:?}"
    );
  }

  #[test]
  fn bookmarks_without_title_do_not_duplicate_url_as_title() {
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
    let bookmark = suggestions
      .iter()
      .find(|s| s.url.as_deref() == Some("https://example.com/bookmark"))
      .expect("expected bookmark suggestion");
    assert!(
      bookmark.title.is_none(),
      "expected untitled bookmark suggestion to have no title so the UI doesn't render the URL twice"
    );
  }

  #[test]
  fn bookmarks_with_title_equal_to_url_treat_title_as_missing() {
    let open_tabs = Vec::new();
    let closed_tabs = Vec::new();
    let visited = VisitedUrlStore::new();
    let mut bookmarks = BookmarkStore::default();
    let url = "https://example.com/bookmark";
    bookmarks
      .add(url.to_string(), Some(url.to_string()), None)
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
    let bookmark = suggestions
      .iter()
      .find(|s| s.url.as_deref() == Some(url))
      .expect("expected bookmark suggestion");
    assert!(
      bookmark.title.is_none(),
      "expected bookmark suggestions to omit titles that are identical to the URL so the UI doesn't render it twice"
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
  fn bookmarks_provider_dedupes_duplicate_urls_case_insensitively() {
    let open_tabs = Vec::new();
    let closed_tabs = Vec::new();
    let visited = VisitedUrlStore::new();
    let mut bookmarks = BookmarkStore::default();
    bookmarks
      .add(
        "HTTP://EXAMPLE.COM".to_string(),
        Some("Upper".to_string()),
        None,
      )
      .unwrap();
    bookmarks
      .add(
        "http://example.com".to_string(),
        Some("Lower".to_string()),
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

    let provider = BookmarksProvider;
    let suggestions = provider.suggestions(&ctx, "example");

    assert_eq!(
      suggestions.len(),
      1,
      "expected provider-level dedupe of duplicate bookmark URLs, got {suggestions:?}"
    );
    assert!(
      suggestions[0]
        .url
        .as_deref()
        .is_some_and(|u| u.eq_ignore_ascii_case("http://example.com")),
      "expected deduped URL to match example.com (case-insensitive), got {suggestions:?}"
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
