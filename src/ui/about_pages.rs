pub const ABOUT_BLANK: &str = "about:blank";
pub const ABOUT_NEWTAB: &str = "about:newtab";
pub const ABOUT_HELP: &str = "about:help";
pub const ABOUT_VERSION: &str = "about:version";
pub const ABOUT_GPU: &str = "about:gpu";
pub const ABOUT_ERROR: &str = "about:error";
pub const ABOUT_HISTORY: &str = "about:history";
pub const ABOUT_BOOKMARKS: &str = "about:bookmarks";
pub const ABOUT_TEST_SCROLL: &str = "about:test-scroll";
pub const ABOUT_TEST_HEAVY: &str = "about:test-heavy";
pub const ABOUT_TEST_FORM: &str = "about:test-form";

/// Known `about:` page URLs.
///
/// This list exists for omnibox/autocomplete providers so built-in pages can be suggested even when
/// they are intentionally excluded from visited history (e.g. `about:newtab`, `about:error`).
pub const ABOUT_PAGE_URLS: &[&str] = &[
  ABOUT_BLANK,
  ABOUT_NEWTAB,
  ABOUT_HELP,
  ABOUT_VERSION,
  ABOUT_GPU,
  ABOUT_ERROR,
  ABOUT_HISTORY,
  ABOUT_BOOKMARKS,
  ABOUT_TEST_SCROLL,
  ABOUT_TEST_HEAVY,
  ABOUT_TEST_FORM,
];

use parking_lot::RwLock;
use std::sync::OnceLock;
use std::time::SystemTime;

use crate::ui::{BookmarkId, BookmarkNode, BookmarkStore, GlobalHistoryStore};
use crate::ui::theme_parsing::RgbaColor;
use crate::ui::url::DEFAULT_SEARCH_ENGINE_TEMPLATE;

#[derive(Debug, Clone, Default)]
pub struct AboutPageSnapshot {
  pub bookmarks: Vec<BookmarkSnapshot>,
  /// Global (cross-tab) browsing history.
  ///
  /// This is expected to be ordered by recency (newest first), but about pages should remain robust
  /// even when callers provide unsorted data.
  pub history: Vec<HistorySnapshot>,
  /// Effective browser chrome accent color (used to theme `about:` pages).
  pub chrome_accent: Option<RgbaColor>,
}

#[derive(Debug, Clone)]
pub struct BookmarkSnapshot {
  pub title: Option<String>,
  pub url: String,
}

#[derive(Debug, Clone)]
pub struct HistorySnapshot {
  pub title: Option<String>,
  pub url: String,
  /// When this URL was last visited.
  pub last_visited: Option<SystemTime>,
  /// Number of times this URL has been visited.
  pub visit_count: u64,
}

static ABOUT_PAGE_SNAPSHOT: OnceLock<RwLock<AboutPageSnapshot>> = OnceLock::new();

fn about_page_snapshot_lock() -> &'static RwLock<AboutPageSnapshot> {
  ABOUT_PAGE_SNAPSHOT.get_or_init(|| RwLock::new(AboutPageSnapshot::default()))
}

pub fn about_page_snapshot() -> AboutPageSnapshot {
  about_page_snapshot_lock().read().clone()
}

pub fn set_about_page_snapshot(snapshot: AboutPageSnapshot) {
  *about_page_snapshot_lock().write() = snapshot;
}

pub fn set_about_snapshot_from_stores(bookmarks: &BookmarkStore, history: &GlobalHistoryStore) {
  // Preserve any separately-updated chrome settings (e.g. accent color) across snapshot refreshes.
  let chrome_accent = about_page_snapshot_lock().read().chrome_accent;
  set_about_page_snapshot(AboutPageSnapshot {
    bookmarks: bookmark_snapshots_from_store(bookmarks),
    history: history_snapshots_from_global_history_store(history),
    chrome_accent,
  });
}

pub fn sync_about_page_snapshot_history_from_global_history_store(store: &GlobalHistoryStore) {
  let history = history_snapshots_from_global_history_store(store);
  about_page_snapshot_lock().write().history = history;
}

pub fn sync_about_page_snapshot_bookmarks_from_bookmark_store(store: &BookmarkStore) {
  let bookmarks = bookmark_snapshots_from_store(store);
  about_page_snapshot_lock().write().bookmarks = bookmarks;
}

pub fn sync_about_page_snapshot_chrome_accent(accent: Option<RgbaColor>) {
  about_page_snapshot_lock().write().chrome_accent = accent;
}

fn bookmark_snapshots_from_store(bookmarks: &BookmarkStore) -> Vec<BookmarkSnapshot> {
  let mut out = Vec::new();
  let mut seen = std::collections::HashSet::<BookmarkId>::new();
  // Use an explicit stack to keep ordering stable and avoid recursion.
  let mut stack: Vec<BookmarkId> = bookmarks.roots.iter().rev().copied().collect();

  while let Some(id) = stack.pop() {
    if !seen.insert(id) {
      continue;
    }
    let Some(node) = bookmarks.nodes.get(&id) else {
      continue;
    };
    match node {
      BookmarkNode::Bookmark(entry) => {
        let url = entry.url.trim();
        if url.is_empty() {
          continue;
        }
        let title = entry
          .title
          .as_deref()
          .map(str::trim)
          .filter(|t| !t.is_empty())
          .map(str::to_string);
        out.push(BookmarkSnapshot {
          title,
          url: url.to_string(),
        });
      }
      BookmarkNode::Folder(folder) => {
        // Maintain folder order by pushing children in reverse onto the LIFO stack.
        for child in folder.children.iter().rev() {
          stack.push(*child);
        }
      }
    }
  }

  out
}

fn history_snapshots_from_global_history_store(store: &GlobalHistoryStore) -> Vec<HistorySnapshot> {
  use std::time::{Duration, UNIX_EPOCH};

  const MAX_HISTORY: usize = 500;

  let mut out = Vec::with_capacity(store.entries.len().min(MAX_HISTORY));
  for entry in store.entries.iter().rev() {
    if out.len() >= MAX_HISTORY {
      break;
    }
    let url = entry.url.trim();
    if url.is_empty() || is_about_url(url) {
      continue;
    }
    let title = entry
      .title
      .as_deref()
      .map(str::trim)
      .filter(|t| !t.is_empty())
      .map(str::to_string);
    let last_visited = if entry.visited_at_ms == 0 {
      None
    } else {
      UNIX_EPOCH.checked_add(Duration::from_millis(entry.visited_at_ms))
    };
    out.push(HistorySnapshot {
      title,
      url: url.to_string(),
      last_visited,
      visit_count: entry.visit_count,
    });
  }
  out
}

const ABOUT_SHARED_CSS_MARKER: &str = "FASTR_ABOUT_SHARED_CSS";

const ABOUT_SHARED_CSS: &str = r#"/* FASTR_ABOUT_SHARED_CSS */
:root {
  color-scheme: light dark;
  --about-mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono",
    "Courier New", monospace;
  --about-surface: rgba(127,127,127,0.06);
  --about-surface-strong: rgba(127,127,127,0.08);
  --about-surface-hover: rgba(127,127,127,0.12);
  --about-border: rgba(127,127,127,0.25);
  --about-border-strong: rgba(127,127,127,0.35);
  --about-focus: rgba(10, 132, 255, 0.65);
  --about-accent-border: rgba(10, 132, 255, 0.55);
  --about-accent-bg: rgba(10, 132, 255, 0.18);
}
body {
  margin: 0;
  padding: 32px 18px;
  font: 15px/1.5 system-ui, -apple-system, Segoe UI, sans-serif;
  background:
    radial-gradient(900px circle at 20% 0%, var(--about-accent-bg), transparent 45%),
    radial-gradient(900px circle at 80% 20%, rgba(127,127,127,0.10), transparent 45%),
    rgba(127,127,127,0.04);
}

