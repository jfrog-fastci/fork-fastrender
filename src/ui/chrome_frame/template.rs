//! HTML templates for renderer-driven browser chrome (dogfooding FastRender).
//!
//! The chrome frame is intended to be rendered by FastRender (trusted browser process) and driven
//! without JS for P0/P1 by using `chrome-action:` URLs.

/// Escape a string for safe inclusion inside a double-quoted HTML attribute.
fn escape_html_attr(value: &str) -> String {
  let mut out = String::with_capacity(value.len());
  for ch in value.chars() {
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

/// Build the chrome frame HTML document.
///
/// The document is expected to be served at a `chrome://` URL (or similar trusted internal origin)
/// so linked resources like `chrome://styles/chrome.css` can be resolved by the embedding browser.
pub fn chrome_frame_html(current_url: Option<&str>) -> String {
  let current_url = current_url.unwrap_or_default();
  let safe_current_url = escape_html_attr(current_url);

  // Keep this template intentionally minimal. It should remain JS-free so the chrome can be driven
  // via simple `chrome-action:` navigations while JS support is still being brought up.
  format!(
    r#"<!doctype html>
<html class="chrome-frame">
  <head>
    <meta charset="utf-8">
    <link rel="stylesheet" href="chrome://styles/chrome.css">
    <title>FastRender</title>
  </head>
  <body>
    <div id="top-toolbar" class="toolbar" role="toolbar" aria-label="Browser toolbar">
      <div class="toolbar-buttons">
        <a class="toolbar-button" href="chrome-action:back" aria-label="Back">Back</a>
        <a class="toolbar-button" href="chrome-action:forward" aria-label="Forward">Forward</a>
        <a class="toolbar-button" href="chrome-action:reload" aria-label="Reload">Reload</a>
      </div>
      <form class="address-bar" action="chrome-action:navigate" method="get" autocomplete="off">
        <input
          class="address-input"
          name="url"
          type="text"
          value="{safe_current_url}"
          placeholder="Enter URL"
          aria-label="Address bar"
        >
      </form>
    </div>

    <div id="content-frame" class="content-frame"></div>
  </body>
</html>"#
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{BrowserDocument, FastRender, FontConfig, RenderOptions};

  fn count_nodes_with_id(root: &crate::dom::DomNode, id: &str) -> usize {
    let mut count = 0usize;
    root.walk_tree(&mut |node| {
      if node
        .is_element()
        .then(|| node.get_attribute_ref("id"))
        .flatten()
        .is_some_and(|value| value == id)
      {
        count += 1;
      }
    });
    count
  }

  #[test]
  fn chrome_frame_template_is_complete_and_parseable() {
    let html = chrome_frame_html(Some("https://example.invalid/"));

    // Static string invariants.
    assert!(
      html.contains(r#"<link rel="stylesheet" href="chrome://styles/chrome.css">"#),
      "chrome frame should link chrome://styles/chrome.css"
    );
    assert!(
      html.contains(r#"href="chrome-action:back""#)
        && html.contains(r#"href="chrome-action:forward""#)
        && html.contains(r#"href="chrome-action:reload""#),
      "chrome frame should include Back/Forward/Reload chrome-action links"
    );
    assert!(
      html.contains(r#"<form class="address-bar" action="chrome-action:navigate" method="get""#),
      "chrome frame should include address bar form using chrome-action:navigate"
    );
    assert!(
      html.contains(r#"name="url""#),
      "chrome frame address bar should include an <input name=\"url\">"
    );
    assert!(
      html.contains(r#"id="content-frame""#),
      "chrome frame should include #content-frame placeholder"
    );

    // Ensure the template can be parsed by the default convenience constructor.
    let _doc = BrowserDocument::from_html(&html, RenderOptions::default())
      .expect("BrowserDocument::from_html should parse chrome frame HTML");

    // Parse again with a deterministic renderer configuration so this test does not depend on
    // host-installed system fonts.
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");
    let doc =
      BrowserDocument::new(renderer, &html, RenderOptions::default()).expect("parse chrome frame");

    assert_eq!(
      count_nodes_with_id(doc.dom(), "content-frame"),
      1,
      "#content-frame should exist exactly once"
    );
  }
}

