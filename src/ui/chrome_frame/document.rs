//! JS-free HTML/CSS-rendered browser chrome document ("renderer-chrome").
//!
//! This module provides a small, embeddable document used by the windowed browser UI when the
//! `FASTR_BROWSER_RENDERER_CHROME=1` toggle is enabled. Interactions are implemented in Rust by the
//! host embedder via DOM hit-testing + geometry queries (no JavaScript).

use crate::error::Result;
use crate::geometry::Rect;
use crate::ui::{
  BrowserAppState, ChromeActionUrl, OmniboxAction, OmniboxSearchSource, OmniboxSuggestion,
  OmniboxSuggestionSource, OmniboxUrlSource, PointerButton, TabId,
};
use crate::chrome_frame::ChromeHoverState;
use crate::interaction::{cursor_kind_for_hit, resolve_url, HitTestKind};
use crate::{BrowserDocumentDom2, FastRender, RenderOptions};

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::resource::ResourceFetcher;
use crate::ui::chrome_assets::ChromeAssetsFetcher;
use crate::ui::chrome_dynamic_asset_fetcher::ChromeDynamicAssetFetcher;
use crate::ui::trusted_chrome_fetcher::TrustedChromeFetcher;
use crate::FontConfig;

use super::dom_mutation_dom2 as dom_mut;

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
  omnibox_row_capacity: usize,
  state_sig: u64,
  appearance_sig: u64,
  drag: Option<TabDragState>,
  click: Option<ClickState>,
  hover_state: ChromeHoverState,
}

impl ChromeFrameDocument {
  pub fn new(viewport_css: (u32, u32), dpr: f32) -> Result<Self> {
    // Use deterministic bundled fonts for unit tests and browser-ui builds; fall back to platform
    // fonts for minimal renderer builds.
    let font_config = if cfg!(any(test, feature = "browser_ui")) {
      FontConfig::bundled_only()
    } else {
      FontConfig::default()
    };

    // Renderer-chrome is trusted browser-process UI. Ensure all loads stay within the chrome://
    // allowlist (no network access) via a strict fetcher stack.
    let chrome_fetcher: Arc<dyn ResourceFetcher> = {
      let assets: Arc<dyn ResourceFetcher> = Arc::new(ChromeAssetsFetcher::new());
      let dynamic: Arc<dyn ResourceFetcher> = Arc::new(ChromeDynamicAssetFetcher::new(assets));
      Arc::new(TrustedChromeFetcher::new(dynamic))
    };

    let renderer = FastRender::builder()
      .font_sources(font_config)
      .fetcher(chrome_fetcher)
      .build()?;
    Self::new_with_renderer(renderer, viewport_css, dpr)
  }