@media (prefers-color-scheme: dark) {
  :root {
    --about-surface: rgba(255,255,255,0.06);
    --about-surface-strong: rgba(255,255,255,0.08);
    --about-surface-hover: rgba(255,255,255,0.12);
    --about-border: rgba(255,255,255,0.20);
    --about-border-strong: rgba(255,255,255,0.30);
  }
  body {
    color: rgba(255,255,255,0.92);
    background:
      radial-gradient(900px circle at 20% 0%, var(--about-accent-bg), transparent 45%),
      radial-gradient(900px circle at 80% 20%, rgba(255,255,255,0.06), transparent 45%),
      rgba(0,0,0,0.88);
  }
  code, kbd {
    background: rgba(255,255,255,0.16);
  }
  .about-card {
    border-color: rgba(255,255,255,0.14);
    box-shadow: 0 18px 60px rgba(0, 0, 0, 0.38);
  }
}
h1 { font-size: 20px; margin: 0 0 12px; letter-spacing: -0.01em; }
h2 { font-size: 16px; margin: 18px 0 8px; }
p { margin: 0 0 10px; }
ul { margin: 0 0 10px; padding-left: 18px; }
code, kbd {
  font-family: var(--about-mono);
  padding: 0.1em 0.3em;
  border-radius: 6px;
  background: rgba(127,127,127,0.2);
}
table { border-collapse: collapse; }
td { padding: 4px 10px 4px 0; vertical-align: top; }

.about-wrap { max-width: 880px; margin: 0 auto; }

.about-header {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 12px;
  margin: 0 0 14px;
}
.about-brand {
  font-weight: 650;
  text-decoration: none;
  color: inherit;
}
.about-nav {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}
.about-nav a,
.about-button,
button {
  display: inline-block;
  padding: 8px 12px;
  border-radius: 999px;
  border: 1px solid var(--about-border);
  background: var(--about-surface-strong);
  color: inherit;
  text-decoration: none;
  font: inherit;
}
.about-nav a:hover,
.about-button:hover,
button:hover {
  background: var(--about-surface-hover);
}
.about-nav a:focus,
.about-button:focus,
button:focus {
  outline: 3px solid var(--about-focus);
  outline-offset: 2px;
}
.about-button.primary {
  border-color: var(--about-accent-border);
  background: var(--about-accent-bg);
}
.about-nav a[aria-current="page"] {
  border-color: rgba(127,127,127,0.42);
  background: rgba(127,127,127,0.14);
}

.about-card {
  border: 1px solid rgba(127,127,127,0.18);
  border-radius: 16px;
  background: var(--about-surface);
  box-shadow: 0 18px 60px rgba(0, 0, 0, 0.12);
  padding: 20px;
}

.about-footer { margin-top: 14px; }

.about-hint {
  margin-top: 16px;
  padding: 12px 14px;
  border-radius: 12px;
  border: 1px solid rgba(127,127,127,0.25);
  background: rgba(127,127,127,0.10);
  display: flex;
  align-items: center;
  gap: 10px;
}
.about-kbd {
  font-family: var(--about-mono);
  font-size: 12px;
  padding: 2px 7px;
  border-radius: 8px;
  border: 1px solid rgba(127,127,127,0.25);
  background: rgba(127,127,127,0.08);
  color: inherit;
}
.about-actions {
  margin-top: 18px;
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
  gap: 12px;
}
a.about-tile {
  display: block;
  text-decoration: none;
  color: inherit;
  border: 1px solid var(--about-border);
  background: var(--about-surface);
  border-radius: 12px;
  padding: 12px 14px;
}
a.about-tile:hover { background: rgba(127,127,127,0.10); }
.about-tile .label { font-weight: 650; margin: 0 0 4px; }
.about-tile .url {
  font-family: var(--about-mono);
  font-size: 12px;
  opacity: 0.82;
}
.about-tip {
  margin-top: 18px;
  font-size: 13px;
  opacity: 0.82;
}

a { text-underline-offset: 2px; }
"#;

fn about_shared_css() -> &'static str {
  debug_assert!(
    ABOUT_SHARED_CSS.contains(ABOUT_SHARED_CSS_MARKER),
    "ABOUT_SHARED_CSS_MARKER must be present in shared about-page CSS"
  );
  ABOUT_SHARED_CSS
}

fn about_header_html(current: &str) -> String {
  let items = [
    (ABOUT_NEWTAB, "New tab"),
    (ABOUT_HISTORY, "History"),
    (ABOUT_BOOKMARKS, "Bookmarks"),
    (ABOUT_HELP, "Help"),
    (ABOUT_VERSION, "Version"),
    (ABOUT_GPU, "GPU"),
  ];
  let mut links = String::with_capacity(256);
  for (url, label) in items {
    let aria = if url == current { " aria-current=\"page\"" } else { "" };
    links.push_str(&format!("<a href=\"{url}\"{aria}>{label}</a>"));
  }
  format!(
    "<header class=\"about-header\">
      <a class=\"about-brand\" href=\"{ABOUT_NEWTAB}\">FastRender</a>
      <nav class=\"about-nav\" aria-label=\"Built-in pages\">{links}</nav>
    </header>"
  )
}

fn about_footer_html() -> String {
  format!(
    "<footer class=\"about-footer\">
      <nav class=\"about-nav\" aria-label=\"Page navigation\">
        <a href=\"{ABOUT_NEWTAB}\">Back to new tab</a>
      </nav>
    </footer>"
  )
}

fn about_theme_css() -> String {
  // Default accent (matches the legacy about-page palette).
  const DEFAULT_ACCENT: RgbaColor = RgbaColor::new(10, 132, 255, 0xFF);
  let accent = about_page_snapshot_lock()
    .read()
    .chrome_accent
    .unwrap_or(DEFAULT_ACCENT);
  let r = accent.r;
  let g = accent.g;
  let b = accent.b;
  format!(
    ":root {{
  --about-focus: rgba({r}, {g}, {b}, 0.65);
  --about-accent-border: rgba({r}, {g}, {b}, 0.55);
  --about-accent-bg: rgba({r}, {g}, {b}, 0.18);
}}
"
  )
}

fn about_layout_html(title: &str, current: &str, body: &str, extra_css: &str) -> String {
  let safe_title = escape_html(title);
  let header = about_header_html(current);
  let footer = about_footer_html();
  let theme_css = about_theme_css();
  format!(
    "<!doctype html>
<html>
  <head>
    <meta charset=\"utf-8\">
    <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
    <title>{safe_title}</title>
    <style>
{shared}
{theme_css}
{extra_css}
    </style>
  </head>
  <body>
    <div class=\"about-wrap\">
      {header}
      <main class=\"about-card\">
        {body}
      </main>
      {footer}
    </div>
  </body>
</html>",
    shared = about_shared_css(),
  )
}

#[derive(Debug, Clone)]
struct GpuInfo {
  adapter_name: String,
  backend: String,
  power_preference: String,
  force_fallback_adapter: String,
  instance_backends: String,
}

static GPU_INFO: OnceLock<GpuInfo> = OnceLock::new();

/// Provide information about the GPU/adapter selected by the windowed `browser` front-end.
///
/// This is intentionally a best-effort global hint: the headless worker (tests, server use-cases)
/// does not have a wgpu adapter, so the `about:gpu` page falls back to `"unknown"`.
pub fn set_gpu_info(
  adapter_name: impl Into<String>,
  backend: impl Into<String>,
  power_preference: impl Into<String>,
  force_fallback_adapter: bool,
  instance_backends: impl Into<String>,
) {
  let _ = GPU_INFO.set(GpuInfo {
    adapter_name: adapter_name.into(),
    backend: backend.into(),
    power_preference: power_preference.into(),
    force_fallback_adapter: force_fallback_adapter.to_string(),
    instance_backends: instance_backends.into(),
  });
}

/// Base URL hint used for all `about:` pages.
///
/// Using `about:blank` prevents relative URLs from being accidentally resolved against the last
/// navigated network origin.
pub const ABOUT_BASE_URL: &str = ABOUT_BLANK;

pub fn is_about_url(url: &str) -> bool {
  url.trim_start().to_ascii_lowercase().starts_with("about:")
}

