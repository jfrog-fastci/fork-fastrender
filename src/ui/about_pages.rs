pub const ABOUT_BLANK: &str = "about:blank";
pub const ABOUT_NEWTAB: &str = "about:newtab";
pub const ABOUT_SETTINGS: &str = "about:settings";
pub const ABOUT_HELP: &str = "about:help";
pub const ABOUT_VERSION: &str = "about:version";
pub const ABOUT_GPU: &str = "about:gpu";
pub const ABOUT_PROCESSES: &str = "about:processes";
pub const ABOUT_ERROR: &str = "about:error";
pub const ABOUT_HISTORY: &str = "about:history";
pub const ABOUT_BOOKMARKS: &str = "about:bookmarks";
pub const ABOUT_TEST_SCROLL: &str = "about:test-scroll";
pub const ABOUT_TEST_HEAVY: &str = "about:test-heavy";
pub const ABOUT_TEST_LAYOUT_STRESS: &str = "about:test-layout-stress";
pub const ABOUT_TEST_FORM: &str = "about:test-form";
pub const ABOUT_SHARED_CSS_URL: &str = "chrome://styles/about.css";

/// Known `about:` page URLs.
///
/// This list exists for omnibox/autocomplete providers so built-in pages can be suggested even when
/// they are intentionally excluded from visited history (e.g. `about:newtab`, `about:error`).
pub const ABOUT_PAGE_URLS: &[&str] = &[
  ABOUT_BLANK,
  ABOUT_NEWTAB,
  ABOUT_SETTINGS,
  ABOUT_HELP,
  ABOUT_VERSION,
  ABOUT_GPU,
  ABOUT_PROCESSES,
  ABOUT_ERROR,
  ABOUT_HISTORY,
  ABOUT_BOOKMARKS,
  ABOUT_TEST_SCROLL,
  ABOUT_TEST_HEAVY,
  ABOUT_TEST_LAYOUT_STRESS,
  ABOUT_TEST_FORM,
];

use parking_lot::RwLock;
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use super::string_match::contains_ascii_case_insensitive;
use crate::ui::html_escape::escape_html;
use crate::ui::theme_parsing::{
  RgbaColor, ENV_BROWSER_ACCENT, ENV_BROWSER_HIGH_CONTRAST, ENV_BROWSER_THEME,
};
use crate::ui::url::DEFAULT_SEARCH_ENGINE_TEMPLATE;
use crate::ui::{BookmarkId, BookmarkNode, BookmarkStore, GlobalHistoryStore};

