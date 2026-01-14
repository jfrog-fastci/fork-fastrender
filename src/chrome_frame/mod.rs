use crate::api::BrowserDocument;
use crate::dom::{DomNode, DomNodeType};
use crate::geometry::Point;
use crate::interaction::dom_index::DomIndex;
use crate::interaction::{fragment_tree_with_scroll, InteractionEngine, InteractionState};
use crate::ui::omnibox_nav::{apply_omnibox_nav_key, OmniboxNavKey};
use crate::ui::{
  BrowserAppState, ChromeAction, ChromeActionUrl, ChromeDynamicAssetFetcher, OmniboxAction,
  OmniboxSearchSource, OmniboxSuggestionSource, OmniboxUrlSource, PointerButton,
  PointerModifiers,
};
use crate::{Error, FastRender, Pixmap, RenderOptions, Result};

pub mod dom_mutation;

/// Stable `id=` attribute for the address bar `<input>`.
pub const CHROME_ADDRESS_BAR_ID: &str = "address-bar";
/// Stable `id=` attribute for the address bar `<form>`.
pub const CHROME_ADDRESS_FORM_ID: &str = "address-form";

/// Stable `id=` attribute for the omnibox dropdown root.
const CHROME_OMNIBOX_POPUP_ID: &str = "omnibox-popup";

/// Stable `id=` attribute for the tab strip root.
const CHROME_TAB_STRIP_ID: &str = "tab-strip";

const CHROME_TOOLBAR_BACK_ID: &str = "toolbar-back";
const CHROME_TOOLBAR_FORWARD_ID: &str = "toolbar-forward";
const CHROME_TOOLBAR_RELOAD_ID: &str = "toolbar-reload";
const CHROME_TOOLBAR_STOP_ID: &str = "toolbar-stop";
const CHROME_TOOLBAR_HOME_ID: &str = "toolbar-home";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChromeFrameEvent {
  /// Emitted when the address bar `<input>`'s value changes due to text input/paste/IME commits.
  AddressBarTextChanged(String),
  /// Emitted when focus enters/leaves the address bar `<input>`.
  AddressBarFocusChanged(bool),
}

#[derive(Debug, Default)]
pub struct ChromeFrameClickOutcome {
  /// True when any DOM-visible or interaction-visible state changed (e.g. focus/selection).
  pub changed: bool,
  /// High-level chrome-frame events derived from the interaction (e.g. address bar focus/value
  /// changes).
  pub events: Vec<ChromeFrameEvent>,
  /// Parsed `chrome-action:` navigation, when the click triggered one.
  pub action: Option<ChromeAction>,
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

#[must_use]
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

fn node_has_class(node: &DomNode, class_name: &str) -> bool {
  node
    .get_attribute_ref("class")
    .is_some_and(|classes| classes.split_ascii_whitespace().any(|c| c == class_name))
}

fn find_first_descendant_with_class_mut<'a>(
  node: &'a mut DomNode,
  class_name: &str,
) -> Option<&'a mut DomNode> {
  if node.is_element() && node_has_class(node, class_name) {
    return Some(node);
  }
  for child in node.children.iter_mut() {
    if let Some(found) = find_first_descendant_with_class_mut(child, class_name) {
      return Some(found);
    }
  }
  None
}

/// Live, mutable chrome-frame document rendered via `BrowserDocument`.
///
/// This is a first step towards a "renderer chrome" implementation where the browser UI is
/// represented as HTML/CSS and rendered by FastRender itself.
pub struct ChromeFrameDocument {
  document: BrowserDocument,
  interaction: InteractionEngine,
  address_bar_node_id: usize,
  /// Cached address bar value for change detection.
  last_address_bar_value: String,
  /// Cached address bar focus state for change detection.
  last_address_bar_focused: bool,
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
    || !state.visited_links().is_empty()
    || state.ime_preedit.is_some()
    || state.text_edit.is_some()
    || state.form_state().has_overrides()
    || state.document_selection.is_some()
    || !state.user_validity().is_empty()
}

impl ChromeFrameDocument {
  /// Convenience constructor for callers that already have a configured [`FastRender`] and
  /// [`RenderOptions`].
  pub fn new_with_renderer_and_options(renderer: FastRender, options: RenderOptions) -> Result<Self> {
    Self::new(renderer, options)
  }