/// Return known `about:` pages that match a user-typed prefix (case-insensitive).
///
/// This is intended to be used by omnibox/autocomplete code and is deliberately independent of any
/// visited-history state.
pub fn suggest_about_pages(prefix: &str) -> Vec<&'static str> {
  let query = prefix.trim().to_ascii_lowercase();
  if query.is_empty() {
    return Vec::new();
  }
  // Avoid suggesting `about:` pages unless the user is clearly heading in that direction.
  if !query.starts_with("about") {
    return Vec::new();
  }
  ABOUT_PAGE_URLS
    .iter()
    .copied()
    .filter(|url| url.starts_with(&query))
    .collect()
}

pub fn html_for_about_url(url: &str) -> Option<String> {
  let normalized = url.trim();
  // `about:` pages may be used with query strings (e.g. form submissions) or fragments.
  // Only the base `about:*` identifier selects the template.
  let normalized = normalized
    .split(|c| matches!(c, '?' | '#'))
    .next()
    .unwrap_or(normalized);
  let lower = normalized.to_ascii_lowercase();
  match lower.as_str() {
    ABOUT_BLANK => Some(blank_html().to_string()),
    ABOUT_NEWTAB => Some(newtab_html()),
    ABOUT_HELP => Some(help_html()),
    ABOUT_VERSION => Some(version_html()),
    ABOUT_GPU => Some(gpu_html()),
    ABOUT_ERROR => Some(error_html("Navigation error", None, None)),
    ABOUT_HISTORY => Some(history_html(url)),
    ABOUT_BOOKMARKS => Some(bookmarks_html(url)),
    ABOUT_TEST_SCROLL => Some(test_scroll_html()),
    ABOUT_TEST_HEAVY => Some(test_heavy_html()),
    ABOUT_TEST_FORM => Some(test_form_html()),
    _ => None,
  }
}

pub fn error_page_html(title: &str, message: &str, retry_url: Option<&str>) -> String {
  error_html(title, Some(message), retry_url)
}

fn blank_html() -> &'static str {
  "<!doctype html><html><head><meta charset=\"utf-8\"></head><body></body></html>"
}

#[derive(Debug, Clone)]
struct SearchFormConfig {
  action: String,
  query_param: String,
  hidden_inputs: Vec<(String, String)>,
}

fn search_form_config_from_template(template: &str) -> Option<SearchFormConfig> {
  const QUERY_PLACEHOLDER: &str = "FASTR_QUERY_PLACEHOLDER";
  if !template.contains("{query}") {
    return None;
  }

  let replaced = template.replace("{query}", QUERY_PLACEHOLDER);
  let mut url = url::Url::parse(&replaced).ok()?;
  let mut query_param = None;
  let mut hidden_inputs = Vec::new();

  for (key, value) in url.query_pairs() {
    if value == QUERY_PLACEHOLDER {
      if query_param.is_none() {
        query_param = Some(key.into_owned());
      }
    } else {
      hidden_inputs.push((key.into_owned(), value.into_owned()));
    }
  }

  let query_param = query_param?;
  url.set_query(None);
  url.set_fragment(None);

  Some(SearchFormConfig {
    action: url.to_string(),
    query_param,
    hidden_inputs,
  })
}