  pub fn new_with_renderer(
    renderer: FastRender,
    viewport_css: (u32, u32),
    dpr: f32,
  ) -> Result<Self> {
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
      omnibox_row_capacity: 0,
      state_sig: 0,
      appearance_sig: 0,
      drag: None,
      click: None,
      hover_state: ChromeHoverState::default(),
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

  /// Returns the most recently computed hover/cursor state.
  pub fn hover_state(&self) -> ChromeHoverState {
    self.hover_state.clone()
  }

  /// Synchronize the chrome document with the provided browser state.
  ///
  /// Returns `Ok(true)` when the HTML was rebuilt (document reset).
  pub fn sync_state(&mut self, app: &BrowserAppState) -> Result<bool> {
    let sig = compute_state_signature(app);
    if sig == self.state_sig {
      return Ok(false);
    }

    let new_tab_order: Vec<TabId> = app.tabs.iter().map(|t| t.id).collect();

    // Tab strip structure changes (add/remove/reorder tabs) currently trigger a full document reset.
    // This avoids complex DOM insert/remove/reorder logic and keeps hit-testing stable.
    if new_tab_order != self.tab_order {
      self.state_sig = sig;
      self.appearance_sig = compute_appearance_signature(app);
      self.tab_order = new_tab_order;
      self.omnibox_row_capacity =
        if app.chrome.omnibox.open && !app.chrome.omnibox.suggestions.is_empty() {
          app.chrome.omnibox.suggestions.len()
        } else {
          0
        };

      // Reuse the canonical renderer-chrome HTML generator.
      //
      // NOTE: This HTML links to `chrome://styles/chrome.css` and references `chrome://favicon/...`
      // URLs. Embedders are expected to configure the renderer with a strict chrome-only fetcher (see
      // `ui::ChromeAssetsFetcher` + `ui::ChromeDynamicAssetFetcher` + `ui::TrustedChromeFetcher`).
      let html = super::state_to_html::chrome_frame_html_from_state(app);
      let options = self.doc.options().clone();
      self.doc.reset_with_html(&html, options)?;
      return Ok(true);
    }

    let appearance_sig = compute_appearance_signature(app);
    let theme_css_owned = if appearance_sig != self.appearance_sig {
      Some(super::theme::chrome_theme_css(&app.appearance))
    } else {
      None
    };
    let theme_css = theme_css_owned.as_deref();

    let active_tab_id = app.active_tab_id();
    let (can_go_back, can_go_forward, loading) = match app.active_tab() {
      Some(tab) => (tab.can_go_back, tab.can_go_forward, tab.loading),
      None => (false, false, false),
    };

    let omnibox_show = app.chrome.omnibox.open && !app.chrome.omnibox.suggestions.is_empty();
    let omnibox_selected_idx = app
      .chrome
      .omnibox
      .selected
      .filter(|idx| *idx < app.chrome.omnibox.suggestions.len());
    let desired_omnibox_rows = if omnibox_show {
      app.chrome.omnibox.suggestions.len()
    } else {
      0
    };
    let new_omnibox_capacity = self.omnibox_row_capacity.max(desired_omnibox_rows);
    let old_omnibox_capacity = self.omnibox_row_capacity;

    let mut patch_failed = false;

    fn sync_toolbar_button(
      dom: &mut crate::dom2::Document,
      id: &str,
      enabled: bool,
      href: &str,
    ) -> bool {
      let mut changed = false;
      changed |= dom_mut::toggle_class_by_element_id(dom, id, "disabled", !enabled);
      if enabled {
        changed |= dom_mut::set_attribute_by_element_id(dom, id, "href", Some(href));
        changed |= dom_mut::set_attribute_by_element_id(dom, id, "aria-disabled", None);
      } else {
        changed |= dom_mut::set_attribute_by_element_id(dom, id, "href", None);
        changed |= dom_mut::set_attribute_by_element_id(dom, id, "aria-disabled", Some("true"));
      }
      changed
    }

    let _ = self.doc.mutate_dom(|dom| {
      // Validate required nodes exist before making any partial mutations.
      if theme_css.is_some() && dom.get_element_by_id("chrome-theme").is_none() {
        patch_failed = true;
        return false;
      }
      if dom.get_element_by_id("address-bar").is_none()
        || dom.get_element_by_id("omnibox-popup").is_none()
      {
        patch_failed = true;
        return false;
      }
      for button in [
        "toolbar-back",
        "toolbar-forward",
        "toolbar-reload",
        "toolbar-stop",
      ] {
        if dom.get_element_by_id(button).is_none() {
          patch_failed = true;
          return false;
        }
      }
      for tab in &app.tabs {
        if dom.get_element_by_id(&tab_element_id(tab.id)).is_none()
          || dom
            .get_element_by_id(&format!("tab-activate-{}", tab.id.0))
            .is_none()
          || dom
            .get_element_by_id(&format!("tab-title-{}", tab.id.0))
            .is_none()
          || dom
            .get_element_by_id(&format!("tab-close-{}", tab.id.0))
            .is_none()
        {
          patch_failed = true;
          return false;
        }
      }

      let mut changed = false;

      if let Some(css) = theme_css {
        changed |= dom_mut::set_text_by_element_id(dom, "chrome-theme", css);
      }

      // Tabs.
      for tab in &app.tabs {
        let is_active = active_tab_id == Some(tab.id);
        let aria_selected = if is_active { "true" } else { "false" };
        let close_label = format!("Close tab: {}", tab.display_title());
        changed |=
          dom_mut::toggle_class_by_element_id(dom, &tab_element_id(tab.id), "active", is_active);
        changed |= dom_mut::set_attribute_by_element_id(
          dom,
          &format!("tab-activate-{}", tab.id.0),
          "aria-selected",
          Some(aria_selected),
        );
        changed |= dom_mut::set_text_by_element_id(
          dom,
          &format!("tab-title-{}", tab.id.0),
          tab.display_title(),
        );
        changed |= dom_mut::set_attribute_by_element_id(
          dom,
          &format!("tab-close-{}", tab.id.0),
          "aria-label",
          Some(&close_label),
        );
      }

      // Toolbar.
      changed |= sync_toolbar_button(dom, "toolbar-back", can_go_back, "chrome-action:back");
      changed |= sync_toolbar_button(
        dom,
        "toolbar-forward",
        can_go_forward,
        "chrome-action:forward",
      );
      changed |= sync_toolbar_button(dom, "toolbar-reload", !loading, "chrome-action:reload");
      changed |= sync_toolbar_button(dom, "toolbar-stop", loading, "chrome-action:stop-loading");

      // Address bar.
      changed |= dom_mut::set_attribute_by_element_id(
        dom,
        "address-bar",
        "value",
        Some(&app.chrome.address_bar_text),
      );
      changed |= dom_mut::set_attribute_by_element_id(
        dom,
        "address-bar",
        "aria-expanded",
        Some(if omnibox_show { "true" } else { "false" }),
      );
      // Combobox should always point at the suggestions listbox, even when collapsed.
      changed |= dom_mut::set_attribute_by_element_id(
        dom,
        "address-bar",
        "aria-controls",
        Some("omnibox-popup"),
      );
      if omnibox_show {
        if let Some(selected) = omnibox_selected_idx {
          changed |= dom_mut::set_attribute_by_element_id(
            dom,
            "address-bar",
            "aria-activedescendant",
            Some(&format!("omnibox-suggestion-{selected}")),
          );
        } else {
          changed |=
            dom_mut::set_attribute_by_element_id(dom, "address-bar", "aria-activedescendant", None);
        }
      } else {
        changed |=
          dom_mut::set_attribute_by_element_id(dom, "address-bar", "aria-activedescendant", None);
      }

      // Omnibox popup container.
      // `aria-label` should remain stable even while hidden (mirrors state_to_html output).
      changed |= dom_mut::set_attribute_by_element_id(
        dom,
        "omnibox-popup",
        "aria-label",
        Some("Suggestions"),
      );
      if omnibox_show {
        changed |= dom_mut::set_attribute_by_element_id(dom, "omnibox-popup", "hidden", None);
      } else {
        changed |= dom_mut::set_attribute_by_element_id(dom, "omnibox-popup", "hidden", Some(""));
      }

      if omnibox_show {
        let Some(popup) = dom.get_element_by_id("omnibox-popup") else {
          patch_failed = true;
          return false;
        };
        let setsize = desired_omnibox_rows.to_string();

        // Create any additional suggestion rows needed to cover the current suggestion list.
        for idx in old_omnibox_capacity..desired_omnibox_rows {
          let row = create_omnibox_row(dom, popup, idx);
          if !row {
            patch_failed = true;
            return false;
          }
          changed = true;
        }

        // Update suggestion rows in place, reusing the row pool.
        for idx in 0..new_omnibox_capacity {
          let row_id = format!("omnibox-suggestion-{idx}");
          if idx >= desired_omnibox_rows {
            changed |= dom_mut::set_attribute_by_element_id(dom, &row_id, "hidden", Some(""));
            continue;
          }

          let suggestion = &app.chrome.omnibox.suggestions[idx];
          let selected = omnibox_selected_idx == Some(idx);
          let aria_selected = if selected { "true" } else { "false" };
          let href = omnibox_suggestion_href(suggestion).unwrap_or_default();

          let mut class_str = String::new();
          class_str.push_str("omnibox-suggestion ");
          class_str.push_str(omnibox_suggestion_type_class(suggestion));
          class_str.push(' ');
          class_str.push_str(omnibox_suggestion_source_class(suggestion));
          if selected {
            class_str.push_str(" selected");
          }

          let (title, secondary) = omnibox_suggestion_title_and_secondary(suggestion);

          changed |= dom_mut::set_attribute_by_element_id(dom, &row_id, "hidden", None);
          changed |= dom_mut::set_attribute_by_element_id(dom, &row_id, "class", Some(&class_str));
          changed |= dom_mut::set_attribute_by_element_id(
            dom,
            &row_id,
            "aria-selected",
            Some(aria_selected),
          );
          changed |= dom_mut::set_attribute_by_element_id(
            dom,
            &row_id,
            "aria-posinset",
            Some(&(idx + 1).to_string()),
          );
          changed |= dom_mut::set_attribute_by_element_id(
            dom,
            &row_id,
            "aria-setsize",
            Some(&setsize),
          );
          changed |= dom_mut::set_attribute_by_element_id(dom, &row_id, "href", Some(&href));

          changed |= dom_mut::set_text_by_element_id(dom, &format!("omnibox-title-{idx}"), title);

          let url_id = format!("omnibox-url-{idx}");
          if let Some(url) = secondary {
            changed |= dom_mut::set_attribute_by_element_id(dom, &url_id, "hidden", None);
            changed |= dom_mut::set_text_by_element_id(dom, &url_id, url);
          } else {
            changed |= dom_mut::set_attribute_by_element_id(dom, &url_id, "hidden", Some(""));
            changed |= dom_mut::set_text_by_element_id(dom, &url_id, "");
          }
        }
      }

      changed
    });

    if patch_failed {
      // Something is structurally off (missing expected node ids). Fall back to a full rebuild to
      // restore invariants.
      self.state_sig = sig;
      self.appearance_sig = appearance_sig;
      self.tab_order = new_tab_order;
      self.omnibox_row_capacity =
        if app.chrome.omnibox.open && !app.chrome.omnibox.suggestions.is_empty() {
          app.chrome.omnibox.suggestions.len()
        } else {
          0
        };
      let html = super::state_to_html::chrome_frame_html_from_state(app);
      let options = self.doc.options().clone();
      self.doc.reset_with_html(&html, options)?;
      return Ok(true);
    }

    self.state_sig = sig;
    self.appearance_sig = appearance_sig;
    self.tab_order = new_tab_order;
    self.omnibox_row_capacity = new_omnibox_capacity;
    Ok(false)
  }

  /// Notify the chrome document that a tab's favicon bytes have changed.
  ///
  /// Renderer-chrome uses stable `chrome://favicon/<tab_id>` URLs for favicons so the HTML does not
  /// need to be rebuilt when the favicon changes. Since FastRender caches decoded images by URL,
  /// callers must explicitly invalidate the cached favicon decode so the next render fetches the
  /// updated bytes.
  pub fn invalidate_tab_favicon(&mut self, tab_id: TabId) {
    let url = ChromeDynamicAssetFetcher::favicon_url(tab_id);
    self.doc.invalidate_image_cache_for_url(&url);
    // Ensure `render_if_needed` repaints even if no DOM/state invalidations occurred.
    self.doc.invalidate_paint();
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

  fn update_hover_state(&mut self, pos_css: (f32, f32)) {
    const BASE_URL: &str = "chrome://chrome-frame/";

    if !pos_css.0.is_finite() || !pos_css.1.is_finite() || pos_css.0 < 0.0 || pos_css.1 < 0.0 {
      self.hover_state = ChromeHoverState::default();
      return;
    }

    let hit = self.doc.hit_test_viewport_point(pos_css.0, pos_css.1).ok().flatten();
    let hit_meta = hit.as_ref().map(|hit| &hit.hit);

    self.hover_state = ChromeHoverState {
      cursor: cursor_kind_for_hit(hit_meta),
      hovered_url: match hit_meta {
        Some(hit) if matches!(hit.kind, HitTestKind::Link) => hit
          .href
          .as_deref()
          .and_then(|href| resolve_url(BASE_URL, href)),
        _ => None,
      },
    };
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

    self.click = self
      .anchor_action_for_node(hit)
      .map(|(anchor, action)| ClickState {
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

    self.update_hover_state(pos_css);

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

fn compute_appearance_signature(app: &BrowserAppState) -> u64 {
  let mut hasher = DefaultHasher::new();
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

fn omnibox_suggestion_type_class(suggestion: &OmniboxSuggestion) -> &'static str {
  match &suggestion.action {
    OmniboxAction::NavigateToUrl => "omnibox-type-url",
    OmniboxAction::Search(_) => "omnibox-type-search",
    OmniboxAction::ActivateTab(_) => "omnibox-type-tab",
  }
}

fn omnibox_suggestion_source_class(suggestion: &OmniboxSuggestion) -> &'static str {
  match suggestion.source {
    OmniboxSuggestionSource::Primary => "omnibox-source-primary",
    OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest) => {
      "omnibox-source-remote-suggest"
    }
    OmniboxSuggestionSource::Url(OmniboxUrlSource::About) => "omnibox-source-about",
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark) => "omnibox-source-bookmark",
    OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab) => "omnibox-source-closed-tab",
    OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab) => "omnibox-source-open-tab",
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited) => "omnibox-source-visited",
  }
}

