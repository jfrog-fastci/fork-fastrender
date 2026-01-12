use crate::text::caret::CaretAffinity;
use rustc_hash::{FxHashMap, FxHashSet};

/// Live (non-DOM) form control state.
///
/// This is used to reflect user-driven changes to form controls without mutating DOM attributes.
/// Downstream systems (paint, accessibility, validation) can consult this store to surface the
/// current control state.
#[derive(Debug, Clone, Default)]
pub struct FormState {
  /// Current value for value-bearing controls (`<input>` / `<textarea>` / etc.), keyed by DOM
  /// pre-order node id.
  pub values: FxHashMap<usize, String>,
  /// Current checked state for checkbox/radio inputs, keyed by DOM pre-order node id.
  pub checked: FxHashMap<usize, bool>,
  /// Current selected option ids for `<select>` elements, keyed by the select element's DOM pre-order
  /// node id.
  ///
  /// When a select id is present in this map, the selection set is treated as authoritative for that
  /// select (including the empty set for multi-selects).
  pub select_selected: FxHashMap<usize, FxHashSet<usize>>,
}

impl FormState {
  #[inline]
  pub fn has_overrides(&self) -> bool {
    !(self.values.is_empty() && self.checked.is_empty() && self.select_selected.is_empty())
  }

  #[inline]
  pub fn value_for(&self, node_id: usize) -> Option<&str> {
    self.values.get(&node_id).map(|s| s.as_str())
  }

  #[inline]
  pub fn checked_for(&self, node_id: usize) -> Option<bool> {
    self.checked.get(&node_id).copied()
  }

  #[inline]
  pub fn select_selected_options(&self, select_node_id: usize) -> Option<&FxHashSet<usize>> {
    self.select_selected.get(&select_node_id)
  }
}

/// Internal, non-DOM-visible interaction state for a single document/tab.
///
/// This replaces the legacy `data-fastr-*` DOM attribute mutations that were previously used to
/// represent dynamic user interaction state (hover/active/focus/visited/user validity/IME preedit).
/// Keeping this state out of the DOM avoids observable author CSS/DOM side effects and reduces DOM
/// churn.
#[derive(Debug, Clone, Default)]
pub struct InteractionState {
  /// Currently focused element node id (pre-order id from `crate::dom::enumerate_dom_ids`).
  pub focused: Option<usize>,
  /// Whether the focused element should match `:focus-visible`.
  pub focus_visible: bool,
  /// The focused element and its element ancestors (used for `:focus-within` matching).
  pub focus_chain: Vec<usize>,

  /// The element under the pointer and its element ancestors (used for `:hover` matching).
  pub hover_chain: Vec<usize>,
  /// The active element (e.g. pointer down) and its element ancestors (used for `:active` matching).
  pub active_chain: Vec<usize>,

  /// Set of link node ids that have been visited in this document.
  ///
  /// Note: This is currently per-document (cleared on navigation), matching the legacy behaviour
  /// where visited state was stored on the DOM element itself.
  pub visited_links: FxHashSet<usize>,

  /// Optional IME composition (preedit) state for the focused text control.
  pub ime_preedit: Option<ImePreeditState>,

  /// Optional caret/selection state for a focused text control (`<input>` / `<textarea>`).
  ///
  /// This is internal UI state used for form-control painting. It must never be mirrored onto the
  /// DOM (e.g. via `data-*` attributes), because that would make selection/caret state observable to
  /// author CSS/DOM.
  pub text_edit: Option<TextEditPaintState>,

  /// Live form state for value-bearing and toggleable controls.
  pub form_state: FormState,

  /// Whether the document (non-form-control) currently has an active selection.
  ///
  /// This is a best-effort signal for downstream tooling (e.g. accessibility debug exports). The
  /// renderer does not yet expose a concrete document selection range.
  pub document_has_selection: bool,

  /// Node ids (controls/forms) that have flipped HTML "user validity" from false to true.
  ///
  /// This gates `:user-valid` / `:user-invalid` pseudo-classes.
  pub user_validity: FxHashSet<usize>,
}

impl InteractionState {
  #[inline]
  pub fn is_focused(&self, node_id: usize) -> bool {
    self.focused == Some(node_id)
  }

  #[inline]
  pub fn is_focus_within(&self, node_id: usize) -> bool {
    self.focus_chain.contains(&node_id)
  }

  #[inline]
  pub fn is_hovered(&self, node_id: usize) -> bool {
    self.hover_chain.contains(&node_id)
  }

  #[inline]
  pub fn is_active(&self, node_id: usize) -> bool {
    self.active_chain.contains(&node_id)
  }

  #[inline]
  pub fn is_visited_link(&self, node_id: usize) -> bool {
    self.visited_links.contains(&node_id)
  }

  #[inline]
  pub fn ime_preedit_for(&self, node_id: usize) -> Option<&str> {
    self
      .ime_preedit
      .as_ref()
      .filter(|state| state.node_id == node_id)
      .map(|state| state.text.as_str())
  }

  #[inline]
  pub fn text_edit_for(&self, node_id: usize) -> Option<&TextEditPaintState> {
    self
      .text_edit
      .as_ref()
      .filter(|state| state.node_id == node_id)
  }

  #[inline]
  pub fn has_user_validity(&self, node_id: usize) -> bool {
    self.user_validity.contains(&node_id)
  }
}

/// In-progress IME (Input Method Editor) composition state for a focused control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImePreeditState {
  pub node_id: usize,
  pub text: String,
  pub cursor: Option<(usize, usize)>,
}

/// Caret + selection state for a focused text control (`<input>` / `<textarea>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextEditPaintState {
  pub node_id: usize,
  /// Caret position in character indices.
  pub caret: usize,
  /// Visual affinity for the caret when the logical boundary maps to multiple x positions.
  pub caret_affinity: CaretAffinity,
  /// Optional selection range in character indices (start, end), where `start < end`.
  pub selection: Option<(usize, usize)>,
}