fn newtab_html() -> String {
  const MAX_BOOKMARKS: usize = 12;
  const MAX_HISTORY: usize = 12;

  let snapshot = about_page_snapshot();
  let search_form = search_form_config_from_template(DEFAULT_SEARCH_ENGINE_TEMPLATE).unwrap_or(
    SearchFormConfig {
      action: "https://duckduckgo.com/".to_string(),
      query_param: "q".to_string(),
      hidden_inputs: Vec::new(),
    },
  );
  let safe_search_action = escape_html(&search_form.action);
  let safe_search_param = escape_html(&search_form.query_param);
  let mut hidden_inputs_html = String::new();
  for (key, value) in search_form.hidden_inputs.into_iter() {
    let safe_key = escape_html(&key);
    let safe_value = escape_html(&value);
    use std::fmt::Write;
    let _ = write!(
      hidden_inputs_html,
      r#"<input type="hidden" name="{safe_key}" value="{safe_value}">"#
    );
  }
  use std::fmt::Write;

  let mut bookmark_tiles = String::new();
  let mut bookmark_count = 0usize;
  for bookmark in snapshot.bookmarks.iter() {
    if bookmark_count >= MAX_BOOKMARKS {
      break;
    }
    let url = bookmark.url.trim();
    if url.is_empty() {
      continue;
    }
    let title = bookmark
      .title
      .as_deref()
      .map(str::trim)
      .filter(|t| !t.is_empty())
      .unwrap_or(url);
    let safe_url = escape_html(url);
    let safe_title = escape_html(title);
    let safe_display_url = escape_html(url);
    let _ = write!(
      bookmark_tiles,
      r#"<a class="about-tile" href="{safe_url}"><div class="label">{safe_title}</div><div class="url">{safe_display_url}</div></a>"#
    );
    bookmark_count += 1;
  }

  let bookmarks_body = if bookmark_count == 0 {
    "<p>No bookmarks yet.</p>".to_string()
  } else {
    format!(r#"<div class="about-actions" aria-label="Bookmarks">{bookmark_tiles}</div>"#)
  };

  // "Recently visited" should ignore duplicate URLs and prefer the most recent visit.
  //
  // Most callers are expected to provide `snapshot.history` already sorted by recency, but we keep
  // this robust: if callers provide unsorted data and include timestamps, we still surface the
  // most recent entry per URL.
  #[derive(Clone)]
  struct HistoryMerged {
    url: String,
    title: Option<String>,
    last_visited: Option<SystemTime>,
    first_idx: usize,
  }

  let mut merged_by_url: std::collections::HashMap<String, HistoryMerged> =
    std::collections::HashMap::new();
  for (idx, entry) in snapshot.history.iter().enumerate() {
    let url = entry.url.trim();
    if url.is_empty() || is_about_url(url) {
      continue;
    }

    let title = entry
      .title
      .as_deref()
      .map(str::trim)
      .filter(|t| !t.is_empty())
      .map(str::to_string);

    let slot = merged_by_url.entry(url.to_string()).or_insert_with(|| HistoryMerged {
      url: url.to_string(),
      title: title.clone(),
      last_visited: entry.last_visited,
      first_idx: idx,
    });

    // Prefer the newest `last_visited` timestamp; break ties by keeping the first seen entry so
    // behaviour is deterministic even when timestamps are missing.
    if entry.last_visited > slot.last_visited {
      slot.last_visited = entry.last_visited;
      if title.is_some() {
        slot.title = title.clone();
      }
    } else if slot.title.is_none() && title.is_some() {
      // Best-effort: if the most-recent entry is missing a title but an older one has it, keep the
      // known title instead of falling back to the raw URL.
      slot.title = title.clone();
    }
  }

  let mut merged_history: Vec<HistoryMerged> = merged_by_url.into_values().collect();
  merged_history.sort_by(|a, b| {
    b.last_visited
      .cmp(&a.last_visited)
      .then_with(|| a.first_idx.cmp(&b.first_idx))
      .then_with(|| a.url.cmp(&b.url))
  });

  let mut history_tiles = String::new();
  let mut history_count = 0usize;
  for entry in merged_history.into_iter().take(MAX_HISTORY) {
    let url = entry.url.trim();
    if url.is_empty() {
      continue;
    }
    let title = entry
      .title
      .as_deref()
      .map(str::trim)
      .filter(|t| !t.is_empty())
      .unwrap_or(url);
    let safe_url = escape_html(url);
    let safe_title = escape_html(title);
    let safe_display_url = escape_html(url);
    let _ = write!(
      history_tiles,
      r#"<a class="about-tile" href="{safe_url}"><div class="label">{safe_title}</div><div class="url">{safe_display_url}</div></a>"#
    );
    history_count += 1;
    if history_count >= MAX_HISTORY {
      break;
    }
  }

  let history_body = if history_count == 0 {
    "<p>No history yet.</p>".to_string()
  } else {
    format!(
      r#"<div class="about-actions" aria-label="Recently visited">{history_tiles}</div>"#
    )
  };

  about_layout_html(
    "New Tab",
    ABOUT_NEWTAB,
    &format!(
      r#"<h1>FastRender</h1>
      <p>
        This is an offline <code>about:newtab</code> page powered by your local bookmarks and
        browsing history.
      </p>

      <form class="about-search" method="get" action="{safe_search_action}" role="search">
        {hidden_inputs_html}
        <input type="search" name="{safe_search_param}" placeholder="Search the web" aria-label="Search the web">
        <button class="about-button primary" type="submit">Search</button>
      </form>

      <div class="about-hint" role="note">
        <span class="about-kbd">Ctrl</span>
        <span class="about-kbd">L</span>
        <span>Type to search or enter a URL</span>
      </div>

      <h2>Shortcuts</h2>
      <div class="about-actions" aria-label="Shortcuts">
        <a class="about-tile" href="https://example.com/">
          <div class="label">Example page</div>
          <div class="url">https://example.com/</div>
        </a>
        <a class="about-tile" href="about:history">
          <div class="label">History</div>
          <div class="url">about:history</div>
        </a>
        <a class="about-tile" href="about:bookmarks">
          <div class="label">Bookmarks</div>
          <div class="url">about:bookmarks</div>
        </a>
        <a class="about-tile" href="about:help">
          <div class="label">Help</div>
          <div class="url">about:help</div>
        </a>
        <a class="about-tile" href="about:version">
          <div class="label">Version</div>
          <div class="url">about:version</div>
        </a>
        <a class="about-tile" href="about:gpu">
          <div class="label">GPU</div>
          <div class="url">about:gpu</div>
        </a>
      </div>

      <h2>Bookmarks</h2>
      {bookmarks_body}

      <h2>Recently visited</h2>
      {history_body}

      <div class="about-tip">
       Tip: You can also open local files by typing a path like <code>/tmp/a.html</code> or
        <code>C:\path\to\file.html</code>.
      </div>"#,
      bookmarks_body = bookmarks_body,
      history_body = history_body,
      safe_search_action = safe_search_action,
      safe_search_param = safe_search_param,
      hidden_inputs_html = hidden_inputs_html
    ),
    r#"
.about-search {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
  margin: 18px 0 18px;
}
.about-search input[type="search"] {
  flex: 1;
  min-width: min(420px, 100%);
  padding: 10px 14px;
  border-radius: 999px;
  border: 1px solid var(--about-border-strong);
  background: var(--about-surface);
  color: inherit;
  font: inherit;
}
.about-search input[type="search"]:focus {
  outline: 3px solid var(--about-focus);
  outline-offset: 2px;
}
.about-search button { cursor: pointer; }
"#,
  )
}

fn help_html() -> String {
  about_layout_html(
    "Help",
    ABOUT_HELP,
    "<h1>FastRender Help</h1>
      <p>This is an offline <code>about:help</code> page.</p>

      <h2>Usage</h2>
      <ul>
        <li>Type a URL (http/https/file/about) or a search query into the address bar.</li>
        <li>Typing <code>example.com</code> defaults to <code>https://example.com/</code>.</li>
        <li>Typing a filesystem path like <code>/tmp/a.html</code> navigates to a <code>file://</code> URL.</li>
        <li>Non-URL queries (e.g. <code>cats</code>) are treated as searches using the default search engine.</li>
        <li>The address bar (omnibox) shows suggestions from history and open tabs while typing.
          Use <kbd>ArrowUp</kbd>/<kbd>ArrowDown</kbd> to select, <kbd>Enter</kbd> to accept, <kbd>Escape</kbd> to close.</li>
      </ul>

      <h2>Bookmarks and history</h2>
      <ul>
        <li>Use the star button in the toolbar (or <kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>D</kbd>) to toggle a bookmark for the current page.</li>
        <li>Bookmarks show up in the bookmarks bar for quick access.</li>
        <li>The history panel supports search and clear.</li>
        <li>Bookmarks and history are persisted as JSON files under FastRender’s per-user config directory (for example <code>~/.config/fastrender/</code> on Linux). You can override the file paths with <code>FASTR_BROWSER_BOOKMARKS_PATH</code> / <code>FASTR_BROWSER_HISTORY_PATH</code>.</li>
      </ul>

      <h2>Keyboard shortcuts</h2>
      <ul>
        <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>L</kbd> / <kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>K</kbd> — Focus address bar</li>
        <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>T</kbd> — New tab</li>
        <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>T</kbd> — Reopen last closed tab</li>
        <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>W</kbd> — Close tab</li>
        <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>Tab</kbd> / <kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>Tab</kbd> — Next/prev tab</li>
        <li><kbd>Alt</kbd>+<kbd>Left</kbd> / <kbd>Alt</kbd>+<kbd>Right</kbd> (Win/Linux); <kbd>Cmd</kbd>+<kbd>[</kbd> / <kbd>Cmd</kbd>+<kbd>]</kbd> (macOS) — Back/forward</li>
        <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>R</kbd> / <kbd>F5</kbd> — Reload</li>
        <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>1</kbd>…<kbd>9</kbd> — Activate tab (9 = last)</li>
        <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>D</kbd> — Toggle bookmark</li>
        <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>O</kbd> — Show bookmarks manager</li>
        <li><kbd>Ctrl</kbd>+<kbd>H</kbd> (Win/Linux); <kbd>Cmd</kbd>+<kbd>Y</kbd> / <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>H</kbd> (macOS) — Show history</li>
      </ul>

      <h2>Built-in pages</h2>
      <ul>
        <li><a href=\"about:newtab\">about:newtab</a></li>
        <li><a href=\"about:history\">about:history</a></li>
        <li><a href=\"about:bookmarks\">about:bookmarks</a></li>
        <li><a href=\"about:version\">about:version</a></li>
        <li><a href=\"about:gpu\">about:gpu</a></li>
      </ul>",
    "",
  )
}

fn version_html() -> String {
  let version = env!("CARGO_PKG_VERSION");
  let profile = option_env!("PROFILE").unwrap_or("unknown");
  let git_hash = option_env!("FASTR_GIT_HASH")
    .or(option_env!("GIT_HASH"))
    .or(option_env!("VERGEN_GIT_SHA"))
    .or(option_env!("VERGEN_GIT_SHA_SHORT"));

  let safe_version = escape_html(version);
  let safe_profile = escape_html(profile);
  let safe_git = escape_html(git_hash.unwrap_or("unknown"));

  about_layout_html(
    "Version",
    ABOUT_VERSION,
    &format!(
      "<h1>Version</h1>
      <table>
        <tr><td>crate version</td><td><code>{safe_version}</code></td></tr>
        <tr><td>git hash</td><td><code>{safe_git}</code></td></tr>
        <tr><td>build profile</td><td><code>{safe_profile}</code></td></tr>
      </table>"
    ),
    "",
  )
}

fn gpu_html() -> String {
  let (adapter_name, backend, power_preference, force_fallback_adapter, instance_backends) =
    match GPU_INFO.get() {
      Some(info) => (
        info.adapter_name.as_str(),
        info.backend.as_str(),
        info.power_preference.as_str(),
        info.force_fallback_adapter.as_str(),
        info.instance_backends.as_str(),
      ),
      None => ("unknown", "unknown", "unknown", "unknown", "unknown"),
  };
  let safe_name = escape_html(adapter_name);
  let safe_backend = escape_html(backend);
  let safe_power_preference = escape_html(power_preference);
  let safe_force_fallback = escape_html(force_fallback_adapter);
  let safe_instance_backends = escape_html(instance_backends);

  about_layout_html(
    "GPU",
    ABOUT_GPU,
    &format!(
      "<h1>GPU</h1>
      <p>This page is best-effort: headless runs do not initialize wgpu.</p>
      <table>
        <tr><td>adapter</td><td><code>{safe_name}</code></td></tr>
        <tr><td>backend</td><td><code>{safe_backend}</code></td></tr>
        <tr><td>power preference</td><td><code>{safe_power_preference}</code></td></tr>
        <tr><td>force fallback adapter</td><td><code>{safe_force_fallback}</code></td></tr>
        <tr><td>instance backends</td><td><code>{safe_instance_backends}</code></td></tr>
      </table>"
    ),
    "",
  )
}

fn about_query_param(url: &str, key: &str) -> Option<String> {
  let (_, query) = url.split_once('?')?;
  let query = query.split('#').next().unwrap_or(query);
  let mut out = None;
  for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
    if k == key {
      out = Some(v.into_owned());
    }
  }
  out
}

fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
  // Lightweight ASCII-only case-insensitive matching (non-ASCII bytes are compared exactly).
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

fn matches_search_tokens(url: &str, title: Option<&str>, tokens: &[&str]) -> bool {
  if tokens.is_empty() {
    return true;
  }

  for token in tokens {
    let in_url = contains_case_insensitive(url, token);
    let in_title = title.is_some_and(|t| contains_case_insensitive(t, token));
    if !in_url && !in_title {
      return false;
    }
  }

  true
}

const ABOUT_SEARCH_PAGE_CSS: &str = r#"
.sub { opacity: 0.82; margin: 0 0 14px; }
.search { display: flex; gap: 8px; margin: 0 0 18px; flex-wrap: wrap; }
.search input {
  flex: 1;
  min-width: min(420px, 100%);
  padding: 8px 12px;
  border-radius: 12px;
  border: 1px solid rgba(127,127,127,0.35);
  background: rgba(127,127,127,0.06);
  color: inherit;
  font: inherit;
}
.search button { cursor: pointer; }

.list {
  list-style: none;
  padding: 0;
  margin: 0;
  border-radius: 14px;
  border: 1px solid rgba(127,127,127,0.28);
  overflow: hidden;
}
.item { padding: 10px 12px; border-bottom: 1px solid rgba(127,127,127,0.22); }
.item:last-child { border-bottom: none; }
.title { font-weight: 650; }
.url { margin-top: 4px; font-size: 12px; opacity: 0.82; }
code { word-break: break-all; }
"#;

fn history_html(full_url: &str) -> String {
  let query = about_query_param(full_url, "q")
    .unwrap_or_default()
    .trim()
    .to_string();
  let tokens: Vec<&str> = query.split_whitespace().filter(|t| !t.is_empty()).collect();
  let safe_query = escape_html(&query);

  let snapshot = about_page_snapshot();

  let mut results_html = String::new();
  let mut match_count = 0usize;
  let mut total_count = 0usize;
  let mut seen_urls = std::collections::HashSet::<&str>::new();

  for entry in snapshot.history.iter() {
    let url = entry.url.trim();
    if url.is_empty() {
      continue;
    }
    if !seen_urls.insert(url) {
      continue;
    }
    total_count += 1;

    let title = entry
      .title
      .as_deref()
      .map(str::trim)
      .filter(|t| !t.is_empty());
    if !matches_search_tokens(url, title, &tokens) {
      continue;
    }

    match_count += 1;
    let display_title = title.unwrap_or(url);
    let safe_title = escape_html(display_title);
    let safe_url = escape_html(url);
    use std::fmt::Write;
    let _ = write!(
      results_html,
      "<li class=\"item\">\
         <div class=\"title\"><a href=\"{safe_url}\">{safe_title}</a></div>\
         <div class=\"url\"><code>{safe_url}</code></div>\
       </li>"
    );
  }

  let body = if match_count == 0 {
    if tokens.is_empty() {
      "<p class=\"empty\">No history entries yet.</p>".to_string()
    } else {
      format!("<p class=\"empty\">No history results for <code>{safe_query}</code>.</p>")
    }
  } else {
    format!("<ul class=\"list\">{results_html}</ul>")
  };

  let page_body = format!(
    "<h1>History</h1>
    <p class=\"sub\">Showing {match_count} of {total_count} entries.</p>
    <form class=\"search\" method=\"get\" action=\"{ABOUT_HISTORY}\">
      <input type=\"search\" name=\"q\" value=\"{safe_query}\" placeholder=\"Search history\">
      <button type=\"submit\">Search</button>
    </form>
    {body}"
  );
  about_layout_html("History", ABOUT_HISTORY, &page_body, ABOUT_SEARCH_PAGE_CSS)
}

fn bookmarks_html(full_url: &str) -> String {
  let query = about_query_param(full_url, "q")
    .unwrap_or_default()
    .trim()
    .to_string();
  let tokens: Vec<&str> = query.split_whitespace().filter(|t| !t.is_empty()).collect();
  let safe_query = escape_html(&query);

  let snapshot = about_page_snapshot();

  let mut results_html = String::new();
  let mut match_count = 0usize;
  let mut total_count = 0usize;
  let mut seen_urls = std::collections::HashSet::<&str>::new();

  for bookmark in snapshot.bookmarks.iter() {
    let url = bookmark.url.trim();
    if url.is_empty() {
      continue;
    }
    if !seen_urls.insert(url) {
      continue;
    }
    total_count += 1;

    let title = bookmark
      .title
      .as_deref()
      .map(str::trim)
      .filter(|t| !t.is_empty());
    if !matches_search_tokens(url, title, &tokens) {
      continue;
    }

    match_count += 1;
    let display_title = title.unwrap_or(url);
    let safe_title = escape_html(display_title);
    let safe_url = escape_html(url);
    use std::fmt::Write;
    let _ = write!(
      results_html,
      "<li class=\"item\">\
         <div class=\"title\"><a href=\"{safe_url}\">{safe_title}</a></div>\
         <div class=\"url\"><code>{safe_url}</code></div>\
       </li>"
    );
  }

  let body = if match_count == 0 {
    if tokens.is_empty() {
      "<p class=\"empty\">No bookmarks yet.</p>".to_string()
    } else {
      format!("<p class=\"empty\">No bookmarks match <code>{safe_query}</code>.</p>")
    }
  } else {
    format!("<ul class=\"list\">{results_html}</ul>")
  };

  let page_body = format!(
    "<h1>Bookmarks</h1>
    <p class=\"sub\">Showing {match_count} of {total_count} entries.</p>
    <form class=\"search\" method=\"get\" action=\"{ABOUT_BOOKMARKS}\">
      <input type=\"search\" name=\"q\" value=\"{safe_query}\" placeholder=\"Search bookmarks\">
      <button type=\"submit\">Search</button>
    </form>
    {body}"
  );
  about_layout_html("Bookmarks", ABOUT_BOOKMARKS, &page_body, ABOUT_SEARCH_PAGE_CSS)
}

