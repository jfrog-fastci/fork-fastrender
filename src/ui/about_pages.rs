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

use std::sync::{OnceLock, RwLock};
use std::time::SystemTime;

#[derive(Debug, Clone, Default)]
pub struct AboutPageSnapshot {
  pub bookmarks: Vec<BookmarkSnapshot>,
  /// Global (cross-tab) browsing history.
  ///
  /// This is expected to be ordered by recency (newest first), but about pages should remain robust
  /// even when callers provide unsorted data.
  pub history: Vec<HistorySnapshot>,
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
  let guard = about_page_snapshot_lock()
    .read()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  guard.clone()
}

pub fn set_about_page_snapshot(snapshot: AboutPageSnapshot) {
  let mut guard = about_page_snapshot_lock()
    .write()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  *guard = snapshot;
}

pub fn record_global_history_visit(url: &str, title: Option<&str>) {
  const MAX_ENTRIES: usize = 500;
  let url = url.trim();
  if url.is_empty() {
    return;
  }
  // Avoid recording internal pages in global history.
  if is_about_url(url) {
    return;
  }

  let title = title
    .map(|t| t.trim())
    .filter(|t| !t.is_empty())
    .map(str::to_string);
  let now = SystemTime::now();

  let mut guard = about_page_snapshot_lock()
    .write()
    .unwrap_or_else(|poisoned| poisoned.into_inner());

  if let Some(pos) = guard.history.iter().position(|entry| entry.url == url) {
    let mut existing = guard.history.remove(pos);
    existing.visit_count = existing.visit_count.saturating_add(1);
    existing.last_visited = Some(now);
    if title.is_some() {
      existing.title = title;
    }
    guard.history.insert(0, existing);
  } else {
    guard.history.insert(
      0,
      HistorySnapshot {
        title,
        url: url.to_string(),
        last_visited: Some(now),
        visit_count: 1,
      },
    );
  }

  if guard.history.len() > MAX_ENTRIES {
    guard.history.truncate(MAX_ENTRIES);
  }
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
    ABOUT_HELP => Some(help_html().to_string()),
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

fn newtab_html() -> String {
  const MAX_BOOKMARKS: usize = 12;
  const MAX_HISTORY: usize = 12;

  let snapshot = about_page_snapshot();

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
    use std::fmt::Write;
    let _ = write!(
      bookmark_tiles,
      "<a class=\"btn\" href=\"{safe_url}\"><div class=\"label\">{safe_title}</div><div class=\"url\">{safe_display_url}</div></a>"
    );
    bookmark_count += 1;
  }

  let mut history_tiles = String::new();
  let mut history_count = 0usize;
  let mut seen_urls = std::collections::HashSet::<&str>::new();
  for entry in snapshot.history.iter() {
    if history_count >= MAX_HISTORY {
      break;
    }
    let url = entry.url.trim();
    if url.is_empty() {
      continue;
    }
    if !seen_urls.insert(url) {
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
    use std::fmt::Write;
    let _ = write!(
      history_tiles,
      "<a class=\"btn\" href=\"{safe_url}\"><div class=\"label\">{safe_title}</div><div class=\"url\">{safe_display_url}</div></a>"
    );
    history_count += 1;
  }

  let bookmarks_body = if bookmark_count == 0 {
    "<p class=\"empty\">No bookmarks yet.</p>".to_string()
  } else {
    format!("<div class=\"actions\" aria-label=\"Bookmarks\">{bookmark_tiles}</div>")
  };

  let history_body = if history_count == 0 {
    "<p class=\"empty\">No history yet.</p>".to_string()
  } else {
    format!("<div class=\"actions\" aria-label=\"Recently visited\">{history_tiles}</div>")
  };

  let mut out = String::new();
  out.push_str(
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>New Tab</title>
    <style>
      :root {
        color-scheme: light dark;
        --bg: #f7f8fb;
        --fg: #111827;
        --muted: #4b5563;
        --card-bg: rgba(255, 255, 255, 0.75);
        --card-border: rgba(17, 24, 39, 0.10);
        --shadow: 0 18px 60px rgba(17, 24, 39, 0.12);
        --btn-bg: rgba(17, 24, 39, 0.04);
        --btn-border: rgba(17, 24, 39, 0.12);
        --btn-hover: rgba(17, 24, 39, 0.07);
        --focus: #2563eb;
        --mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono",
          "Courier New", monospace;
      }

      @media (prefers-color-scheme: dark) {
        :root {
          --bg: #0b1020;
          --fg: #e5e7eb;
          --muted: #9ca3af;
          --card-bg: rgba(255, 255, 255, 0.04);
          --card-border: rgba(255, 255, 255, 0.10);
          --shadow: 0 18px 60px rgba(0, 0, 0, 0.45);
          --btn-bg: rgba(255, 255, 255, 0.06);
          --btn-border: rgba(255, 255, 255, 0.12);
          --btn-hover: rgba(255, 255, 255, 0.10);
          --focus: #60a5fa;
        }
      }

      html, body { height: 100%; }
      body {
        margin: 0;
        font: 16px/1.5 system-ui, -apple-system, Segoe UI, sans-serif;
        color: var(--fg);
        background:
          radial-gradient(900px circle at 20% 0%, rgba(37, 99, 235, 0.13), transparent 45%),
          radial-gradient(900px circle at 80% 20%, rgba(16, 185, 129, 0.10), transparent 45%),
          var(--bg);
        display: flex;
        align-items: center;
        justify-content: center;
        padding: 48px 18px;
      }

      .wrap { width: 100%; max-width: 920px; }
      .card {
        background: var(--card-bg);
        border: 1px solid var(--card-border);
        border-radius: 18px;
        box-shadow: var(--shadow);
        padding: 28px;
      }

      h1 {
        font-size: 40px;
        line-height: 1.05;
        margin: 0 0 10px;
        letter-spacing: -0.02em;
      }

      h2 {
        font-size: 16px;
        margin: 22px 0 10px;
        letter-spacing: -0.01em;
      }

      p { margin: 0 0 14px; color: var(--muted); }
      code { font-family: var(--mono); }

      .hint {
        margin-top: 16px;
        padding: 12px 14px;
        border-radius: 12px;
        border: 1px solid var(--btn-border);
        background: rgba(127, 127, 127, 0.10);
        display: flex;
        align-items: center;
        gap: 10px;
      }

      .kbd {
        font-family: var(--mono);
        font-size: 12px;
        padding: 2px 7px;
        border-radius: 8px;
        border: 1px solid var(--btn-border);
        background: var(--btn-bg);
        color: var(--fg);
      }

      .actions {
        margin-top: 18px;
        display: grid;
        grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
        gap: 12px;
      }

      a.btn {
        display: block;
        text-decoration: none;
        color: inherit;
        border: 1px solid var(--btn-border);
        background: var(--btn-bg);
        border-radius: 12px;
        padding: 12px 14px;
      }

      a.btn:hover { background: var(--btn-hover); }
      a.btn:focus-visible { outline: 3px solid var(--focus); outline-offset: 2px; }

      .btn .label { font-weight: 650; margin: 0 0 4px; }
      .btn .url { font-family: var(--mono); font-size: 12px; color: var(--muted); }

      .empty { color: var(--muted); }

      .footer {
        margin-top: 18px;
        font-size: 13px;
        color: var(--muted);
      }
    </style>
  </head>
  <body>
    <main class="wrap">
      <section class="card">
        <h1>FastRender</h1>
        <p>
          This is an offline <code>about:newtab</code> page powered by your local bookmarks and
          browsing history.
        </p>

        <div class="hint" role="note">
          <span class="kbd">Ctrl</span>
          <span class="kbd">L</span>
          <span>Type to search or enter a URL</span>
        </div>

        <h2>Shortcuts</h2>
        <div class="actions" aria-label="Shortcuts">
          <a class="btn" href="https://example.com/">
            <div class="label">Example page</div>
            <div class="url">https://example.com/</div>
          </a>
          <a class="btn" href="about:history">
            <div class="label">History</div>
            <div class="url">about:history</div>
          </a>
          <a class="btn" href="about:bookmarks">
            <div class="label">Bookmarks</div>
            <div class="url">about:bookmarks</div>
          </a>
          <a class="btn" href="about:help">
            <div class="label">Help</div>
            <div class="url">about:help</div>
          </a>
          <a class="btn" href="about:version">
            <div class="label">Version</div>
            <div class="url">about:version</div>
          </a>
          <a class="btn" href="about:gpu">
            <div class="label">GPU</div>
            <div class="url">about:gpu</div>
          </a>
        </div>

        <h2>Bookmarks</h2>
"#,
  );
  out.push_str(&bookmarks_body);
  out.push_str(
    r#"

        <h2>Recently visited</h2>
"#,
  );
  out.push_str(&history_body);
  out.push_str(
    r#"

        <div class="footer">
          Tip: You can also open local files by typing a path like <code>/tmp/a.html</code> or
          <code>C:\path\to\file.html</code>.
        </div>
      </section>
    </main>
  </body>
</html>"#,
  );
  out
}

fn help_html() -> &'static str {
  "<!doctype html>
<html>
  <head>
    <meta charset=\"utf-8\">
    <title>Help</title>
    <style>
      :root { color-scheme: light dark; }
      body { font: 14px/1.45 system-ui, -apple-system, Segoe UI, sans-serif; margin: 24px; }
      h1 { font-size: 20px; margin: 0 0 12px; }
      h2 { font-size: 16px; margin: 18px 0 8px; }
      code, kbd { padding: 0.1em 0.3em; border-radius: 4px; background: rgba(127,127,127,0.2); }
      .box { max-width: 760px; }
      ul { padding-left: 18px; }
      .nav { margin-top: 16px; }
      a { color: inherit; }
    </style>
  </head>
  <body>
    <div class=\"box\">
      <h1>FastRender Help</h1>
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
      </ul>

      <div class=\"nav\">
        <a href=\"about:newtab\">Back to new tab</a>
      </div>
    </div>
  </body>
</html>"
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

  format!(
    "<!doctype html>
<html>
  <head>
    <meta charset=\"utf-8\">
    <title>Version</title>
    <style>
      :root {{ color-scheme: light dark; }}
      body {{ font: 14px/1.45 system-ui, -apple-system, Segoe UI, sans-serif; margin: 24px; }}
      h1 {{ margin: 0 0 12px; font-size: 20px; }}
      code {{ padding: 0.1em 0.3em; border-radius: 4px; background: rgba(127,127,127,0.2); }}
      table {{ border-collapse: collapse; }}
      td {{ padding: 4px 10px 4px 0; vertical-align: top; }}
      .nav {{ margin-top: 16px; }}
      a {{ color: inherit; }}
    </style>
  </head>
  <body>
    <h1>Version</h1>
    <table>
      <tr><td>crate version</td><td><code>{safe_version}</code></td></tr>
      <tr><td>git hash</td><td><code>{safe_git}</code></td></tr>
      <tr><td>build profile</td><td><code>{safe_profile}</code></td></tr>
    </table>
    <div class=\"nav\">
      <a href=\"about:newtab\">Back to new tab</a>
    </div>
  </body>
</html>"
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

  format!(
    "<!doctype html>
<html>
  <head>
    <meta charset=\"utf-8\">
    <title>GPU</title>
    <style>
      :root {{ color-scheme: light dark; }}
      body {{ font: 14px/1.45 system-ui, -apple-system, Segoe UI, sans-serif; margin: 24px; }}
      h1 {{ margin: 0 0 12px; font-size: 20px; }}
      code {{ padding: 0.1em 0.3em; border-radius: 4px; background: rgba(127,127,127,0.2); }}
      table {{ border-collapse: collapse; }}
      td {{ padding: 4px 10px 4px 0; vertical-align: top; }}
      .nav {{ margin-top: 16px; }}
      a {{ color: inherit; }}
    </style>
  </head>
  <body>
    <h1>GPU</h1>
    <p>This page is best-effort: headless runs do not initialize wgpu.</p>
    <table>
      <tr><td>adapter</td><td><code>{safe_name}</code></td></tr>
      <tr><td>backend</td><td><code>{safe_backend}</code></td></tr>
      <tr><td>power preference</td><td><code>{safe_power_preference}</code></td></tr>
      <tr><td>force fallback adapter</td><td><code>{safe_force_fallback}</code></td></tr>
      <tr><td>instance backends</td><td><code>{safe_instance_backends}</code></td></tr>
    </table>
    <div class=\"nav\">
      <a href=\"about:newtab\">Back to new tab</a>
    </div>
  </body>
</html>"
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

  format!(
    "<!doctype html>
<html>
  <head>
    <meta charset=\"utf-8\">
    <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
    <title>History</title>
    <style>
      :root {{ color-scheme: light dark; }}
      body {{ font: 14px/1.45 system-ui, -apple-system, Segoe UI, sans-serif; margin: 24px; }}
      a {{ color: inherit; }}
      h1 {{ margin: 0 0 10px; font-size: 20px; }}
      code {{ padding: 0.1em 0.35em; border-radius: 6px; background: rgba(127,127,127,0.22); word-break: break-all; }}

      .wrap {{ max-width: 900px; }}
      .sub {{ margin: 0 0 14px; color: rgba(127,127,127,0.95); }}
      .search {{ display: flex; gap: 8px; margin: 0 0 18px; }}
      .search input {{ flex: 1; min-width: 0; padding: 8px 10px; border-radius: 10px; border: 1px solid rgba(127,127,127,0.35); background: rgba(127,127,127,0.06); color: inherit; }}
      .search button {{ padding: 8px 12px; border-radius: 10px; border: 1px solid rgba(127,127,127,0.35); background: rgba(127,127,127,0.10); color: inherit; font-weight: 600; }}

      .list {{ list-style: none; padding: 0; margin: 0; border-radius: 14px; border: 1px solid rgba(127,127,127,0.28); overflow: hidden; }}
      .item {{ padding: 10px 12px; border-bottom: 1px solid rgba(127,127,127,0.22); }}
      .item:last-child {{ border-bottom: none; }}
      .title {{ font-weight: 650; }}
      .url {{ margin-top: 4px; font-size: 12px; color: rgba(127,127,127,0.95); }}
      .empty {{ color: rgba(127,127,127,0.95); }}
      .nav {{ margin-top: 16px; }}
    </style>
  </head>
  <body>
    <main class=\"wrap\">
      <h1>History</h1>
      <p class=\"sub\">Showing {match_count} of {total_count} entries.</p>
      <form class=\"search\" method=\"get\" action=\"{ABOUT_HISTORY}\">
        <input type=\"search\" name=\"q\" value=\"{safe_query}\" placeholder=\"Search history\">
        <button type=\"submit\">Search</button>
      </form>
      {body}
      <div class=\"nav\">
        <a href=\"about:newtab\">Back to new tab</a>
      </div>
    </main>
  </body>
</html>"
  )
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

  format!(
    "<!doctype html>
<html>
  <head>
    <meta charset=\"utf-8\">
    <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
    <title>Bookmarks</title>
    <style>
      :root {{ color-scheme: light dark; }}
      body {{ font: 14px/1.45 system-ui, -apple-system, Segoe UI, sans-serif; margin: 24px; }}
      a {{ color: inherit; }}
      h1 {{ margin: 0 0 10px; font-size: 20px; }}
      code {{ padding: 0.1em 0.35em; border-radius: 6px; background: rgba(127,127,127,0.22); word-break: break-all; }}

      .wrap {{ max-width: 900px; }}
      .sub {{ margin: 0 0 14px; color: rgba(127,127,127,0.95); }}
      .search {{ display: flex; gap: 8px; margin: 0 0 18px; }}
      .search input {{ flex: 1; min-width: 0; padding: 8px 10px; border-radius: 10px; border: 1px solid rgba(127,127,127,0.35); background: rgba(127,127,127,0.06); color: inherit; }}
      .search button {{ padding: 8px 12px; border-radius: 10px; border: 1px solid rgba(127,127,127,0.35); background: rgba(127,127,127,0.10); color: inherit; font-weight: 600; }}

      .list {{ list-style: none; padding: 0; margin: 0; border-radius: 14px; border: 1px solid rgba(127,127,127,0.28); overflow: hidden; }}
      .item {{ padding: 10px 12px; border-bottom: 1px solid rgba(127,127,127,0.22); }}
      .item:last-child {{ border-bottom: none; }}
      .title {{ font-weight: 650; }}
      .url {{ margin-top: 4px; font-size: 12px; color: rgba(127,127,127,0.95); }}
      .empty {{ color: rgba(127,127,127,0.95); }}
      .nav {{ margin-top: 16px; }}
    </style>
  </head>
  <body>
    <main class=\"wrap\">
      <h1>Bookmarks</h1>
      <p class=\"sub\">Showing {match_count} of {total_count} entries.</p>
      <form class=\"search\" method=\"get\" action=\"{ABOUT_BOOKMARKS}\">
        <input type=\"search\" name=\"q\" value=\"{safe_query}\" placeholder=\"Search bookmarks\">
        <button type=\"submit\">Search</button>
      </form>
      {body}
      <div class=\"nav\">
        <a href=\"about:newtab\">Back to new tab</a>
      </div>
    </main>
  </body>
</html>"
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
    .map(|url| format!("<a class=\"btn primary\" href=\"{url}\">Retry</a>"))
    .unwrap_or_default();
  let url_line = safe_retry_url
    .as_deref()
    .map(|url| format!("<p class=\"url\">URL: <code>{url}</code></p>"))
    .unwrap_or_default();

  let details_body = match message {
    Some(message) if !message.trim().is_empty() => {
      let safe = escape_html(message);
      format!("<pre>{safe}</pre>")
    }
    _ => "<p class=\"details-empty\">No additional details are available.</p>".to_string(),
  };

  format!(
    "<!doctype html>
<html>
  <head>
    <meta charset=\"utf-8\">
    <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
    <title>{safe_title}</title>
    <style>
      :root {{ color-scheme: light dark; }}

      body {{
        margin: 0;
        font: 14px/1.45 system-ui, -apple-system, Segoe UI, sans-serif;
      }}

      a {{ color: inherit; }}

      .page {{
        padding: 32px 24px;
      }}

      .card {{
        max-width: 760px;
        margin: 0 auto;
        padding: 24px;
        border-radius: 16px;
        border: 1px solid rgba(127,127,127,0.28);
        background: rgba(127,127,127,0.08);
      }}

      .hdr {{
        display: flex;
        gap: 14px;
        align-items: flex-start;
      }}

      .icon {{
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
      }}

      h1 {{
        margin: 0;
        font-size: 20px;
        line-height: 1.2;
      }}

      .sub {{
        margin: 6px 0 0;
        color: rgba(127,127,127,0.95);
      }}

      .url {{
        margin: 12px 0 0;
      }}

      code {{
        padding: 0.1em 0.35em;
        border-radius: 6px;
        background: rgba(127,127,127,0.22);
        word-break: break-all;
      }}

      .actions {{
        margin-top: 18px;
        display: flex;
        gap: 10px;
        flex-wrap: wrap;
      }}

      .btn {{
        display: inline-block;
        padding: 10px 14px;
        border-radius: 12px;
        border: 1px solid rgba(127,127,127,0.35);
        text-decoration: none;
        background: rgba(127,127,127,0.06);
        font-weight: 600;
      }}

      .btn.primary {{
        border-color: rgba(10, 132, 255, 0.55);
        background: rgba(10, 132, 255, 0.18);
      }}

      .btn:focus {{
        outline: 2px solid rgba(10, 132, 255, 0.65);
        outline-offset: 2px;
      }}

      .help {{
        margin-top: 18px;
      }}

      .help p {{
        margin: 0 0 8px;
      }}

      .help ul {{
        margin: 0;
        padding-left: 18px;
      }}

      details {{
        margin-top: 18px;
      }}

      summary {{
        cursor: pointer;
        font-weight: 600;
      }}

      .details-box {{
        margin-top: 10px;
        padding: 12px;
        border-radius: 12px;
        border: 1px solid rgba(127,127,127,0.28);
        background: rgba(255, 59, 48, 0.08);
      }}

      pre {{
        margin: 0;
        white-space: pre-wrap;
        word-break: break-word;
      }}

      .details-empty {{
        margin: 0;
        color: rgba(127,127,127,0.95);
      }}
    </style>
  </head>
  <body>
    <div class=\"page\">
      <div class=\"card\">
        <div class=\"hdr\">
          <div class=\"icon\" aria-hidden=\"true\">!</div>
          <div>
            <h1>{safe_title}</h1>
            <p class=\"sub\">FastRender couldn&rsquo;t load this page.</p>
          </div>
        </div>

        <div class=\"actions\">
          {retry_button}
          <a class=\"btn\" href=\"about:newtab\">Back to new tab</a>
        </div>

        {url_line}

        <div class=\"help\">
          <p>Try:</p>
          <ul>
            <li>Checking the URL for typos.</li>
            <li>Verifying the file exists (for <code>file://</code> URLs).</li>
            <li>Checking your network connection or firewall (for <code>http(s)://</code> URLs).</li>
          </ul>
        </div>

        <details>
          <summary>Technical details</summary>
          <div class=\"details-box\">
            {details_body}
          </div>
        </details>
      </div>
    </div>
  </body>
</html>"
  )
}

fn test_scroll_html() -> String {
  // Simple tall page used by browser UI tests.
  "<!doctype html>
<html>
  <head>
    <meta charset=\"utf-8\">
    <title>Scroll Test</title>
    <style>
      body { margin: 0; font: 14px/1.3 system-ui, -apple-system, Segoe UI, sans-serif; }
      .spacer { height: 4000px; background: linear-gradient(#eee, #ccc); }
    </style>
  </head>
  <body>
    <div class=\"spacer\">scroll</div>
  </body>
</html>"
    .to_string()
}

fn test_heavy_html() -> String {
  // Large DOM used by cancellation tests. Keep this deterministic and offline.
  let mut out = String::with_capacity(256 * 1024);
  out.push_str(
    "<!doctype html><html><head><meta charset=\"utf-8\"><title>Heavy Test</title>\
     <style>body{margin:0;font:14px/1.3 system-ui, -apple-system, Segoe UI, sans-serif;}\
     .row{padding:4px 8px;border-bottom:1px solid rgba(0,0,0,0.08);}</style>\
     </head><body>",
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
  "<!doctype html>
<html>
  <head>
    <meta charset=\"utf-8\">
    <title>Form Test</title>
    <style>
      body { margin: 0; font: 14px/1.3 system-ui, -apple-system, Segoe UI, sans-serif; }
      input { display: block; width: 180px; height: 28px; }
      button { display: block; width: 180px; height: 28px; margin-top: 8px; }
    </style>
  </head>
  <body>
    <form>
      <input name=\"q\">
      <button type=\"submit\" name=\"go\" value=\"1\">Go</button>
    </form>
  </body>
</html>"
    .to_string()
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

  static SNAPSHOT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

  fn extract_title(html: &str) -> Option<&str> {
    let start = html.find("<title>")? + "<title>".len();
    let end = html[start..].find("</title>")? + start;
    Some(&html[start..end])
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
    let html = newtab_html();
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
  fn newtab_renders_snapshot_bookmarks_and_history() {
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
        // Duplicate URL: only the first (most recent) should render.
        HistorySnapshot {
          title: Some("New title".to_string()),
          url: "https://dup.example/?a=1&b=2".to_string(),
          last_visited: None,
          visit_count: 3,
        },
        HistorySnapshot {
          title: Some("Old title".to_string()),
          url: "https://dup.example/?a=1&b=2".to_string(),
          last_visited: None,
          visit_count: 1,
        },
        HistorySnapshot {
          title: Some("Visited & <Site>".to_string()),
          url: "https://visited.example/".to_string(),
          last_visited: None,
          visit_count: 1,
        },
      ],
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
    });

    let html_all = html_for_about_url(ABOUT_BOOKMARKS).unwrap();
    assert!(html_all.contains("https://example.com/"));
    assert!(html_all.contains("https://www.rust-lang.org/"));

    let html_filtered = html_for_about_url("about:bookmarks?q=rust").unwrap();
    assert!(!html_filtered.contains("https://example.com/"));
    assert!(html_filtered.contains("https://www.rust-lang.org/"));

    set_about_page_snapshot(before);
  }
}
