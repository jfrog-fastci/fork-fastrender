use crate::api::BrowserDocument;
use crate::dom::DomNode;
use crate::ui::BrowserAppState;
use crate::{FastRender, Pixmap, RenderOptions, Result};

pub mod dom_mutation;

const CHROME_FRAME_HTML: &str = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <style>
      html, body {
        margin: 0;
        padding: 0;
        background: white;
        font-size: 14px;
      }

      #toolbar {
        display: flex;
        flex-direction: row;
        align-items: center;
        gap: 6px;
        padding: 6px;
        background: #f0f0f0;
      }

      button {
        width: 28px;
        height: 22px;
        border: 1px solid #888;
        border-radius: 4px;
        background: rgb(0, 200, 0);
        color: black;
      }

      button[disabled] {
        background: rgb(200, 200, 200);
        color: rgb(120, 120, 120);
      }

      #loading-indicator {
        width: 12px;
        height: 12px;
        background: rgb(255, 0, 0);
      }

      #address-bar {
        width: 220px;
        height: 22px;
        border: 1px solid #888;
        border-radius: 4px;
        padding: 2px 6px;
        background: white;
        color: black;
      }

      #tab-title {
        font-weight: bold;
        color: black;
      }
    </style>
  </head>
  <body>
    <div id="toolbar">
      <button id="nav-back" disabled aria-label="Back">←</button>
      <button id="nav-forward" disabled aria-label="Forward">→</button>
      <div id="loading-indicator" hidden></div>
      <input id="address-bar" type="text" value="" />
      <span id="tab-title"></span>
    </div>
  </body>
</html>
"#;

/// Live, mutable chrome-frame document rendered via `BrowserDocument`.
///
/// This is a first step towards a "renderer chrome" implementation where the browser UI is
/// represented as HTML/CSS and rendered by FastRender itself.
pub struct ChromeFrameDocument {
  document: BrowserDocument,
}

impl ChromeFrameDocument {
  /// Create a chrome frame document from the built-in HTML template.
  pub fn new(renderer: FastRender, options: RenderOptions) -> Result<Self> {
    let document = BrowserDocument::new(renderer, CHROME_FRAME_HTML, options)?;
    Ok(Self { document })
  }

  /// Render the chrome frame to a pixmap.
  pub fn render(&mut self) -> Result<Pixmap> {
    self.document.render_frame()
  }

  pub fn document(&self) -> &BrowserDocument {
    &self.document
  }

  pub fn document_mut(&mut self) -> &mut BrowserDocument {
    &mut self.document
  }

  /// Mutate the existing DOM in-place to reflect the latest chrome state.
  ///
  /// Returns `true` when any DOM changes were applied (so callers can request a repaint).
  pub fn sync_state(&mut self, app: &BrowserAppState) -> bool {
    let active = app.active_tab();
    let active_url = active
      .and_then(|t| t.current_url.as_deref())
      .unwrap_or_default();
    let can_go_back = active.map(|t| t.can_go_back).unwrap_or(false);
    let can_go_forward = active.map(|t| t.can_go_forward).unwrap_or(false);
    let loading = active.map(|t| t.loading).unwrap_or(false);
    let title = active
      .map(|t| t.display_title())
      .unwrap_or_else(|| "New Tab".to_string());

    self.document.mutate_dom(|dom: &mut DomNode| {
      let mut changed = false;

      // Address bar: avoid clobbering active edits.
      if !app.chrome.address_bar_editing {
        if let Some(address) = dom_mutation::find_element_by_id_mut(dom, "address-bar") {
          changed |= dom_mutation::set_attr(address, "value", active_url);
        }
      }

      if let Some(back) = dom_mutation::find_element_by_id_mut(dom, "nav-back") {
        changed |= dom_mutation::set_bool_attr(back, "disabled", !can_go_back);
        changed |= dom_mutation::set_attr(
          back,
          "aria-disabled",
          if can_go_back { "false" } else { "true" },
        );
      }

      if let Some(forward) = dom_mutation::find_element_by_id_mut(dom, "nav-forward") {
        changed |= dom_mutation::set_bool_attr(forward, "disabled", !can_go_forward);
        changed |= dom_mutation::set_attr(
          forward,
          "aria-disabled",
          if can_go_forward { "false" } else { "true" },
        );
      }

      if let Some(indicator) = dom_mutation::find_element_by_id_mut(dom, "loading-indicator") {
        changed |= dom_mutation::set_bool_attr(indicator, "hidden", !loading);
      }

      if let Some(tab_title) = dom_mutation::find_element_by_id_mut(dom, "tab-title") {
        changed |= dom_mutation::set_text_content(tab_title, &title);
      }

      changed
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::text::font_db::FontConfig;
  use std::collections::hash_map::DefaultHasher;
  use std::hash::{Hash, Hasher};

  fn pixmap_hash(pixmap: &Pixmap) -> u64 {
    let mut hasher = DefaultHasher::new();
    pixmap.width().hash(&mut hasher);
    pixmap.height().hash(&mut hasher);
    pixmap.data().hash(&mut hasher);
    hasher.finish()
  }

  #[test]
  fn chrome_frame_sync_state_mutates_dom_in_place() -> Result<()> {
    let mut app = BrowserAppState::new_with_initial_tab("https://example.com/".to_string());

    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()?;

    let mut chrome =
      ChromeFrameDocument::new(renderer, RenderOptions::new().with_viewport(360, 40))?;
    chrome.sync_state(&app);
    let first = chrome.render()?;
    let first_hash = pixmap_hash(&first);

    // Flip state that should be reflected in the chrome UI via DOM mutations.
    if let Some(tab) = app.active_tab_mut() {
      tab.can_go_back = true;
      tab.can_go_forward = true;
      tab.loading = true;
      tab.current_url = Some("https://example.com/next".to_string());
      tab.title = Some("Next".to_string());
    }

    let changed = chrome.sync_state(&app);
    assert!(changed, "expected sync_state to report DOM changes");

    let second = chrome.render()?;
    let second_hash = pixmap_hash(&second);

    assert_ne!(
      first_hash, second_hash,
      "expected chrome render to change after sync_state"
    );
    Ok(())
  }
}
