pub const ABOUT_BLANK: &str = "about:blank";
pub const ABOUT_NEWTAB: &str = "about:newtab";
pub const ABOUT_HELP: &str = "about:help";
pub const ABOUT_VERSION: &str = "about:version";
pub const ABOUT_GPU: &str = "about:gpu";
pub const ABOUT_ERROR: &str = "about:error";
pub const ABOUT_TEST_SCROLL: &str = "about:test-scroll";
pub const ABOUT_TEST_HEAVY: &str = "about:test-heavy";
pub const ABOUT_TEST_FORM: &str = "about:test-form";

use std::sync::OnceLock;

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
    ABOUT_NEWTAB => Some(newtab_html().to_string()),
    ABOUT_HELP => Some(help_html().to_string()),
    ABOUT_VERSION => Some(version_html()),
    ABOUT_GPU => Some(gpu_html()),
    ABOUT_ERROR => Some(error_html("Navigation error", None)),
    ABOUT_TEST_SCROLL => Some(test_scroll_html()),
    ABOUT_TEST_HEAVY => Some(test_heavy_html()),
    ABOUT_TEST_FORM => Some(test_form_html()),
    _ => None,
  }
}

pub fn error_page_html(title: &str, message: &str) -> String {
  error_html(title, Some(message))
}

fn blank_html() -> &'static str {
  "<!doctype html><html><head><meta charset=\"utf-8\"></head><body></body></html>"
}

fn newtab_html() -> &'static str {
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
          A fast, offline-first HTML/CSS renderer. This <code>about:newtab</code> page is built in
          and deterministic.
        </p>

        <div class="hint" role="note">
          <span class="kbd">Ctrl</span>
          <span class="kbd">L</span>
          <span>Type to search or enter a URL</span>
        </div>

        <div class="actions" aria-label="Shortcuts">
          <a class="btn" href="https://example.com/">
            <div class="label">Example page</div>
            <div class="url">https://example.com/</div>
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

        <div class="footer">
          Tip: You can also open local files by typing a path like <code>/tmp/a.html</code> or
          <code>C:\path\to\file.html</code>.
        </div>
      </section>
    </main>
  </body>
</html>"#
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
        <li>Type a URL into the address bar (http/https/file/about).</li>
        <li>Typing <code>example.com</code> defaults to <code>https://example.com/</code>.</li>
        <li>Typing a filesystem path like <code>/tmp/a.html</code> navigates to a <code>file://</code> URL.</li>
      </ul>

      <h2>Keyboard shortcuts</h2>
      <ul>
        <li><kbd>Ctrl</kbd>+<kbd>L</kbd> / <kbd>Ctrl</kbd>+<kbd>K</kbd> — Focus address bar</li>
        <li><kbd>Ctrl</kbd>+<kbd>T</kbd> — New tab</li>
        <li><kbd>Ctrl</kbd>+<kbd>W</kbd> — Close tab</li>
        <li><kbd>Ctrl</kbd>+<kbd>Tab</kbd> / <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>Tab</kbd> — Next/prev tab</li>
        <li><kbd>Alt</kbd>+<kbd>Left</kbd> / <kbd>Alt</kbd>+<kbd>Right</kbd> — Back/forward</li>
        <li><kbd>Ctrl</kbd>+<kbd>R</kbd> / <kbd>F5</kbd> — Reload</li>
        <li><kbd>Ctrl</kbd>+<kbd>1</kbd>…<kbd>9</kbd> — Activate tab (9 = last)</li>
      </ul>

      <h2>Built-in pages</h2>
      <ul>
        <li><a href=\"about:newtab\">about:newtab</a></li>
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

fn error_html(title: &str, message: Option<&str>) -> String {
  let safe_title = escape_html(title);
  let body = match message {
    Some(message) => {
      let safe = escape_html(message);
      format!("<pre class=\"msg\">{safe}</pre>")
    }
    None => "<p class=\"msg\">No details.</p>".to_string(),
  };

  format!(
    "<!doctype html>
<html>
  <head>
    <meta charset=\"utf-8\">
    <title>{safe_title}</title>
    <style>
      :root {{ color-scheme: light dark; }}
      body {{ font: 14px/1.45 system-ui, -apple-system, Segoe UI, sans-serif; margin: 24px; }}
      h1 {{ margin: 0 0 12px; font-size: 20px; }}
      a {{ color: inherit; }}
      .msg {{ white-space: pre-wrap; padding: 12px; border-radius: 8px; background: rgba(255, 0, 0, 0.08); }}
      .nav {{ margin-top: 16px; }}
    </style>
  </head>
  <body>
    <h1>{safe_title}</h1>
    {body}
    <div class=\"nav\">
      <a href=\"about:newtab\">Back to new tab</a>
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

    for url in ["https://example.com/", ABOUT_HELP, ABOUT_VERSION, ABOUT_GPU] {
      assert!(
        html.contains(url),
        "expected about:newtab HTML to link to {url}"
      );
    }
  }
}