#[derive(Debug, Clone, Default)]
pub struct AboutPageSnapshot {
  pub bookmarks: Vec<BookmarkSnapshot>,
  /// Global (cross-tab) browsing history.
  ///
  /// This is expected to be ordered by recency (newest first), but about pages should remain robust
  /// even when callers provide unsorted data.
  pub history: Vec<HistorySnapshot>,
  /// Best-effort snapshot of currently open tabs, for debug `about:` pages.
  ///
  /// This is optional (front-ends may not populate it) and intentionally kept independent of any UI
  /// toolkit types so it can be updated across the UI↔worker boundary.
  pub open_tabs: Vec<OpenTabSnapshot>,
  /// Effective browser chrome accent color (used to theme `about:` pages).
  pub chrome_accent: Option<RgbaColor>,
  /// Displayable path to the persisted browser session JSON (when available).
  ///
  /// Stored as a `String` (rather than `PathBuf`) so that `AboutPageSnapshot` stays lightweight and
  /// does not require extra serde/path feature plumbing.
  pub session_path: Option<String>,
  /// Displayable path to the persisted bookmarks JSON (when available).
  pub bookmarks_path: Option<String>,
  /// Displayable path to the persisted browsing history JSON (when available).
  pub history_path: Option<String>,
  /// Displayable path to the active download directory (when available).
  pub download_dir: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OpenTabSnapshot {
  /// Best-effort identifier for the window that owns this tab.
  ///
  /// The windowed browser populates this with a debug string derived from the platform window id.
  pub window_id: Option<String>,
  pub tab_id: u64,
  pub url: String,
  /// Best-effort tab title (sanitized by the UI), preferring the last committed title when
  /// available.
  pub title: Option<String>,
  /// Derived site key for the current URL (best-effort).
  pub site_key: Option<String>,
  /// Renderer process identifier assigned to this tab (if multiprocess is enabled).
  pub renderer_process: Option<u64>,
  /// Whether this tab is currently the active tab in its owning window.
  pub is_active: bool,
  /// Whether the UI believes this tab is currently loading.
  pub loading: bool,
  /// Whether the tab is currently in a crashed state (user-visible crash page).
  pub crashed: bool,
  /// Whether the browser UI watchdog considers this tab unresponsive.
  pub unresponsive: bool,
  /// Whether the renderer was terminated/detached due to a protocol violation.
  pub renderer_crashed: bool,
  /// Optional crash reason, if the tab is in a crashed state.
  pub crash_reason: Option<String>,
  /// Optional renderer protocol violation detail (best-effort).
  pub renderer_protocol_violation: Option<String>,
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

static ABOUT_PAGE_SNAPSHOT: OnceLock<RwLock<Arc<AboutPageSnapshot>>> = OnceLock::new();

fn about_page_snapshot_lock() -> &'static RwLock<Arc<AboutPageSnapshot>> {
  ABOUT_PAGE_SNAPSHOT.get_or_init(|| RwLock::new(Arc::new(AboutPageSnapshot::default())))
}

pub fn about_page_snapshot() -> Arc<AboutPageSnapshot> {
  about_page_snapshot_lock().read().clone()
}

#[cfg(feature = "browser_ui")]
pub fn set_about_page_snapshot(snapshot: AboutPageSnapshot) {
  *about_page_snapshot_lock().write() = Arc::new(snapshot);
}

#[cfg(feature = "browser_ui")]
pub fn set_about_snapshot_from_stores(bookmarks: &BookmarkStore, history: &GlobalHistoryStore) {
  // Preserve any separately-updated chrome settings (e.g. accent color) across snapshot refreshes.
  // Similarly, keep optional debug snapshots (open tabs) intact unless explicitly overwritten by
  // callers.
  let new_bookmarks = bookmark_snapshots_from_store(bookmarks);
  let new_history = history_snapshots_from_global_history_store(history);

  let mut guard = about_page_snapshot_lock().write();
  if let Some(snapshot) = Arc::get_mut(&mut *guard) {
    snapshot.bookmarks = new_bookmarks;
    snapshot.history = new_history;
    return;
  }

  let chrome_accent = guard.chrome_accent;
  let open_tabs = guard.open_tabs.clone();
  let session_path = guard.session_path.clone();
  let bookmarks_path = guard.bookmarks_path.clone();
  let history_path = guard.history_path.clone();
  let download_dir = guard.download_dir.clone();
  *guard = Arc::new(AboutPageSnapshot {
    bookmarks: new_bookmarks,
    history: new_history,
    open_tabs,
    chrome_accent,
    session_path,
    bookmarks_path,
    history_path,
    download_dir,
  });
}

#[cfg(feature = "browser_ui")]
pub fn sync_about_page_snapshot_history_from_global_history_store(store: &GlobalHistoryStore) {
  let history = history_snapshots_from_global_history_store(store);
  let mut guard = about_page_snapshot_lock().write();
  if let Some(snapshot) = Arc::get_mut(&mut *guard) {
    snapshot.history = history;
    return;
  }

  let chrome_accent = guard.chrome_accent;
  let bookmarks = guard.bookmarks.clone();
  let open_tabs = guard.open_tabs.clone();
  let session_path = guard.session_path.clone();
  let bookmarks_path = guard.bookmarks_path.clone();
  let history_path = guard.history_path.clone();
  let download_dir = guard.download_dir.clone();
  *guard = Arc::new(AboutPageSnapshot {
    bookmarks,
    history,
    open_tabs,
    chrome_accent,
    session_path,
    bookmarks_path,
    history_path,
    download_dir,
  });
}

#[cfg(feature = "browser_ui")]
pub fn sync_about_page_snapshot_bookmarks_from_bookmark_store(store: &BookmarkStore) {
  let bookmarks = bookmark_snapshots_from_store(store);
  let mut guard = about_page_snapshot_lock().write();
  if let Some(snapshot) = Arc::get_mut(&mut *guard) {
    snapshot.bookmarks = bookmarks;
    return;
  }

  let chrome_accent = guard.chrome_accent;
  let history = guard.history.clone();
  let open_tabs = guard.open_tabs.clone();
  let session_path = guard.session_path.clone();
  let bookmarks_path = guard.bookmarks_path.clone();
  let history_path = guard.history_path.clone();
  let download_dir = guard.download_dir.clone();
  *guard = Arc::new(AboutPageSnapshot {
    bookmarks,
    history,
    open_tabs,
    chrome_accent,
    session_path,
    bookmarks_path,
    history_path,
    download_dir,
  });
}

#[cfg(feature = "browser_ui")]
pub fn sync_about_page_snapshot_chrome_accent(accent: Option<RgbaColor>) {
  let mut guard = about_page_snapshot_lock().write();
  if let Some(snapshot) = Arc::get_mut(&mut *guard) {
    snapshot.chrome_accent = accent;
    return;
  }

  let bookmarks = guard.bookmarks.clone();
  let history = guard.history.clone();
  let open_tabs = guard.open_tabs.clone();
  let session_path = guard.session_path.clone();
  let bookmarks_path = guard.bookmarks_path.clone();
  let history_path = guard.history_path.clone();
  let download_dir = guard.download_dir.clone();
  *guard = Arc::new(AboutPageSnapshot {
    bookmarks,
    history,
    open_tabs,
    chrome_accent: accent,
    session_path,
    bookmarks_path,
    history_path,
    download_dir,
  });
}

#[cfg(feature = "browser_ui")]
pub fn sync_about_page_snapshot_download_dir(download_dir: Option<String>) {
  let mut guard = about_page_snapshot_lock().write();
  if let Some(snapshot) = Arc::get_mut(&mut *guard) {
    snapshot.download_dir = download_dir;
    return;
  }

  let chrome_accent = guard.chrome_accent;
  let bookmarks = guard.bookmarks.clone();
  let history = guard.history.clone();
  let open_tabs = guard.open_tabs.clone();
  let session_path = guard.session_path.clone();
  let bookmarks_path = guard.bookmarks_path.clone();
  let history_path = guard.history_path.clone();
  *guard = Arc::new(AboutPageSnapshot {
    bookmarks,
    history,
    open_tabs,
    chrome_accent,
    session_path,
    bookmarks_path,
    history_path,
    download_dir,
  });
}

pub fn sync_about_page_snapshot_open_tabs(open_tabs: Vec<OpenTabSnapshot>) {
  let mut guard = about_page_snapshot_lock().write();
  if let Some(snapshot) = Arc::get_mut(&mut *guard) {
    snapshot.open_tabs = open_tabs;
    return;
  }

  let chrome_accent = guard.chrome_accent;
  let bookmarks = guard.bookmarks.clone();
  let history = guard.history.clone();
  let session_path = guard.session_path.clone();
  let bookmarks_path = guard.bookmarks_path.clone();
  let history_path = guard.history_path.clone();
  let download_dir = guard.download_dir.clone();
  *guard = Arc::new(AboutPageSnapshot {
    bookmarks,
    history,
    open_tabs,
    chrome_accent,
    session_path,
    bookmarks_path,
    history_path,
    download_dir,
  });
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

#[cfg(test)]
const ABOUT_SHARED_CSS_MARKER: &str = "FASTR_ABOUT_SHARED_CSS";

#[cfg(test)]
fn about_shared_css() -> &'static str {
  const CSS: &str = include_str!("../../assets/chrome/about.css");
  debug_assert!(
    CSS.contains(ABOUT_SHARED_CSS_MARKER),
    "ABOUT_SHARED_CSS_MARKER must be present in shared about-page CSS"
  );
  CSS
}

fn about_header_html(current: &str) -> String {
  let items = [
    (ABOUT_NEWTAB, "New tab"),
    (ABOUT_HISTORY, "History"),
    (ABOUT_BOOKMARKS, "Bookmarks"),
    (ABOUT_SETTINGS, "Settings"),
    (ABOUT_HELP, "Help"),
    (ABOUT_VERSION, "Version"),
    (ABOUT_GPU, "GPU"),
    (ABOUT_PROCESSES, "Processes"),
  ];
  let mut links = String::with_capacity(256);
  for (url, label) in items {
    let aria = if url == current {
      " aria-current=\"page\""
    } else {
      ""
    };
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

// Default accent (matches the legacy about-page palette).
const DEFAULT_ABOUT_ACCENT: RgbaColor = RgbaColor::new(10, 132, 255, 0xFF);

fn about_theme_css() -> String {
  let accent = about_page_snapshot()
    .chrome_accent
    .unwrap_or(DEFAULT_ABOUT_ACCENT);
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
    <link rel=\"stylesheet\" href=\"{ABOUT_SHARED_CSS_URL}\">
    <style>
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
    ABOUT_SETTINGS => Some(settings_html(url)),
    ABOUT_HELP => Some(help_html()),
    ABOUT_VERSION => Some(version_html()),
    ABOUT_GPU => Some(gpu_html()),
    ABOUT_PROCESSES => Some(processes_html(url)),
    ABOUT_ERROR => Some(error_html("Navigation error", None, None)),
    ABOUT_HISTORY => Some(history_html(url)),
    ABOUT_BOOKMARKS => Some(bookmarks_html(url)),
    ABOUT_TEST_SCROLL => Some(test_scroll_html()),
    ABOUT_TEST_HEAVY => Some(test_heavy_html()),
    ABOUT_TEST_LAYOUT_STRESS => Some(test_layout_stress_html()),
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
  let omnibox_modifier = if cfg!(target_os = "macos") {
    "Cmd"
  } else {
    "Ctrl"
  };
  use std::fmt::Write;

  let search_form =
    search_form_config_from_template(DEFAULT_SEARCH_ENGINE_TEMPLATE).unwrap_or(SearchFormConfig {
      action: "https://duckduckgo.com/".to_string(),
      query_param: "q".to_string(),
      hidden_inputs: Vec::new(),
    });
  let safe_search_action = escape_html(&search_form.action);
  let safe_search_param = escape_html(&search_form.query_param);
  let mut hidden_inputs_html = String::new();
  for (key, value) in search_form.hidden_inputs.into_iter() {
    let safe_key = escape_html(&key);
    let safe_value = escape_html(&value);
    let _ = write!(
      hidden_inputs_html,
      r#"<input type="hidden" name="{safe_key}" value="{safe_value}">"#
    );
  }

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

    let slot = merged_by_url
      .entry(url.to_string())
      .or_insert_with(|| HistoryMerged {
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
    format!(r#"<div class="about-actions" aria-label="Recently visited">{history_tiles}</div>"#)
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
        <span class="about-kbd">{omnibox_modifier}</span>
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
        <a class="about-tile" href="about:settings">
          <div class="label">Settings</div>
          <div class="url">about:settings</div>
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
        <a class="about-tile" href="about:processes">
          <div class="label">Processes</div>
          <div class="url">about:processes</div>
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
      hidden_inputs_html = hidden_inputs_html,
      omnibox_modifier = omnibox_modifier,
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

const ABOUT_SETTINGS_CSS: &str = r#"
.settings-table td { padding: 6px 10px 6px 0; vertical-align: middle; }
.muted { color: var(--about-muted); }
.swatch {
  display: inline-block;
  width: 14px;
  height: 14px;
  border-radius: 5px;
  border: 1px solid var(--about-border);
  background: var(--swatch, var(--about-focus-ring));
  margin-right: 8px;
  vertical-align: -2px;
}
"#;

fn settings_html(_full_url: &str) -> String {
  let snapshot = about_page_snapshot();
  let effective_accent = snapshot.chrome_accent.unwrap_or(DEFAULT_ABOUT_ACCENT);
  let accent_source = if snapshot.chrome_accent.is_some() {
    "browser"
  } else {
    "default"
  };

  let hex = effective_accent.to_hex_string();
  let safe_hex = escape_html(&hex);
  let alpha = (effective_accent.a as f64) / 255.0;
  let rgba = format!(
    "rgba({}, {}, {}, {:.3})",
    effective_accent.r, effective_accent.g, effective_accent.b, alpha
  );
  let safe_rgba = escape_html(&rgba);
  let safe_accent_source = escape_html(accent_source);

  let safe_env_theme = escape_html(ENV_BROWSER_THEME);
  let safe_env_accent = escape_html(ENV_BROWSER_ACCENT);
  let safe_env_high_contrast = escape_html(ENV_BROWSER_HIGH_CONTRAST);

  let mut path_rows = String::new();
  for (label, value) in [
    ("Session", snapshot.session_path.as_deref()),
    ("Bookmarks", snapshot.bookmarks_path.as_deref()),
    ("History", snapshot.history_path.as_deref()),
    ("Download directory", snapshot.download_dir.as_deref()),
  ] {
    let safe_label = escape_html(label);
    if let Some(value) = value.filter(|v| !v.trim().is_empty()) {
      let safe_value = escape_html(value);
      path_rows.push_str(&format!(
        "<tr><td>{safe_label}</td><td><code>{safe_value}</code></td></tr>"
      ));
    } else {
      path_rows.push_str(&format!(
        "<tr><td>{safe_label}</td><td><span class=\"muted\">unknown</span></td></tr>"
      ));
    }
  }

  let pages = [
    (ABOUT_NEWTAB, "New tab"),
    (ABOUT_HISTORY, "History"),
    (ABOUT_BOOKMARKS, "Bookmarks"),
    (ABOUT_HELP, "Help"),
    (ABOUT_VERSION, "Version"),
    (ABOUT_GPU, "GPU"),
  ];
  use std::fmt::Write;
  let mut page_links = String::new();
  for (url, label) in pages {
    let safe_url = escape_html(url);
    let safe_label = escape_html(label);
    let _ = write!(
      page_links,
      r#"<a class="about-tile" href="{safe_url}"><div class="label">{safe_label}</div><div class="url">{safe_url}</div></a>"#
    );
  }

  about_layout_html(
    "Settings",
    ABOUT_SETTINGS,
    &format!(
      r#"<h1>Settings</h1>
      <p>This is an offline <code>{ABOUT_SETTINGS}</code> page.</p>

      <h2>Appearance</h2>
      <table class="settings-table">
        <tr>
          <td>Accent color</td>
          <td><span class="swatch" style="--swatch: {rgba};"></span><code>{safe_hex}</code> <span class="muted">({safe_accent_source})</span></td>
        </tr>
        <tr><td>RGBA</td><td><code>{safe_rgba}</code></td></tr>
      </table>

      <h2>Overrides</h2>
      <p class="muted">
        Appearance can be overridden via environment variables:
      </p>
       <ul>
         <li><code>{safe_env_theme}</code> — <code>system</code>/<code>light</code>/<code>dark</code></li>
         <li><code>{safe_env_accent}</code> — hex color (<code>#RRGGBB</code> or <code>#RRGGBBAA</code>)</li>
         <li><code>{safe_env_high_contrast}</code> — enable high-contrast UI</li>
       </ul>
 
       <h2>Runtime paths</h2>
       <table class="settings-table">
         {path_rows}
       </table>
 
       <h2>Built-in pages</h2>
       <div class="about-actions" aria-label="Built-in pages">{page_links}</div>"#,
      rgba = rgba,
      safe_hex = safe_hex,
      safe_rgba = safe_rgba,
      safe_accent_source = safe_accent_source,
      safe_env_theme = safe_env_theme,
      safe_env_accent = safe_env_accent,
      safe_env_high_contrast = safe_env_high_contrast,
      path_rows = path_rows,
      page_links = page_links,
    ),
    ABOUT_SETTINGS_CSS,
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
          <li><kbd>Alt</kbd>+<kbd>Enter</kbd> (Win/Linux); <kbd>Option</kbd>+<kbd>Enter</kbd> (macOS) — Open omnibox input in a new tab</li>
          <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>F</kbd> — Find in page</li>
          <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>T</kbd> — New tab</li>
          <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>T</kbd> — Reopen last closed tab</li>
          <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>W</kbd> — Close tab</li>
          <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>Tab</kbd> / <kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>Tab</kbd> — Next/prev tab</li>
          <li><kbd>Alt</kbd>+<kbd>Left</kbd> / <kbd>Alt</kbd>+<kbd>Right</kbd> (Win/Linux); <kbd>Cmd</kbd>+<kbd>[</kbd> / <kbd>Cmd</kbd>+<kbd>]</kbd> (macOS) — Back/forward</li>
          <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>R</kbd> / <kbd>F5</kbd> — Reload</li>
          <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>+</kbd> / <kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>-</kbd> / <kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>0</kbd> — Zoom in/out/reset</li>
          <li><kbd>F11</kbd> (Win/Linux); <kbd>Ctrl</kbd>+<kbd>Cmd</kbd>+<kbd>F</kbd> (macOS) — Toggle fullscreen</li>
          <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>1</kbd>…<kbd>9</kbd> — Activate tab (9 = last)</li>
         <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>D</kbd> — Toggle bookmark</li>
         <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>B</kbd> — Toggle bookmarks bar</li>
         <li><kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>O</kbd> — Show bookmarks manager</li>
         <li><kbd>Ctrl</kbd>+<kbd>J</kbd> (Win/Linux); <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>J</kbd> (macOS) — Show downloads (<code>Window → Show Downloads…</code>)</li>
         <li><kbd>Ctrl</kbd>+<kbd>H</kbd> (Win/Linux); <kbd>Cmd</kbd>+<kbd>Y</kbd> / <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>H</kbd> (macOS) — Show history</li>
       </ul>

       <h2>Built-in pages</h2>
       <ul>
         <li><a href=\"about:newtab\">about:newtab</a></li>
         <li><a href=\"about:history\">about:history</a></li>
         <li><a href=\"about:bookmarks\">about:bookmarks</a></li>
         <li><a href=\"about:settings\">about:settings</a></li>
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

fn best_effort_site_for_url(url: &str) -> String {
  let trimmed = url.trim();
  if trimmed.is_empty() {
    return "unknown".to_string();
  }
  let parsed = match url::Url::parse(trimmed) {
    Ok(url) => url,
    Err(_) => return "unknown".to_string(),
  };

  match parsed.scheme() {
    "http" | "https" => parsed
      .host_str()
      .map(str::to_string)
      .unwrap_or_else(|| "unknown".to_string()),
    "file" => "file".to_string(),
    other => other.to_string(),
  }
}

fn best_effort_site_key_for_url(url: &str) -> String {
  let trimmed = url.trim();
  if trimmed.is_empty() {
    return "unknown".to_string();
  }
  crate::ui::SiteKey::from_url(trimmed)
    .map(|key| key.to_string())
    .unwrap_or_else(|_| "unknown".to_string())
}

fn processes_html(full_url: &str) -> String {
  let snapshot = about_page_snapshot();

  let query = about_query_param(full_url, "q")
    .unwrap_or_default()
    .trim()
    .to_string();
  let query_lower = query.to_ascii_lowercase();
  let tokens: Vec<&str> = query_lower
    .split_whitespace()
    .filter(|t| !t.is_empty())
    .collect();
  let safe_query = escape_html(&query);
  let filter_href = |q: &str| -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("q", q);
    format!("{ABOUT_PROCESSES}?{}", serializer.finish())
  };

  let process_model = crate::ui::process_assignment_config::process_model_from_env();
  let process_model_label = match process_model {
    crate::ui::process_assignment::ProcessModel::PerTab => "tab",
    crate::ui::process_assignment::ProcessModel::PerSiteKey => "site",
  };
  let site_isolation_enabled = matches!(
    process_model,
    crate::ui::process_assignment::ProcessModel::PerSiteKey
  );
  let network_mode_html = if cfg!(feature = "direct_network") {
    "<code>direct</code> <span class=\"muted\">(direct_network)</span>"
  } else {
    "<code>ipc</code> <span class=\"muted\">(direct_network disabled)</span>"
  };
  let websocket_mode_html = if cfg!(feature = "direct_websocket") {
    "<code>direct</code> <span class=\"muted\">(direct_websocket)</span>"
  } else {
    "<code>ipc</code> <span class=\"muted\">(direct_websocket disabled)</span>"
  };
  let process_model_env_html = {
    let raw = std::env::var(crate::ui::process_assignment_config::ENV_PROCESS_MODEL)
      .ok()
      .unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
      format!(
        "<span class=\"muted\">({}=default)</span>",
        crate::ui::process_assignment_config::ENV_PROCESS_MODEL
      )
    } else {
      format!(
        "<span class=\"muted\">({}={})</span>",
        crate::ui::process_assignment_config::ENV_PROCESS_MODEL,
        escape_html(raw)
      )
    }
  };
  #[cfg(any(test, feature = "browser_ui"))]
  let registry_stats_html = {
    let live = crate::multiprocess::renderer_process_count_for_test();
    let spawned = crate::multiprocess::renderer_process_spawn_count_for_test();
    format!(" · Registry live: <code>{live}</code> · Registry spawned: <code>{spawned}</code>")
  };
  #[cfg(not(any(test, feature = "browser_ui")))]
  let registry_stats_html = String::new();

  #[derive(Default)]
  struct RendererProcessGroup {
    tabs: Vec<(Option<String>, u64)>,
    sites: std::collections::BTreeSet<String>,
  }

  #[derive(Default)]
  struct SiteGroup {
    tabs: Vec<(Option<String>, u64)>,
    renderer_processes: std::collections::BTreeSet<u64>,
    has_unassigned: bool,
  }

  let mut windows = std::collections::BTreeSet::<String>::new();
  let mut unknown_windows = 0usize;
  let mut renderer_processes = std::collections::BTreeMap::<u64, RendererProcessGroup>::new();
  let mut unassigned_tabs = 0usize;
  let mut site_groups = std::collections::BTreeMap::<String, SiteGroup>::new();
  let mut unknown_sites = 0usize;
  let mut loading_tabs = 0usize;
  let mut crashed_tabs = 0usize;
  let mut unresponsive_tabs = 0usize;
  let mut renderer_crashed_tabs = 0usize;
  let mut visible_tabs = 0usize;

  let mut rows = String::new();
  if snapshot.open_tabs.is_empty() {
    rows.push_str("<tr><td colspan=\"7\" class=\"empty\">No tab snapshot is available.</td></tr>");
  } else {
    for tab in &snapshot.open_tabs {
      let safe_window = escape_html(tab.window_id.as_deref().unwrap_or("-"));
      let safe_url = escape_html(&tab.url);
      let title = tab
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
      let site_display = best_effort_site_for_url(&tab.url);
      let site_key = tab
        .site_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| best_effort_site_key_for_url(&tab.url));
      let site_cell = if site_display == "unknown" {
        "<span class=\"muted\">unknown</span>".to_string()
      } else {
        format!("<code>{}</code>", escape_html(&site_display))
      };
      let renderer = "<span class=\"muted\">(not implemented)</span>".to_string();
      let url_cell = match title {
        Some(title) => format!(
          "<div class=\"tab-title\">{}</div>\
           <div><a href=\"{}\"><code>{}</code></a></div>",
          escape_html(title),
          safe_url,
          safe_url
        ),
        None => format!("<a href=\"{}\"><code>{}</code></a>", safe_url, safe_url),
      };
      let network_cell = "<span class=\"muted\">(not implemented)</span>".to_string();
      if !tokens.is_empty() {
        use std::fmt::Write;
        let mut searchable = String::new();
        let renderer_id = tab
          .renderer_process
          .map(|id| id.to_string())
          .unwrap_or_else(|| "unassigned".to_string());
        let _ = write!(
          searchable,
          "{} {} {} {} {} {} {}",
          tab.window_id.as_deref().unwrap_or(""),
          tab.tab_id,
          tab.url,
          site_key,
          site_display,
          renderer_id,
          title.unwrap_or(""),
        );
        searchable.push_str(&format!(
          " window:{} tab:{} site:{} site_display:{} renderer:{}",
          tab.window_id.as_deref().unwrap_or(""),
          tab.tab_id,
          site_key,
          site_display,
          renderer_id
        ));
        for (enabled, label) in [
          (tab.is_active, "active"),
          (tab.loading, "loading"),
          (tab.unresponsive, "unresponsive"),
          (tab.crashed, "crashed"),
          (tab.renderer_crashed, "renderer_crashed"),
        ] {
          if enabled {
            searchable.push(' ');
            searchable.push_str(label);
          }
        }
        if let Some(reason) = tab.crash_reason.as_deref().filter(|s| !s.trim().is_empty()) {
          searchable.push(' ');
          searchable.push_str(reason);
        }
        if let Some(violation) = tab
          .renderer_protocol_violation
          .as_deref()
          .filter(|s| !s.trim().is_empty())
        {
          searchable.push(' ');
          searchable.push_str(violation);
        }
        if !tokens
          .iter()
          .all(|t| contains_ascii_case_insensitive(&searchable, t))
        {
          continue;
        }
      }
      visible_tabs += 1;
      let state = {
        let mut out = String::new();
        let mut any_flag = false;
        for (enabled, label) in [
          (tab.is_active, "active"),
          (tab.loading, "loading"),
          (tab.unresponsive, "unresponsive"),
          (tab.crashed, "crashed"),
          (tab.renderer_crashed, "renderer_crashed"),
        ] {
          if !enabled {
            continue;
          }
          any_flag = true;
          if !out.is_empty() {
            out.push(' ');
          }
          out.push_str(&format!("<code>{label}</code>"));
        }
        if !any_flag {
          out.push_str("<span class=\"muted\">ok</span>");
        }
        if let Some(reason) = tab
          .crash_reason
          .as_deref()
          .map(str::trim)
          .filter(|s| !s.is_empty())
        {
          out.push_str(&format!(
            "<div class=\"detail muted\">crash: <code>{}</code></div>",
            escape_html(reason)
          ));
        }
        if let Some(violation) = tab
          .renderer_protocol_violation
          .as_deref()
          .map(str::trim)
          .filter(|s| !s.is_empty())
        {
          out.push_str(&format!(
            "<div class=\"detail muted\">violation: <code>{}</code></div>",
            escape_html(violation)
          ));
        }
        out
      };
      let row_class = if tab.is_active {
        " class=\"active\""
      } else {
        ""
      };
      rows.push_str(&format!(
        "<tr{row_class}>
          <td><code>{safe_window}</code></td>
          <td><code>{}</code></td>
          <td>{url_cell}</td>
          <td>{site_cell}</td>
          <td>{renderer}</td>
          <td>{state}</td>
          <td>{network_cell}</td>
        </tr>",
        tab.tab_id
      ));

      if tab.loading {
        loading_tabs += 1;
      }
      if tab.crashed {
        crashed_tabs += 1;
      }
      if tab.unresponsive {
        unresponsive_tabs += 1;
      }
      if tab.renderer_crashed {
        renderer_crashed_tabs += 1;
      }

      if let Some(window_id) = tab
        .window_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
      {
        windows.insert(window_id.to_string());
      } else {
        unknown_windows += 1;
      }

      match tab.renderer_process {
        Some(process) => {
          let group = renderer_processes.entry(process).or_default();
          group.tabs.push((tab.window_id.clone(), tab.tab_id));
          if site_key != "unknown" {
            group.sites.insert(site_key.clone());
          }
        }
        None => {
          unassigned_tabs += 1;
        }
      }

      {
        let group = site_groups.entry(site_key.clone()).or_default();
        group.tabs.push((tab.window_id.clone(), tab.tab_id));
        match tab.renderer_process {
          Some(process) => {
            group.renderer_processes.insert(process);
          }
          None => {
            group.has_unassigned = true;
          }
        }
      }
      if site_key == "unknown" {
        unknown_sites += 1;
      }
    }
  }

  let renderer_process_count = renderer_processes.len();
  let multi_site_renderers = renderer_processes
    .values()
    .filter(|group| group.sites.len() > 1)
    .count();
  let mut process_rows = String::new();
  if renderer_processes.is_empty() {
    process_rows.push_str(
      "<tr><td colspan=\"3\" class=\"empty\">No renderer processes are assigned yet.</td></tr>",
    );
  } else {
    for (process_id, group) in renderer_processes.iter() {
      let row_class = if site_isolation_enabled && group.sites.len() > 1 {
        " class=\"warn\""
      } else {
        ""
      };
      let process_href = escape_html(&filter_href(&format!("renderer:{process_id}")));
      let mut tabs_cell = String::new();
      for (idx, (window_id, tab_id)) in group.tabs.iter().enumerate() {
        if idx > 0 {
          tabs_cell.push_str("<br>");
        }
        let safe_window = escape_html(window_id.as_deref().unwrap_or("-"));
        let tab_href = escape_html(&filter_href(&format!("tab:{tab_id}")));
        tabs_cell.push_str(&format!(
          "<a href=\"{tab_href}\"><code>{safe_window}:{tab_id}</code></a>"
        ));
      }

      let sites_cell = if group.sites.is_empty() {
        "<span class=\"muted\">(unknown)</span>".to_string()
      } else {
        let mut sites_cell = String::new();
        if group.sites.len() > 1 {
          sites_cell.push_str("<span class=\"muted\">(multiple)</span><br>");
        }
        for (idx, site) in group.sites.iter().enumerate() {
          if idx > 0 {
            sites_cell.push_str("<br>");
          }
          let site_href = escape_html(&filter_href(&format!("site:{site}")));
          sites_cell.push_str(&format!(
            "<a href=\"{site_href}\"><code>{}</code></a>",
            escape_html(site)
          ));
        }
        sites_cell
      };

      process_rows.push_str(&format!(
        "<tr{row_class}>
          <td><a href=\"{process_href}\"><code>{process_id}</code></a></td>
          <td>{tabs_cell}</td>
          <td>{sites_cell}</td>
        </tr>"
      ));
    }
  }

  let site_count = site_groups
    .keys()
    .filter(|site| site.as_str() != "unknown")
    .count();
  let multi_renderer_sites = site_groups
    .values()
    .filter(|group| group.renderer_processes.len() + usize::from(group.has_unassigned) > 1)
    .count();
  let mut site_rows = String::new();
  if site_groups.is_empty() {
    site_rows
      .push_str("<tr><td colspan=\"3\" class=\"empty\">No site snapshot is available.</td></tr>");
  } else {
    for (site, group) in site_groups.iter() {
      let row_class = if site_isolation_enabled
        && site != "unknown"
        && group.renderer_processes.len() + usize::from(group.has_unassigned) > 1
      {
        " class=\"warn\""
      } else {
        ""
      };
      let site_cell = if site == "unknown" {
        "<span class=\"muted\">unknown</span>".to_string()
      } else {
        let site_href = escape_html(&filter_href(&format!("site:{site}")));
        format!(
          "<a href=\"{site_href}\"><code>{}</code></a>",
          escape_html(site)
        )
      };

      let mut renderer_cell = String::new();
      if group.renderer_processes.is_empty() && !group.has_unassigned {
        renderer_cell.push_str("<span class=\"muted\">(unknown)</span>");
      } else {
        if group.renderer_processes.len() + usize::from(group.has_unassigned) > 1 {
          renderer_cell.push_str("<span class=\"muted\">(multiple)</span><br>");
        }
        for (idx, process) in group.renderer_processes.iter().enumerate() {
          if idx > 0 {
            renderer_cell.push_str("<br>");
          }
          let renderer_href = escape_html(&filter_href(&format!("renderer:{process}")));
          renderer_cell.push_str(&format!(
            "<a href=\"{renderer_href}\"><code>{process}</code></a>"
          ));
        }
        if group.has_unassigned {
          if !group.renderer_processes.is_empty() {
            renderer_cell.push_str("<br>");
          }
          let renderer_href = escape_html(&filter_href("renderer:unassigned"));
          renderer_cell.push_str(&format!(
            "<a href=\"{renderer_href}\"><span class=\"muted\">(unassigned)</span></a>"
          ));
        }
      }

      let mut tabs_cell = String::new();
      for (idx, (window_id, tab_id)) in group.tabs.iter().enumerate() {
        if idx > 0 {
          tabs_cell.push_str("<br>");
        }
        let safe_window = escape_html(window_id.as_deref().unwrap_or("-"));
        let tab_href = escape_html(&filter_href(&format!("tab:{tab_id}")));
        tabs_cell.push_str(&format!(
          "<a href=\"{tab_href}\"><code>{safe_window}:{tab_id}</code></a>"
        ));
      }

      site_rows.push_str(&format!(
        "<tr{row_class}>
          <td>{site_cell}</td>
          <td>{renderer_cell}</td>
          <td>{tabs_cell}</td>
        </tr>"
      ));
    }
  }

  let total_tabs = snapshot.open_tabs.len();
  let window_count = windows.len();
  let tabs_suffix = if tokens.is_empty() {
    String::new()
  } else {
    format!(" <span class=\"muted\">(filtered from {total_tabs})</span>")
  };
  let clear_filter = if tokens.is_empty() {
    String::new()
  } else {
    format!("<a class=\"about-button\" href=\"{ABOUT_PROCESSES}\">Clear</a>")
  };

  about_layout_html(
    "Processes",
    ABOUT_PROCESSES,
    &format!(
      "<h1>Processes</h1>
      <p class=\"sub\">
        This page is a placeholder. It will eventually show renderer/network processes and the
        tab→process assignment used by FastRender&rsquo;s multiprocess model.
      </p>

      <form class=\"search\" method=\"get\" action=\"{ABOUT_PROCESSES}\" role=\"search\">
        <input type=\"search\" name=\"q\" value=\"{safe_query}\" placeholder=\"Filter tabs, sites, processes\">
        <button class=\"about-button primary\" type=\"submit\">Filter</button>
        {clear_filter}
      </form>

      <p class=\"summary\">
        Process model: <code>{process_model_label}</code> {process_model_env_html}<br>
        Networking: {network_mode_html} · WebSocket: {websocket_mode_html}<br>
        Tabs: <code>{visible_tabs}</code>{tabs_suffix}
        · Windows: <code>{window_count}</code>
        · Renderer processes: <code>{renderer_process_count}</code>
        · Unassigned tabs: <code>{unassigned_tabs}</code>
        · Sites: <code>{site_count}</code>
        · Multi-site renderers: <code>{multi_site_renderers}</code>
        · Sites on multiple renderers: <code>{multi_renderer_sites}</code>
        · Loading: <code>{loading_tabs}</code>
        · Crashed: <code>{crashed_tabs}</code>
        · Unresponsive: <code>{unresponsive_tabs}</code>
        · Renderer crashed: <code>{renderer_crashed_tabs}</code>
        · Tabs missing window id: <code>{unknown_windows}</code>
        · Tabs missing site key: <code>{unknown_sites}</code>
        {registry_stats_html}
      </p>

      <p class=\"sub\">
        Rows highlighted in red indicate potential site-isolation mismatches (best-effort; only
        highlighted when the process model is <code>site</code>).
      </p>

      <h2>Renderer processes</h2>
      <table class=\"proc-table\">
        <thead>
          <tr>
            <th>Renderer</th>
            <th>Tabs</th>
            <th>Sites</th>
          </tr>
        </thead>
        <tbody>
          {process_rows}
        </tbody>
      </table>

      <h2>Sites</h2>
      <table class=\"proc-table\">
        <thead>
          <tr>
            <th>Site</th>
            <th>Renderers</th>
            <th>Tabs</th>
          </tr>
        </thead>
        <tbody>
          {site_rows}
        </tbody>
      </table>

      <h2>Tabs</h2>
      <table class=\"proc-table\">
        <thead>
          <tr>
            <th>Window</th>
            <th>Tab</th>
            <th>URL</th>
            <th>Site</th>
            <th>Renderer</th>
            <th>State</th>
            <th>Network</th>
          </tr>
        </thead>
        <tbody>
          {rows}
        </tbody>
      </table>"
    ),
    r#"
.sub { color: var(--about-muted); margin: 0 0 14px; }
.search { display: flex; gap: 10px; margin: 0 0 18px; flex-wrap: wrap; }
.search input {
  flex: 1;
  min-width: min(420px, 100%);
  padding: 10px 14px;
  border-radius: 999px;
  border: 1px solid var(--about-border-strong);
  background: var(--about-surface);
  color: inherit;
  font: inherit;
}
.search input:focus {
  outline: 3px solid var(--about-focus);
  outline-offset: 2px;
}
.summary { margin: 0 0 18px; }
.summary code { font-weight: 650; }
.proc-table {
  width: 100%;
  border-collapse: collapse;
}
.proc-table th,
.proc-table td {
  text-align: left;
  padding: 6px 10px;
  border-bottom: 1px solid var(--about-border);
  vertical-align: top;
}
.proc-table th {
  font-weight: 650;
  color: var(--about-muted);
}
.proc-table code {
  word-break: break-all;
}
.tab-title {
  font-weight: 650;
  margin-bottom: 2px;
}
.proc-table tr.active td {
  background: var(--about-accent-bg);
}
.proc-table tr.active td:first-child {
  box-shadow: inset 4px 0 0 var(--about-accent-border);
}
.proc-table tr.warn td {
  background: rgba(239, 68, 68, 0.10);
}
.detail { margin-top: 4px; font-size: 12px; }
.muted { color: var(--about-muted); }
.empty { color: var(--about-muted); padding: 10px 0; }
"#,
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

fn matches_search_tokens(url: &str, title: Option<&str>, tokens: &[&str]) -> bool {
  if tokens.is_empty() {
    return true;
  }

  for token in tokens {
    let in_url = contains_ascii_case_insensitive(url, token);
    let in_title = title.is_some_and(|t| contains_ascii_case_insensitive(t, token));
    if !in_url && !in_title {
      return false;
    }
  }

  true
}

const ABOUT_SEARCH_PAGE_CSS: &str = r#"
.sub { color: var(--about-muted); margin: 0 0 14px; }
.search { display: flex; gap: 8px; margin: 0 0 18px; flex-wrap: wrap; }
.search input {
  flex: 1;
  min-width: min(420px, 100%);
  padding: 8px 12px;
  border-radius: 12px;
  border: 1px solid var(--about-border);
  background: var(--about-surface-2);
  color: inherit;
  font: inherit;
}
.search input:focus {
  outline: 3px solid var(--about-focus-ring);
  outline-offset: 2px;
}
.search input::placeholder {
  color: var(--about-muted);
  opacity: 0.9;
}
.search button { cursor: pointer; }

.empty { color: var(--about-muted); }
.list {
  list-style: none;
  padding: 0;
  margin: 0;
  border-radius: 14px;
  border: 1px solid var(--about-border);
  overflow: hidden;
}
.item { padding: 10px 12px; border-bottom: 1px solid var(--about-border); }
.item:last-child { border-bottom: none; }
.title { font-weight: 650; }
.url { margin-top: 4px; font-size: 12px; color: var(--about-muted); }
code { word-break: break-all; }
"#;

fn history_html(full_url: &str) -> String {
  let query = about_query_param(full_url, "q")
    .unwrap_or_default()
    .trim()
    .to_string();
  let query_lower = query.to_ascii_lowercase();
  let tokens: Vec<&str> = query_lower
    .split_whitespace()
    .filter(|t| !t.is_empty())
    .collect();
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
  let query_lower = query.to_ascii_lowercase();
  let tokens: Vec<&str> = query_lower
    .split_whitespace()
    .filter(|t| !t.is_empty())
    .collect();
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
  about_layout_html(
    "Bookmarks",
    ABOUT_BOOKMARKS,
    &page_body,
    ABOUT_SEARCH_PAGE_CSS,
  )
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
    <link rel=\"stylesheet\" href=\"{ABOUT_SHARED_CSS_URL}\">
    <style>
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
  )
}

fn test_heavy_html() -> String {
  // Large DOM used by cancellation tests. Keep this deterministic and offline.
  let mut out = String::with_capacity(256 * 1024);
  out.push_str("<!doctype html><html><head><meta charset=\"utf-8\"><title>Heavy Test</title>");
  out.push_str(&format!(
    "<link rel=\"stylesheet\" href=\"{ABOUT_SHARED_CSS_URL}\">"
  ));
  out.push_str("<style>");
  out.push_str(
    "body{margin:0;padding:0;font:14px/1.3 system-ui, -apple-system, Segoe UI, sans-serif;}\
     .row{padding:4px 8px;border-bottom:1px solid rgba(0,0,0,0.08);}",
  );
  out.push_str("</style></head><body>");
  // Keep this large enough that cancellation tests can reliably interrupt in-flight layout/paint,
  // but small enough that debug builds complete comfortably under CI contention.
  for i in 0..3000u32 {
    use std::fmt::Write;
    let _ = write!(out, "<div class=\"row\">row {i}</div>");
  }
  out.push_str("</body></html>");
  out
}

fn test_layout_stress_html() -> String {
  // Width-sensitive layout+scroll fixture for responsiveness benchmarks.
  //
  // Goals:
  // - Reflow significantly on viewport width changes (wrapping + grid relayout).
  // - Provide enough vertical content for scroll stress.
  // - Remain deterministic/offline (no external resources, no scripts).
  //
  // Keep this bounded so debug builds and CI remain comfortable.
  const CARD_COUNT: u32 = 100;
  const METRIC_COUNT: u32 = 6;
  const TAG_COUNT: u32 = 8;
  const LOREM: &str = "FastRender layout stress fixture: long-form text that wraps across multiple lines when the viewport width changes. The card structure uses nested grid and flex containers, forcing intrinsic sizing, line breaking, and reflow work during resize.";

  let mut out = String::with_capacity(512 * 1024);
  out.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
  out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
  out.push_str("<title>Layout Stress Test</title>");
  out.push_str(&format!(
    "<link rel=\"stylesheet\" href=\"{ABOUT_SHARED_CSS_URL}\">"
  ));
  out.push_str("<style>");
  out.push_str(
    "body{margin:0;padding:0;font:14px/1.4 system-ui, -apple-system, Segoe UI, sans-serif;}\
     .topbar{position:sticky;top:0;z-index:10;padding:10px 12px;border-bottom:1px solid rgba(127,127,127,0.22);\
     background:var(--about-bg);}\
     .topbar strong{font-weight:650;}\
     .wrap{padding:12px;}\
     .grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(240px,1fr));gap:12px;align-items:start;}\
     .card{border:1px solid rgba(127,127,127,0.24);border-radius:14px;background:var(--about-surface);\
     box-shadow:0 8px 28px rgba(0,0,0,0.10);padding:12px;}\
     .card-header{display:flex;flex-wrap:wrap;gap:8px;align-items:baseline;justify-content:space-between;}\
     .card-title{font-weight:650;letter-spacing:-0.01em;}\
     .card-meta{display:flex;flex-wrap:wrap;gap:6px;font-size:12px;color:var(--about-muted);}\
     .card-meta span{white-space:nowrap;}\
     .card-body{margin-top:8px;}\
     .card-body p{margin:0;overflow-wrap:anywhere;}\
     .metrics{margin-top:10px;display:grid;grid-template-columns:repeat(auto-fit,minmax(90px,1fr));gap:6px;}\
     .kv{display:flex;align-items:baseline;justify-content:space-between;gap:6px;\
     border:1px solid rgba(127,127,127,0.18);border-radius:10px;padding:6px 8px;background:var(--about-surface-2);}\
     .kv .k{font-size:12px;color:var(--about-muted);}\
     .kv .v{font-family:var(--about-mono);font-size:12px;}\
     .tags{margin-top:10px;display:flex;flex-wrap:wrap;gap:6px;}\
     .tag{display:inline-block;padding:2px 9px;border-radius:999px;border:1px solid rgba(127,127,127,0.22);\
     background:var(--about-surface-2);font-size:12px;}\
     .tag code{padding:0;border:0;background:transparent;}\
     ",
  );
  out.push_str("</style></head><body>");
  out.push_str("<div class=\"topbar\"><strong>Layout Stress Test</strong> — resize the window to trigger reflow + rewrap; scroll for sustained load.</div>");
  out.push_str("<div class=\"wrap\"><div class=\"grid\">");

  for i in 0..CARD_COUNT {
    use std::fmt::Write;

    let _ = write!(
      out,
      "<article class=\"card\"><div class=\"card-header\"><div class=\"card-title\">Card {i}</div>\
       <div class=\"card-meta\"><span>group {}</span><span>·</span><span>item {}</span></div></div>",
      i % 10,
      i + 1
    );
    out.push_str("<div class=\"card-body\"><p>");
    out.push_str(LOREM);
    out.push_str("</p>");
    out.push_str("<div class=\"metrics\">");
    for j in 0..METRIC_COUNT {
      let _ = write!(
        out,
        "<div class=\"kv\"><span class=\"k\">k{j}</span><span class=\"v\">{}</span></div>",
        (i * 17 + j * 13) % 1000
      );
    }
    out.push_str("</div>");
    out.push_str("<div class=\"tags\">");
    for t in 0..TAG_COUNT {
      let _ = write!(out, "<span class=\"tag\">tag-{}</span>", (i + t) % 32);
    }
    out.push_str("</div></div></article>");
  }

  out.push_str("</div></div></body></html>");
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
    <link rel=\"stylesheet\" href=\"{ABOUT_SHARED_CSS_URL}\">
    <style>
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
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ui::GlobalHistoryEntry;
  use std::time::{Duration, UNIX_EPOCH};

  fn extract_title(html: &str) -> Option<&str> {
    let start = html.find("<title>")? + "<title>".len();
    let end = html[start..].find("</title>")? + start;
    Some(&html[start..end])
  }

  static SNAPSHOT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

  #[test]
  fn about_page_snapshot_returns_same_arc_when_unchanged() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let a = about_page_snapshot();
    let b = about_page_snapshot();
    assert!(
      std::sync::Arc::ptr_eq(&a, &b),
      "expected about_page_snapshot() to clone the existing Arc when unchanged"
    );
  }

  #[cfg(not(feature = "browser_ui"))]
  #[test]
  fn about_page_snapshot_getter_returns_empty_without_browser_ui() {
    let snapshot = about_page_snapshot();
    assert!(
      snapshot.bookmarks.is_empty(),
      "expected about-page snapshot bookmarks to be empty without browser_ui"
    );
    assert!(
      snapshot.history.is_empty(),
      "expected about-page snapshot history to be empty without browser_ui"
    );
    assert!(
      snapshot.chrome_accent.is_none(),
      "expected about-page snapshot chrome accent to be None without browser_ui"
    );
  }

  #[cfg(not(feature = "browser_ui"))]
  #[test]
  fn about_pages_render_empty_state_without_browser_ui_snapshot_data() {
    const SECRET_BOOKMARK_URL: &str = "https://secret-bookmark.example.invalid/";
    const SECRET_HISTORY_URL: &str = "https://secret-history.example.invalid/";
    const SECRET_TITLE: &str = "Super Secret Title";

    let snapshot = about_page_snapshot();
    assert!(snapshot.bookmarks.is_empty());
    assert!(snapshot.history.is_empty());

    let newtab = html_for_about_url(ABOUT_NEWTAB).unwrap();
    assert!(
      newtab.contains("No bookmarks yet."),
      "expected about:newtab to render empty bookmarks state without browser_ui"
    );
    assert!(
      newtab.contains("No history yet."),
      "expected about:newtab to render empty history state without browser_ui"
    );
    for needle in [SECRET_BOOKMARK_URL, SECRET_HISTORY_URL, SECRET_TITLE] {
      assert!(
        !newtab.contains(needle),
        "about:newtab unexpectedly contained snapshot needle {needle:?} without browser_ui"
      );
    }

    let history = html_for_about_url(ABOUT_HISTORY).unwrap();
    assert!(
      history.contains("No history entries yet."),
      "expected about:history to render empty state without browser_ui"
    );
    for needle in [SECRET_HISTORY_URL, SECRET_TITLE] {
      assert!(
        !history.contains(needle),
        "about:history unexpectedly contained snapshot needle {needle:?} without browser_ui"
      );
    }

    let bookmarks = html_for_about_url(ABOUT_BOOKMARKS).unwrap();
    assert!(
      bookmarks.contains("No bookmarks yet."),
      "expected about:bookmarks to render empty state without browser_ui"
    );
    for needle in [SECRET_BOOKMARK_URL, SECRET_TITLE] {
      assert!(
        !bookmarks.contains(needle),
        "about:bookmarks unexpectedly contained snapshot needle {needle:?} without browser_ui"
      );
    }
  }

  #[test]
  fn about_processes_site_column_shows_best_effort_site_for_open_tabs() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());

    struct RestoreOpenTabs(Vec<OpenTabSnapshot>);
    impl Drop for RestoreOpenTabs {
      fn drop(&mut self) {
        sync_about_page_snapshot_open_tabs(std::mem::take(&mut self.0));
      }
    }

    let before_open_tabs = about_page_snapshot().open_tabs.clone();
    let _restore = RestoreOpenTabs(before_open_tabs);

    sync_about_page_snapshot_open_tabs(vec![
      OpenTabSnapshot {
        window_id: None,
        tab_id: 1,
        url: "https://example.com/a".to_string(),
        title: None,
        site_key: None,
        renderer_process: None,
        is_active: false,
        loading: false,
        crashed: false,
        unresponsive: false,
        renderer_crashed: false,
        crash_reason: None,
        renderer_protocol_violation: None,
      },
      OpenTabSnapshot {
        window_id: None,
        tab_id: 2,
        url: "file:///tmp/a.html".to_string(),
        title: None,
        site_key: None,
        renderer_process: None,
        is_active: false,
        loading: false,
        crashed: false,
        unresponsive: false,
        renderer_crashed: false,
        crash_reason: None,
        renderer_protocol_violation: None,
      },
      OpenTabSnapshot {
        window_id: None,
        tab_id: 3,
        url: "about:newtab".to_string(),
        title: None,
        site_key: None,
        renderer_process: None,
        is_active: false,
        loading: false,
        crashed: false,
        unresponsive: false,
        renderer_crashed: false,
        crash_reason: None,
        renderer_protocol_violation: None,
      },
    ]);

    let html = html_for_about_url(ABOUT_PROCESSES).unwrap();
    let normalized: String = html.chars().filter(|c| !c.is_whitespace()).collect();
    for (url, expected_site) in [
      ("https://example.com/a", "example.com"),
      ("file:///tmp/a.html", "file"),
      ("about:newtab", "about"),
    ] {
      let safe_url = escape_html(url);
      let safe_site = escape_html(expected_site);
      let needle = format!(
        "<td><ahref=\"{safe_url}\"><code>{safe_url}</code></a></td><td><code>{safe_site}</code></td>"
      );
      assert!(
        normalized.contains(&needle),
        "expected about:processes to render site {expected_site:?} for URL {url:?}, got: {html}"
      );
    }
  }

  #[test]
  fn about_processes_escapes_open_tab_titles() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());

    struct RestoreOpenTabs(Vec<OpenTabSnapshot>);
    impl Drop for RestoreOpenTabs {
      fn drop(&mut self) {
        sync_about_page_snapshot_open_tabs(std::mem::take(&mut self.0));
      }
    }

    let before_open_tabs = about_page_snapshot().open_tabs;
    let _restore = RestoreOpenTabs(before_open_tabs);

    let raw_title = "My <Tab> & \"quotes\" 'single'";
    sync_about_page_snapshot_open_tabs(vec![OpenTabSnapshot {
      window_id: None,
      tab_id: 1,
      url: "https://example.com/".to_string(),
      title: Some(raw_title.to_string()),
      site_key: None,
      renderer_process: None,
      is_active: false,
      loading: false,
      crashed: false,
      unresponsive: false,
      renderer_crashed: false,
      crash_reason: None,
      renderer_protocol_violation: None,
    }]);

    let html = html_for_about_url(ABOUT_PROCESSES).unwrap();
    let escaped = "My &lt;Tab&gt; &amp; &quot;quotes&quot; &#39;single&#39;";
    assert!(
      html.contains(escaped),
      "expected about:processes to include escaped title {escaped:?}, got: {html}"
    );
    assert!(
      !html.contains(raw_title),
      "raw title should not appear unescaped in about:processes HTML"
    );
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
      (ABOUT_SETTINGS, Some("Settings")),
      (ABOUT_HELP, Some("Help")),
      (ABOUT_VERSION, Some("Version")),
      (ABOUT_GPU, Some("GPU")),
      (ABOUT_PROCESSES, Some("Processes")),
      (ABOUT_ERROR, Some("Navigation error")),
      (ABOUT_HISTORY, Some("History")),
      (ABOUT_BOOKMARKS, Some("Bookmarks")),
      (ABOUT_TEST_SCROLL, Some("Scroll Test")),
      (ABOUT_TEST_HEAVY, Some("Heavy Test")),
      (ABOUT_TEST_LAYOUT_STRESS, Some("Layout Stress Test")),
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
  fn about_page_urls_list_includes_about_settings() {
    assert!(
      ABOUT_PAGE_URLS.contains(&ABOUT_SETTINGS),
      "expected ABOUT_PAGE_URLS to include {ABOUT_SETTINGS}"
    );
  }

  #[test]
  fn about_gpu_falls_back_to_unknown_when_headless() {
    let html = html_for_about_url(ABOUT_GPU).unwrap();
    assert!(html.contains("<title>GPU</title>"));
    assert!(html.contains(">unknown<"));
  }

  #[test]
  fn newtab_html_links_shared_stylesheet_and_primary_links() {
    let html = html_for_about_url(ABOUT_NEWTAB).unwrap();
    assert!(
      html.contains(&format!("href=\"{ABOUT_SHARED_CSS_URL}\"")),
      "expected about:newtab to link the shared chrome stylesheet"
    );

    for url in [
      "https://example.com/",
      ABOUT_HISTORY,
      ABOUT_BOOKMARKS,
      ABOUT_SETTINGS,
      ABOUT_HELP,
      ABOUT_VERSION,
      ABOUT_GPU,
      ABOUT_PROCESSES,
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
    assert!(
      html.contains("<form"),
      "expected about:newtab to include a <form>"
    );
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
    let query_param =
      query_param.expect("expected search template to include {query} as a query param value");

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
  fn about_shared_css_includes_dark_mode_overrides() {
    let css = about_shared_css();
    assert!(
      css.contains("prefers-color-scheme: dark"),
      "expected shared about-page CSS to include a dark-mode media query"
    );
  }

  #[test]
  fn newtab_keyboard_hint_uses_platform_correct_modifier() {
    let html = html_for_about_url(ABOUT_NEWTAB).unwrap();
    let expected = if cfg!(target_os = "macos") {
      "Cmd"
    } else {
      "Ctrl"
    };
    assert!(
      html.contains(&format!("<span class=\"about-kbd\">{expected}</span>")),
      "expected about:newtab to contain platform modifier key hint {expected}, got: {html}"
    );
    assert!(
      html.contains("<span class=\"about-kbd\">L</span>"),
      "expected about:newtab to contain L key hint, got: {html}"
    );
  }

  #[test]
  fn help_page_lists_common_chrome_shortcuts_and_settings_page() {
    let html = html_for_about_url(ABOUT_HELP).unwrap();

    for needle in [
      r#"<kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>F</kbd> — Find in page"#,
      r#"<kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>+</kbd> / <kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>-</kbd> / <kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>0</kbd> — Zoom in/out/reset"#,
      r#"<kbd>F11</kbd> (Win/Linux); <kbd>Ctrl</kbd>+<kbd>Cmd</kbd>+<kbd>F</kbd> (macOS) — Toggle fullscreen"#,
      r#"<kbd>Ctrl</kbd>/<kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>B</kbd> — Toggle bookmarks bar"#,
      r#"<kbd>Ctrl</kbd>+<kbd>J</kbd> (Win/Linux); <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>J</kbd> (macOS) — Show downloads (<code>Window → Show Downloads…</code>)"#,
      r#"<a href="about:settings">about:settings</a>"#,
    ] {
      assert!(
        html.contains(needle),
        "expected about:help HTML to contain {needle:?}, got: {html}"
      );
    }
  }

  #[cfg(feature = "browser_ui")]
  #[test]
  fn newtab_renders_snapshot_bookmarks_and_history() {
    use std::time::{Duration, UNIX_EPOCH};

    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot().as_ref().clone();

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

  #[cfg(feature = "browser_ui")]
  #[test]
  fn about_processes_renders_open_tabs_snapshot_and_escapes_urls() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot().as_ref().clone();

    let raw_url1 = "https://example.test/?q=1&x=<tag>\"'";
    let raw_url2 = "about:newtab?x=1&y=<2>";
    let safe_url1 = escape_html(raw_url1);
    let safe_url2 = escape_html(raw_url2);

    set_about_page_snapshot(AboutPageSnapshot {
      open_tabs: vec![
        OpenTabSnapshot {
          window_id: None,
          tab_id: 1111,
          url: raw_url1.to_string(),
          title: None,
          site_key: None,
          renderer_process: None,
          is_active: false,
          loading: false,
          crashed: false,
          unresponsive: false,
          renderer_crashed: false,
          crash_reason: None,
          renderer_protocol_violation: None,
        },
        OpenTabSnapshot {
          window_id: None,
          tab_id: 2222,
          url: raw_url2.to_string(),
          title: None,
          site_key: None,
          renderer_process: None,
          is_active: false,
          loading: false,
          crashed: false,
          unresponsive: false,
          renderer_crashed: false,
          crash_reason: None,
          renderer_protocol_violation: None,
        },
      ],
      ..Default::default()
    });

    let html = html_for_about_url(ABOUT_PROCESSES).unwrap();
    assert!(html.contains("<title>Processes</title>"));
    assert!(html.contains("<code>1111</code>"));
    assert!(html.contains("<code>2222</code>"));

    // URLs must be HTML escaped before being inserted into the template.
    assert!(html.contains(&safe_url1));
    assert!(html.contains(&safe_url2));
    assert!(!html.contains(raw_url1));
    assert!(!html.contains(raw_url2));

    set_about_page_snapshot(before);
  }

  #[cfg(feature = "browser_ui")]
  #[test]
  fn sync_history_from_global_history_store_updates_snapshot_and_newtab() {
    use std::time::{Duration, UNIX_EPOCH};

    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot().as_ref().clone();

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
    for needle in [
      "https://example.test/a",
      "A2",
      "https://example.test/b",
      "B",
    ] {
      assert!(
        html.contains(needle),
        "expected about:newtab to contain {needle}"
      );
    }

    set_about_page_snapshot(before);
  }

  #[cfg(feature = "browser_ui")]
  #[test]
  fn sync_history_from_global_history_store_preserves_fragment_stripping() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot().as_ref().clone();

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

  #[cfg(feature = "browser_ui")]
  #[test]
  fn newtab_contains_static_default_links_when_snapshot_empty() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot().as_ref().clone();
    set_about_page_snapshot(AboutPageSnapshot::default());

    let html = html_for_about_url(ABOUT_NEWTAB).unwrap();
    for url in [ABOUT_SETTINGS, ABOUT_HELP, ABOUT_VERSION, ABOUT_GPU, ABOUT_PROCESSES] {
      let needle = format!("class=\"about-tile\" href=\"{url}\"");
      assert!(
        html.contains(&needle),
        "expected about:newtab HTML to contain default shortcut link to {url}"
      );
    }

    set_about_page_snapshot(before);
  }

  #[test]
  fn about_pages_link_shared_chrome_stylesheet() {
    let css = about_shared_css();
    assert!(
      css.contains(ABOUT_SHARED_CSS_MARKER),
      "expected shared about-page stylesheet to include marker"
    );

    for url in [
      ABOUT_NEWTAB,
      ABOUT_SETTINGS,
      ABOUT_HISTORY,
      ABOUT_BOOKMARKS,
      ABOUT_HELP,
      ABOUT_VERSION,
      ABOUT_GPU,
      ABOUT_PROCESSES,
      ABOUT_ERROR,
      ABOUT_TEST_SCROLL,
      ABOUT_TEST_HEAVY,
      ABOUT_TEST_LAYOUT_STRESS,
      ABOUT_TEST_FORM,
    ] {
      let html = html_for_about_url(url).unwrap();
      assert!(
        html.contains(&format!("href=\"{ABOUT_SHARED_CSS_URL}\"")),
        "expected {url} to link shared about-page chrome stylesheet"
      );
    }

    let html = error_page_html("Navigation error", "details", None);
    assert!(html.contains(&format!("href=\"{ABOUT_SHARED_CSS_URL}\"")));
  }

  #[cfg(feature = "browser_ui")]
  #[test]
  fn about_pages_use_chrome_accent_in_css_variables() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot().as_ref().clone();

    set_about_page_snapshot(AboutPageSnapshot {
      chrome_accent: Some(RgbaColor::new(255, 0, 255, 0xFF)),
      ..Default::default()
    });

    let html = html_for_about_url(ABOUT_HELP).unwrap();
    assert!(
      html.contains(&format!("href=\"{ABOUT_SHARED_CSS_URL}\"")),
      "expected about pages to link shared chrome stylesheet"
    );

    for needle in [
      "--about-focus: rgba(255, 0, 255, 0.65);",
      "--about-accent-border: rgba(255, 0, 255, 0.55);",
      "--about-accent-bg: rgba(255, 0, 255, 0.18);",
    ] {
      assert!(
        html.contains(needle),
        "expected about page HTML to include themed accent CSS, missing {needle:?}"
      );
    }

    let css = about_shared_css();
    for needle in [
      "radial-gradient(900px circle at 20% 0%, var(--about-bg-grad1)",
      "@media (prefers-color-scheme: dark)",
    ] {
      assert!(
        css.contains(needle),
        "expected shared about-page stylesheet to include {needle:?}"
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
      // Omnibox.
      "<kbd>Alt</kbd>+<kbd>Enter</kbd>",
      "<kbd>Option</kbd>+<kbd>Enter</kbd>",
      "Open omnibox input in a new tab",
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

  #[cfg(feature = "browser_ui")]
  #[test]
  fn about_history_html_escapes_urls_and_titles() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot().as_ref().clone();

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

  #[cfg(feature = "browser_ui")]
  #[test]
  fn about_history_filters_by_query_param() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot().as_ref().clone();

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

  #[cfg(feature = "browser_ui")]
  #[test]
  fn about_bookmarks_filters_and_includes_entries() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot().as_ref().clone();

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

  #[cfg(feature = "browser_ui")]
  #[test]
  fn about_snapshot_from_stores_includes_nested_bookmarks() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot().as_ref().clone();

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

  #[cfg(feature = "browser_ui")]
  #[test]
  fn about_snapshot_from_stores_preserves_paths_and_other_fields() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot().as_ref().clone();

    let open_tabs = vec![OpenTabSnapshot {
      window_id: None,
      tab_id: 1,
      url: "https://tab.example/".to_string(),
      title: None,
      site_key: None,
      renderer_process: None,
      is_active: false,
      loading: false,
      crashed: false,
      unresponsive: false,
      renderer_crashed: false,
      crash_reason: None,
      renderer_protocol_violation: None,
    }];
    let accent = Some(RgbaColor::new(1, 2, 3, 4));
    set_about_page_snapshot(AboutPageSnapshot {
      open_tabs: open_tabs.clone(),
      chrome_accent: accent,
      session_path: Some("/tmp/session<test>.json".to_string()),
      bookmarks_path: Some("/tmp/bookmarks<test>.json".to_string()),
      history_path: Some("/tmp/history<test>.json".to_string()),
      download_dir: Some("/tmp/downloads<test>".to_string()),
      ..Default::default()
    });

    let mut bookmarks = BookmarkStore::default();
    bookmarks
      .add(
        "https://example.com/".to_string(),
        Some("Example".to_string()),
        None,
      )
      .unwrap();
    let history = GlobalHistoryStore::default();
    set_about_snapshot_from_stores(&bookmarks, &history);

    let snapshot = about_page_snapshot();
    assert_eq!(snapshot.open_tabs.len(), 1);
    assert_eq!(snapshot.open_tabs[0].url, "https://tab.example/");
    assert_eq!(snapshot.chrome_accent, accent);
    assert_eq!(
      snapshot.session_path.as_deref(),
      Some("/tmp/session<test>.json")
    );
    assert_eq!(
      snapshot.bookmarks_path.as_deref(),
      Some("/tmp/bookmarks<test>.json")
    );
    assert_eq!(
      snapshot.history_path.as_deref(),
      Some("/tmp/history<test>.json")
    );
    assert_eq!(
      snapshot.download_dir.as_deref(),
      Some("/tmp/downloads<test>")
    );

    set_about_page_snapshot(before);
  }

  #[cfg(feature = "browser_ui")]
  #[test]
  fn about_settings_renders_snapshot_paths_with_html_escaping() {
    let _lock = SNAPSHOT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = about_page_snapshot().as_ref().clone();

    let session_path = r"C:\path\<test>&session.json".to_string();
    let bookmarks_path = r"C:\path\<test>&bookmarks.json".to_string();
    let history_path = r"C:\path\<test>&history.json".to_string();
    let download_dir = r"C:\downloads\<test>&".to_string();

    set_about_page_snapshot(AboutPageSnapshot {
      session_path: Some(session_path.clone()),
      bookmarks_path: Some(bookmarks_path.clone()),
      history_path: Some(history_path.clone()),
      download_dir: Some(download_dir.clone()),
      ..Default::default()
    });

    let html = html_for_about_url(ABOUT_SETTINGS).unwrap();
    for raw in [&session_path, &bookmarks_path, &history_path, &download_dir] {
      let escaped = escape_html(raw);
      assert!(
        html.contains(&escaped),
        "expected about:settings HTML to contain escaped path {escaped:?}, got: {html}"
      );
      assert!(
        !html.contains(raw),
        "expected about:settings HTML not to contain unescaped path {raw:?}, got: {html}"
      );
    }

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
      ABOUT_PROCESSES,
      ABOUT_ERROR,
      ABOUT_HISTORY,
      ABOUT_BOOKMARKS,
      ABOUT_TEST_SCROLL,
      ABOUT_TEST_HEAVY,
      ABOUT_TEST_LAYOUT_STRESS,
      ABOUT_TEST_FORM,
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

  #[test]
  fn test_layout_stress_fixture_is_width_sensitive_offline_and_large() {
    let html = html_for_about_url(ABOUT_TEST_LAYOUT_STRESS).unwrap();
    assert!(
      html.contains("repeat(auto-fit,minmax(240px,1fr))"),
      "expected layout-stress fixture to use an auto-fit grid for width-sensitive layout"
    );
    let card_count = html.match_indices("<article class=\"card\">").count();
    assert!(
      card_count >= 50,
      "expected layout-stress fixture to have >= 50 cards, got {card_count}"
    );
    assert!(
      !html.contains("http://") && !html.contains("https://"),
      "expected layout-stress fixture to avoid network URLs"
    );
  }

  #[test]
  fn about_help_lists_test_pages() {
    let html = html_for_about_url(ABOUT_HELP).unwrap();
    for url in [
      ABOUT_TEST_SCROLL,
      ABOUT_TEST_HEAVY,
      ABOUT_TEST_LAYOUT_STRESS,
      ABOUT_TEST_FORM,
    ] {
      assert!(
        html.contains(url),
        "expected about:help to list test page {url}"
      );
    }
  }
}