fn omnibox_suggestion_href(suggestion: &OmniboxSuggestion) -> Option<String> {
  match &suggestion.action {
    OmniboxAction::NavigateToUrl => suggestion
      .url
      .as_ref()
      .map(|url| ChromeActionUrl::Navigate { url: url.clone() }.to_url_string()),
    // `chrome-action:navigate` is handled like a typed navigation; passing the raw query preserves
    // the same behaviour as pressing Enter in the address bar (search-vs-url resolution happens in
    // the action handler).
    OmniboxAction::Search(query) => {
      Some(ChromeActionUrl::Navigate { url: query.clone() }.to_url_string())
    }
    OmniboxAction::ActivateTab(tab_id) => {
      Some(ChromeActionUrl::ActivateTab { tab_id: *tab_id }.to_url_string())
    }
  }
}

fn omnibox_suggestion_title_and_secondary(suggestion: &OmniboxSuggestion) -> (&str, Option<&str>) {
  let title = suggestion
    .title
    .as_deref()
    .map(str::trim)
    .filter(|t| !t.is_empty())
    .or_else(|| match &suggestion.action {
      OmniboxAction::NavigateToUrl => suggestion.url.as_deref(),
      OmniboxAction::Search(query) => Some(query.as_str()),
      OmniboxAction::ActivateTab(_) => suggestion.url.as_deref(),
    })
    .unwrap_or_default();

  let mut secondary = suggestion
    .url
    .as_deref()
    .map(str::trim)
    .filter(|u| !u.is_empty());
  if secondary.is_some_and(|u| u == title) {
    secondary = None;
  }

  (title, secondary)
}

