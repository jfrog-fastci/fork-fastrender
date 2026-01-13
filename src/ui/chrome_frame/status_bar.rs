//! Status bar component rendered via FastRender.
//!
//! This is a minimal, standalone HTML document that can be rasterized into a pixmap and then
//! composited by the browser UI. The status bar has left/right text regions (e.g. hovered link URL,
//! loading progress, zoom percent).

use crate::{FastRender, Pixmap, RenderOptions, Result};

/// Hard cap on input text length to keep HTML generation and layout bounded.
///
/// The bar also applies CSS truncation (`text-overflow: ellipsis`) so this is purely defensive
/// against pathological inputs.
const MAX_TEXT_CHARS: usize = 2048;

/// Maximum portion of the bar width each side may occupy before truncating.
///
/// This keeps the right-side status (e.g. zoom/loading) visible even when the left-side URL is long.
const MAX_SECTION_WIDTH_PERCENT: u8 = 70;

#[derive(Debug, Clone)]
pub struct StatusBarDocument {
  pub left_text: String,
  pub right_text: String,
}

impl StatusBarDocument {
  pub fn new(left_text: impl Into<String>, right_text: impl Into<String>) -> Self {
    Self {
      left_text: left_text.into(),
      right_text: right_text.into(),
    }
  }

  /// Build the HTML string for this status bar.
  pub fn html(&self) -> String {
    let left = escape_html(&clamp_text(&self.left_text, MAX_TEXT_CHARS));
    let right = escape_html(&clamp_text(&self.right_text, MAX_TEXT_CHARS));
    // Keep CSS tiny and self-contained; the compositor can place/scale the resulting pixmap.
    //
    // Important flexbox detail:
    // - `min-width: 0` enables text-overflow ellipsis for long unbroken strings (e.g. URLs).
    //   Without it, the min-content width can explode and cause pathological layout behaviour.
    format!(
      "<!doctype html>
<html class=\"fastr-status-bar\">
  <head>
    <meta charset=\"utf-8\">
    <style>
:root {{
  color-scheme: light dark;
  --sb-bg: #f3f3f3;
  --sb-fg: #111;
  --sb-border: rgba(0, 0, 0, 0.18);
}}
@media (prefers-color-scheme: dark) {{
  :root {{
    --sb-bg: #1e1e1e;
    --sb-fg: #eee;
    --sb-border: rgba(255, 255, 255, 0.18);
  }}
}}
html, body {{
  margin: 0;
  padding: 0;
  width: 100%;
  height: 100%;
  overflow: hidden;
}}
body {{
  background: var(--sb-bg);
  color: var(--sb-fg);
  font: 12px/1.2 system-ui, -apple-system, \"Segoe UI\", sans-serif;
}}
.bar {{
  box-sizing: border-box;
  width: 100%;
  height: 100%;
  padding: 0 8px;
  display: flex;
  align-items: center;
  gap: 12px;
  border-top: 1px solid var(--sb-border);
}}
.left, .right {{
  min-width: 0;
  max-width: {max_pct}%;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}}
.left {{
  flex: 1 1 auto;
}}
.right {{
  flex: 0 1 auto;
  text-align: right;
}}
    </style>
  </head>
  <body>
    <div class=\"bar\">
      <div class=\"left\">{left}</div>
      <div class=\"right\">{right}</div>
    </div>
  </body>
</html>",
      max_pct = MAX_SECTION_WIDTH_PERCENT,
    )
  }

  /// Render the status bar into a pixmap using the provided renderer and viewport parameters.
  pub fn render(
    &self,
    renderer: &mut FastRender,
    viewport_width: u32,
    viewport_height: u32,
    dpr: f32,
  ) -> Result<Pixmap> {
    let options = RenderOptions::new()
      .with_viewport(viewport_width, viewport_height)
      .with_device_pixel_ratio(dpr);
    renderer.render_html_with_options(&self.html(), options)
  }
}

fn clamp_text(text: &str, max_chars: usize) -> String {
  if max_chars == 0 {
    return String::new();
  }
  let mut end = text.len();
  let mut count = 0usize;
  for (idx, _) in text.char_indices() {
    if count == max_chars {
      end = idx;
      break;
    }
    count += 1;
  }

  if end == text.len() {
    return text.to_string();
  }

  text[..end].to_string()
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
  use crate::text::font_db::FontConfig;
  use crate::FastRenderConfig;

  #[test]
  fn status_bar_renders_long_url_without_panicking() {
    let long_url = format!(
      "https://example.test/{}",
      "a".repeat(16 * 1024) // ensure we exceed internal and CSS truncation limits
    );
    let doc = StatusBarDocument::new(long_url, "Loading…");

    let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
    let mut renderer = FastRender::with_config(config).expect("renderer");
    let pixmap = doc.render(&mut renderer, 800, 24, 1.0).expect("render");
    assert_eq!(pixmap.width(), 800);
    assert_eq!(pixmap.height(), 24);
  }
}
