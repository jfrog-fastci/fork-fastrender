//! Experimental "renderer chrome" document: HTML/CSS-rendered browser chrome frame.
//!
//! This is intentionally egui/winit-agnostic so it can be exercised in headless tests.

use crate::dom::DomNode;
use crate::geometry::{Point, Rect};
use crate::interaction::dom_index::DomIndex;
use crate::interaction::{
  absolute_bounds_by_styled_node_id, InteractionAction, InteractionEngine, InteractionState,
};
use crate::ui::chrome_action::ChromeAction;
use crate::ui::messages::TabId;
use crate::{BrowserDocument, Error, FastRender, RenderOptions, Result};

/// Stable `id=` attribute for the tab strip container element.
pub const CHROME_TAB_STRIP_ID: &str = "tab-strip";
/// Stable `id=` attribute for the address bar `<input>`.
pub const CHROME_ADDRESS_BAR_ID: &str = "address-bar";
/// Stable `id=` attribute for the address bar `<form>`.
pub const CHROME_ADDRESS_FORM_ID: &str = "address-form";

/// Minimum vertical distance (in CSS px) the pointer must be outside the tab strip bounds before a
/// tab drag-drop is treated as a "detach into new window" gesture.
const TAB_DETACH_VERTICAL_THRESHOLD_CSS_PX: f32 = 20.0;

const CHROME_FRAME_HTML: &str = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      body { font: 14px sans-serif; }
      #tab-strip {
        height: 40px;
        background: #eee;
      }
      #address-form { padding: 6px; }
      #address-bar {
        width: 100%;
        box-sizing: border-box;
        padding: 6px 8px;
        border: 1px solid #ccc;
        border-radius: 6px;
      }
    </style>
  </head>
  <body>
    <div id="tab-strip"></div>
    <form id="address-form">
      <input id="address-bar" type="text" value="">
    </form>
  </body>
</html>
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChromeFrameEvent {
  /// Emitted when the address bar `<input>`'s value changes due to text input/paste/IME commits.
  AddressBarTextChanged(String),
  /// Emitted when focus enters/leaves the address bar `<input>`.
  AddressBarFocusChanged(bool),
}

/// Read the current `<input>` value for the element with the given `id=` attribute.
///
/// This uses [`DomIndex`] for id → node lookup.
#[must_use]
pub fn dom_input_value_by_element_id(dom: &mut DomNode, element_id: &str) -> Option<String> {
  let index = DomIndex::build(dom);
  let node_id = index.id_by_element_id.get(element_id).copied()?;
  let node = index.node(node_id)?;
  if !node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
  {
    return None;
  }
  Some(node.get_attribute_ref("value").unwrap_or("").to_string())
}

fn dom_set_input_value_by_element_id(dom: &mut DomNode, element_id: &str, value: &str) -> bool {
  let mut index = DomIndex::build(dom);
  let node_id = match index.id_by_element_id.get(element_id).copied() {
    Some(id) => id,
    None => return false,
  };
  let Some(node) = index.node_mut(node_id) else {
    return false;
  };
  if !node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
  {
    return false;
  }
  if node.get_attribute_ref("value").unwrap_or("") == value {
    return false;
  }
  node.set_attribute("value", value);
  true
}

pub struct ChromeFrameDocument {
  document: BrowserDocument,
  interaction: InteractionEngine,
  address_bar_node_id: usize,
  /// Cached address bar value for change detection.
  last_address_bar_value: String,
  /// Cached address bar focus state for change detection.
  last_address_bar_focused: bool,
  /// Transient tab-strip drag state (dragging tab id, if any).
  dragging_tab_id: Option<TabId>,
}

impl ChromeFrameDocument {
  pub fn new(viewport_css: (u32, u32), dpr: f32) -> Result<Self> {
    let renderer = FastRender::new()?;
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
    Self::new_with_renderer_and_options(renderer, options)
  }