fn create_omnibox_row(
  dom: &mut crate::dom2::Document,
  popup: crate::dom2::NodeId,
  idx: usize,
) -> bool {
  let row_id = format!("omnibox-suggestion-{idx}");
  if dom.get_element_by_id(&row_id).is_some() {
    return true;
  }

  let row = dom.create_element("a", "");
  let icon = dom.create_element("span", "");
  let text = dom.create_element("span", "");
  let title = dom.create_element("span", "");
  let url = dom.create_element("span", "");

  let title_id = format!("omnibox-title-{idx}");
  let url_id = format!("omnibox-url-{idx}");

  let _ = dom.set_attribute(row, "id", &row_id);
  let _ = dom.set_attribute(row, "class", "omnibox-suggestion");
  let _ = dom.set_attribute(row, "role", "option");
  let _ = dom.set_attribute(row, "aria-selected", "false");
  let _ = dom.set_attribute(row, "href", "");

  let _ = dom.set_attribute(icon, "class", "omnibox-icon");
  let _ = dom.set_attribute(icon, "aria-hidden", "true");

  let _ = dom.set_attribute(text, "class", "omnibox-text");

  let _ = dom.set_attribute(title, "id", &title_id);
  let _ = dom.set_attribute(title, "class", "omnibox-title");
  let title_text = dom.create_text("");
  let _ = dom.append_child(title, title_text);

  let _ = dom.set_attribute(url, "id", &url_id);
  let _ = dom.set_attribute(url, "class", "omnibox-url");
  let _ = dom.set_attribute(url, "hidden", "");
  let url_text = dom.create_text("");
  let _ = dom.append_child(url, url_text);

  let _ = dom.append_child(text, title);
  let _ = dom.append_child(text, url);

  let _ = dom.append_child(row, icon);
  let _ = dom.append_child(row, text);

  let _ = dom.append_child(popup, row);

  dom.get_element_by_id(&row_id).is_some()
    && dom.get_element_by_id(&title_id).is_some()
    && dom.get_element_by_id(&url_id).is_some()
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
  use crate::ui::chrome_assets::ChromeAssetsFetcher;
  use crate::ui::chrome_dynamic_asset_fetcher::ChromeDynamicAssetFetcher;
  use crate::ui::BrowserTabState;
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

    let fetcher = Arc::new(ChromeDynamicAssetFetcher::new(Arc::new(
      ChromeAssetsFetcher::new(),
    )));
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

    assert!(
      chrome.tick(Some(0.0)),
      "tick(Some) should request a repaint"
    );
    let first = chrome
      .render_if_needed()?
      .expect("expected repaint after tick(Some(0.0))");
    let first_hash = pixmap_hash(&first);

    assert!(
      chrome.tick(Some(500.0)),
      "tick(Some) should request a repaint"
    );
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
  fn chrome_frame_pointer_move_updates_hover_state() -> Result<()> {
    let _lock = crate::testing::global_test_lock();

    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()?;

    let mut chrome = ChromeFrameDocument::new_with_renderer(renderer, (160, 80), 1.0)?;
    let html = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <style>
      html, body { margin: 0; padding: 0; }
      #link {
        position: absolute;
        left: 0;
        top: 0;
        display: block;
        width: 100px;
        height: 30px;
        background: rgb(0, 0, 0);
      }
      #input {
        position: absolute;
        left: 0;
        top: 40px;
        width: 120px;
        height: 30px;
      }
    </style>
  </head>
  <body>
    <a id="link" href="chrome-action:test">Link</a>
    <input id="input" type="text" value="" />
  </body>
