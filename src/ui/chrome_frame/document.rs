//! JS-free HTML/CSS-rendered browser chrome document ("renderer-chrome").
//!
//! This module provides a small, embeddable document used by the windowed browser UI when the
//! `FASTR_BROWSER_RENDERER_CHROME=1` toggle is enabled. Interactions are implemented in Rust by the
//! host embedder via DOM hit-testing + geometry queries (no JavaScript).

use crate::error::Result;
use crate::geometry::Rect;
use crate::ui::{BrowserAppState, ChromeActionUrl, OmniboxAction, PointerButton, TabId};
use crate::{BrowserDocumentDom2, FastRender, RenderOptions};

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Output events produced by the HTML-rendered chrome document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChromeFrameOutput {
  /// Request that the tab strip reorder `tab_id` to the target insertion index.
  ///
  /// The `target_index` matches [`BrowserAppState::drag_reorder_tab`]'s contract: it is the
  /// insertion index *after removing the dragged tab* (i.e. it counts only the other tabs).
  ReorderTab { tab_id: TabId, target_index: usize },
  /// Request that the embedder dispatch a `chrome-action:` URL that was activated inside the chrome
  /// document (e.g. clicking a link in the tab strip, toolbar, or omnibox dropdown).
  ActionUrl(ChromeActionUrl),
}

#[derive(Debug, Clone, Copy)]
struct TabDragState {
  tab_id: TabId,
  down_pos_css: (f32, f32),
  active: bool,
  last_target_index: usize,
}

#[derive(Debug, Clone)]
struct ClickState {
  anchor: crate::dom2::NodeId,
  action: ChromeActionUrl,
  down_pos_css: (f32, f32),
}

/// Minimal HTML/CSS "chrome frame" document rendered by FastRender in the browser process.
///
/// This document is intentionally JS-free: interactions such as tab drag-to-reorder are implemented
/// by the host embedding via DOM hit-testing + geometry queries.
pub struct ChromeFrameDocument {
  doc: BrowserDocumentDom2,
  viewport_css: (u32, u32),
  dpr: f32,
  tab_order: Vec<TabId>,
  state_sig: u64,
  drag: Option<TabDragState>,
  click: Option<ClickState>,
}

impl ChromeFrameDocument {
  pub fn new(viewport_css: (u32, u32), dpr: f32) -> Result<Self> {
    let renderer = FastRender::new()?;
    Self::new_with_renderer(renderer, viewport_css, dpr)
  }

  pub fn new_with_renderer(renderer: FastRender, viewport_css: (u32, u32), dpr: f32) -> Result<Self> {
    let options = RenderOptions::new()
      .with_viewport(viewport_css.0, viewport_css.1)
      .with_device_pixel_ratio(dpr);
    // Start with an empty document; `sync_state` will replace it with the full chrome HTML.
    let doc = BrowserDocumentDom2::new(renderer, "<!doctype html><html></html>", options)?;
    Ok(Self {
      doc,
      viewport_css,
      dpr,
      tab_order: Vec::new(),
      state_sig: 0,
      drag: None,
      click: None,
    })
  }

  /// Update viewport size (CSS px) and device pixel ratio for this chrome document.
  ///
  /// This avoids invalidating style/layout when the values are unchanged.
  pub fn set_viewport(&mut self, viewport_css: (u32, u32), dpr: f32) {
    if viewport_css != self.viewport_css {
      self.viewport_css = viewport_css;
      self.doc.set_viewport(viewport_css.0, viewport_css.1);
    }
    if (dpr - self.dpr).abs() > 1e-6 {
      self.dpr = dpr;
      self.doc.set_device_pixel_ratio(dpr);
    }
  }

  /// Synchronize the chrome document with the provided browser state.
  ///
  /// Returns `Ok(true)` when the HTML was rebuilt (document reset).
  pub fn sync_state(&mut self, app: &BrowserAppState) -> Result<bool> {
    let sig = compute_state_signature(app);
    if sig == self.state_sig {
      return Ok(false);
    }
    self.state_sig = sig;
    self.tab_order = app.tabs.iter().map(|t| t.id).collect();

    // Reuse the canonical renderer-chrome HTML generator.
    //
    // NOTE: This HTML links to `chrome://styles/chrome.css` and references `chrome://favicon/...`
    // URLs. Embedders are expected to configure the renderer with a strict chrome-only fetcher (see
    // `ui::ChromeAssetsFetcher` + `ui::ChromeDynamicAssetFetcher` + `ui::TrustedChromeFetcher`).
    let html = super::state_to_html::chrome_frame_html_from_state(app);
    let options = self.doc.options().clone();
    self.doc.reset_with_html(&html, options)?;
    Ok(true)
  }