  pub fn new_with_renderer_and_options(renderer: FastRender, options: RenderOptions) -> Result<Self> {
    let mut document = BrowserDocument::new(renderer, CHROME_FRAME_HTML, options)?;

    let mut address_bar_node_id: Option<usize> = None;
    document.mutate_dom(|dom| {
      let index = DomIndex::build(dom);
      address_bar_node_id = index.id_by_element_id.get(CHROME_ADDRESS_BAR_ID).copied();
      false
    });
    let address_bar_node_id = address_bar_node_id.ok_or_else(|| {
      Error::Other(format!(
        "ChromeFrameDocument template missing element id={CHROME_ADDRESS_BAR_ID:?}"
      ))
    })?;

    let mut this = Self {
      document,
      interaction: InteractionEngine::new(),
      address_bar_node_id,
      last_address_bar_value: String::new(),
      last_address_bar_focused: false,
      dragging_tab_id: None,
    };
    // Seed the cached value from the initial DOM so the first user edit is detected correctly.
    this.last_address_bar_value = this.address_bar_value();
    Ok(this)
  }

  pub fn document(&self) -> &BrowserDocument {
    &self.document
  }

  pub fn document_mut(&mut self) -> &mut BrowserDocument {
    &mut self.document
  }

  pub fn dom(&self) -> &DomNode {
    self.document.dom()
  }

  pub fn interaction(&self) -> &InteractionEngine {
    &self.interaction
  }

  pub fn interaction_mut(&mut self) -> &mut InteractionEngine {
    &mut self.interaction
  }

  pub fn interaction_state(&self) -> &InteractionState {
    self.interaction.interaction_state()
  }

  /// Render a new frame if anything is dirty (DOM or interaction state).
  pub fn render_if_needed(&mut self) -> Result<Option<crate::Pixmap>> {
    self
      .document
      .render_if_needed_with_interaction_state(Some(self.interaction.interaction_state()))
  }

  /// Render a new frame unconditionally.
  pub fn render_frame(&mut self) -> Result<crate::Pixmap> {
    self
      .document
      .render_frame_with_scroll_state_and_interaction_state(Some(
        self.interaction.interaction_state(),
      ))
      .map(|frame| frame.pixmap)
  }

  /// Returns `true` when the most recently prepared fragment tree contains any time-based effects
  /// (currently CSS animations/transitions) that require periodic ticking.
  pub fn wants_ticks(&self) -> bool {
    crate::ui::document_ticks::browser_document_wants_ticks(&self.document)
  }

  /// Advance the chrome document's animation timeline.
  ///
  /// This mirrors the worker-side tick protocol:
  /// - When `now_ms` is `Some(t)`, CSS animations/transitions are sampled at `t` milliseconds since
  ///   load by calling [`BrowserDocument::set_animation_time_ms`]. This only invalidates paint, so
  ///   the next render can repaint from cached layout artifacts.
  /// - When `now_ms` is `None`, real-time animation sampling is enabled and callers should only
  ///   repaint when [`BrowserDocument::needs_animation_frame`] reports that the animation clock has
  ///   advanced.
  ///
  /// Returns `true` when callers should render a new frame.
  pub fn tick(&mut self, now_ms: Option<f32>) -> bool {
    match now_ms {
      Some(ms) => {
        self.document.set_animation_time_ms(ms);
        true
      }
      None => {
        // Ensure explicit timelines are cleared so real-time sampling is active.
        self.document.set_animation_time(None);
        self.document.set_realtime_animations_enabled(true);
        self.document.needs_animation_frame()
      }
    }
  }

  pub fn address_bar_value(&mut self) -> String {
    let mut out: Option<String> = None;
    self.document.mutate_dom(|dom| {
      out = dom_input_value_by_element_id(dom, CHROME_ADDRESS_BAR_ID);
      false
    });
    out.unwrap_or_default()
  }

  pub fn address_bar_has_focus(&self) -> bool {
    self.last_address_bar_focused
  }

  /// Set the address bar value from browser state (state → DOM sync).
  ///
  /// This does **not** emit [`ChromeFrameEvent::AddressBarTextChanged`] (the state already holds the
  /// authoritative value).
  pub fn set_address_bar_value_from_state(&mut self, value: &str) {
    let mut changed = false;
    self.document.mutate_dom(|dom| {
      changed = dom_set_input_value_by_element_id(dom, CHROME_ADDRESS_BAR_ID, value);
      changed
    });
    if changed {
      self.last_address_bar_value.clear();
      self.last_address_bar_value.push_str(value);
    }
  }

  pub fn focus_address_bar(&mut self) -> Vec<ChromeFrameEvent> {
    self.focus_node_id(Some(self.address_bar_node_id), true)
  }