</html>"#;

    let options = chrome.doc.options().clone();
    chrome.doc.reset_with_html(html, options)?;
    let _ = chrome.render_if_needed()?;

    chrome.pointer_move((10.0, 10.0));
    assert_eq!(chrome.hover_state().cursor, CursorKind::Pointer);
    assert_eq!(
      chrome.hover_state().hovered_url.as_deref(),
      Some("chrome-action:test")
    );

    chrome.pointer_move((10.0, 50.0));
    assert_eq!(chrome.hover_state().cursor, CursorKind::Text);
    assert_eq!(chrome.hover_state().hovered_url, None);

    chrome.pointer_move((-1.0, -1.0));
    assert_eq!(chrome.hover_state(), ChromeHoverState::default());

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

    let fetcher = Arc::new(ChromeDynamicAssetFetcher::new(Arc::new(
      ChromeAssetsFetcher::new(),
    )));
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
    let rect = chrome
      .doc
      .bounding_client_rect(node)
      .expect("suggestion rect");
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

  fn text_child_contents(dom: &crate::dom2::Document, element_id: &str) -> String {
    let node = dom.get_element_by_id(element_id).expect("expected element");
    dom
      .node(node)
      .children
      .iter()
      .copied()
      .find_map(|child| {
        let child_node = dom.node(child);
        if child_node.parent != Some(node) {
          return None;
        }
        match &child_node.kind {
          crate::dom2::NodeKind::Text { content } => Some(content.clone()),
          _ => None,
        }
      })
      .unwrap_or_default()
  }

  #[test]
  fn chrome_frame_sync_state_updates_address_bar_without_reset() {
    let mut app = BrowserAppState::new_with_initial_tab("https://example.invalid/".to_string());

    let fetcher = Arc::new(ChromeDynamicAssetFetcher::new(Arc::new(
      ChromeAssetsFetcher::new(),
    )));
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .fetcher(fetcher)
      .build()
      .expect("build deterministic renderer");
    let mut chrome =
      ChromeFrameDocument::new_with_renderer(renderer, (600, 64), 1.0).expect("create chrome doc");

    assert!(
      chrome.sync_state(&app).expect("sync state"),
      "expected first sync_state call to rebuild the chrome document"
    );

    let node = chrome
      .doc
      .dom()
      .get_element_by_id("address-bar")
      .expect("address bar input");

    app.chrome.address_bar_text = "hello world".to_string();
    assert!(
      !chrome.sync_state(&app).expect("sync state"),
      "expected address-bar update to avoid full reset"
    );

    let node2 = chrome
      .doc
      .dom()
      .get_element_by_id("address-bar")
      .expect("address bar input");
    assert_eq!(node, node2, "expected address-bar node id to be stable");

    let value = chrome
      .doc
      .dom()
      .get_attribute(node, "value")
      .expect("get value attribute");
    assert_eq!(value, Some("hello world"));
  }

  #[test]
  fn chrome_frame_sync_state_updates_tab_close_label_without_reset() {
    let mut app = BrowserAppState::new_with_initial_tab("https://example.invalid/".to_string());
    app.tabs[0].title = Some("Old title".to_string());
    let close_id = format!("tab-close-{}", app.tabs[0].id.0);

    let fetcher = Arc::new(ChromeDynamicAssetFetcher::new(Arc::new(
      ChromeAssetsFetcher::new(),
    )));
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .fetcher(fetcher)
      .build()
      .expect("build deterministic renderer");
    let mut chrome =
      ChromeFrameDocument::new_with_renderer(renderer, (600, 64), 1.0).expect("create chrome doc");

    assert!(
      chrome.sync_state(&app).expect("sync state"),
      "expected first sync_state call to rebuild the chrome document"
    );

    let close = chrome
      .doc
      .dom()
      .get_element_by_id(&close_id)
      .expect("tab close button");
    let label = chrome
      .doc
      .dom()
      .get_attribute(close, "aria-label")
      .expect("get aria-label");
    assert_eq!(label, Some("Close tab: Old title"));

    app.tabs[0].title = Some("New title".to_string());
    assert!(
      !chrome.sync_state(&app).expect("sync state"),
      "expected tab label updates to avoid full reset"
    );

    let close2 = chrome
      .doc
      .dom()
      .get_element_by_id(&close_id)
      .expect("tab close button");
    assert_eq!(close, close2, "expected close button node id to be stable");

    let label2 = chrome
      .doc
      .dom()
      .get_attribute(close, "aria-label")
      .expect("get aria-label");
    assert_eq!(label2, Some("Close tab: New title"));
  }

  #[test]
  fn chrome_frame_sync_state_updates_omnibox_without_reset_and_reuses_nodes() {
    let mut app = BrowserAppState::new_with_initial_tab("https://example.invalid/".to_string());
    app.chrome.omnibox.open = true;
    app.chrome.omnibox.selected = Some(0);
    app.chrome.omnibox.suggestions = vec![OmniboxSuggestion {
      action: OmniboxAction::NavigateToUrl,
      title: Some("Example".to_string()),
      url: Some("https://example.com/".to_string()),
      source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
    }];

    let fetcher = Arc::new(ChromeDynamicAssetFetcher::new(Arc::new(
      ChromeAssetsFetcher::new(),
    )));
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .fetcher(fetcher)
      .build()
      .expect("build deterministic renderer");
    let mut chrome =
      ChromeFrameDocument::new_with_renderer(renderer, (600, 320), 1.0).expect("create chrome doc");

    assert!(
      chrome.sync_state(&app).expect("sync state"),
      "expected first sync_state call to rebuild the chrome document"
    );

    let row = chrome
      .doc
      .dom()
      .get_element_by_id("omnibox-suggestion-0")
      .expect("suggestion row");
    let setsize = chrome
      .doc
      .dom()
      .get_attribute(row, "aria-setsize")
      .expect("get aria-setsize");
    assert_eq!(setsize, Some("1"));
    let posinset = chrome
      .doc
      .dom()
      .get_attribute(row, "aria-posinset")
      .expect("get aria-posinset");
    assert_eq!(posinset, Some("1"));

    app.chrome.omnibox.suggestions[0].title = Some("Updated".to_string());
    assert!(
      !chrome.sync_state(&app).expect("sync state"),
      "expected omnibox updates to avoid full reset"
    );
    let row2 = chrome
      .doc
      .dom()
      .get_element_by_id("omnibox-suggestion-0")
      .expect("suggestion row");
    assert_eq!(row, row2, "expected suggestion row node id to be stable");

    let title = text_child_contents(chrome.doc.dom(), "omnibox-title-0");
    assert_eq!(title, "Updated");

    // Add a second suggestion; the existing row should be reused and ARIA setsize/posinset should
    // update in-place.
    app.chrome.omnibox.suggestions.push(OmniboxSuggestion {
      action: OmniboxAction::NavigateToUrl,
      title: Some("Second".to_string()),
      url: Some("https://second.example/".to_string()),
      source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
    });
    assert!(
      !chrome.sync_state(&app).expect("sync state"),
      "expected omnibox list growth to avoid reset"
    );
    let row_after_grow = chrome
      .doc
      .dom()
      .get_element_by_id("omnibox-suggestion-0")
      .expect("suggestion row");
    assert_eq!(
      row, row_after_grow,
      "expected suggestion-0 row to be reused when suggestions grow"
    );
    let row1 = chrome
      .doc
      .dom()
      .get_element_by_id("omnibox-suggestion-1")
      .expect("suggestion row 1");
    assert_eq!(
      chrome
        .doc
        .dom()
        .get_attribute(row, "aria-setsize")
        .expect("get aria-setsize"),
      Some("2")
    );
    assert_eq!(
      chrome
        .doc
        .dom()
        .get_attribute(row, "aria-posinset")
        .expect("get aria-posinset"),
      Some("1")
    );
    assert_eq!(
      chrome
        .doc
        .dom()
        .get_attribute(row1, "aria-setsize")
        .expect("get aria-setsize"),
      Some("2")
    );
    assert_eq!(
      chrome
        .doc
        .dom()
        .get_attribute(row1, "aria-posinset")
        .expect("get aria-posinset"),
      Some("2")
    );

    // Close then reopen omnibox; rows should remain available for reuse.
    app.chrome.omnibox.open = false;
    assert!(
      !chrome.sync_state(&app).expect("sync state"),
      "expected omnibox close to avoid reset"
    );
    let row_closed = chrome
      .doc
      .dom()
      .get_element_by_id("omnibox-suggestion-0")
      .expect("suggestion row");
    assert_eq!(
      row, row_closed,
      "expected suggestion row node id to remain stable while omnibox is hidden"
    );
    let row1_closed = chrome
      .doc
      .dom()
      .get_element_by_id("omnibox-suggestion-1")
      .expect("suggestion row 1");
    assert_eq!(
      row1, row1_closed,
      "expected second suggestion row node id to remain stable while omnibox is hidden"
    );

    app.chrome.omnibox.open = true;
    assert!(
      !chrome.sync_state(&app).expect("sync state"),
      "expected omnibox reopen to avoid reset"
    );
    let row_open = chrome
      .doc
      .dom()
      .get_element_by_id("omnibox-suggestion-0")
      .expect("suggestion row");
    assert_eq!(
      row, row_open,
      "expected suggestion row node id to be reused"
    );
    let row1_open = chrome
      .doc
      .dom()
      .get_element_by_id("omnibox-suggestion-1")
      .expect("suggestion row 1");
    assert_eq!(
      row1, row1_open,
      "expected second suggestion row node id to be reused"
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
