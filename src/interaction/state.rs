use crate::text::caret::CaretAffinity;
use crate::interaction::selection_serialize::{DocumentSelectionPoint, DocumentSelectionRange};
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

  /// Current document (non-form-control) selection.
  pub document_selection: Option<DocumentSelectionState>,

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