  pub fn blur_address_bar(&mut self) -> Vec<ChromeFrameEvent> {
    self.focus_node_id(None, false)
  }

  fn focus_node_id(&mut self, node_id: Option<usize>, focus_visible: bool) -> Vec<ChromeFrameEvent> {
    let mut action: InteractionAction = InteractionAction::None;
    self.document.mutate_dom(|dom| {
      let (_changed, got_action) = self.interaction.focus_node_id(dom, node_id, focus_visible);
      action = got_action;
      // Focus changes are reflected via interaction state hashing (BrowserDocument invalidates on
      // render), so do not force DOM invalidation here.
      false
    });
    self.events_for_interaction_action(action)
  }

  pub fn select_all_address_bar(&mut self) {
    let end = self.last_address_bar_value.chars().count();
    self.interaction.set_text_selection_range(self.address_bar_node_id, 0, end);
  }

  pub fn text_input(&mut self, text: &str) -> Vec<ChromeFrameEvent> {
    self.mutate_text_value(|engine, dom| engine.text_input(dom, text))
  }

  pub fn paste(&mut self, text: &str) -> Vec<ChromeFrameEvent> {
    self.mutate_text_value(|engine, dom| engine.clipboard_paste(dom, text))
  }

  /// Update the active IME preedit (composition) string for the focused text control.
  pub fn ime_preedit(&mut self, text: &str, cursor: Option<(usize, usize)>) -> bool {
    let mut changed = false;
    let _ = self.document.mutate_dom(|dom| {
      changed = self.interaction.ime_preedit(dom, text, cursor);
      // IME preedit is non-DOM-visible state; do not mark the document dirty here.
      false
    });
    changed
  }

  pub fn ime_commit(&mut self, text: &str) -> Vec<ChromeFrameEvent> {
    self.mutate_text_value(|engine, dom| engine.ime_commit(dom, text))
  }

  /// Cancel any active IME preedit string without mutating the DOM value.
  pub fn ime_cancel(&mut self) -> bool {
    let mut changed = false;
    let _ = self.document.mutate_dom(|dom| {
      changed = self.interaction.ime_cancel(dom);
      // IME cancel is non-DOM-visible state; do not mark the document dirty here.
      false
    });
    changed
  }

  fn mutate_text_value(
    &mut self,
    mut f: impl FnMut(&mut InteractionEngine, &mut DomNode) -> bool,
  ) -> Vec<ChromeFrameEvent> {
    let mut before = String::new();
    let mut after: Option<String> = None;
    let mut dom_changed = false;
    self.document.mutate_dom(|dom| {
      before = dom_input_value_by_element_id(dom, CHROME_ADDRESS_BAR_ID).unwrap_or_default();
      let _changed = f(&mut self.interaction, dom);
      let new_value = dom_input_value_by_element_id(dom, CHROME_ADDRESS_BAR_ID).unwrap_or_default();
      if new_value != before {
        after = Some(new_value);
        dom_changed = true;
      }
      dom_changed
    });

    let mut events = Vec::new();
    if let Some(after) = after {
      if after != self.last_address_bar_value {
        self.last_address_bar_value = after.clone();
      }
      events.push(ChromeFrameEvent::AddressBarTextChanged(after));
    }
    events
  }

  fn events_for_interaction_action(&mut self, action: InteractionAction) -> Vec<ChromeFrameEvent> {
    match action {
      InteractionAction::FocusChanged { node_id } => {
        let focused = node_id == Some(self.address_bar_node_id);
        if focused != self.last_address_bar_focused {
          self.last_address_bar_focused = focused;
          vec![ChromeFrameEvent::AddressBarFocusChanged(focused)]
        } else {
          Vec::new()
        }
      }
      _ => Vec::new(),
    }
  }

  /// Set (or clear) the currently dragged tab id.
  pub fn set_dragging_tab(&mut self, tab_id: Option<TabId>) {
    self.dragging_tab_id = tab_id;
  }

