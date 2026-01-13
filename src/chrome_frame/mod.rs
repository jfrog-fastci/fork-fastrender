use crate::api::BrowserDocument;
use crate::dom::{DomNode, DomNodeType};
use crate::geometry::Point;
use crate::interaction::{InteractionAction, InteractionEngine, InteractionState, KeyAction};
use crate::ui::omnibox_nav::{apply_omnibox_nav_key, OmniboxNavKey};
use crate::ui::{
  BrowserAppState, ChromeAction, ChromeActionUrl, OmniboxSuggestion, PointerButton, PointerModifiers,
};
use crate::{FastRender, Pixmap, RenderOptions, Result};

pub mod dom_mutation;

const ADDRESS_BAR_ID: &str = "address-bar";

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

      #address-form {
        margin: 0;
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

      #omnibox {
        margin: 0 6px 6px 6px;
        border: 1px solid #888;
        border-radius: 4px;
        background: white;
        color: black;
        width: 244px;
      }

      .omnibox-item {
        padding: 2px 6px;
      }

      .omnibox-item.selected {
        background: rgb(200, 200, 255);
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
      <form id="address-form" action="chrome-action:navigate" method="get" autocomplete="off">
        <input id="address-bar" name="url" type="text" value="" />
      </form>
      <span id="tab-title"></span>
    </div>
    <div id="omnibox" hidden></div>
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
    || !state.visited_links().is_empty()
    || state.ime_preedit.is_some()
    || state.text_edit.is_some()
    || state.form_state.has_overrides()
    || state.document_selection.is_some()
    || !state.user_validity().is_empty()
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

  /// Render a new chrome frame only when something is invalidated (DOM mutations, scroll, etc).
  ///
  /// Returns `Ok(None)` when no dirty flags are set and no time-based repaint is required.
  pub fn render_if_needed(&mut self) -> Result<Option<Pixmap>> {
    if self.dirty {
      return Ok(Some(self.render()?));
    }
    let interaction_state = self.interaction.interaction_state();
    // Preserve `interaction_state = None` behavior (notably autofocus synthesis) unless we have any
    // dynamic interaction state to apply.
    let interaction_state =
      has_nontrivial_interaction_state(interaction_state).then_some(interaction_state);
    let rendered = self
      .document
      .render_if_needed_with_interaction_state(interaction_state)?;
    if rendered.is_some() {
      self.dirty = false;
    }
    Ok(rendered)
  }

  /// Returns `true` when the most recently prepared fragment tree contains any time-based effects
  /// (currently CSS animations/transitions) that require periodic ticking.
  pub fn wants_ticks(&self) -> bool {
    crate::document_ticks::browser_document_wants_ticks(&self.document)
  }

  /// Advance the animation timeline.
  ///
  /// - When `now_ms` is `Some(t)`, CSS animations/transitions are sampled at `t` milliseconds since
  ///   load. This marks paint dirty (but does not invalidate style/layout), allowing repaints from
  ///   cached layout artifacts.
  /// - When `now_ms` is `None`, real-time animation sampling is enabled and a repaint is requested
  ///   only when [`BrowserDocument::needs_animation_frame`] indicates the animation clock has
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
        // Ensure any previous explicit timeline is cleared so real-time sampling can be used.
        self.document.set_animation_time(None);
        self.document.set_realtime_animations_enabled(true);
        self.document.needs_animation_frame()
      }
    }
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

  pub fn interaction_state(&self) -> &InteractionState {
    self.interaction.interaction_state()
  }

  fn address_bar_node_id(dom: &DomNode) -> Option<usize> {
    // Node ids are pre-order traversal indices (see `crate::dom::enumerate_dom_ids`).
    let mut next_id = 1usize;
    let mut stack: Vec<&DomNode> = vec![dom];
    while let Some(node) = stack.pop() {
      let current_id = next_id;
      next_id = next_id.saturating_add(1);
      if node
        .is_element()
        .then(|| node.get_attribute_ref("id"))
        .flatten()
        .is_some_and(|value| value == ADDRESS_BAR_ID)
      {
        return Some(current_id);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn address_bar_value(dom: &mut DomNode) -> String {
    dom_mutation::find_element_by_id_mut(dom, ADDRESS_BAR_ID)
      .and_then(|node| node.get_attribute_ref("value"))
      .unwrap_or_default()
      .to_string()
  }

  fn sync_address_bar_focus_flags(&mut self, app: &mut BrowserAppState) {
    let address_id = Self::address_bar_node_id(self.document.dom());
    let has_focus = address_id.is_some_and(|id| self.interaction_state().focused == Some(id));

    if has_focus == app.chrome.address_bar_has_focus {
      return;
    }

    app.chrome.address_bar_has_focus = has_focus;

    if !has_focus {
      if app.chrome.address_bar_editing {
        // Discard uncommitted edits and close the dropdown.
        app.set_address_bar_editing(false);
      } else {
        app.chrome.omnibox.reset();
      }
    }
  }

  /// Programmatically focus the address bar input.
  ///
  /// When `focus_visible` is true (keyboard modality), this selects all text so the next typed
  /// character replaces the current value (matching typical browser omnibox behavior).
  pub fn focus_address_bar(&mut self, app: &mut BrowserAppState, focus_visible: bool) -> bool {
    let Some(address_id) = Self::address_bar_node_id(self.document.dom()) else {
      return false;
    };

    let (document, interaction) = (&mut self.document, &mut self.interaction);
    let mut action = InteractionAction::None;
    let changed = document.mutate_dom(|dom| {
      let (interaction_changed, got_action) =
        interaction.focus_node_id(dom, Some(address_id), focus_visible);
      action = got_action;

      if focus_visible {
        // Select all so typing replaces the full value.
        let len = Self::address_bar_value(dom).chars().count();
        interaction.set_text_selection_range(address_id, 0, len);
      }

      interaction_changed
    });

    if matches!(action, InteractionAction::FocusChanged { .. }) {
      self.sync_address_bar_focus_flags(app);
    }

    self.dirty |= changed;
    changed
  }

  /// Apply a text input event to the DOM and sync the address bar value into `BrowserAppState`.
  pub fn text_input_address_bar(&mut self, app: &mut BrowserAppState, text: &str) -> bool {
    let address_id = Self::address_bar_node_id(self.document.dom());
    let mut before = String::new();
    let mut after = String::new();

    let (document, interaction) = (&mut self.document, &mut self.interaction);
    let changed = document.mutate_dom(|dom| {
      let focused = address_id.is_some_and(|id| interaction.interaction_state().focused == Some(id));
      if focused {
        before = Self::address_bar_value(dom);
      }
      let interaction_changed = interaction.text_input(dom, text);
      if focused {
        after = Self::address_bar_value(dom);
      }
      interaction_changed
    });

    if before != after
      && address_id.is_some_and(|id| self.interaction_state().focused == Some(id))
    {
      app.set_address_bar_text(after);
      app.chrome.address_bar_editing = true;
      app.chrome.address_bar_has_focus = true;
    }

    self.dirty |= changed;
    changed
  }

  /// Apply a keyboard action to the chrome DOM, returning any dispatched chrome action.
  pub fn key_action(
    &mut self,
    app: &mut BrowserAppState,
    key: KeyAction,
  ) -> (bool, Option<ChromeAction>) {
    let address_id = Self::address_bar_node_id(self.document.dom());
    let mut before = String::new();
    let mut after = String::new();

    let document_url = self
      .document
      .document_url()
      .unwrap_or("chrome://chrome-frame/")
      .to_string();
    let base_url = self
      .document
      .base_url()
      .unwrap_or(document_url.as_str())
      .to_string();

    let mut interaction_action = InteractionAction::None;
    let (document, interaction) = (&mut self.document, &mut self.interaction);
    let changed = document.mutate_dom(|dom| {
      let focused = address_id.is_some_and(|id| interaction.interaction_state().focused == Some(id));
      if focused {
        before = Self::address_bar_value(dom);
      }

      let (interaction_changed, action) = interaction.key_activate(dom, key, &document_url, &base_url);
      interaction_action = action;

      let focused = address_id.is_some_and(|id| interaction.interaction_state().focused == Some(id));
      if focused {
        after = Self::address_bar_value(dom);
      }

      interaction_changed
    });

    if matches!(interaction_action, InteractionAction::FocusChanged { .. }) {
      self.sync_address_bar_focus_flags(app);
    }

    if before != after
      && address_id.is_some_and(|id| self.interaction_state().focused == Some(id))
    {
      app.set_address_bar_text(after);
      app.chrome.address_bar_editing = true;
      app.chrome.address_bar_has_focus = true;
    }

    let chrome_action = match interaction_action {
      InteractionAction::Navigate { href } | InteractionAction::OpenInNewTab { href } => {
        ChromeActionUrl::parse(&href)
          .and_then(|parsed| parsed.into_chrome_action())
          .ok()
      }
      _ => None,
    };

    // Submitting the address bar should stop "editing" so future navigation commits can resync.
    if matches!(chrome_action, Some(ChromeAction::NavigateTo(_))) {
      app.chrome.address_bar_editing = false;
      app.chrome.omnibox.reset();
    }

    self.dirty |= changed;
    (changed, chrome_action)
  }

  pub(crate) fn text_input(&mut self, text: &str) -> bool {
    let (document, interaction) = (&mut self.document, &mut self.interaction);
    let changed = document.mutate_dom(|dom| interaction.text_input(dom, text));
    self.dirty |= changed;
    changed
  }

  /// Simulate a primary-button click at the given viewport point.
  ///
  /// Returns `false` when the document has no cached layout yet (call [`render`](Self::render)
  /// first).
  pub(crate) fn click_viewport_point(&mut self, viewport_point: Point) -> bool {
    let scroll = self.document.scroll_state();
    let document_url = self
      .document
      .document_url()
      .unwrap_or("chrome://chrome-frame/")
      .to_string();
    let base_url = self
      .document
      .base_url()
      .unwrap_or(document_url.as_str())
      .to_string();

    let hit_tree =
      (scroll.viewport != Point::ZERO || !scroll.elements.is_empty())
        .then(|| self.document.prepared().map(|prepared| prepared.fragment_tree_for_geometry(&scroll)))
        .flatten();

    let (document, interaction) = (&mut self.document, &mut self.interaction);
    let result = document.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let hit_tree = hit_tree.as_ref().unwrap_or(fragment_tree);

      let mut changed = interaction.pointer_down_with_click_count(
        dom,
        box_tree,
        hit_tree,
        &scroll,
        viewport_point,
        PointerButton::Primary,
        PointerModifiers::NONE,
        1,
      );
      let (up_changed, _action) = interaction.pointer_up_with_scroll(
        dom,
        box_tree,
        hit_tree,
        &scroll,
        viewport_point,
        PointerButton::Primary,
        PointerModifiers::NONE,
        true,
        &document_url,
        &base_url,
      );
      changed |= up_changed;
      (changed, changed)
    });

    match result {
      Ok(changed) => {
        self.dirty |= changed;
        changed
      }
      Err(_) => false,
    }
  }

  /// Return the selected text for either:
  /// - a focused text control (`<input>` / `<textarea>`), or
  /// - an active document selection (e.g. from [`select_all`](Self::select_all) when no text
  ///   control is focused).
  pub fn copy(&mut self) -> Option<String> {
    let mut copied: Option<String> = None;
    {
      let (document, interaction) = (&mut self.document, &mut self.interaction);
      if document
        .mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
          copied = interaction.clipboard_copy_with_layout(dom, box_tree, fragment_tree);
          (false, ())
        })
        .is_ok()
      {
        return copied;
      }
    }

    // Fallback when layout is unavailable.
    let (document, interaction) = (&mut self.document, &mut self.interaction);
    let _ = document.mutate_dom(|dom| {
      copied = interaction.clipboard_copy(dom);
      false
    });
    copied
  }

  /// Cut the current selection into the clipboard, deleting it when the control is editable.
  ///
  /// Returns `(changed, clipboard_text)`.
  pub fn cut(&mut self) -> (bool, Option<String>) {
    let mut cut_text: Option<String> = None;
    let changed = {
      let (document, interaction) = (&mut self.document, &mut self.interaction);
      document.mutate_dom(|dom| {
        let (dom_changed, text) = interaction.clipboard_cut(dom);
        cut_text = text;
        dom_changed
      })
    };
    self.dirty |= changed;

    // When the focused element isn't an editable text control, native browsers typically treat
    // Cut as Copy (copy selection but do not delete). Mirror that for document selections.
    if cut_text.is_none() {
      cut_text = self.copy();
    }

    (changed, cut_text)
  }

  /// Paste text into the focused text control (`<input>`/`<textarea>`), replacing any selection.
  pub fn paste(&mut self, text: &str) -> bool {
    let (document, interaction) = (&mut self.document, &mut self.interaction);
    let changed = document.mutate_dom(|dom| interaction.clipboard_paste(dom, text));
    self.dirty |= changed;
    changed
  }

  /// Select all text in the focused text control, falling back to selecting the document when no
  /// text control is focused.
  pub fn select_all(&mut self) -> bool {
    let (document, interaction) = (&mut self.document, &mut self.interaction);
    let changed = document.mutate_dom(|dom| interaction.clipboard_select_all(dom));
    self.dirty |= changed;
    changed
  }

  /// Update the active IME preedit (composition) string for the focused text control.
  pub fn ime_preedit(&mut self, text: &str, cursor: Option<usize>) -> bool {
    let cursor = cursor.map(|idx| (idx, idx));
    let (document, interaction) = (&mut self.document, &mut self.interaction);
    let changed = document.mutate_dom(|dom| interaction.ime_preedit(dom, text, cursor));
    self.dirty |= changed;
    changed
  }

  /// Commit IME text into the focused text control, clearing any active preedit.
  pub fn ime_commit(&mut self, text: &str) -> bool {
    let (document, interaction) = (&mut self.document, &mut self.interaction);
    let changed = document.mutate_dom(|dom| interaction.ime_commit(dom, text));
    self.dirty |= changed;
    changed
  }

  /// Cancel any active IME preedit string without mutating the DOM value.
  pub fn ime_cancel(&mut self) -> bool {
    let (document, interaction) = (&mut self.document, &mut self.interaction);
    let changed = document.mutate_dom(|dom| interaction.ime_cancel(dom));
    self.dirty |= changed;
    changed
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
    let title: &str = active.map(|t| t.display_title()).unwrap_or("New Tab");

    let changed = self.document.mutate_dom(|dom: &mut DomNode| {
      let mut changed = false;

      // Address bar:
      // - When the user is editing, mirror `ChromeState::address_bar_text` so keyboard omnibox
      //   navigation can preview suggestions by updating the value.
      // - Otherwise, display the current committed URL from the active tab.
      if let Some(address) = dom_mutation::find_element_by_id_mut(dom, ADDRESS_BAR_ID) {
        let desired = if app.chrome.address_bar_has_focus || app.chrome.address_bar_editing {
          app.chrome.address_bar_text.as_str()
        } else {
          active_url
        };
        changed |= dom_mutation::set_attr(address, "value", desired);
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

      // Omnibox dropdown list.
      if let Some(omnibox) = dom_mutation::find_element_by_id_mut(dom, "omnibox") {
        let show = app.chrome.omnibox.open && !app.chrome.omnibox.suggestions.is_empty();
        changed |= dom_mutation::set_bool_attr(omnibox, "hidden", !show);

        if show {
          let selected = app.chrome.omnibox.selected;
          let desired_len = app.chrome.omnibox.suggestions.len();

          let needs_rebuild = omnibox.children.len() != desired_len
            || omnibox.children.iter().any(|c| !c.is_element());
          if needs_rebuild {
            omnibox.children.clear();
            for (idx, suggestion) in app.chrome.omnibox.suggestions.iter().enumerate() {
              let class = if selected == Some(idx) {
                "omnibox-item selected"
              } else {
                "omnibox-item"
              };
              let label = omnibox_item_label(suggestion);

              let mut item = DomNode {
                node_type: DomNodeType::Element {
                  tag_name: "div".to_string(),
                  namespace: String::new(),
                  attributes: vec![
                    ("class".to_string(), class.to_string()),
                    ("data-index".to_string(), idx.to_string()),
                  ],
                },
                children: Vec::new(),
              };
              dom_mutation::set_text_content(&mut item, label);
              omnibox.children.push(item);
            }
            changed = true;
          } else {
            for (idx, (child, suggestion)) in omnibox
              .children
              .iter_mut()
              .zip(app.chrome.omnibox.suggestions.iter())
              .enumerate()
            {
              let class = if selected == Some(idx) {
                "omnibox-item selected"
              } else {
                "omnibox-item"
              };
              changed |= dom_mutation::set_attr(child, "class", class);
              changed |= dom_mutation::set_attr(child, "data-index", &idx.to_string());
              changed |= dom_mutation::set_text_content(child, omnibox_item_label(suggestion));
            }
          }
        } else if !omnibox.children.is_empty() {
          omnibox.children.clear();
          changed = true;
        }
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
      let hit_tree =
        (scroll.viewport != Point::ZERO || !scroll.elements.is_empty())
          .then(|| self.document.prepared().map(|prepared| prepared.fragment_tree_for_geometry(&scroll)))
          .flatten();
      let interaction = &mut self.interaction;
      self.document.mutate_dom_with_layout_artifacts(
        |dom: &mut DomNode, box_tree, fragment_tree| {
          let hit_tree = hit_tree.as_ref().unwrap_or(fragment_tree);
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

  /// Apply an address-bar omnibox navigation key and update the DOM.
  ///
  /// Returns the accepted [`ChromeAction`] for Enter, if any.
  pub fn handle_address_bar_key(
    &mut self,
    app: &mut BrowserAppState,
    key: OmniboxNavKey,
  ) -> Option<ChromeAction> {
    if !app.chrome.address_bar_has_focus {
      return None;
    }

    let outcome = apply_omnibox_nav_key(app, key);
    self.sync_state(app);
    outcome.action
  }
}

fn omnibox_item_label(suggestion: &OmniboxSuggestion) -> &str {
  suggestion
    .title
    .as_deref()
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .or_else(|| {
      suggestion
        .url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    })
    .or_else(|| crate::ui::omnibox_nav::omnibox_suggestion_fill_text(suggestion))
    .unwrap_or("")
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::clock::VirtualClock;
  use crate::text::font_db::FontConfig;
  use crate::ui::{BrowserTabState, OmniboxAction, OmniboxSuggestionSource, OmniboxUrlSource, TabId};
  use std::collections::hash_map::DefaultHasher;
  use std::hash::{Hash, Hasher};
  use std::sync::Arc;
  use std::time::Duration;

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

  #[test]
  fn chrome_frame_omnibox_keyboard_navigation_emits_actions() -> Result<()> {
    let tab1 = TabId::new();
    let tab2 = TabId::new();
    let mut app = BrowserAppState::new();
    app.push_tab(
      BrowserTabState::new(tab1, "https://a.example/".to_string()),
      true,
    );
    app.push_tab(
      BrowserTabState::new(tab2, "https://b.example/".to_string()),
      false,
    );

    app.chrome.address_bar_text = "typed input".to_string();
    app.chrome.address_bar_editing = true;
    app.chrome.address_bar_has_focus = true;
    app.chrome.omnibox.suggestions = vec![crate::ui::OmniboxSuggestion {
      action: OmniboxAction::ActivateTab(tab2),
      title: Some("B".to_string()),
      url: Some("https://b.example/".to_string()),
      source: OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab),
    }];

    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()?;
    let mut chrome = ChromeFrameDocument::new(renderer, RenderOptions::new().with_viewport(360, 80))?;
    chrome.sync_state(&app);

    assert_eq!(
      chrome.handle_address_bar_key(&mut app, OmniboxNavKey::ArrowDown),
      None
    );
    let action = chrome.handle_address_bar_key(&mut app, OmniboxNavKey::Enter);
    assert_eq!(action, Some(ChromeAction::ActivateTab(tab2)));
    Ok(())
  }

  #[test]
  fn chrome_frame_address_bar_sync_updates_app_state_and_emits_navigate_action() -> Result<()> {
    let mut app = BrowserAppState::new_with_initial_tab("https://example.com/".to_string());
    app.chrome.address_bar_text.clear();

    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()?;
    let mut chrome =
      ChromeFrameDocument::new(renderer, RenderOptions::new().with_viewport(360, 40))?;
    chrome.sync_state(&app);

    chrome.focus_address_bar(&mut app, true);
    assert!(app.chrome.address_bar_has_focus);
    assert!(!app.chrome.address_bar_editing);

    chrome.text_input_address_bar(&mut app, "example.com");
    assert_eq!(app.chrome.address_bar_text, "example.com");
    assert!(app.chrome.address_bar_editing);

    let (_changed, action) = chrome.key_action(&mut app, KeyAction::Enter);
    assert_eq!(
      action,
      Some(ChromeAction::NavigateTo("example.com".to_string()))
    );
    assert!(!app.chrome.address_bar_editing);
    Ok(())
  }

  #[test]
  fn chrome_frame_tick_advances_css_keyframes_animation() -> Result<()> {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()?;

    let options = RenderOptions::new().with_viewport(32, 32);
    let mut chrome = ChromeFrameDocument::new(renderer, options.clone())?;

    // Replace the built-in chrome template with a tiny deterministic animated document.
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

    chrome.document_mut().reset_with_html(html, options.clone())?;

    assert!(
      !chrome.wants_ticks(),
      "expected wants_ticks to be false before the first render (no prepared fragment tree)"
    );

    // Prime the layout/paint cache so wants_ticks can inspect the prepared fragment tree.
    let _ = chrome.render()?;
    assert!(
      chrome.wants_ticks(),
      "expected wants_ticks to be true after first render for a document containing @keyframes"
    );

    // Drive the animation timeline deterministically and ensure subsequent renders differ.
    assert!(chrome.tick(Some(0.0)), "tick(Some) should request a repaint");
    let first = chrome
      .render_if_needed()?
      .expect("expected a new frame after tick(Some(0.0))");
    let first_hash = pixmap_hash(&first);

    assert!(chrome.tick(Some(500.0)), "tick(Some) should request a repaint");
    let second = chrome
      .render_if_needed()?
      .expect("expected a new frame after tick(Some(500.0))");
    let second_hash = pixmap_hash(&second);

    assert_ne!(
      first_hash, second_hash,
      "expected keyframes animation sampling to change the rendered output between two times"
    );
    Ok(())
  }

  #[test]
  fn chrome_frame_tick_realtime_animations_only_repaint_when_clock_advances() -> Result<()> {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()?;

    let options = RenderOptions::new().with_viewport(32, 32);
    let mut chrome = ChromeFrameDocument::new(renderer, options.clone())?;

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

    // Install a deterministic animation clock so real-time sampling is predictable in tests.
    let clock = Arc::new(VirtualClock::new());
    chrome.document_mut().set_animation_clock(Arc::clone(&clock));
    chrome.document_mut().reset_with_html(html, options.clone())?;

    // Prime the layout/paint cache so `wants_ticks()` can observe keyframes.
    chrome.render()?;
    assert!(chrome.wants_ticks(), "expected wants_ticks after first render");

    // First realtime tick enables sampling and should request a paint.
    assert!(
      chrome.tick(None),
      "expected tick(None) to request a repaint when enabling realtime animations"
    );
    let first = chrome
      .render_if_needed()?
      .expect("expected repaint after enabling realtime animations");
    let first_hash = pixmap_hash(&first);

    // Without advancing the clock, no repaint should be needed.
    assert!(
      !chrome.tick(None),
      "expected tick(None) to be false when animation clock did not advance"
    );
    assert!(
      chrome.render_if_needed()?.is_none(),
      "expected render_if_needed to return None when clock did not advance"
    );

    // Advance the clock: repaint should be needed and the output should change.
    clock.advance(Duration::from_millis(500));
    assert!(
      chrome.tick(None),
      "expected tick(None) to request repaint after clock advance"
    );
    let second = chrome
      .render_if_needed()?
      .expect("expected repaint after clock advance");
    let second_hash = pixmap_hash(&second);
    assert_ne!(
      first_hash, second_hash,
      "expected real-time animation sampling to change rendered output after clock advance"
    );

    Ok(())
  }
}
