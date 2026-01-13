use crate::text::caret::CaretAffinity;
use crate::interaction::selection_serialize::{DocumentSelectionPoint, DocumentSelectionRange};
use crate::dom2::{NodeId, RendererDomMapping};
use rustc_hash::{FxHashMap, FxHashSet};
use std::cmp::Ordering;
use std::path::PathBuf;

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
  /// Current file selections for `<input type="file">`, keyed by DOM pre-order node id.
  pub file_inputs: FxHashMap<usize, Vec<FileSelection>>,
  /// Current selected option ids for `<select>` elements, keyed by the select element's DOM pre-order
  /// node id.
  ///
  /// When a select id is present in this map, the selection set is treated as authoritative for that
  /// select (including the empty set for multi-selects).
  pub select_selected: FxHashMap<usize, FxHashSet<usize>>,
}

/// A selected file for an `<input type="file">` control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSelection {
  pub path: PathBuf,
  pub filename: String,
  pub content_type: String,
  pub bytes: Vec<u8>,
}

impl FormState {
  #[inline]
  pub fn has_overrides(&self) -> bool {
    !(self.values.is_empty()
      && self.checked.is_empty()
      && self.file_inputs.is_empty()
      && self.select_selected.is_empty())
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
  pub fn files_for(&self, node_id: usize) -> Option<&Vec<FileSelection>> {
    self.file_inputs.get(&node_id)
  }

  #[inline]
  pub fn select_selected_options(&self, select_node_id: usize) -> Option<&FxHashSet<usize>> {
    self.select_selected.get(&select_node_id)
  }
}

/// Document (non-form-control) selection state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DocumentSelectionState {
  /// The entire rendered document (excluding non-selectable/hidden content).
  All,
  /// One or more explicit selection ranges.
  Ranges(DocumentSelectionRanges),
}

impl DocumentSelectionState {
  /// Returns true when this selection contains at least one non-collapsed range.
  pub fn has_highlight(&self) -> bool {
    match self {
      Self::All => true,
      Self::Ranges(ranges) => ranges.has_highlight(),
    }
  }
}

/// A multi-range document selection.
///
/// Ranges are expected to be:
/// - normalized (`start <= end`),
/// - ordered by DOM position, and
/// - non-overlapping (adjacent/overlapping ranges are merged).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DocumentSelectionRanges {
  pub ranges: Vec<DocumentSelectionRange>,
  /// Index into `ranges` representing the primary range for caret/extension semantics.
  pub primary: usize,
  /// Fixed anchor point for extending the primary range.
  pub anchor: DocumentSelectionPoint,
  /// Moving focus point for extending the primary range.
  pub focus: DocumentSelectionPoint,
}

impl DocumentSelectionRanges {
  pub fn collapsed(point: DocumentSelectionPoint) -> Self {
    Self {
      ranges: vec![DocumentSelectionRange {
        start: point,
        end: point,
      }],
      primary: 0,
      anchor: point,
      focus: point,
    }
  }

  pub fn has_highlight(&self) -> bool {
    self
      .ranges
      .iter()
      .any(|r| r.start != r.end)
  }

  fn cmp_point(a: DocumentSelectionPoint, b: DocumentSelectionPoint) -> Ordering {
    a.node_id
      .cmp(&b.node_id)
      .then_with(|| a.char_offset.cmp(&b.char_offset))
  }

  fn range_contains_range(
    outer: &DocumentSelectionRange,
    inner: &DocumentSelectionRange,
  ) -> bool {
    Self::cmp_point(outer.start, inner.start) != Ordering::Greater
      && Self::cmp_point(outer.end, inner.end) != Ordering::Less
  }