  fn tab_strip_bounds(&mut self) -> Option<Rect> {
    let tab_strip_node_id = {
      let mut id: Option<usize> = None;
      self.document.mutate_dom(|dom| {
        let index = DomIndex::build(dom);
        id = index.id_by_element_id.get(CHROME_TAB_STRIP_ID).copied();
        false
      });
      id?
    };

    let prepared = self.document.prepared()?;
    let scroll = self.document.scroll_state();
    let tree = prepared.fragment_tree_for_geometry(&scroll);
    let bounds_map = absolute_bounds_by_styled_node_id(prepared.box_tree(), &tree);
    let bounds_page = bounds_map.get(&tab_strip_node_id).copied()?;

    let dx = if scroll.viewport.x.is_finite() {
      -scroll.viewport.x
    } else {
      0.0
    };
    let dy = if scroll.viewport.y.is_finite() {
      -scroll.viewport.y
    } else {
      0.0
    };
    Some(bounds_page.translate(Point::new(dx, dy)))
  }

  fn should_detach_tab_from_drop(strip_bounds: Rect, pointer_pos: Point) -> bool {
    let y = pointer_pos.y;
    if !y.is_finite() {
      return false;
    }
    y < strip_bounds.min_y() - TAB_DETACH_VERTICAL_THRESHOLD_CSS_PX
      || y > strip_bounds.max_y() + TAB_DETACH_VERTICAL_THRESHOLD_CSS_PX
  }

  /// Handle a pointer-up event in chrome-viewport coordinates.
  ///
  /// When a tab drag is active and the drop ends sufficiently outside the tab strip, this emits a
  /// [`ChromeAction::DetachTab`]. Otherwise it behaves like a normal drag drop (tab reorder, if any,
  /// is expected to have been applied during the drag).
  pub fn pointer_up(&mut self, pointer_pos: Point) -> Vec<ChromeAction> {
    let Some(tab_id) = self.dragging_tab_id.take() else {
      return Vec::new();
    };

    let Some(strip_bounds) = self.tab_strip_bounds() else {
      // Treat geometry failures as a normal drop.
      return Vec::new();
    };

    if Self::should_detach_tab_from_drop(strip_bounds, pointer_pos) {
      vec![ChromeAction::DetachTab(tab_id)]
    } else {
      Vec::new()
    }
  }
}

// -----------------------------------------------------------------------------
// Experimental browser integration helpers (BrowserAppState.chrome ↔ DOM sync)
// -----------------------------------------------------------------------------

use crate::ui::browser_app::BrowserAppState;

/// Apply DOM-driven chrome frame events to the canonical `BrowserAppState.chrome` model.
pub fn apply_chrome_frame_event(app: &mut BrowserAppState, event: ChromeFrameEvent) {
  match event {
    ChromeFrameEvent::AddressBarTextChanged(text) => {
      app.chrome.address_bar_text = text;
      // Typing implies "editing" mode even if some other action previously disabled it while
      // keeping focus (mirrors egui `TextEdit::changed()` behaviour).
      app.chrome.address_bar_editing = true;
      app.chrome.address_bar_has_focus = true;
    }
    ChromeFrameEvent::AddressBarFocusChanged(has_focus) => {
      // Focus changes should not automatically enter "editing" mode:
      // - focusing the omnibox does not imply the user modified it yet,
      // - but losing focus should discard any uncommitted edits.
      app.chrome.address_bar_has_focus = has_focus;
      if !has_focus {
        // Only revert to the active tab URL when the user was actively editing.
        if app.chrome.address_bar_editing {
          app.set_address_bar_editing(false);
        } else {
          // Still close the dropdown so stale suggestion state doesn't linger.
          app.chrome.omnibox.reset();
        }
      }
    }
  }
}

