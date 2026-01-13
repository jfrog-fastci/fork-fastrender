//! JS-free HTML/CSS-rendered browser chrome document ("renderer-chrome").
//!
//! This module provides a small, embeddable document used by the windowed browser UI when the
//! `FASTR_BROWSER_RENDERER_CHROME=1` toggle is enabled. Interactions are implemented in Rust by the
//! host embedder via DOM hit-testing + geometry queries (no JavaScript).

use crate::error::Result;
use crate::geometry::Rect;
use crate::ui::{BrowserAppState, PointerButton, TabId};
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
}

#[derive(Debug, Clone, Copy)]
struct TabDragState {
  tab_id: TabId,
  down_pos_css: (f32, f32),
  active: bool,
  last_target_index: usize,
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
    let doc = BrowserDocumentDom2::new(renderer, &chrome_html(&[], None), options)?;
    Ok(Self {
      doc,
      viewport_css,
      dpr,
      tab_order: Vec::new(),
      state_sig: 0,
      drag: None,
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

    let active = app.active_tab_id();
    let tabs = app
      .tabs
      .iter()
      .map(|tab| (tab.id, tab.display_title().to_string()))
      .collect::<Vec<_>>();

    let html = chrome_html(&tabs, active);
    let options = self.doc.options().clone();
    self.doc.reset_with_html(&html, options)?;
    Ok(true)
  }

  pub fn render_if_needed(&mut self) -> Result<Option<crate::Pixmap>> {
    self.doc.render_if_needed()
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
      return Vec::new();
    }

    let hit = match self.doc.element_from_point(pos_css.0, pos_css.1) {
      Ok(hit) => hit,
      Err(_) => None,
    };
    let Some(hit) = hit else {
      self.drag = None;
      return Vec::new();
    };

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
    let Some(mut drag) = self.drag else {
      return Vec::new();
    };

    if !drag.active {
      let dx = pos_css.0 - drag.down_pos_css.0;
      let dy = pos_css.1 - drag.down_pos_css.1;
      let dist2 = dx * dx + dy * dy;
      if dist2 >= DRAG_THRESHOLD_CSS_PX * DRAG_THRESHOLD_CSS_PX {
        drag.active = true;
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

  pub fn pointer_up(&mut self, button: PointerButton) -> Vec<ChromeFrameOutput> {
    if matches!(button, PointerButton::Primary) {
      self.drag = None;
    }
    Vec::new()
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
  for tab in &app.tabs {
    tab.id.hash(&mut hasher);
    tab.display_title().hash(&mut hasher);
    tab.pinned.hash(&mut hasher);
    tab.group.hash(&mut hasher);
  }
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

fn chrome_html(tabs: &[(TabId, String)], active: Option<TabId>) -> String {
  // Keep geometry deterministic for hit-testing and headless unit tests. In particular:
  // - fixed tab widths (avoid font-metric-dependent sizing),
  // - fixed strip height,
  // - no external resources.
  let mut body = String::with_capacity(256 + tabs.len() * 128);
  body.push_str("<div id=\"tab-strip\">");
  for (tab_id, title) in tabs {
    let class = if Some(*tab_id) == active {
      "tab active"
    } else {
      "tab"
    };
    let safe_title = escape_html(title);
    let id = tab_element_id(*tab_id);
    body.push_str(&format!(
      "<div class=\"{class}\" id=\"{id}\" data-tab-id=\"{tab_id}\"><span class=\"tab-title\">{safe_title}</span></div>",
      tab_id = tab_id.0
    ));
  }
  body.push_str("</div>");

  format!(
    "<!doctype html>
<html>
  <head>
    <meta charset=\"utf-8\">
    <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
    <style>
      html, body {{ margin: 0; padding: 0; }}
      body {{
        font: 13px/1.2 system-ui, -apple-system, Segoe UI, sans-serif;
        background: rgb(245, 246, 248);
      }}
      #tab-strip {{
        display: flex;
        flex-direction: row;
        align-items: stretch;
        gap: 6px;
        padding: 6px;
        box-sizing: border-box;
        height: 40px;
      }}
      .tab {{
        box-sizing: border-box;
        width: 160px;
        height: 28px;
        border-radius: 8px;
        border: 1px solid rgba(0,0,0,0.22);
        background: rgba(255,255,255,0.96);
        padding: 4px 10px;
        overflow: hidden;
        white-space: nowrap;
        text-overflow: ellipsis;
        display: flex;
        align-items: center;
        user-select: none;
      }}
      .tab.active {{
        background: rgba(255,255,255,1.0);
        border-color: rgba(0,0,0,0.35);
      }}
      .tab-title {{
        overflow: hidden;
        text-overflow: ellipsis;
      }}
    </style>
  </head>
  <body>{body}</body>
</html>"
  )
}

fn escape_html(value: &str) -> String {
  let mut out = String::with_capacity(value.len() + 16);
  for ch in value.chars() {
    match ch {
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '&' => out.push_str("&amp;"),
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
  use crate::ui::BrowserTabState;
  use crate::FontConfig;

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

    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
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
