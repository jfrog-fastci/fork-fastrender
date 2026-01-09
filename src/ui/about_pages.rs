pub const ABOUT_BLANK: &str = "about:blank";
pub const ABOUT_NEWTAB: &str = "about:newtab";
pub const ABOUT_ERROR: &str = "about:error";
pub const ABOUT_TEST_SCROLL: &str = "about:test-scroll";
pub const ABOUT_TEST_HEAVY: &str = "about:test-heavy";

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
  let lower = normalized.to_ascii_lowercase();
  match lower.as_str() {
    ABOUT_BLANK => Some(blank_html().to_string()),
    ABOUT_NEWTAB => Some(newtab_html().to_string()),
    ABOUT_ERROR => Some(error_html("Navigation error", None)),
    ABOUT_TEST_SCROLL => Some(test_scroll_html()),
    ABOUT_TEST_HEAVY => Some(test_heavy_html()),
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
  "<!doctype html>
<html>
  <head>
    <meta charset=\"utf-8\">
    <title>New Tab</title>
    <style>
      :root { color-scheme: light dark; }
      body { font: 14px/1.45 system-ui, -apple-system, Segoe UI, sans-serif; margin: 24px; }
      h1 { font-size: 20px; margin: 0 0 12px; }
      code { padding: 0.1em 0.3em; border-radius: 4px; background: rgba(127,127,127,0.2); }
      .box { max-width: 720px; }
      ul { padding-left: 18px; }
    </style>
  </head>
  <body>
    <div class=\"box\">
      <h1>FastRender</h1>
      <p>This is an offline <code>about:newtab</code> page.</p>
      <p>Try navigating to:</p>
      <ul>
        <li><a href=\"https://example.com/\">https://example.com/</a></li>
        <li><a href=\"about:blank\">about:blank</a></li>
        <li><a href=\"about:error\">about:error</a> (template)</li>
      </ul>
      <p>You can also type filesystem paths into the address bar:</p>
      <ul>
        <li><code>/tmp/a.html</code> (POSIX)</li>
        <li><code>C:\\\\path\\\\to\\\\file.html</code> (Windows)</li>
      </ul>
    </div>
  </body>
</html>"
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
  // 5000 rows is enough to trigger a meaningful amount of layout/paint work without making tests
  // excessively slow in CI.
  for i in 0..5000u32 {
    use std::fmt::Write;
    let _ = write!(out, "<div class=\"row\">row {i}</div>");
  }
  out.push_str("</body></html>");
  out
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