/// Drive state → DOM sync for the address bar:
/// - update the DOM input value from `BrowserAppState.chrome.address_bar_text`
/// - consume one-frame focus/select-all requests and translate them into DOM focus/selection.
pub fn sync_browser_state_to_chrome_frame(app: &mut BrowserAppState, chrome: &mut ChromeFrameDocument) {
  // Always keep the DOM value consistent with the model; the model already avoids clobbering typed
  // edits during `address_bar_editing`.
  chrome.set_address_bar_value_from_state(&app.chrome.address_bar_text);

  // If state explicitly cleared focus (e.g. after committing a navigation), propagate that to the
  // DOM so the interaction engine doesn't keep treating the input as focused.
  if !app.chrome.address_bar_has_focus && chrome.address_bar_has_focus() {
    let events = chrome.blur_address_bar();
    for event in events {
      apply_chrome_frame_event(app, event);
    }
  }

  // Apply focus/select-all requests *after* syncing value so selection uses the correct length.
  if app.chrome.request_focus_address_bar {
    let events = chrome.focus_address_bar();
    for event in events {
      apply_chrome_frame_event(app, event);
    }
    app.chrome.request_focus_address_bar = false;
  }

  if app.chrome.request_select_all_address_bar {
    if !chrome.address_bar_has_focus() {
      let events = chrome.focus_address_bar();
      for event in events {
        apply_chrome_frame_event(app, event);
      }
    }
    chrome.select_all_address_bar();
    app.chrome.request_select_all_address_bar = false;
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::text::font_db::FontConfig;
  use crate::ui::omnibox_nav::{apply_omnibox_nav_key, OmniboxNavKey};
  use crate::ui::ChromeAction;

  #[test]
  fn address_bar_text_input_emits_event() {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");
    let mut doc =
      ChromeFrameDocument::new_with_renderer(renderer, (320, 80), 1.0).expect("create chrome frame");

    // Focus the address bar (programmatic; equivalent to Ctrl/Cmd+L focus request).
    let focus_events = doc.focus_address_bar();
    assert!(
      focus_events
        .iter()
        .any(|e| matches!(e, ChromeFrameEvent::AddressBarFocusChanged(true))),
      "expected focus event when focusing address bar, got {focus_events:?}"
    );

    // Type text.
    let events = doc.text_input("hello");
    assert_eq!(
      events,
      vec![ChromeFrameEvent::AddressBarTextChanged("hello".to_string())]
    );
    assert_eq!(doc.address_bar_value(), "hello");
  }

  #[test]
  fn chrome_frame_address_bar_sync_updates_browser_state_and_enter_clears_editing() {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");
    let mut chrome =
      ChromeFrameDocument::new_with_renderer(renderer, (320, 80), 1.0).expect("create chrome frame");

    let mut app = BrowserAppState::new_with_initial_tab("https://example.invalid/".to_string());
    // Start with a blank omnibox value so the test doesn't depend on initial tab URL formatting.
    app.chrome.address_bar_text.clear();
    app.chrome.address_bar_editing = false;
    app.chrome.address_bar_has_focus = false;

    // State -> DOM initial sync.
    sync_browser_state_to_chrome_frame(&mut app, &mut chrome);

    // Focus should update `address_bar_has_focus` but not enter editing mode.
    for event in chrome.focus_address_bar() {
      apply_chrome_frame_event(&mut app, event);
    }
    assert!(app.chrome.address_bar_has_focus);
    assert!(!app.chrome.address_bar_editing);

    // Typing should sync DOM -> state and flip editing on.
    for event in chrome.text_input("example.com") {
      apply_chrome_frame_event(&mut app, event);
    }
    assert_eq!(app.chrome.address_bar_text, "example.com");
    assert!(app.chrome.address_bar_editing);

    // Enter should resolve into a navigation action and clear editing/focus.
    let outcome = apply_omnibox_nav_key(&mut app, OmniboxNavKey::Enter);
    assert_eq!(
      outcome.action,
      Some(ChromeAction::NavigateTo("example.com".to_string()))
    );
    assert!(!app.chrome.address_bar_editing);
    assert!(!app.chrome.address_bar_has_focus);

    // Apply state -> DOM sync; this should blur the DOM input to match the model and keep the
    // committed value visible.
    sync_browser_state_to_chrome_frame(&mut app, &mut chrome);
    assert!(!chrome.address_bar_has_focus());
    assert_eq!(chrome.address_bar_value(), "example.com");
  }

  #[test]
  fn drag_drop_outside_strip_emits_detach() {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");
    let mut doc =
      ChromeFrameDocument::new_with_renderer(renderer, (800, 200), 1.0).expect("create chrome frame");

    // Populate layout cache so `tab_strip_bounds` can query fragment geometry.
    let _ = doc.render_frame().expect("render chrome frame");

    doc.set_dragging_tab(Some(TabId(1)));
    let actions = doc.pointer_up(Point::new(10.0, -50.0));
    assert_eq!(actions, vec![ChromeAction::DetachTab(TabId(1))]);
  }
}