  pub fn render_if_needed(&mut self) -> Result<Option<crate::Pixmap>> {
    self.doc.render_if_needed()
  }

  /// Returns `true` when the most recently prepared fragment tree contains any time-based effects
  /// (currently CSS animations/transitions) that require periodic ticking.
  pub fn wants_ticks(&self) -> bool {
    self.doc.prepared().is_some_and(|prepared| {
      let tree = prepared.fragment_tree();
      !tree.keyframes.is_empty() || tree.transition_state.is_some()
    })
  }

  /// Advance the chrome document's animation timeline.
  ///
  /// This mirrors the UI↔worker tick protocol used by the page renderer:
  /// - When `now_ms` is `Some(t)`, CSS animations/transitions are sampled at `t` milliseconds since
  ///   load by calling [`BrowserDocumentDom2::set_animation_time_ms`]. This only invalidates paint,
  ///   so the next render can repaint from cached layout artifacts.
  /// - When `now_ms` is `None`, real-time animation sampling is enabled and callers should only
  ///   repaint when [`BrowserDocumentDom2::needs_animation_frame`] reports that the animation clock
  ///   has advanced.
  ///
  /// Returns `true` when callers should render a new frame.
  pub fn tick(&mut self, now_ms: Option<f32>) -> bool {
    match now_ms {
      Some(ms) => {
        self.doc.set_animation_time_ms(ms);
        true
      }
      None => {
        // Ensure explicit timelines are cleared so real-time sampling is active.
        self.doc.set_animation_time(None);
        self.doc.set_realtime_animations_enabled(true);
        self.doc.needs_animation_frame()
      }
    }
  }

  /// True when a pointer-down on a tab is currently active (candidate or active drag).
  pub fn has_active_drag(&self) -> bool {
    self.drag.is_some()
  }

  pub fn cancel_drag(&mut self) {
    self.drag = None;
  }

  pub fn pointer_down(
    &mut self,
    button: PointerButton,
    pos_css: (f32, f32),
  ) -> Vec<ChromeFrameOutput> {
    if !matches!(button, PointerButton::Primary) {
      self.drag = None;
      self.click = None;
      return Vec::new();
    }

    let hit = match self.doc.element_from_point(pos_css.0, pos_css.1) {
      Ok(hit) => hit,
      Err(_) => None,
    };
    let Some(hit) = hit else {
      self.drag = None;
      self.click = None;
      return Vec::new();
    };

    self.click = self.anchor_action_for_node(hit).map(|(anchor, action)| ClickState {
      anchor,
      action,
      down_pos_css: pos_css,
    });

    let Some(tab_id) = self.tab_id_for_node(hit) else {
      self.drag = None;
      return Vec::new();
    };

    let src_idx = self
      .tab_order
      .iter()
      .position(|id| *id == tab_id)
      .unwrap_or(0);

    self.drag = Some(TabDragState {
      tab_id,
      down_pos_css: pos_css,
      active: false,
      last_target_index: src_idx,
    });
    Vec::new()
  }

  pub fn pointer_move(&mut self, pos_css: (f32, f32)) -> Vec<ChromeFrameOutput> {
    const DRAG_THRESHOLD_CSS_PX: f32 = 6.0;

    // Cancel pending click if the pointer moved beyond the slop threshold.
    if let Some(click) = self.click.as_ref() {
      let dx = pos_css.0 - click.down_pos_css.0;
      let dy = pos_css.1 - click.down_pos_css.1;
      let dist2 = dx * dx + dy * dy;
      if dist2 >= DRAG_THRESHOLD_CSS_PX * DRAG_THRESHOLD_CSS_PX {
        self.click = None;
      }
    }

    let Some(mut drag) = self.drag else {
      return Vec::new();
    };

    if !drag.active {
      let dx = pos_css.0 - drag.down_pos_css.0;
      let dy = pos_css.1 - drag.down_pos_css.1;
      let dist2 = dx * dx + dy * dy;
      if dist2 >= DRAG_THRESHOLD_CSS_PX * DRAG_THRESHOLD_CSS_PX {
        drag.active = true;
        // Once a drag is active, do not treat this gesture as a click.
        self.click = None;
      } else {
        self.drag = Some(drag);
        return Vec::new();
      }
    }

    let rects = self.tab_rects();
    if rects.is_empty() {
      self.drag = Some(drag);
      return Vec::new();
    }

    let target_index = compute_tab_insertion_index(pos_css.0, &rects, drag.tab_id);
    if target_index == drag.last_target_index {
      self.drag = Some(drag);
      return Vec::new();
    }
    drag.last_target_index = target_index;
    self.drag = Some(drag);
    vec![ChromeFrameOutput::ReorderTab {
      tab_id: drag.tab_id,
      target_index,
    }]
  }