fn error_html(title: &str, message: Option<&str>, retry_url: Option<&str>) -> String {
  let safe_title = escape_html(title);
  let safe_retry_url = retry_url
    .map(str::trim)
    .filter(|url| !url.is_empty())
    .map(escape_html);
  let retry_button = safe_retry_url
    .as_deref()
    .map(|url| format!("<a class=\"about-button primary\" href=\"{url}\">Retry</a>"))
    .unwrap_or_default();
  let url_line = safe_retry_url
    .as_deref()
    .map(|url| format!("<p class=\"about-error-url\">URL: <code>{url}</code></p>"))
    .unwrap_or_default();

  let details_body = match message {
    Some(message) if !message.trim().is_empty() => {
      let safe = escape_html(message);
      format!("<pre>{safe}</pre>")
    }
    _ => "<p class=\"details-empty\">No additional details are available.</p>".to_string(),
  };

  about_layout_html(
    title,
    ABOUT_ERROR,
    &format!(
      "<div class=\"about-error-header\">
        <div class=\"about-error-icon\" aria-hidden=\"true\">!</div>
        <div>
          <h1>{safe_title}</h1>
          <p class=\"about-error-sub\">FastRender couldn&rsquo;t load this page.</p>
        </div>
      </div>

      <div class=\"about-error-actions\">
        {retry_button}
        <a class=\"about-button\" href=\"about:newtab\">Back to new tab</a>
      </div>

      {url_line}

      <div class=\"about-error-help\">
        <p>Try:</p>
        <ul>
          <li>Checking the URL for typos.</li>
          <li>Verifying the file exists (for <code>file://</code> URLs).</li>
          <li>Checking your network connection or firewall (for <code>http(s)://</code> URLs).</li>
        </ul>
      </div>

      <details>
        <summary>Technical details</summary>
        <div class=\"about-error-details\">{details_body}</div>
      </details>"
    ),
    r#"
.about-error-header {
  display: flex;
  gap: 14px;
  align-items: flex-start;
}
.about-error-icon {
  width: 40px;
  height: 40px;
  border-radius: 12px;
  display: flex;
  align-items: center;
  justify-content: center;
  flex: 0 0 auto;
  font-weight: 800;
  font-size: 22px;
  color: rgb(215, 0, 21);
  background: rgba(255, 59, 48, 0.14);
  border: 1px solid rgba(255, 59, 48, 0.35);
}
.about-error-sub {
  margin: 6px 0 0;
  opacity: 0.82;
}
.about-error-url {
  margin: 12px 0 0;
}
.about-error-url code {
  word-break: break-all;
}
.about-error-actions {
  margin-top: 18px;
  display: flex;
  gap: 10px;
  flex-wrap: wrap;
}
.about-error-help {
  margin-top: 18px;
}
.about-error-help p {
  margin: 0 0 8px;
}
.about-error-help ul {
  margin: 0;
}
details {
  margin-top: 18px;
}
summary {
  cursor: pointer;
  font-weight: 600;
}
.about-error-details {
  margin-top: 10px;
  padding: 12px;
  border-radius: 12px;
  border: 1px solid rgba(127,127,127,0.28);
  background: rgba(255, 59, 48, 0.08);
}
.about-error-details pre {
  margin: 0;
  white-space: pre-wrap;
  word-break: break-word;
}
.details-empty {
  margin: 0;
  opacity: 0.82;
}
"#,
  )
}

fn test_scroll_html() -> String {
  // Simple tall page used by browser UI tests.
  format!(
    "<!doctype html>
<html>
  <head>
    <meta charset=\"utf-8\">
    <title>Scroll Test</title>
    <style>
{shared}
      body {{ margin: 0; padding: 0; font: 14px/1.3 system-ui, -apple-system, Segoe UI, sans-serif; }}
      a {{ display: block; padding: 8px; }}
      .spacer {{ height: 4000px; background: linear-gradient(#eee, #ccc); }}
    </style>
  </head>
  <body>
    <a href=\"about:blank\">focus link</a>
    <div class=\"spacer\">scroll</div>
  </body>
</html>",
    shared = about_shared_css(),
  )
}

fn test_heavy_html() -> String {
  // Large DOM used by cancellation tests. Keep this deterministic and offline.
  let mut out = String::with_capacity(256 * 1024);
  out.push_str("<!doctype html><html><head><meta charset=\"utf-8\"><title>Heavy Test</title><style>");
  out.push_str(about_shared_css());
  out.push_str(
    "body{margin:0;padding:0;font:14px/1.3 system-ui, -apple-system, Segoe UI, sans-serif;}\
     .row{padding:4px 8px;border-bottom:1px solid rgba(0,0,0,0.08);}</style></head><body>",
  );
  // Keep this large enough that cancellation tests can reliably interrupt in-flight layout/paint,
  // but small enough that debug builds complete comfortably under CI contention.
  for i in 0..3000u32 {
    use std::fmt::Write;
    let _ = write!(out, "<div class=\"row\">row {i}</div>");
  }
  out.push_str("</body></html>");
  out
}

fn test_form_html() -> String {
  // Offline form used by browser UI interaction tests.
  format!(
    "<!doctype html>
<html>
  <head>
    <meta charset=\"utf-8\">
    <title>Form Test</title>
    <style>
{shared}
      body {{ margin: 0; padding: 0; font: 14px/1.3 system-ui, -apple-system, Segoe UI, sans-serif; }}
      input {{ display: block; width: 180px; height: 28px; }}
      button {{ display: block; width: 180px; height: 28px; margin-top: 8px; }}
    </style>
  </head>
  <body>
    <form>
      <input name=\"q\">
      <button type=\"submit\" name=\"go\" value=\"1\">Go</button>
    </form>
  </body>
</html>",
    shared = about_shared_css(),
  )
}

fn escape_html(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  for ch in text.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '"' => out.push_str("&quot;"),
      '\'' => out.push_str("&#39;"),
      _ => out.push(ch),
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ui::GlobalHistoryEntry;
  use std::time::{Duration, UNIX_EPOCH};

  static SNAPSHOT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

  fn extract_title(html: &str) -> Option<&str> {
    let start = html.find("<title>")? + "<title>".len();
    let end = html[start..].find("</title>")? + start;
    Some(&html[start..end])
  }

  #[test]
  fn history_snapshot_rebuild_preserves_recency_order_and_visit_counts() {
    let mut history = GlobalHistoryStore::default();
    history.entries = vec![
        GlobalHistoryEntry {
          url: "https://old.example/".to_string(),
          title: Some("Old".to_string()),
          visited_at_ms: 1,
          visit_count: 2,
        },
        GlobalHistoryEntry {
          url: "https://mid.example/".to_string(),
          title: None,
          visited_at_ms: 2,
          visit_count: 1,
        },
        GlobalHistoryEntry {
          url: "https://new.example/".to_string(),
          title: Some("New".to_string()),
          visited_at_ms: 3,
          visit_count: 9,
        },
      ];

    let snapshot = super::history_snapshots_from_global_history_store(&history);
    assert_eq!(snapshot.len(), 3);

    assert_eq!(snapshot[0].url, "https://new.example/");
    assert_eq!(snapshot[0].visit_count, 9);
    assert_eq!(
      snapshot[0]
        .last_visited
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap(),
      Duration::from_millis(3)
    );

    assert_eq!(snapshot[1].url, "https://mid.example/");
    assert_eq!(snapshot[1].visit_count, 1);

    assert_eq!(snapshot[2].url, "https://old.example/");
    assert_eq!(snapshot[2].visit_count, 2);
  }

  #[test]
  fn history_snapshot_rebuild_uses_normalized_urls_without_fragments() {
    let mut history = GlobalHistoryStore::default();
    history.entries = vec![GlobalHistoryEntry {
        url: "https://example.test/a#frag".to_string(),
        title: None,
        visited_at_ms: 1,
        visit_count: 1,
      }];
    history.normalize_in_place();

    let snapshot = super::history_snapshots_from_global_history_store(&history);
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].url, "https://example.test/a");
    assert!(
      !snapshot[0].url.contains('#'),
      "expected fragment to be stripped by GlobalHistoryStore normalization"
    );
  }

  #[test]
  fn html_for_about_url_maps_known_pages_and_ignores_query_and_fragment() {
    let cases = [
      (ABOUT_BLANK, None),
      (ABOUT_NEWTAB, Some("New Tab")),
      (ABOUT_HELP, Some("Help")),
      (ABOUT_VERSION, Some("Version")),
      (ABOUT_GPU, Some("GPU")),
      (ABOUT_ERROR, Some("Navigation error")),
      (ABOUT_HISTORY, Some("History")),
      (ABOUT_BOOKMARKS, Some("Bookmarks")),
      (ABOUT_TEST_SCROLL, Some("Scroll Test")),
      (ABOUT_TEST_HEAVY, Some("Heavy Test")),
      (ABOUT_TEST_FORM, Some("Form Test")),
    ];

    for (url, expected_title) in cases {
      let html = html_for_about_url(&format!("{url}?q=1#frag")).unwrap();
      if let Some(expected_title) = expected_title {
        assert_eq!(
          extract_title(&html),
          Some(expected_title),
          "unexpected title for {url}"
        );
      }
    }
  }

  #[test]
  fn about_gpu_falls_back_to_unknown_when_headless() {
    let html = html_for_about_url(ABOUT_GPU).unwrap();
    assert!(html.contains("<title>GPU</title>"));
    assert!(html.contains(">unknown<"));
  }

  #[test]
  fn newtab_html_includes_color_scheme_and_primary_links() {
    let html = html_for_about_url(ABOUT_NEWTAB).unwrap();
    assert!(
      html.contains("color-scheme: light dark"),
      "expected about:newtab to set `color-scheme: light dark`"
    );

    for url in [
      "https://example.com/",
      ABOUT_HISTORY,
      ABOUT_BOOKMARKS,
      ABOUT_HELP,
      ABOUT_VERSION,
      ABOUT_GPU,
    ] {
      assert!(
        html.contains(url),
        "expected about:newtab HTML to link to {url}"
      );
    }
  }

  #[test]
  fn newtab_html_includes_search_form_for_default_engine() {
    use url::Url;

    let html = html_for_about_url(ABOUT_NEWTAB).unwrap();
    assert!(html.contains("<form"), "expected about:newtab to include a <form>");
    assert!(
      html.contains("method=\"get\""),
      "expected about:newtab search form to use method=get"
    );
    assert!(
      html.contains("role=\"search\""),
      "expected about:newtab search form to include role=search"
    );
    assert!(
      html.contains("type=\"search\""),
      "expected about:newtab search form to include a search <input>"
    );

    const NEEDLE: &str = "fastrender_test_query";
    let replaced = DEFAULT_SEARCH_ENGINE_TEMPLATE.replace("{query}", NEEDLE);
    let mut url = Url::parse(&replaced).expect("default search engine template must parse");

    let mut query_param = None;
    for (k, v) in url.query_pairs() {
      if v == NEEDLE {
        query_param = Some(k.into_owned());
        break;
      }
    }
    let query_param = query_param.expect("expected search template to include {query} as a query param value");

    url.set_query(None);
    url.set_fragment(None);
    let action = url.to_string();
    let safe_action = escape_html(&action);

    assert!(
      html.contains(&format!("action=\"{safe_action}\"")),
      "expected about:newtab HTML to submit to {action}, got: {html}"
    );
    assert!(
      html.contains(&format!("name=\"{query_param}\"")),
      "expected about:newtab HTML to include an <input> with name={query_param:?}, got: {html}"
    );
  }

  #[test]
  fn newtab_renders_snapshot_bookmarks_and_history() {
    use std::time::{Duration, UNIX_EPOCH};

    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot();

    set_about_page_snapshot(AboutPageSnapshot {
      bookmarks: vec![
        BookmarkSnapshot {
          title: Some("My <Bookmark>".to_string()),
          url: "https://example.com/".to_string(),
        },
        BookmarkSnapshot {
          title: None,
          url: "https://fallback.example/".to_string(),
        },
      ],
      history: vec![
        // Duplicate URL: only the most recently visited entry should render.
        HistorySnapshot {
          title: Some("Old title".to_string()),
          url: "https://dup.example/?a=1&b=2".to_string(),
          last_visited: Some(UNIX_EPOCH + Duration::from_secs(10)),
          visit_count: 1,
        },
        HistorySnapshot {
          title: Some("New title".to_string()),
          url: "https://dup.example/?a=1&b=2".to_string(),
          last_visited: Some(UNIX_EPOCH + Duration::from_secs(20)),
          visit_count: 3,
        },
        HistorySnapshot {
          title: Some("Visited & <Site>".to_string()),
          url: "https://visited.example/".to_string(),
          last_visited: Some(UNIX_EPOCH + Duration::from_secs(30)),
          visit_count: 1,
        },
      ],
      ..Default::default()
    });

    let html = html_for_about_url(ABOUT_NEWTAB).unwrap();
    // Bookmarks
    assert!(html.contains("https://example.com/"));
    assert!(html.contains("My &lt;Bookmark&gt;"));
    assert!(html.contains("https://fallback.example/"));

    // Recently visited
    assert!(html.contains("https://dup.example/?a=1&amp;b=2"));
    assert!(html.contains("New title"));
    assert!(!html.contains("Old title"));
    assert!(html.contains("Visited &amp; &lt;Site&gt;"));

    set_about_page_snapshot(before);
  }

  #[test]
  fn sync_history_from_global_history_store_updates_snapshot_and_newtab() {
    use std::time::{Duration, UNIX_EPOCH};

    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot();

    let mut store = GlobalHistoryStore::default();
    store.record(
      "https://example.test/a#one".to_string(),
      Some("A1".to_string()),
    );
    store.record("https://example.test/b".to_string(), Some("B".to_string()));
    store.record(
      "https://example.test/a#two".to_string(),
      Some("A2".to_string()),
    );

    for entry in store.entries.iter_mut() {
      match entry.url.as_str() {
        "https://example.test/a" => entry.visited_at_ms = 2000,
        "https://example.test/b" => entry.visited_at_ms = 1000,
        _ => {}
      }
    }

    set_about_page_snapshot(AboutPageSnapshot::default());
    sync_about_page_snapshot_history_from_global_history_store(&store);

    let snapshot = about_page_snapshot();
    assert_eq!(snapshot.history.len(), 2);

    let a = &snapshot.history[0];
    assert_eq!(a.url, "https://example.test/a");
    assert_eq!(a.title.as_deref(), Some("A2"));
    assert_eq!(a.visit_count, 2);
    assert_eq!(
      a.last_visited,
      UNIX_EPOCH.checked_add(Duration::from_millis(2000))
    );

    let b = &snapshot.history[1];
    assert_eq!(b.url, "https://example.test/b");
    assert_eq!(b.title.as_deref(), Some("B"));
    assert_eq!(b.visit_count, 1);
    assert_eq!(
      b.last_visited,
      UNIX_EPOCH.checked_add(Duration::from_millis(1000))
    );

    assert!(
      snapshot.history.iter().all(|e| !e.url.contains('#')),
      "expected fragments to be stripped in about-page history snapshot"
    );

    let html = html_for_about_url(ABOUT_NEWTAB).unwrap();
    for needle in ["https://example.test/a", "A2", "https://example.test/b", "B"] {
      assert!(html.contains(needle), "expected about:newtab to contain {needle}");
    }

    set_about_page_snapshot(before);
  }

  #[test]
  fn sync_history_from_global_history_store_preserves_fragment_stripping() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot();

    let mut store = GlobalHistoryStore::default();
    store.record("https://example.test/frag#section".to_string(), None);
    if let Some(entry) = store.entries.first_mut() {
      entry.visited_at_ms = 1;
    }

    set_about_page_snapshot(AboutPageSnapshot::default());
    sync_about_page_snapshot_history_from_global_history_store(&store);

    let snapshot = about_page_snapshot();
    assert_eq!(snapshot.history.len(), 1);
    assert_eq!(snapshot.history[0].url, "https://example.test/frag");
    assert!(
      !snapshot.history[0].url.contains('#'),
      "expected fragment to be stripped"
    );

    set_about_page_snapshot(before);
  }

  #[test]
  fn newtab_contains_static_default_links_when_snapshot_empty() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot();
    set_about_page_snapshot(AboutPageSnapshot::default());

    let html = html_for_about_url(ABOUT_NEWTAB).unwrap();
    for url in [ABOUT_HELP, ABOUT_VERSION, ABOUT_GPU] {
      assert!(
        html.contains(url),
        "expected about:newtab HTML to link to {url}"
      );
    }

    set_about_page_snapshot(before);
  }

  #[test]
  fn about_pages_include_shared_css_marker() {
    for url in [
      ABOUT_NEWTAB,
      ABOUT_HISTORY,
      ABOUT_BOOKMARKS,
      ABOUT_HELP,
      ABOUT_VERSION,
      ABOUT_GPU,
      ABOUT_ERROR,
      ABOUT_TEST_SCROLL,
      ABOUT_TEST_HEAVY,
      ABOUT_TEST_FORM,
    ] {
      let html = html_for_about_url(url).unwrap();
      assert!(
        html.contains(ABOUT_SHARED_CSS_MARKER),
        "expected {url} to include shared about-page CSS marker"
      );
    }

    let html = error_page_html("Navigation error", "details", None);
    assert!(html.contains(ABOUT_SHARED_CSS_MARKER));
  }

  #[test]
  fn about_pages_use_chrome_accent_in_css_variables() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot();

    set_about_page_snapshot(AboutPageSnapshot {
      chrome_accent: Some(RgbaColor::new(255, 0, 255, 0xFF)),
      ..Default::default()
    });

    let html = html_for_about_url(ABOUT_HELP).unwrap();
    for needle in [
      "--about-focus: rgba(255, 0, 255, 0.65);",
      "--about-accent-border: rgba(255, 0, 255, 0.55);",
      "--about-accent-bg: rgba(255, 0, 255, 0.18);",
      "radial-gradient(900px circle at 20% 0%, var(--about-accent-bg)",
      "@media (prefers-color-scheme: dark)",
    ] {
      assert!(
        html.contains(needle),
        "expected about page HTML to include themed accent CSS, missing {needle:?}"
      );
    }

    set_about_page_snapshot(before);
  }

  #[test]
  fn error_page_html_includes_retry_link_and_escapes_url() {
    let retry_url = "https://example.com/?a=1&b=<x>\"'";
    let html = error_page_html("Navigation failed", "boom", Some(retry_url));

    let escaped = "https://example.com/?a=1&amp;b=&lt;x&gt;&quot;&#39;";
    assert!(
      html.contains(&format!("href=\"{escaped}\"")),
      "expected escaped retry URL in href"
    );
    assert!(
      html.contains(&format!("<code>{escaped}</code>")),
      "expected escaped retry URL in visible URL line"
    );
    assert!(
      html.contains(">Retry</a>"),
      "expected retry button label to be present"
    );
    assert!(
      !html.contains(retry_url),
      "raw retry URL should not appear unescaped in HTML"
    );
  }

  #[test]
  fn error_page_html_hides_raw_error_in_details_element() {
    let html = error_page_html(
      "Navigation failed",
      "network failed: <timeout>",
      Some("https://example.com/"),
    );
    assert!(html.contains("<details>"));
    assert!(html.contains("<summary>Technical details</summary>"));
    assert!(
      html.contains("<pre>network failed: &lt;timeout&gt;</pre>"),
      "expected HTML-escaped raw error message inside <details>"
    );
  }

  #[test]
  fn help_page_includes_bookmarks_and_history_shortcuts() {
    let html = html_for_about_url(ABOUT_HELP).unwrap();

    for needle in [
      // Bookmarks.
      "<kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>D</kbd>",
      "Toggle bookmark",
      "<kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>O</kbd>",
      "Show bookmarks manager",
      // History.
      "<kbd>Ctrl</kbd>+<kbd>H</kbd>",
      "<kbd>Cmd</kbd>+<kbd>Y</kbd>",
      "<kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>H</kbd>",
      "Show history",
    ] {
      assert!(
        html.contains(needle),
        "expected about:help HTML to contain {needle:?}"
      );
    }
  }

  #[test]
  fn about_help_mentions_search_and_omnibox_suggestions() {
    let html = html_for_about_url(ABOUT_HELP).unwrap();
    assert!(
      html.contains("default search engine"),
      "expected about:help HTML to mention search fallback, got: {html}"
    );
    assert!(
      html.contains("omnibox") && html.contains("ArrowDown"),
      "expected about:help HTML to mention omnibox suggestions, got: {html}"
    );
  }

  #[test]
  fn escape_html_escapes_html_special_chars() {
    assert_eq!(
      escape_html("&<>\"'"),
      "&amp;&lt;&gt;&quot;&#39;".to_string()
    );
  }

  #[test]
  fn about_history_html_escapes_urls_and_titles() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot();

    set_about_page_snapshot(AboutPageSnapshot {
      bookmarks: Vec::new(),
      history: vec![HistorySnapshot {
        title: Some("<script>alert(1)</script>".to_string()),
        url: "https://example.com/?a=1&b=<x>\"'".to_string(),
        last_visited: None,
        visit_count: 1,
      }],
      ..Default::default()
    });

    let html = html_for_about_url(ABOUT_HISTORY).unwrap();
    assert!(
      html.contains("https://example.com/?a=1&amp;b=&lt;x&gt;&quot;&#39;"),
      "expected URL to be HTML escaped"
    );
    assert!(
      html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
      "expected title to be HTML escaped"
    );
    assert!(
      !html.contains("<script>alert(1)</script>"),
      "raw title should not appear unescaped"
    );

    set_about_page_snapshot(before);
  }

  #[test]
  fn about_history_filters_by_query_param() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot();

    set_about_page_snapshot(AboutPageSnapshot {
      bookmarks: Vec::new(),
      history: vec![
        HistorySnapshot {
          title: Some("Rust".to_string()),
          url: "https://www.rust-lang.org/".to_string(),
          last_visited: None,
          visit_count: 1,
        },
        HistorySnapshot {
          title: Some("Example Domain".to_string()),
          url: "https://example.com/".to_string(),
          last_visited: None,
          visit_count: 1,
        },
      ],
      ..Default::default()
    });

    let html = html_for_about_url("about:history?q=rust").unwrap();
    assert!(html.contains("https://www.rust-lang.org/"));
    assert!(!html.contains("https://example.com/"));

    set_about_page_snapshot(before);
  }

  #[test]
  fn about_bookmarks_filters_and_includes_entries() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot();

    set_about_page_snapshot(AboutPageSnapshot {
      bookmarks: vec![
        BookmarkSnapshot {
          title: None,
          url: "https://example.com/".to_string(),
        },
        BookmarkSnapshot {
          title: None,
          url: "https://www.rust-lang.org/".to_string(),
        },
      ],
      history: Vec::new(),
      ..Default::default()
    });

    let html_all = html_for_about_url(ABOUT_BOOKMARKS).unwrap();
    assert!(html_all.contains("https://example.com/"));
    assert!(html_all.contains("https://www.rust-lang.org/"));

    let html_filtered = html_for_about_url("about:bookmarks?q=rust").unwrap();
    assert!(!html_filtered.contains("https://example.com/"));
    assert!(html_filtered.contains("https://www.rust-lang.org/"));

    set_about_page_snapshot(before);
  }

  #[test]
  fn about_snapshot_from_stores_includes_nested_bookmarks() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot();

    let mut bookmarks = BookmarkStore::default();
    let folder = bookmarks.create_folder("Folder".to_string(), None).unwrap();
    bookmarks
      .add(
        "https://example.com/nested".to_string(),
        Some("Nested Bookmark".to_string()),
        Some(folder),
      )
      .unwrap();

    let history = GlobalHistoryStore::default();
    set_about_snapshot_from_stores(&bookmarks, &history);

    let snapshot = about_page_snapshot();
    assert!(
      snapshot
        .bookmarks
        .iter()
        .any(|bookmark| bookmark.url == "https://example.com/nested"),
      "expected nested bookmark to appear in about-page snapshot"
    );

    let html = html_for_about_url(ABOUT_BOOKMARKS).unwrap();
    assert!(
      html.contains("https://example.com/nested"),
      "expected about:bookmarks HTML to include nested bookmark URL"
    );

    set_about_page_snapshot(before);
  }

  #[test]
  fn suggest_about_pages_matches_prefix_and_includes_unrecorded_pages() {
    let suggestions = suggest_about_pages("about:");
    for url in [
      ABOUT_NEWTAB,
      ABOUT_BLANK,
      ABOUT_HELP,
      ABOUT_VERSION,
      ABOUT_GPU,
      ABOUT_ERROR,
      ABOUT_HISTORY,
      ABOUT_BOOKMARKS,
    ] {
      assert!(
        suggestions.contains(&url),
        "expected suggestions to contain {url}, got {suggestions:?}"
      );
    }

    assert!(
      suggest_about_pages("help").is_empty(),
      "expected non-about prefix not to suggest about pages"
    );
    assert!(
      suggest_about_pages("ABOUT:H").contains(&ABOUT_HELP),
      "expected suggestions to be case-insensitive"
    );
  }
}