  /// Ensure `ranges` are normalized, sorted, and non-overlapping (merging overlap/adjacency).
  ///
  /// Also repairs `primary` to point at the range containing the current anchor/focus span.
  pub fn normalize(&mut self) {
    if self.ranges.is_empty() {
      self.primary = 0;
      return;
    }

    for range in &mut self.ranges {
      *range = range.normalized();
    }

    self.ranges.sort_by(|a, b| {
      Self::cmp_point(a.start, b.start).then_with(|| Self::cmp_point(a.end, b.end))
    });

    let mut merged: Vec<DocumentSelectionRange> = Vec::with_capacity(self.ranges.len());
    for range in self.ranges.drain(..) {
      if let Some(last) = merged.last_mut() {
        // Merge when overlapping or adjacent.
        if Self::cmp_point(range.start, last.end) != Ordering::Greater {
          if Self::cmp_point(range.end, last.end) == Ordering::Greater {
            last.end = range.end;
          }
          continue;
        }
      }
      merged.push(range);
    }
    self.ranges = merged;

    // Repair primary index based on the current anchor/focus span.
    let primary_span = DocumentSelectionRange {
      start: self.anchor,
      end: self.focus,
    }
    .normalized();
    if let Some(idx) = self
      .ranges
      .iter()
      .position(|r| Self::range_contains_range(r, &primary_span))
    {
      self.primary = idx;
    } else {
      // Fallback: clamp primary into bounds and update anchor/focus to match.
      self.primary = self.primary.min(self.ranges.len().saturating_sub(1));
      let primary = self.ranges[self.primary];
      self.anchor = primary.start;
      self.focus = primary.end;
    }
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
  focus_chain: Vec<usize>,
  focus_chain_membership: FxHashSet<usize>,

  /// The element under the pointer and its element ancestors (used for `:hover` matching).
  hover_chain: Vec<usize>,
  hover_chain_membership: FxHashSet<usize>,
  /// The active element (e.g. pointer down) and its element ancestors (used for `:active` matching).
  active_chain: Vec<usize>,
  active_chain_membership: FxHashSet<usize>,

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

  /// Current document (non-form-control) selection.
  pub document_selection: Option<DocumentSelectionState>,

  /// Node ids (controls/forms) that have flipped HTML "user validity" from false to true.
  ///
  /// This gates `:user-valid` / `:user-invalid` pseudo-classes.
  pub user_validity: FxHashSet<usize>,
}

impl InteractionState {
  #[inline]
  pub fn focus_chain(&self) -> &[usize] {
    &self.focus_chain
  }

  #[inline]
  pub fn hover_chain(&self) -> &[usize] {
    &self.hover_chain
  }

  #[inline]
  pub fn active_chain(&self) -> &[usize] {
    &self.active_chain
  }

  pub fn set_focus_chain(&mut self, chain: Vec<usize>) {
    self.focus_chain_membership.clear();
    self.focus_chain_membership.reserve(chain.len());
    for &id in &chain {
      self.focus_chain_membership.insert(id);
    }
    self.focus_chain = chain;
  }

  pub fn set_hover_chain(&mut self, chain: Vec<usize>) {
    self.hover_chain_membership.clear();
    self.hover_chain_membership.reserve(chain.len());
    for &id in &chain {
      self.hover_chain_membership.insert(id);
    }
    self.hover_chain = chain;
  }

  pub fn set_active_chain(&mut self, chain: Vec<usize>) {
    self.active_chain_membership.clear();
    self.active_chain_membership.reserve(chain.len());
    for &id in &chain {
      self.active_chain_membership.insert(id);
    }
    self.active_chain = chain;
  }

  pub fn clear_focus_chain(&mut self) {
    self.focus_chain.clear();
    self.focus_chain_membership.clear();
  }

  pub fn clear_hover_chain(&mut self) {
    self.hover_chain.clear();
    self.hover_chain_membership.clear();
  }

  pub fn clear_active_chain(&mut self) {
    self.active_chain.clear();
    self.active_chain_membership.clear();
  }

  pub(crate) fn mutate_focus_chain(&mut self, f: impl FnOnce(&mut Vec<usize>)) {
    f(&mut self.focus_chain);
    self.focus_chain_membership.clear();
    self.focus_chain_membership.reserve(self.focus_chain.len());
    for &id in &self.focus_chain {
      self.focus_chain_membership.insert(id);
    }
  }

  pub(crate) fn mutate_hover_chain(&mut self, f: impl FnOnce(&mut Vec<usize>)) {
    f(&mut self.hover_chain);
    self.hover_chain_membership.clear();
    self.hover_chain_membership.reserve(self.hover_chain.len());
    for &id in &self.hover_chain {
      self.hover_chain_membership.insert(id);
    }
  }

  pub(crate) fn mutate_active_chain(&mut self, f: impl FnOnce(&mut Vec<usize>)) {
    f(&mut self.active_chain);
    self.active_chain_membership.clear();
    self.active_chain_membership.reserve(self.active_chain.len());
    for &id in &self.active_chain {
      self.active_chain_membership.insert(id);
    }
  }

  #[inline]
  pub fn is_focused(&self, node_id: usize) -> bool {
    self.focused == Some(node_id)
  }

  #[inline]
  pub fn is_focus_within(&self, node_id: usize) -> bool {
    self.focus_chain_membership.contains(&node_id)
  }