  pub fn pointer_up(
    &mut self,
    button: PointerButton,
    pos_css: Option<(f32, f32)>,
  ) -> Vec<ChromeFrameOutput> {
    if !matches!(button, PointerButton::Primary) {
      self.drag = None;
      self.click = None;
      return Vec::new();
    }

    let drag_active = self.drag.as_ref().is_some_and(|d| d.active);
    self.drag = None;

    if drag_active {
      self.click = None;
      return Vec::new();
    }

    let click = self.click.take();
    let Some(click) = click else {
      return Vec::new();
    };
    let Some(pos_css) = pos_css else {
      return Vec::new();
    };

    let hit = match self.doc.element_from_point(pos_css.0, pos_css.1) {
      Ok(hit) => hit,
      Err(_) => None,
    };
    let Some(hit) = hit else {
      return Vec::new();
    };

    let Some((anchor, action)) = self.anchor_action_for_node(hit) else {
      return Vec::new();
    };
    if anchor != click.anchor {
      return Vec::new();
    }

    vec![ChromeFrameOutput::ActionUrl(action)]
  }

  fn tab_id_for_node(&self, node: crate::dom2::NodeId) -> Option<TabId> {
    let dom = self.doc.dom();
    let mut current = Some(node);
    let mut remaining = 128usize;
    while let Some(id) = current {
      if remaining == 0 {
        break;
      }
      remaining -= 1;
      if let Ok(Some(raw)) = dom.get_attribute(id, "data-tab-id") {
        if let Ok(parsed) = raw.trim().parse::<u64>() {
          return Some(TabId(parsed));
        }
      }
      current = dom.parent_node(id);
    }
    None
  }

  fn anchor_action_for_node(
    &self,
    node: crate::dom2::NodeId,
  ) -> Option<(crate::dom2::NodeId, ChromeActionUrl)> {
    let dom = self.doc.dom();
    let mut current = Some(node);
    let mut remaining = 128usize;
    while let Some(id) = current {
      if remaining == 0 {
        break;
      }
      remaining -= 1;

      if let crate::dom2::NodeKind::Element { tag_name, .. } = &dom.node(id).kind {
        if tag_name.eq_ignore_ascii_case("a") {
          if let Ok(Some(href)) = dom.get_attribute(id, "href") {
            let href = href.trim();
            if let Ok(action) = ChromeActionUrl::parse(href) {
              return Some((id, action));
            }
          }
        }
      }

      current = dom.parent_node(id);
    }
    None
  }

  fn tab_rects(&mut self) -> Vec<(TabId, Rect)> {
    let mut out = Vec::with_capacity(self.tab_order.len());
    for tab_id in &self.tab_order {
      let node = self.doc.dom().get_element_by_id(&tab_element_id(*tab_id));
      let Some(node) = node else {
        continue;
      };
      let Some(rect) = self.doc.bounding_client_rect(node) else {
        continue;
      };
      out.push((*tab_id, rect));
    }
    out
  }
}