  /// Convenience constructor that configures viewport/dpr into [`RenderOptions`].
  pub fn new_with_renderer(renderer: FastRender, viewport_css: (u32, u32), dpr: f32) -> Result<Self> {
    let options = RenderOptions::new()
      .with_viewport(viewport_css.0, viewport_css.1)
      .with_device_pixel_ratio(dpr);
    Self::new(renderer, options)
  }

  /// Create a chrome frame document from the canonical renderer-chrome template.
  pub fn new(renderer: FastRender, options: RenderOptions) -> Result<Self> {
    // Bootstrap with an empty/default browser state; callers are expected to drive state → DOM sync
    // via [`ChromeFrameDocument::sync_state`].
    let bootstrap_state = BrowserAppState::new();
    let html = crate::ui::chrome_frame::chrome_frame_html_from_state(&bootstrap_state);

    let mut document = BrowserDocument::new(renderer, &html, options)?;

    let mut address_bar_node_id: Option<usize> = None;
    let mut address_bar_value: Option<String> = None;
    document.mutate_dom(|dom| {
      let index = DomIndex::build(dom);
      address_bar_node_id = index.id_by_element_id.get(CHROME_ADDRESS_BAR_ID).copied();
      if let Some(node_id) = address_bar_node_id {
        if let Some(node) = index.node(node_id) {
          address_bar_value = Some(node.get_attribute_ref("value").unwrap_or("").to_string());
        }
      }
      false
    });

    let address_bar_node_id = address_bar_node_id.ok_or_else(|| {
      Error::Other(format!(
        "chrome frame template missing element id={CHROME_ADDRESS_BAR_ID:?}"
      ))
    })?;

    Ok(Self {
      document,
      interaction: InteractionEngine::new(),
      address_bar_node_id,
      last_address_bar_value: address_bar_value.unwrap_or_default(),
      last_address_bar_focused: false,
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

  /// Alias for [`render`](Self::render), matching the naming used by other renderer documents.
  pub fn render_frame(&mut self) -> Result<Pixmap> {
    self.render()
  }

  /// Render a new chrome frame only when something is invalidated (DOM mutations, scroll, etc).
  ///
  /// Returns `Ok(None)` when no dirty flags are set and no animation frame is required.
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

  /// Returns `true` when the most recently prepared fragment tree contains any CSS animations or
  /// transitions that require time-based sampling.
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

  pub fn dom(&self) -> &DomNode {
    self.document.dom()
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
      self.dirty = true;
    }
  }

  pub fn focus_address_bar(&mut self) -> Vec<ChromeFrameEvent> {
    self.focus_node_id(Some(self.address_bar_node_id), true)
  }

  pub fn blur_address_bar(&mut self) -> Vec<ChromeFrameEvent> {
    self.focus_node_id(None, false)
  }

  fn focus_node_id(&mut self, node_id: Option<usize>, focus_visible: bool) -> Vec<ChromeFrameEvent> {
    let mut action: crate::interaction::InteractionAction = crate::interaction::InteractionAction::None;
    self.document.mutate_dom(|dom| {
      let (_changed, got_action) = self.interaction.focus_node_id(dom, node_id, focus_visible);
      action = got_action;
      false
    });
    self.events_for_interaction_action(action)
  }

  pub fn select_all_address_bar(&mut self) {
    let end = self.last_address_bar_value.chars().count();
    self
      .interaction
      .set_text_selection_range(self.address_bar_node_id, 0, end);
  }

  pub fn text_input(&mut self, text: &str) -> Vec<ChromeFrameEvent> {
    self.mutate_address_bar_text_value(|engine, dom| engine.text_input(dom, text))
  }

  /// Simulate a primary-button click at the given viewport point.
  ///
  /// If the document has not been rendered yet, this will render the first frame implicitly so
  /// cached layout artifacts are available for hit-testing.
  pub fn click_viewport_point(&mut self, viewport_point: Point) -> Result<ChromeFrameClickOutcome> {
    if self.document.prepared().is_none() {
      let _ = self.render()?;
    }

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

    let (document, interaction) = (&mut self.document, &mut self.interaction);
    let outcome = document.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let scrolled_tree =
        (!scroll.elements.is_empty()).then(|| fragment_tree_with_scroll(fragment_tree, &scroll));
      let hit_tree = scrolled_tree.as_ref().unwrap_or(fragment_tree);

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
      let (up_changed, action) = interaction.pointer_up_with_scroll(
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
      (changed, (changed, action))
    });

    let (changed, action) = outcome?;
    self.dirty |= changed;

    let (events, chrome_action) = self.outcome_for_interaction_action(action);
    Ok(ChromeFrameClickOutcome {
      changed,
      events,
      action: chrome_action,
    })
  }

  /// Update hover state for a pointer move in viewport CSS pixels.
  ///
  /// This is a convenience wrapper around [`InteractionEngine::pointer_move`]. The document must
  /// have been rendered at least once before calling this (so cached layout artifacts exist). When
  /// the document has not been rendered yet, this method will render the first frame implicitly.
  ///
  /// Returns `true` when the interaction state changed in a way that should trigger a repaint
  /// (notably hover/active pseudo-classes).
  pub fn pointer_move(&mut self, pos_css: (f32, f32)) -> Result<bool> {
    // Support the same sentinel used by the windowed browser integration: a negative coordinate
    // means "pointer left this frame".
    if pos_css.0 < 0.0 || pos_css.1 < 0.0 {
      return Ok(self.pointer_leave());
    }

    if self.document.prepared().is_none() {
      // Populate the layout cache so we can hit-test.
      let _ = self.render()?;
    }

    let scroll = self.document.scroll_state();
    let viewport_point = Point::new(pos_css.0, pos_css.1);
    let (document, interaction) = (&mut self.document, &mut self.interaction);
    let changed = document.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let scrolled =
        (!scroll.elements.is_empty()).then(|| fragment_tree_with_scroll(fragment_tree, &scroll));
      let fragment_tree = scrolled.as_ref().unwrap_or(fragment_tree);
      let changed = interaction.pointer_move(dom, box_tree, fragment_tree, &scroll, viewport_point);
      (changed, changed)
    })?;