  #[inline]
  pub fn is_hovered(&self, node_id: usize) -> bool {
    self.hover_chain_membership.contains(&node_id)
  }

  #[inline]
  pub fn is_active(&self, node_id: usize) -> bool {
    self.active_chain_membership.contains(&node_id)
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

/// Live (non-DOM) form control state keyed by stable [`dom2::NodeId`](crate::dom2::NodeId).
///
/// This is the stable counterpart to [`FormState`]. It is intended to be stored alongside a live
/// `dom2` document where nodes can be inserted/removed without invalidating this state.
#[derive(Debug, Clone, Default)]
pub struct FormStateDom2 {
  /// Current value for value-bearing controls (`<input>` / `<textarea>` / etc.), keyed by stable
  /// `dom2` [`NodeId`].
  pub values: FxHashMap<NodeId, String>,
  /// Current checked state for checkbox/radio inputs, keyed by stable `dom2` [`NodeId`].
  pub checked: FxHashMap<NodeId, bool>,
  /// Current file selections for `<input type="file">`, keyed by stable `dom2` [`NodeId`].
  pub file_inputs: FxHashMap<NodeId, Vec<FileSelection>>,
  /// Current selected option ids for `<select>` elements, keyed by stable `dom2` [`NodeId`].
  ///
  /// When a select id is present in this map, the selection set is treated as authoritative for that
  /// select (including the empty set for multi-selects).
  pub select_selected: FxHashMap<NodeId, FxHashSet<NodeId>>,
}

impl FormStateDom2 {
  #[inline]
  pub fn has_overrides(&self) -> bool {
    !(self.values.is_empty()
      && self.checked.is_empty()
      && self.file_inputs.is_empty()
      && self.select_selected.is_empty())
  }

  /// Project this stable state into the renderer's preorder-id keyed [`FormState`].
  ///
  /// Any entries whose nodes are detached from the renderer snapshot (unmappable `NodeId`) are
  /// dropped.
  pub fn project_to_preorder(&self, mapping: &RendererDomMapping) -> FormState {
    let mut projected = FormState::default();

    projected.values = self
      .values
      .iter()
      .filter_map(|(&node_id, value)| {
        mapping
          .preorder_for_node_id(node_id)
          .map(|preorder| (preorder, value.clone()))
      })
      .collect();

    projected.checked = self
      .checked
      .iter()
      .filter_map(|(&node_id, &checked)| {
        mapping
          .preorder_for_node_id(node_id)
          .map(|preorder| (preorder, checked))
      })
      .collect();

    projected.file_inputs = self
      .file_inputs
      .iter()
      .filter_map(|(&node_id, files)| {
        mapping
          .preorder_for_node_id(node_id)
          .map(|preorder| (preorder, files.clone()))
      })
      .collect();

    projected.select_selected = self
      .select_selected
      .iter()
      .filter_map(|(&select_id, options)| {
        let select_preorder = mapping.preorder_for_node_id(select_id)?;
        let projected_options: FxHashSet<usize> = options
          .iter()
          .filter_map(|&id| mapping.preorder_for_node_id(id))
          .collect();
        Some((select_preorder, projected_options))
      })
      .collect();

    projected
  }
}

/// In-progress IME (Input Method Editor) composition state for a focused control, keyed by stable
/// `dom2` node ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImePreeditStateDom2 {
  pub node_id: NodeId,
  pub text: String,
  pub cursor: Option<(usize, usize)>,
}

/// Caret + selection state for a focused text control (`<input>` / `<textarea>`), keyed by stable
/// `dom2` node ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextEditPaintStateDom2 {
  pub node_id: NodeId,
  /// Caret position in character indices.
  pub caret: usize,
  /// Visual affinity for the caret when the logical boundary maps to multiple x positions.
  pub caret_affinity: CaretAffinity,
  /// Optional selection range in character indices (start, end), where `start < end`.
  pub selection: Option<(usize, usize)>,
}

/// Internal, non-DOM-visible interaction state keyed by stable [`dom2::NodeId`](crate::dom2::NodeId).
///
/// ## Stable vs renderer interaction state
///
/// The renderer's selector/layout/paint pipeline consumes [`InteractionState`], which is keyed by
/// **renderer preorder ids** (the 1-based ids produced by [`crate::dom::enumerate_dom_ids`]).
/// Preorder ids are specific to a particular immutable DOM snapshot; they can change whenever the
/// underlying `dom2` document is mutated (insertion/removal/reordering).
///
/// `InteractionStateDom2` is the stable counterpart intended to be stored alongside the live `dom2`
/// document. When the renderer needs interaction state for a particular snapshot, project this
/// stable state to preorder ids using [`InteractionStateDom2::project_to_preorder`] with the
/// snapshot's [`RendererDomMapping`].
#[derive(Debug, Clone, Default)]
pub struct InteractionStateDom2 {
  /// Currently focused element `NodeId`.
  pub focused: Option<NodeId>,
  /// Whether the focused element should match `:focus-visible`.
  pub focus_visible: bool,
  /// The focused element and its element ancestors (used for `:focus-within` matching).
  pub focus_chain: Vec<NodeId>,
  /// The element under the pointer and its element ancestors (used for `:hover` matching).
  pub hover_chain: Vec<NodeId>,
  /// The active element (e.g. pointer down) and its element ancestors (used for `:active` matching).
  pub active_chain: Vec<NodeId>,
  /// Set of link node ids that have been visited in this document.
  pub visited_links: FxHashSet<NodeId>,
  /// Optional IME composition (preedit) state for the focused text control.
  pub ime_preedit: Option<ImePreeditStateDom2>,
  /// Optional caret/selection state for a focused text control (`<input>` / `<textarea>`).
  pub text_edit: Option<TextEditPaintStateDom2>,
  /// Live form state for value-bearing and toggleable controls.
  pub form_state: FormStateDom2,
  /// Node ids (controls/forms) that have flipped HTML "user validity" from false to true.
  pub user_validity: FxHashSet<NodeId>,
}

impl InteractionStateDom2 {
  /// Project this stable, `dom2::NodeId` keyed state into the renderer's preorder-id keyed
  /// [`InteractionState`].
  ///
  /// Mapping semantics:
  /// - Each `NodeId` is translated via [`RendererDomMapping::preorder_for_node_id`].
  /// - Any nodes that are detached/unmappable in the target snapshot are dropped.
  /// - For vec "chains", order is preserved while filtering out unmappable nodes.
  /// - If the focused node is unmappable, the projected `focused` is set to `None` and the projected
  ///   `focus_chain` is cleared (since it is derived from focus).
  pub fn project_to_preorder(&self, mapping: &RendererDomMapping) -> InteractionState {
    let focused_preorder = self
      .focused
      .and_then(|node_id| mapping.preorder_for_node_id(node_id));

    let focus_chain = if focused_preorder.is_some() {
      self
        .focus_chain
        .iter()
        .copied()
        .filter_map(|id| mapping.preorder_for_node_id(id))
        .collect()
    } else {
      Vec::new()
    };

    let hover_chain = self
      .hover_chain
      .iter()
      .copied()
      .filter_map(|id| mapping.preorder_for_node_id(id))
      .collect();

    let active_chain = self
      .active_chain
      .iter()
      .copied()
      .filter_map(|id| mapping.preorder_for_node_id(id))
      .collect();

    let visited_links: FxHashSet<usize> = self
      .visited_links
      .iter()
      .copied()
      .filter_map(|id| mapping.preorder_for_node_id(id))
      .collect();

    let user_validity: FxHashSet<usize> = self
      .user_validity
      .iter()
      .copied()
      .filter_map(|id| mapping.preorder_for_node_id(id))
      .collect();

    let ime_preedit = self.ime_preedit.as_ref().and_then(|state| {
      let node_id = mapping.preorder_for_node_id(state.node_id)?;
      Some(ImePreeditState {
        node_id,
        text: state.text.clone(),
        cursor: state.cursor,
      })
    });

    let text_edit = self.text_edit.and_then(|state| {
      let node_id = mapping.preorder_for_node_id(state.node_id)?;
      Some(TextEditPaintState {
        node_id,
        caret: state.caret,
        caret_affinity: state.caret_affinity,
        selection: state.selection,
      })
    });

    let mut projected = InteractionState::default();
    projected.focused = focused_preorder;
    projected.focus_visible = self.focus_visible && focused_preorder.is_some();
    projected.set_focus_chain(focus_chain);
    projected.set_hover_chain(hover_chain);
    projected.set_active_chain(active_chain);
    projected.visited_links = visited_links;
    projected.ime_preedit = ime_preedit;
    projected.text_edit = text_edit;
    projected.form_state = self.form_state.project_to_preorder(mapping);
    // Document selection state is currently tracked by the preorder-id based interaction engine.
    // Porting it to stable node ids is out of scope for this initial projection layer.
    projected.document_selection = None;
    projected.user_validity = user_validity;
    projected
  }
}