fn compute_state_signature(app: &BrowserAppState) -> u64 {
  let mut hasher = DefaultHasher::new();
  app.active_tab_id().hash(&mut hasher);
  if let Some(tab) = app.active_tab() {
    tab.can_go_back.hash(&mut hasher);
    tab.can_go_forward.hash(&mut hasher);
    tab.loading.hash(&mut hasher);
  }
  for tab in &app.tabs {
    tab.id.hash(&mut hasher);
    tab.display_title().hash(&mut hasher);
    tab.pinned.hash(&mut hasher);
    tab.group.hash(&mut hasher);
  }
  app.chrome.address_bar_text.hash(&mut hasher);

  app.chrome.omnibox.open.hash(&mut hasher);
  app.chrome.omnibox.selected.hash(&mut hasher);
  for suggestion in &app.chrome.omnibox.suggestions {
    match &suggestion.action {
      OmniboxAction::NavigateToUrl => {
        0u8.hash(&mut hasher);
        // Preserve previous behaviour where URL suggestions contributed their URL to the action
        // hash. The canonical URL is stored on the suggestion itself.
        suggestion.url.hash(&mut hasher);
      }
      OmniboxAction::Search(query) => {
        1u8.hash(&mut hasher);
        query.hash(&mut hasher);
      }
      OmniboxAction::ActivateTab(tab_id) => {
        2u8.hash(&mut hasher);
        tab_id.hash(&mut hasher);
      }
    }
    suggestion.title.hash(&mut hasher);
    suggestion.url.hash(&mut hasher);
    suggestion.source.hash(&mut hasher);
  }

  // Theme affects the generated CSS variables.
  match app.appearance.theme {
    crate::ui::theme_parsing::BrowserTheme::System => 0u8.hash(&mut hasher),
    crate::ui::theme_parsing::BrowserTheme::Light => 1u8.hash(&mut hasher),
    crate::ui::theme_parsing::BrowserTheme::Dark => 2u8.hash(&mut hasher),
  }
  app.appearance.accent_color.hash(&mut hasher);
  app.appearance.ui_scale.to_bits().hash(&mut hasher);
  app.appearance.high_contrast.hash(&mut hasher);
  app.appearance.reduced_motion.hash(&mut hasher);
  hasher.finish()
}

fn tab_element_id(tab_id: TabId) -> String {
  format!("tab-{}", tab_id.0)
}

