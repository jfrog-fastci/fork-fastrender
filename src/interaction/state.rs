use rustc_hash::FxHashSet;

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