    self.dirty |= changed;
    Ok(changed)
  }

  /// Clear hover/active pointer state (equivalent to a `pointerleave` event).
  ///
  /// This should be called by embeddings when the cursor leaves the chrome region so `:hover`
  /// styles/cursor state do not remain stuck while the pointer is over other composited content.
  ///
  /// Returns `true` when the interaction state changed (i.e. a rerender is required to clear
  /// hover/active styling).
  pub fn pointer_leave(&mut self) -> bool {
    let state = self.interaction.interaction_state();
    let had_hover = !state.hover_chain().is_empty();
    let had_active = !state.active_chain().is_empty();
    if !had_hover && !had_active {
      // Still clear internal drag state so future interactions don't carry stale capture state, but
      // avoid forcing a rerender when no pseudo-class-visible state changed.
      self.interaction.clear_pointer_state_without_dom();
      return false;
    }

    self.interaction.clear_pointer_state_without_dom();
    self.dirty = true;
    true
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
  pub fn paste(&mut self, text: &str) -> Vec<ChromeFrameEvent> {
    self.mutate_address_bar_text_value(|engine, dom| engine.clipboard_paste(dom, text))
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
  pub fn ime_preedit(&mut self, text: &str, cursor: Option<(usize, usize)>) -> bool {
    let (document, interaction) = (&mut self.document, &mut self.interaction);
    let changed = document.mutate_dom(|dom| interaction.ime_preedit(dom, text, cursor));
    self.dirty |= changed;
    changed
  }

  /// Commit IME text into the focused text control, clearing any active preedit.
  pub fn ime_commit(&mut self, text: &str) -> Vec<ChromeFrameEvent> {
    self.mutate_address_bar_text_value(|engine, dom| engine.ime_commit(dom, text))
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
    let can_go_back = active.map(|t| t.can_go_back).unwrap_or(false);
    let can_go_forward = active.map(|t| t.can_go_forward).unwrap_or(false);
    let loading = active.map(|t| t.loading).unwrap_or(false);

    // Address bar:
    // - The browser UI state owns the displayed text (`ChromeState::address_bar_text`) and already
    //   gates syncs while `address_bar_editing` is true (see `BrowserAppState::sync_address_bar_to_active`).
    // - While navigating omnibox suggestions via keyboard, that same state is updated to preview the
    //   selected item.
    let desired_address_bar_value = app.chrome.address_bar_text.as_str();
    let mut changed = false;
    if self.last_address_bar_value != desired_address_bar_value {
      self.set_address_bar_value_from_state(desired_address_bar_value);
      changed = true;
    }

    let mut changed_before_address_bar = false;
    let dom_changed = self.document.mutate_dom(|dom: &mut DomNode| {
      let mut dom_changed = false;

      // -----------------------------------------------------------------------
      // Toolbar buttons (back/forward/reload/stop/home)
      // -----------------------------------------------------------------------
      fn sync_toolbar_button(node: &mut DomNode, base_class: &str, href: &str, enabled: bool) -> bool {
        let mut changed = false;
        let class = if enabled {
          format!("toolbar-button {base_class}")
        } else {
          format!("toolbar-button {base_class} disabled")
        };
        changed |= dom_mutation::set_attr(node, "class", &class);

        if enabled {
          changed |= dom_mutation::set_attr(node, "href", href);
          changed |= dom_mutation::remove_attr(node, "aria-disabled");
        } else {
          changed |= dom_mutation::remove_attr(node, "href");
          changed |= dom_mutation::set_attr(node, "aria-disabled", "true");
        }

        changed
      }

      if let Some(back) = dom_mutation::find_element_by_id_mut(dom, CHROME_TOOLBAR_BACK_ID) {
        dom_changed |= sync_toolbar_button(back, "back", "chrome-action:back", can_go_back);
      }
      if let Some(forward) = dom_mutation::find_element_by_id_mut(dom, CHROME_TOOLBAR_FORWARD_ID) {
        dom_changed |=
          sync_toolbar_button(forward, "forward", "chrome-action:forward", can_go_forward);
      }
      if let Some(reload) = dom_mutation::find_element_by_id_mut(dom, CHROME_TOOLBAR_RELOAD_ID) {
        dom_changed |= sync_toolbar_button(reload, "reload", "chrome-action:reload", !loading);
      }
      if let Some(stop) = dom_mutation::find_element_by_id_mut(dom, CHROME_TOOLBAR_STOP_ID) {
        dom_changed |=
          sync_toolbar_button(stop, "stop", "chrome-action:stop-loading", loading);
      }
      if let Some(home) = dom_mutation::find_element_by_id_mut(dom, CHROME_TOOLBAR_HOME_ID) {
        dom_changed |= sync_toolbar_button(home, "home", "chrome-action:home", true);
      }

      // -----------------------------------------------------------------------
      // Tab strip
      // -----------------------------------------------------------------------
      if let Some(tab_strip) = dom_mutation::find_element_by_id_mut(dom, CHROME_TAB_STRIP_ID) {
        let mut element_children = tab_strip.children.iter_mut().filter(|c| c.is_element());

        let mut needs_rebuild = false;
        for tab in &app.tabs {
          let Some(node) = element_children.next() else {
            needs_rebuild = true;
            break;
          };
          let tab_id_str = tab.id.0.to_string();
          if node.get_attribute_ref("data-tab-id") != Some(tab_id_str.as_str()) {
            needs_rebuild = true;
            break;
          }
        }
        if element_children.next().is_some() {
          // Extra element children beyond our model.
          needs_rebuild = true;
        }

        if needs_rebuild {
          tab_strip.children.clear();
          for tab in &app.tabs {
            let tab_id_str = tab.id.0.to_string();
            let class = if app.active_tab_id() == Some(tab.id) {
              "tab active"
            } else {
              "tab"
            };
            let favicon_url = ChromeDynamicAssetFetcher::favicon_url(tab.id);
            let title = tab.display_title();

            let activate_href = ChromeActionUrl::ActivateTab { tab_id: tab.id }.to_url_string();
            let close_href = ChromeActionUrl::CloseTab { tab_id: tab.id }.to_url_string();

            let mut tab_node = DomNode {
              node_type: DomNodeType::Element {
                tag_name: "div".to_string(),
                namespace: String::new(),
                attributes: vec![
                  ("class".to_string(), class.to_string()),
                  ("data-tab-id".to_string(), tab_id_str),
                ],
              },
              children: Vec::new(),
            };

            // <a class="tab-activate" ...>
            let mut activate = DomNode {
              node_type: DomNodeType::Element {
                tag_name: "a".to_string(),
                namespace: String::new(),
                attributes: vec![
                  ("class".to_string(), "tab-activate".to_string()),
                  ("href".to_string(), activate_href),
                ],
              },
              children: Vec::new(),
            };
            // <img class="tab-favicon" ... />
            activate.children.push(DomNode {
              node_type: DomNodeType::Element {
                tag_name: "img".to_string(),
                namespace: String::new(),
                attributes: vec![
                  ("class".to_string(), "tab-favicon".to_string()),
                  ("src".to_string(), favicon_url),
                  ("alt".to_string(), String::new()),
                ],
              },
              children: Vec::new(),
            });
            // <span class="tab-title">...</span>
            let mut title_span = DomNode {
              node_type: DomNodeType::Element {
                tag_name: "span".to_string(),
                namespace: String::new(),
                attributes: vec![("class".to_string(), "tab-title".to_string())],
              },
              children: Vec::new(),
            };
            dom_mutation::set_text_content(&mut title_span, &title);
            activate.children.push(title_span);
            tab_node.children.push(activate);

            // <a class="tab-close" ...>×</a>
            let mut close = DomNode {
              node_type: DomNodeType::Element {
                tag_name: "a".to_string(),
                namespace: String::new(),
                attributes: vec![
                  ("class".to_string(), "tab-close".to_string()),
                  ("aria-label".to_string(), "Close tab".to_string()),
                  ("href".to_string(), close_href),
                ],
              },
              children: Vec::new(),
            };
            dom_mutation::set_text_content(&mut close, "×");
            tab_node.children.push(close);

            tab_strip.children.push(tab_node);
          }
          dom_changed = true;
          changed_before_address_bar = true;
        } else {
          // Patch in place (preserves node ids so focus/selection are stable).
          let mut element_children = tab_strip.children.iter_mut().filter(|c| c.is_element());
          for tab in &app.tabs {
            let Some(node) = element_children.next() else {
              break;
            };

            let class = if app.active_tab_id() == Some(tab.id) {
              "tab active"
            } else {
              "tab"
            };
            dom_changed |= dom_mutation::set_attr(node, "class", class);
            let tab_id_str = tab.id.0.to_string();
            dom_changed |= dom_mutation::set_attr(node, "data-tab-id", &tab_id_str);

            // Update activate href, title, favicon.
            if let Some(activate) = find_first_descendant_with_class_mut(node, "tab-activate") {
              let activate_href = ChromeActionUrl::ActivateTab { tab_id: tab.id }.to_url_string();
              dom_changed |= dom_mutation::set_attr(activate, "href", &activate_href);
            }
            if let Some(favicon) = find_first_descendant_with_class_mut(node, "tab-favicon") {
              let favicon_url = ChromeDynamicAssetFetcher::favicon_url(tab.id);
              dom_changed |= dom_mutation::set_attr(favicon, "src", &favicon_url);
            }
            if let Some(title_span) = find_first_descendant_with_class_mut(node, "tab-title") {
              let title = tab.display_title();
              dom_changed |= dom_mutation::set_text_content(title_span, &title);
            }
            if let Some(close) = find_first_descendant_with_class_mut(node, "tab-close") {
              let close_href = ChromeActionUrl::CloseTab { tab_id: tab.id }.to_url_string();
              dom_changed |= dom_mutation::set_attr(close, "href", &close_href);
            }
          }
        }
      }

      // -----------------------------------------------------------------------
      // Omnibox popup
      // -----------------------------------------------------------------------
      if let Some(popup) = dom_mutation::find_element_by_id_mut(dom, CHROME_OMNIBOX_POPUP_ID) {
        let show = app.chrome.omnibox.open && !app.chrome.omnibox.suggestions.is_empty();
        dom_changed |= dom_mutation::set_bool_attr(popup, "hidden", !show);

        if !show {
          if !popup.children.is_empty() {
            popup.children.clear();
            dom_changed = true;
          }
        } else {
          popup.children.clear();
          for (idx, suggestion) in app.chrome.omnibox.suggestions.iter().enumerate() {
            let href = match &suggestion.action {
              OmniboxAction::NavigateToUrl => {
                let url = suggestion.url.clone().unwrap_or_default();
                ChromeActionUrl::Navigate { url }.to_url_string()
              }
              OmniboxAction::Search(query) => {
                ChromeActionUrl::Navigate { url: query.clone() }.to_url_string()
              }
              OmniboxAction::ActivateTab(tab_id) => {
                ChromeActionUrl::ActivateTab { tab_id: *tab_id }.to_url_string()
              }
            };

            let type_class = match &suggestion.action {
              OmniboxAction::NavigateToUrl => "omnibox-type-url",
              OmniboxAction::Search(_) => "omnibox-type-search",
              OmniboxAction::ActivateTab(_) => "omnibox-type-tab",
            };
            let source_class = match suggestion.source {
              OmniboxSuggestionSource::Primary => "omnibox-source-primary",
              OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest) => {
                "omnibox-source-remote-suggest"
              }
              OmniboxSuggestionSource::Url(OmniboxUrlSource::About) => "omnibox-source-about",
              OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark) => "omnibox-source-bookmark",
              OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab) => "omnibox-source-closed-tab",
              OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab) => "omnibox-source-open-tab",
              OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited) => "omnibox-source-visited",
            };

            let selected = app.chrome.omnibox.selected == Some(idx);
            let mut classes = format!("omnibox-suggestion {type_class} {source_class}");
            if selected {
              classes.push_str(" selected");
            }

            let aria_selected = if selected { "true" } else { "false" };

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

            let mut row = DomNode {
              node_type: DomNodeType::Element {
                tag_name: "a".to_string(),
                namespace: String::new(),
                attributes: vec![
                  ("id".to_string(), format!("omnibox-suggestion-{idx}")),
                  ("class".to_string(), classes),
                  ("role".to_string(), "option".to_string()),
                  ("aria-selected".to_string(), aria_selected.to_string()),
                  ("href".to_string(), href),
                ],
              },
              children: Vec::new(),
            };

            row.children.push(DomNode {
              node_type: DomNodeType::Element {
                tag_name: "span".to_string(),
                namespace: String::new(),
                attributes: vec![
                  ("class".to_string(), "omnibox-icon".to_string()),
                  ("aria-hidden".to_string(), "true".to_string()),
                ],
              },
              children: Vec::new(),
            });

            let mut text_wrap = DomNode {
              node_type: DomNodeType::Element {
                tag_name: "span".to_string(),
                namespace: String::new(),
                attributes: vec![("class".to_string(), "omnibox-text".to_string())],
              },
              children: Vec::new(),
            };

            let mut title_span = DomNode {
              node_type: DomNodeType::Element {
                tag_name: "span".to_string(),
                namespace: String::new(),
                attributes: vec![("class".to_string(), "omnibox-title".to_string())],
              },
              children: Vec::new(),
            };
            dom_mutation::set_text_content(&mut title_span, title);
            text_wrap.children.push(title_span);

            if let Some(url) = secondary {
              let mut url_span = DomNode {
                node_type: DomNodeType::Element {
                  tag_name: "span".to_string(),
                  namespace: String::new(),
                  attributes: vec![("class".to_string(), "omnibox-url".to_string())],
                },
                children: Vec::new(),
              };
              dom_mutation::set_text_content(&mut url_span, url);
              text_wrap.children.push(url_span);
            }

            row.children.push(text_wrap);
            popup.children.push(row);
          }
          dom_changed = true;
        }
      }

      dom_changed
    });

    changed |= dom_changed;
    self.dirty |= dom_changed;

    if changed_before_address_bar {
      let old_node_id = self.address_bar_node_id;
      let was_focused = self.interaction_state().focused == Some(old_node_id);
      let focus_visible = self.interaction_state().focus_visible;

      let mut new_node_id: Option<usize> = None;
      self.document.mutate_dom(|dom| {
        let index = DomIndex::build(dom);
        new_node_id = index.id_by_element_id.get(CHROME_ADDRESS_BAR_ID).copied();
        false
      });

      if let Some(new_id) = new_node_id {
        self.address_bar_node_id = new_id;
        if was_focused && new_id != old_node_id {
          // Best-effort focus remap so subsequent text edits and focus queries target the updated id.
          let _ = self.focus_node_id(Some(new_id), focus_visible);
        }
      }
    }

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
    // Apply focus/selection side effects driven by the updated `BrowserAppState.chrome` (notably,
    // Enter/Escape can clear focus and we need to propagate that into the interaction engine so
    // `:focus` styles don't remain stuck).
    sync_browser_state_to_chrome_frame(app, self);
    outcome.action
  }

  fn mutate_address_bar_text_value(
    &mut self,
    mut f: impl FnMut(&mut InteractionEngine, &mut DomNode) -> bool,
  ) -> Vec<ChromeFrameEvent> {
    let mut before = String::new();
    let mut after: Option<String> = None;
    let mut changed = false;
    self.document.mutate_dom(|dom| {
      before = dom_input_value_by_element_id(dom, CHROME_ADDRESS_BAR_ID).unwrap_or_default();
      let f_changed = f(&mut self.interaction, dom);
      let new_value = dom_input_value_by_element_id(dom, CHROME_ADDRESS_BAR_ID).unwrap_or_default();
      if new_value != before {
        after = Some(new_value);
        changed = true;
      }
      // Ensure any non-value mutations requested by `f` are propagated (e.g. selection changes).
      changed || f_changed
    });

    self.dirty |= changed;

    let mut events = Vec::new();
    if let Some(after) = after {
      if after != self.last_address_bar_value {
        self.last_address_bar_value = after.clone();
      }
      events.push(ChromeFrameEvent::AddressBarTextChanged(after));
    }
    events
  }

  fn events_for_interaction_action(
    &mut self,
    action: crate::interaction::InteractionAction,
  ) -> Vec<ChromeFrameEvent> {
    match action {
      crate::interaction::InteractionAction::FocusChanged { node_id } => {
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

  fn outcome_for_interaction_action(
    &mut self,
    action: crate::interaction::InteractionAction,
  ) -> (Vec<ChromeFrameEvent>, Option<ChromeAction>) {
    use crate::interaction::InteractionAction;

    fn chrome_action_or(raw: String, fallback: fn(String) -> ChromeAction) -> Option<ChromeAction> {
      // `chrome-action:` URLs are our internal JS-free chrome bridge.
      //
      // Important: if a `chrome-action:` URL fails to parse, treat it as "no action" rather than
      // falling back to a normal navigation. This avoids accidentally treating malformed internal
      // action URLs as user navigations.
      if raw.to_ascii_lowercase().starts_with("chrome-action:") {
        return ChromeActionUrl::parse(&raw)
          .ok()
          .and_then(|url| url.into_chrome_action().ok());
      }
      Some(fallback(raw))
    }

    match action {
      InteractionAction::Navigate { href } => (
        Vec::new(),
        chrome_action_or(href, ChromeAction::NavigateTo),
      ),
      InteractionAction::OpenInNewTab { href } => (
        Vec::new(),
        chrome_action_or(href, ChromeAction::OpenUrlInNewTab),
      ),
      InteractionAction::NavigateRequest { request } => (
        Vec::new(),
        chrome_action_or(request.url, ChromeAction::NavigateTo),
      ),
      InteractionAction::OpenInNewTabRequest { request } => (
        Vec::new(),
        chrome_action_or(request.url, ChromeAction::OpenUrlInNewTab),
      ),
      other => (self.events_for_interaction_action(other), None),
    }
  }
}

// -----------------------------------------------------------------------------
// Browser integration helpers (`BrowserAppState.chrome` ↔ chrome-frame DOM sync)
// -----------------------------------------------------------------------------

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
  use crate::ui::{BrowserTabState, OmniboxAction, OmniboxSuggestionSource, OmniboxUrlSource, TabId};
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
  fn chrome_frame_address_bar_text_input_emits_event() -> Result<()> {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");
    let mut doc =
      ChromeFrameDocument::new(renderer, RenderOptions::new().with_viewport(320, 80))?;

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
    Ok(())
  }

  #[test]
  fn chrome_frame_address_bar_sync_updates_browser_state_and_enter_clears_editing() -> Result<()> {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");
    let mut chrome =
      ChromeFrameDocument::new(renderer, RenderOptions::new().with_viewport(320, 80))?;

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
    Ok(())
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
}