fn compute_tab_insertion_index(
  pointer_x: f32,
  tab_rects: &[(TabId, Rect)],
  dragged_id: TabId,
) -> usize {
  if pointer_x.is_nan() || pointer_x == f32::NEG_INFINITY {
    return 0;
  }
  if pointer_x == f32::INFINITY {
    return tab_rects
      .iter()
      .filter(|(tab_id, _)| *tab_id != dragged_id)
      .count();
  }

  // Compare against tab centers (ignoring the dragged tab itself). Treat equality as "after" so
  // the boundary is deterministic.
  let mut insertion_index: usize = 0;
  for (tab_id, rect) in tab_rects {
    if *tab_id == dragged_id {
      continue;
    }
    if pointer_x < rect.center().x {
      break;
    }
    insertion_index += 1;
  }
  insertion_index
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ui::BrowserTabState;
  use crate::ui::chrome_assets::ChromeAssetsFetcher;
  use crate::ui::chrome_dynamic_asset_fetcher::ChromeDynamicAssetFetcher;
  use crate::ui::{OmniboxSuggestion, OmniboxSuggestionSource, OmniboxUrlSource};
  use crate::FontConfig;
  use std::collections::hash_map::DefaultHasher;
  use std::hash::{Hash, Hasher};
  use std::sync::Arc;

  fn pixmap_hash(pixmap: &crate::Pixmap) -> u64 {
    let mut hasher = DefaultHasher::new();
    pixmap.width().hash(&mut hasher);
    pixmap.height().hash(&mut hasher);
    pixmap.data().hash(&mut hasher);
    hasher.finish()
  }

  #[test]
  fn chrome_frame_drag_emits_reorder_event() {
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(TabId(1), "https://a.example/".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(TabId(2), "https://b.example/".to_string()),
      false,
    );
    app.push_tab(
      BrowserTabState::new(TabId(3), "https://c.example/".to_string()),
      false,
    );

    let fetcher = Arc::new(ChromeDynamicAssetFetcher::new(Arc::new(ChromeAssetsFetcher::new())));
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .fetcher(fetcher)
      .build()
      .expect("build deterministic renderer");
    let mut chrome =
      ChromeFrameDocument::new_with_renderer(renderer, (600, 40), 1.0).expect("create chrome doc");
    chrome.sync_state(&app).expect("sync state");
    let _ = chrome.render_if_needed().expect("render");

    let node = chrome
      .doc
      .dom()
      .get_element_by_id("tab-1")
      .expect("tab element");
    let rect = chrome.doc.bounding_client_rect(node).expect("tab rect");
    let start = rect.center();

    chrome.pointer_down(PointerButton::Primary, (start.x, start.y));
    let outputs = chrome.pointer_move((599.0, start.y));
    assert!(
      outputs.iter().any(|out| matches!(
        out,
        ChromeFrameOutput::ReorderTab { tab_id, target_index } if *tab_id == TabId(1) && *target_index >= 1
      )),
      "expected drag to emit reorder output, got: {outputs:?}"
    );
  }

  #[test]
  fn chrome_frame_wants_ticks_and_tick_advances_css_keyframes_animation() -> Result<()> {
    let _lock = crate::testing::global_test_lock();

    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()?;

    let mut chrome = ChromeFrameDocument::new_with_renderer(renderer, (32, 32), 1.0)?;

    let html = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <style>
      html, body { margin: 0; padding: 0; }
      #box {
        width: 32px;
        height: 32px;
        background: rgb(255, 0, 0);
        animation: bg 1000ms linear infinite;
      }
      @keyframes bg {
        from { background: rgb(255, 0, 0); }
        to   { background: rgb(0, 0, 255); }
      }
    </style>
  </head>
  <body><div id="box"></div></body>
</html>"#;

    let options = chrome.doc.options().clone();
    chrome.doc.reset_with_html(html, options)?;

    assert!(
      !chrome.wants_ticks(),
      "expected wants_ticks to be false before first render (no prepared fragment tree)"
    );

    let _ = chrome.render_if_needed()?;
    assert!(
      chrome.wants_ticks(),
      "expected wants_ticks to be true after first render for document containing @keyframes"
    );

    assert!(chrome.tick(Some(0.0)), "tick(Some) should request a repaint");
    let first = chrome
      .render_if_needed()?
      .expect("expected repaint after tick(Some(0.0))");
    let first_hash = pixmap_hash(&first);

    assert!(chrome.tick(Some(500.0)), "tick(Some) should request a repaint");
    let second = chrome
      .render_if_needed()?
      .expect("expected repaint after tick(Some(500.0))");
    let second_hash = pixmap_hash(&second);

    assert_ne!(
      first_hash, second_hash,
      "expected keyframes animation sampling to change rendered output between two times"
    );

    Ok(())
  }

  #[test]
  fn chrome_frame_clicking_omnibox_suggestion_emits_action_url() {
    let mut app = BrowserAppState::new_with_initial_tab("https://example.invalid/".to_string());
    app.chrome.omnibox.open = true;
    app.chrome.omnibox.selected = Some(0);
    app.chrome.omnibox.suggestions = vec![OmniboxSuggestion {
      action: OmniboxAction::NavigateToUrl,
      title: Some("Example".to_string()),
      url: Some("https://example.com/".to_string()),
      source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
    }];

    let fetcher = Arc::new(ChromeDynamicAssetFetcher::new(Arc::new(ChromeAssetsFetcher::new())));
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .fetcher(fetcher)
      .build()
      .expect("build deterministic renderer");
    let mut chrome =
      ChromeFrameDocument::new_with_renderer(renderer, (600, 320), 1.0).expect("create chrome doc");
    chrome.sync_state(&app).expect("sync state");
    let _ = chrome.render_if_needed().expect("render");

    let node = chrome
      .doc
      .dom()
      .get_element_by_id("omnibox-suggestion-0")
      .expect("suggestion node");
    let rect = chrome.doc.bounding_client_rect(node).expect("suggestion rect");
    let pos = rect.center();

    chrome.pointer_down(PointerButton::Primary, (pos.x, pos.y));
    let outputs = chrome.pointer_up(PointerButton::Primary, Some((pos.x, pos.y)));

    assert!(
      outputs.iter().any(|out| matches!(
        out,
        ChromeFrameOutput::ActionUrl(ChromeActionUrl::Navigate { url }) if url == "https://example.com/"
      )),
      "expected clicking suggestion to emit Navigate action url, got: {outputs:?}"
    );
  }

  #[test]
  fn compute_tab_insertion_index_ignores_dragged_tab() {
    let rects = vec![
      (TabId(1), Rect::from_xywh(0.0, 0.0, 50.0, 10.0)),
      (TabId(2), Rect::from_xywh(60.0, 0.0, 50.0, 10.0)),
      (TabId(3), Rect::from_xywh(120.0, 0.0, 50.0, 10.0)),
    ];
    assert_eq!(compute_tab_insertion_index(0.0, &rects, TabId(2)), 0);
    assert_eq!(compute_tab_insertion_index(70.0, &rects, TabId(2)), 1);
    assert_eq!(compute_tab_insertion_index(10_000.0, &rects, TabId(2)), 2);
  }
}
