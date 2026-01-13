use crate::api::BrowserDocument;
use crate::dom::DomNode;
use crate::interaction::{fragment_tree_with_scroll, InteractionEngine, InteractionState};
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
  interaction: InteractionEngine,
  /// True when chrome UI state (DOM mutations, scroll offsets, interaction state) has changed since
  /// the last successful render.
  dirty: bool,
}

fn has_nontrivial_interaction_state(state: &InteractionState) -> bool {
  state.focused.is_some()
    || state.focus_visible
    || !state.focus_chain().is_empty()
    || !state.hover_chain().is_empty()
    || !state.active_chain().is_empty()
    || !state.visited_links.is_empty()
    || state.ime_preedit.is_some()
    || state.text_edit.is_some()
    || state.form_state.has_overrides()
    || state.document_selection.is_some()
    || !state.user_validity.is_empty()
}

impl ChromeFrameDocument {
  /// Create a chrome frame document from the built-in HTML template.
  pub fn new(renderer: FastRender, options: RenderOptions) -> Result<Self> {
    let document = BrowserDocument::new(renderer, CHROME_FRAME_HTML, options)?;
    Ok(Self {
      document,
      interaction: InteractionEngine::new(),
      dirty: true,
    })
  }

  /// Render the chrome frame to a pixmap.
  pub fn render(&mut self) -> Result<Pixmap> {
    let interaction_state = self.interaction.interaction_state();
    // Preserve `interaction_state = None` behavior (notably autofocus synthesis) unless we have any
    // dynamic interaction state to apply.
    let interaction_state =
      has_nontrivial_interaction_state(interaction_state).then_some(interaction_state);
    let frame = self
      .document
      .render_frame_with_scroll_state_and_interaction_state(interaction_state)?;
    self.dirty = false;
    Ok(frame.pixmap)
  }

  pub fn document(&self) -> &BrowserDocument {
    &self.document
  }

  pub fn document_mut(&mut self) -> &mut BrowserDocument {
    // The caller may mutate the document directly; treat this as dirty so a chrome runtime that
    // depends on `dirty` stays conservative.
    self.dirty = true;
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
    let title = active.map(|t| t.display_title()).unwrap_or("New Tab");

    let changed = self.document.mutate_dom(|dom: &mut DomNode| {
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
        changed |= dom_mutation::set_text_content(tab_title, title);
      }

      changed
    });
    self.dirty |= changed;
    changed
  }

  /// Apply a scroll wheel delta (in CSS px) at a point in the chrome viewport.
  ///
  /// Returns `true` when the chrome document's scroll state changed (so callers can request a
  /// repaint).
  pub fn wheel_scroll(
    &mut self,
    pointer_css: (f32, f32),
    delta_css: (f32, f32),
  ) -> Result<bool> {
    // Ignore invalid/no-op scroll deltas.
    let delta_x = delta_css.0;
    let delta_y = delta_css.1;
    if (!delta_x.is_finite() && !delta_y.is_finite()) || (delta_x == 0.0 && delta_y == 0.0) {
      return Ok(false);
    }
    let delta_x = if delta_x.is_finite() { delta_x } else { 0.0 };
    let delta_y = if delta_y.is_finite() { delta_y } else { 0.0 };

    let pointer_in_viewport =
      pointer_css.0.is_finite() && pointer_css.1.is_finite() && pointer_css.0 >= 0.0 && pointer_css.1 >= 0.0;
    if !pointer_in_viewport {
      return Ok(false);
    }

    let viewport_point = crate::geometry::Point::new(pointer_css.0, pointer_css.1);
    let scrolled =
      self
        .document
        .wheel_scroll_at_viewport_point(viewport_point, (delta_x, delta_y))?;

    if scrolled {
      // Scrolling moves content under a stationary pointer, so refresh hover state using the
      // updated scroll offsets and the cached layout artifacts.
      let scroll = self.document.scroll_state();
      let interaction = &mut self.interaction;
      self.document.mutate_dom_with_layout_artifacts(
        |dom: &mut DomNode, box_tree, fragment_tree| {
          let scrolled_tree =
            (!scroll.elements.is_empty()).then(|| fragment_tree_with_scroll(fragment_tree, &scroll));
          let hit_tree = scrolled_tree.as_ref().unwrap_or(fragment_tree);
          let dom_changed =
            interaction.pointer_move(dom, box_tree, hit_tree, &scroll, viewport_point);
          (dom_changed, ())
        },
      )?;

      // Any scroll change requires repaint; hover-state DOM mutations (rare) should also keep this
      // marked dirty.
      self.dirty = true;
    }

    Ok(scrolled)
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

  const SCROLL_SNIPPET: &str = r#"<!doctype html>
  <html>
    <head>
      <meta charset="utf-8" />
      <style>
        html, body { margin: 0; padding: 0; background: white; }
        #scroller {
          width: 48px;
          height: 16px;
          overflow: auto;
        }
        .row { width: 48px; height: 16px; }
        #row-a { background: rgb(255, 0, 0); }
        #row-b { background: rgb(0, 255, 0); }
        #row-c { background: rgb(0, 0, 255); }
      </style>
    </head>
    <body>
      <div id="scroller">
        <div id="row-a" class="row"></div>
        <div id="row-b" class="row"></div>
        <div id="row-c" class="row"></div>
      </div>
    </body>
  </html>"#;

  #[test]
  fn chrome_frame_wheel_scroll_updates_scroll_state_and_rerenders() -> Result<()> {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()?;
    let options = RenderOptions::new().with_viewport(64, 24);
    let mut chrome = ChromeFrameDocument::new(renderer, options.clone())?;
    chrome
      .document_mut()
      .reset_with_html(SCROLL_SNIPPET, options.clone())?;

    let first = chrome.render()?;
    let first_hash = pixmap_hash(&first);

    let scrolled = chrome.wheel_scroll((8.0, 8.0), (0.0, 16.0))?;
    assert!(scrolled, "expected wheel scroll to update scroll state");

    let state = chrome.document().scroll_state();
    assert!(
      state.elements.len() == 1,
      "expected one element scroll offset after wheel scroll, got {:?}",
      state.elements
    );
    let (_, offset) = state.elements.iter().next().expect("scroll state element");
    assert!(
      offset.y > 0.1,
      "expected element scroll offset to be >0 after scroll, got {offset:?}"
    );

    let second = chrome.render()?;
    let second_hash = pixmap_hash(&second);
    assert_ne!(
      first_hash, second_hash,
      "expected chrome render to change after wheel scrolling"
    );
    Ok(())
  }

  #[test]
  fn chrome_frame_wheel_scroll_returns_false_when_no_scroll_change() -> Result<()> {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()?;
    let options = RenderOptions::new().with_viewport(64, 24);
    let mut chrome = ChromeFrameDocument::new(renderer, options.clone())?;
    chrome
      .document_mut()
      .reset_with_html(SCROLL_SNIPPET, options.clone())?;

    // Prime the layout cache.
    chrome.render()?;

    // Scroll to the bottom of the element.
    assert!(
      chrome.wheel_scroll((8.0, 8.0), (0.0, 1000.0))?,
      "expected initial scroll-to-bottom to change scroll state"
    );

    // Further scrolling should be a no-op once the scroll container is at its max offset.
    let state_before = chrome.document().scroll_state();
    let no_change = chrome.wheel_scroll((8.0, 8.0), (0.0, 1000.0))?;
    assert!(!no_change, "expected wheel scroll at bottom to report no change");
    assert_eq!(
      state_before,
      chrome.document().scroll_state(),
      "expected scroll state to remain unchanged at bottom"
    );
    Ok(())
  }
}
