use crate::dom::DomNode;
use crate::dom::DomNodeType;
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::interaction::selection_serialize::{
  serialize_document_selection, DocumentSelection, DocumentSelectionPoint, DocumentSelectionRange,
};
use crate::layout::contexts::inline::baseline::compute_line_height_with_metrics_viewport;
use crate::layout::contexts::inline::line_builder::TextItem;
use crate::scroll::ScrollState;
use crate::style::ComputedStyle;
use crate::style::types::Appearance;
use crate::text::caret::CaretAffinity;
use crate::tree::box_tree::BoxNode;
use crate::tree::box_tree::BoxTree;
use crate::tree::box_tree::BoxType;
use crate::tree::box_tree::FormControlKind;
use crate::tree::box_tree::ReplacedType;
use crate::tree::box_tree::SelectControl;
use crate::tree::box_tree::SelectItem;
use crate::tree::fragment_tree::FragmentTree;
use crate::tree::fragment_tree::{FragmentContent, HitTestRoot};
use crate::ui::messages::{CursorKind, MediaElementKind, PointerButton, PointerModifiers};
use rustc_hash::FxHashSet;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

use super::dom_mutation;
use super::form_submit::{
  form_submission, form_submission_without_submitter, FormSubmission, FormSubmissionMethod,
};
use super::fragment_geometry::content_rect_for_border_rect;
use super::hit_test::{
  hit_test_dom_with_indices, BoxIndex as HitTestBoxIndex, HitTestKind, HitTestResult,
};
use super::image_maps;
use super::resolve_url;
use super::state::{
  DocumentSelectionRanges, DocumentSelectionState, FileSelection, InteractionState,
  TextEditPaintState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputModality {
  Pointer,
  Keyboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateTimeInputKind {
  Date,
  Time,
  DateTimeLocal,
  Month,
  Week,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionAction {
  None,
  /// A pending default text insertion for a drag-and-drop gesture into a text control.
  ///
  /// The interaction engine computes focus/caret placement during the pointer gesture, but defers
  /// mutating the control's value so higher layers can dispatch cancelable JS `dragover`/`drop`
  /// events. If default handling is still allowed after event dispatch, the UI should call
  /// [`InteractionEngine::apply_text_drop`] to perform the insertion.
  TextDrop {
    target_dom_id: usize,
    text: String,
  },
  Navigate {
    href: String,
  },
  OpenInNewTab {
    href: String,
  },
  /// Trigger a download for the resolved URL (typically from `<a download>`).
  Download {
    href: String,
    /// Suggested filename from the element's `download` attribute (when present and non-empty).
    ///
    /// When `None`, the worker should derive a filename from the URL.
    file_name: Option<String>,
  },
  /// Navigation that carries an explicit HTTP method and optional body (used for form POST).
  NavigateRequest {
    request: FormSubmission,
  },
  /// Request that the submission be opened in a new tab (used for form `target=_blank` with POST).
  OpenInNewTabRequest {
    request: FormSubmission,
  },
  FocusChanged {
    node_id: Option<usize>,
  },
  OpenSelectDropdown {
    select_node_id: usize,
    control: crate::tree::box_tree::SelectControl,
  },
  OpenDateTimePicker {
    input_node_id: usize,
    kind: DateTimeInputKind,
  },
  OpenColorPicker {
    input_node_id: usize,
  },
  OpenFilePicker {
    input_node_id: usize,
    multiple: bool,
    accept: Option<String>,
  },
  /// Request that the UI show native media controls for a `<video>`/`<audio>` element.
  OpenMediaControls {
    media_node_id: usize,
    kind: MediaElementKind,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
  Backspace,
  Delete,
  WordBackspace,
  WordDelete,
  Enter,
  Tab,
  ShiftTab,
  Space,
  ShiftSpace,
  PageUp,
  PageDown,
  ArrowLeft,
  ArrowRight,
  WordLeft,
  WordRight,
  /// Move caret left by one word boundary, extending selection (Ctrl/Cmd/Alt+Shift+ArrowLeft).
  ShiftWordLeft,
  /// Move caret right by one word boundary, extending selection (Ctrl/Cmd/Alt+Shift+ArrowRight).
  ShiftWordRight,
  ShiftArrowLeft,
  ShiftArrowRight,
  ShiftArrowUp,
  ShiftArrowDown,
  ArrowUp,
  ArrowDown,
  Home,
  End,
  ShiftHome,
  ShiftEnd,
  SelectAll,
  Undo,
  Redo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragDropKind {
  /// Dragging a selected text range from a focused `<input>` / `<textarea>`.
  TextSelection,
  /// Dragging an active document selection (outside form controls).
  DocumentSelection,
}

#[derive(Debug, Clone)]
pub struct InteractionEngine {
  state: InteractionState,
  /// Cached hover tooltip text derived from HTML `title` attributes (if any).
  ///
  /// This is maintained by pointer-move handlers so UI layers can display tooltips without
  /// repeatedly walking the DOM to resolve `title` attributes on each high-frequency pointer move.
  hover_tooltip: Option<String>,
  pointer_down_target: Option<usize>,
  link_drag: Option<LinkDragState>,
  range_drag: Option<RangeDragState>,
  number_spin: Option<NumberSpinState>,
  text_drag: Option<TextDragState>,
  text_drag_drop: Option<TextDragDropState>,
  document_drag: Option<DocumentDragState>,
  document_selection_drag_drop: Option<DocumentSelectionDragDropState>,
  pending_text_drop_move: Option<PendingTextDropMove>,
  text_edit: Option<TextEditState>,
  text_undo: HashMap<usize, TextUndoHistory>,
  form_default_snapshots: HashMap<usize, FormDefaultSnapshot>,
  /// Per-`<select>` anchor option for Shift range-selection in listbox controls (`multiple` or
  /// `size > 1`).
  ///
  /// Native browsers treat Shift-click as selecting a contiguous option range from a stable anchor.
  /// The anchor updates on non-shift interactions (plain click, Ctrl/Cmd click, arrow-key
  /// navigation), and is then used as the start of future range selections.
  select_listbox_anchor: HashMap<usize, usize>,
  modality: InputModality,
  last_click_target: Option<usize>,
  last_click_target_element_id: Option<String>,
  last_form_submitter: Option<usize>,
  last_form_submitter_element_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct FormDefaultSnapshot {
  /// Default (initial) `value` content attribute for `<input>` elements, keyed by input node id.
  ///
  /// When `None`, the `value` attribute was absent in the original document.
  input_value: HashMap<usize, Option<String>>,
  /// Default checkedness for checkbox/radio inputs, keyed by input node id.
  input_checked: HashMap<usize, bool>,
  /// Default `selected` content attribute presence for `<option>` elements in `<select>` controls
  /// whose form owner matches the snapshot's form.
  option_selected: HashMap<usize, bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RangeDragState {
  node_id: usize,
  box_id: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LinkDragState {
  node_id: usize,
  down_point: Point,
  active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionDragGranularity {
  Char,
  Word,
  LineOrBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DocumentDragState {
  down_point: DocumentSelectionPoint,
  initial_range: Option<DocumentSelectionRange>,
  granularity: SelectionDragGranularity,
}

#[derive(Debug, Clone)]
struct DocumentSelectionDragDropState {
  down_page_point: Point,
  payload: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingTextDropMove {
  /// The pre-order DOM id of the `<input>`/`<textarea>` whose selection should be moved.
  node_id: usize,
  /// The original selected range (start, end) in character indices.
  selection: (usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumberSpinDirection {
  Up,
  Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NumberSpinState {
  node_id: usize,
  box_id: usize,
  direction: NumberSpinDirection,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct TextEditState {
  /// The pre-order DOM id of the focused `<input>`/`<textarea>`.
  node_id: usize,
  /// Insertion point in character indices (not bytes).
  caret: usize,
  /// Visual affinity for the caret when the logical boundary maps to multiple x positions.
  caret_affinity: CaretAffinity,
  /// Anchor for selection extension. When present and differs from `caret`, the control has an
  /// active selection.
  selection_anchor: Option<usize>,
  /// Preferred x position (CSS px, relative to the textarea's text rect) for vertical caret
  /// movement in `<textarea>`.
  preferred_x: Option<f32>,
}

impl TextEditState {
  fn selection(&self) -> Option<(usize, usize)> {
    let anchor = self.selection_anchor?;
    if anchor == self.caret {
      return None;
    }
    Some(if anchor < self.caret {
      (anchor, self.caret)
    } else {
      (self.caret, anchor)
    })
  }

  fn clear_selection(&mut self) {
    self.selection_anchor = None;
  }

  fn set_caret(&mut self, caret: usize) {
    self.caret = caret;
    self.caret_affinity = CaretAffinity::Downstream;
    self.preferred_x = None;
  }

  fn set_caret_with_affinity(&mut self, caret: usize, affinity: CaretAffinity) {
    self.caret = caret;
    self.caret_affinity = affinity;
    self.preferred_x = None;
  }

  fn set_caret_with_affinity_and_maybe_extend_selection(
    &mut self,
    caret: usize,
    affinity: CaretAffinity,
    extend_selection: bool,
  ) {
    if extend_selection {
      if self.selection_anchor.is_none() {
        self.selection_anchor = Some(self.caret);
      }
      self.caret = caret;
      self.caret_affinity = affinity;
    } else {
      self.selection_anchor = None;
      self.caret = caret;
      self.caret_affinity = affinity;
    }
    self.preferred_x = None;
  }

  fn set_caret_and_maybe_extend_selection(&mut self, caret: usize, extend_selection: bool) {
    self.set_caret_with_affinity_and_maybe_extend_selection(
      caret,
      CaretAffinity::Downstream,
      extend_selection,
    );
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TextUndoEntry {
  value: String,
  caret: usize,
  caret_affinity: CaretAffinity,
  selection_anchor: Option<usize>,
}

#[derive(Debug, Clone, Default)]
struct TextUndoHistory {
  undo: Vec<TextUndoEntry>,
  redo: Vec<TextUndoEntry>,
  undo_bytes: usize,
  redo_bytes: usize,
}

impl TextUndoHistory {
  const MAX_ENTRIES: usize = 128;
  const MAX_BYTES: usize = 1_048_576;

  fn clear_redo(&mut self) {
    self.redo.clear();
    self.redo_bytes = 0;
  }

  fn push_undo(&mut self, entry: TextUndoEntry) {
    self.undo_bytes = self.undo_bytes.saturating_add(entry.value.len());
    self.undo.push(entry);
    self.truncate_undo();
  }

  fn push_redo(&mut self, entry: TextUndoEntry) {
    self.redo_bytes = self.redo_bytes.saturating_add(entry.value.len());
    self.redo.push(entry);
    self.truncate_redo();
  }

  fn pop_undo(&mut self) -> Option<TextUndoEntry> {
    let entry = self.undo.pop()?;
    self.undo_bytes = self.undo_bytes.saturating_sub(entry.value.len());
    Some(entry)
  }

  fn pop_redo(&mut self) -> Option<TextUndoEntry> {
    let entry = self.redo.pop()?;
    self.redo_bytes = self.redo_bytes.saturating_sub(entry.value.len());
    Some(entry)
  }

  fn truncate_undo(&mut self) {
    while self.undo.len() > Self::MAX_ENTRIES || self.undo_bytes > Self::MAX_BYTES {
      if let Some(removed) = self.undo.first() {
        self.undo_bytes = self.undo_bytes.saturating_sub(removed.value.len());
      }
      self.undo.remove(0);
    }
  }

  fn truncate_redo(&mut self) {
    while self.redo.len() > Self::MAX_ENTRIES || self.redo_bytes > Self::MAX_BYTES {
      if let Some(removed) = self.redo.first() {
        self.redo_bytes = self.redo_bytes.saturating_sub(removed.value.len());
      }
      self.redo.remove(0);
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextDragState {
  node_id: usize,
  box_id: usize,
  anchor: usize,
  down_caret: usize,
  initial_range: Option<(usize, usize)>,
  granularity: SelectionDragGranularity,
  focus_before: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
struct TextDragDropCandidate {
  node_id: usize,
  box_id: usize,
  down_point: Point,
  down_caret: usize,
  down_caret_affinity: CaretAffinity,
  selection: (usize, usize),
  text: String,
  focus_before: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
struct TextDragDropActive {
  node_id: usize,
  box_id: usize,
  down_point: Point,
  down_caret: usize,
  down_caret_affinity: CaretAffinity,
  selection: (usize, usize),
  text: String,
  focus_before: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
enum TextDragDropState {
  Candidate(TextDragDropCandidate),
  Active(TextDragDropActive),
}

impl TextDragDropState {
  fn node_id(&self) -> usize {
    match self {
      TextDragDropState::Candidate(state) => state.node_id,
      TextDragDropState::Active(state) => state.node_id,
    }
  }

  fn focus_before(&self) -> Option<usize> {
    match self {
      TextDragDropState::Candidate(state) => state.focus_before,
      TextDragDropState::Active(state) => state.focus_before,
    }
  }
}

struct DomIndexMut {
  id_to_node: Vec<*mut DomNode>,
  parent: Vec<usize>,
}

impl DomIndexMut {
  fn new(root: &mut DomNode) -> Self {
    // Node ids are pre-order traversal indices, matching `crate::dom::enumerate_dom_ids`.
    let mut id_to_node: Vec<*mut DomNode> = vec![std::ptr::null_mut()];
    let mut parent: Vec<usize> = vec![0];

    // (node_ptr, parent_id)
    let mut stack: Vec<(*mut DomNode, usize)> = vec![(root as *mut DomNode, 0)];
    let mut next_id = 1usize;

    while let Some((node_ptr, parent_id)) = stack.pop() {
      let id = next_id;
      next_id = next_id.saturating_add(1);

      id_to_node.push(node_ptr);
      parent.push(parent_id);

      // SAFETY: We never mutate `children` while this traversal runs, so raw pointers remain valid.
      let node = unsafe { &mut *node_ptr };
      for child in node.children.iter_mut().rev() {
        stack.push((child as *mut DomNode, id));
      }
    }

    Self { id_to_node, parent }
  }

  fn node(&self, node_id: usize) -> Option<&DomNode> {
    let ptr = self.id_to_node.get(node_id).copied()?;
    if ptr.is_null() {
      return None;
    }
    // SAFETY: Pointers are built from a live `DomNode` tree.
    Some(unsafe { &*ptr })
  }

  fn node_mut(&mut self, node_id: usize) -> Option<&mut DomNode> {
    let ptr = self.id_to_node.get(node_id).copied()?;
    if ptr.is_null() {
      return None;
    }
    // SAFETY: We only produce a temporary mutable reference for the current call site.
    Some(unsafe { &mut *ptr })
  }
}

impl super::effective_disabled::DomIdLookup for DomIndexMut {
  fn len(&self) -> usize {
    self.id_to_node.len().saturating_sub(1)
  }

  fn node(&self, node_id: usize) -> Option<&DomNode> {
    self.node(node_id)
  }

  fn parent_id(&self, node_id: usize) -> usize {
    self.parent.get(node_id).copied().unwrap_or(0)
  }
}

fn element_id_for_node(index: &DomIndexMut, node_id: usize) -> Option<String> {
  index
    .node(node_id)
    .filter(|node| node.is_element())
    .and_then(|node| node.get_attribute_ref("id"))
    .filter(|id| !id.is_empty())
    .map(|id| id.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn find_element_node_id(dom: &mut DomNode, tag: &str) -> usize {
    let index = DomIndexMut::new(dom);
    for node_id in 1..index.id_to_node.len() {
      if index
        .node(node_id)
        .and_then(|node| node.tag_name())
        .is_some_and(|name| name.eq_ignore_ascii_case(tag))
      {
        return node_id;
      }
    }
    panic!("missing element {tag}");
  }

  fn input_value(dom: &mut DomNode, node_id: usize) -> String {
    let index = DomIndexMut::new(dom);
    index
      .node(node_id)
      .and_then(|node| node.get_attribute_ref("value"))
      .unwrap_or("")
      .to_string()
  }

  fn textarea_value(dom: &mut DomNode, node_id: usize) -> String {
    let index = DomIndexMut::new(dom);
    let node = index.node(node_id).expect("textarea node");
    crate::dom::textarea_current_value(node)
  }

  fn attr_count(dom: &mut DomNode, node_id: usize) -> usize {
    let index = DomIndexMut::new(dom);
    let node = index.node(node_id).expect("node");
    match &node.node_type {
      DomNodeType::Element { attributes, .. } | DomNodeType::Slot { attributes, .. } => {
        // Ignore the internal textarea value snapshot. The DOM layer models user-edited textarea
        // values via `data-fastr-value`, so text-editing operations can legitimately add/update
        // that attribute even when they avoid mutating author-provided attributes.
        attributes
          .iter()
          .filter(|(name, _)| name != "data-fastr-value")
          .count()
      }
      _ => 0,
    }
  }

  fn set_text_selection_caret(
    engine: &mut InteractionEngine,
    _dom: &mut DomNode,
    node_id: usize,
    caret: usize,
  ) {
    engine.text_edit = Some(TextEditState {
      node_id,
      caret,
      caret_affinity: CaretAffinity::Downstream,
      selection_anchor: None,
      preferred_x: None,
    });
  }

  fn set_text_selection_range(
    engine: &mut InteractionEngine,
    _dom: &mut DomNode,
    node_id: usize,
    start: usize,
    end: usize,
  ) {
    engine.text_edit = Some(TextEditState {
      node_id,
      caret: end,
      caret_affinity: CaretAffinity::Downstream,
      selection_anchor: Some(start),
      preferred_x: None,
    });
  }

  #[test]
  fn interaction_hash_paint_only_mutation_changes_paint_not_css() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"abc\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    let css_before = engine.interaction_state().interaction_css_hash();
    let paint_before = engine.interaction_state().interaction_paint_hash();

    // Paint-only: caret/selection update inside the already-focused control.
    engine.set_text_selection_caret(input_id, 1);

    let css_after = engine.interaction_state().interaction_css_hash();
    let paint_after = engine.interaction_state().interaction_paint_hash();

    assert_eq!(css_before, css_after, "paint-only mutations must not affect css hash");
    assert_ne!(paint_before, paint_after, "paint-only mutations must affect paint hash");
  }

  #[test]
  fn interaction_hash_repeated_reads_are_stable_without_mutation() {
    let state = InteractionState::default();

    let css_a = state.interaction_css_hash();
    let css_b = state.interaction_css_hash();
    assert_eq!(css_a, css_b);

    let paint_a = state.interaction_paint_hash();
    let paint_b = state.interaction_paint_hash();
    assert_eq!(paint_a, paint_b);
  }

  #[test]
  fn interaction_hash_clone_is_dirty_by_default() {
    let state = InteractionState::default();
    // Force the original state to compute and cache hashes (clearing dirty flags).
    let css_before = state.interaction_css_hash();
    let paint_before = state.interaction_paint_hash();

    // Clone + mutate a paint-only field directly (simulating embedder/test code that doesn't go
    // through `InteractionEngine` APIs).
    let mut cloned = state.clone();
    cloned.document_selection = Some(DocumentSelectionState::All);

    let css_after = cloned.interaction_css_hash();
    let paint_after = cloned.interaction_paint_hash();

    assert_eq!(css_before, css_after, "paint-only mutations must not affect css hash");
    assert_ne!(paint_before, paint_after, "paint-only mutations must affect paint hash");
  }

  #[test]
  fn interaction_hash_set_visited_links_changes_css_not_paint() {
    let mut engine = InteractionEngine::new();

    let css_before = engine.interaction_state().interaction_css_hash();
    let paint_before = engine.interaction_state().interaction_paint_hash();

    let mut visited = rustc_hash::FxHashSet::default();
    visited.insert(42);
    engine.set_visited_links(visited);

    let css_after = engine.interaction_state().interaction_css_hash();
    let paint_after = engine.interaction_state().interaction_paint_hash();

    assert_ne!(css_before, css_after, "visited links must affect css hash");
    assert_eq!(paint_before, paint_after, "visited links must not affect paint hash");
  }

  #[test]
  fn interaction_hash_form_state_value_changes_paint_not_css() {
    let mut state = InteractionState::default();
    let css_before = state.interaction_css_hash();
    let paint_before = state.interaction_paint_hash();

    state.form_state_mut().values.insert(1, "hello".to_string());

    let css_after = state.interaction_css_hash();
    let paint_after = state.interaction_paint_hash();

    assert_eq!(css_before, css_after, "form-state overrides must not affect css hash");
    assert_ne!(paint_before, paint_after, "form-state overrides must affect paint hash");
  }

  #[test]
  fn style_for_styled_node_id_ignores_pseudo_boxes() {
    let styled_node_id = 42;

    let mut pseudo_style = ComputedStyle::default();
    pseudo_style.direction = crate::style::types::Direction::Rtl;

    let mut real_style = ComputedStyle::default();
    real_style.direction = crate::style::types::Direction::Ltr;

    let mut pseudo_box = BoxNode::new_block(
      Arc::new(pseudo_style),
      crate::style::display::FormattingContextType::Block,
      vec![],
    );
    pseudo_box.styled_node_id = Some(styled_node_id);
    pseudo_box.generated_pseudo = Some(crate::tree::box_tree::GeneratedPseudoElement::Before);

    let mut real_box = BoxNode::new_block(
      Arc::new(real_style),
      crate::style::display::FormattingContextType::Block,
      vec![],
    );
    real_box.styled_node_id = Some(styled_node_id);

    let root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      crate::style::display::FormattingContextType::Block,
      vec![pseudo_box, real_box],
    );
    let box_tree = BoxTree::new(root);

    let style =
      style_for_styled_node_id(&box_tree, styled_node_id).expect("expected styled node style");
    assert_eq!(style.direction, crate::style::types::Direction::Ltr);
  }

  #[test]
  fn document_selection_contains_point_ignores_collapsed_ranges() {
    let highlight = DocumentSelectionRange {
      start: DocumentSelectionPoint {
        node_id: 1,
        char_offset: 0,
      },
      end: DocumentSelectionPoint {
        node_id: 1,
        char_offset: 5,
      },
    };
    let caret = DocumentSelectionPoint {
      node_id: 2,
      char_offset: 3,
    };
    let collapsed = DocumentSelectionRange {
      start: caret,
      end: caret,
    };

    let mut ranges = DocumentSelectionRanges {
      ranges: vec![highlight, collapsed],
      primary: 0,
      anchor: highlight.start,
      focus: highlight.end,
    };
    ranges.normalize();
    let selection = DocumentSelectionState::Ranges(ranges);

    assert!(document_selection_contains_point(
      &selection,
      DocumentSelectionPoint {
        node_id: 1,
        char_offset: 2,
      }
    ));
    assert!(!document_selection_contains_point(&selection, caret));
  }

  #[test]
  fn document_word_selection_range_spans_across_adjacent_text_boxes() {
    use crate::style::display::{Display, FormattingContextType};

    let mut block_style = ComputedStyle::default();
    block_style.display = Display::Block;
    let block_style = Arc::new(block_style);

    let text_style = Arc::new(ComputedStyle::default());

    let mut first = BoxNode::new_text(text_style.clone(), "he".to_string());
    first.styled_node_id = Some(1);
    let mut second = BoxNode::new_text(text_style, "llo".to_string());
    second.styled_node_id = Some(2);

    let root = BoxNode::new_block(block_style, FormattingContextType::Block, vec![first, second]);
    let tree = BoxTree::new(root);

    let first_box_id = tree.root.children[0].id;
    let second_box_id = tree.root.children[1].id;

    let expected = DocumentSelectionRange {
      start: DocumentSelectionPoint {
        node_id: 1,
        char_offset: 0,
      },
      end: DocumentSelectionPoint {
        node_id: 2,
        char_offset: 3,
      },
    };

    let from_second = document_word_selection_range(
      &tree,
      second_box_id,
      DocumentSelectionPoint {
        node_id: 2,
        char_offset: 1,
      },
    )
    .expect("range from second text box");
    assert_eq!(from_second, expected);

    let from_first = document_word_selection_range(
      &tree,
      first_box_id,
      DocumentSelectionPoint {
        node_id: 1,
        char_offset: 1,
      },
    )
    .expect("range from first text box");
    assert_eq!(from_first, expected);
  }

  #[test]
  fn document_word_selection_range_does_not_span_across_line_break_boxes() {
    use crate::style::display::{Display, FormattingContextType};

    let mut block_style = ComputedStyle::default();
    block_style.display = Display::Block;
    let block_style = Arc::new(block_style);

    let text_style = Arc::new(ComputedStyle::default());

    let mut first = BoxNode::new_text(text_style.clone(), "he".to_string());
    first.styled_node_id = Some(1);
    let br = BoxNode::new_line_break(text_style.clone());
    let mut second = BoxNode::new_text(text_style, "llo".to_string());
    second.styled_node_id = Some(2);

    let root =
      BoxNode::new_block(block_style, FormattingContextType::Block, vec![first, br, second]);
    let tree = BoxTree::new(root);

    let first_box_id = tree.root.children[0].id;
    let second_box_id = tree.root.children[2].id;

    let from_second = document_word_selection_range(
      &tree,
      second_box_id,
      DocumentSelectionPoint {
        node_id: 2,
        char_offset: 1,
      },
    )
    .expect("range from second text box");
    assert_eq!(
      from_second,
      DocumentSelectionRange {
        start: DocumentSelectionPoint {
          node_id: 2,
          char_offset: 0,
        },
        end: DocumentSelectionPoint {
          node_id: 2,
          char_offset: 3,
        },
      }
    );

    let from_first = document_word_selection_range(
      &tree,
      first_box_id,
      DocumentSelectionPoint {
        node_id: 1,
        char_offset: 1,
      },
    )
    .expect("range from first text box");
    assert_eq!(
      from_first,
      DocumentSelectionRange {
        start: DocumentSelectionPoint {
          node_id: 1,
          char_offset: 0,
        },
        end: DocumentSelectionPoint {
          node_id: 1,
          char_offset: 2,
        },
      }
    );
  }

  #[test]
  fn document_word_selection_range_does_not_span_across_replaced_boxes() {
    use crate::style::display::{Display, FormattingContextType};

    let mut block_style = ComputedStyle::default();
    block_style.display = Display::Block;
    let block_style = Arc::new(block_style);

    let text_style = Arc::new(ComputedStyle::default());

    let mut first = BoxNode::new_text(text_style.clone(), "he".to_string());
    first.styled_node_id = Some(1);
    let replaced = BoxNode::new_replaced(text_style.clone(), ReplacedType::Canvas, None, None);
    let mut second = BoxNode::new_text(text_style, "llo".to_string());
    second.styled_node_id = Some(2);

    let root =
      BoxNode::new_block(block_style, FormattingContextType::Block, vec![first, replaced, second]);
    let tree = BoxTree::new(root);

    let second_box_id = tree.root.children[2].id;
    let range = document_word_selection_range(
      &tree,
      second_box_id,
      DocumentSelectionPoint {
        node_id: 2,
        char_offset: 1,
      },
    )
    .expect("range from second text box");
    assert_eq!(
      range,
      DocumentSelectionRange {
        start: DocumentSelectionPoint {
          node_id: 2,
          char_offset: 0,
        },
        end: DocumentSelectionPoint {
          node_id: 2,
          char_offset: 3,
        },
      }
    );
  }

  #[test]
  fn style_for_styled_node_id_falls_back_to_pseudo_style() {
    let styled_node_id = 42;

    let mut pseudo_style = ComputedStyle::default();
    pseudo_style.direction = crate::style::types::Direction::Rtl;

    let mut pseudo_box = BoxNode::new_block(
      Arc::new(pseudo_style),
      crate::style::display::FormattingContextType::Block,
      vec![],
    );
    pseudo_box.styled_node_id = Some(styled_node_id);
    pseudo_box.generated_pseudo = Some(crate::tree::box_tree::GeneratedPseudoElement::Before);

    let root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      crate::style::display::FormattingContextType::Block,
      vec![pseudo_box],
    );
    let box_tree = BoxTree::new(root);

    let style =
      style_for_styled_node_id(&box_tree, styled_node_id).expect("expected styled node style");
    assert_eq!(style.direction, crate::style::types::Direction::Rtl);
  }

  #[test]
  fn ime_preedit_sets_composition_without_mutating_value() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"a\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);
    let attr_count_before = attr_count(&mut dom, input_id);

    engine.ime_preedit(&mut dom, "あ", Some((0, 1)));

    let comp = engine.state.ime_preedit.as_ref().expect("preedit state");
    assert_eq!(comp.node_id, input_id);
    assert_eq!(comp.text, "あ");
    assert_eq!(comp.cursor, Some((0, 1)));

    assert_eq!(input_value(&mut dom, input_id), "a");
    assert_eq!(
      attr_count(&mut dom, input_id),
      attr_count_before,
      "IME preedit must not mutate element attributes"
    );
  }

  #[test]
  fn ime_commit_inserts_text_and_clears_preedit() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"a\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);
    let attr_count_before = attr_count(&mut dom, input_id);

    engine.ime_preedit(&mut dom, "あ", Some((0, 1)));

    engine.ime_commit(&mut dom, "あ");

    assert!(engine.state.ime_preedit.is_none());
    assert_eq!(attr_count(&mut dom, input_id), attr_count_before);
    assert_eq!(input_value(&mut dom, input_id), "aあ");
  }

  #[test]
  fn ime_cancel_clears_preedit_without_mutating_value() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"a\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);
    let attr_count_before = attr_count(&mut dom, input_id);

    engine.ime_preedit(&mut dom, "あ", Some((0, 1)));

    engine.ime_cancel(&mut dom);

    assert!(engine.state.ime_preedit.is_none());
    assert_eq!(attr_count(&mut dom, input_id), attr_count_before);
    assert_eq!(input_value(&mut dom, input_id), "a");
  }

  #[test]
  fn ime_commit_updates_textarea_value() {
    let mut dom =
      crate::dom::parse_html("<html><body><textarea>hi</textarea></body></html>").expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);
    let attr_count_before = attr_count(&mut dom, textarea_id);

    engine.ime_preedit(&mut dom, "あ", None);

    engine.ime_commit(&mut dom, "あ");

    assert!(engine.state.ime_preedit.is_none());
    assert_eq!(attr_count(&mut dom, textarea_id), attr_count_before);
    assert_eq!(textarea_value(&mut dom, textarea_id), "hiあ");
  }

  #[test]
  fn clipboard_paste_cancels_ime_preedit_for_input() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"hello\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.state.ime_preedit.is_some());

    assert!(engine.clipboard_paste(&mut dom, "X"));
    assert!(engine.state.ime_preedit.is_none());
    assert_eq!(input_value(&mut dom, input_id), "helloX");
  }

  #[test]
  fn clipboard_cut_cancels_ime_preedit_for_input() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"hello\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);
    engine.clipboard_select_all(&mut dom);

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.state.ime_preedit.is_some());

    let (changed, text) = engine.clipboard_cut(&mut dom);
    assert!(changed);
    assert_eq!(text.as_deref(), Some("hello"));
    assert!(engine.state.ime_preedit.is_none());
    assert_eq!(input_value(&mut dom, input_id), "");
  }

  #[test]
  fn clipboard_paste_cancels_ime_preedit_for_textarea() {
    let mut dom = crate::dom::parse_html("<html><body><textarea>hello</textarea></body></html>")
      .expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.state.ime_preedit.is_some());

    assert!(engine.clipboard_paste(&mut dom, "X"));
    assert!(engine.state.ime_preedit.is_none());
    assert_eq!(textarea_value(&mut dom, textarea_id), "helloX");
  }

  #[test]
  fn clipboard_cut_cancels_ime_preedit_for_textarea() {
    let mut dom = crate::dom::parse_html("<html><body><textarea>hello</textarea></body></html>")
      .expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);
    engine.clipboard_select_all(&mut dom);

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.state.ime_preedit.is_some());

    let (changed, text) = engine.clipboard_cut(&mut dom);
    assert!(changed);
    assert_eq!(text.as_deref(), Some("hello"));
    assert!(engine.state.ime_preedit.is_none());
    assert_eq!(textarea_value(&mut dom, textarea_id), "");
  }

  #[test]
  fn clipboard_select_all_cancels_ime_preedit_for_input() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"hello\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");
    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.state.ime_preedit.is_some());

    assert!(engine.clipboard_select_all(&mut dom));
    assert!(engine.state.ime_preedit.is_none());
  }

  #[test]
  fn arrow_keys_cancel_ime_preedit_for_focused_input() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"abcd\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");
    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    // Place caret between "b" and "c".
    set_text_selection_caret(&mut engine, &mut dom, input_id, 2);

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.state.ime_preedit.is_some());
    assert!(engine.key_action(&mut dom, KeyAction::ArrowLeft));
    assert!(engine.state.ime_preedit.is_none());

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.state.ime_preedit.is_some());
    assert!(engine.key_action(&mut dom, KeyAction::ArrowRight));
    assert!(engine.state.ime_preedit.is_none());
  }

  #[test]
  fn pointer_caret_placement_cancels_ime_preedit_for_focused_input() {
    use crate::style::display::FormattingContextType;
    use crate::tree::fragment_tree::FragmentNode;

    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"hello\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut input_box = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    input_box.styled_node_id = Some(input_id);
    let box_tree = BoxTree::new(BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![input_box],
    ));

    let mut input_box_id = None;
    let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
    while let Some(node) = stack.pop() {
      if node.styled_node_id == Some(input_id) {
        input_box_id = Some(node.id);
        break;
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    let input_box_id = input_box_id.expect("input box id");

    let fragment_tree = FragmentTree::new(FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
      vec![FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
        input_box_id,
        vec![],
      )],
    ));

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.state.ime_preedit.is_some());

    assert!(engine.pointer_down_with_click_count(
      &mut dom,
      &box_tree,
      &fragment_tree,
      &ScrollState::default(),
      Point::new(10.0, 10.0),
      PointerButton::Primary,
      PointerModifiers::NONE,
      1,
    ));
    assert!(engine.state.ime_preedit.is_none());
  }

  #[test]
  fn pointer_selection_collapse_cancels_ime_preedit_for_focused_input() {
    use crate::style::display::FormattingContextType;
    use crate::tree::fragment_tree::FragmentNode;

    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"hello\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut input_box = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    input_box.styled_node_id = Some(input_id);
    let box_tree = BoxTree::new(BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![input_box],
    ));

    let mut input_box_id = None;
    let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
    while let Some(node) = stack.pop() {
      if node.styled_node_id == Some(input_id) {
        input_box_id = Some(node.id);
        break;
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    let input_box_id = input_box_id.expect("input box id");

    let fragment_tree = FragmentTree::new(FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
      vec![FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
        input_box_id,
        vec![],
      )],
    ));

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    // Create an active selection. Clicking inside the highlight starts a drag-drop candidate, which
    // defers selection collapse until mouseup.
    set_text_selection_range(&mut engine, &mut dom, input_id, 0, "hello".chars().count());

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.state.ime_preedit.is_some());

    assert!(engine.pointer_down_with_click_count(
      &mut dom,
      &box_tree,
      &fragment_tree,
      &ScrollState::default(),
      Point::new(10.0, 10.0),
      PointerButton::Primary,
      PointerModifiers::NONE,
      1,
    ));
    assert!(engine.state.ime_preedit.is_some(), "preedit should survive drag-drop candidate down");

    let (_changed, _action) = engine.pointer_up(
      &mut dom,
      &box_tree,
      &fragment_tree,
      Point::new(10.0, 10.0),
      PointerButton::Primary,
      PointerModifiers::NONE,
      /* allow_default_drop */ false,
      "https://example.com/index.html",
      "https://example.com/",
    );
    assert!(engine.state.ime_preedit.is_none());
  }

  #[test]
  fn delete_removes_next_character_in_focused_input() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"aあb\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    // Place the caret between "a" and "あ".
    set_text_selection_caret(&mut engine, &mut dom, input_id, "a".len());

    let changed = engine.key_action(&mut dom, KeyAction::Delete);
    assert!(changed);
    assert_eq!(input_value(&mut dom, input_id), "ab");
  }

  #[test]
  fn delete_removes_selection_in_focused_textarea() {
    let mut dom = crate::dom::parse_html("<html><body><textarea>hello</textarea></body></html>")
      .expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);

    // Delete "ell".
    set_text_selection_range(&mut engine, &mut dom, textarea_id, 1, 4);

    let changed = engine.key_action(&mut dom, KeyAction::Delete);
    assert!(changed);
    assert_eq!(textarea_value(&mut dom, textarea_id), "ho");
  }

  #[test]
  fn delete_is_noop_at_end_of_input() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"abc\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    // Default caret is at the end of the text; `Delete` should be a no-op.
    let changed = engine.key_action(&mut dom, KeyAction::Delete);
    assert!(!changed);
    assert_eq!(input_value(&mut dom, input_id), "abc");
  }

  #[test]
  fn arrow_keys_move_caret_and_shift_extends_selection() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"abcd\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    // Place the caret between "b" and "c".
    set_text_selection_caret(&mut engine, &mut dom, input_id, 2);

    assert!(engine.key_action(&mut dom, KeyAction::ArrowLeft));
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, 1);
    assert_eq!(engine.text_edit.as_ref().unwrap().selection(), None);

    assert!(engine.key_action(&mut dom, KeyAction::ArrowRight));
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, 2);
    assert_eq!(engine.text_edit.as_ref().unwrap().selection(), None);

    assert!(engine.key_action(&mut dom, KeyAction::ShiftArrowRight));
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.caret, 3);
    assert_eq!(edit.selection(), Some((2, 3)));
  }

  #[test]
  fn rtl_arrow_keys_move_caret_visually() {
    let mut dom =
      crate::dom::parse_html("<html><body><input dir=\"rtl\" value=\"אבג\"></body></html>")
        .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    let len = "אבג".chars().count();

    // RTL: ArrowLeft moves visually left, which increments the logical caret index.
    set_text_selection_caret(&mut engine, &mut dom, input_id, 0);
    assert!(engine.key_action(&mut dom, KeyAction::ArrowLeft));
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, 1);
    assert!(engine.key_action(&mut dom, KeyAction::ArrowRight));
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, 0);

    // RTL: ArrowRight moves visually right, which decrements the logical caret index.
    set_text_selection_caret(&mut engine, &mut dom, input_id, len);
    assert!(engine.key_action(&mut dom, KeyAction::ArrowRight));
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, len - 1);
    assert!(engine.key_action(&mut dom, KeyAction::ArrowLeft));
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, len);
  }

  #[test]
  fn rtl_shift_arrow_extends_selection_visually() {
    let mut dom =
      crate::dom::parse_html("<html><body><input dir=\"rtl\" value=\"אבג\"></body></html>")
        .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    set_text_selection_caret(&mut engine, &mut dom, input_id, 0);
    assert!(engine.key_action(&mut dom, KeyAction::ShiftArrowLeft));
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.caret, 1);
    assert_eq!(edit.selection(), Some((0, 1)));
  }

  #[test]
  fn bidi_mixed_direction_arrow_keys_follow_visual_order() {
    let mut dom =
      crate::dom::parse_html("<html><body><input dir=\"ltr\" value=\"ABC אבג\"></body></html>")
        .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    // In "ABC אבג" (LTR paragraph with an RTL run), the caret's visual order differs from the
    // logical character index order. In addition, the LTR/RTL run boundary at char_idx=4 has a
    // "split caret": two distinct visual caret stops for the same logical boundary.
    //
    // Visual ArrowRight traversal should step through all visual caret stops:
    //   (0,D) → (1,D) → (2,D) → (3,D) → (4,U) → (7,U) → (6,D) → (5,D) → (4,D)
    // where D=Downstream, U=Upstream.
    let expected = [
      (0usize, CaretAffinity::Downstream),
      (1, CaretAffinity::Downstream),
      (2, CaretAffinity::Downstream),
      (3, CaretAffinity::Downstream),
      (4, CaretAffinity::Upstream),
      (7, CaretAffinity::Upstream),
      (6, CaretAffinity::Downstream),
      (5, CaretAffinity::Downstream),
      (4, CaretAffinity::Downstream),
    ];
    set_text_selection_caret(&mut engine, &mut dom, input_id, expected[0].0);
    for (idx, &(caret, affinity)) in expected.iter().enumerate() {
      let edit = engine.text_edit.as_ref().unwrap();
      assert_eq!(edit.caret, caret, "at step {idx}");
      assert_eq!(edit.caret_affinity, affinity, "at step {idx}");
      if idx + 1 < expected.len() {
        assert!(
          engine.key_action(&mut dom, KeyAction::ArrowRight),
          "ArrowRight should move at step {idx}"
        );
      }
    }
  }

  #[test]
  fn style_for_styled_node_id_prefers_non_pseudo_boxes() {
    use crate::style::display::FormattingContextType;
    use crate::style::types::Direction;
    use crate::tree::box_tree::GeneratedPseudoElement;

    let mut pseudo_style = ComputedStyle::default();
    pseudo_style.direction = Direction::Ltr;
    let pseudo_style = Arc::new(pseudo_style);

    let mut element_style = ComputedStyle::default();
    element_style.direction = Direction::Rtl;
    let element_style = Arc::new(element_style);

    let mut pseudo_box = BoxNode::new_block(pseudo_style, FormattingContextType::Block, vec![]);
    pseudo_box.styled_node_id = Some(2);
    pseudo_box.generated_pseudo = Some(GeneratedPseudoElement::Placeholder);

    let mut element_box = BoxNode::new_block(element_style, FormattingContextType::Block, vec![]);
    element_box.styled_node_id = Some(2);

    let root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![pseudo_box, element_box],
    );
    let tree = BoxTree::new(root);

    let style = style_for_styled_node_id(&tree, 2).expect("style");
    assert_eq!(style.direction, Direction::Rtl);
  }

  #[test]
  fn bidi_selection_collapse_preserves_split_caret_affinity() {
    let mut dom =
      crate::dom::parse_html("<html><body><input dir=\"ltr\" value=\"ABC אבג\"></body></html>")
        .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    // Extend selection from the start to the split-caret boundary at char_idx=4 ("ABC ").
    set_text_selection_caret(&mut engine, &mut dom, input_id, 0);
    for _ in 0..4 {
      assert!(engine.key_action(&mut dom, KeyAction::ShiftArrowRight));
    }

    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.selection(), Some((0, 4)));
    assert_eq!(edit.caret, 4);
    assert_eq!(
      edit.caret_affinity,
      CaretAffinity::Upstream,
      "expected selection end at split caret to land on the upstream (LTR) side"
    );

    // Collapsing the selection to the end (ArrowRight without shift) must keep the caret on the
    // same visual side of the split caret; it must not "teleport" to the RTL run's start edge.
    assert!(engine.key_action(&mut dom, KeyAction::ArrowRight));
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.selection(), None);
    assert_eq!(edit.caret, 4);
    assert_eq!(edit.caret_affinity, CaretAffinity::Upstream);
  }

  #[test]
  fn bidi_delete_preserves_split_caret_affinity() {
    let mut dom =
      crate::dom::parse_html("<html><body><input dir=\"ltr\" value=\"ABC אבג\"></body></html>")
        .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    // Move to the split-caret boundary at char_idx=4 on the upstream (LTR) side.
    set_text_selection_caret(&mut engine, &mut dom, input_id, 0);
    for _ in 0..4 {
      assert!(engine.key_action(&mut dom, KeyAction::ArrowRight));
    }

    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.caret, 4);
    assert_eq!(edit.caret_affinity, CaretAffinity::Upstream);

    // Deleting the following character should not reset the caret affinity (otherwise the caret
    // would jump to the other split-caret stop).
    assert!(engine.key_action(&mut dom, KeyAction::Delete));
    assert_eq!(input_value(&mut dom, input_id), "ABC בג");

    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.caret, 4);
    assert_eq!(edit.caret_affinity, CaretAffinity::Upstream);
  }

  #[test]
  fn bidi_clipboard_cut_preserves_split_caret_affinity() {
    let mut dom =
      crate::dom::parse_html("<html><body><input dir=\"ltr\" value=\"ABC אבג DEF\"></body></html>")
        .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    // Select the first RTL character ("א") while keeping the caret on the upstream (LTR) side of
    // the split caret boundary at char_idx=4.
    engine.text_edit = Some(TextEditState {
      node_id: input_id,
      caret: 4,
      caret_affinity: CaretAffinity::Upstream,
      selection_anchor: Some(5),
      preferred_x: None,
    });

    let (changed, text) = engine.clipboard_cut(&mut dom);
    assert!(changed);
    assert_eq!(text.as_deref(), Some("א"));
    assert_eq!(input_value(&mut dom, input_id), "ABC בג DEF");

    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.selection(), None);
    assert_eq!(edit.caret, 4);
    assert_eq!(edit.caret_affinity, CaretAffinity::Upstream);
  }

  #[test]
  fn bidi_text_input_preserves_split_caret_affinity_after_insert() {
    let mut dom =
      crate::dom::parse_html("<html><body><input dir=\"ltr\" value=\"ABC אבג\"></body></html>")
        .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    // Move to the split-caret boundary at char_idx=4 on the upstream (LTR) side.
    set_text_selection_caret(&mut engine, &mut dom, input_id, 0);
    for _ in 0..4 {
      assert!(engine.key_action(&mut dom, KeyAction::ArrowRight));
    }
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.caret, 4);
    assert_eq!(edit.caret_affinity, CaretAffinity::Upstream);

    assert!(engine.text_input(&mut dom, "X"));
    assert_eq!(input_value(&mut dom, input_id), "ABC Xאבג");

    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.selection(), None);
    assert_eq!(edit.caret, 5);
    assert_eq!(edit.caret_affinity, CaretAffinity::Upstream);
  }

  #[test]
  fn bidi_clipboard_paste_preserves_split_caret_affinity_after_insert() {
    let mut dom =
      crate::dom::parse_html("<html><body><input dir=\"ltr\" value=\"ABC אבג\"></body></html>")
        .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    // Move to the split-caret boundary at char_idx=4 on the upstream (LTR) side.
    set_text_selection_caret(&mut engine, &mut dom, input_id, 0);
    for _ in 0..4 {
      assert!(engine.key_action(&mut dom, KeyAction::ArrowRight));
    }
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.caret, 4);
    assert_eq!(edit.caret_affinity, CaretAffinity::Upstream);

    assert!(engine.clipboard_paste(&mut dom, "X"));
    assert_eq!(input_value(&mut dom, input_id), "ABC Xאבג");

    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.selection(), None);
    assert_eq!(edit.caret, 5);
    assert_eq!(edit.caret_affinity, CaretAffinity::Upstream);
  }

  #[test]
  fn bidi_arrow_down_selection_collapse_preserves_split_caret_affinity() {
    let mut dom =
      crate::dom::parse_html("<html><body><textarea dir=\"ltr\">ABC אבג</textarea></body></html>")
        .expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);

    // Extend selection from the start to the split-caret boundary at char_idx=4 ("ABC ").
    set_text_selection_caret(&mut engine, &mut dom, textarea_id, 0);
    for _ in 0..4 {
      assert!(engine.key_action(&mut dom, KeyAction::ShiftArrowRight));
    }

    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.selection(), Some((0, 4)));
    assert_eq!(edit.caret, 4);
    assert_eq!(
      edit.caret_affinity,
      CaretAffinity::Upstream,
      "expected selection end at split caret to land on the upstream (LTR) side"
    );

    // ArrowDown collapses selection (without moving in a single-line textarea) and should preserve
    // the caret's visual side at the split-caret boundary.
    assert!(engine.key_action(&mut dom, KeyAction::ArrowDown));
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.selection(), None);
    assert_eq!(edit.caret, 4);
    assert_eq!(edit.caret_affinity, CaretAffinity::Upstream);
  }

  #[test]
  fn bidi_arrow_down_selection_collapse_to_end_uses_upstream_affinity() {
    let mut dom =
      crate::dom::parse_html("<html><body><textarea dir=\"ltr\">ABC אבג</textarea></body></html>")
        .expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);

    // Selection ends at the split-caret boundary but the caret currently sits at the start edge.
    // Collapsing to the end should choose the upstream (LTR) side of the boundary.
    set_text_selection_range(&mut engine, &mut dom, textarea_id, 4, 0);
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.selection(), Some((0, 4)));
    assert_eq!(edit.caret, 0);

    assert!(engine.key_action(&mut dom, KeyAction::ArrowDown));
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.selection(), None);
    assert_eq!(edit.caret, 4);
    assert_eq!(edit.caret_affinity, CaretAffinity::Upstream);
  }

  #[test]
  fn home_clears_selection_in_text_controls() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"abc\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    let len = "abc".chars().count();

    // Create a selection anchored at the end by extending the caret leftwards.
    set_text_selection_caret(&mut engine, &mut dom, input_id, len);
    assert!(engine.key_action(&mut dom, KeyAction::ShiftArrowLeft));
    assert!(engine.text_edit.as_ref().unwrap().selection().is_some());

    // Home (without shift) should collapse the selection and clear the selection anchor.
    assert!(engine.key_action(&mut dom, KeyAction::Home));
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.caret, 0);
    assert_eq!(edit.selection(), None);
  }

  #[test]
  fn end_clears_selection_in_text_controls() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"abc\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    let len = "abc".chars().count();

    // Create a selection anchored at the start by extending the caret rightwards.
    set_text_selection_caret(&mut engine, &mut dom, input_id, 0);
    assert!(engine.key_action(&mut dom, KeyAction::ShiftArrowRight));
    assert!(engine.text_edit.as_ref().unwrap().selection().is_some());

    // End (without shift) should collapse the selection and clear the selection anchor.
    assert!(engine.key_action(&mut dom, KeyAction::End));
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.caret, len);
    assert_eq!(edit.selection(), None);
  }

  #[test]
  fn arrow_up_clears_selection_in_textarea_without_moving() {
    let mut dom = crate::dom::parse_html("<html><body><textarea>ab\ncdef</textarea></body></html>")
      .expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);

    // Selection spanning two lines. ArrowUp should collapse to the start of the selection rather
    // than moving the caret based on its current row/column.
    set_text_selection_range(&mut engine, &mut dom, textarea_id, 0, 4);
    assert!(engine.text_edit.as_ref().unwrap().selection().is_some());

    assert!(engine.key_action(&mut dom, KeyAction::ArrowUp));
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.caret, 0);
    assert_eq!(edit.selection(), None);
  }

  #[test]
  fn arrow_down_clears_selection_in_textarea_without_moving() {
    let mut dom = crate::dom::parse_html("<html><body><textarea>ab\ncdef</textarea></body></html>")
      .expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);

    // Selection spanning two lines with the caret at the start edge.
    set_text_selection_range(&mut engine, &mut dom, textarea_id, 4, 0);
    assert!(engine.text_edit.as_ref().unwrap().selection().is_some());

    assert!(engine.key_action(&mut dom, KeyAction::ArrowDown));
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.caret, 4);
    assert_eq!(edit.selection(), None);
  }

  #[test]
  fn arrow_down_clears_selection_at_textarea_end() {
    let value = "ab\ncdef";
    let mut dom = crate::dom::parse_html("<html><body><textarea>ab\ncdef</textarea></body></html>")
      .expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);

    let len = value.chars().count();
    set_text_selection_range(&mut engine, &mut dom, textarea_id, 0, len);
    assert!(engine.text_edit.as_ref().unwrap().selection().is_some());

    // Even when there's no next line to move into, ArrowDown should still collapse the selection.
    assert!(engine.key_action(&mut dom, KeyAction::ArrowDown));
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.caret, len);
    assert_eq!(edit.selection(), None);
  }

  #[test]
  fn backspace_deletes_previous_character_and_updates_caret() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"abc\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    // Place the caret between "b" and "c".
    set_text_selection_caret(&mut engine, &mut dom, input_id, 2);

    assert!(engine.key_action(&mut dom, KeyAction::Backspace));
    assert_eq!(input_value(&mut dom, input_id), "ac");
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, 1);
    assert_eq!(engine.text_edit.as_ref().unwrap().selection(), None);
  }

  #[test]
  fn backspace_deletes_single_grapheme_cluster() {
    let emoji = "👨‍👩‍👧‍👦";
    let mut dom = crate::dom::parse_html(&format!(
      "<html><body><input value=\"{emoji}\"></body></html>"
    ))
    .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    set_text_selection_caret(&mut engine, &mut dom, input_id, emoji.chars().count());
    assert!(engine.key_action(&mut dom, KeyAction::Backspace));
    assert_eq!(input_value(&mut dom, input_id), "");
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, 0);
  }

  #[test]
  fn delete_deletes_single_grapheme_cluster() {
    let emoji = "👨‍👩‍👧‍👦";
    let value = format!("{emoji}a");
    let mut dom = crate::dom::parse_html(&format!(
      "<html><body><input value=\"{value}\"></body></html>"
    ))
    .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    set_text_selection_caret(&mut engine, &mut dom, input_id, 0);
    assert!(engine.key_action(&mut dom, KeyAction::Delete));
    assert_eq!(input_value(&mut dom, input_id), "a");
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, 0);
  }

  #[test]
  fn word_left_right_move_over_words() {
    let value = "hello world";
    let mut dom = crate::dom::parse_html(&format!(
      "<html><body><input value=\"{value}\"></body></html>"
    ))
    .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    let len = value.chars().count();
    set_text_selection_caret(&mut engine, &mut dom, input_id, len);
    assert!(engine.key_action(&mut dom, KeyAction::WordLeft));
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, 6);
    assert!(engine.key_action(&mut dom, KeyAction::WordLeft));
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, 0);

    assert!(engine.key_action(&mut dom, KeyAction::WordRight));
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, 6);
    assert!(engine.key_action(&mut dom, KeyAction::WordRight));
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, len);
  }

  #[test]
  fn word_backspace_delete_remove_words_and_respect_unicode() {
    let value = "hello 世界";
    let mut dom = crate::dom::parse_html(&format!(
      "<html><body><input value=\"{value}\"></body></html>"
    ))
    .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    let len = value.chars().count();
    set_text_selection_caret(&mut engine, &mut dom, input_id, len);
    assert!(engine.key_action(&mut dom, KeyAction::WordBackspace));
    assert_eq!(input_value(&mut dom, input_id), "hello ");

    set_text_selection_caret(&mut engine, &mut dom, input_id, 0);
    assert!(engine.key_action(&mut dom, KeyAction::WordDelete));
    assert_eq!(input_value(&mut dom, input_id), " ");
    assert!(engine.key_action(&mut dom, KeyAction::WordDelete));
    assert_eq!(input_value(&mut dom, input_id), "");
  }

  #[test]
  fn word_left_skips_combining_mark_sequence() {
    let value = "a\u{0301}b";
    let mut dom = crate::dom::parse_html(&format!(
      "<html><body><input value=\"{value}\"></body></html>"
    ))
    .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    set_text_selection_caret(&mut engine, &mut dom, input_id, value.chars().count());
    assert!(engine.key_action(&mut dom, KeyAction::WordLeft));
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, 0);
  }

  #[test]
  fn word_backspace_deletes_combining_mark_sequence() {
    let value = "a\u{0301}b";
    let mut dom = crate::dom::parse_html(&format!(
      "<html><body><input value=\"{value}\"></body></html>"
    ))
    .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    set_text_selection_caret(&mut engine, &mut dom, input_id, value.chars().count());
    assert!(engine.key_action(&mut dom, KeyAction::WordBackspace));
    assert_eq!(input_value(&mut dom, input_id), "");
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, 0);
  }

  #[test]
  fn text_delete_range_for_key_is_total_for_unexpected_key_actions() {
    // Ensure we never panic if some unrelated KeyAction is routed into the text delete handler.
    let selection = Some((1, 3));
    assert_eq!(
      text_delete_range_for_key(KeyAction::Enter, "abcd", 2, selection),
      None
    );
    assert_eq!(
      text_delete_range_for_key(KeyAction::ArrowLeft, "abcd", 2, None),
      None
    );
  }

  #[test]
  fn undo_redo_restores_value_and_selection() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"abcd\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    set_text_selection_range(&mut engine, &mut dom, input_id, 1, 3);
    assert!(engine.text_input(&mut dom, "X"));
    assert_eq!(input_value(&mut dom, input_id), "aXd");

    assert!(engine.key_action(&mut dom, KeyAction::Undo));
    assert_eq!(input_value(&mut dom, input_id), "abcd");
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.selection(), Some((1, 3)));

    assert!(engine.key_action(&mut dom, KeyAction::Redo));
    assert_eq!(input_value(&mut dom, input_id), "aXd");
    assert_eq!(engine.text_edit.as_ref().unwrap().selection(), None);
  }

  #[test]
  fn text_input_replaces_selection_and_updates_caret() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"hello\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    // Select "ell".
    set_text_selection_range(&mut engine, &mut dom, input_id, 1, 4);

    assert!(engine.text_input(&mut dom, "X"));
    assert_eq!(input_value(&mut dom, input_id), "hXo");
    let edit = engine.text_edit.as_ref().unwrap();
    assert_eq!(edit.caret, 2);
    assert_eq!(edit.selection(), None);
  }

  #[test]
  fn maxlength_blocks_additional_typing_in_input() {
    let mut dom = crate::dom::parse_html(
      "<html><body><input maxlength=\"5\" value=\"hello\"></body></html>",
    )
    .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");
    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);
    set_text_selection_caret(&mut engine, &mut dom, input_id, "hello".chars().count());
    engine.text_input(&mut dom, "X");
    assert_eq!(input_value(&mut dom, input_id), "hello");
  }

  #[test]
  fn maxlength_allows_replacing_selection_in_input() {
    let mut dom = crate::dom::parse_html(
      "<html><body><input maxlength=\"5\" value=\"hello\"></body></html>",
    )
    .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");
    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);
    // Select the last two characters ("lo") and replace with one character.
    set_text_selection_range(&mut engine, &mut dom, input_id, 3, 5);
    engine.text_input(&mut dom, "X");
    assert_eq!(input_value(&mut dom, input_id), "helX");
  }

  #[test]
  fn maxlength_counts_utf16_units() {
    let mut dom = crate::dom::parse_html("<html><body><input maxlength=\"2\"></body></html>")
      .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");
    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);
    engine.text_input(&mut dom, "😀");
    assert_eq!(input_value(&mut dom, input_id), "😀");
    engine.text_input(&mut dom, "😀");
    assert_eq!(input_value(&mut dom, input_id), "😀");
  }

  #[test]
  fn maxlength_blocks_textarea_insertion() {
    let mut dom = crate::dom::parse_html(
      "<html><body><textarea maxlength=\"3\">abc</textarea></body></html>",
    )
    .expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");
    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);
    set_text_selection_caret(&mut engine, &mut dom, textarea_id, "abc".chars().count());
    engine.text_input(&mut dom, "X");
    assert_eq!(textarea_value(&mut dom, textarea_id), "abc");
  }

  #[test]
  fn enter_inserts_newline_for_textarea_but_not_input() {
    let mut dom = crate::dom::parse_html(
      "<html><body><textarea>hi</textarea><input value=\"hi\"></body></html>",
    )
    .expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);
    assert!(engine.key_action(&mut dom, KeyAction::Enter));
    assert_eq!(textarea_value(&mut dom, textarea_id), "hi\n");

    engine.focus_node_id(&mut dom, Some(input_id), true);
    engine.key_action(&mut dom, KeyAction::Enter);
    assert_eq!(input_value(&mut dom, input_id), "hi");
  }

  #[test]
  fn ime_commit_inserts_at_non_end_caret() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"abc\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    // Place caret between "a" and "b".
    set_text_selection_caret(&mut engine, &mut dom, input_id, 1);

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.state.ime_preedit.is_some());

    engine.ime_commit(&mut dom, "Z");

    assert!(engine.state.ime_preedit.is_none());
    assert_eq!(input_value(&mut dom, input_id), "aZbc");
    assert_eq!(engine.text_edit.as_ref().unwrap().caret, 2);
  }

  #[test]
  fn style_for_styled_node_id_ignores_pseudo_element_boxes() {
    use crate::style::display::FormattingContextType;
    use crate::style::types::Direction;
    use crate::tree::box_tree::GeneratedPseudoElement;

    let mut pseudo_style = ComputedStyle::default();
    pseudo_style.direction = Direction::Ltr;
    let pseudo_style = Arc::new(pseudo_style);

    let mut real_style = ComputedStyle::default();
    real_style.direction = Direction::Rtl;
    let real_style = Arc::new(real_style);

    let mut pseudo = BoxNode::new_block(pseudo_style, FormattingContextType::Block, vec![]);
    pseudo.styled_node_id = Some(1);
    pseudo.generated_pseudo = Some(GeneratedPseudoElement::Before);

    let mut real = BoxNode::new_block(real_style, FormattingContextType::Block, vec![]);
    real.styled_node_id = Some(1);

    let root_style = Arc::new(ComputedStyle::default());
    let root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![pseudo, real]);
    let tree = BoxTree::new(root);

    let style = style_for_styled_node_id(&tree, 1).expect("style");
    assert_eq!(style.direction, Direction::Rtl);
  }

  #[test]
  fn label_hover_chain_includes_associated_control() {
    let mut dom = crate::dom::parse_html(
      "<html><body><label for=\"c\">Label</label><input id=\"c\"></body></html>",
    )
    .expect("parse");
    let label_id = find_element_node_id(&mut dom, "label");
    let input_id = find_element_node_id(&mut dom, "input");
    let index = DomIndexMut::new(&mut dom);
    let chain = collect_element_chain_with_label_associated_controls(&index, label_id);
    assert!(chain.contains(&label_id));
    assert!(
      chain.contains(&input_id),
      "label chains should include the associated control (for=)"
    );

    let mut dom =
      crate::dom::parse_html("<html><body><label>Label <input id=\"c\"></label></body></html>")
        .expect("parse");
    let label_id = find_element_node_id(&mut dom, "label");
    let input_id = find_element_node_id(&mut dom, "input");
    let index = DomIndexMut::new(&mut dom);
    let chain = collect_element_chain_with_label_associated_controls(&index, label_id);
    assert!(chain.contains(&label_id));
    assert!(
      chain.contains(&input_id),
      "label chains should include the associated control (descendant)"
    );
  }

  #[test]
  fn disabled_fieldset_allows_controls_in_first_legend() {
    let mut dom = crate::dom::parse_html(
      "<html><body><fieldset disabled><legend><input></legend><input></fieldset></body></html>",
    )
    .expect("parse");
    let index = DomIndexMut::new(&mut dom);

    let mut inputs = Vec::new();
    for node_id in 1..index.id_to_node.len() {
      if index
        .node(node_id)
        .and_then(|node| node.tag_name())
        .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
      {
        inputs.push(node_id);
      }
    }
    assert_eq!(inputs.len(), 2);
    let in_legend = inputs[0];
    let outside_legend = inputs[1];

    assert!(
      is_focusable_interactive_element(&index, in_legend),
      "input inside the first <legend> of a disabled <fieldset> should remain focusable"
    );
    assert!(
      !is_focusable_interactive_element(&index, outside_legend),
      "input outside the first <legend> should be disabled by the <fieldset>"
    );

    let focusables = collect_tab_stops(&index);
    assert!(
      focusables.contains(&in_legend),
      "Tab focusables should include the input inside the first <legend>"
    );
    assert!(
      !focusables.contains(&outside_legend),
      "Tab focusables should not include disabled controls"
    );
  }

  #[test]
  fn disabled_fieldset_does_not_disable_non_controls() {
    let mut dom = crate::dom::parse_html(
      "<html><body><fieldset disabled><div tabindex=\"0\"></div></fieldset></body></html>",
    )
    .expect("parse");
    let div_id = find_element_node_id(&mut dom, "div");
    let index = DomIndexMut::new(&mut dom);

    assert!(
      is_focusable_interactive_element(&index, div_id),
      "tabindex elements should remain focusable inside a disabled <fieldset>"
    );
    let focusables = collect_tab_stops(&index);
    assert!(
      focusables.contains(&div_id),
      "tabindex elements should remain reachable via Tab inside a disabled <fieldset>"
    );
  }

  #[test]
  fn tabindex_negative_allows_pointer_focus_but_is_not_tab_stop() {
    let mut dom = crate::dom::parse_html(
      "<html><body><button></button><div tabindex=\"-1\"></div></body></html>",
    )
    .expect("parse");
    let button_id = find_element_node_id(&mut dom, "button");
    let div_id = find_element_node_id(&mut dom, "div");
    let index = DomIndexMut::new(&mut dom);

    assert!(
      is_focusable_interactive_element(&index, div_id),
      "tabindex=-1 elements should remain focusable via pointer click"
    );

    let focusables = collect_tab_stops(&index);
    assert_eq!(
      focusables,
      vec![button_id],
      "tabindex < 0 elements must be skipped by sequential Tab focus navigation"
    );
  }

  #[test]
  fn tabindex_does_not_make_hidden_input_focusable() {
    let mut dom =
      crate::dom::parse_html("<html><body><input type=\"hidden\" tabindex=\"-1\"></body></html>")
        .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");
    let index = DomIndexMut::new(&mut dom);

    assert!(
      !is_focusable_interactive_element(&index, input_id),
      "input type=hidden must never be focusable even if tabindex is set"
    );

    let focusables = collect_tab_stops(&index);
    assert!(
      !focusables.contains(&input_id),
      "input type=hidden must never appear in the Tab order even if tabindex is set"
    );
  }

  #[test]
  fn form_submission_includes_first_legend_controls_in_disabled_fieldset() {
    let mut dom = crate::dom::parse_html(
      "<html><body><form action=\"https://example.com/submit\">\
       <fieldset disabled>\
         <legend><input name=\"a\" value=\"1\"></legend>\
         <input name=\"b\" value=\"2\">\
       </fieldset>\
     </form></body></html>",
    )
    .expect("parse");
    let form_id = find_element_node_id(&mut dom, "form");

    let submission = form_submission_without_submitter(
      &dom,
      form_id,
      "https://example.com/page",
      "https://example.com/page",
      None,
    )
    .expect("submission");
    assert_eq!(
      submission.url, "https://example.com/submit?a=1",
      "controls in the first <legend> should not be considered disabled when collecting form data"
    );
  }

  #[test]
  fn a11y_set_text_value_updates_dom_and_caret_for_focused_input() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"hi\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    assert!(engine.set_text_control_value(&mut dom, input_id, "hello"));
    assert_eq!(input_value(&mut dom, input_id), "hello");
    assert_eq!(engine.interaction_state().text_edit.unwrap().caret, 5);
  }

  #[test]
  fn a11y_set_text_value_is_noop_for_disabled_control() {
    let mut dom = crate::dom::parse_html("<html><body><input disabled value=\"abc\"></body></html>")
      .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    assert!(!engine.set_text_control_value(&mut dom, input_id, "xyz"));
    assert_eq!(input_value(&mut dom, input_id), "abc");
    assert_eq!(engine.interaction_state().text_edit.unwrap().caret, 3);
  }

  #[test]
  fn a11y_set_text_value_is_noop_for_readonly_control() {
    let mut dom = crate::dom::parse_html("<html><body><input readonly value=\"abc\"></body></html>")
      .expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    assert!(!engine.set_text_control_value(&mut dom, input_id, "xyz"));
    assert_eq!(input_value(&mut dom, input_id), "abc");
    assert_eq!(engine.interaction_state().text_edit.unwrap().caret, 3);
  }

  #[test]
  fn a11y_set_text_value_uses_data_fastr_value_for_textarea() {
    let mut dom =
      crate::dom::parse_html("<html><body><textarea>orig</textarea></body></html>").expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);

    assert!(engine.set_text_control_value(&mut dom, textarea_id, "new"));
    assert_eq!(textarea_value(&mut dom, textarea_id), "new");

    let index = DomIndexMut::new(&mut dom);
    let node = index.node(textarea_id).expect("textarea");
    assert_eq!(node.get_attribute_ref("data-fastr-value"), Some("new"));
    assert_eq!(engine.interaction_state().text_edit.unwrap().caret, 3);
  }

  #[test]
  fn a11y_set_text_selection_range_clamps_to_value_len() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"abc\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    assert!(engine.a11y_set_text_selection_range(&mut dom, input_id, 0, 10));
    let paint = engine.interaction_state().text_edit.unwrap();
    assert_eq!(paint.caret, 3);
    assert_eq!(paint.selection, Some((0, 3)));
  }

  #[test]
  fn focus_node_id_ignores_missing_target() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"a\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);
    assert_eq!(engine.focused_node_id(), Some(input_id));

    // Focusing an out-of-range node id should be a no-op rather than clearing focus.
    let (changed, action) = engine.focus_node_id(&mut dom, Some(input_id + 9999), true);
    assert!(!changed);
    assert_eq!(action, InteractionAction::None);
    assert_eq!(engine.focused_node_id(), Some(input_id));
  }

  #[test]
  fn key_action_clears_detached_focus() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"abc\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);
    assert_eq!(engine.focused_node_id(), Some(input_id));

    // Replace the DOM with one that does not contain the focused node id.
    let mut dom_detached = crate::dom::parse_html("<html><body></body></html>").expect("parse");

    // Should not panic; stale focus should be dropped.
    assert!(engine.key_action(&mut dom_detached, KeyAction::ArrowLeft));
    assert_eq!(engine.focused_node_id(), None);
  }

  #[test]
  fn select_keyboard_action_no_enabled_option_is_noop() {
    let mut dom = crate::dom::parse_html(
      "<html><body><select>\
         <option disabled selected>One</option>\
         <option disabled>Two</option>\
       </select></body></html>",
    )
    .expect("parse");
    let select_id = find_element_node_id(&mut dom, "select");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(select_id), true);

    let index_before = DomIndexMut::new(&mut dom);
    let mut option_ids: Vec<usize> = Vec::new();
    for node_id in 1..index_before.id_to_node.len() {
      if index_before.node(node_id).is_some_and(|node| {
        node
          .tag_name()
          .is_some_and(|t| t.eq_ignore_ascii_case("option"))
      }) {
        option_ids.push(node_id);
      }
    }
    assert_eq!(option_ids.len(), 2);
    let selected_before: Vec<bool> = option_ids
      .iter()
      .map(|&id| {
        index_before
          .node(id)
          .and_then(|node| node.get_attribute_ref("selected"))
          .is_some()
      })
      .collect();

    // With no enabled options, arrow navigation should be a no-op and must not panic.
    assert!(!engine.key_action(&mut dom, KeyAction::ArrowDown));

    let index_after = DomIndexMut::new(&mut dom);
    let selected_after: Vec<bool> = option_ids
      .iter()
      .map(|&id| {
        index_after
          .node(id)
          .and_then(|node| node.get_attribute_ref("selected"))
          .is_some()
      })
      .collect();
    assert_eq!(selected_after, selected_before);
  }

  fn find_box_id_for_styled_node_id(box_tree: &BoxTree, styled_node_id: usize) -> usize {
    let mut stack = vec![&box_tree.root];
    while let Some(node) = stack.pop() {
      if node.styled_node_id == Some(styled_node_id) {
        return node.id;
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    panic!("missing box for styled_node_id={styled_node_id}");
  }

  fn find_text_node_id(dom: &mut DomNode, content: &str) -> usize {
    let index = DomIndexMut::new(dom);
    for node_id in 1..index.id_to_node.len() {
      if let Some(node) = index.node(node_id) {
        if matches!(&node.node_type, DomNodeType::Text { content: c } if c == content) {
          return node_id;
        }
      }
    }
    panic!("missing text node with content={content:?}");
  }

  #[test]
  fn active_text_drag_reflects_pointer_selection_gesture() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"abc\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut input_box = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      crate::style::display::FormattingContextType::Block,
      vec![],
    );
    input_box.styled_node_id = Some(input_id);
    let root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      crate::style::display::FormattingContextType::Block,
      vec![input_box],
    );
    let box_tree = BoxTree::new(root);
    let input_box_id = find_box_id_for_styled_node_id(&box_tree, input_id);

    let fragment_tree = FragmentTree::new(crate::tree::fragment_tree::FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
      vec![crate::tree::fragment_tree::FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 0.0, 200.0, 40.0),
        input_box_id,
        vec![],
      )],
    ));

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);
    engine.pointer_down_with_click_count(
      &mut dom,
      &box_tree,
      &fragment_tree,
      &ScrollState::default(),
      Point::new(5.0, 5.0),
      PointerButton::Primary,
      PointerModifiers::NONE,
      1,
    );
    assert_eq!(engine.active_text_drag(), Some((input_id, input_box_id)));

    let _ = engine.pointer_up_with_scroll(
      &mut dom,
      &box_tree,
      &fragment_tree,
      &ScrollState::default(),
      Point::new(5.0, 5.0),
      PointerButton::Primary,
      PointerModifiers::NONE,
      false,
      "https://example.com/",
      "https://example.com/",
    );
    assert_eq!(engine.active_text_drag(), None);
  }

  #[test]
  fn active_document_selection_drag_reflects_pointer_selection_gesture() {
    let text = "Hello";
    let mut dom = crate::dom::parse_html("<html><body><p>Hello</p></body></html>").expect("parse");
    let text_node_id = find_text_node_id(&mut dom, text);

    let mut text_box = BoxNode::new_text(Arc::new(ComputedStyle::default()), text.to_string());
    text_box.styled_node_id = Some(text_node_id);
    let root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      crate::style::display::FormattingContextType::Block,
      vec![text_box],
    );
    let box_tree = BoxTree::new(root);
    let text_box_id = find_box_id_for_styled_node_id(&box_tree, text_node_id);

    let mut text_fragment = crate::tree::fragment_tree::FragmentNode::new_text(
      Rect::from_xywh(0.0, 0.0, 200.0, 40.0),
      text,
      0.0,
    );
    if let FragmentContent::Text {
      box_id,
      source_range,
      ..
    } = &mut text_fragment.content
    {
      *box_id = Some(text_box_id);
      *source_range = crate::tree::fragment_tree::TextSourceRange::new(0..text.len());
    } else {
      panic!("expected text fragment content");
    }

    let fragment_tree = FragmentTree::new(crate::tree::fragment_tree::FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
      vec![text_fragment],
    ));

    let mut engine = InteractionEngine::new();
    engine.pointer_down(
      &mut dom,
      &box_tree,
      &fragment_tree,
      &ScrollState::default(),
      Point::new(5.0, 5.0),
    );
    assert!(engine.active_document_selection_drag());

    let _ = engine.pointer_up_with_scroll(
      &mut dom,
      &box_tree,
      &fragment_tree,
      &ScrollState::default(),
      Point::new(5.0, 5.0),
      PointerButton::Primary,
      PointerModifiers::NONE,
      false,
      "https://example.com/",
      "https://example.com/",
    );
    assert!(!engine.active_document_selection_drag());
  }

  #[test]
  fn active_text_drag_clears_on_focus_change() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"abc\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut input_box = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      crate::style::display::FormattingContextType::Block,
      vec![],
    );
    input_box.styled_node_id = Some(input_id);
    let root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      crate::style::display::FormattingContextType::Block,
      vec![input_box],
    );
    let box_tree = BoxTree::new(root);
    let input_box_id = find_box_id_for_styled_node_id(&box_tree, input_id);

    let fragment_tree = FragmentTree::new(crate::tree::fragment_tree::FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
      vec![crate::tree::fragment_tree::FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 0.0, 200.0, 40.0),
        input_box_id,
        vec![],
      )],
    ));

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);
    engine.pointer_down_with_click_count(
      &mut dom,
      &box_tree,
      &fragment_tree,
      &ScrollState::default(),
      Point::new(5.0, 5.0),
      PointerButton::Primary,
      PointerModifiers::NONE,
      1,
    );
    assert_eq!(engine.active_text_drag(), Some((input_id, input_box_id)));

    // Any focus change should clear an in-progress text selection drag.
    let _ = engine.focus_node_id(&mut dom, None, true);
    assert_eq!(engine.active_text_drag(), None);
  }

  #[test]
  fn active_text_drag_clears_on_clear_pointer_state() {
    let mut dom =
      crate::dom::parse_html("<html><body><input value=\"abc\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut input_box = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      crate::style::display::FormattingContextType::Block,
      vec![],
    );
    input_box.styled_node_id = Some(input_id);
    let root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      crate::style::display::FormattingContextType::Block,
      vec![input_box],
    );
    let box_tree = BoxTree::new(root);
    let input_box_id = find_box_id_for_styled_node_id(&box_tree, input_id);

    let fragment_tree = FragmentTree::new(crate::tree::fragment_tree::FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
      vec![crate::tree::fragment_tree::FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 0.0, 200.0, 40.0),
        input_box_id,
        vec![],
      )],
    ));

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);
    engine.pointer_down_with_click_count(
      &mut dom,
      &box_tree,
      &fragment_tree,
      &ScrollState::default(),
      Point::new(5.0, 5.0),
      PointerButton::Primary,
      PointerModifiers::NONE,
      1,
    );
    assert_eq!(engine.active_text_drag(), Some((input_id, input_box_id)));

    engine.clear_pointer_state(&mut dom);
    assert_eq!(engine.active_text_drag(), None);
  }

  #[test]
  fn active_document_selection_drag_clears_on_focus_change() {
    let text = "Hello";
    let mut dom = crate::dom::parse_html("<html><body><p>Hello</p></body></html>").expect("parse");
    let p_id = find_element_node_id(&mut dom, "p");
    let text_node_id = find_text_node_id(&mut dom, text);

    let mut text_box = BoxNode::new_text(Arc::new(ComputedStyle::default()), text.to_string());
    text_box.styled_node_id = Some(text_node_id);
    let root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      crate::style::display::FormattingContextType::Block,
      vec![text_box],
    );
    let box_tree = BoxTree::new(root);
    let text_box_id = find_box_id_for_styled_node_id(&box_tree, text_node_id);

    let mut text_fragment = crate::tree::fragment_tree::FragmentNode::new_text(
      Rect::from_xywh(0.0, 0.0, 200.0, 40.0),
      text,
      0.0,
    );
    if let FragmentContent::Text {
      box_id,
      source_range,
      ..
    } = &mut text_fragment.content
    {
      *box_id = Some(text_box_id);
      *source_range = crate::tree::fragment_tree::TextSourceRange::new(0..text.len());
    } else {
      panic!("expected text fragment content");
    }

    let fragment_tree = FragmentTree::new(crate::tree::fragment_tree::FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
      vec![text_fragment],
    ));

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(p_id), true);
    engine.pointer_down(
      &mut dom,
      &box_tree,
      &fragment_tree,
      &ScrollState::default(),
      Point::new(5.0, 5.0),
    );
    assert!(engine.active_document_selection_drag());

    // Any focus change should clear an in-progress document selection drag.
    let _ = engine.focus_node_id(&mut dom, None, true);
    assert!(!engine.active_document_selection_drag());
  }

  #[test]
  fn active_document_selection_drag_clears_on_clear_pointer_state() {
    let text = "Hello";
    let mut dom = crate::dom::parse_html("<html><body><p>Hello</p></body></html>").expect("parse");
    let text_node_id = find_text_node_id(&mut dom, text);

    let mut text_box = BoxNode::new_text(Arc::new(ComputedStyle::default()), text.to_string());
    text_box.styled_node_id = Some(text_node_id);
    let root = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      crate::style::display::FormattingContextType::Block,
      vec![text_box],
    );
    let box_tree = BoxTree::new(root);
    let text_box_id = find_box_id_for_styled_node_id(&box_tree, text_node_id);

    let mut text_fragment = crate::tree::fragment_tree::FragmentNode::new_text(
      Rect::from_xywh(0.0, 0.0, 200.0, 40.0),
      text,
      0.0,
    );
    if let FragmentContent::Text {
      box_id,
      source_range,
      ..
    } = &mut text_fragment.content
    {
      *box_id = Some(text_box_id);
      *source_range = crate::tree::fragment_tree::TextSourceRange::new(0..text.len());
    } else {
      panic!("expected text fragment content");
    }

    let fragment_tree = FragmentTree::new(crate::tree::fragment_tree::FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
      vec![text_fragment],
    ));

    let mut engine = InteractionEngine::new();
    engine.pointer_down(
      &mut dom,
      &box_tree,
      &fragment_tree,
      &ScrollState::default(),
      Point::new(5.0, 5.0),
    );
    assert!(engine.active_document_selection_drag());

    engine.clear_pointer_state(&mut dom);
    assert!(!engine.active_document_selection_drag());
  }
}

fn nearest_element_ancestor(index: &DomIndexMut, mut node_id: usize) -> Option<usize> {
  while node_id != 0 {
    let node = index.node(node_id)?;
    if node.is_element() {
      return Some(node_id);
    }
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  None
}

fn collect_element_chain(index: &DomIndexMut, start: usize) -> Vec<usize> {
  let mut chain = Vec::new();
  let mut current = Some(start);
  while let Some(id) = current {
    if id == 0 {
      break;
    }
    if index.node(id).is_some_and(DomNode::is_element) {
      chain.push(id);
    }
    current = index.parent.get(id).copied();
  }
  chain
}

fn collect_element_chain_with_label_associated_controls(
  index: &DomIndexMut,
  start: usize,
) -> Vec<usize> {
  let mut chain = collect_element_chain(index, start);
  // HTML defines that a label's associated control also matches :hover/:active when the label
  // itself matches. For interaction state, approximate this by unioning the element chain of any
  // hovered/active label with the chain of its associated control.
  //
  // Note: We only scan the *original* chain for labels to avoid cascading into newly added
  // ancestors; nested label associations are extremely rare and not worth the complexity here.
  let baseline = chain.clone();
  for id in baseline {
    if index.node(id).is_some_and(is_label) {
      if let Some(control) = find_label_associated_control(index, id) {
        for control_id in collect_element_chain(index, control) {
          if !chain.contains(&control_id) {
            chain.push(control_id);
          }
        }
      }
    }
  }
  chain
}

fn set_attr(attrs: &mut Vec<(String, String)>, name: &str, value: &str) -> bool {
  if let Some((_, existing)) = attrs.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
    if existing == value {
      return false;
    }
    existing.clear();
    existing.push_str(value);
    return true;
  }
  attrs.push((name.to_string(), value.to_string()));
  true
}

fn remove_attr(attrs: &mut Vec<(String, String)>, name: &str) -> bool {
  if let Some(idx) = attrs.iter().position(|(k, _)| k.eq_ignore_ascii_case(name)) {
    attrs.remove(idx);
    return true;
  }
  false
}

fn set_node_attr(node: &mut DomNode, name: &str, value: &str) -> bool {
  match &mut node.node_type {
    DomNodeType::Element { attributes, .. } | DomNodeType::Slot { attributes, .. } => {
      set_attr(attributes, name, value)
    }
    _ => false,
  }
}

fn remove_node_attr(node: &mut DomNode, name: &str) -> bool {
  match &mut node.node_type {
    DomNodeType::Element { attributes, .. } | DomNodeType::Slot { attributes, .. } => {
      remove_attr(attributes, name)
    }
    _ => false,
  }
}

fn trim_ascii_whitespace(value: &str) -> &str {
  // HTML URL-ish attributes strip leading/trailing ASCII whitespace (TAB/LF/FF/CR/SPACE) but do not
  // treat all Unicode whitespace as ignorable. Use an explicit trim to avoid incorrectly dropping
  // characters like NBSP (U+00A0).
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn parse_non_negative_integer(value: &str) -> Option<usize> {
  let trimmed = trim_ascii_whitespace(value);
  if trimmed.is_empty() {
    return None;
  }
  let parsed = trimmed.parse::<i64>().ok()?;
  if parsed < 0 {
    return None;
  }
  usize::try_from(parsed).ok()
}

fn utf16_len(value: &str) -> usize {
  value.chars().map(|ch| ch.len_utf16()).sum()
}

fn truncate_str_to_utf16_units(value: &str, max_units: usize) -> &str {
  if max_units == 0 {
    return "";
  }
  let mut units = 0usize;
  let mut end_byte = 0usize;
  for (byte_idx, ch) in value.char_indices() {
    let next_units = units.saturating_add(ch.len_utf16());
    if next_units > max_units {
      break;
    }
    units = next_units;
    end_byte = byte_idx.saturating_add(ch.len_utf8());
  }
  &value[..end_byte]
}

fn strip_ascii_line_breaks(value: &str) -> Cow<'_, str> {
  if !value.as_bytes().iter().any(|b| matches!(*b, b'\n' | b'\r')) {
    return Cow::Borrowed(value);
  }
  let mut out = String::with_capacity(value.len());
  for ch in value.chars() {
    if matches!(ch, '\n' | '\r') {
      continue;
    }
    out.push(ch);
  }
  Cow::Owned(out)
}

fn is_anchor_with_href(node: &DomNode) -> bool {
  // MVP: treat <a href> and <area href> as focusable/navigable "links".
  node.tag_name().is_some_and(|tag| {
    (tag.eq_ignore_ascii_case("a") || tag.eq_ignore_ascii_case("area"))
      && node.get_attribute_ref("href").is_some_and(|href| {
        let href = trim_ascii_whitespace(href);
        // Treat an explicitly present `href` attribute as a valid same-document navigation target,
        // even when the value is empty/whitespace-only (matches common browser behavior for
        // `<a href="">`).
        !href
          .as_bytes()
          .get(.."javascript:".len())
          .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"javascript:"))
      })
  })
}

fn is_focusable_anchor(node: &DomNode) -> bool {
  if !node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("a") || tag.eq_ignore_ascii_case("area"))
  {
    return false;
  }
  let Some(href) = node.get_attribute_ref("href") else {
    return false;
  };
  let href = trim_ascii_whitespace(href);
  // The browser UI doesn't execute JS, so `javascript:` URLs aren't meaningful navigation targets
  // and should not appear in the Tab sequence.
  if href
    .as_bytes()
    .get(.."javascript:".len())
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"javascript:"))
  {
    return false;
  }
  true
}

fn is_label(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("label"))
}

fn is_labelable_form_control(node: &DomNode) -> bool {
  if is_input(node) {
    return !input_type(node).eq_ignore_ascii_case("hidden");
  }
  is_textarea(node) || is_select(node) || is_button(node)
}

fn is_input(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
}

fn is_textarea(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("textarea"))
}

fn is_select(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("select"))
}

fn is_option(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("option"))
}

fn is_form(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("form"))
}

fn is_button(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("button"))
}

fn media_controls_kind(node: &DomNode) -> Option<MediaElementKind> {
  let tag = node.tag_name()?;
  if tag.eq_ignore_ascii_case("video") {
    return node
      .get_attribute_ref("controls")
      .is_some()
      .then_some(MediaElementKind::Video);
  }
  if tag.eq_ignore_ascii_case("audio") {
    return node
      .get_attribute_ref("controls")
      .is_some()
      .then_some(MediaElementKind::Audio);
  }
  None
}

fn is_details(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("details"))
}

fn is_summary(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("summary"))
}

/// Returns `Some(details_id)` if `summary_id` is the *details summary* for its parent `<details>`.
///
/// A details summary is:
/// - a `<summary>` element
/// - whose parent is a `<details>` element
/// - and which is the *first* `<summary>` element child of that `<details>`.
fn details_owner_for_summary(index: &DomIndexMut, summary_id: usize) -> Option<usize> {
  let summary = index.node(summary_id)?;
  if !is_summary(summary) {
    return None;
  }

  let details_id = index.parent.get(summary_id).copied().unwrap_or(0);
  if details_id == 0 {
    return None;
  }
  if !index.node(details_id).is_some_and(is_details) {
    return None;
  }

  // Find the first `<summary>` element child of the `<details>` in DOM order (ignore nested
  // summaries).
  for node_id in (details_id + 1)..index.id_to_node.len() {
    if !is_ancestor_or_self(index, details_id, node_id) {
      break;
    }
    if index.parent.get(node_id).copied().unwrap_or(0) != details_id {
      continue;
    }
    if index.node(node_id).is_some_and(is_summary) {
      return (node_id == summary_id).then_some(details_id);
    }
  }

  None
}

/// Walk up the ancestor chain (including `start`) to find the nearest details summary.
///
/// Returns `(summary_id, details_id)` when found.
fn nearest_details_summary(index: &DomIndexMut, mut node_id: usize) -> Option<(usize, usize)> {
  while node_id != 0 {
    if let Some(details_id) = details_owner_for_summary(index, node_id) {
      return Some((node_id, details_id));
    }
    node_id = index.parent.get(node_id).copied().unwrap_or(0);
  }
  None
}

fn toggle_details_open(index: &mut DomIndexMut, details_id: usize) -> bool {
  let Some(node_mut) = index.node_mut(details_id) else {
    return false;
  };
  if !is_details(node_mut) {
    return false;
  }
  if node_mut.get_attribute_ref("open").is_some() {
    remove_node_attr(node_mut, "open")
  } else {
    set_node_attr(node_mut, "open", "")
  }
}

fn input_type(node: &DomNode) -> &str {
  node
    .get_attribute_ref("type")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .unwrap_or("text")
}

fn button_type(node: &DomNode) -> &str {
  node
    .get_attribute_ref("type")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    // HTML default button type is "submit".
    .unwrap_or("submit")
}

fn is_checkbox_input(node: &DomNode) -> bool {
  is_input(node) && input_type(node).eq_ignore_ascii_case("checkbox")
}

fn is_radio_input(node: &DomNode) -> bool {
  is_input(node) && input_type(node).eq_ignore_ascii_case("radio")
}

fn is_range_input(node: &DomNode) -> bool {
  is_input(node) && input_type(node).eq_ignore_ascii_case("range")
}

fn is_color_input(node: &DomNode) -> bool {
  is_input(node) && input_type(node).eq_ignore_ascii_case("color")
}

fn is_file_input(node: &DomNode) -> bool {
  is_input(node) && input_type(node).eq_ignore_ascii_case("file")
}

fn normalize_file_selection_path(path: &PathBuf) -> PathBuf {
  std::fs::canonicalize(path).unwrap_or_else(|_| {
    if path.is_absolute() {
      path.clone()
    } else {
      std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(path)
    }
  })
}

fn content_type_for_path(path: &std::path::Path) -> String {
  let ext = path
    .extension()
    .map(|ext| ext.to_string_lossy().to_ascii_lowercase());
  let mime = match ext.as_deref() {
    Some("txt") => "text/plain",
    Some("html") | Some("htm") => "text/html",
    Some("css") => "text/css",
    Some("js") | Some("mjs") => "text/javascript",
    Some("json") => "application/json",
    Some("xml") => "application/xml",
    Some("png") => "image/png",
    Some("jpg") | Some("jpeg") => "image/jpeg",
    Some("gif") => "image/gif",
    Some("webp") => "image/webp",
    Some("svg") | Some("svgz") => "image/svg+xml",
    Some("pdf") => "application/pdf",
    _ => "application/octet-stream",
  };
  mime.to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileAcceptToken {
  Extension(String),
  MimeExact(String),
  MimeWildcard(String),
}

fn parse_file_accept_tokens(accept: &str) -> Vec<FileAcceptToken> {
  accept
    .split(',')
    .filter_map(|raw| {
      let token = trim_ascii_whitespace(raw);
      if token.is_empty() {
        return None;
      }

      if let Some(ext) = token.strip_prefix('.') {
        let ext = trim_ascii_whitespace(ext);
        if ext.is_empty() {
          return None;
        }
        return Some(FileAcceptToken::Extension(ext.to_ascii_lowercase()));
      }

      let (ty, sub) = token.split_once('/')?;
      let ty = trim_ascii_whitespace(ty);
      let sub = trim_ascii_whitespace(sub);
      if ty.is_empty() || sub.is_empty() {
        return None;
      }
      let ty = ty.to_ascii_lowercase();
      let sub = sub.to_ascii_lowercase();
      if sub == "*" {
        Some(FileAcceptToken::MimeWildcard(ty))
      } else {
        Some(FileAcceptToken::MimeExact(format!("{ty}/{sub}")))
      }
    })
    .collect()
}

fn file_path_matches_accept(path: &std::path::Path, tokens: &[FileAcceptToken]) -> bool {
  if tokens.is_empty() {
    return true;
  }

  let mut ext: Option<String> = None;
  let mut mime: Option<String> = None;

  for token in tokens {
    match token {
      FileAcceptToken::Extension(want_ext) => {
        if ext.is_none() {
          ext = path
            .extension()
            .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
            .filter(|ext| !ext.is_empty());
        }
        if ext.as_deref() == Some(want_ext.as_str()) {
          return true;
        }
      }
      FileAcceptToken::MimeExact(want) => {
        if mime.is_none() {
          mime = Some(content_type_for_path(path));
        }
        if mime.as_deref() == Some(want.as_str()) {
          return true;
        }
      }
      FileAcceptToken::MimeWildcard(major) => {
        if mime.is_none() {
          mime = Some(content_type_for_path(path));
        }
        let Some(mime) = mime.as_deref() else {
          continue;
        };
        if mime
          .split_once('/')
          .is_some_and(|(got_major, _)| got_major.eq_ignore_ascii_case(major))
        {
          return true;
        }
      }
    }
  }

  false
}

fn filter_paths_by_file_accept<'a>(
  paths: &'a [PathBuf],
  accept: Option<&str>,
) -> Cow<'a, [PathBuf]> {
  let accept = accept.map(trim_ascii_whitespace).filter(|v| !v.is_empty());
  let Some(accept) = accept else {
    return Cow::Borrowed(paths);
  };
  let tokens = parse_file_accept_tokens(accept);
  if tokens.is_empty() {
    return Cow::Borrowed(paths);
  }

  let filtered: Vec<PathBuf> = paths
    .iter()
    .filter(|path| file_path_matches_accept(path, &tokens))
    .cloned()
    .collect();
  Cow::Owned(filtered)
}

/// Environment variable override for the per-file read limit used by `<input type="file">`.
///
/// FastRender currently snapshots selected files by eagerly reading their bytes into memory so they
/// can be included in form submissions. To prevent accidental OOM when a user selects an enormous
/// file, we enforce a hard cap on the number of bytes read per selected file.
///
/// Behavior: files whose metadata-reported size exceeds the limit are **skipped** (not selected).
const ENV_MAX_FILE_INPUT_BYTES: &str = "FASTR_MAX_FILE_INPUT_BYTES";

/// Default per-file byte limit for file input selections.
///
/// This is a best-effort safety bound, not an HTTP upload limit.
const DEFAULT_MAX_FILE_INPUT_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB

fn max_file_input_bytes() -> u64 {
  let raw = match std::env::var(ENV_MAX_FILE_INPUT_BYTES) {
    Ok(v) => v,
    Err(_) => return DEFAULT_MAX_FILE_INPUT_BYTES,
  };
  let raw = raw.trim();
  if raw.is_empty() {
    return DEFAULT_MAX_FILE_INPUT_BYTES;
  }
  let raw = raw.replace('_', "");
  match raw.parse::<u64>() {
    Ok(v) if v > 0 => v,
    _ => DEFAULT_MAX_FILE_INPUT_BYTES,
  }
}

fn read_file_bytes_bounded(path: &std::path::Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
  use std::io::Read;

  // Keep allocations representable on this platform even if the user configured a huge limit.
  let max_bytes = max_bytes.min(usize::MAX as u64);
  let max_plus_one = max_bytes.saturating_add(1);

  let mut file = std::fs::File::open(path)?;
  let mut limited = file.take(max_plus_one);
  let mut buf = Vec::new();
  limited.read_to_end(&mut buf)?;
  if (buf.len() as u64) > max_bytes {
    return Err(std::io::Error::new(
      std::io::ErrorKind::Other,
      "file exceeds maximum byte limit",
    ));
  }
  Ok(buf)
}

fn build_file_selections_from_paths(paths: &[PathBuf], multiple: bool) -> Vec<FileSelection> {
  let max_bytes = max_file_input_bytes();
  let mut selected: Vec<FileSelection> = Vec::new();
  for path in paths {
    if path.as_os_str().is_empty() {
      continue;
    }
    if !multiple && !selected.is_empty() {
      break;
    }

    let filename = path
      .file_name()
      .map(|name| name.to_string_lossy().to_string())
      .filter(|name| !name.is_empty());
    let Some(filename) = filename else {
      continue;
    };

    let normalized_path = normalize_file_selection_path(path);
    let file_len = match std::fs::metadata(&normalized_path) {
      Ok(meta) => meta.len(),
      Err(_) => continue,
    };
    if file_len > max_bytes {
      continue;
    }
    // NOTE: Avoid `std::fs::read` here: it reads the full file into memory unconditionally.
    // `read_file_bytes_bounded` is additionally defensive against TOCTOU races where the file grows
    // after the metadata check.
    let bytes = match read_file_bytes_bounded(&normalized_path, max_bytes) {
      Ok(bytes) => bytes,
      Err(_) => continue,
    };

    selected.push(FileSelection {
      path: normalized_path,
      filename,
      content_type: content_type_for_path(path),
      bytes,
    });
  }
  selected
}

fn date_time_input_kind(node: &DomNode) -> Option<DateTimeInputKind> {
  if !is_input(node) {
    return None;
  }
  let ty = input_type(node);
  if ty.eq_ignore_ascii_case("date") {
    Some(DateTimeInputKind::Date)
  } else if ty.eq_ignore_ascii_case("time") {
    Some(DateTimeInputKind::Time)
  } else if ty.eq_ignore_ascii_case("datetime-local") {
    Some(DateTimeInputKind::DateTimeLocal)
  } else if ty.eq_ignore_ascii_case("month") {
    Some(DateTimeInputKind::Month)
  } else if ty.eq_ignore_ascii_case("week") {
    Some(DateTimeInputKind::Week)
  } else {
    None
  }
}

fn is_submit_input(node: &DomNode) -> bool {
  is_input(node) && input_type(node).eq_ignore_ascii_case("submit")
}

fn is_image_submit_input(node: &DomNode) -> bool {
  is_input(node) && input_type(node).eq_ignore_ascii_case("image")
}

fn is_submit_button(node: &DomNode) -> bool {
  is_button(node) && button_type(node).eq_ignore_ascii_case("submit")
}

fn is_submit_control(node: &DomNode) -> bool {
  is_submit_input(node) || is_image_submit_input(node) || is_submit_button(node)
}

fn is_reset_input(node: &DomNode) -> bool {
  is_input(node) && input_type(node).eq_ignore_ascii_case("reset")
}

fn is_reset_button(node: &DomNode) -> bool {
  is_button(node) && button_type(node).eq_ignore_ascii_case("reset")
}

fn is_reset_control(node: &DomNode) -> bool {
  is_reset_input(node) || is_reset_button(node)
}

fn is_text_input(node: &DomNode) -> bool {
  if !is_input(node) {
    return false;
  }

  let t = input_type(node);
  // MVP heuristic: treat any non-button-ish, non-choice-ish input as a text control.
  !t.eq_ignore_ascii_case("checkbox")
    && !t.eq_ignore_ascii_case("radio")
    && !t.eq_ignore_ascii_case("button")
    && !t.eq_ignore_ascii_case("submit")
    && !t.eq_ignore_ascii_case("reset")
    && !t.eq_ignore_ascii_case("hidden")
    && !t.eq_ignore_ascii_case("range")
    && !t.eq_ignore_ascii_case("color")
    && !t.eq_ignore_ascii_case("file")
    && !t.eq_ignore_ascii_case("image")
}

fn is_text_like_input_for_maxlength(node: &DomNode) -> bool {
  if !is_input(node) {
    return false;
  }
  let ty = input_type(node);
  if ty.eq_ignore_ascii_case("text")
    || ty.eq_ignore_ascii_case("search")
    || ty.eq_ignore_ascii_case("url")
    || ty.eq_ignore_ascii_case("tel")
    || ty.eq_ignore_ascii_case("email")
    || ty.eq_ignore_ascii_case("password")
  {
    return true;
  }
  if ty.eq_ignore_ascii_case("hidden")
    || ty.eq_ignore_ascii_case("submit")
    || ty.eq_ignore_ascii_case("reset")
    || ty.eq_ignore_ascii_case("button")
    || ty.eq_ignore_ascii_case("image")
    || ty.eq_ignore_ascii_case("file")
    || ty.eq_ignore_ascii_case("checkbox")
    || ty.eq_ignore_ascii_case("radio")
    || ty.eq_ignore_ascii_case("range")
    || ty.eq_ignore_ascii_case("color")
    || ty.eq_ignore_ascii_case("number")
    || ty.eq_ignore_ascii_case("date")
    || ty.eq_ignore_ascii_case("datetime-local")
    || ty.eq_ignore_ascii_case("month")
    || ty.eq_ignore_ascii_case("week")
    || ty.eq_ignore_ascii_case("time")
  {
    return false;
  }
  // Unknown input types default to text-like state for maxlength enforcement.
  true
}

fn text_control_maxlength_for_user_editing(node: &DomNode) -> Option<usize> {
  if is_textarea(node) {
    return node
      .get_attribute_ref("maxlength")
      .and_then(parse_non_negative_integer);
  }
  if is_text_like_input_for_maxlength(node) {
    return node
      .get_attribute_ref("maxlength")
      .and_then(parse_non_negative_integer);
  }
  None
}

fn is_text_like_input(node: &DomNode) -> bool {
  is_text_like_input_for_maxlength(node)
}

fn is_disabled_or_inert(index: &DomIndexMut, node_id: usize) -> bool {
  node_or_ancestor_is_inert(index, node_id) || node_is_disabled(index, node_id)
}

/// MVP focusable predicate for pointer focus / blur decisions.
///
/// This covers native interactive elements we currently support, plus `tabindex` focusability.
fn is_focusable_interactive_element(index: &DomIndexMut, node_id: usize) -> bool {
  let Some(node) = index.node(node_id) else {
    return false;
  };

  if is_disabled_or_inert(index, node_id) {
    return false;
  }

  // HTML tabindex support: any parsed `tabindex` value makes the element focusable via pointer and
  // programmatic focus, even when `tabindex < 0`. (Sequential Tab navigation is handled separately
  // by `tab_stop_tabindex`, which excludes `tabindex < 0`.)
  if parse_tabindex(node).is_some() {
    // `input type=hidden` is never focusable, even if tabindex is set.
    if is_input(node) && input_type(node).eq_ignore_ascii_case("hidden") {
      return false;
    }
    return true;
  }

  if is_anchor_with_href(node) {
    return true;
  }

  if details_owner_for_summary(index, node_id).is_some() {
    return true;
  }

  if is_input(node) {
    return !input_type(node).eq_ignore_ascii_case("hidden");
  }

  is_textarea(node) || is_select(node) || is_button(node)
}

fn is_ancestor_or_self(index: &DomIndexMut, ancestor: usize, mut node: usize) -> bool {
  while node != 0 {
    if node == ancestor {
      return true;
    }
    node = *index.parent.get(node).unwrap_or(&0);
  }
  false
}

fn node_is_inert_like(node: &DomNode) -> bool {
  super::effective_disabled::node_self_is_inert(node)
}

fn node_or_ancestor_is_inert(index: &DomIndexMut, node_id: usize) -> bool {
  super::effective_disabled::is_effectively_inert_or_hidden(node_id, index)
}

fn node_self_is_tab_inert(node: &DomNode) -> bool {
  // `<template>` contents are inert and should not be reachable via Tab.
  node_is_inert_like(node) || super::effective_disabled::node_self_is_hidden(node)
}

fn parse_tabindex(node: &DomNode) -> Option<i32> {
  let raw = node.get_attribute_ref("tabindex")?;
  let raw = trim_ascii_whitespace(raw);
  if raw.is_empty() {
    return None;
  }
  raw.parse::<i32>().ok()
}

fn collect_inert_subtree_flags(index: &DomIndexMut) -> Vec<bool> {
  // Stack-safe derived state: `inert[id]` is true if this node is in a subtree excluded from
  // sequential focus navigation (Tab order), driven by `inert`/`hidden`/`data-fastr-inert=true` or a
  // `<template>` ancestor.
  let mut inert = vec![false; index.id_to_node.len()];
  for node_id in 1..index.id_to_node.len() {
    let parent_id = index.parent.get(node_id).copied().unwrap_or(0);
    let parent_inert = inert.get(parent_id).copied().unwrap_or(false);
    let self_inert = index.node(node_id).is_some_and(node_self_is_tab_inert);
    inert[node_id] = parent_inert || self_inert;
  }
  inert
}

/// Returns the effective `tabindex` value for sequential focus navigation (Tab order).
///
/// - `Some(n >= 0)` => element is a tab stop with the given tabindex.
/// - `None` => element is either not focusable, or focusable but not reachable via Tab
///   (e.g. `tabindex < 0`).
fn tab_stop_tabindex(index: &DomIndexMut, inert: &[bool], node_id: usize) -> Option<i32> {
  if inert.get(node_id).copied().unwrap_or(true) {
    return None;
  }
  let Some(node) = index.node(node_id) else {
    return None;
  };
  if !node.is_element() {
    return None;
  }
  if super::effective_disabled::node_self_is_hidden(node) {
    return None;
  }
  if is_input(node) && input_type(node).eq_ignore_ascii_case("hidden") {
    return None;
  }

  let tabindex = parse_tabindex(node);

  // `tabindex` makes any element focusable (even if it is not a native interactive element).
  let focusable = if tabindex.is_some() {
    true
  } else {
    is_focusable_anchor(node)
      || details_owner_for_summary(index, node_id).is_some()
      || is_input(node)
      || is_textarea(node)
      || is_select(node)
      || is_button(node)
  };
  if !focusable {
    return None;
  }

  let tabindex = tabindex.unwrap_or(0);
  if tabindex < 0 {
    return None;
  }

  if node_is_disabled(index, node_id) {
    return None;
  }

  Some(tabindex)
}

fn collect_tab_stops(index: &DomIndexMut) -> Vec<usize> {
  let inert = collect_inert_subtree_flags(index);
  let mut positive: Vec<(i32, usize)> = Vec::new();
  let mut zero: Vec<usize> = Vec::new();

  for node_id in 1..index.id_to_node.len() {
    let Some(tabindex) = tab_stop_tabindex(index, &inert, node_id) else {
      continue;
    };
    if tabindex > 0 {
      positive.push((tabindex, node_id));
    } else {
      // `tabindex == 0` (or omitted) participates in Tab order in DOM order.
      zero.push(node_id);
    }
  }

  // Positive tabindex values are visited first, in ascending order, with ties broken by DOM order.
  positive.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

  let mut out = Vec::with_capacity(positive.len() + zero.len());
  out.extend(positive.into_iter().map(|(_, node_id)| node_id));
  out.extend(zero);
  out
}

fn next_tab_focus(current: Option<usize>, focusables: &[usize]) -> Option<usize> {
  if focusables.is_empty() {
    return None;
  }
  let idx = current.and_then(|current| focusables.iter().position(|id| *id == current));
  match idx {
    Some(i) => Some(focusables[(i + 1) % focusables.len()]),
    // If the currently focused element is not a tab stop (or there is no focus), start at the
    // first tab stop.
    None => Some(focusables[0]),
  }
}

fn prev_tab_focus(current: Option<usize>, focusables: &[usize]) -> Option<usize> {
  if focusables.is_empty() {
    return None;
  }
  let idx = current.and_then(|current| focusables.iter().position(|id| *id == current));
  match idx {
    Some(0) => focusables.last().copied(),
    Some(i) => focusables.get(i.wrapping_sub(1)).copied(),
    // If the currently focused element is not a tab stop (or there is no focus), start at the
    // last tab stop.
    None => focusables.last().copied(),
  }
}

fn node_is_disabled(index: &DomIndexMut, node_id: usize) -> bool {
  super::effective_disabled::is_effectively_disabled(node_id, index)
}

fn node_is_readonly(index: &DomIndexMut, node_id: usize) -> bool {
  index
    .node(node_id)
    .and_then(|node| node.get_attribute_ref("readonly"))
    .is_some()
}

fn find_label_associated_control(index: &DomIndexMut, label_id: usize) -> Option<usize> {
  let label = index.node(label_id)?;
  if !is_label(label) {
    return None;
  }

  if let Some(for_attr) = label
    .get_attribute_ref("for")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
  {
    // Spec-ish: `for` matches element IDs in the same tree (tree-root boundary, i.e. the document
    // or current shadow root).
    let tree_root = tree_root_boundary_id(index, label_id)?;
    let referenced = find_element_by_id_attr_in_tree(index, tree_root, for_attr)?;
    return index
      .node(referenced)
      .is_some_and(is_labelable_form_control)
      .then_some(referenced);
  }

  // Fallback: first descendant control.
  // Pre-order ids are contiguous for a subtree, so scan forward until we leave the label subtree.
  let mut end = label_id;
  for id in (label_id + 1)..index.id_to_node.len() {
    if is_ancestor_or_self(index, label_id, id) {
      end = id;
    } else {
      break;
    }
  }
  for id in (label_id + 1)..=end {
    let Some(node) = index.node(id) else {
      continue;
    };
    if is_labelable_form_control(node) {
      return Some(id);
    }
  }

  None
}

fn textarea_value_for_editing(node: &DomNode) -> String {
  // HTML textarea values have special normalization rules (notably: strip a single leading newline
  // when the value comes from text content, but *not* for user-edited values). The DOM layer models
  // this via `data-fastr-value`; prefer that so text editing matches what box generation / painting
  // will render.
  crate::dom::textarea_current_value(node)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordSelectionClass {
  Word,
  Whitespace,
  Other,
}

fn word_selection_class(ch: char) -> WordSelectionClass {
  use unicode_general_category::GeneralCategory;
  if ch.is_alphanumeric()
    || ch == '_'
    || matches!(
      unicode_general_category::get_general_category(ch),
      GeneralCategory::NonspacingMark
        | GeneralCategory::SpacingMark
        | GeneralCategory::EnclosingMark
    )
  {
    WordSelectionClass::Word
  } else if ch.is_whitespace() {
    WordSelectionClass::Whitespace
  } else {
    WordSelectionClass::Other
  }
}

fn word_selection_range(text: &str, caret: usize) -> Option<(usize, usize)> {
  use unicode_segmentation::UnicodeSegmentation;

  let len = text.chars().count();
  if len == 0 {
    return None;
  }
  let caret = caret.min(len);
  let hit = if caret == len { len - 1 } else { caret };

  let mut idx = 0usize;
  let mut run_start = 0usize;
  let mut run_class: Option<WordSelectionClass> = None;

  let mut target_class: Option<WordSelectionClass> = None;
  let mut target_start = 0usize;

  for segment in UnicodeSegmentation::split_word_bounds(text) {
    for ch in segment.chars() {
      let class = word_selection_class(ch);
      if run_class != Some(class) {
        if let Some(target) = target_class {
          if run_class == Some(target) {
            return Some((target_start, idx));
          }
        }
        run_class = Some(class);
        run_start = idx;
      }

      if idx == hit {
        target_class = Some(class);
        target_start = run_start;
      }

      idx += 1;
    }
  }

  // Reached end of string; if we hit a target class, its run extends to the end.
  target_class.map(|_| (target_start, len))
}

fn textarea_line_selection_range(text: &str, caret: usize) -> (usize, usize) {
  let len = text.chars().count();
  if len == 0 {
    return (0, 0);
  }
  let caret = caret.min(len);

  // Find the start of the line containing `caret` (exclusive of the previous '\n').
  let mut start = 0usize;
  let mut idx = 0usize;
  for ch in text.chars() {
    if idx >= caret {
      break;
    }
    if ch == '\n' {
      start = idx.saturating_add(1);
    }
    idx += 1;
  }

  // Find the end of the line (exclusive of the next '\n').
  let mut end = len;
  for (i, ch) in text.chars().enumerate().skip(start) {
    if ch == '\n' {
      end = i;
      break;
    }
  }

  (start.min(len), end.min(len))
}

fn inset_rect_uniform(rect: Rect, inset: f32) -> Rect {
  Rect::from_xywh(
    rect.x() + inset,
    rect.y() + inset,
    (rect.width() - inset * 2.0).max(0.0),
    (rect.height() - inset * 2.0).max(0.0),
  )
}

const NUMBER_INPUT_AFFORDANCE_WIDTH: f32 = 14.0;
const DATE_LIKE_INPUT_AFFORDANCE_WIDTH: f32 = 12.0;

fn input_affordance_space(input_type: &str, style: &ComputedStyle) -> f32 {
  if matches!(style.appearance, Appearance::None) {
    return 0.0;
  }
  if input_type.eq_ignore_ascii_case("number") {
    return NUMBER_INPUT_AFFORDANCE_WIDTH;
  }
  if matches!(
    input_type.to_ascii_lowercase().as_str(),
    "date" | "datetime-local" | "month" | "week" | "time"
  ) {
    return DATE_LIKE_INPUT_AFFORDANCE_WIDTH;
  }
  0.0
}

fn effective_text_align(style: &ComputedStyle) -> crate::style::types::TextAlign {
  use crate::style::types::{Direction, TextAlign};
  match style.text_align {
    TextAlign::Start | TextAlign::MatchParent | TextAlign::Justify | TextAlign::JustifyAll => {
      if style.direction == Direction::Rtl {
        TextAlign::Right
      } else {
        TextAlign::Left
      }
    }
    TextAlign::End => {
      if style.direction == Direction::Rtl {
        TextAlign::Left
      } else {
        TextAlign::Right
      }
    }
    other => other,
  }
}

fn aligned_text_start_x(style: &ComputedStyle, rect: Rect, advance_width: f32) -> f32 {
  use crate::style::types::TextAlign;
  let advance_width = if advance_width.is_finite() {
    advance_width.max(0.0)
  } else {
    0.0
  };
  match effective_text_align(style) {
    TextAlign::Center => rect.x() + ((rect.width() - advance_width).max(0.0) / 2.0),
    TextAlign::Right => rect.x() + (rect.width() - advance_width).max(0.0),
    _ => rect.x(),
  }
}

fn box_node_by_id(box_tree: &BoxTree, target_box_id: usize) -> Option<&BoxNode> {
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.id == target_box_id {
      return Some(node);
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

trait BoxNodeLookup {
  fn node(&self, box_id: usize) -> Option<&BoxNode>;
}

impl BoxNodeLookup for BoxTree {
  fn node(&self, box_id: usize) -> Option<&BoxNode> {
    box_node_by_id(self, box_id)
  }
}

impl<'a> BoxNodeLookup for HitTestBoxIndex<'a> {
  fn node(&self, box_id: usize) -> Option<&BoxNode> {
    HitTestBoxIndex::node(self, box_id)
  }
}

fn byte_offset_for_char_idx(text: &str, char_idx: usize) -> usize {
  if char_idx == 0 {
    return 0;
  }
  let mut count = 0usize;
  for (byte_idx, _) in text.char_indices() {
    if count == char_idx {
      return byte_idx;
    }
    count += 1;
  }
  text.len()
}

fn char_boundary_bytes(text: &str) -> Vec<usize> {
  let mut out = Vec::with_capacity(text.chars().count().saturating_add(1));
  for (byte_idx, _) in text.char_indices() {
    out.push(byte_idx);
  }
  out.push(text.len());
  out
}

fn char_idx_at_byte(boundary_bytes: &[usize], byte_idx: usize) -> usize {
  match boundary_bytes.binary_search(&byte_idx) {
    Ok(idx) => idx,
    Err(idx) => idx,
  }
}

fn word_char_classes(text: &str) -> Vec<bool> {
  let mut out = Vec::with_capacity(text.chars().count());
  for segment in text.split_word_bounds() {
    for ch in segment.chars() {
      // Keep keyboard word navigation (Ctrl+Arrow, Ctrl+Backspace/Delete) consistent with the
      // pointer's word selection behavior (double click), including treating Unicode combining
      // marks as part of a word.
      out.push(matches!(word_selection_class(ch), WordSelectionClass::Word));
    }
  }
  out
}

fn word_left_char_idx(word_chars: &[bool], caret: usize) -> usize {
  let mut i = caret.min(word_chars.len());
  while i > 0 && !word_chars[i - 1] {
    i -= 1;
  }
  while i > 0 && word_chars[i - 1] {
    i -= 1;
  }
  i
}

fn word_right_char_idx(word_chars: &[bool], caret: usize) -> usize {
  let mut i = caret.min(word_chars.len());
  while i < word_chars.len() && word_chars[i] {
    i += 1;
  }
  while i < word_chars.len() && !word_chars[i] {
    i += 1;
  }
  i
}

fn word_backspace_range(word_chars: &[bool], caret: usize) -> Option<(usize, usize)> {
  let caret = caret.min(word_chars.len());
  if caret == 0 {
    return None;
  }
  let mut start = caret;
  if word_chars[start - 1] {
    while start > 0 && word_chars[start - 1] {
      start -= 1;
    }
  } else {
    while start > 0 && !word_chars[start - 1] {
      start -= 1;
    }
  }
  (start != caret).then_some((start, caret))
}

fn word_delete_range(word_chars: &[bool], caret: usize) -> Option<(usize, usize)> {
  let caret = caret.min(word_chars.len());
  if caret >= word_chars.len() {
    return None;
  }
  let mut end = caret;
  if word_chars[end] {
    while end < word_chars.len() && word_chars[end] {
      end += 1;
    }
  } else {
    while end < word_chars.len() && !word_chars[end] {
      end += 1;
    }
  }
  (end != caret).then_some((caret, end))
}

fn grapheme_cluster_boundaries_char_idx(text: &str) -> Vec<usize> {
  if text.is_empty() {
    return vec![0];
  }
  let boundary_bytes = char_boundary_bytes(text);
  let mut out = Vec::with_capacity(boundary_bytes.len());
  for (byte_idx, _) in text.grapheme_indices(true) {
    out.push(char_idx_at_byte(&boundary_bytes, byte_idx));
  }
  out.push(boundary_bytes.len().saturating_sub(1));
  out
}

fn is_grapheme_cluster_boundary(boundaries: &[usize], char_idx: usize) -> bool {
  boundaries.binary_search(&char_idx).is_ok()
}

fn snap_char_idx_down_to_grapheme_boundary(boundaries: &[usize], char_idx: usize) -> usize {
  if boundaries.is_empty() {
    return 0;
  }
  let len = *boundaries.last().unwrap_or(&0);
  let char_idx = char_idx.min(len);
  let pos = boundaries.partition_point(|&b| b <= char_idx);
  boundaries.get(pos.saturating_sub(1)).copied().unwrap_or(0)
}

fn snap_char_idx_up_to_grapheme_boundary(boundaries: &[usize], char_idx: usize) -> usize {
  if boundaries.is_empty() {
    return 0;
  }
  let len = *boundaries.last().unwrap_or(&0);
  let char_idx = char_idx.min(len);
  let pos = boundaries.partition_point(|&b| b < char_idx);
  boundaries.get(pos).copied().unwrap_or(len)
}

fn prev_grapheme_cluster(text: &str, caret: usize) -> Option<(usize, usize)> {
  if caret == 0 {
    return None;
  }
  let boundaries = grapheme_cluster_boundaries_char_idx(text);
  if boundaries.len() < 2 {
    return None;
  }
  let idx = boundaries
    .partition_point(|&b| b < caret)
    .saturating_sub(1)
    .min(boundaries.len().saturating_sub(2));
  Some((boundaries[idx], boundaries[idx + 1]))
}

fn next_grapheme_cluster(text: &str, caret: usize) -> Option<(usize, usize)> {
  let boundaries = grapheme_cluster_boundaries_char_idx(text);
  let len = *boundaries.last().unwrap_or(&0);
  if caret >= len || boundaries.len() < 2 {
    return None;
  }
  let idx = boundaries
    .partition_point(|&b| b <= caret)
    .saturating_sub(1)
    .min(boundaries.len().saturating_sub(2));
  Some((boundaries[idx], boundaries[idx + 1]))
}

fn text_delete_range_for_key(
  key: KeyAction,
  current: &str,
  caret: usize,
  selection: Option<(usize, usize)>,
) -> Option<(usize, usize, usize)> {
  if !matches!(
    key,
    KeyAction::Backspace | KeyAction::Delete | KeyAction::WordBackspace | KeyAction::WordDelete
  ) {
    return None;
  }

  let (start, end) = if let Some(selection) = selection {
    selection
  } else {
    match key {
      KeyAction::Backspace => prev_grapheme_cluster(current, caret)?,
      KeyAction::Delete => next_grapheme_cluster(current, caret)?,
      KeyAction::WordBackspace => {
        let word_chars = word_char_classes(current);
        word_backspace_range(&word_chars, caret)?
      }
      KeyAction::WordDelete => {
        let word_chars = word_char_classes(current);
        word_delete_range(&word_chars, caret)?
      }
      _ => return None,
    }
  };

  Some((start, end, start))
}

fn shape_text_runs_for_interaction(
  text: &str,
  style: &ComputedStyle,
) -> Option<Arc<Vec<crate::text::pipeline::ShapedRun>>> {
  if text.is_empty() {
    return Some(Arc::new(Vec::new()));
  }

  let pipeline = super::shaping_pipeline_for_interaction();
  let font_ctx = super::font_context_for_interaction();

  if style.letter_spacing == 0.0 && style.word_spacing == 0.0 {
    return pipeline.shape_arc(text, style, font_ctx).ok();
  }

  let mut runs = pipeline.shape(text, style, font_ctx).ok()?;
  TextItem::apply_spacing_to_runs(&mut runs, text, style.letter_spacing, style.word_spacing);
  Some(Arc::new(runs))
}

fn fallback_text_advance(text: &str, style: &ComputedStyle) -> f32 {
  text.chars().count() as f32 * style.font_size * 0.6
}

fn shaped_total_advance(runs: &[crate::text::pipeline::ShapedRun], fallback: f32) -> f32 {
  if runs.is_empty() {
    return fallback;
  }
  let sum: f32 = runs.iter().map(|run| run.advance).sum();
  if sum.is_finite() {
    sum.max(0.0)
  } else {
    fallback
  }
}

fn caret_position_for_x_in_text(
  text: &str,
  boundary_text: &str,
  style: &ComputedStyle,
  rect: Rect,
  x: f32,
) -> (usize, CaretAffinity) {
  let char_count = text.chars().count();
  if char_count == 0 {
    return (0, CaretAffinity::Downstream);
  }

  let fallback_advance = fallback_text_advance(text, style);
  let runs = shape_text_runs_for_interaction(text, style)
    .unwrap_or_else(|| Arc::new(Vec::new()));
  let total_advance = shaped_total_advance(runs.as_ref(), fallback_advance);
  let start_x = aligned_text_start_x(style, rect, total_advance);

  let mut local_x = x - start_x;
  if !local_x.is_finite() {
    local_x = 0.0;
  }
  local_x = local_x.clamp(0.0, total_advance);

  let allowed_boundaries = grapheme_cluster_boundaries_char_idx(boundary_text);
  let stops = crate::text::caret::caret_stops_for_runs(text, runs.as_ref(), total_advance);
  let Some(best) = stops
    .iter()
    .filter(|stop| is_grapheme_cluster_boundary(&allowed_boundaries, stop.char_idx))
    .filter(|stop| stop.x.is_finite())
    .min_by(|a, b| {
      let da = (local_x - a.x).abs();
      let db = (local_x - b.x).abs();
      da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    })
  else {
    return (0, CaretAffinity::Downstream);
  };

  (best.char_idx.min(char_count), best.affinity)
}

fn caret_index_for_text_control_point(
  index: &DomIndexMut,
  box_lookup: &impl BoxNodeLookup,
  fragment_tree: &FragmentTree,
  scroll: &ScrollState,
  node_id: usize,
  box_id: usize,
  page_point: Point,
) -> Option<(usize, CaretAffinity)> {
  let node = index.node(node_id)?;
  let box_node = box_lookup.node(box_id)?;
  let style = box_node.style.as_ref();

  let border_rect = fragment_rect_for_box_id(fragment_tree, box_id)?;
  let viewport_size = fragment_tree.viewport_size();
  let content_rect = content_rect_for_border_rect(border_rect, style, viewport_size);

  if is_textarea(node) {
    let value = textarea_value_for_editing(node);
    if value.is_empty() {
      return Some((0, CaretAffinity::Downstream));
    }

    let rect = inset_rect_uniform(content_rect, 2.0);
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
      return Some((0, CaretAffinity::Downstream));
    }

    let metrics = if matches!(style.line_height, crate::style::types::LineHeight::Normal) {
      super::resolve_scaled_metrics_for_interaction(style)
    } else {
      None
    };
    let line_height =
      compute_line_height_with_metrics_viewport(style, metrics.as_ref(), Some(viewport_size), None);
    if line_height <= 0.0 || !line_height.is_finite() {
      return Some((0, CaretAffinity::Downstream));
    }

    let mut scroll_y = scroll.element_offset(box_id).y;
    if !scroll_y.is_finite() {
      scroll_y = 0.0;
    }
    scroll_y = scroll_y.max(0.0);

    let chars_per_line = crate::textarea::textarea_chars_per_line(style, rect.width());
    let layout = crate::textarea::build_textarea_visual_lines(&value, chars_per_line);
    let content_height = layout.lines.len() as f32 * line_height;

    let mut local_y = page_point.y - rect.y() + scroll_y;
    if !local_y.is_finite() {
      local_y = 0.0;
    }
    local_y = local_y.clamp(0.0, content_height.max(0.0));

    let line_idx = ((local_y / line_height).floor() as isize).max(0) as usize;
    let line_idx = line_idx.min(layout.lines.len().saturating_sub(1));

    let line = layout
      .lines
      .get(line_idx)
      .copied()
      .unwrap_or(crate::textarea::TextareaVisualLine {
        start_char: 0,
        end_char: 0,
        start_byte: 0,
        end_byte: 0,
      });
    let caret_line = line.text(&value);
    let line_y = rect.y() + line_idx as f32 * line_height - scroll_y;
    let line_rect = Rect::from_xywh(rect.x(), line_y, rect.width(), line_height);
    let (caret_in_line, affinity) =
      caret_position_for_x_in_text(caret_line, caret_line, style, line_rect, page_point.x);

    let total_chars = value.chars().count();
    let caret = line
      .start_char
      .saturating_add(caret_in_line)
      .min(total_chars);
    return Some((caret, affinity));
  }

  if is_text_input(node) {
    let value = node.get_attribute_ref("value").unwrap_or("").to_string();
    if value.is_empty() {
      return Some((0, CaretAffinity::Downstream));
    }

    let input_type = input_type(node);
    let display_text = if input_type.eq_ignore_ascii_case("password") {
      // Mirror the painter's password masking: render bullet characters with the same length as the
      // underlying value.
      "•".repeat(value.chars().count())
    } else {
      value.clone()
    };

    let mut rect = inset_rect_uniform(content_rect, 2.0);

    // Mirror the painter's reserved affordance space for some input types (and honor
    // `appearance: none` which hides these affordances).
    let affordance_space = input_affordance_space(input_type, style);
    if affordance_space > 0.0 {
      rect = Rect::from_xywh(
        rect.x(),
        rect.y(),
        (rect.width() - affordance_space).max(0.0),
        rect.height(),
      );
    }

    let (caret, affinity) =
      caret_position_for_x_in_text(&display_text, &value, style, rect, page_point.x);
    let total_chars = value.chars().count();
    return Some((caret.min(total_chars), affinity));
  }

  None
}

pub(crate) fn box_is_selectable_for_document_selection(box_node: &BoxNode) -> bool {
  // Keep this aligned with `interaction::selection_serialize::box_is_selectable` so painting and
  // clipboard serialization see the same selectable content.
  let style = box_node.style.as_ref();
  if style.visibility != crate::style::computed::Visibility::Visible {
    return false;
  }
  if style.user_select == crate::style::types::UserSelect::None {
    return false;
  }
  if style.inert {
    return false;
  }
  true
}

fn document_selection_point_at_page_point(
  box_lookup: &impl BoxNodeLookup,
  fragment_tree: &FragmentTree,
  page_point: Point,
) -> Option<DocumentSelectionPoint> {
  document_selection_hit_at_page_point(box_lookup, fragment_tree, page_point)
    .map(|(point, _)| point)
}

fn document_selection_hit_at_page_point(
  box_lookup: &impl BoxNodeLookup,
  fragment_tree: &FragmentTree,
  page_point: Point,
) -> Option<(DocumentSelectionPoint, usize)> {
  let (root, path) = fragment_tree.hit_test_path(page_point)?;
  let mut node = match root {
    HitTestRoot::Root => &fragment_tree.root,
    HitTestRoot::Additional(idx) => fragment_tree.additional_fragments.get(idx)?,
  };

  let mut abs_origin = node.bounds.origin;
  for &child_idx in &path {
    let child = node.children.get(child_idx)?;
    abs_origin = abs_origin.translate(child.bounds.origin);
    node = child;
  }

  let FragmentContent::Text {
    text,
    box_id,
    source_range,
    shaped,
    is_marker,
    ..
  } = &node.content
  else {
    return None;
  };
  if *is_marker {
    return None;
  }
  let box_id = (*box_id)?;
  let source_range = (*source_range)?;

  let box_node = box_lookup.node(box_id)?;
  if !box_is_selectable_for_document_selection(box_node) {
    return None;
  }
  let node_id = box_node.styled_node_id?;

  let BoxType::Text(text_box) = &box_node.box_type else {
    return None;
  };

  let local_x = page_point.x - abs_origin.x;
  let runs: &[crate::text::pipeline::ShapedRun] =
    shaped.as_deref().map(|runs| runs.as_slice()).unwrap_or(&[]);
  let local_char = crate::text::caret::char_idx_for_x(text, runs, local_x);

  // Map fragment-local caret into the full text-node character index using the fragment's stable
  // source byte range.
  let start_byte = source_range.start().min(text_box.text.len());
  let mut start_char = 0usize;
  for (byte_idx, _) in text_box.text.char_indices() {
    if byte_idx >= start_byte {
      break;
    }
    start_char += 1;
  }
  let total_chars = text_box.text.chars().count();
  let char_offset = (start_char + local_char).min(total_chars);

  Some((
    DocumentSelectionPoint {
      node_id,
      char_offset,
    },
    box_id,
  ))
}

fn cmp_document_selection_points(
  a: DocumentSelectionPoint,
  b: DocumentSelectionPoint,
) -> std::cmp::Ordering {
  a.node_id
    .cmp(&b.node_id)
    .then_with(|| a.char_offset.cmp(&b.char_offset))
}

fn document_word_selection_range(
  box_tree: &BoxTree,
  text_box_id: usize,
  point: DocumentSelectionPoint,
) -> Option<DocumentSelectionRange> {
  #[derive(Clone, Copy)]
  struct SelectableTextBox<'a> {
    box_id: usize,
    node_id: usize,
    text: &'a str,
    len: usize,
    break_before: bool,
  }

  fn fallback_single_node(
    box_tree: &BoxTree,
    text_box_id: usize,
    point: DocumentSelectionPoint,
  ) -> Option<DocumentSelectionRange> {
    let box_node = box_node_by_id(box_tree, text_box_id)?;
    let node_id = box_node.styled_node_id.unwrap_or(point.node_id);
    let BoxType::Text(text_box) = &box_node.box_type else {
      return None;
    };
    let (start, end) = word_selection_range(&text_box.text, point.char_offset)?;
    let len = text_box.text.chars().count();
    let start = start.min(len);
    let end = end.min(len);
    if start >= end {
      return None;
    }
    Some(DocumentSelectionRange {
      start: DocumentSelectionPoint {
        node_id,
        char_offset: start,
      },
      end: DocumentSelectionPoint {
        node_id,
        char_offset: end,
      },
    })
  }

  let Some(block) = nearest_block_level_box_for_box_id(&box_tree.root, text_box_id) else {
    return fallback_single_node(box_tree, text_box_id, point);
  };

  // Collect selectable text boxes within the nearest block-level container, matching the same
  // traversal order used by selection serialization.
  let mut boxes: Vec<SelectableTextBox<'_>> = Vec::new();
  let mut pending_break = false;
  let mut stack: Vec<&BoxNode> = vec![block];
  while let Some(node) = stack.pop() {
    if !box_is_selectable_for_document_selection(node) {
      continue;
    }

    // Treat structural separators as hard boundaries so word selection doesn't span across them.
    // This approximates native browser behaviour for `<br>` and table serialization.
    if matches!(
      node.style.display,
      crate::style::display::Display::TableRow | crate::style::display::Display::TableCell
    ) && !boxes.is_empty()
    {
      pending_break = true;
    }

    if let BoxType::Text(text_box) = &node.box_type {
      if let Some(node_id) = node.styled_node_id {
        let len = text_box.text.chars().count();
        if len > 0 {
          boxes.push(SelectableTextBox {
            box_id: node.id,
            node_id,
            text: &text_box.text,
            len,
            break_before: pending_break && !boxes.is_empty(),
          });
          pending_break = false;
        }
      }
    } else if matches!(node.box_type, BoxType::LineBreak(_) | BoxType::Replaced(_)) && !boxes.is_empty()
    {
      pending_break = true;
    }

    // Mirror selection serialization traversal ordering:
    // visit `footnote_body` before normal children, and visit children left-to-right.
    for child in node.children.iter().rev() {
      stack.push(child);
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
  }

  let Some(clicked_idx) = boxes.iter().position(|entry| entry.box_id == text_box_id) else {
    return fallback_single_node(box_tree, text_box_id, point);
  };

  let clicked = boxes.get(clicked_idx)?;
  let local_caret = point.char_offset.min(clicked.len);
  let prefix_len: usize = boxes.iter().take(clicked_idx).map(|b| b.len).sum();

  // Materialize the concatenated text as a flat char vector so we can index it by character.
  let total_len: usize = boxes.iter().map(|b| b.len).sum();
  if total_len == 0 {
    return None;
  }
  let mut chars: Vec<char> = Vec::with_capacity(total_len);
  for entry in &boxes {
    chars.extend(entry.text.chars());
  }
  debug_assert_eq!(chars.len(), total_len);

  let mut break_indices: FxHashSet<usize> = FxHashSet::default();
  let mut acc = 0usize;
  for entry in &boxes {
    if entry.break_before {
      break_indices.insert(acc);
    }
    acc = acc.saturating_add(entry.len);
  }

  let caret = (prefix_len + local_caret).min(total_len);
  let hit = if caret == total_len { total_len - 1 } else { caret };
  let target_class = word_selection_class(*chars.get(hit)?);

  let mut start = hit;
  while start > 0
    && !break_indices.contains(&start)
    && word_selection_class(chars[start - 1]) == target_class
  {
    start -= 1;
  }
  let mut end = hit + 1;
  while end < total_len
    && !break_indices.contains(&end)
    && word_selection_class(chars[end]) == target_class
  {
    end += 1;
  }
  if start >= end {
    return None;
  }

  let start_point = {
    let mut acc = 0usize;
    let mut out = None;
    for entry in &boxes {
      let next = acc.saturating_add(entry.len);
      if start < next {
        out = Some(DocumentSelectionPoint {
          node_id: entry.node_id,
          char_offset: start.saturating_sub(acc).min(entry.len),
        });
        break;
      }
      acc = next;
    }
    out?
  };
  let end_point = {
    let mut acc = 0usize;
    let mut out = None;
    for entry in &boxes {
      let next = acc.saturating_add(entry.len);
      if end <= next {
        out = Some(DocumentSelectionPoint {
          node_id: entry.node_id,
          char_offset: end.saturating_sub(acc).min(entry.len),
        });
        break;
      }
      acc = next;
    }
    out?
  };
  if start_point == end_point {
    return None;
  }

  Some(DocumentSelectionRange {
    start: start_point,
    end: end_point,
  })
}

fn nearest_block_level_box_for_box_id<'a>(
  root: &'a BoxNode,
  target_box_id: usize,
) -> Option<&'a BoxNode> {
  struct Frame<'a> {
    node: &'a BoxNode,
    nearest_block: Option<&'a BoxNode>,
  }

  let mut stack = vec![Frame {
    node: root,
    nearest_block: None,
  }];

  while let Some(Frame {
    node,
    nearest_block,
  }) = stack.pop()
  {
    let nearest_block = if node.style.display.is_block_level() {
      Some(node)
    } else {
      nearest_block
    };

    if node.id == target_box_id {
      return nearest_block;
    }

    // Mirror `assign_box_ids` / selection serialization traversal ordering.
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(Frame {
        node: body,
        nearest_block,
      });
    }
    for child in node.children.iter().rev() {
      stack.push(Frame {
        node: child,
        nearest_block,
      });
    }
  }

  None
}

fn document_text_extents_in_box(block: &BoxNode) -> Option<DocumentSelectionRange> {
  let mut min: Option<DocumentSelectionPoint> = None;
  let mut max: Option<DocumentSelectionPoint> = None;

  let mut stack: Vec<&BoxNode> = vec![block];
  while let Some(node) = stack.pop() {
    if !box_is_selectable_for_document_selection(node) {
      continue;
    }

    if let BoxType::Text(text_box) = &node.box_type {
      if let Some(node_id) = node.styled_node_id {
        let len = text_box.text.chars().count();
        if len > 0 {
          let start = DocumentSelectionPoint {
            node_id,
            char_offset: 0,
          };
          let end = DocumentSelectionPoint {
            node_id,
            char_offset: len,
          };
          min = Some(match min {
            None => start,
            Some(existing) => {
              if cmp_document_selection_points(start, existing) == std::cmp::Ordering::Less {
                start
              } else {
                existing
              }
            }
          });
          max = Some(match max {
            None => end,
            Some(existing) => {
              if cmp_document_selection_points(end, existing) == std::cmp::Ordering::Greater {
                end
              } else {
                existing
              }
            }
          });
        }
      }
    }

    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  match (min, max) {
    (Some(start), Some(end)) if start != end => Some(DocumentSelectionRange { start, end }),
    _ => None,
  }
}

fn document_block_selection_range(
  box_tree: &BoxTree,
  text_box_id: usize,
  point: DocumentSelectionPoint,
) -> Option<DocumentSelectionRange> {
  if let Some(block) = nearest_block_level_box_for_box_id(&box_tree.root, text_box_id) {
    if let Some(range) = document_text_extents_in_box(block) {
      return Some(range.normalized());
    }
  }

  // Fallback: select the entire current text node.
  let box_node = box_node_by_id(box_tree, text_box_id)?;
  let node_id = box_node.styled_node_id.unwrap_or(point.node_id);
  let BoxType::Text(text_box) = &box_node.box_type else {
    return None;
  };
  let len = text_box.text.chars().count();
  if len == 0 {
    return None;
  }
  Some(DocumentSelectionRange {
    start: DocumentSelectionPoint {
      node_id,
      char_offset: 0,
    },
    end: DocumentSelectionPoint {
      node_id,
      char_offset: len,
    },
  })
}

fn cmp_document_selection_point(a: DocumentSelectionPoint, b: DocumentSelectionPoint) -> Ordering {
  a.node_id
    .cmp(&b.node_id)
    .then_with(|| a.char_offset.cmp(&b.char_offset))
}

fn document_selection_contains_point(
  selection: &DocumentSelectionState,
  point: DocumentSelectionPoint,
) -> bool {
  match selection {
    DocumentSelectionState::All => true,
    DocumentSelectionState::Ranges(ranges) => ranges.ranges.iter().any(|range| {
      // Collapsed ranges represent a caret without any selected text; starting a drag-drop from such
      // a point would be surprising when other ranges in the selection are highlighted.
      if range.start == range.end {
        return false;
      }
      // Allow starting a drag at either boundary. This is more forgiving than the half-open
      // selection model and better matches typical "click anywhere on the highlight" UX.
      cmp_document_selection_point(range.start, point) != Ordering::Greater
        && cmp_document_selection_point(point, range.end) != Ordering::Greater
    }),
  }
}

fn inferred_text_direction_from_dom(
  index: &DomIndexMut,
  mut node_id: usize,
) -> crate::style::types::Direction {
  while node_id != 0 {
    let Some(node) = index.node(node_id) else {
      break;
    };
    if let Some(dir) = node
      .get_attribute_ref("dir")
      .or_else(|| node.get_attribute_ref("xml:dir"))
    {
      let dir = trim_ascii_whitespace(dir);
      if dir.eq_ignore_ascii_case("rtl") {
        return crate::style::types::Direction::Rtl;
      }
      if dir.eq_ignore_ascii_case("ltr") {
        return crate::style::types::Direction::Ltr;
      }
      if dir.eq_ignore_ascii_case("auto") {
        if let Some(resolved) = crate::dom::resolve_first_strong_direction(node) {
          return match resolved {
            crate::css::selectors::TextDirection::Ltr => crate::style::types::Direction::Ltr,
            crate::css::selectors::TextDirection::Rtl => crate::style::types::Direction::Rtl,
          };
        }
      }
    }
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  crate::style::types::Direction::Ltr
}

fn collect_select_option_nodes_dom(index: &DomIndexMut, select_id: usize) -> Vec<(usize, bool)> {
  let mut end = select_id;
  for id in (select_id + 1)..index.id_to_node.len() {
    if is_ancestor_or_self(index, select_id, id) {
      end = id;
    } else {
      break;
    }
  }

  let mut options = Vec::new();
  for id in (select_id + 1)..=end {
    let Some(node) = index.node(id) else {
      continue;
    };
    if !node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"))
    {
      continue;
    }

    let mut disabled = node.get_attribute_ref("disabled").is_some();
    let mut ancestor = *index.parent.get(id).unwrap_or(&0);
    while ancestor != 0 && ancestor != select_id {
      if index.node(ancestor).is_some_and(|node| {
        node
          .tag_name()
          .is_some_and(|tag| tag.eq_ignore_ascii_case("optgroup"))
          && node.get_attribute_ref("disabled").is_some()
      }) {
        disabled = true;
        break;
      }
      ancestor = *index.parent.get(ancestor).unwrap_or(&0);
    }

    options.push((id, disabled));
  }

  options
}

fn fragment_rect_for_box_id_at_point(
  fragment_tree: &FragmentTree,
  page_point: Point,
  target_box_id: usize,
) -> Option<Rect> {
  struct Frame<'a> {
    node: &'a crate::tree::fragment_tree::FragmentNode,
    abs_origin: Point,
  }

  let mut stack: Vec<Frame<'_>> = Vec::new();
  stack.push(Frame {
    node: &fragment_tree.root,
    abs_origin: fragment_tree.root.bounds.origin,
  });
  for root in &fragment_tree.additional_fragments {
    stack.push(Frame {
      node: root,
      abs_origin: root.bounds.origin,
    });
  }

  while let Some(Frame { node, abs_origin }) = stack.pop() {
    let rect = Rect::from_xywh(
      abs_origin.x,
      abs_origin.y,
      node.bounds.width(),
      node.bounds.height(),
    );
    if rect.contains_point(page_point) && node.box_id() == Some(target_box_id) {
      return Some(rect);
    }

    for child in node.children.iter().rev() {
      stack.push(Frame {
        node: child,
        abs_origin: abs_origin.translate(child.bounds.origin),
      });
    }
  }

  None
}

fn fragment_rect_for_box_id(fragment_tree: &FragmentTree, target_box_id: usize) -> Option<Rect> {
  struct Frame<'a> {
    node: &'a crate::tree::fragment_tree::FragmentNode,
    abs_origin: Point,
  }

  let mut stack: Vec<Frame<'_>> = Vec::new();
  stack.push(Frame {
    node: &fragment_tree.root,
    abs_origin: fragment_tree.root.bounds.origin,
  });
  for root in &fragment_tree.additional_fragments {
    stack.push(Frame {
      node: root,
      abs_origin: root.bounds.origin,
    });
  }

  while let Some(Frame { node, abs_origin }) = stack.pop() {
    if node.box_id() == Some(target_box_id) {
      return Some(Rect::from_xywh(
        abs_origin.x,
        abs_origin.y,
        node.bounds.width(),
        node.bounds.height(),
      ));
    }

    for child in node.children.iter().rev() {
      stack.push(Frame {
        node: child,
        abs_origin: abs_origin.translate(child.bounds.origin),
      });
    }
  }

  None
}

fn update_range_value_from_pointer(
  index: &mut DomIndexMut,
  box_lookup: &impl BoxNodeLookup,
  fragment_tree: &FragmentTree,
  node_id: usize,
  box_id: usize,
  page_point: Point,
) -> bool {
  use crate::style::types::Direction;
  if node_or_ancestor_is_inert(index, node_id)
    || node_is_disabled(index, node_id)
    || node_is_readonly(index, node_id)
  {
    return false;
  }

  if !index.node(node_id).is_some_and(is_range_input) {
    return false;
  }

  let Some(rect) = fragment_rect_for_box_id(fragment_tree, box_id) else {
    return false;
  };

  let width = rect.width();
  if width <= 0.0 || !width.is_finite() {
    return false;
  }

  let mut fraction = (page_point.x - rect.x()) / width;
  if !fraction.is_finite() {
    return false;
  }
  fraction = fraction.clamp(0.0, 1.0);

  let dir = box_lookup
    .node(box_id)
    .map(|node| node.style.direction)
    .or_else(|| {
      // Fallback when box-tree metadata is unavailable: infer direction from `dir`/`xml:dir`
      // attributes, walking ancestors (HTML directionality inheritance).
      let mut current = node_id;
      while current != 0 {
        let Some(node) = index.node(current) else {
          break;
        };
        let attr = node
          .get_attribute_ref("dir")
          .or_else(|| node.get_attribute_ref("xml:dir"));
        let Some(value) = attr.map(trim_ascii_whitespace).filter(|v| !v.is_empty()) else {
          current = index.parent.get(current).copied().unwrap_or(0);
          continue;
        };
        if value.eq_ignore_ascii_case("rtl") {
          return Some(Direction::Rtl);
        }
        if value.eq_ignore_ascii_case("ltr") {
          return Some(Direction::Ltr);
        }
        if value.eq_ignore_ascii_case("auto") {
          if let Some(resolved) = crate::dom::resolve_first_strong_direction(node) {
            return Some(match resolved {
              crate::css::selectors::TextDirection::Ltr => Direction::Ltr,
              crate::css::selectors::TextDirection::Rtl => Direction::Rtl,
            });
          }
          // If the subtree has no strong characters, HTML falls back to the parent's direction.
          current = index.parent.get(current).copied().unwrap_or(0);
          continue;
        }
        current = index.parent.get(current).copied().unwrap_or(0);
      }
      None
    })
    .unwrap_or(Direction::Ltr);

  if dir == Direction::Rtl {
    // Painting mirrors the thumb in RTL; invert pointer mapping so the minimum value corresponds
    // to the right edge and the maximum to the left edge.
    fraction = 1.0 - fraction;
  }

  let Some(node_mut) = index.node_mut(node_id) else {
    return false;
  };
  dom_mutation::set_range_value_from_ratio(node_mut, fraction)
}

fn number_input_spin_direction_at_point(
  index: &DomIndexMut,
  box_lookup: &impl BoxNodeLookup,
  fragment_tree: &FragmentTree,
  node_id: usize,
  box_id: usize,
  page_point: Point,
) -> Option<NumberSpinDirection> {
  if !page_point.x.is_finite() || !page_point.y.is_finite() {
    return None;
  }
  let node = index.node(node_id)?;
  if !(is_input(node) && input_type(node).eq_ignore_ascii_case("number")) {
    return None;
  }

  let box_node = box_lookup.node(box_id)?;
  let style = box_node.style.as_ref();

  let border_rect = fragment_rect_for_box_id(fragment_tree, box_id)?;
  let viewport_size = fragment_tree.viewport_size();
  let content_rect = content_rect_for_border_rect(border_rect, style, viewport_size);
  let inner_rect = inset_rect_uniform(content_rect, 2.0);
  if inner_rect.width() <= 0.0 || inner_rect.height() <= 0.0 {
    return None;
  }

  let affordance_space = input_affordance_space("number", style);
  if affordance_space <= 0.0 || !affordance_space.is_finite() {
    return None;
  }

  let start_x = inner_rect.x() + (inner_rect.width() - affordance_space).max(0.0);
  let spinner_rect = Rect::from_xywh(
    start_x,
    inner_rect.y(),
    (inner_rect.max_x() - start_x).max(0.0),
    inner_rect.height(),
  );
  if spinner_rect.width() <= 0.0 || spinner_rect.height() <= 0.0 {
    return None;
  }
  if !spinner_rect.contains_point(page_point) {
    return None;
  }

  let half = spinner_rect.height() / 2.0;
  if !half.is_finite() || half <= 0.0 {
    return None;
  }

  if page_point.y < spinner_rect.y() + half {
    Some(NumberSpinDirection::Up)
  } else {
    Some(NumberSpinDirection::Down)
  }
}

fn apply_select_listbox_click(
  dom: &mut DomNode,
  fragment_tree: &FragmentTree,
  page_point: Point,
  select_id: usize,
  select_box_id: usize,
  scroll_state: &ScrollState,
  control: &SelectControl,
  style: &ComputedStyle,
  modifiers: PointerModifiers,
  select_listbox_anchor: &mut HashMap<usize, usize>,
) -> bool {
  let is_listbox = control.multiple || control.size > 1;
  if !is_listbox {
    return false;
  }

  let Some(select_rect) =
    fragment_rect_for_box_id_at_point(fragment_tree, page_point, select_box_id)
  else {
    return false;
  };

  let total_rows = control.items.len();
  if total_rows == 0 {
    return false;
  }

  let viewport_size = fragment_tree.viewport_size();
  let content_rect = content_rect_for_border_rect(select_rect, style, viewport_size);
  // Keep the click mapping consistent with the select listbox painter:
  // - base row height from `line-height` (mirroring paint-time `line-height: normal` resolution),
  // - but when the listbox is explicitly taller than its intrinsic size, stretch rows so exactly
  //   `size` rows fill the content rect (avoids dead whitespace and keeps tests deterministic).
  //
  // Only resolve full font metrics when needed. (This keeps listbox interaction fast in the
  // common case where `line-height` is numeric/absolute, while still handling `normal` accurately.)
  let metrics = if matches!(style.line_height, crate::style::types::LineHeight::Normal) {
    super::resolve_scaled_metrics_for_interaction(style)
  } else {
    None
  };
  let line_height =
    compute_line_height_with_metrics_viewport(style, metrics.as_ref(), Some(viewport_size), None);
  if line_height <= 0.0 || !line_height.is_finite() {
    return false;
  }

  let viewport_height = content_rect.height().max(0.0);
  let viewport_width = content_rect.width().max(0.0);
  let size_rows = control.size.max(1) as f32;
  let mut row_height = line_height;
  let stretched_row_height = viewport_height / size_rows;
  if stretched_row_height.is_finite() && stretched_row_height > row_height {
    row_height = stretched_row_height;
  }
  let content_height = row_height * total_rows as f32;
  if !viewport_height.is_finite() || !content_height.is_finite() {
    return false;
  }

  let max_scroll_y = (content_height - viewport_height).max(0.0);
  if !max_scroll_y.is_finite() {
    return false;
  }

  let mut scroll_y = scroll_state.element_offset(select_box_id).y;
  if !scroll_y.is_finite() {
    scroll_y = 0.0;
  }
  scroll_y = scroll_y.clamp(0.0, max_scroll_y);

  // Mirror the painter's behavior: when a vertical scrollbar is present, only the text area is
  // clickable (clicking the scrollbar itself should not select an option).
  let scrollbar_width = if max_scroll_y > 0.0 {
    crate::layout::utils::resolve_scrollbar_width(style).min(viewport_width)
  } else {
    0.0
  };
  let text_width = (viewport_width - scrollbar_width).max(0.0);

  let local_x = page_point.x - content_rect.x();
  if !local_x.is_finite() {
    return false;
  }
  if local_x < 0.0 || local_x >= text_width {
    return false;
  }

  let local_y = page_point.y - content_rect.y();
  if !local_y.is_finite() {
    return false;
  }
  // Only clicks within the listbox's scrollable viewport should map to an item row. Clicking in
  // border/padding (outside the content rect) should be a no-op.
  if local_y < 0.0 || local_y >= viewport_height {
    return false;
  }
  let content_y = local_y + scroll_y;
  if !content_y.is_finite() {
    return false;
  }
  // The painter draws rows only for `SelectControl.items`. If the click lands in the extra blank
  // area (e.g. `size` is larger than the number of items), do not clamp to the last row; treat it
  // as a no-op instead.
  if content_y < 0.0 || content_y >= content_height {
    return false;
  }
  let row_idx = (content_y / row_height).floor() as usize;

  let Some(item) = control.items.get(row_idx) else {
    return false;
  };

  match item {
    SelectItem::OptGroupLabel { .. } => false,
    SelectItem::Option {
      node_id, disabled, ..
    } => {
      if *disabled {
        return false;
      }

      // Native browser listbox semantics:
      // - Plain click replaces selection (even in `<select multiple>`).
      // - Ctrl/Cmd click toggles a single option (multiple-select only).
      // - Shift click range-selects from a stable anchor option.
      let clicked_option_id = *node_id;

      // Single-select listboxes always use replacement semantics.
      if !control.multiple {
        select_listbox_anchor.insert(select_id, clicked_option_id);
        return dom_mutation::activate_select_option(dom, select_id, clicked_option_id, false);
      }

      if modifiers.shift() {
        // Shift-click range selection for multiple-select listboxes.
        let stored_anchor = select_listbox_anchor.get(&select_id).copied();
        let anchor_option_id = stored_anchor.unwrap_or_else(|| {
          // Fallback when there is no stored anchor yet (e.g. first interaction is a shift click):
          // use the last selected option from the painted snapshot, falling back to the clicked
          // option.
          control
            .selected
            .iter()
            .rev()
            .filter_map(|&idx| match control.items.get(idx) {
              Some(SelectItem::Option {
                node_id, disabled, ..
              }) if !*disabled => Some(*node_id),
              _ => None,
            })
            .next()
            .unwrap_or(clicked_option_id)
        });

        // If this is the first time we've needed an anchor for this select, persist it so future
        // shift-clicks remain stable.
        if stored_anchor.is_none() {
          select_listbox_anchor.insert(select_id, anchor_option_id);
        }

        let anchor_idx = control
          .items
          .iter()
          .enumerate()
          .find_map(|(idx, item)| match item {
            SelectItem::Option { node_id, .. } if *node_id == anchor_option_id => Some(idx),
            _ => None,
          })
          .unwrap_or(row_idx);

        let (start_idx, end_idx) = if anchor_idx <= row_idx {
          (anchor_idx, row_idx)
        } else {
          (row_idx, anchor_idx)
        };

        let mut range_option_ids: Vec<usize> = Vec::new();
        for idx in start_idx..=end_idx {
          if let Some(SelectItem::Option {
            node_id, disabled, ..
          }) = control.items.get(idx)
          {
            if !*disabled {
              range_option_ids.push(*node_id);
            }
          }
        }

        // Shift-only replaces selection with the range. Ctrl/Cmd+Shift adds the range.
        let clear_others = !modifiers.command();
        return dom_mutation::set_select_selected_options(
          dom,
          select_id,
          &range_option_ids,
          clear_others,
        );
      }

      // Non-shift interactions update the range-selection anchor.
      select_listbox_anchor.insert(select_id, clicked_option_id);

      // Ctrl/Cmd toggles in multiple-select listboxes; plain click replaces.
      dom_mutation::activate_select_option(dom, select_id, clicked_option_id, modifiers.command())
    }
  }
}

fn select_control_snapshot_from_box_tree(
  box_tree: &BoxTree,
  select_node_id: usize,
) -> Option<(usize, SelectControl, bool, Arc<ComputedStyle>)> {
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.styled_node_id == Some(select_node_id) {
      if let BoxType::Replaced(replaced) = &node.box_type {
        if let ReplacedType::FormControl(form_control) = &replaced.replaced_type {
          if let FormControlKind::Select(control) = &form_control.control {
            return Some((
              node.id,
              control.clone(),
              form_control.disabled,
              node.style.clone(),
            ));
          }
        }
      }
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn select_control_snapshot_from_dom(
  index: &DomIndexMut,
  select_node_id: usize,
) -> Option<SelectControl> {
  let select_node = index.node(select_node_id)?;
  if !select_node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("select"))
  {
    return None;
  }

  fn inline_style_display_is_none(node: &DomNode) -> Option<bool> {
    let style = node.get_attribute_ref("style")?;
    let mut display_is_none: Option<bool> = None;
    for decl in style.split(';') {
      let Some((name, value)) = decl.split_once(':') else {
        continue;
      };
      if !trim_ascii_whitespace(name).eq_ignore_ascii_case("display") {
        continue;
      }
      let token = trim_ascii_whitespace(value)
        .split(|c: char| c.is_ascii_whitespace() || c == '!' || c == ';')
        .next()
        .unwrap_or("");
      if token.is_empty() {
        continue;
      }
      // Inline style declarations follow standard CSS rules: later declarations override earlier ones.
      display_is_none = Some(token.eq_ignore_ascii_case("none"));
    }
    display_is_none
  }

  fn node_hidden_for_select(node: &DomNode) -> bool {
    // Inline `style="display: ..."` wins over the boolean `[hidden]` attribute. This keeps
    // `<option hidden style="display:block">` consistent with the computed `display` used by the
    // rendering pipeline.
    if let Some(is_none) = inline_style_display_is_none(node) {
      return is_none;
    }
    node.get_attribute_ref("hidden").is_some()
      || node
        .get_attribute_ref("data-fastr-hidden")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
  }

  fn node_or_ancestor_hidden_for_select(
    index: &DomIndexMut,
    mut node_id: usize,
    select_id: usize,
  ) -> bool {
    while node_id != 0 {
      let Some(node) = index.node(node_id) else {
        break;
      };
      if node_hidden_for_select(node) {
        return true;
      }
      if node_id == select_id {
        break;
      }
      node_id = *index.parent.get(node_id).unwrap_or(&0);
    }
    false
  }

  fn collect_descendant_text_content(node: &DomNode) -> String {
    let mut text = String::new();
    let mut stack: Vec<&DomNode> = vec![node];
    while let Some(node) = stack.pop() {
      match &node.node_type {
        DomNodeType::Text { content } => text.push_str(content),
        DomNodeType::Element {
          tag_name,
          namespace,
          ..
        } => {
          if tag_name.eq_ignore_ascii_case("script")
            && (namespace.is_empty()
              || namespace == crate::dom::HTML_NAMESPACE
              || namespace == crate::dom::SVG_NAMESPACE)
          {
            continue;
          }
        }
        _ => {}
      }
      for child in node.children.iter().rev() {
        if matches!(child.node_type, DomNodeType::ShadowRoot { .. }) {
          continue;
        }
        stack.push(child);
      }
    }
    text
  }

  fn option_text(node: &DomNode) -> String {
    crate::dom::strip_and_collapse_ascii_whitespace(&collect_descendant_text_content(node))
  }

  fn option_label(node: &DomNode) -> String {
    if let Some(label) = node
      .get_attribute_ref("label")
      .filter(|label| !label.is_empty())
    {
      return label.to_string();
    }
    option_text(node)
  }

  fn option_value(node: &DomNode) -> String {
    if let Some(value) = node.get_attribute_ref("value") {
      return value.to_string();
    }
    option_text(node)
  }

  let multiple = select_node.get_attribute_ref("multiple").is_some();
  let size = crate::dom::select_effective_size(select_node);

  // Pre-order traversal ids form contiguous ranges, so the select subtree is `[select_id, end]`.
  let mut end = select_node_id;
  for id in (select_node_id + 1)..index.id_to_node.len() {
    if is_ancestor_or_self(index, select_node_id, id) {
      end = id;
    } else {
      break;
    }
  }

  let mut items: Vec<SelectItem> = Vec::new();
  let mut option_item_indices: Vec<usize> = Vec::new();

  for id in (select_node_id + 1)..=end {
    let Some(node) = index.node(id) else {
      continue;
    };
    if node_or_ancestor_hidden_for_select(index, id, select_node_id) {
      continue;
    }
    let Some(tag) = node.tag_name() else {
      continue;
    };

    if tag.eq_ignore_ascii_case("option") {
      let mut in_optgroup = false;
      let mut optgroup_disabled = false;
      let mut ancestor = *index.parent.get(id).unwrap_or(&0);
      while ancestor != 0 && ancestor != select_node_id {
        if index.node(ancestor).is_some_and(|node| {
          node
            .tag_name()
            .is_some_and(|tag| tag.eq_ignore_ascii_case("optgroup"))
        }) {
          in_optgroup = true;
          if index
            .node(ancestor)
            .and_then(|node| node.get_attribute_ref("disabled"))
            .is_some()
          {
            optgroup_disabled = true;
          }
        }
        ancestor = *index.parent.get(ancestor).unwrap_or(&0);
      }

      let disabled = optgroup_disabled || node.get_attribute_ref("disabled").is_some();
      let idx = items.len();
      items.push(SelectItem::Option {
        node_id: id,
        label: option_label(node),
        value: option_value(node),
        selected: node.get_attribute_ref("selected").is_some(),
        disabled,
        in_optgroup,
      });
      option_item_indices.push(idx);
      continue;
    }

    if tag.eq_ignore_ascii_case("optgroup") {
      let mut disabled = node.get_attribute_ref("disabled").is_some();
      let mut ancestor = *index.parent.get(id).unwrap_or(&0);
      while !disabled && ancestor != 0 && ancestor != select_node_id {
        if index.node(ancestor).is_some_and(|node| {
          node
            .tag_name()
            .is_some_and(|tag| tag.eq_ignore_ascii_case("optgroup"))
            && node.get_attribute_ref("disabled").is_some()
        }) {
          disabled = true;
          break;
        }
        ancestor = *index.parent.get(ancestor).unwrap_or(&0);
      }

      let label = node
        .get_attribute_ref("label")
        .map(|label| label.to_string())
        .unwrap_or_default();
      items.push(SelectItem::OptGroupLabel { label, disabled });
    }
  }

  let mut selected: Vec<usize> = Vec::new();
  if multiple {
    for &idx in option_item_indices.iter() {
      if let SelectItem::Option {
        selected: is_selected,
        ..
      } = &items[idx]
      {
        if *is_selected {
          selected.push(idx);
        }
      }
    }
  } else {
    let mut chosen: Option<usize> = None;
    for &idx in option_item_indices.iter() {
      if let SelectItem::Option {
        selected: is_selected,
        ..
      } = &items[idx]
      {
        if *is_selected {
          chosen = Some(idx);
        }
      }
    }

    if chosen.is_none() {
      for &idx in option_item_indices.iter() {
        if let SelectItem::Option { disabled, .. } = &items[idx] {
          if !*disabled {
            chosen = Some(idx);
            break;
          }
        }
      }
    }

    if chosen.is_none() {
      chosen = option_item_indices.first().copied();
    }

    for &idx in option_item_indices.iter() {
      if let Some(SelectItem::Option { selected, .. }) = items.get_mut(idx) {
        *selected = Some(idx) == chosen;
      }
    }

    if let Some(chosen) = chosen {
      selected.push(chosen);
    }
  }

  Some(SelectControl {
    multiple,
    size,
    items: Arc::new(items),
    selected,
  })
}

fn textarea_control_snapshot_from_box_tree(
  box_tree: &BoxTree,
  textarea_node_id: usize,
) -> Option<(usize, Arc<ComputedStyle>)> {
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.styled_node_id == Some(textarea_node_id) {
      if let BoxType::Replaced(replaced) = &node.box_type {
        if let ReplacedType::FormControl(form_control) = &replaced.replaced_type {
          if matches!(form_control.control, FormControlKind::TextArea { .. }) {
            return Some((node.id, node.style.clone()));
          }
        }
      }
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn style_for_styled_node_id(
  box_tree: &BoxTree,
  styled_node_id: usize,
) -> Option<Arc<ComputedStyle>> {
  // Multiple box nodes can map back to the same styled node id (e.g. anonymous wrappers,
  // fragmentation, etc). For interaction purposes we only need a representative computed style
  // (currently just text direction for caret movement), so prefer the first non-pseudo box style.
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  let mut fallback: Option<Arc<ComputedStyle>> = None;
  while let Some(node) = stack.pop() {
    if node.styled_node_id == Some(styled_node_id) {
      if node.generated_pseudo.is_none() {
        return Some(Arc::clone(&node.style));
      }
      if fallback.is_none() {
        fallback = Some(Arc::clone(&node.style));
      }
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  fallback
}

fn find_ancestor_form(index: &DomIndexMut, mut node_id: usize) -> Option<usize> {
  while node_id != 0 {
    let node = index.node(node_id)?;
    if is_form(node) {
      return Some(node_id);
    }
    // Shadow roots are tree root boundaries for form owner resolution; do not walk out into the
    // shadow host tree.
    if matches!(
      node.node_type,
      DomNodeType::ShadowRoot { .. } | DomNodeType::Document { .. }
    ) {
      break;
    }
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  None
}

fn tree_root_boundary_id(index: &DomIndexMut, mut node_id: usize) -> Option<usize> {
  while node_id != 0 {
    let node = index.node(node_id)?;
    if matches!(
      node.node_type,
      DomNodeType::Document { .. } | DomNodeType::ShadowRoot { .. }
    ) {
      return Some(node_id);
    }
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  None
}

fn node_or_ancestor_is_template(index: &DomIndexMut, mut node_id: usize) -> bool {
  while node_id != 0 {
    let Some(node) = index.node(node_id) else {
      return false;
    };
    if node.template_contents_are_inert() {
      return true;
    }
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  false
}

fn find_element_by_id_attr_in_tree(
  index: &DomIndexMut,
  tree_root_id: usize,
  html_id: &str,
) -> Option<usize> {
  for node_id in 1..index.id_to_node.len() {
    let Some(node) = index.node(node_id) else {
      continue;
    };
    if !node.is_element() {
      continue;
    }
    if node_or_ancestor_is_template(index, node_id) {
      continue;
    }
    if node.get_attribute_ref("id") != Some(html_id) {
      continue;
    }
    if tree_root_boundary_id(index, node_id) == Some(tree_root_id) {
      return Some(node_id);
    }
  }
  None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DatalistOption {
  pub value: String,
  pub label: String,
  pub disabled: bool,
}

/// A `<datalist>` `<option>` along with its stable pre-order DOM node id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DatalistOptionEntry {
  pub node_id: usize,
  pub option: DatalistOption,
}

fn is_html_datalist(node: &DomNode) -> bool {
  if !node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("datalist"))
  {
    return false;
  }
  matches!(
    node.namespace(),
    Some(ns) if ns.is_empty() || ns == crate::dom::HTML_NAMESPACE
  )
}

fn resolve_associated_datalist_in_index(index: &DomIndexMut, input_node_id: usize) -> Option<usize> {
  let input = index.node(input_node_id)?;
  if !is_input(input) {
    return None;
  }

  let list_attr = input
    .get_attribute_ref("list")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())?;

  // Spec-ish: `list` matches element IDs in the same tree (tree-root boundary, i.e. the document
  // or current shadow root).
  let tree_root = tree_root_boundary_id(index, input_node_id)?;
  let referenced = find_element_by_id_attr_in_tree(index, tree_root, list_attr)?;
  index.node(referenced).is_some_and(is_html_datalist).then_some(referenced)
}

/// Resolve the `<datalist>` element associated with an `<input list="...">`.
///
/// This matches `list` by ID within the same tree-root boundary (document or shadow root) and
/// ignores `id` values inside inert `<template>` contents.
pub(crate) fn resolve_associated_datalist(dom: &mut DomNode, input_node_id: usize) -> Option<usize> {
  let index = DomIndexMut::new(dom);
  resolve_associated_datalist_in_index(&index, input_node_id)
}

fn collect_descendant_text_content_excluding_script_and_shadow_roots(node: &DomNode) -> String {
  let mut text = String::new();
  let mut stack: Vec<&DomNode> = vec![node];
  while let Some(node) = stack.pop() {
    match &node.node_type {
      DomNodeType::Text { content } => text.push_str(content),
      DomNodeType::Element {
        tag_name,
        namespace,
        ..
      } => {
        if tag_name.eq_ignore_ascii_case("script")
          && (namespace.is_empty()
            || namespace == crate::dom::HTML_NAMESPACE
            || namespace == crate::dom::SVG_NAMESPACE)
        {
          continue;
        }
      }
      _ => {}
    }
    for child in node.children.iter().rev() {
      // Mirror select option text extraction: ignore shadow root subtrees.
      if matches!(child.node_type, DomNodeType::ShadowRoot { .. }) {
        continue;
      }
      stack.push(child);
    }
  }
  text
}

fn datalist_option_text(node: &DomNode) -> String {
  crate::dom::strip_and_collapse_ascii_whitespace(
    &collect_descendant_text_content_excluding_script_and_shadow_roots(node),
  )
}

fn collect_datalist_option_entries_in_index(
  index: &DomIndexMut,
  datalist_node_id: usize,
) -> Vec<DatalistOptionEntry> {
  let Some(datalist) = index.node(datalist_node_id) else {
    return Vec::new();
  };
  if !is_html_datalist(datalist) {
    return Vec::new();
  }
  if node_or_ancestor_is_template(index, datalist_node_id) {
    return Vec::new();
  }

  let Some(datalist_tree_root) = tree_root_boundary_id(index, datalist_node_id) else {
    return Vec::new();
  };

  // Pre-order traversal ids form contiguous ranges, so the datalist subtree is `[datalist_id, end]`.
  let mut end = datalist_node_id;
  for id in (datalist_node_id + 1)..index.id_to_node.len() {
    if is_ancestor_or_self(index, datalist_node_id, id) {
      end = id;
    } else {
      break;
    }
  }

  let mut options: Vec<DatalistOptionEntry> = Vec::new();
  for id in (datalist_node_id + 1)..=end {
    let Some(node) = index.node(id) else {
      continue;
    };
    if !node.is_element() {
      continue;
    }

    // Skip any nodes that live in a nested shadow root subtree.
    if tree_root_boundary_id(index, id) != Some(datalist_tree_root) {
      continue;
    }

    if node_or_ancestor_is_template(index, id) {
      continue;
    }

    let Some(tag) = node.tag_name() else {
      continue;
    };
    if !tag.eq_ignore_ascii_case("option") {
      continue;
    }

    let text = datalist_option_text(node);
    let value = if let Some(value) = node.get_attribute_ref("value") {
      value.to_string()
    } else {
      text.clone()
    };
    let label = if let Some(label) = node
      .get_attribute_ref("label")
      .filter(|label| !label.is_empty())
    {
      label.to_string()
    } else {
      text
    };
    let disabled = node.get_attribute_ref("disabled").is_some();

    options.push(DatalistOptionEntry {
      node_id: id,
      option: DatalistOption {
        value,
        label,
        disabled,
      },
    });
  }

  options
}

fn collect_datalist_options_in_index(index: &DomIndexMut, datalist_node_id: usize) -> Vec<DatalistOption> {
  collect_datalist_option_entries_in_index(index, datalist_node_id)
    .into_iter()
    .map(|entry| entry.option)
    .collect()
}

/// Collect all `<option>` descendants of a `<datalist>` in DOM (pre-order) order, returning the
/// option element's node id along with extracted value/label metadata.
///
/// Options inside inert `<template>` contents are ignored. Descendant text used for value/label
/// fallback ignores `<script>` and does not cross shadow-root boundaries.
pub(crate) fn collect_datalist_option_entries(
  dom: &mut DomNode,
  datalist_node_id: usize,
) -> Vec<DatalistOptionEntry> {
  let index = DomIndexMut::new(dom);
  collect_datalist_option_entries_in_index(&index, datalist_node_id)
}

/// Collect all `<option>` descendants of a `<datalist>` in DOM (pre-order) order.
///
/// Options inside inert `<template>` contents are ignored. Descendant text used for value/label
/// fallback ignores `<script>` and does not cross shadow-root boundaries.
pub(crate) fn collect_datalist_options(dom: &mut DomNode, datalist_node_id: usize) -> Vec<DatalistOption> {
  let index = DomIndexMut::new(dom);
  collect_datalist_options_in_index(&index, datalist_node_id)
}

fn ascii_case_insensitive_starts_with(haystack: &str, prefix: &str) -> bool {
  if prefix.is_empty() {
    return true;
  }
  haystack
    .as_bytes()
    .get(..prefix.len())
    .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// Filter function used to match `<datalist>` options for an `<input>` value.
///
/// Matching datalist suggestions is UA-defined; we use a simple prefix match on `value` or `label`,
/// ASCII case-insensitively.
pub(crate) fn datalist_option_matches_input_value(option: &DatalistOption, input_value: &str) -> bool {
  ascii_case_insensitive_starts_with(&option.value, input_value)
    || ascii_case_insensitive_starts_with(&option.label, input_value)
}

fn resolve_form_owner(index: &DomIndexMut, control_node_id: usize) -> Option<usize> {
  let control = index.node(control_node_id)?;

  if let Some(form_attr) = control
    .get_attribute_ref("form")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
  {
    let tree_root = tree_root_boundary_id(index, control_node_id)?;
    let referenced = find_element_by_id_attr_in_tree(index, tree_root, form_attr)?;
    return index
      .node(referenced)
      .is_some_and(is_form)
      .then_some(referenced);
  }

  find_ancestor_form(index, control_node_id)
}

fn find_ancestor_select(index: &DomIndexMut, mut node_id: usize) -> Option<usize> {
  while node_id != 0 {
    let node = index.node(node_id)?;
    if is_select(node) {
      return Some(node_id);
    }
    // Shadow roots are tree root boundaries for select/option relationships; do not walk out into
    // the shadow host tree.
    if matches!(
      node.node_type,
      DomNodeType::ShadowRoot { .. } | DomNodeType::Document { .. }
    ) {
      break;
    }
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  None
}

fn submission_target_is_blank(
  index: &DomIndexMut,
  submitter_id: Option<usize>,
  form_id: usize,
) -> bool {
  if let Some(submitter_id) = submitter_id {
    if let Some(submitter) = index.node(submitter_id) {
      // `formtarget` on the submitter overrides the form's `target` (even when empty/invalid).
      if let Some(target) = submitter.get_attribute_ref("formtarget") {
        return trim_ascii_whitespace(target).eq_ignore_ascii_case("_blank");
      }
    }
  }

  index
    .node(form_id)
    .and_then(|form| form.get_attribute_ref("target"))
    .is_some_and(|target| trim_ascii_whitespace(target).eq_ignore_ascii_case("_blank"))
}

// `SelectControl` uses Strings/Vecs and does not contain floats, so its derived `PartialEq` is a
// full equivalence relation. Mark it as `Eq` so interaction actions can remain `Eq` as well.
impl Eq for SelectControl {}

fn find_default_form_submitter(index: &DomIndexMut, form_id: usize) -> Option<usize> {
  // Spec-ish: choose the first submit control in tree order whose form owner matches `form_id`.
  // This is used for implicit submission (Enter in a text input).
  for node_id in 1..index.id_to_node.len() {
    let Some(node) = index.node(node_id) else {
      continue;
    };
    if !node.is_element() {
      continue;
    }
    if !is_submit_control(node) {
      continue;
    }
    if resolve_form_owner(index, node_id) != Some(form_id) {
      continue;
    }
    if is_disabled_or_inert(index, node_id) {
      continue;
    }
    return Some(node_id);
  }
  None
}

fn apply_select_keyboard_action(
  dom: &mut DomNode,
  index: &DomIndexMut,
  select_id: usize,
  key: KeyAction,
) -> bool {
  if node_or_ancestor_is_inert(index, select_id) || node_is_disabled(index, select_id) {
    return false;
  }

  let options = collect_select_option_nodes_dom(index, select_id);
  if options.is_empty() {
    return false;
  }

  let mut last_selected_idx: Option<usize> = None;
  let mut first_enabled_idx: Option<usize> = None;
  let mut last_enabled_idx: Option<usize> = None;

  for (idx, (node_id, disabled)) in options.iter().enumerate() {
    if index
      .node(*node_id)
      .and_then(|node| node.get_attribute_ref("selected"))
      .is_some()
    {
      last_selected_idx = Some(idx);
    }

    if !*disabled {
      if first_enabled_idx.is_none() {
        first_enabled_idx = Some(idx);
      }
      last_enabled_idx = Some(idx);
    }
  }

  let Some(first_enabled_idx) = first_enabled_idx else {
    return false;
  };
  let last_enabled_idx = last_enabled_idx.unwrap_or(first_enabled_idx);

  // Selection anchor: last `<option selected>` in tree order; fallback to first enabled option.
  let anchor_idx = last_selected_idx.unwrap_or(first_enabled_idx);

  let next_idx = match key {
    KeyAction::ArrowDown => {
      let mut found = None;
      for idx in (anchor_idx + 1)..options.len() {
        if !options[idx].1 {
          found = Some(idx);
          break;
        }
      }
      found.unwrap_or(last_enabled_idx)
    }
    KeyAction::ArrowUp => {
      let mut found = None;
      for idx in (0..anchor_idx).rev() {
        if !options[idx].1 {
          found = Some(idx);
          break;
        }
      }
      found.unwrap_or(first_enabled_idx)
    }
    KeyAction::Home => first_enabled_idx,
    KeyAction::End => last_enabled_idx,
    _ => anchor_idx,
  };

  let option_id = options
    .get(next_idx)
    .copied()
    .map(|(node_id, _)| node_id)
    .or_else(|| options.get(first_enabled_idx).copied().map(|(node_id, _)| node_id));
  let Some(option_id) = option_id else {
    return false;
  };

  dom_mutation::activate_select_option(dom, select_id, option_id, false)
}
impl InteractionEngine {
  pub fn new() -> Self {
    Self {
      state: InteractionState::default(),
      hover_tooltip: None,
      pointer_down_target: None,
      link_drag: None,
      range_drag: None,
      number_spin: None,
      text_drag: None,
      text_drag_drop: None,
      document_drag: None,
      document_selection_drag_drop: None,
      pending_text_drop_move: None,
      text_edit: None,
      text_undo: HashMap::new(),
      form_default_snapshots: HashMap::new(),
      select_listbox_anchor: HashMap::new(),
      modality: InputModality::Pointer,
      last_click_target: None,
      last_click_target_element_id: None,
      last_form_submitter: None,
      last_form_submitter_element_id: None,
    }
  }

  pub fn interaction_state(&self) -> &InteractionState {
    &self.state
  }

  pub fn hover_tooltip(&self) -> Option<&str> {
    self.hover_tooltip.as_deref()
  }
  /// Replace the set of visited link node ids for the current document.
  ///
  /// This is used by browser-UI integrations to populate `:visited` pseudo-class matching from an
  /// external per-tab visited URL store without mutating the DOM.
  pub fn set_visited_links(&mut self, visited_links: FxHashSet<usize>) {
    *self.state.visited_links_mut() = visited_links;
  }

  /// Return the kind of *active* drag-and-drop gesture, if any.
  ///
  /// Drag-and-drop gestures begin as "candidates" when the pointer is pressed inside an existing
  /// selection highlight, but only become active after the pointer crosses the drag threshold and a
  /// payload has been captured.
  pub fn drag_drop_active_kind(&self) -> Option<DragDropKind> {
    if self
      .text_drag_drop
      .as_ref()
      .is_some_and(|state| matches!(state, TextDragDropState::Active(_)))
    {
      return Some(DragDropKind::TextSelection);
    }
    if self
      .document_selection_drag_drop
      .as_ref()
      .is_some_and(|state| state.payload.is_some())
    {
      return Some(DragDropKind::DocumentSelection);
    }
    None
  }

  /// Returns true while the user is extending a document selection via a pointer drag.
  ///
  /// This tracks "click and drag to select text" gestures in normal document content (outside of
  /// form controls).
  ///
  /// The drag state is cleared on pointer up, focus changes, and `clear_pointer_state`.
  pub fn active_document_selection_drag(&self) -> bool {
    self.document_drag.is_some()
  }

  pub fn drag_cursor_hint(
    &self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    scroll: &ScrollState,
    page_point: Point,
    hit: Option<&HitTestResult>,
  ) -> Option<CursorKind> {
    if self.drag_drop_active_kind().is_some() {
      return None;
    }

    // Document selection highlight can be dragged.
    if page_point.x.is_finite()
      && page_point.y.is_finite()
      && page_point.x >= 0.0
      && page_point.y >= 0.0
    {
      if let Some(selection) = self
        .state
        .document_selection
        .as_ref()
        .filter(|sel| sel.has_highlight())
      {
        if let Some(point) =
          document_selection_point_at_page_point(box_tree, fragment_tree, page_point)
        {
          if document_selection_contains_point(selection, point) {
            return Some(CursorKind::Grab);
          }
        }
      }
    }

    // Focused text-control selection highlight can be dragged.
    let Some(edit) = self.text_edit.as_ref() else {
      return None;
    };
    let Some((sel_start, sel_end)) = edit.selection() else {
      return None;
    };
    let Some(hit) = hit
      .filter(|hit| hit.kind == HitTestKind::FormControl && hit.dom_node_id == edit.node_id)
    else {
      return None;
    };

    let index = DomIndexMut::new(dom);
    let Some((caret, _)) = caret_index_for_text_control_point(
      &index,
      box_tree,
      fragment_tree,
      scroll,
      hit.dom_node_id,
      hit.box_id,
      page_point,
    ) else {
      return None;
    };

    if caret >= sel_start && caret <= sel_end {
      Some(CursorKind::Grab)
    } else {
      None
    }
  }

  /// Debug/test helper: validate internal interaction invariants.
  ///
  /// This is intentionally a no-op in non-debug builds so it can be called liberally by fuzz-like
  /// harnesses without impacting release binaries.
  #[allow(unused_variables)]
  pub fn assert_invariants(&self, dom: &mut DomNode, scroll: &ScrollState) {
    #[cfg(any(debug_assertions, test))]
    self.assert_invariants_debug(dom, scroll);
  }

  #[cfg(any(debug_assertions, test))]
  fn assert_invariants_debug(&self, dom: &mut DomNode, scroll: &ScrollState) {
    let index = DomIndexMut::new(dom);
    let max_node_id = index.id_to_node.len().saturating_sub(1);

    let check_node_id = |label: &str, node_id: usize| {
      debug_assert!(
        node_id > 0 && node_id <= max_node_id,
        "{label} node id {node_id} out of range (max={max_node_id})"
      );
      debug_assert!(
        index.node(node_id).is_some_and(DomNode::is_element),
        "{label} node id {node_id} does not refer to a live element"
      );
    };

    if let Some(focused) = self.state.focused {
      check_node_id("focused", focused);
    }

    for &id in self.state.focus_chain() {
      check_node_id("focus_chain", id);
    }
    for &id in self.state.hover_chain() {
      check_node_id("hover_chain", id);
    }
    for &id in self.state.active_chain() {
      check_node_id("active_chain", id);
    }
    self.state.debug_assert_chain_caches_consistent();
    for (&id, _) in &self.state.form_state().file_inputs {
      check_node_id("file_inputs", id);
    }

    if let Some(id) = self.pointer_down_target {
      check_node_id("pointer_down_target", id);
    }
    if let Some(state) = self.link_drag {
      check_node_id("link_drag", state.node_id);
    }
    if let Some(state) = self.range_drag {
      check_node_id("range_drag", state.node_id);
    }
    if let Some(state) = self.text_drag {
      check_node_id("text_drag", state.node_id);
    }
    if let Some(state) = &self.text_drag_drop {
      check_node_id("text_drag_drop", state.node_id());
      if let Some(focus_before) = state.focus_before() {
        check_node_id("text_drag_drop.focus_before", focus_before);
      }
    }
    if let Some(id) = self.last_click_target {
      check_node_id("last_click_target", id);
    }
    if let Some(id) = self.last_form_submitter {
      check_node_id("last_form_submitter", id);
    }
    for (&select_id, &option_id) in &self.select_listbox_anchor {
      check_node_id("select_listbox_anchor.select", select_id);
      check_node_id("select_listbox_anchor.option", option_id);
    }
    if let Some(ime) = &self.state.ime_preedit {
      check_node_id("ime_preedit", ime.node_id);
    }

    let text_control_len = |node_id: usize| -> usize {
      let Some(node) = index.node(node_id) else {
        return 0;
      };
      if is_textarea(node) {
        textarea_value_for_editing(node).chars().count()
      } else {
        node
          .get_attribute_ref("value")
          .unwrap_or("")
          .chars()
          .count()
      }
    };

    if let Some(edit) = &self.text_edit {
      check_node_id("text_edit", edit.node_id);
      debug_assert_eq!(
        self.state.focused,
        Some(edit.node_id),
        "text_edit must track the focused node"
      );
      let len = text_control_len(edit.node_id);
      debug_assert!(
        edit.caret <= len,
        "text_edit caret {} out of bounds for length {len}",
        edit.caret
      );
      if let Some(anchor) = edit.selection_anchor {
        debug_assert!(
          anchor <= len,
          "text_edit selection anchor {anchor} out of bounds for length {len}"
        );
      }
    }

    if let Some(paint) = self.state.text_edit {
      check_node_id("text_edit_paint_state", paint.node_id);
      debug_assert_eq!(
        self.state.focused,
        Some(paint.node_id),
        "text_edit paint state must track the focused node"
      );
      let len = text_control_len(paint.node_id);
      debug_assert!(
        paint.caret <= len,
        "paint caret {} out of bounds for length {len}",
        paint.caret
      );
      if let Some((start, end)) = paint.selection {
        debug_assert!(
          start < end,
          "paint selection must be normalized (start < end), got ({start}, {end})"
        );
        debug_assert!(
          end <= len,
          "paint selection end {end} out of bounds for length {len}"
        );
      }
    }

    let check_point = |label: &str, p: Point| {
      debug_assert!(
        p.x.is_finite() && p.y.is_finite(),
        "{label} contains non-finite values ({}, {})",
        p.x,
        p.y
      );
    };
    check_point("scroll.viewport", scroll.viewport);
    check_point("scroll.viewport_delta", scroll.viewport_delta);
    for (&box_id, &offset) in &scroll.elements {
      check_point(&format!("scroll.elements[{box_id}]"), offset);
    }
    for (&box_id, &delta) in &scroll.elements_delta {
      check_point(&format!("scroll.elements_delta[{box_id}]"), delta);
    }
  }

  fn mark_user_validity(&mut self, node_id: usize) -> bool {
    self.state.insert_user_validity(node_id)
  }

  fn record_text_undo_snapshot(&mut self, node_id: usize, value: &str, edit: &TextEditState) {
    let entry = TextUndoEntry {
      value: value.to_string(),
      caret: edit.caret,
      caret_affinity: edit.caret_affinity,
      selection_anchor: edit.selection_anchor,
    };
    let history = self.text_undo.entry(node_id).or_default();
    if history.undo.last().is_some_and(|prev| prev == &entry) {
      history.clear_redo();
      return;
    }
    history.clear_redo();
    history.push_undo(entry);
  }

  fn mark_form_user_validity(&mut self, index: &DomIndexMut, control_node_id: usize) -> bool {
    resolve_form_owner(index, control_node_id)
      .is_some_and(|form_id| self.mark_user_validity(form_id))
  }

  fn ensure_form_default_snapshot_for_control(
    &mut self,
    index: &DomIndexMut,
    control_node_id: usize,
  ) {
    let Some(form_id) = resolve_form_owner(index, control_node_id) else {
      return;
    };
    self.ensure_form_default_snapshot_for_form(index, form_id);
  }

  fn ensure_form_default_snapshot_for_form(&mut self, index: &DomIndexMut, form_id: usize) {
    if self.form_default_snapshots.contains_key(&form_id) {
      return;
    }

    let mut snapshot = FormDefaultSnapshot::default();

    for node_id in 1..index.id_to_node.len() {
      if node_or_ancestor_is_template(index, node_id) {
        continue;
      }
      let Some(node) = index.node(node_id) else {
        continue;
      };
      if !node.is_element() {
        continue;
      }

      if is_input(node) && resolve_form_owner(index, node_id) == Some(form_id) {
        snapshot.input_value.insert(
          node_id,
          node.get_attribute_ref("value").map(|v| v.to_string()),
        );

        if is_checkbox_input(node) || is_radio_input(node) {
          snapshot
            .input_checked
            .insert(node_id, node.get_attribute_ref("checked").is_some());
        }
      }

      if is_option(node) {
        let Some(select_id) = find_ancestor_select(index, node_id) else {
          continue;
        };
        if resolve_form_owner(index, select_id) != Some(form_id) {
          continue;
        }
        snapshot
          .option_selected
          .insert(node_id, node.get_attribute_ref("selected").is_some());
      }
    }

    self.form_default_snapshots.insert(form_id, snapshot);
  }

  fn perform_form_reset(&mut self, index: &mut DomIndexMut, control_node_id: usize) -> bool {
    let Some(form_id) = resolve_form_owner(index, control_node_id) else {
      return false;
    };

    self.ensure_form_default_snapshot_for_form(index, form_id);
    let Some(snapshot) = self.form_default_snapshots.get(&form_id) else {
      return false;
    };

    let mut changed = false;

    // Reset value-bearing `<input>` elements.
    for (&input_id, default_value_attr) in &snapshot.input_value {
      let Some(node) = index.node(input_id) else {
        continue;
      };
      if !is_input(node) {
        continue;
      }

      let ty = input_type(node);
      // Button-ish inputs are not resettable in practice and never change without JS.
      if ty.eq_ignore_ascii_case("submit")
        || ty.eq_ignore_ascii_case("reset")
        || ty.eq_ignore_ascii_case("button")
        || ty.eq_ignore_ascii_case("image")
      {
        continue;
      }

      if is_checkbox_input(node) || is_radio_input(node) {
        let desired_checked = snapshot
          .input_checked
          .get(&input_id)
          .copied()
          .unwrap_or(false);
        if let Some(node_mut) = index.node_mut(input_id) {
          if desired_checked {
            changed |= set_node_attr(node_mut, "checked", "");
          } else {
            changed |= remove_node_attr(node_mut, "checked");
          }
          // Resetting checkbox/radio state also clears `indeterminate` if present.
          changed |= remove_node_attr(node_mut, "indeterminate");
        }
        continue;
      }

      if let Some(node_mut) = index.node_mut(input_id) {
        match default_value_attr {
          Some(value) => {
            changed |= set_node_attr(node_mut, "value", value);
          }
          None => {
            changed |= remove_node_attr(node_mut, "value");
          }
        }
      }
    }

    // Reset `<textarea>` values by removing the internal override attribute so the current value
    // falls back to the original text content.
    for node_id in 1..index.id_to_node.len() {
      if node_or_ancestor_is_template(index, node_id) {
        continue;
      }
      let Some(node) = index.node(node_id) else {
        continue;
      };
      if !is_textarea(node) {
        continue;
      }
      if resolve_form_owner(index, node_id) != Some(form_id) {
        continue;
      }
      if let Some(node_mut) = index.node_mut(node_id) {
        changed |= remove_node_attr(node_mut, "data-fastr-value");
      }
    }

    // Reset `<select>` selections by restoring each `<option>`'s `selected` content attribute.
    for (&option_id, selected) in &snapshot.option_selected {
      if let Some(node_mut) = index.node_mut(option_id) {
        if *selected {
          changed |= set_node_attr(node_mut, "selected", "");
        } else {
          changed |= remove_node_attr(node_mut, "selected");
        }
      }
    }

    // Reset `<input type="file">` selections.
    //
    // File input state lives outside markup:
    // - selected files are stored in `InteractionState`'s live form state so form submission
    //   can include file bytes without leaking paths/bytes into the DOM, and
    // - a synthetic `data-fastr-file-value` attribute mirrors the "value string" for validation /
    //   accessibility (`C:\fakepath\...`), matching browser behavior where markup `value=` is ignored.
    for node_id in 1..index.id_to_node.len() {
      if node_or_ancestor_is_template(index, node_id) {
        continue;
      }
      let Some(node) = index.node(node_id) else {
        continue;
      };
      if !is_file_input(node) {
        continue;
      }
      if resolve_form_owner(index, node_id) != Some(form_id) {
        continue;
      }

      if self.state.form_state().file_inputs.contains_key(&node_id) {
        self.state.form_state_mut().file_inputs.remove(&node_id);
        changed = true;
      }
      if let Some(node_mut) = index.node_mut(node_id) {
        changed |= remove_node_attr(node_mut, "data-fastr-file-value");
      }
    }

    // Clear text undo history for controls in this form.
    {
      let mut ids_in_form = std::collections::HashSet::<usize>::new();
      for node_id in 1..index.id_to_node.len() {
        if node_or_ancestor_is_template(index, node_id) {
          continue;
        }
        let Some(node) = index.node(node_id) else {
          continue;
        };
        if !(is_input(node) || is_textarea(node)) {
          continue;
        }
        if resolve_form_owner(index, node_id) != Some(form_id) {
          continue;
        }
        ids_in_form.insert(node_id);
      }
      self
        .text_undo
        .retain(|node_id, _| !ids_in_form.contains(node_id));
    }

    // Clear HTML "user validity" gating for the form + its associated controls.
    let mut cleared_user_validity = false;
    if !self.state.user_validity().is_empty() {
      let user_validity = self.state.user_validity_mut();
      if user_validity.remove(&form_id) {
        cleared_user_validity = true;
      }
      for node_id in 1..index.id_to_node.len() {
        if node_or_ancestor_is_template(index, node_id) {
          continue;
        }
        let Some(node) = index.node(node_id) else {
          continue;
        };
        if !(is_input(node) || is_textarea(node) || is_select(node) || is_button(node)) {
          continue;
        }
        if resolve_form_owner(index, node_id) != Some(form_id) {
          continue;
        }
        if user_validity.remove(&node_id) {
          cleared_user_validity = true;
        }
      }
    }
    changed |= cleared_user_validity;

    // If the focused element is within the reset form, clamp caret/selection state to the new
    // value length to preserve internal invariants.
    if let Some(focused) = self.state.focused {
      let focused_in_form = focused == form_id
        || index
          .node(focused)
          .is_some_and(|node| resolve_form_owner(index, focused) == Some(form_id));
      if focused_in_form {
        if let Some(edit) = self
          .text_edit
          .as_mut()
          .filter(|edit| edit.node_id == focused)
        {
          let value_len = index
            .node(focused)
            .map(|node| {
              if is_textarea(node) {
                textarea_value_for_editing(node).chars().count()
              } else {
                node
                  .get_attribute_ref("value")
                  .unwrap_or("")
                  .chars()
                  .count()
              }
            })
            .unwrap_or(0);
          edit.caret = edit.caret.min(value_len);
          edit.selection_anchor = edit.selection_anchor.map(|a| a.min(value_len));
          changed |= self.sync_text_edit_paint_state();
        }
      }
    }

    changed
  }

  fn step_number_input(
    &mut self,
    index: &mut DomIndexMut,
    node_id: usize,
    delta_steps: i32,
  ) -> bool {
    if delta_steps == 0 {
      return false;
    }
    if node_or_ancestor_is_inert(index, node_id)
      || node_is_disabled(index, node_id)
      || node_is_readonly(index, node_id)
    {
      return false;
    }
    if !index
      .node(node_id)
      .is_some_and(|node| is_input(node) && input_type(node).eq_ignore_ascii_case("number"))
    {
      return false;
    }

    self.ensure_form_default_snapshot_for_control(index, node_id);

    // Any direct value mutation cancels an in-progress IME preedit string.
    let mut changed = self.ime_cancel_internal();

    let value_changed = if let Some(node_mut) = index.node_mut(node_id) {
      dom_mutation::step_number_value(node_mut, delta_steps)
    } else {
      false
    };
    changed |= value_changed;
    if value_changed {
      changed |= self.mark_user_validity(node_id);
    }

    // Keep caret/selection state consistent with the new value when the control is focused.
    if self.state.focused == Some(node_id) {
      let new_len = index
        .node(node_id)
        .and_then(|node| node.get_attribute_ref("value"))
        .unwrap_or("")
        .chars()
        .count();
      if let Some(edit) = self
        .text_edit
        .as_mut()
        .filter(|edit| edit.node_id == node_id)
      {
        let prev = (
          edit.caret,
          edit.caret_affinity,
          edit.selection_anchor,
          edit.preferred_x,
        );
        edit.caret = new_len;
        edit.caret_affinity = CaretAffinity::Downstream;
        edit.selection_anchor = None;
        edit.preferred_x = None;
        if (
          edit.caret,
          edit.caret_affinity,
          edit.selection_anchor,
          edit.preferred_x,
        ) != prev
        {
          changed = true;
        }
      }
      changed |= self.sync_text_edit_paint_state();
    }

    changed
  }

  /// Update `<select>` selection and mark HTML "user validity" when the selection changes.
  pub fn activate_select_option(
    &mut self,
    dom: &mut DomNode,
    select_node_id: usize,
    option_node_id: usize,
    toggle_for_multiple: bool,
  ) -> bool {
    let index = DomIndexMut::new(dom);
    self.ensure_form_default_snapshot_for_control(&index, select_node_id);
    let dom_changed = dom_mutation::activate_select_option(
      dom,
      select_node_id,
      option_node_id,
      toggle_for_multiple,
    );
    let mut changed = dom_changed;
    if dom_changed {
      changed |= self.mark_user_validity(select_node_id);
    }
    changed
  }

  /// Update an `<input>` element's value from a chosen `<datalist>` `<option>`.
  ///
  /// This validates that the chosen option is a descendant of the `<datalist>` referenced by
  /// `input[list]` and rejects disabled/inert/readonly controls.
  pub fn activate_datalist_option(
    &mut self,
    dom: &mut DomNode,
    input_node_id: usize,
    option_node_id: usize,
  ) -> bool {
    let index = DomIndexMut::new(dom);
    self.ensure_form_default_snapshot_for_control(&index, input_node_id);

    // Direct value mutation cancels any in-progress IME preedit.
    let mut changed = self.ime_cancel_internal();
    let dom_changed = dom_mutation::activate_datalist_option(dom, input_node_id, option_node_id);
    changed |= dom_changed;
    if dom_changed {
      changed |= self.mark_user_validity(input_node_id);
    }

    if dom_changed && self.state.focused == Some(input_node_id) {
      let value = index
        .node(input_node_id)
        .and_then(|node| node.get_attribute_ref("value"))
        .unwrap_or("");
      let len = value.chars().count();
      self.text_edit = Some(TextEditState {
        node_id: input_node_id,
        caret: len,
        caret_affinity: CaretAffinity::Downstream,
        selection_anchor: None,
        preferred_x: None,
      });
      changed |= self.sync_text_edit_paint_state();
    }

    changed
  }

  /// Update the value for a date/time-like `<input>` control as if the user edited it.
  ///
  /// This applies HTML value sanitization rules: invalid values sanitize to the empty string.
  pub fn set_date_time_input_value(
    &mut self,
    dom: &mut DomNode,
    input_node_id: usize,
    value: &str,
  ) -> bool {
    let mut index = DomIndexMut::new(dom);
    let Some(node) = index.node(input_node_id) else {
      return false;
    };
    let Some(kind) = date_time_input_kind(node) else {
      return false;
    };

    if node_or_ancestor_is_inert(&index, input_node_id) || node_is_disabled(&index, input_node_id) {
      return false;
    }
    if node_is_readonly(&index, input_node_id) {
      return false;
    }

    let trimmed = trim_ascii_whitespace(value);
    let sanitized = if trimmed.is_empty() {
      ""
    } else {
      let ok = match kind {
        DateTimeInputKind::Date => crate::dom::parse_input_date_value(trimmed).is_some(),
        DateTimeInputKind::Time => crate::dom::parse_input_time_value(trimmed).is_some(),
        DateTimeInputKind::DateTimeLocal => {
          crate::dom::parse_input_datetime_local_value(trimmed).is_some()
        }
        DateTimeInputKind::Month => crate::dom::parse_input_month_value(trimmed).is_some(),
        DateTimeInputKind::Week => crate::dom::parse_input_week_value(trimmed).is_some(),
      };
      if ok {
        trimmed
      } else {
        ""
      }
    };

    self.ensure_form_default_snapshot_for_control(&index, input_node_id);

    let Some(node_mut) = index.node_mut(input_node_id) else {
      return false;
    };

    // Any direct value mutation cancels an in-progress IME preedit string.
    let mut changed = self.ime_cancel_internal();
    let changed_value = set_node_attr(node_mut, "value", sanitized);
    changed |= changed_value;
    if changed_value {
      changed |= self.mark_user_validity(input_node_id);
    }

    // Keep caret state in sync for focused controls.
    if self.state.focused == Some(input_node_id) {
      let len = sanitized.chars().count();
      self.text_edit = Some(TextEditState {
        node_id: input_node_id,
        caret: len,
        caret_affinity: CaretAffinity::Downstream,
        selection_anchor: None,
        preferred_x: None,
      });
      changed |= self.sync_text_edit_paint_state();
    }

    changed
  }

  /// Replace the full value of a text control (`<input>`/`<textarea>`) as if the user edited it.
  ///
  /// This is similar to `text_input`, but replaces the entire value instead of inserting at the
  /// caret/selection, and it supports setting the value to the empty string.
  pub fn set_text_control_value(&mut self, dom: &mut DomNode, node_id: usize, value: &str) -> bool {
    let mut index = DomIndexMut::new(dom);

    let (is_input_text_like, is_textarea, current, textarea_default_value) = {
      let Some(node) = index.node(node_id) else {
        return false;
      };

      let is_input_text_like = is_text_like_input(node);
      let is_textarea = is_textarea(node);
      if !(is_input_text_like || is_textarea) {
        return false;
      }

      if node_or_ancestor_is_inert(&index, node_id) || node_is_disabled(&index, node_id) {
        return false;
      }
      if node_is_readonly(&index, node_id) {
        return false;
      }

      let current = if is_textarea {
        textarea_value_for_editing(node)
      } else {
        strip_ascii_line_breaks(node.get_attribute_ref("value").unwrap_or("")).into_owned()
      };
      let textarea_default_value = if is_textarea {
        crate::dom::textarea_value(node)
      } else {
        String::new()
      };

      (is_input_text_like, is_textarea, current, textarea_default_value)
    };

    self.ensure_form_default_snapshot_for_control(&index, node_id);

    // Any direct value mutation cancels an in-progress IME preedit string.
    let mut changed = self.ime_cancel_internal();

    let current_len = current.chars().count();

    // Capture the current caret/selection state for undo snapshots.
    let mut edit = self.text_edit.unwrap_or(TextEditState {
      node_id,
      caret: current_len,
      caret_affinity: CaretAffinity::Downstream,
      selection_anchor: None,
      preferred_x: None,
    });
    if edit.node_id != node_id {
      edit = TextEditState {
        node_id,
        caret: current_len,
        caret_affinity: CaretAffinity::Downstream,
        selection_anchor: None,
        preferred_x: None,
      };
    }
    edit.caret = edit.caret.min(current_len);
    edit.selection_anchor = edit.selection_anchor.map(|a| a.min(current_len));

    let sanitized = if is_textarea {
      crate::dom::normalize_textarea_newlines(value.to_string())
    } else {
      strip_ascii_line_breaks(value).into_owned()
    };

    if sanitized != current {
      self.record_text_undo_snapshot(node_id, &current, &edit);
    }

    let Some(node_mut) = index.node_mut(node_id) else {
      return changed;
    };

    let changed_value = if is_input_text_like {
      if sanitized.is_empty() {
        remove_node_attr(node_mut, "value")
      } else {
        set_node_attr(node_mut, "value", &sanitized)
      }
    } else if sanitized == textarea_default_value {
      remove_node_attr(node_mut, "data-fastr-value")
    } else {
      set_node_attr(node_mut, "data-fastr-value", &sanitized)
    };

    changed |= changed_value;
    if changed_value {
      changed |= self.mark_user_validity(node_id);
    }

    // Keep caret state in sync for focused controls.
    if self.state.focused == Some(node_id) {
      let caret = sanitized.chars().count();
      self.text_edit = Some(TextEditState {
        node_id,
        caret,
        caret_affinity: CaretAffinity::Downstream,
        selection_anchor: None,
        preferred_x: None,
      });
      changed |= self.sync_text_edit_paint_state();
    }

    changed
  }

  /// Update the value for an `<input type="color">` control as if the user edited it.
  ///
  /// This applies HTML value sanitization rules for color inputs:
  /// - Values must be a [simple color](https://html.spec.whatwg.org/multipage/input.html#simple-colour),
  ///   i.e. `#` followed by exactly 6 ASCII hex digits.
  /// - Invalid (including empty) values are sanitized to black (`#000000`), matching browser
  ///   behavior and the spec's default value semantics for color inputs.
  pub fn set_color_input_value(
    &mut self,
    dom: &mut DomNode,
    input_node_id: usize,
    value: &str,
  ) -> bool {
    let mut index = DomIndexMut::new(dom);
    let Some(node) = index.node(input_node_id) else {
      return false;
    };
    if !is_color_input(node) {
      return false;
    }

    if node_or_ancestor_is_inert(&index, input_node_id) || node_is_disabled(&index, input_node_id) {
      return false;
    }
    if node_is_readonly(&index, input_node_id) {
      return false;
    }

    // https://html.spec.whatwg.org/multipage/input.html#simple-colour
    //
    // A "simple color" is exactly 7 code points: '#' followed by 6 ASCII hex digits. Invalid values
    // sanitize to black (`#000000`), matching `dom::input_color_value_string`.
    let trimmed = trim_ascii_whitespace(value);
    let sanitized = {
      let valid = trimmed.len() == 7
        && trimmed.starts_with('#')
        && trimmed.as_bytes()[1..].iter().all(|b| b.is_ascii_hexdigit());
      if valid {
        let hex = &trimmed[1..];
        let parsed = (
          u8::from_str_radix(&hex[0..2], 16),
          u8::from_str_radix(&hex[2..4], 16),
          u8::from_str_radix(&hex[4..6], 16),
        );
        if let (Ok(r), Ok(g), Ok(b)) = parsed {
          format!("#{r:02x}{g:02x}{b:02x}")
        } else {
          "#000000".to_string()
        }
      } else {
        "#000000".to_string()
      }
    };

    self.ensure_form_default_snapshot_for_control(&index, input_node_id);

    let Some(node_mut) = index.node_mut(input_node_id) else {
      return false;
    };

    // Any direct value mutation cancels an in-progress IME preedit string.
    let mut changed = self.ime_cancel_internal();
    let changed_value = set_node_attr(node_mut, "value", &sanitized);
    changed |= changed_value;
    if changed_value {
      changed |= self.mark_user_validity(input_node_id);
    }

    changed
  }

  pub fn focused_node_id(&self) -> Option<usize> {
    self.state.focused
  }

  /// Returns the `<input type="range">` node id currently being dragged by the pointer, if any.
  ///
  /// This is a UI-layer integration hook so external code can keep higher-level state (e.g. JS
  /// `dom2` form control state) synchronized while the user drags the slider.
  pub fn active_range_drag_node_id(&self) -> Option<usize> {
    self.range_drag.map(|state| state.node_id)
  }

  /// Returns the `(node_id, box_id)` for an active text-control selection drag.
  ///
  /// This corresponds to the user holding the primary pointer button down inside a focused
  /// `<input>`/`<textarea>` and moving the pointer to extend the selection (i.e. not text
  /// drag-and-drop).
  pub fn active_text_drag(&self) -> Option<(usize, usize)> {
    self.text_drag.map(|state| (state.node_id, state.box_id))
  }

  fn sync_text_edit_paint_state(&mut self) -> bool {
    let next = self
      .text_edit
      .as_ref()
      .filter(|edit| self.state.focused == Some(edit.node_id))
      .map(|edit| TextEditPaintState {
        node_id: edit.node_id,
        caret: edit.caret,
        caret_affinity: edit.caret_affinity,
        selection: edit.selection(),
      });
    let changed = self.state.text_edit != next;
    self.state.set_text_edit(next);
    changed
  }

  /// Returns the most recent click target (pre-order DOM node id) produced by
  /// [`InteractionEngine::pointer_up_with_scroll`].
  ///
  /// This is a UI-layer hook that allows external code to dispatch higher-level click events
  /// (e.g. JavaScript DOM `"click"` listeners) using the same hit-test/label remapping semantics
  /// as the interaction engine's built-in default actions.
  pub fn take_last_click_target(&mut self) -> Option<usize> {
    // Keep the id and element-id payloads in sync: when the UI consumes the click target, clear
    // both.
    self.last_click_target_element_id = None;
    self.last_click_target.take()
  }

  /// Like [`InteractionEngine::take_last_click_target`], but also returns the target element's HTML
  /// `id` attribute when available.
  pub fn take_last_click_target_with_element_id(&mut self) -> (Option<usize>, Option<String>) {
    (
      self.last_click_target.take(),
      self.last_click_target_element_id.take(),
    )
  }
  /// Returns the most recent form submitter (pre-order DOM node id) that produced a submission
  /// navigation request during user activation.
  ///
  /// This is an integration hook for higher-level layers (e.g. browser UI workers) that need to
  /// dispatch JS `"submit"` events and honor `event.preventDefault()` before committing the
  /// navigation.
  pub fn take_last_form_submitter(&mut self) -> Option<usize> {
    self.last_form_submitter_element_id = None;
    self.last_form_submitter.take()
  }

  /// Like [`InteractionEngine::take_last_form_submitter`], but also returns the submitter element's
  /// HTML `id` attribute when available.
  pub fn take_last_form_submitter_with_element_id(&mut self) -> (Option<usize>, Option<String>) {
    (
      self.last_form_submitter.take(),
      self.last_form_submitter_element_id.take(),
    )
  }

  /// Returns the plain-text payload of the currently active drag-and-drop gesture, if any.
  ///
  /// This is intended as a UI-layer integration hook so higher-level code can construct a native
  /// drag session / JS `DataTransfer` using the interaction engine's current drag state.
  ///
  /// This returns `Some` only when a drag-drop gesture is *active* (i.e. after crossing the drag
  /// threshold), not when a drag candidate is pending.
  pub fn active_drag_text_payload(&self) -> Option<String> {
    if let Some(TextDragDropState::Active(active)) = self.text_drag_drop.as_ref() {
      return Some(active.text.clone());
    }
    self
      .document_selection_drag_drop
      .as_ref()
      .and_then(|state| state.payload.clone())
  }

  /// Returns the semantic pre-order DOM node id that initiated the active drag-and-drop gesture, if
  /// any.
  pub fn active_drag_source_node_id(&self) -> Option<usize> {
    if let Some(TextDragDropState::Active(active)) = self.text_drag_drop.as_ref() {
      return Some(active.node_id);
    }
    if self
      .document_selection_drag_drop
      .as_ref()
      .is_some_and(|state| state.payload.is_some())
    {
      // Document-selection drags can span multiple DOM nodes. Use the semantic target from the
      // pointer-down that began the drag candidate.
      return self.pointer_down_target;
    }
    None
  }

  /// Overrides the plain-text payload for the active drag-and-drop gesture.
  ///
  /// This is primarily intended for JS `DataTransfer.setData("text/plain", ...)` integration: UI
  /// layers can update the engine's stored payload so any subsequent default drop insertion uses the
  /// new text.
  ///
  /// Returns `true` when an active drag session was updated.
  pub fn override_active_drag_text_payload(&mut self, text: String) -> bool {
    let mut updated = false;

    if let Some(TextDragDropState::Active(active)) = self.text_drag_drop.as_mut() {
      active.text = text.clone();
      updated = true;
    }
    if self
      .document_selection_drag_drop
      .as_ref()
      .is_some_and(|state| state.payload.is_some())
    {
      if let Some(state) = self.document_selection_drag_drop.as_mut() {
        state.payload = Some(text);
        updated = true;
      }
    }

    updated
  }

  /// Cancels any in-progress drag-and-drop gesture.
  ///
  /// This is a UI-layer hook used when JS cancels the `"dragstart"` default action.
  pub fn cancel_active_drag_drop(&mut self) {
    self.text_drag_drop = None;
    self.document_selection_drag_drop = None;
  }

  fn set_focus(
    &mut self,
    index: &mut DomIndexMut,
    new_focused: Option<usize>,
    focus_visible: bool,
  ) -> bool {
    let prev_focused = self.state.focused;
    let prev_focus_visible = self.state.focus_visible;
    let mut changed = false;

    // Any focus change cancels an in-progress IME composition and resets text-editing state.
    if prev_focused != new_focused {
      if self.state.ime_preedit.is_some() {
        changed = true;
      }
      self.state.set_ime_preedit(None);
      self.text_edit = None;
      self.text_drag = None;
      self.text_drag_drop = None;
      self.document_drag = None;
      self.document_selection_drag_drop = None;
      self.link_drag = None;
      self.pending_text_drop_move = None;
      // Focus changes collapse any existing document selection (e.g. a prior Ctrl+A selection).
      self.state.set_document_selection(None);
    }

    self.state.set_focused(new_focused);
    self
      .state
      .set_focus_visible(new_focused.is_some() && focus_visible);
    let new_focus_chain = new_focused
      .map(|id| collect_element_chain(index, id))
      .unwrap_or_default();
    let focus_chain_changed = self.state.focus_chain() != new_focus_chain.as_slice();
    if focus_chain_changed {
      self.state.set_focus_chain(new_focus_chain);
    }

    if prev_focused != new_focused {
      if let Some(new_id) = new_focused {
        // Initialize text editing state for focused text controls.
        if index
          .node(new_id)
          .is_some_and(|node| is_text_input(node) || is_textarea(node))
        {
          let caret = index
            .node(new_id)
            .map(|node| {
              if is_textarea(node) {
                textarea_value_for_editing(node).chars().count()
              } else {
                node
                  .get_attribute_ref("value")
                  .unwrap_or("")
                  .chars()
                  .count()
              }
            })
            .unwrap_or(0);
          self.text_edit = Some(TextEditState {
            node_id: new_id,
            caret,
            caret_affinity: CaretAffinity::Downstream,
            selection_anchor: None,
            preferred_x: None,
          });
        }
      }
    }

    // Keep caret/selection paint state in sync with the internal text editing state.
    changed |= self.sync_text_edit_paint_state();

    changed
      || prev_focused != self.state.focused
      || prev_focus_visible != self.state.focus_visible
      || focus_chain_changed
  }

  /// Programmatically update focus state for the given DOM node id.
  ///
  /// This is useful for implementing keyboard focus traversal (e.g. Tab) and for tests that need to
  /// set up a focused element without synthesizing pointer events.
  pub fn focus_node_id(
    &mut self,
    dom: &mut DomNode,
    node_id: Option<usize>,
    focus_visible: bool,
  ) -> (bool, InteractionAction) {
    let mut index = DomIndexMut::new(dom);
    let prev_focus = self.state.focused;

    // If focus already points at a detached node, drop it so we don't propagate stale ids.
    let mut changed = false;
    if prev_focus.is_some_and(|id| !index.node(id).is_some_and(DomNode::is_element)) {
      changed |= self.set_focus(&mut index, None, false);
    }

    // Invalid focus targets should be ignored (no-op). Clearing focus is an explicit `None`.
    if let Some(requested) = node_id {
      if !index.node(requested).is_some_and(DomNode::is_element) {
        let action = if self.state.focused != prev_focus {
          InteractionAction::FocusChanged {
            node_id: self.state.focused,
          }
        } else {
          InteractionAction::None
        };
        return (changed, action);
      }
    }

    self.modality = if focus_visible {
      InputModality::Keyboard
    } else {
      InputModality::Pointer
    };

    changed |= self.set_focus(&mut index, node_id, focus_visible);

    let action = if self.state.focused != prev_focus {
      InteractionAction::FocusChanged {
        node_id: self.state.focused,
      }
    } else {
      InteractionAction::None
    };

    (changed, action)
  }

  pub fn set_text_selection_caret(&mut self, node_id: usize, caret: usize) {
    if self.state.focused != Some(node_id) {
      return;
    }
    if self
      .state
      .ime_preedit
      .as_ref()
      .is_some_and(|ime_state| ime_state.node_id == node_id)
    {
      self.ime_cancel_internal();
    }
    self.text_drag = None;
    self.text_drag_drop = None;
    match self.text_edit.as_mut() {
      Some(edit) if edit.node_id == node_id => {
        edit.caret = caret;
        edit.caret_affinity = CaretAffinity::Downstream;
        edit.selection_anchor = None;
        edit.preferred_x = None;
      }
      _ => {
        self.text_edit = Some(TextEditState {
          node_id,
          caret,
          caret_affinity: CaretAffinity::Downstream,
          selection_anchor: None,
          preferred_x: None,
        });
      }
    }
    self.sync_text_edit_paint_state();
  }

  pub fn set_text_selection_range(&mut self, node_id: usize, start: usize, end: usize) {
    if self.state.focused != Some(node_id) {
      return;
    }
    if start == end {
      self.set_text_selection_caret(node_id, start);
      return;
    }
    if self
      .state
      .ime_preedit
      .as_ref()
      .is_some_and(|ime_state| ime_state.node_id == node_id)
    {
      self.ime_cancel_internal();
    }
    self.text_drag = None;
    self.text_drag_drop = None;
    match self.text_edit.as_mut() {
      Some(edit) if edit.node_id == node_id => {
        edit.caret = end;
        edit.caret_affinity = CaretAffinity::Downstream;
        edit.selection_anchor = Some(start);
        edit.preferred_x = None;
      }
      _ => {
        self.text_edit = Some(TextEditState {
          node_id,
          caret: end,
          caret_affinity: CaretAffinity::Downstream,
          selection_anchor: Some(start),
          preferred_x: None,
        });
      }
    }
    self.sync_text_edit_paint_state();
  }

  /// Assistive-tech hook: set the selection range for the focused text control, clamping indices
  /// into the current value length.
  ///
  /// Returns `true` when the caret/selection paint state changed.
  pub fn a11y_set_text_selection_range(
    &mut self,
    dom: &mut DomNode,
    node_id: usize,
    start: usize,
    end: usize,
  ) -> bool {
    if self.state.focused != Some(node_id) {
      return false;
    }

    let index = DomIndexMut::new(dom);
    let Some(node) = index.node(node_id) else {
      return false;
    };
    if !(is_text_input(node) || is_textarea(node)) {
      return false;
    }

    let len = if is_textarea(node) {
      textarea_value_for_editing(node).chars().count()
    } else {
      node.get_attribute_ref("value").unwrap_or("").chars().count()
    };

    let start = start.min(len);
    let end = end.min(len);

    let mut changed = false;
    if self
      .state
      .ime_preedit
      .as_ref()
      .is_some_and(|ime_state| ime_state.node_id == node_id)
    {
      changed |= self.ime_cancel_internal();
    }

    self.text_drag = None;
    self.text_drag_drop = None;

    match self.text_edit.as_mut() {
      Some(edit) if edit.node_id == node_id => {
        edit.caret = end;
        edit.caret_affinity = CaretAffinity::Downstream;
        edit.selection_anchor = if start == end { None } else { Some(start) };
        edit.preferred_x = None;
      }
      _ => {
        self.text_edit = Some(TextEditState {
          node_id,
          caret: end,
          caret_affinity: CaretAffinity::Downstream,
          selection_anchor: if start == end { None } else { Some(start) },
          preferred_x: None,
        });
      }
    }

    changed |= self.sync_text_edit_paint_state();
    changed
  }

  /// Place the caret in the currently-focused text control based on a page-space point.
  ///
  /// This is primarily used by UI layers that receive a context-menu/right-click request without a
  /// preceding `PointerDown`, and still want paste/selection operations to target the clicked caret
  /// position (matching native browser behavior).
  ///
  /// Returns `true` when the caret/selection paint state changed.
  pub fn set_text_caret_from_page_point(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    scroll: &ScrollState,
    node_id: usize,
    box_id: usize,
    page_point: Point,
  ) -> bool {
    // Only update caret state for the focused node. This keeps invariants simple (text_edit must
    // track focused).
    if self.state.focused != Some(node_id) {
      return false;
    }

    let index = DomIndexMut::new(dom);
    let Some((caret, affinity)) = caret_index_for_text_control_point(
      &index,
      box_tree,
      fragment_tree,
      scroll,
      node_id,
      box_id,
      page_point,
    ) else {
      return false;
    };

    // Preserve an existing selection when the right-click falls inside the selected range, matching
    // native browser behaviour.
    let current_len = index
      .node(node_id)
      .map(|node| {
        if is_textarea(node) {
          textarea_value_for_editing(node).chars().count()
        } else {
          node
            .get_attribute_ref("value")
            .unwrap_or("")
            .chars()
            .count()
        }
      })
      .unwrap_or(0);
    let caret = caret.min(current_len);

    if let Some(edit) = self
      .text_edit
      .as_ref()
      .filter(|edit| edit.node_id == node_id)
    {
      let mut edit = *edit;
      edit.caret = edit.caret.min(current_len);
      edit.selection_anchor = edit.selection_anchor.map(|a| a.min(current_len));
      if let Some((start, end)) = edit.selection() {
        if caret >= start && caret <= end {
          return false;
        }
      }
    }

    let mut changed = false;
    if self
      .state
      .ime_preedit
      .as_ref()
      .is_some_and(|ime_state| ime_state.node_id == node_id)
    {
      changed |= self.ime_cancel_internal();
    }

    self.text_drag = None;

    match self.text_edit.as_mut() {
      Some(edit) if edit.node_id == node_id => {
        let prev = (edit.caret, edit.caret_affinity, edit.selection_anchor);
        edit.caret = caret;
        edit.caret_affinity = affinity;
        edit.selection_anchor = None;
        edit.preferred_x = None;
        changed |= (edit.caret, edit.caret_affinity, edit.selection_anchor) != prev;
      }
      _ => {
        self.text_edit = Some(TextEditState {
          node_id,
          caret,
          caret_affinity: affinity,
          selection_anchor: None,
          preferred_x: None,
        });
        changed = true;
      }
    }

    changed |= self.sync_text_edit_paint_state();
    changed
  }

  pub fn clear_pointer_state(&mut self, _dom: &mut DomNode) -> bool {
    let hover_changed = !self.state.hover_chain().is_empty();
    let active_changed = !self.state.active_chain().is_empty();
    self.state.clear_hover_chain();
    self.state.clear_active_chain();
    self.hover_tooltip = None;
    self.pointer_down_target = None;
    self.link_drag = None;
    self.range_drag = None;
    self.number_spin = None;
    self.text_drag = None;
    self.text_drag_drop = None;
    self.document_drag = None;
    self.document_selection_drag_drop = None;
    self.pending_text_drop_move = None;
    hover_changed | active_changed
  }

  pub fn clear_pointer_state_without_dom(&mut self) {
    self.state.clear_hover_chain();
    self.state.clear_active_chain();
    self.hover_tooltip = None;
    self.pointer_down_target = None;
    self.link_drag = None;
    self.range_drag = None;
    self.number_spin = None;
    self.text_drag = None;
    self.text_drag_drop = None;
    self.document_drag = None;
    self.document_selection_drag_drop = None;
    self.pending_text_drop_move = None;
  }

  fn hover_tooltip_from_title_attributes<'a>(
    index: &'a DomIndexMut,
    hover_chain: &[usize],
  ) -> Option<&'a str> {
    // `InteractionEngine` stores hover chain ids in target→root order.
    //
    // The chain may contain additional non-ancestor nodes (e.g. label-associated controls) appended
    // after the real ancestor chain. Ancestor ids are strictly decreasing in DOM pre-order, so keep
    // only that prefix for HTML `title` tooltip semantics.
    let mut prev = usize::MAX;
    for &node_id in hover_chain {
      if node_id >= prev {
        break;
      }
      prev = node_id;

      let Some(node) = index.node(node_id) else {
        continue;
      };
      let Some(title) = node.get_attribute_ref("title") else {
        continue;
      };
      let title = trim_ascii_whitespace(title);
      if !title.is_empty() {
        return Some(title);
      }
    }
    None
  }

  fn title_attribute_tooltip<'a>(index: &'a DomIndexMut, node_id: usize) -> Option<&'a str> {
    index
      .node(node_id)
      .and_then(|node| node.get_attribute_ref("title"))
      .map(trim_ascii_whitespace)
      .filter(|title| !title.is_empty())
  }

  /// Update hover state (element under pointer + ancestors).
  /// `viewport_point` is in viewport coordinates; this method converts it to a page point by
  /// translating it by `scroll.viewport`.
  ///
  /// The provided `fragment_tree` must already have element scroll offsets applied (e.g. via
  /// [`crate::interaction::fragment_tree_with_scroll`]).
  pub fn pointer_move(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    scroll: &ScrollState,
    viewport_point: Point,
  ) -> bool {
    self
      .pointer_move_and_hit_and_drop_target(dom, box_tree, fragment_tree, scroll, viewport_point)
      .0
  }

  /// Like [`InteractionEngine::pointer_move`], but also returns the hit-test result.
  ///
  /// Returning the hit test allows UI layers to reuse the interaction engine's source-of-truth
  /// hover target for tasks like cursor selection and JS event dispatch without performing a second
  /// DOM hit test.
  pub fn pointer_move_and_hit(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    scroll: &ScrollState,
    viewport_point: Point,
  ) -> (bool, Option<HitTestResult>) {
    let (changed, hit, _) =
      self.pointer_move_and_hit_and_drop_target(dom, box_tree, fragment_tree, scroll, viewport_point);
    (changed, hit)
  }

  /// Like [`InteractionEngine::pointer_move_and_hit`], but also returns whether the hover target is
  /// a valid drop target for the current drag-and-drop gesture (if any).
  ///
  /// This performs the full effective disabled/inert/hidden checks using the same DOM index already
  /// built for hover hit-testing, allowing UI layers to avoid rebuilding an O(n) DOM index on every
  /// pointer move during drag-and-drop.
  pub fn pointer_move_and_hit_and_drop_target(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    scroll: &ScrollState,
    viewport_point: Point,
  ) -> (bool, Option<HitTestResult>, bool) {
    let page_point = viewport_point.translate(scroll.viewport);
    let mut index = DomIndexMut::new(dom);
    let box_index = HitTestBoxIndex::new(box_tree);
    let mut dom_changed = false;

    // Link drag suppression: native browsers suppress click navigation when the pointer moves past a
    // small threshold while holding the primary button down on a link.
    const LINK_DRAG_THRESHOLD_PX: f32 = 5.0;
    if let Some(state) = self.link_drag.as_mut() {
      if !state.active
        && page_point.x.is_finite()
        && page_point.y.is_finite()
        && page_point.x >= 0.0
        && page_point.y >= 0.0
        && state.down_point.distance_to(page_point) >= LINK_DRAG_THRESHOLD_PX
      {
        state.active = true;
      }
    }

    // Text-control drag-and-drop: promote a candidate drag when the pointer moves past a small
    // threshold, mirroring browser behavior where a plain click inside selection doesn't collapse
    // until mouseup.
    const TEXT_DRAG_DROP_THRESHOLD_PX: f32 = 5.0;
    if let Some(TextDragDropState::Candidate(candidate)) = self.text_drag_drop.as_mut() {
      if page_point.x.is_finite()
        && page_point.y.is_finite()
        && page_point.x >= 0.0
        && page_point.y >= 0.0
        && candidate.down_point.distance_to(page_point) >= TEXT_DRAG_DROP_THRESHOLD_PX
      {
        let active = TextDragDropActive {
          node_id: candidate.node_id,
          box_id: candidate.box_id,
          down_point: candidate.down_point,
          down_caret: candidate.down_caret,
          down_caret_affinity: candidate.down_caret_affinity,
          selection: candidate.selection,
          text: std::mem::take(&mut candidate.text),
          focus_before: candidate.focus_before,
        };
        self.text_drag_drop = Some(TextDragDropState::Active(active));
        dom_changed = true;
      }
    }

    if let Some(state) = self.range_drag {
      // When the browser UI's cursor leaves the page image while dragging a range input, it sends a
      // sentinel pointer position (translated by the worker to a negative page-point) to clear
      // hover state.
      //
      // Feeding this sentinel into range dragging would clamp the slider to the minimum value,
      // causing surprising jumps as soon as the cursor leaves the page. Ignore those sentinel
      // points so the range value stays at its last in-page value until the cursor re-enters.
      if page_point.x.is_finite()
        && page_point.y.is_finite()
        && page_point.x >= 0.0
        && page_point.y >= 0.0
      {
        self.ensure_form_default_snapshot_for_control(&index, state.node_id);
        let changed = update_range_value_from_pointer(
          &mut index,
          &box_index,
          fragment_tree,
          state.node_id,
          state.box_id,
          page_point,
        );
        dom_changed |= changed;
        if changed {
          dom_changed |= self.mark_user_validity(state.node_id);
        }
      }
    }

    if let Some(state) = self.text_drag {
      // Mirror the sentinel handling in range drags: when the pointer leaves the page image the UI
      // sends a negative page-point. Do not treat that as dragging the selection to the start of the
      // text control; keep the last in-control selection instead.
      if page_point.x.is_finite()
        && page_point.y.is_finite()
        && page_point.x >= 0.0
        && page_point.y >= 0.0
      {
        if self
          .state
          .ime_preedit
          .as_ref()
          .is_some_and(|ime_state| ime_state.node_id == state.node_id)
        {
          dom_changed |= self.ime_cancel_internal();
        }

        if let Some(edit) = self
          .text_edit
          .as_mut()
          .filter(|edit| edit.node_id == state.node_id)
        {
          if let Some((raw_caret, raw_affinity)) = caret_index_for_text_control_point(
            &index,
            &box_index,
            fragment_tree,
            scroll,
            state.node_id,
            state.box_id,
            page_point,
          ) {
            let prev_caret = edit.caret;
            let prev_affinity = edit.caret_affinity;
            let prev_anchor = edit.selection_anchor;
            edit.preferred_x = None;

            if let Some((mut sel_start, mut sel_end)) = state.initial_range {
              // Multi-click selection (double-click / triple-click) dragging should preserve the
              // originally selected word/line/all content.
              let node = index.node(state.node_id);
              let is_textarea = node.is_some_and(is_textarea);
              let current_value = node
                .map(|node| {
                  if is_textarea {
                    textarea_value_for_editing(node)
                  } else {
                    node.get_attribute_ref("value").unwrap_or("").to_string()
                  }
                })
                .unwrap_or_default();
              let current_len = current_value.chars().count();

              // Clamp stored endpoints to the current text length in case the value changed while
              // dragging (e.g. via script).
              sel_start = sel_start.min(current_len);
              sel_end = sel_end.min(current_len);

              let down_caret = state.down_caret.min(current_len);
              let caret = raw_caret.min(current_len);
              let dragging_left = caret < down_caret;
              let fixed = if dragging_left { sel_end } else { sel_start };

              let mut caret = caret;
              match state.granularity {
                SelectionDragGranularity::Char => {}
                SelectionDragGranularity::Word => {
                  if let Some((word_start, word_end)) = word_selection_range(&current_value, caret) {
                    caret = if dragging_left { word_start } else { word_end };
                  }
                }
                SelectionDragGranularity::LineOrBlock => {
                  if is_textarea {
                    let (line_start, line_end) = textarea_line_selection_range(&current_value, caret);
                    caret = if dragging_left { line_start } else { line_end };
                  } else {
                    // `<input>` has no line concept; triple-click already selects all.
                    caret = if dragging_left { 0 } else { current_len };
                  }
                }
              }

              // Prevent shrinking into the initial multi-click selection.
              if dragging_left {
                caret = caret.min(sel_start);
              } else {
                caret = caret.max(sel_end);
              }

              edit.caret = caret;
              edit.caret_affinity = raw_affinity;
              edit.selection_anchor = if caret == fixed { None } else { Some(fixed) };
            } else {
              // Normal click-and-drag selection: extend in character granularity from the initial
              // anchor.
              edit.caret = raw_caret;
              edit.caret_affinity = raw_affinity;
              if raw_caret == state.anchor {
                edit.selection_anchor = None;
              } else {
                edit.selection_anchor = Some(state.anchor);
              }
            }

            if edit.caret != prev_caret
              || edit.caret_affinity != prev_affinity
              || edit.selection_anchor != prev_anchor
            {
              dom_changed = true;
            }
          }
        }
      }
    }

    if let Some(state) = self.document_drag {
      // Mirror the sentinel handling in other drags: when the pointer leaves the page image the UI
      // sends a negative page-point. Do not treat that as dragging the selection to the start of the
      // document; keep the last in-page selection instead.
      if page_point.x.is_finite()
        && page_point.y.is_finite()
        && page_point.x >= 0.0
        && page_point.y >= 0.0
      {
        if let Some((point, text_box_id)) =
          document_selection_hit_at_page_point(&box_index, fragment_tree, page_point)
        {
          let selection_changed = self.state.mutate_document_selection(|selection| {
            let DocumentSelectionState::Ranges(ranges) = selection else {
              return;
            };

            if let Some(initial_range) = state.initial_range {
              let initial_range = initial_range.normalized();
              let dragging_left =
                cmp_document_selection_point(point, state.down_point) == Ordering::Less;
              let fixed = if dragging_left {
                initial_range.end
              } else {
                initial_range.start
              };

              let mut focus = point;
              match state.granularity {
                SelectionDragGranularity::Char => {}
                SelectionDragGranularity::Word => {
                  if let Some(word_range) =
                    document_word_selection_range(box_tree, text_box_id, point)
                  {
                    focus = if dragging_left {
                      word_range.start
                    } else {
                      word_range.end
                    };
                  }
                }
                SelectionDragGranularity::LineOrBlock => {
                  if let Some(block_range) =
                    document_block_selection_range(box_tree, text_box_id, point)
                  {
                    focus = if dragging_left {
                      block_range.start
                    } else {
                      block_range.end
                    };
                  }
                }
              }

              // Prevent shrinking into the initial multi-click selection.
              if dragging_left {
                if cmp_document_selection_point(focus, initial_range.start) == Ordering::Greater {
                  focus = initial_range.start;
                }
              } else if cmp_document_selection_point(focus, initial_range.end) == Ordering::Less {
                focus = initial_range.end;
              }

              ranges.anchor = fixed;
              ranges.focus = focus;
            } else {
              ranges.focus = point;
            }
            if ranges.primary < ranges.ranges.len() {
              ranges.ranges[ranges.primary] = DocumentSelectionRange {
                start: ranges.anchor,
                end: ranges.focus,
              }
              .normalized();
            }
            ranges.normalize();
          });
          if selection_changed {
            dom_changed = true;
          }
        }
      }
    }

    // Drag-and-drop of an existing document selection into a text control.
    if self
      .document_selection_drag_drop
      .as_ref()
      .is_some_and(|state| state.payload.is_none())
    {
      const DRAG_THRESHOLD_PX: f32 = 4.0;
      let should_activate = self
        .document_selection_drag_drop
        .as_ref()
        .is_some_and(|state| {
          // Match sentinel handling for other drags: when the pointer leaves the page image, the UI
          // sends a negative page-point to clear hover state. Ignore those sentinel points so leaving
          // the page doesn't accidentally activate a drag-drop gesture.
          if !(page_point.x.is_finite()
            && page_point.y.is_finite()
            && page_point.x >= 0.0
            && page_point.y >= 0.0)
          {
            return false;
          }

          state.down_page_point.distance_to(page_point) >= DRAG_THRESHOLD_PX
        });

      if should_activate {
        let payload = self.document_selection_text_with_layout(box_tree, fragment_tree);
        if let Some(state) = self.document_selection_drag_drop.as_mut() {
          state.payload = payload;
          dom_changed = true;
        }
      }
    }

    dom_changed |= self.sync_text_edit_paint_state();

    // Pointer-leave robustness: the browser UI uses negative/non-finite coordinates as a sentinel
    // when the pointer leaves the page image. Treat these as "no hit" for hover-chain updates so we
    // do not accidentally hover negatively-positioned content.
    let hit = if page_point.x.is_finite()
      && page_point.y.is_finite()
      && page_point.x >= 0.0
      && page_point.y >= 0.0
    {
      hit_test_dom_with_indices(dom, &index, &box_index, fragment_tree, page_point)
    } else {
      None
    };
    let hover_is_drop_target = hit.as_ref().is_some_and(|hit| {
      self.drag_drop_active_kind().is_some()
        && hit.is_editable_text_drop_target_candidate
        && !node_is_disabled(&index, hit.dom_node_id)
        && !node_or_ancestor_is_inert(&index, hit.dom_node_id)
    });
    let new_chain = hit
      .as_ref()
      .and_then(|hit| nearest_element_ancestor(&index, hit.styled_node_id))
      .map(|target| collect_element_chain_with_label_associated_controls(&index, target))
      .unwrap_or_default();

    let changed = self.state.hover_chain() != new_chain.as_slice();
    if changed {
      self.state.set_hover_chain(new_chain);
    }
    // Tooltip semantics: prefer the semantic hit target (e.g. `<area>` for client-side image maps).
    // If it doesn't have a `title`, fall back to the hovered element chain.
    let tooltip = hit
      .as_ref()
      .and_then(|hit| Self::title_attribute_tooltip(&index, hit.dom_node_id))
      .or_else(|| Self::hover_tooltip_from_title_attributes(&index, self.state.hover_chain()));
    if tooltip != self.hover_tooltip.as_deref() {
      self.hover_tooltip = tooltip.map(|title| title.to_string());
    }
    (dom_changed | changed, hit, hover_is_drop_target)
  }

  /// Handle mouse wheel stepping for a focused `<input type="number">`.
  ///
  /// Returns `Some(dom_changed)` when the wheel was consumed for numeric stepping, or `None` when
  /// the wheel should be treated as a normal page/element scroll gesture.
  ///
  /// The provided `fragment_tree` must already have element scroll offsets applied (e.g. via
  /// [`crate::interaction::fragment_tree_with_scroll`]).
  pub fn wheel_step_number_input(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    scroll: &ScrollState,
    viewport_point: Point,
    delta_y_css: f32,
  ) -> Option<bool> {
    self.modality = InputModality::Pointer;

    let Some(focused) = self.state.focused else {
      return None;
    };

    let delta_steps = if delta_y_css < 0.0 {
      1
    } else if delta_y_css > 0.0 {
      -1
    } else {
      return None;
    };

    let mut index = DomIndexMut::new(dom);
    if !index
      .node(focused)
      .is_some_and(|node| is_input(node) && input_type(node).eq_ignore_ascii_case("number"))
    {
      return None;
    }

    let page_point = viewport_point.translate(scroll.viewport);
    let box_index = HitTestBoxIndex::new(box_tree);
    let hit = hit_test_dom_with_indices(dom, &index, &box_index, fragment_tree, page_point)?;
    if hit.dom_node_id != focused {
      return None;
    }

    let dom_changed = self.step_number_input(&mut index, focused, delta_steps);
    Some(dom_changed)
  }

  /// Begin active state (pointer down target + ancestors) and set modality=Pointer.
  /// `viewport_point` is in viewport coordinates; this method converts it to a page point by
  /// translating it by `scroll.viewport`.
  ///
  /// The provided `fragment_tree` must already have element scroll offsets applied (e.g. via
  /// [`crate::interaction::fragment_tree_with_scroll`]).
  pub fn pointer_down(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    scroll: &ScrollState,
    viewport_point: Point,
  ) -> bool {
    self.pointer_down_with_click_count(
      dom,
      box_tree,
      fragment_tree,
      scroll,
      viewport_point,
      PointerButton::Primary,
      PointerModifiers::NONE,
      1,
    )
  }

  /// Like [`InteractionEngine::pointer_down_with_click_count`], but also returns the hit-test
  /// result used to resolve the pointer-down target.
  ///
  /// Returning the hit test allows UI layers to reuse the interaction engine's source-of-truth
  /// target resolution for tasks like JS event dispatch without performing a second DOM hit test.
  pub fn pointer_down_with_click_count_and_hit(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    scroll: &ScrollState,
    viewport_point: Point,
    button: PointerButton,
    modifiers: PointerModifiers,
    click_count: u8,
  ) -> (bool, Option<HitTestResult>) {
    self.modality = InputModality::Pointer;

    self.range_drag = None;
    self.number_spin = None;
    self.text_drag = None;
    self.text_drag_drop = None;
    self.document_drag = None;
    self.document_selection_drag_drop = None;
    self.link_drag = None;
    self.pending_text_drop_move = None;
    let prev_doc_selection = self.state.document_selection.clone();

    let page_point = viewport_point.translate(scroll.viewport);
    let mut index = DomIndexMut::new(dom);
    let box_index = HitTestBoxIndex::new(box_tree);
    let down_hit = hit_test_dom_with_indices(dom, &index, &box_index, fragment_tree, page_point);
    let down_target = down_hit.as_ref().map(|hit| hit.dom_node_id);

    if matches!(button, PointerButton::Primary) {
      if let Some(hit) = down_hit.as_ref().filter(|hit| hit.kind == HitTestKind::Link) {
        self.link_drag = Some(LinkDragState {
          node_id: hit.dom_node_id,
          down_point: page_point,
          active: false,
        });
      }
    }
    let new_chain = down_target
      .map(|target| collect_element_chain_with_label_associated_controls(&index, target))
      .unwrap_or_default();

    let changed = self.state.active_chain() != new_chain.as_slice();
    if changed {
      self.state.set_active_chain(new_chain);
    }
    self.pointer_down_target = down_target;

    let mut dom_changed = changed;
    if let Some(hit) = down_hit.as_ref() {
      if matches!(button, PointerButton::Primary)
        && index.node(hit.dom_node_id).is_some_and(is_range_input)
      {
        self.range_drag = Some(RangeDragState {
          node_id: hit.dom_node_id,
          box_id: hit.box_id,
        });
        self.ensure_form_default_snapshot_for_control(&index, hit.dom_node_id);
        let changed = update_range_value_from_pointer(
          &mut index,
          &box_index,
          fragment_tree,
          hit.dom_node_id,
          hit.box_id,
          page_point,
        );
        dom_changed |= changed;
        if changed {
          dom_changed |= self.mark_user_validity(hit.dom_node_id);
        }
      }

      // Click-to-place caret / begin selection dragging for focused text controls.
      if matches!(button, PointerButton::Primary)
        && index
          .node(hit.dom_node_id)
          .is_some_and(|node| is_text_input(node) || is_textarea(node))
      {
        let focus_before = self.state.focused;
        let spin_direction = number_input_spin_direction_at_point(
          &index,
          &box_index,
          fragment_tree,
          hit.dom_node_id,
          hit.box_id,
          page_point,
        );
        if is_focusable_interactive_element(&index, hit.dom_node_id) {
          dom_changed |= self.set_focus(&mut index, Some(hit.dom_node_id), false);
        }

        // Only update caret/selection state when the text control is (now) focused.
        if self.state.focused == Some(hit.dom_node_id) {
          // Clicking the number input spinner should not move the caret or start selection dragging.
          if let Some(direction) = spin_direction {
            self.number_spin = Some(NumberSpinState {
              node_id: hit.dom_node_id,
              box_id: hit.box_id,
              direction,
            });
            return (dom_changed, down_hit);
          }

          let (caret, caret_affinity) = caret_index_for_text_control_point(
            &index,
            &box_index,
            fragment_tree,
            scroll,
            hit.dom_node_id,
            hit.box_id,
            page_point,
          )
          .unwrap_or((0, CaretAffinity::Downstream));

          let node = index.node(hit.dom_node_id);
          let is_textarea = node.is_some_and(is_textarea);
          let current_value = node
            .map(|node| {
              if is_textarea {
                textarea_value_for_editing(node)
              } else {
                node.get_attribute_ref("value").unwrap_or("").to_string()
              }
            })
            .unwrap_or_default();
          let current_len = current_value.chars().count();
          let down_caret = caret.min(current_len);

          let click_count = click_count.clamp(1, 3);
          let click_count = if click_count > 1 && focus_before != Some(hit.dom_node_id) {
            1
          } else {
            click_count
          };

          let shift_extend = modifiers.shift() && focus_before == Some(hit.dom_node_id);

          // If this is a single-click inside an existing selection highlight of the already-focused
          // control, defer caret collapse until mouseup so we can interpret the gesture as a text
          // drag-and-drop.
          let mut started_drag_drop = false;
          if click_count == 1 && !shift_extend && focus_before == Some(hit.dom_node_id) {
            if let Some((sel_start, sel_end)) = self
              .text_edit
              .as_ref()
              .filter(|state| state.node_id == hit.dom_node_id)
              .and_then(|state| state.selection())
            {
              let caret_in_bounds = down_caret;
              // `caret_index_for_text_control_point` returns the nearest caret stop. At the edges of
              // the selection highlight, clicks can legitimately quantize to `sel_start`/`sel_end`
              // (e.g. clicking inside the first/last selected glyph). Treat boundary caret indices
              // as inside-selection so selection collapse is deferred until mouseup (browser-like).
              if caret_in_bounds >= sel_start && caret_in_bounds <= sel_end {
                let start_byte = byte_offset_for_char_idx(&current_value, sel_start);
                let end_byte = byte_offset_for_char_idx(&current_value, sel_end);
                if start_byte < end_byte && end_byte <= current_value.len() {
                  let text = current_value[start_byte..end_byte].to_string();
                  self.text_drag_drop = Some(TextDragDropState::Candidate(TextDragDropCandidate {
                    node_id: hit.dom_node_id,
                    box_id: hit.box_id,
                    down_point: page_point,
                    down_caret: caret_in_bounds,
                    down_caret_affinity: caret_affinity,
                    selection: (sel_start, sel_end),
                    text,
                    focus_before,
                  }));
                  started_drag_drop = true;
                }
              }
            }
          }

          if !started_drag_drop
            && self
              .state
              .ime_preedit
              .as_ref()
              .is_some_and(|ime_state| ime_state.node_id == hit.dom_node_id)
          {
            dom_changed |= self.ime_cancel_internal();
          }

          let text_edit_changed = if started_drag_drop {
            false
          } else if let Some(state) = self
            .text_edit
            .as_mut()
            .filter(|state| state.node_id == hit.dom_node_id)
          {
            let prev = (state.caret, state.caret_affinity, state.selection_anchor);

            match click_count {
              1 => {
                state.set_caret_with_affinity_and_maybe_extend_selection(
                  caret.min(current_len),
                  caret_affinity,
                  shift_extend,
                );
              }
              2 => {
                if let Some((start, end)) =
                  word_selection_range(&current_value, caret.min(current_len))
                {
                  state.caret = end.min(current_len);
                  state.caret_affinity = CaretAffinity::Downstream;
                  state.selection_anchor = Some(start.min(current_len));
                  state.preferred_x = None;
                } else {
                  state.set_caret_with_affinity(caret.min(current_len), caret_affinity);
                  state.clear_selection();
                }
              }
              _ => {
                if is_textarea {
                  let (start, end) =
                    textarea_line_selection_range(&current_value, caret.min(current_len));
                  state.caret = end.min(current_len);
                  state.caret_affinity = CaretAffinity::Downstream;
                  state.selection_anchor = if start == end {
                    None
                  } else {
                    Some(start.min(current_len))
                  };
                  state.preferred_x = None;
                } else if current_len == 0 {
                  state.caret = 0;
                  state.caret_affinity = CaretAffinity::Downstream;
                  state.selection_anchor = None;
                  state.preferred_x = None;
                } else {
                  state.caret = current_len;
                  state.caret_affinity = CaretAffinity::Downstream;
                  state.selection_anchor = Some(0);
                  state.preferred_x = None;
                }
              }
            }

            (state.caret, state.caret_affinity, state.selection_anchor) != prev
          } else {
            let (caret, caret_affinity) = (caret.min(current_len), caret_affinity);
            let mut edit = TextEditState {
              node_id: hit.dom_node_id,
              caret,
              caret_affinity,
              selection_anchor: None,
              preferred_x: None,
            };

            match click_count {
              1 => {
                // No pre-existing edit state for this control, so shift-extend has nothing to extend
                // from; treat it like a normal caret placement.
              }
              2 => {
                if let Some((start, end)) = word_selection_range(&current_value, caret) {
                  edit.caret = end.min(current_len);
                  edit.caret_affinity = CaretAffinity::Downstream;
                  edit.selection_anchor = Some(start.min(current_len));
                }
              }
              _ => {
                if is_textarea {
                  let (start, end) = textarea_line_selection_range(&current_value, caret);
                  edit.caret = end.min(current_len);
                  edit.caret_affinity = CaretAffinity::Downstream;
                  edit.selection_anchor = if start == end {
                    None
                  } else {
                    Some(start.min(current_len))
                  };
                } else if current_len == 0 {
                  edit.caret = 0;
                  edit.caret_affinity = CaretAffinity::Downstream;
                  edit.selection_anchor = None;
                } else {
                  edit.caret = current_len;
                  edit.caret_affinity = CaretAffinity::Downstream;
                  edit.selection_anchor = Some(0);
                }
              }
            }

            self.text_edit = Some(edit);
            true
          };

          if !started_drag_drop {
            if text_edit_changed {
              dom_changed = true;
            }
            dom_changed |= self.sync_text_edit_paint_state();

            let drag_anchor = self
              .text_edit
              .as_ref()
              .filter(|state| state.node_id == hit.dom_node_id)
              .map(|state| state.selection_anchor.unwrap_or(state.caret))
              .unwrap_or(down_caret);
            let granularity = match click_count {
              2 => SelectionDragGranularity::Word,
              3 => SelectionDragGranularity::LineOrBlock,
              _ => SelectionDragGranularity::Char,
            };
            let initial_range = if matches!(granularity, SelectionDragGranularity::Char) {
              None
            } else {
              self
                .text_edit
                .as_ref()
                .filter(|state| state.node_id == hit.dom_node_id)
                .and_then(|state| state.selection())
            };
            self.text_drag = Some(TextDragState {
              node_id: hit.dom_node_id,
              box_id: hit.box_id,
              anchor: drag_anchor,
              down_caret,
              initial_range,
              granularity,
              focus_before,
            });
          }
        }
      }
    }

    // Document selection (non-form-control). This is tracked separately from focused text-control
    // selection.
    //
    // For now, only primary-button gestures participate in document selection.
    if matches!(button, PointerButton::Primary)
      && self.range_drag.is_none()
      && self.number_spin.is_none()
      && self.text_drag.is_none()
      && !down_hit
        .as_ref()
        .is_some_and(|hit| matches!(hit.kind, HitTestKind::FormControl))
    {
      let click_count = click_count.clamp(1, 3);
      if let Some((point, text_box_id)) =
        document_selection_hit_at_page_point(&box_index, fragment_tree, page_point)
      {
        let should_start_drag_drop = click_count == 1
          && !modifiers.shift()
          && !modifiers.command()
          && self.state.document_selection.as_ref().is_some_and(|sel| {
            sel.has_highlight() && document_selection_contains_point(sel, point)
          });

        if should_start_drag_drop {
          // Clicking inside an existing highlighted selection begins a drag candidate. Do not
          // collapse/replace the existing selection unless the gesture ends without exceeding the
          // drag threshold (handled in `pointer_up_with_scroll`).
          self.document_selection_drag_drop = Some(DocumentSelectionDragDropState {
            down_page_point: page_point,
            payload: None,
          });
        } else {
          let single_range = |range: DocumentSelectionRange| {
            let range = range.normalized();
            let mut ranges = DocumentSelectionRanges {
              ranges: vec![range],
              primary: 0,
              anchor: range.start,
              focus: range.end,
            };
            ranges.normalize();
            Some(DocumentSelectionState::Ranges(ranges))
          };

          let next = if modifiers.shift() {
            match self.state.document_selection.clone() {
              Some(DocumentSelectionState::Ranges(mut ranges)) => {
                ranges.focus = point;
                if ranges.primary < ranges.ranges.len() {
                  ranges.ranges[ranges.primary] = DocumentSelectionRange {
                    start: ranges.anchor,
                    end: ranges.focus,
                  }
                  .normalized();
                }
                ranges.normalize();
                Some(DocumentSelectionState::Ranges(ranges))
              }
              // No primary range to extend: treat as a normal click.
              _ => Some(DocumentSelectionState::Ranges(
                DocumentSelectionRanges::collapsed(point),
              )),
            }
          } else if modifiers.command() {
            match self.state.document_selection.clone() {
              Some(DocumentSelectionState::Ranges(mut ranges)) => {
                ranges.ranges.push(DocumentSelectionRange {
                  start: point,
                  end: point,
                });
                ranges.primary = ranges.ranges.len().saturating_sub(1);
                ranges.anchor = point;
                ranges.focus = point;
                ranges.normalize();
                Some(DocumentSelectionState::Ranges(ranges))
              }
              _ => Some(DocumentSelectionState::Ranges(
                DocumentSelectionRanges::collapsed(point),
              )),
            }
          } else {
            match click_count {
              2 => document_word_selection_range(box_tree, text_box_id, point)
                .and_then(single_range)
                .or_else(|| {
                  Some(DocumentSelectionState::Ranges(
                    DocumentSelectionRanges::collapsed(point),
                  ))
                }),
              3 => document_block_selection_range(box_tree, text_box_id, point)
                .and_then(single_range)
                .or_else(|| {
                  Some(DocumentSelectionState::Ranges(
                    DocumentSelectionRanges::collapsed(point),
                  ))
                }),
              _ => Some(DocumentSelectionState::Ranges(
                DocumentSelectionRanges::collapsed(point),
              )),
            }
          };

          self.state.set_document_selection(next);
          if let Some(DocumentSelectionState::Ranges(ranges)) =
            self.state.document_selection.as_ref()
          {
            let (granularity, initial_range) = if !modifiers.shift() && !modifiers.command() {
              match click_count {
                2 => (
                  SelectionDragGranularity::Word,
                  ranges
                    .ranges
                    .get(ranges.primary)
                    .copied()
                    .filter(|r| r.start != r.end),
                ),
                3 => (
                  SelectionDragGranularity::LineOrBlock,
                  ranges
                    .ranges
                    .get(ranges.primary)
                    .copied()
                    .filter(|r| r.start != r.end),
                ),
                _ => (SelectionDragGranularity::Char, None),
              }
            } else {
              (SelectionDragGranularity::Char, None)
            };
            self.document_drag = Some(DocumentDragState {
              down_point: point,
              initial_range,
              granularity,
            });
          }
        }
      } else if !modifiers.shift() && !modifiers.command() {
        // Plain click away from selectable text clears the selection.
        self.state.set_document_selection(None);
      }
    }
    let selection_changed = prev_doc_selection != self.state.document_selection;
    dom_changed |= selection_changed;
    (dom_changed, down_hit)
  }

  /// Like [`InteractionEngine::pointer_down`], but allows the UI layer to provide click metadata
  /// needed for browser-like text selection gestures in `<input>`/`<textarea>`.
  ///
  /// This is a convenience wrapper for call sites that don't need access to the hit-test result.
  /// Use [`InteractionEngine::pointer_down_with_click_count_and_hit`] to reuse the engine's hit
  /// target without performing an extra `hit_test_dom`.
  pub fn pointer_down_with_click_count(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    scroll: &ScrollState,
    viewport_point: Point,
    button: PointerButton,
    modifiers: PointerModifiers,
    click_count: u8,
  ) -> bool {
    self
      .pointer_down_with_click_count_and_hit(
        dom,
        box_tree,
        fragment_tree,
        scroll,
        viewport_point,
        button,
        modifiers,
        click_count,
      )
      .0
  }

  /// Prepare text-control caret/selection state for a context-menu (right-click) gesture.
  ///
  /// Native browser behaviour for `<input>` / `<textarea>`:
  /// - Right-click *inside* an existing selection preserves the selection.
  /// - Right-click outside the selection collapses it and moves the caret to the click point.
  ///
  /// The provided `fragment_tree` must already have element scroll offsets applied (e.g. via
  /// [`crate::interaction::fragment_tree_with_scroll`]).
  pub fn place_text_control_caret_for_context_menu_request(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    scroll: &ScrollState,
    node_id: usize,
    box_id: usize,
    page_point: Point,
  ) -> bool {
    self.modality = InputModality::Pointer;

    let mut index = DomIndexMut::new(dom);
    let Some(node) = index.node(node_id) else {
      return false;
    };

    let is_text_input = is_text_input(node);
    let is_textarea = is_textarea(node);
    if !(is_text_input || is_textarea) {
      return false;
    }

    // Focus the control like a normal pointer interaction would.
    let mut changed = false;
    if is_focusable_interactive_element(&index, node_id) {
      changed |= self.set_focus(&mut index, Some(node_id), false);
    }

    // Only update caret/selection state when the control is (now) focused.
    if self.state.focused != Some(node_id) {
      return changed;
    }

    let (caret, caret_affinity) = caret_index_for_text_control_point(
      &index,
      box_tree,
      fragment_tree,
      scroll,
      node_id,
      box_id,
      page_point,
    )
    .unwrap_or((0, CaretAffinity::Downstream));

    let current_value = index
      .node(node_id)
      .map(|node| {
        if is_textarea {
          textarea_value_for_editing(node)
        } else {
          node.get_attribute_ref("value").unwrap_or("").to_string()
        }
      })
      .unwrap_or_default();
    let current_len = current_value.chars().count();
    let caret = caret.min(current_len);

    // Preserve an existing selection if the right-click fell within it.
    if let Some(edit) = self
      .text_edit
      .as_ref()
      .filter(|edit| edit.node_id == node_id)
    {
      let mut edit = *edit;
      edit.caret = edit.caret.min(current_len);
      edit.selection_anchor = edit.selection_anchor.map(|a| a.min(current_len));
      if let Some((start, end)) = edit.selection() {
        if caret >= start && caret <= end {
          return changed;
        }
      }
    }

    // Otherwise collapse selection and place caret at the click point.
    let before = self.text_edit;
    match self.text_edit.as_mut() {
      Some(edit) if edit.node_id == node_id => {
        edit.caret = caret;
        edit.caret_affinity = caret_affinity;
        edit.selection_anchor = None;
        edit.preferred_x = None;
      }
      _ => {
        self.text_edit = Some(TextEditState {
          node_id,
          caret,
          caret_affinity,
          selection_anchor: None,
          preferred_x: None,
        });
      }
    }
    if self.text_edit != before {
      changed = true;
    }
    changed |= self.sync_text_edit_paint_state();

    changed
  }

  fn remap_engine_ids_after_dom_change(
    &mut self,
    old_index: &DomIndexMut,
    new_ids: &HashMap<*const DomNode, usize>,
  ) {
    fn remap_vec(
      ids: &mut Vec<usize>,
      old_index: &DomIndexMut,
      new_ids: &HashMap<*const DomNode, usize>,
    ) {
      for id in ids.iter_mut() {
        let Some(ptr) = old_index.id_to_node.get(*id).copied() else {
          continue;
        };
        if ptr.is_null() {
          continue;
        }
        if let Some(new_id) = new_ids.get(&(ptr as *const DomNode)) {
          *id = *new_id;
        }
      }
    }
    fn remap_opt(
      id: &mut Option<usize>,
      old_index: &DomIndexMut,
      new_ids: &HashMap<*const DomNode, usize>,
    ) {
      let Some(old) = *id else { return };
      let Some(ptr) = old_index.id_to_node.get(old).copied() else {
        *id = None;
        return;
      };
      if ptr.is_null() {
        *id = None;
        return;
      }
      *id = new_ids.get(&(ptr as *const DomNode)).copied();
    }

    self
      .state
      .mutate_hover_chain(|ids| remap_vec(ids, old_index, new_ids));
    self
      .state
      .mutate_active_chain(|ids| remap_vec(ids, old_index, new_ids));
    remap_opt(&mut self.pointer_down_target, old_index, new_ids);
    remap_opt(&mut self.last_click_target, old_index, new_ids);
    remap_opt(&mut self.last_form_submitter, old_index, new_ids);
    if self.last_click_target.is_none() {
      self.last_click_target_element_id = None;
    }
    if self.last_form_submitter.is_none() {
      self.last_form_submitter_element_id = None;
    }
    if let Some(state) = &mut self.link_drag {
      let new_node_id = old_index
        .id_to_node
        .get(state.node_id)
        .copied()
        .filter(|ptr| !ptr.is_null())
        .and_then(|ptr| new_ids.get(&(ptr as *const DomNode)).copied());
      match new_node_id {
        Some(id) => state.node_id = id,
        None => self.link_drag = None,
      }
    }
    if let Some(state) = &mut self.range_drag {
      let new_node_id = old_index
        .id_to_node
        .get(state.node_id)
        .copied()
        .filter(|ptr| !ptr.is_null())
        .and_then(|ptr| new_ids.get(&(ptr as *const DomNode)).copied());
      match new_node_id {
        Some(id) => state.node_id = id,
        None => self.range_drag = None,
      }
    }
    remap_opt(&mut self.state.focused, old_index, new_ids);
    self
      .state
      .mutate_focus_chain(|ids| remap_vec(ids, old_index, new_ids));

    // Remap visited links.
    if !self.state.visited_links().is_empty() {
      let mut remapped = rustc_hash::FxHashSet::default();
      {
        let visited_links = self.state.visited_links();
        remapped.reserve(visited_links.len());
        for old in visited_links.iter().copied() {
          let Some(ptr) = old_index.id_to_node.get(old).copied() else {
            continue;
          };
          if ptr.is_null() {
            continue;
          }
          if let Some(&new_id) = new_ids.get(&(ptr as *const DomNode)) {
            remapped.insert(new_id);
          }
        }
      }
      *self.state.visited_links_mut() = remapped;
    }

    // Remap active IME preedit state.
    if let Some(preedit) = &mut self.state.ime_preedit {
      let new_node_id = old_index
        .id_to_node
        .get(preedit.node_id)
        .copied()
        .filter(|ptr| !ptr.is_null())
        .and_then(|ptr| new_ids.get(&(ptr as *const DomNode)).copied());
      match new_node_id {
        Some(id) => preedit.node_id = id,
        None => self.state.ime_preedit = None,
      }
    }

    if let Some(edit) = &mut self.text_edit {
      let new_node_id = old_index
        .id_to_node
        .get(edit.node_id)
        .copied()
        .filter(|ptr| !ptr.is_null())
        .and_then(|ptr| new_ids.get(&(ptr as *const DomNode)).copied());
      match new_node_id {
        Some(new_id) => edit.node_id = new_id,
        None => self.text_edit = None,
      }
    }

    if let Some(state) = &mut self.text_drag {
      let new_node_id = old_index
        .id_to_node
        .get(state.node_id)
        .copied()
        .filter(|ptr| !ptr.is_null())
        .and_then(|ptr| new_ids.get(&(ptr as *const DomNode)).copied());
      match new_node_id {
        Some(new_id) => {
          state.node_id = new_id;
          remap_opt(&mut state.focus_before, old_index, new_ids);
        }
        None => self.text_drag = None,
      }
    }

    if let Some(state) = &mut self.text_drag_drop {
      let old_node_id = state.node_id();
      let new_node_id = old_index
        .id_to_node
        .get(old_node_id)
        .copied()
        .filter(|ptr| !ptr.is_null())
        .and_then(|ptr| new_ids.get(&(ptr as *const DomNode)).copied());
      match new_node_id {
        Some(new_id) => match state {
          TextDragDropState::Candidate(candidate) => {
            candidate.node_id = new_id;
            remap_opt(&mut candidate.focus_before, old_index, new_ids);
          }
          TextDragDropState::Active(active) => {
            active.node_id = new_id;
            remap_opt(&mut active.focus_before, old_index, new_ids);
          }
        },
        None => self.text_drag_drop = None,
      }
    }

    if let Some(state) = &mut self.pending_text_drop_move {
      let ptr = old_index
        .id_to_node
        .get(state.node_id)
        .copied()
        .unwrap_or(std::ptr::null_mut());
      if ptr.is_null() {
        self.pending_text_drop_move = None;
      } else if let Some(&new_id) = new_ids.get(&(ptr as *const DomNode)) {
        state.node_id = new_id;
      } else {
        self.pending_text_drop_move = None;
      }
    }

    if let Some(state) = &mut self.document_drag {
      let mut ok = true;
      let mut remap_point = |point: &mut DocumentSelectionPoint| {
        let ptr = old_index
          .id_to_node
          .get(point.node_id)
          .copied()
          .unwrap_or(std::ptr::null_mut());
        if ptr.is_null() {
          ok = false;
          return;
        }
        let Some(&new_id) = new_ids.get(&(ptr as *const DomNode)) else {
          ok = false;
          return;
        };
        point.node_id = new_id;
      };

      remap_point(&mut state.down_point);
      if let Some(range) = &mut state.initial_range {
        remap_point(&mut range.start);
        remap_point(&mut range.end);
      }
      if !ok {
        self.document_drag = None;
      }
    }

    // Remap document selection endpoints (used for clipboard copy of document text).
    let mut clear_document_selection = false;
    if let Some(selection) = &mut self.state.document_selection {
      if let DocumentSelectionState::Ranges(ranges) = selection {
        let mut ok = true;
        let mut remap_point = |point: &mut DocumentSelectionPoint| {
          let ptr = old_index
            .id_to_node
            .get(point.node_id)
            .copied()
            .unwrap_or(std::ptr::null_mut());
          if ptr.is_null() {
            ok = false;
            return;
          }
          let Some(&new_id) = new_ids.get(&(ptr as *const DomNode)) else {
            ok = false;
            return;
          };
          point.node_id = new_id;
        };

        remap_point(&mut ranges.anchor);
        remap_point(&mut ranges.focus);
        for range in &mut ranges.ranges {
          remap_point(&mut range.start);
          remap_point(&mut range.end);
        }

        if ok {
          ranges.normalize();
        } else {
          clear_document_selection = true;
        }
      }
    }
    if clear_document_selection {
      self.state.document_selection = None;
      self.document_drag = None;
      self.document_selection_drag_drop = None;
    }

    // Ensure the paint-only caret/selection state stays in sync with the remapped internal edit
    // state.
    let _ = self.sync_text_edit_paint_state();

    // Node ids used by both CSS selector matching and paint-only state may have been remapped above.
    // Ensure cached interaction digests are recomputed before the next render.
    self.state.mark_all_hashes_dirty();
  }
  /// Like [`InteractionEngine::pointer_up_with_scroll`], but also returns the hit-test result used
  /// to resolve the pointer-up target.
  ///
  /// Returning the hit test allows UI layers to reuse the interaction engine's source-of-truth hit
  /// target for tasks like JS event dispatch without performing a second DOM hit test.
  pub fn pointer_up_with_scroll_and_hit(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    scroll: &ScrollState,
    viewport_point: Point,
    button: PointerButton,
    modifiers: PointerModifiers,
    allow_default_drop: bool,
    document_url: &str,
    base_url: &str,
  ) -> (bool, InteractionAction, Option<HitTestResult>) {
    self.last_click_target = None;
    self.last_click_target_element_id = None;
    self.last_form_submitter = None;
    self.last_form_submitter_element_id = None;

    let link_drag = self.link_drag.take();
    let range_drag = self.range_drag.take();
    let number_spin = self.number_spin.take();
    let text_drag = self.text_drag.take();
    let text_drag_drop = self.text_drag_drop.take();
    let document_drag = self.document_drag.take();
    let document_selection_drag_drop = self.document_selection_drag_drop.take();

    let mut drag_drop_candidate: Option<TextDragDropCandidate> = None;
    let mut drag_drop_active: Option<TextDragDropActive> = None;
    if let Some(state) = text_drag_drop {
      match state {
        TextDragDropState::Candidate(candidate) => drag_drop_candidate = Some(candidate),
        TextDragDropState::Active(active) => drag_drop_active = Some(active),
      }
    }
    let mut suppress_click = (document_drag.is_some()
      && self
        .state
        .document_selection
        .as_ref()
        .is_some_and(|sel| sel.has_highlight()))
      || drag_drop_active.is_some()
      || document_selection_drag_drop
        .as_ref()
        .is_some_and(|state| state.payload.is_some());
    let prev_focus = text_drag
      .as_ref()
      .map(|state| state.focus_before)
      .unwrap_or_else(|| {
        drag_drop_candidate
          .as_ref()
          .map(|state| state.focus_before)
          .unwrap_or_else(|| {
            drag_drop_active
              .as_ref()
              .map(|state| state.focus_before)
              .unwrap_or(self.state.focused)
          })
      });

    let page_point = viewport_point.translate(scroll.viewport);
    // Link drag suppression: if the pointer moves beyond a small threshold while holding down on a
    // link, do not treat the gesture as a click (mirrors native browsers).
    const LINK_DRAG_THRESHOLD_PX: f32 = 5.0;
    if link_drag.as_ref().is_some_and(|state| {
      if state.active {
        return true;
      }
      // Ignore sentinel/out-of-page points (the UI sends negative coordinates when leaving the
      // page image).
      page_point.x.is_finite()
        && page_point.y.is_finite()
        && page_point.x >= 0.0
        && page_point.y >= 0.0
        && state.down_point.distance_to(page_point) >= LINK_DRAG_THRESHOLD_PX
    }) {
      suppress_click = true;
    }
    let mut index = DomIndexMut::new(dom);
    let box_index = HitTestBoxIndex::new(box_tree);
    let up_hit = hit_test_dom_with_indices(dom, &index, &box_index, fragment_tree, page_point);
    let up_semantic = up_hit.as_ref().map(|hit| hit.dom_node_id);

    let down_semantic = self.pointer_down_target;

    // Clear active chain unconditionally.
    let mut dom_changed = false;
    if let Some(state) = range_drag {
      // Mirror the sentinel handling in `pointer_move` for range drags: when the pointer leaves the
      // page image the worker synthesizes a negative page-point. Do not treat that as dragging the
      // slider to its minimum; keep the last in-page value instead.
      if page_point.x.is_finite()
        && page_point.y.is_finite()
        && page_point.x >= 0.0
        && page_point.y >= 0.0
      {
        self.ensure_form_default_snapshot_for_control(&index, state.node_id);
        let changed = update_range_value_from_pointer(
          &mut index,
          &box_index,
          fragment_tree,
          state.node_id,
          state.box_id,
          page_point,
        );
        dom_changed |= changed;
        if changed {
          dom_changed |= self.mark_user_validity(state.node_id);
        }
      }
    }
    let active_changed = !self.state.active_chain().is_empty();
    self.state.clear_active_chain();
    self.pointer_down_target = None;
    dom_changed |= active_changed;

    // Text drag-and-drop: when active, suppress click behavior and prepare a deferred default
    // insertion into the drop target.
    if let Some(active) = drag_drop_active {
      self.pending_text_drop_move = None;
      let mut pending_drop: Option<(usize, String)> = None;

      if allow_default_drop && matches!(button, PointerButton::Primary) {
        if let Some(hit) = up_hit.as_ref() {
          let target_id = hit.dom_node_id;
          if index
            .node(target_id)
            .is_some_and(|node| is_text_input(node) || is_textarea(node))
            && !(node_or_ancestor_is_inert(&index, target_id)
              || node_is_disabled(&index, target_id)
              || node_is_readonly(&index, target_id))
          {
            if is_focusable_interactive_element(&index, target_id) {
              dom_changed |= self.set_focus(&mut index, Some(target_id), false);
            }

            let (caret, caret_affinity) = caret_index_for_text_control_point(
              &index,
              &box_index,
              fragment_tree,
              scroll,
              target_id,
              hit.box_id,
              page_point,
            )
            .unwrap_or((0, CaretAffinity::Downstream));

            // Update caret/selection state like a normal pointer interaction, but do not mutate the
            // value until `apply_text_drop` is called.
            if self.state.focused == Some(target_id) {
              if self
                .state
                .ime_preedit
                .as_ref()
                .is_some_and(|ime_state| ime_state.node_id == target_id)
              {
                dom_changed |= self.ime_cancel_internal();
              }
              match self.text_edit.as_mut().filter(|edit| edit.node_id == target_id) {
                Some(edit) => {
                  edit.caret = caret;
                  edit.caret_affinity = caret_affinity;
                  edit.selection_anchor = None;
                  edit.preferred_x = None;
                }
                None => {
                  self.text_edit = Some(TextEditState {
                    node_id: target_id,
                    caret,
                    caret_affinity,
                    selection_anchor: None,
                    preferred_x: None,
                  });
                }
              }
              dom_changed |= self.sync_text_edit_paint_state();
            }

            let force_copy = modifiers.command();
            let same_control = target_id == active.node_id;
            if same_control && !force_copy {
              self.pending_text_drop_move = Some(PendingTextDropMove {
                node_id: target_id,
                selection: active.selection,
              });
            }

            pending_drop = Some((target_id, active.text));
          }
        }
      }

      let action = if let Some((target_dom_id, text)) = pending_drop {
        InteractionAction::TextDrop { target_dom_id, text }
      } else if self.state.focused != prev_focus {
        InteractionAction::FocusChanged {
          node_id: self.state.focused,
        }
      } else {
        InteractionAction::None
      };

      return (dom_changed, action, up_hit);
    }

    if let Some(drag_drop) = document_selection_drag_drop {
      match drag_drop.payload {
        Some(payload) => {
          // Active drag-drop: dropping selected document text into a text control.
          if allow_default_drop {
            if let Some(hit) = up_hit.as_ref() {
              let target_id = hit.dom_node_id;
              let is_text_control = index
                .node(target_id)
                .is_some_and(|node| is_text_input(node) || is_textarea(node));
              if is_text_control
                && !node_or_ancestor_is_inert(&index, target_id)
                && !node_is_disabled(&index, target_id)
                && !node_is_readonly(&index, target_id)
              {
                if let Some((caret, affinity)) = caret_index_for_text_control_point(
                  &index,
                  &box_index,
                  fragment_tree,
                  scroll,
                  target_id,
                  hit.box_id,
                  page_point,
                ) {
                  let preserved_selection = self.state.document_selection.clone();

                  // Focus the drop target (pointer-driven focus, so `focus_visible=false`).
                  if is_focusable_interactive_element(&index, target_id) {
                    dom_changed |= self.set_focus(&mut index, Some(target_id), false);
                    // Restore the document selection for copy semantics.
                    self
                      .state
                      .set_document_selection(preserved_selection.clone());

                    // Place caret/selection state for the pending drop so `apply_text_drop` inserts
                    // at the drop location.
                    if self.state.focused == Some(target_id) {
                      match self.text_edit.as_mut().filter(|edit| edit.node_id == target_id) {
                        Some(edit) => {
                          edit.caret = caret;
                          edit.caret_affinity = affinity;
                          edit.selection_anchor = None;
                          edit.preferred_x = None;
                        }
                        None => {
                          self.text_edit = Some(TextEditState {
                            node_id: target_id,
                            caret,
                            caret_affinity: affinity,
                            selection_anchor: None,
                            preferred_x: None,
                          });
                        }
                      }
                      dom_changed |= self.sync_text_edit_paint_state();
                    }
                  }

                  // Defer default insertion until `apply_text_drop` is called.
                  let action = InteractionAction::TextDrop {
                    target_dom_id: target_id,
                    text: payload,
                  };
                  // Ensure selection remains highlighted for copy semantics even if focusing cleared
                  // it above.
                  self.state.set_document_selection(preserved_selection);
                  return (dom_changed, action, up_hit);
                }
              }
            }
          }

          let action = if self.state.focused != prev_focus {
            InteractionAction::FocusChanged {
              node_id: self.state.focused,
            }
          } else {
            InteractionAction::None
          };

          return (dom_changed, action, up_hit);
        }
        None => {
          // Drag candidate ended without activation: fall back to normal click behavior by
          // collapsing the document selection to the original down point.
          let before = self.state.document_selection.clone();
          let next = if let Some(point) = document_selection_point_at_page_point(
            &box_index,
            fragment_tree,
            drag_drop.down_page_point,
          ) {
            Some(DocumentSelectionState::Ranges(DocumentSelectionRanges::collapsed(
              point,
            )))
          } else {
            None
          };
          self.state.set_document_selection(next);
          let selection_changed = before != self.state.document_selection;
          dom_changed |= selection_changed;
        }
      }
    }

    let mut click_qualifies = match (down_semantic, up_semantic) {
      (Some(down), Some(up)) => down == up || is_ancestor_or_self(&index, down, up),
      (None, None) => true,
      _ => false,
    };
    if suppress_click {
      click_qualifies = false;
    }

    let mut action = InteractionAction::None;
    let is_primary_button = matches!(button, PointerButton::Primary);
    let allow_link_activation = matches!(button, PointerButton::Primary | PointerButton::Middle);

    // Text drag-and-drop candidate: if the pointer was pressed inside an existing selection but we
    // never crossed the drag threshold, treat this as a normal click (collapse selection to the
    // down-point caret) on mouseup.
    if let Some(candidate) = drag_drop_candidate.take() {
      if is_primary_button
        && click_qualifies
        && down_semantic == Some(candidate.node_id)
        && self.state.focused == Some(candidate.node_id)
      {
        let current_len = index
          .node(candidate.node_id)
          .map(|node| {
            if is_textarea(node) {
              textarea_value_for_editing(node).chars().count()
            } else {
              node
                .get_attribute_ref("value")
                .unwrap_or("")
                .chars()
                .count()
            }
          })
          .unwrap_or(0);

        let next_caret = candidate.down_caret.min(current_len);
        if self
          .state
          .ime_preedit
          .as_ref()
          .is_some_and(|ime_state| ime_state.node_id == candidate.node_id)
        {
          dom_changed |= self.ime_cancel_internal();
        }
        let selection_changed = if let Some(edit) = self
          .text_edit
          .as_mut()
          .filter(|edit| edit.node_id == candidate.node_id)
        {
          let prev = (edit.caret, edit.caret_affinity, edit.selection_anchor);
          edit.caret = next_caret;
          edit.caret_affinity = candidate.down_caret_affinity;
          edit.selection_anchor = None;
          edit.preferred_x = None;
          (edit.caret, edit.caret_affinity, edit.selection_anchor) != prev
        } else {
          self.text_edit = Some(TextEditState {
            node_id: candidate.node_id,
            caret: next_caret,
            caret_affinity: candidate.down_caret_affinity,
            selection_anchor: None,
            preferred_x: None,
          });
          true
        };

        if selection_changed {
          dom_changed = true;
        }
        dom_changed |= self.sync_text_edit_paint_state();
      }
    }

    let mut click_target = if click_qualifies { down_semantic } else { None };
    // `<details>/<summary>`: clicking the "details summary" toggles the parent `<details open>`
    // attribute.
    //
    // We consider it a summary "click" when both the pointer-down and pointer-up semantic targets
    // are within the same details-summary subtree. This matches typical activation behavior for
    // nested content (e.g. `<summary><span>...</span></summary>`): drifting between descendants
    // should still toggle the `<details>`.
    //
    // Compute this from the original semantic targets before label resolution so summary clicks
    // inside a `<label>` still toggle.
    let summary_toggle = if suppress_click {
      None
    } else {
      match (down_semantic, up_semantic) {
        (Some(down), Some(up)) => {
          let down_summary = nearest_details_summary(&index, down);
          let up_summary = nearest_details_summary(&index, up);
          match (down_summary, up_summary) {
            (Some(down_summary), Some(up_summary)) if down_summary == up_summary => {
              Some(down_summary)
            }
            _ => None,
          }
        }
        _ => None,
      }
    };
    if is_primary_button {
      if let Some(target_id) = click_target {
        if index.node(target_id).is_some_and(is_label) {
          if let Some(control) = find_label_associated_control(&index, target_id) {
            click_target = Some(control);
          }
        }
      }
    }

    // Track the UI-facing click target for both primary ("click") and middle ("auxclick") presses
    // so higher-level layers can dispatch JS DOM events and honor `preventDefault()` before
    // committing default actions (e.g. link navigations).
    if matches!(button, PointerButton::Primary | PointerButton::Middle) {
      self.last_click_target = click_target;
      self.last_click_target_element_id =
        click_target.and_then(|target_id| element_id_for_node(&index, target_id));
    }
    // If a click lands within a details summary but does not qualify for our generic semantic
    // click-target heuristics (e.g. pointer-down on a non-interactive descendant, pointer-up on the
    // summary background), still report the `<summary>` as the click target so higher-level layers
    // can dispatch `"click"` events.
    if is_primary_button && self.last_click_target.is_none() {
      if let Some((summary_id, _details_id)) = summary_toggle {
        self.last_click_target = Some(summary_id);
        self.last_click_target_element_id = element_id_for_node(&index, summary_id);
      }
    }

    if click_qualifies {
      if let Some(target_id) = click_target {
        if node_or_ancestor_is_inert(&index, target_id) {
          // Inert subtrees are not interactive: do not navigate, focus, or mutate form state.
        } else if index.node(target_id).is_some_and(is_select) {
          let snapshot = select_control_snapshot_from_box_tree(box_tree, target_id);
          let computed_disabled = snapshot
            .as_ref()
            .is_some_and(|(_, _, disabled, _)| *disabled);
          if is_primary_button
            && is_focusable_interactive_element(&index, target_id)
            && !computed_disabled
          {
            dom_changed |= self.set_focus(&mut index, Some(target_id), false);
          }

          let disabled = is_disabled_or_inert(&index, target_id) || computed_disabled;

          if is_primary_button && !disabled {
            if let Some((select_box_id, control, _, style)) = snapshot.as_ref() {
              self.ensure_form_default_snapshot_for_control(&index, target_id);
              let changed = apply_select_listbox_click(
                dom,
                fragment_tree,
                page_point,
                target_id,
                *select_box_id,
                scroll,
                control,
                style,
                modifiers,
                &mut self.select_listbox_anchor,
              );
              dom_changed |= changed;
              if changed {
                dom_changed |= self.mark_user_validity(target_id);
              }
            }
          }

          if is_primary_button && !disabled {
            if let Some((_, control, _, _)) = snapshot.as_ref() {
              let is_dropdown = !control.multiple && control.size == 1;
              if is_dropdown {
                action = InteractionAction::OpenSelectDropdown {
                  select_node_id: target_id,
                  control: control.clone(),
                };
              }
            }
          }
        } else if is_primary_button {
          if let Some(kind) = index.node(target_id).and_then(media_controls_kind) {
            action = InteractionAction::OpenMediaControls {
              media_node_id: target_id,
              kind,
            };
          }
        } else {
          // If the click happened within a details summary but did not resolve to a focusable target
          // (e.g. `<summary><span>...</span></summary>`), focus the summary like a native button.
          if is_primary_button {
            if let Some((summary_id, _details_id)) = summary_toggle {
              if !node_or_ancestor_is_inert(&index, summary_id) {
                if !is_focusable_interactive_element(&index, target_id) {
                  dom_changed |= self.set_focus(&mut index, Some(summary_id), false);
                }
              }
            }
          }

          if is_primary_button && is_focusable_interactive_element(&index, target_id) {
            dom_changed |= self.set_focus(&mut index, Some(target_id), false);
          }

          if allow_link_activation {
            if let Some(href) = index
              .node(target_id)
              .filter(|node| is_anchor_with_href(node))
              .and_then(|node| node.get_attribute_ref("href"))
            {
              let mut href_for_resolution = trim_ascii_whitespace(href).to_string();

              // Server-side image maps: `<img ismap>` inside `<a href>` appends `?x,y` to the anchor
              // URL before resolution.
              //
              // Precedence: If a client-side image map `<img usemap>` resolves to an `<area>`, the
              // click target becomes the `<area>` (not the `<a>`), so we only apply `ismap` when the
              // semantic target is an `<a>`.
              let target_is_a = index
                .node(target_id)
                .and_then(|node| node.tag_name())
                .is_some_and(|tag| tag.eq_ignore_ascii_case("a"));

              if target_is_a {
                if let Some(hit) = up_hit.as_ref() {
                  let event_target = nearest_element_ancestor(&index, hit.styled_node_id);
                  let event_target_is_img_ismap = event_target
                    .and_then(|id| index.node(id))
                    .is_some_and(|node| {
                      node
                        .tag_name()
                        .is_some_and(|tag| tag.eq_ignore_ascii_case("img"))
                        && node.get_attribute_ref("ismap").is_some()
                    });

                  if let Some(img_id) = event_target.filter(|_| event_target_is_img_ismap) {
                    if is_ancestor_or_self(&index, target_id, img_id) {
                      let img_fragment = fragment_tree
                        .hit_test(page_point)
                        .into_iter()
                        .find(|fragment| fragment.box_id() == Some(hit.box_id));
                      if let Some(img_fragment) = img_fragment {
                        if let Some(img_point) = image_maps::local_point_in_fragment(
                          fragment_tree,
                          img_fragment,
                          page_point,
                        ) {
                          let x = img_point.x.max(0.0).floor() as i32;
                          let y = img_point.y.max(0.0).floor() as i32;
                          href_for_resolution.push('?');
                          href_for_resolution.push_str(&format!("{x},{y}"));
                        }
                      }
                    }
                  }
                }
              }

              if let Some(resolved) = resolve_url(base_url, &href_for_resolution) {
                dom_changed |= self.state.insert_visited_link(target_id);

                let download_attr = index
                  .node(target_id)
                  .and_then(|node| node.get_attribute_ref("download"));
                let is_download = download_attr.is_some();
                let download_name = download_attr
                  .map(trim_ascii_whitespace)
                  .filter(|v| !v.is_empty())
                  .map(|v| v.to_string());

                let target_blank = index
                  .node(target_id)
                  .and_then(|node| node.get_attribute_ref("target"))
                  .is_some_and(|target| {
                    trim_ascii_whitespace(target).eq_ignore_ascii_case("_blank")
                  });

                let gesture_new_tab = matches!(button, PointerButton::Middle)
                  || (matches!(button, PointerButton::Primary) && modifiers.command());

                action = if is_download {
                  InteractionAction::Download {
                    href: resolved,
                    file_name: download_name,
                  }
                } else if target_blank || gesture_new_tab {
                  InteractionAction::OpenInNewTab { href: resolved }
                } else {
                  InteractionAction::Navigate { href: resolved }
                };
              }
            }
          }

          if is_primary_button {
            if let Some(spin) = number_spin.filter(|spin| spin.node_id == target_id) {
              let up_dir = up_hit
                .as_ref()
                .filter(|hit| hit.dom_node_id == target_id)
                .and_then(|hit| {
                  number_input_spin_direction_at_point(
                    &index,
                    &box_index,
                    fragment_tree,
                    target_id,
                    hit.box_id,
                    page_point,
                  )
                });
              if up_dir == Some(spin.direction) {
                let delta_steps = match spin.direction {
                  NumberSpinDirection::Up => 1,
                  NumberSpinDirection::Down => -1,
                };
                dom_changed |= self.step_number_input(&mut index, target_id, delta_steps);
              }
            } else if index.node(target_id).is_some_and(is_checkbox_input) {
              if !node_is_disabled(&index, target_id) {
                self.ensure_form_default_snapshot_for_control(&index, target_id);
                if let Some(node_mut) = index.node_mut(target_id) {
                  let changed = dom_mutation::toggle_checkbox(node_mut);
                  dom_changed |= changed;
                  if changed {
                    dom_changed |= self.mark_user_validity(target_id);
                  }
                }
              }
            } else if index.node(target_id).is_some_and(is_radio_input) {
              if !node_is_disabled(&index, target_id) {
                self.ensure_form_default_snapshot_for_control(&index, target_id);
                let changed = dom_mutation::activate_radio(dom, target_id);
                dom_changed |= changed;
                if changed {
                  dom_changed |= self.mark_user_validity(target_id);
                }
              }
            } else if index.node(target_id).is_some_and(is_file_input) {
              if !node_is_disabled(&index, target_id) {
                let multiple = index
                  .node(target_id)
                  .is_some_and(|node| node.get_attribute_ref("multiple").is_some());
                let accept = index
                  .node(target_id)
                  .and_then(|node| node.get_attribute_ref("accept"))
                  .map(trim_ascii_whitespace)
                  .filter(|v| !v.is_empty())
                  .map(|v| v.to_string());
                action = InteractionAction::OpenFilePicker {
                  input_node_id: target_id,
                  multiple,
                  accept,
                };
              }
            } else if let Some(kind) = index
              .node(target_id)
              .and_then(|node| date_time_input_kind(node))
            {
              if !node_is_disabled(&index, target_id) && !node_is_readonly(&index, target_id) {
                action = InteractionAction::OpenDateTimePicker {
                  input_node_id: target_id,
                  kind,
                };
              }
            } else if index.node(target_id).is_some_and(is_color_input) {
              if !node_is_disabled(&index, target_id) {
                action = InteractionAction::OpenColorPicker {
                  input_node_id: target_id,
                };
              }
            } else if index.node(target_id).is_some_and(is_reset_control) {
              if is_disabled_or_inert(&index, target_id) {
                // Disabled reset controls do not reset.
              } else {
                dom_changed |= self.perform_form_reset(&mut index, target_id);
              }
            } else if index.node(target_id).is_some_and(is_submit_control) {
              if node_is_disabled(&index, target_id) {
                // Disabled submit controls do not submit.
              } else {
                // A form submission attempt flips HTML "user validity" so `:user-invalid` matches.
                dom_changed |= self.mark_user_validity(target_id);
                dom_changed |= self.mark_form_user_validity(&index, target_id);
                let image_coords = if index.node(target_id).is_some_and(is_image_submit_input) {
                  let coords = up_hit
                    .as_ref()
                    .filter(|hit| hit.dom_node_id == target_id)
                    .and_then(|hit| {
                      fragment_tree
                        .hit_test(page_point)
                        .into_iter()
                        .find(|fragment| fragment.box_id() == Some(hit.box_id))
                    })
                    .and_then(|fragment| {
                      image_maps::local_point_in_fragment(fragment_tree, fragment, page_point)
                    })
                    .map(|point| {
                      let x = point.x.max(0.0).floor() as i32;
                      let y = point.y.max(0.0).floor() as i32;
                      (x, y)
                    })
                    .unwrap_or((0, 0));
                  Some(coords)
                } else {
                  None
                };

                if let Some(submission) = form_submission(
                  dom,
                  target_id,
                  image_coords,
                  document_url,
                  base_url,
                  Some(&self.state),
                ) {
                  self.last_form_submitter = Some(target_id);
                  self.last_form_submitter_element_id = element_id_for_node(&index, target_id);
                  let target_blank = resolve_form_owner(&index, target_id).is_some_and(|form_id| {
                    submission_target_is_blank(&index, Some(target_id), form_id)
                  });
                  match submission.method {
                    FormSubmissionMethod::Get => {
                      if target_blank {
                        action = InteractionAction::OpenInNewTab {
                          href: submission.url,
                        };
                      } else {
                        action = InteractionAction::Navigate {
                          href: submission.url,
                        };
                      }
                    }
                    FormSubmissionMethod::Post => {
                      if target_blank {
                        action = InteractionAction::OpenInNewTabRequest {
                          request: submission,
                        };
                      } else {
                        action = InteractionAction::NavigateRequest {
                          request: submission,
                        };
                      }
                    }
                  }
                }
              }
            }
          }
        }
      }

      // Blur when clicking outside focusable controls.
      //
      // If the pointer down started on a focusable target but the click was cancelled (pointer up
      // happened elsewhere / outside the page), we should not clear focus: typical browser UX
      // keeps the previously focused element focused in that case.
      if is_primary_button {
        let clicked_focusable =
          click_target.is_some_and(|id| is_focusable_interactive_element(&index, id));
        let down_prevents_blur = down_semantic.is_some_and(|id| {
          is_focusable_interactive_element(&index, id) || index.node(id).is_some_and(is_label)
        });
        if !clicked_focusable && !down_prevents_blur && prev_focus.is_some() {
          dom_changed |= self.set_focus(&mut index, None, false);
        }
      }
    }

    // Apply the default open/close toggle for `<details>` when the click is within its details
    // summary.
    if is_primary_button {
      if let Some((summary_id, details_id)) = summary_toggle {
        if !node_or_ancestor_is_inert(&index, summary_id) {
          dom_changed |= toggle_details_open(&mut index, details_id);
          // If there is no focusable semantic click target (e.g. pointer-up resolved to a sibling
          // element), still focus the summary.
          if click_target.is_none() {
            dom_changed |= self.set_focus(&mut index, Some(summary_id), false);
          }
        }
      }
    }

    // `OpenSelectDropdown` includes the focus update; do not replace it with `FocusChanged`.
    if matches!(action, InteractionAction::None) && self.state.focused != prev_focus {
      action = InteractionAction::FocusChanged {
        node_id: self.state.focused,
      };
    }

    (dom_changed, action, up_hit)
  }

  /// End active state, and if click qualifies, perform action:
  /// - link: return Navigate
  /// - checkbox/radio: toggle/activate
  /// - text control/textarea: focus
  /// - dropdown select: return OpenSelectDropdown (selection deferred to UI)
  /// `viewport_point` is in viewport coordinates; this method converts it to a page point by
  /// translating it by `scroll.viewport`.
  ///
  /// The provided `fragment_tree` must already have element scroll offsets applied (e.g. via
  /// [`crate::interaction::fragment_tree_with_scroll`]).
  pub fn pointer_up_with_scroll(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    scroll: &ScrollState,
    viewport_point: Point,
    button: PointerButton,
    modifiers: PointerModifiers,
    allow_default_drop: bool,
    document_url: &str,
    base_url: &str,
  ) -> (bool, InteractionAction) {
    let (changed, action, _hit) = self.pointer_up_with_scroll_and_hit(
      dom,
      box_tree,
      fragment_tree,
      scroll,
      viewport_point,
      button,
      modifiers,
      allow_default_drop,
      document_url,
      base_url,
    );
    (changed, action)
  }

  /// Legacy wrapper for [`InteractionEngine::pointer_up_with_scroll`] that assumes no scrolling.
  ///
  /// This is suitable for call sites that do not maintain scroll state (e.g. unit tests), but UI
  /// layers should generally call `pointer_up_with_scroll` so hit-testing stays aligned with the
  /// visible, scrolled content (including element scroll containers like `<select size>` listboxes).
  pub fn pointer_up(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    viewport_point: Point,
    button: PointerButton,
    modifiers: PointerModifiers,
    allow_default_drop: bool,
    document_url: &str,
    base_url: &str,
  ) -> (bool, InteractionAction) {
    self.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &ScrollState::default(),
      viewport_point,
      button,
      modifiers,
      allow_default_drop,
      document_url,
      base_url,
    )
  }

  /// Apply a pending [`InteractionAction::TextDrop`] default insertion into a text control.
  ///
  /// The interaction engine defers the actual value mutation so higher layers can dispatch
  /// cancelable JS `dragover`/`drop` events. When those events do not call `preventDefault()`, the
  /// UI layer should call this method to perform the insertion.
  ///
  /// Returns `true` when the DOM or interaction state changed.
  pub fn apply_text_drop(
    &mut self,
    dom: &mut DomNode,
    target_dom_id: usize,
    text: &str,
  ) -> bool {
    self.modality = InputModality::Pointer;

    // Consume any pending "move selection within same control" state for this drop. If the caller
    // applies a different drop target than the one that queued the move, drop the pending state.
    let pending_move = self
      .pending_text_drop_move
      .take()
      .filter(|state| state.node_id == target_dom_id);

    if text.is_empty() {
      return false;
    }

    let preserved_document_selection = self.state.document_selection.clone();
    let mut index = DomIndexMut::new(dom);

    let current_len = {
      let Some(node) = index.node(target_dom_id) else {
        return false;
      };
      let is_text_input = is_text_input(node);
      let is_textarea = is_textarea(node);
      if !(is_text_input || is_textarea) {
        return false;
      }
      if node_or_ancestor_is_inert(&index, target_dom_id)
        || node_is_disabled(&index, target_dom_id)
        || node_is_readonly(&index, target_dom_id)
      {
        return false;
      }
      if is_textarea {
        textarea_value_for_editing(node).chars().count()
      } else {
        node
          .get_attribute_ref("value")
          .unwrap_or("")
          .chars()
          .count()
      }
    };

    // Keep focus on the drop target (browser-like) and ensure pointer-modality semantics
    // (`focus_visible=false`).
    let mut changed = if is_focusable_interactive_element(&index, target_dom_id) {
      let focus_changed = self.set_focus(&mut index, Some(target_dom_id), false);
      // `set_focus` collapses any active document selection; restore it for drag-drop copy semantics.
      self
        .state
        .set_document_selection(preserved_document_selection.clone());
      focus_changed
    } else {
      false
    };

    let (caret, caret_affinity) = self
      .text_edit
      .as_ref()
      .filter(|edit| edit.node_id == target_dom_id)
      .map(|edit| (edit.caret, edit.caret_affinity))
      .unwrap_or((current_len, CaretAffinity::Downstream));

    // Apply insertion without treating it as keyboard input.
    if let Some(move_state) = pending_move {
      changed |= self.move_selected_text_within_text_control_for_drag_drop(
        &mut index,
        target_dom_id,
        move_state.selection,
        text,
        caret,
      );
    } else {
      changed |= self.insert_text_into_text_control_at_caret(
        &mut index,
        target_dom_id,
        text,
        caret,
        caret_affinity,
      );
    }

    // Restore document selection for copy semantics even if focus changes occurred above.
    self.state.set_document_selection(preserved_document_selection);

    changed
  }

  /// Drop one or more local files onto the page.
  ///
  /// When the drop target resolves to an `<input type="file">` control, this updates its selected
  /// file list:
  /// - For single-file inputs (no `multiple` attribute), only the first provided file is selected.
  /// - For `multiple` file inputs, all provided files are selected.
  ///
  /// This updates both:
  /// - internal per-control file selection state (used for form submission), and
  /// - `data-fastr-file-value` on the input element for browser-like value/validation semantics
  ///   (ignored from markup `value=`).
  ///
  /// `viewport_point` is in viewport coordinates; this method converts it to a page point by
  /// translating it by `scroll.viewport`.
  ///
  /// The provided `fragment_tree` must already have element scroll offsets applied (e.g. via
  /// [`crate::interaction::fragment_tree_with_scroll`]).
  pub fn drop_files_with_scroll(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    scroll: &ScrollState,
    viewport_point: Point,
    paths: &[PathBuf],
  ) -> bool {
    self.modality = InputModality::Pointer;
    let page_point = viewport_point.translate(scroll.viewport);
    let mut index = DomIndexMut::new(dom);
    let box_index = HitTestBoxIndex::new(box_tree);
    let hit = hit_test_dom_with_indices(dom, &index, &box_index, fragment_tree, page_point);

    let mut target_id = hit.as_ref().map(|hit| hit.dom_node_id);
    if let Some(hit) = hit.as_ref() {
      if hit.kind == HitTestKind::Label {
        if let Some(control_id) = find_label_associated_control(&index, hit.dom_node_id) {
          target_id = Some(control_id);
        }
      }
    }

    let Some(target_id) = target_id else {
      return false;
    };

    // Only file inputs accept file drops.
    if !index.node(target_id).is_some_and(is_file_input) {
      return false;
    }

    // Respect inert/disabled semantics.
    if is_disabled_or_inert(&index, target_id) {
      return false;
    }

    // Focus the input (browser-like).
    let mut changed = if is_focusable_interactive_element(&index, target_id) {
      self.set_focus(&mut index, Some(target_id), false)
    } else {
      false
    };

    let multiple = index
      .node(target_id)
      .is_some_and(|node| node.get_attribute_ref("multiple").is_some());

    let accept = index
      .node(target_id)
      .and_then(|node| node.get_attribute_ref("accept"));
    let filtered_paths = filter_paths_by_file_accept(paths, accept);
    let selected = build_file_selections_from_paths(filtered_paths.as_ref(), multiple);
    let selection_unchanged = match self.state.form_state().file_inputs.get(&target_id) {
      Some(prev) => prev.as_slice() == selected.as_slice(),
      None => selected.is_empty(),
    };
    if !selection_unchanged {
      changed = true;
      if selected.is_empty() {
        self.state.form_state_mut().file_inputs.remove(&target_id);
      } else {
        self.state.form_state_mut().file_inputs.insert(target_id, selected);
      }

      // Selecting files flips HTML user validity so `:user-invalid` can match after interaction.
      changed |= self.mark_user_validity(target_id);
      changed |= self.mark_form_user_validity(&index, target_id);
    }

    // Mirror browser value semantics: the value string reflects the first filename only.
    let value_string = self
      .state
      .form_state()
      .file_input_value_string(target_id)
      .unwrap_or_default();
    if let Some(node_mut) = index.node_mut(target_id) {
      let attr_changed = if value_string.is_empty() {
        remove_node_attr(node_mut, "data-fastr-file-value")
      } else {
        set_node_attr(node_mut, "data-fastr-file-value", &value_string)
      };
      changed |= attr_changed;
    }

    changed
  }

  /// Choose one or more local files for a specific `<input type="file">` control.
  ///
  /// This is used by browser UI file picker overlays, which already know the target input node.
  pub fn file_picker_choose(
    &mut self,
    dom: &mut DomNode,
    input_node_id: usize,
    paths: &[PathBuf],
  ) -> bool {
    self.modality = InputModality::Pointer;
    let mut index = DomIndexMut::new(dom);

    if !index.node(input_node_id).is_some_and(is_file_input) {
      return false;
    }
    if is_disabled_or_inert(&index, input_node_id) {
      return false;
    }

    // Keep focus on the input (browser-like).
    let mut changed = if is_focusable_interactive_element(&index, input_node_id) {
      self.set_focus(&mut index, Some(input_node_id), false)
    } else {
      false
    };

    let multiple = index
      .node(input_node_id)
      .is_some_and(|node| node.get_attribute_ref("multiple").is_some());
    let accept = index
      .node(input_node_id)
      .and_then(|node| node.get_attribute_ref("accept"));
    let filtered_paths = filter_paths_by_file_accept(paths, accept);
    let selected = build_file_selections_from_paths(filtered_paths.as_ref(), multiple);
    let selection_unchanged = match self.state.form_state().file_inputs.get(&input_node_id) {
      Some(prev) => prev.as_slice() == selected.as_slice(),
      None => selected.is_empty(),
    };
    if !selection_unchanged {
      changed = true;
      if selected.is_empty() {
        self.state.form_state_mut().file_inputs.remove(&input_node_id);
      } else {
        self.state.form_state_mut().file_inputs.insert(input_node_id, selected);
      }

      // Selecting files flips HTML user validity so `:user-invalid` can match after interaction.
      changed |= self.mark_user_validity(input_node_id);
      changed |= self.mark_form_user_validity(&index, input_node_id);
    }

    // Mirror browser value semantics: the value string reflects the first filename only.
    let value_string = self
      .state
      .form_state()
      .file_input_value_string(input_node_id)
      .unwrap_or_default();
    if let Some(node_mut) = index.node_mut(input_node_id) {
      let attr_changed = if value_string.is_empty() {
        remove_node_attr(node_mut, "data-fastr-file-value")
      } else {
        set_node_attr(node_mut, "data-fastr-file-value", &value_string)
      };
      changed |= attr_changed;
    }

    changed
  }

  fn move_selected_text_within_text_control_for_drag_drop(
    &mut self,
    index: &mut DomIndexMut,
    node_id: usize,
    selection: (usize, usize),
    text: &str,
    caret: usize,
  ) -> bool {
    let Some(node) = index.node(node_id) else {
      return false;
    };
    let is_text_input = is_text_input(node);
    let is_textarea = is_textarea(node);
    if !(is_text_input || is_textarea) {
      return false;
    }
    if text.is_empty() {
      return false;
    }
    if node_or_ancestor_is_inert(index, node_id) || node_is_disabled(index, node_id) {
      return false;
    }
    if node_is_readonly(index, node_id) {
      return false;
    }

    self.ensure_form_default_snapshot_for_control(index, node_id);

    let current = if is_textarea {
      textarea_value_for_editing(node)
    } else {
      node.get_attribute_ref("value").unwrap_or("").to_string()
    };
    let current_len = current.chars().count();

    let (sel_start, sel_end) = selection;
    let sel_start = sel_start.min(current_len);
    let sel_end = sel_end.min(current_len);
    if sel_start >= sel_end {
      return false;
    }

    let caret = caret.min(current_len);
    // Browser-like behavior: dropping within the original selection is a no-op.
    if caret >= sel_start && caret <= sel_end {
      return false;
    }

    // Any direct text mutation cancels an in-progress IME preedit string.
    let mut changed = self.ime_cancel_internal();

    let mut edit = self.text_edit.unwrap_or(TextEditState {
      node_id,
      caret: current_len,
      caret_affinity: CaretAffinity::Downstream,
      selection_anchor: None,
      preferred_x: None,
    });
    if edit.node_id != node_id {
      edit = TextEditState {
        node_id,
        caret: current_len,
        caret_affinity: CaretAffinity::Downstream,
        selection_anchor: None,
        preferred_x: None,
      };
    }
    edit.caret = edit.caret.min(current_len);
    edit.selection_anchor = edit.selection_anchor.map(|a| a.min(current_len));

    let start_byte = byte_offset_for_char_idx(&current, sel_start);
    let end_byte = byte_offset_for_char_idx(&current, sel_end);
    if start_byte >= end_byte || end_byte > current.len() {
      return changed;
    }

    let mut without_selection =
      String::with_capacity(current.len().saturating_sub(end_byte.saturating_sub(start_byte)));
    without_selection.push_str(&current[..start_byte]);
    without_selection.push_str(&current[end_byte..]);

    let removed_chars = sel_end.saturating_sub(sel_start);
    let without_len_chars = current_len.saturating_sub(removed_chars);
    let insert_at = if caret > sel_end {
      caret.saturating_sub(removed_chars)
    } else {
      caret
    }
    .min(without_len_chars);

    let insert_byte = byte_offset_for_char_idx(&without_selection, insert_at);
    let mut next = String::with_capacity(without_selection.len().saturating_add(text.len()));
    next.push_str(&without_selection[..insert_byte]);
    next.push_str(text);
    next.push_str(&without_selection[insert_byte..]);

    let next_len = next.chars().count();
    let inserted_chars = text.chars().count();
    let next_caret = insert_at.saturating_add(inserted_chars).min(next_len);

    let Some(node_mut) = index.node_mut(node_id) else {
      return changed;
    };
    if next != current {
      self.record_text_undo_snapshot(node_id, &current, &edit);
    }
    let changed_value = if is_text_input {
      set_node_attr(node_mut, "value", &next)
    } else {
      set_node_attr(node_mut, "data-fastr-value", &next)
    };
    changed |= changed_value;
    if changed_value {
      changed |= self.mark_user_validity(node_id);
    }

    if self.state.focused == Some(node_id) {
      self.text_edit = Some(TextEditState {
        node_id,
        caret: next_caret,
        caret_affinity: CaretAffinity::Upstream,
        selection_anchor: None,
        preferred_x: None,
      });
      changed |= self.sync_text_edit_paint_state();
    }

    changed
  }

  fn insert_text_into_text_control_at_caret(
    &mut self,
    index: &mut DomIndexMut,
    node_id: usize,
    text: &str,
    caret: usize,
    caret_affinity: CaretAffinity,
  ) -> bool {
    let Some(node) = index.node(node_id) else {
      return false;
    };
    let is_text_input = is_text_input(node);
    let is_textarea = is_textarea(node);
    if !(is_text_input || is_textarea) {
      return false;
    }
    if text.is_empty() {
      return false;
    }
    if node_or_ancestor_is_inert(index, node_id) || node_is_disabled(index, node_id) {
      return false;
    }
    if node_is_readonly(index, node_id) {
      return false;
    }

    self.ensure_form_default_snapshot_for_control(index, node_id);

    let current = if is_textarea {
      textarea_value_for_editing(node)
    } else {
      node.get_attribute_ref("value").unwrap_or("").to_string()
    };
    let current_len = current.chars().count();
    let maxlength = text_control_maxlength_for_user_editing(node);

    // Any direct text mutation cancels an in-progress IME preedit string.
    let mut changed = self.ime_cancel_internal();

    let caret = caret.min(current_len);
    let mut edit = self.text_edit.unwrap_or(TextEditState {
      node_id,
      caret,
      caret_affinity,
      selection_anchor: None,
      preferred_x: None,
    });
    if edit.node_id != node_id {
      edit = TextEditState {
        node_id,
        caret,
        caret_affinity,
        selection_anchor: None,
        preferred_x: None,
      };
    }
    // For drag-and-drop insertion, treat the drop caret as a collapsed selection.
    edit.caret = caret;
    edit.caret_affinity = caret_affinity;
    edit.selection_anchor = None;
    edit.preferred_x = None;

    let start_byte = byte_offset_for_char_idx(&current, caret);
    let end_byte = start_byte;

    let insert_text = if is_text_input {
      strip_ascii_line_breaks(text)
    } else {
      Cow::Borrowed(text)
    };
    let insert_text = if let Some(max) = maxlength {
      let current_units = utf16_len(&current);
      let allowed_units = max.saturating_sub(current_units);
      truncate_str_to_utf16_units(insert_text.as_ref(), allowed_units)
    } else {
      insert_text.as_ref()
    };

    let mut next = String::with_capacity(current.len().saturating_add(insert_text.len()));
    next.push_str(&current[..start_byte]);
    next.push_str(insert_text);
    next.push_str(&current[end_byte..]);

    let next_len = next.chars().count();
    let inserted_chars = insert_text.chars().count();
    let next_caret = caret.saturating_add(inserted_chars).min(next_len);

    let Some(node_mut) = index.node_mut(node_id) else {
      return changed;
    };
    if next != current {
      self.record_text_undo_snapshot(node_id, &current, &edit);
    }
    let changed_value = if is_text_input {
      set_node_attr(node_mut, "value", &next)
    } else {
      set_node_attr(node_mut, "data-fastr-value", &next)
    };
    changed |= changed_value;
    if changed_value {
      changed |= self.mark_user_validity(node_id);
    }

    if self.state.focused == Some(node_id) {
      self.text_edit = Some(TextEditState {
        node_id,
        caret: next_caret,
        caret_affinity: if insert_text.is_empty() {
          caret_affinity
        } else {
          CaretAffinity::Upstream
        },
        selection_anchor: None,
        preferred_x: None,
      });
      changed |= self.sync_text_edit_paint_state();
    }

    changed
  }

  /// Insert typed text into focused text control (input/textarea) and set focus-visible.
  pub fn text_input(&mut self, dom: &mut DomNode, text: &str) -> bool {
    self.text_input_with_box_tree(dom, None, text)
  }

  /// Like [`InteractionEngine::text_input`], but optionally accepts a cached [`BoxTree`].
  ///
  /// This is used for `<select>` typeahead: when layout artifacts are available, the box tree's
  /// `SelectControl` snapshot reflects the painted/visible option list (e.g. options with computed
  /// `display:none` are absent).
  pub fn text_input_with_box_tree(
    &mut self,
    dom: &mut DomNode,
    box_tree: Option<&BoxTree>,
    text: &str,
  ) -> bool {
    self.modality = InputModality::Keyboard;
    let mut index = DomIndexMut::new(dom);
    let mut changed = false;

    // Guard against stale focus when the DOM changes underneath us.
    if self
      .state
      .focused
      .is_some_and(|id| !index.node(id).is_some_and(DomNode::is_element))
    {
      changed |= self.set_focus(&mut index, None, false);
    }
    let Some(focused) = self.state.focused else {
      return changed;
    };

    // Ensure focus-visible when the keyboard is used.
    changed |= self.set_focus(&mut index, Some(focused), true);

    let focused_is_text_input = index.node(focused).is_some_and(is_text_input);
    let focused_is_textarea = index.node(focused).is_some_and(is_textarea);
    let focused_is_select = index.node(focused).is_some_and(is_select);
    if !(focused_is_text_input || focused_is_textarea) {
      if !focused_is_select {
        return changed;
      }

      // `<select>` typeahead: when a dropdown select is focused, typing should jump to the next
      // matching *visible* option.
      //
      // Keep this conservative: only apply to dropdown selects (non-multiple, size=1) to avoid
      // changing behaviour for listbox/multiple selects.
      let query = trim_ascii_whitespace(text);
      if query.is_empty() {
        return changed;
      }
      if node_or_ancestor_is_inert(&index, focused) || node_is_disabled(&index, focused) {
        return changed;
      }

      let mut computed_disabled = false;
      let control = match box_tree.and_then(|box_tree| select_control_snapshot_from_box_tree(box_tree, focused)) {
        Some((_, control, disabled, _)) => {
          computed_disabled = disabled;
          Some(control)
        }
        None => select_control_snapshot_from_dom(&index, focused),
      };
      let Some(control) = control else {
        return changed;
      };
      if computed_disabled {
        return changed;
      }
      if control.multiple || control.size != 1 {
        return changed;
      }

      // Collect enabled options in paint order.
      let mut options: Vec<(usize, usize, String)> = Vec::new();
      for (item_idx, item) in control.items.iter().enumerate() {
        let SelectItem::Option {
          node_id,
          label,
          value,
          disabled,
          ..
        } = item
        else {
          continue;
        };
        if *disabled {
          continue;
        }
        let display = if trim_ascii_whitespace(label).is_empty() {
          value.as_str()
        } else {
          label.as_str()
        };
        options.push((*node_id, item_idx, display.to_string()));
      }
      if options.is_empty() {
        return changed;
      }

      // Start searching just after the currently selected option, wrapping to the beginning.
      let anchor_item_idx = control.selected.last().copied();
      let anchor_pos = anchor_item_idx
        .and_then(|idx| options.iter().position(|(_, item_idx, _)| *item_idx == idx));
      let start = anchor_pos.map(|p| p + 1).unwrap_or(0) % options.len();

      fn starts_with_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
        if needle.is_empty() {
          return false;
        }
        haystack
          .as_bytes()
          .get(..needle.len())
          .is_some_and(|prefix| prefix.eq_ignore_ascii_case(needle.as_bytes()))
      }

      for offset in 0..options.len() {
        let pos = (start + offset) % options.len();
        let (option_node_id, _, label) = &options[pos];
        if starts_with_ignore_ascii_case(label, query) {
          changed |= self.activate_select_option(dom, focused, *option_node_id, false);
          break;
        }
      }

      return changed;
    }

    // `<input type=date|time|datetime-local|month|week>` uses Space to open its picker UI
    // (handled via `KeyAction::Space`). The browser UI sends both KeyAction and TextInput for Space,
    // so suppress raw space insertion here to avoid corrupting date/time values.
    if focused_is_text_input
      && text == " "
      && index
        .node(focused)
        .is_some_and(|node| date_time_input_kind(node).is_some())
    {
      return changed;
    }

    // Text controls in inert/disabled subtrees should not accept input. Read-only controls can
    // still receive focus/caret updates (e.g. click-to-place), but do not mutate their value.
    if node_or_ancestor_is_inert(&index, focused) || node_is_disabled(&index, focused) {
      return changed;
    }
    if node_is_readonly(&index, focused) {
      return changed;
    }
    if text.is_empty() {
      return changed;
    }

    self.ensure_form_default_snapshot_for_control(&index, focused);

    let current = if focused_is_textarea {
      index
        .node(focused)
        .map(textarea_value_for_editing)
        .unwrap_or_default()
    } else {
      index
        .node(focused)
        .and_then(|node| node.get_attribute_ref("value"))
        .unwrap_or("")
        .to_string()
    };
    let current_len = current.chars().count();
    let maxlength = index
      .node(focused)
      .and_then(|node| text_control_maxlength_for_user_editing(node));

    // Any direct text mutation cancels an in-progress IME preedit string.
    changed |= self.ime_cancel_internal();

    let mut edit = self.text_edit.unwrap_or(TextEditState {
      node_id: focused,
      caret: current_len,
      caret_affinity: CaretAffinity::Downstream,
      selection_anchor: None,
      preferred_x: None,
    });
    if edit.node_id != focused {
      edit = TextEditState {
        node_id: focused,
        caret: current_len,
        caret_affinity: CaretAffinity::Downstream,
        selection_anchor: None,
        preferred_x: None,
      };
    }
    edit.caret = edit.caret.min(current_len);
    edit.selection_anchor = edit.selection_anchor.map(|a| a.min(current_len));

    let selection = edit.selection();
    let (replace_start, replace_end) = selection.unwrap_or((edit.caret, edit.caret));
    let start_byte = byte_offset_for_char_idx(&current, replace_start);
    let end_byte = byte_offset_for_char_idx(&current, replace_end);

    let insert_text = if focused_is_text_input {
      strip_ascii_line_breaks(text)
    } else {
      Cow::Borrowed(text)
    };
    let insert_text = if let Some(max) = maxlength {
      let current_units = utf16_len(&current);
      let replaced_units = utf16_len(&current[start_byte..end_byte]);
      let base_units = current_units.saturating_sub(replaced_units);
      let allowed_units = max.saturating_sub(base_units);
      truncate_str_to_utf16_units(insert_text.as_ref(), allowed_units)
    } else {
      insert_text.as_ref()
    };

    let mut next = String::with_capacity(
      current
        .len()
        .saturating_sub(end_byte.saturating_sub(start_byte))
        .saturating_add(insert_text.len()),
    );
    next.push_str(&current[..start_byte]);
    next.push_str(insert_text);
    next.push_str(&current[end_byte..]);

    let next_len = next.chars().count();
    let inserted_chars = insert_text.chars().count();
    let next_caret = replace_start.saturating_add(inserted_chars).min(next_len);

    let Some(node_mut) = index.node_mut(focused) else {
      return changed;
    };
    if next != current {
      self.record_text_undo_snapshot(focused, &current, &edit);
    }
    let changed_value = if focused_is_text_input {
      set_node_attr(node_mut, "value", &next)
    } else {
      set_node_attr(node_mut, "data-fastr-value", &next)
    };
    changed |= changed_value;
    if changed_value {
      changed |= self.mark_user_validity(focused);
    }

    self.text_edit = Some(TextEditState {
      node_id: focused,
      caret: next_caret,
      // After inserting text, keep the caret attached to the inserted content. This matters at
      // split-caret bidi boundaries where the same logical caret position maps to multiple visual
      // x positions.
      caret_affinity: CaretAffinity::Upstream,
      selection_anchor: None,
      preferred_x: None,
    });
    changed |= self.sync_text_edit_paint_state();

    changed
  }

  fn ime_cancel_internal(&mut self) -> bool {
    let changed = self.state.ime_preedit.is_some();
    self.state.set_ime_preedit(None);
    changed
  }

  /// Update the active IME preedit (composition) string for the focused text control.
  pub fn ime_preedit(
    &mut self,
    dom: &mut DomNode,
    text: &str,
    cursor: Option<(usize, usize)>,
  ) -> bool {
    self.modality = InputModality::Keyboard;

    // Empty preedit text is treated as cancellation by most platform IMEs.
    if text.is_empty() {
      return self.ime_cancel(dom);
    }

    let Some(focused) = self.state.focused else {
      return self.ime_cancel(dom);
    };

    let mut index = DomIndexMut::new(dom);

    // Ensure focus-visible when the keyboard/IME is used.
    let mut changed = self.set_focus(&mut index, Some(focused), true);

    // Only text inputs and textareas participate in IME composition.
    let is_text_control = index.node(focused).is_some_and(is_text_input)
      || index.node(focused).is_some_and(is_textarea);
    if !is_text_control {
      changed |= self.ime_cancel_internal();
      return changed;
    }

    if node_or_ancestor_is_inert(&index, focused)
      || node_is_disabled(&index, focused)
      || node_is_readonly(&index, focused)
    {
      changed |= self.ime_cancel_internal();
      return changed;
    }

    // Update internal state (paint-only).
    changed |= self.state.update_ime_preedit(focused, text, cursor);

    changed
  }

  /// Commit IME text into the focused text control, clearing any active preedit.
  pub fn ime_commit(&mut self, dom: &mut DomNode, text: &str) -> bool {
    self.modality = InputModality::Keyboard;
    let Some(focused) = self.state.focused else {
      // IME commits can arrive after focus has been cleared; treat them as cancelling any remaining
      // preedit state.
      return self.ime_cancel(dom);
    };

    let mut index = DomIndexMut::new(dom);
    // Ensure focus-visible when the IME is used.
    let mut changed = self.set_focus(&mut index, Some(focused), true);
    // Clear any in-flight preedit before inserting committed text.
    changed |= self.ime_cancel_internal();

    if text.is_empty() {
      return changed;
    }

    // Drop the index before delegating to `text_input`; it will re-index the DOM.
    drop(index);
    changed | self.text_input(dom, text)
  }

  /// Cancel any active IME preedit string without mutating the DOM value.
  pub fn ime_cancel(&mut self, _dom: &mut DomNode) -> bool {
    self.ime_cancel_internal()
  }

  /// Select all text in the currently focused text control (`<input>`/`<textarea>`).
  ///
  /// This does not mutate the DOM; it only updates the internal selection range used by clipboard
  /// and text-editing actions.
  pub fn clipboard_select_all(&mut self, dom: &mut DomNode) -> bool {
    self.modality = InputModality::Keyboard;
    let focused = self.state.focused;
    let mut index = DomIndexMut::new(dom);

    // Ensure focus-visible when the keyboard is used.
    let mut changed = false;
    if let Some(focused) = focused {
      changed |= self.set_focus(&mut index, Some(focused), true);
    }

    let mut handled_text_control = false;
    if let Some(focused) = focused {
      if !(node_or_ancestor_is_inert(&index, focused) || node_is_disabled(&index, focused)) {
        if let Some(node) = index.node(focused) {
          let is_text_input = is_text_input(node);
          let is_textarea = is_textarea(node);
          if is_text_input || is_textarea {
            handled_text_control = true;

            // Selecting all moves the caret/selection via a non-IME interaction, so mirror native UX
            // by cancelling any active preedit for this control.
            if self
              .state
              .ime_preedit
              .as_ref()
              .is_some_and(|ime_state| ime_state.node_id == focused)
            {
              changed |= self.ime_cancel_internal();
            }

            let current = if is_textarea {
              textarea_value_for_editing(node)
            } else {
              node.get_attribute_ref("value").unwrap_or("").to_string()
            };
            let len = current.chars().count();

            let mut edit = self.text_edit.unwrap_or(TextEditState {
              node_id: focused,
              caret: len,
              caret_affinity: CaretAffinity::Downstream,
              selection_anchor: None,
              preferred_x: None,
            });
            if edit.node_id != focused {
              edit = TextEditState {
                node_id: focused,
                caret: len,
                caret_affinity: CaretAffinity::Downstream,
                selection_anchor: None,
                preferred_x: None,
              };
            }
            edit.preferred_x = None;

            if len == 0 {
              edit.caret = 0;
              edit.caret_affinity = CaretAffinity::Downstream;
              edit.selection_anchor = None;
            } else {
              edit.caret = len;
              edit.caret_affinity = CaretAffinity::Downstream;
              edit.selection_anchor = Some(0);
            }

            let prev = self.text_edit;
            self.text_edit = Some(edit);
            changed |= prev != self.text_edit;
            changed |= self.sync_text_edit_paint_state();

            // Text-control selection is distinct from document selection.
            if self.state.document_selection.is_some() {
              self.state.set_document_selection(None);
              changed = true;
            }
          }
        }
      }
    }

    if handled_text_control {
      return changed;
    }

    // No focused text control: fall back to document selection.
    let prev_doc = self.state.document_selection.clone();
    self
      .state
      .set_document_selection(Some(DocumentSelectionState::All));
    if prev_doc != self.state.document_selection {
      changed = true;
    }

    if self.text_edit.is_some() {
      self.text_edit = None;
      changed |= self.sync_text_edit_paint_state();
    }

    changed
  }

  /// Return the current selection text for a focused text control (`<input>`/`<textarea>`), if any.
  ///
  /// This does not mutate the DOM.
  pub fn clipboard_copy(&mut self, dom: &mut DomNode) -> Option<String> {
    self.modality = InputModality::Keyboard;
    let Some(focused) = self.state.focused else {
      return None;
    };

    let index = DomIndexMut::new(dom);
    if node_or_ancestor_is_inert(&index, focused) || node_is_disabled(&index, focused) {
      return None;
    }

    let Some(node) = index.node(focused) else {
      return None;
    };
    let is_text_input = is_text_input(node);
    let is_textarea = is_textarea(node);
    if !(is_text_input || is_textarea) {
      return None;
    }

    let value = if is_textarea {
      textarea_value_for_editing(node)
    } else {
      node.get_attribute_ref("value").unwrap_or("").to_string()
    };
    let len = value.chars().count();

    let mut edit = self.text_edit?;
    if edit.node_id != focused {
      return None;
    }
    edit.caret = edit.caret.min(len);
    edit.selection_anchor = edit.selection_anchor.map(|a| a.min(len));

    let (start, end) = edit.selection()?;
    let start_byte = byte_offset_for_char_idx(&value, start);
    let end_byte = byte_offset_for_char_idx(&value, end);
    if start_byte >= end_byte {
      return None;
    }
    Some(value[start_byte..end_byte].to_string())
  }

  fn document_selection_text_with_layout(
    &self,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
  ) -> Option<String> {
    let selection = self.state.document_selection.as_ref()?;
    let text = match selection {
      DocumentSelectionState::All => {
        serialize_document_selection(box_tree, fragment_tree, DocumentSelection::All)
      }
      DocumentSelectionState::Ranges(ranges) => {
        let mut parts: Vec<String> = Vec::new();
        for range in &ranges.ranges {
          if range.start == range.end {
            continue;
          }
          let part =
            serialize_document_selection(box_tree, fragment_tree, DocumentSelection::Range(*range));
          if !part.is_empty() {
            parts.push(part);
          }
        }
        parts.join("\n")
      }
    };
    (!text.is_empty()).then_some(text)
  }

  /// Return the current selection text for either:
  /// - a focused text control (`<input>` / `<textarea>`), or
  /// - an active document selection (e.g. from `SelectAll` when no text control is focused).
  ///
  /// This does not mutate the DOM.
  pub fn clipboard_copy_with_layout(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
  ) -> Option<String> {
    // Prefer the focused text control selection when present.
    if let Some(text) = self.clipboard_copy(dom) {
      return Some(text);
    }

    let selection = self.state.document_selection.as_ref()?;
    let text = match selection {
      DocumentSelectionState::All => {
        serialize_document_selection(box_tree, fragment_tree, DocumentSelection::All)
      }
      DocumentSelectionState::Ranges(ranges) => {
        let mut parts: Vec<String> = Vec::new();
        for range in &ranges.ranges {
          if range.start == range.end {
            continue;
          }
          let part =
            serialize_document_selection(box_tree, fragment_tree, DocumentSelection::Range(*range));
          if !part.is_empty() {
            parts.push(part);
          }
        }
        parts.join("\n")
      }
    };
    (!text.is_empty()).then_some(text)
  }

  /// Cut the current selection into the clipboard, deleting it when the control is editable.
  ///
  /// Returns `(dom_changed, clipboard_text)`.
  pub fn clipboard_cut(&mut self, dom: &mut DomNode) -> (bool, Option<String>) {
    self.modality = InputModality::Keyboard;
    let mut index = DomIndexMut::new(dom);
    let mut dom_changed = false;

    // Drop stale focus if the DOM was replaced/compacted since the last interaction.
    if self
      .state
      .focused
      .is_some_and(|id| !index.node(id).is_some_and(DomNode::is_element))
    {
      dom_changed |= self.set_focus(&mut index, None, false);
    }
    let Some(focused) = self.state.focused else {
      return (dom_changed, None);
    };
    // Ensure focus-visible when the keyboard is used.
    dom_changed |= self.set_focus(&mut index, Some(focused), true);

    if node_or_ancestor_is_inert(&index, focused) || node_is_disabled(&index, focused) {
      return (dom_changed, None);
    }

    let Some(node) = index.node(focused) else {
      return (dom_changed, None);
    };
    let is_text_input = is_text_input(node);
    let is_textarea = is_textarea(node);
    if !(is_text_input || is_textarea) {
      return (dom_changed, None);
    }

    let current = if is_textarea {
      textarea_value_for_editing(node)
    } else {
      node.get_attribute_ref("value").unwrap_or("").to_string()
    };
    let current_len = current.chars().count();
    let mut edit = self.text_edit.unwrap_or(TextEditState {
      node_id: focused,
      caret: current_len,
      caret_affinity: CaretAffinity::Downstream,
      selection_anchor: None,
      preferred_x: None,
    });
    if edit.node_id != focused {
      edit = TextEditState {
        node_id: focused,
        caret: current_len,
        caret_affinity: CaretAffinity::Downstream,
        selection_anchor: None,
        preferred_x: None,
      };
    }
    edit.caret = edit.caret.min(current_len);
    edit.selection_anchor = edit.selection_anchor.map(|a| a.min(current_len));

    let Some((start, end)) = edit.selection() else {
      return (dom_changed, None);
    };
    let prev_caret = edit.caret;
    let prev_affinity = edit.caret_affinity;

    let start_byte = byte_offset_for_char_idx(&current, start);
    let end_byte = byte_offset_for_char_idx(&current, end);
    if start_byte >= end_byte {
      return (dom_changed, None);
    }

    let selected = Some(current[start_byte..end_byte].to_string());
    if node_is_readonly(&index, focused) {
      return (dom_changed, selected);
    }

    self.ensure_form_default_snapshot_for_control(&index, focused);

    self.record_text_undo_snapshot(focused, &current, &edit);
    dom_changed |= self.ime_cancel_internal();

    let mut next = String::with_capacity(
      current
        .len()
        .saturating_sub(end_byte.saturating_sub(start_byte)),
    );
    next.push_str(&current[..start_byte]);
    next.push_str(&current[end_byte..]);
    let next_len = next.chars().count();

    let Some(node_mut) = index.node_mut(focused) else {
      return (dom_changed, selected);
    };
    let changed_value = if is_text_input {
      set_node_attr(node_mut, "value", &next)
    } else {
      set_node_attr(node_mut, "data-fastr-value", &next)
    };
    dom_changed |= changed_value;
    if changed_value {
      dom_changed |= self.mark_user_validity(focused);
    }

    edit.caret = start.min(next_len);
    edit.caret_affinity = if edit.caret == prev_caret {
      prev_affinity
    } else {
      CaretAffinity::Downstream
    };
    edit.selection_anchor = None;
    edit.preferred_x = None;
    self.text_edit = Some(edit);
    dom_changed |= self.sync_text_edit_paint_state();

    (dom_changed, selected)
  }

  /// Paste text into the focused text control (`<input>`/`<textarea>`), replacing any selection.
  pub fn clipboard_paste(&mut self, dom: &mut DomNode, text: &str) -> bool {
    self.text_input(dom, text)
  }

  /// Handle keyboard actions that mutate the DOM without performing navigation.
  ///
  /// Element activation (links, form submission, etc.) is handled by [`InteractionEngine::key_activate`].
  pub fn key_action(&mut self, dom: &mut DomNode, key: KeyAction) -> bool {
    self.key_action_internal(dom, None, None, key)
  }

  /// Like [`InteractionEngine::key_action`], but optionally supplies the cached [`BoxTree`].
  ///
  /// When a `box_tree` snapshot is available, `<select>` arrow-key navigation uses the painted
  /// [`SelectControl`] rows (skipping `hidden`/`display:none` options) so keyboard selection stays
  /// aligned with what the user can see.
  pub fn key_action_with_box_tree(
    &mut self,
    dom: &mut DomNode,
    box_tree: Option<&BoxTree>,
    key: KeyAction,
  ) -> bool {
    self.key_action_internal(dom, box_tree, None, key)
  }

  /// Like [`InteractionEngine::key_action_with_box_tree`], but also supplies the cached
  /// [`FragmentTree`].
  ///
  /// This enables interactions that require layout geometry, such as soft-wrap-aware `<textarea>`
  /// caret movement.
  pub fn key_action_with_layout_artifacts(
    &mut self,
    dom: &mut DomNode,
    box_tree: Option<&BoxTree>,
    fragment_tree: &FragmentTree,
    key: KeyAction,
  ) -> bool {
    self.key_action_internal(dom, box_tree, Some(fragment_tree), key)
  }

  fn key_action_internal(
    &mut self,
    dom: &mut DomNode,
    box_tree: Option<&BoxTree>,
    fragment_tree: Option<&FragmentTree>,
    key: KeyAction,
  ) -> bool {
    self.modality = InputModality::Keyboard;

    let mut index = DomIndexMut::new(dom);
    let mut changed = false;
    if self
      .state
      .focused
      .is_some_and(|id| !index.node(id).is_some_and(DomNode::is_element))
    {
      changed |= self.set_focus(&mut index, None, false);
    }

    if matches!(key, KeyAction::Tab | KeyAction::ShiftTab) {
      // Focus traversal (wraps at ends).
      let focusables = collect_tab_stops(&index);
      let next_focus = match key {
        KeyAction::Tab => next_tab_focus(self.state.focused, &focusables),
        KeyAction::ShiftTab => prev_tab_focus(self.state.focused, &focusables),
        _ => None,
      };
      let Some(next_focus) = next_focus else {
        return changed;
      };
      changed |= self.set_focus(&mut index, Some(next_focus), true);
      return changed;
    }

    let Some(focused) = self.state.focused else {
      return changed;
    };

    // Ensure focus-visible when the keyboard is used.
    changed |= self.set_focus(&mut index, Some(focused), true);

    let focused_is_text_input = index.node(focused).is_some_and(is_text_input);
    let focused_is_textarea = index.node(focused).is_some_and(is_textarea);

    if focused_is_text_input || focused_is_textarea {
      if node_or_ancestor_is_inert(&index, focused) || node_is_disabled(&index, focused) {
        return changed;
      }

      let can_edit_value = !node_is_readonly(&index, focused);
      self.ensure_form_default_snapshot_for_control(&index, focused);

      // `<input type=number>` uses ArrowUp/ArrowDown to increment/decrement (like browsers).
      if focused_is_text_input
        && matches!(
          key,
          KeyAction::ArrowUp
            | KeyAction::ArrowDown
            | KeyAction::ShiftArrowUp
            | KeyAction::ShiftArrowDown
        )
        && index
          .node(focused)
          .is_some_and(|node| input_type(node).eq_ignore_ascii_case("number"))
      {
        if !can_edit_value {
          return changed;
        }
        let delta_steps = if matches!(key, KeyAction::ArrowUp | KeyAction::ShiftArrowUp) {
          1
        } else {
          -1
        };
        changed |= self.step_number_input(&mut index, focused, delta_steps);
        return changed;
      }
      let current = if focused_is_textarea {
        index
          .node(focused)
          .map(textarea_value_for_editing)
          .unwrap_or_default()
      } else {
        index
          .node(focused)
          .and_then(|node| node.get_attribute_ref("value"))
          .unwrap_or("")
          .to_string()
      };
      let current_len = current.chars().count();

      let mut edit = self.text_edit.unwrap_or(TextEditState {
        node_id: focused,
        caret: current_len,
        caret_affinity: CaretAffinity::Downstream,
        selection_anchor: None,
        preferred_x: None,
      });
      if edit.node_id != focused {
        edit = TextEditState {
          node_id: focused,
          caret: current_len,
          caret_affinity: CaretAffinity::Downstream,
          selection_anchor: None,
          preferred_x: None,
        };
      }
      edit.caret = edit.caret.min(current_len);
      edit.selection_anchor = edit.selection_anchor.map(|a| a.min(current_len));

      let original = edit;

      if matches!(
        key,
        KeyAction::ArrowLeft
          | KeyAction::ArrowRight
          | KeyAction::ShiftArrowLeft
          | KeyAction::ShiftArrowRight
          | KeyAction::ShiftArrowUp
          | KeyAction::ShiftArrowDown
          | KeyAction::ArrowUp
          | KeyAction::ArrowDown
          | KeyAction::WordLeft
          | KeyAction::WordRight
          | KeyAction::ShiftWordLeft
          | KeyAction::ShiftWordRight
          | KeyAction::Home
          | KeyAction::End
          | KeyAction::ShiftHome
          | KeyAction::ShiftEnd
          | KeyAction::SelectAll
      ) && self
        .state
        .ime_preedit
        .as_ref()
        .is_some_and(|ime_state| ime_state.node_id == focused)
      {
        changed |= self.ime_cancel_internal();
      }

      match key {
        KeyAction::Backspace
        | KeyAction::Delete
        | KeyAction::WordBackspace
        | KeyAction::WordDelete => {
          if !can_edit_value {
            return changed;
          }
          let prev_caret = edit.caret;
          let prev_affinity = edit.caret_affinity;
          let selection = edit.selection();

          let Some((delete_start, delete_end, next_caret)) =
            text_delete_range_for_key(key, &current, edit.caret, selection)
          else {
            return changed;
          };

          if delete_start >= delete_end {
            return changed;
          }

          self.record_text_undo_snapshot(focused, &current, &edit);

          // Any direct text mutation cancels an in-progress IME preedit string.
          changed |= self.ime_cancel_internal();

          let start_byte = byte_offset_for_char_idx(&current, delete_start);
          let end_byte = byte_offset_for_char_idx(&current, delete_end);
          let mut next = String::with_capacity(
            current
              .len()
              .saturating_sub(end_byte.saturating_sub(start_byte)),
          );
          next.push_str(&current[..start_byte]);
          next.push_str(&current[end_byte..]);

          let next_len = next.chars().count();
          edit.caret = next_caret.min(next_len);
          edit.caret_affinity = if edit.caret == prev_caret {
            prev_affinity
          } else {
            CaretAffinity::Downstream
          };
          edit.selection_anchor = None;
          edit.preferred_x = None;

          if let Some(node_mut) = index.node_mut(focused) {
            let changed_value = if focused_is_text_input {
              set_node_attr(node_mut, "value", &next)
            } else {
              set_node_attr(node_mut, "data-fastr-value", &next)
            };
            changed |= changed_value;
            if changed_value {
              changed |= self.mark_user_validity(focused);
            }
          }
        }
        KeyAction::Undo | KeyAction::Redo => {
          if !can_edit_value {
            return changed;
          }
          let history = self.text_undo.entry(focused).or_default();
          let current_snapshot = TextUndoEntry {
            value: current.clone(),
            caret: edit.caret,
            caret_affinity: edit.caret_affinity,
            selection_anchor: edit.selection_anchor,
          };
          let target = if matches!(key, KeyAction::Undo) {
            let Some(entry) = history.pop_undo() else {
              return changed;
            };
            history.push_redo(current_snapshot);
            entry
          } else {
            let Some(entry) = history.pop_redo() else {
              return changed;
            };
            history.push_undo(current_snapshot);
            entry
          };

          changed |= self.ime_cancel_internal();

          let TextUndoEntry {
            value: next_value,
            caret: target_caret,
            caret_affinity: target_affinity,
            selection_anchor: target_selection_anchor,
          } = target;
          let next_len = next_value.chars().count();
          edit.caret = target_caret.min(next_len);
          edit.caret_affinity = target_affinity;
          edit.selection_anchor = target_selection_anchor.map(|a| a.min(next_len));
          edit.preferred_x = None;

          if let Some(node_mut) = index.node_mut(focused) {
            let changed_value = if focused_is_text_input {
              set_node_attr(node_mut, "value", &next_value)
            } else {
              set_node_attr(node_mut, "data-fastr-value", &next_value)
            };
            changed |= changed_value;
            if changed_value {
              changed |= self.mark_user_validity(focused);
            }
          }
        }
        KeyAction::WordLeft
        | KeyAction::WordRight
        | KeyAction::ShiftWordLeft
        | KeyAction::ShiftWordRight => {
          let move_left = matches!(key, KeyAction::WordLeft | KeyAction::ShiftWordLeft);
          let extend_selection =
            matches!(key, KeyAction::ShiftWordLeft | KeyAction::ShiftWordRight);

          if let Some((start, end)) = edit.selection().filter(|_| !extend_selection) {
            // When a selection exists and Shift is *not* held, collapse to the edge in the direction
            // of travel before moving further (matching native behaviour).
            let (next, next_affinity) = if move_left {
              (start, CaretAffinity::Downstream)
            } else {
              (end, CaretAffinity::Upstream)
            };
            if next == edit.caret {
              edit.set_caret_with_affinity_and_maybe_extend_selection(
                next,
                edit.caret_affinity,
                false,
              );
            } else {
              edit.set_caret_with_affinity_and_maybe_extend_selection(next, next_affinity, false);
            }
          } else {
            let word_chars = word_char_classes(&current);
            let next = if move_left {
              word_left_char_idx(&word_chars, edit.caret)
            } else {
              word_right_char_idx(&word_chars, edit.caret)
            };
            if next == edit.caret {
              edit.set_caret_with_affinity_and_maybe_extend_selection(
                next,
                edit.caret_affinity,
                extend_selection,
              );
            } else {
              edit.set_caret_and_maybe_extend_selection(next, extend_selection);
            }
          }
        }
        KeyAction::Enter => {
          if focused_is_textarea {
            return changed | self.text_input(dom, "\n");
          }
        }
        KeyAction::ArrowLeft
        | KeyAction::ArrowRight
        | KeyAction::ShiftArrowLeft
        | KeyAction::ShiftArrowRight => {
          let move_left = matches!(key, KeyAction::ArrowLeft | KeyAction::ShiftArrowLeft);
          let extend_selection =
            matches!(key, KeyAction::ShiftArrowLeft | KeyAction::ShiftArrowRight);

          let style = box_tree.and_then(|tree| style_for_styled_node_id(tree, focused));
          let base_dir = style
            .as_deref()
            .map(|style| style.direction)
            .unwrap_or_else(|| inferred_text_direction_from_dom(&index, focused));

          let selection = edit.selection();

          // Ensure the shaping style has a direction even when we don't have computed layout style.
          let mut default_style = ComputedStyle::default();
          default_style.direction = base_dir;
          let shape_style = style.as_deref().unwrap_or(&default_style);

          // Mirror painter behavior: password inputs render bullets instead of the underlying value.
          let display_text = if focused_is_text_input {
            index
              .node(focused)
              .map(|node| {
                if input_type(node).eq_ignore_ascii_case("password") {
                  "•".repeat(current_len)
                } else {
                  current.clone()
                }
              })
              .unwrap_or_else(|| current.clone())
          } else {
            current.clone()
          };

          if focused_is_textarea && current.contains('\n') {
            // For `<textarea>`, use visual left/right movement within the current newline-delimited
            // line. (Vertical movement remains line-based elsewhere.)
            let total_chars = current_len;
            let caret = edit.caret.min(total_chars);

            let mut line_starts: Vec<usize> = vec![0];
            let mut idx = 0usize;
            for ch in current.chars() {
              if ch == '\n' {
                line_starts.push(idx + 1);
              }
              idx += 1;
            }

            let line_idx = line_starts
              .partition_point(|&start| start <= caret)
              .saturating_sub(1);
            let line_start = *line_starts.get(line_idx).unwrap_or(&0);
            let line_end = if let Some(next_start) = line_starts.get(line_idx + 1) {
              next_start.saturating_sub(1)
            } else {
              total_chars
            };
            let line_len = line_end.saturating_sub(line_start);
            let caret_in_line = caret.saturating_sub(line_start).min(line_len);

            let start_byte = byte_offset_for_char_idx(&current, line_start);
            let end_byte = byte_offset_for_char_idx(&current, line_end);
            let line_text = current.get(start_byte..end_byte).unwrap_or("");

            let fallback_advance = fallback_text_advance(line_text, shape_style);
            let runs = shape_text_runs_for_interaction(line_text, shape_style)
              .unwrap_or_else(|| Arc::new(Vec::new()));
            let total_advance = shaped_total_advance(runs.as_ref(), fallback_advance);
            let stops =
              crate::text::caret::caret_stops_for_runs(line_text, runs.as_ref(), total_advance);
            let grapheme_boundaries = grapheme_cluster_boundaries_char_idx(line_text);

            if let Some((start, end)) = selection.filter(|_| !extend_selection) {
              // Collapse selection without shift.
              if start >= line_start && end <= line_end {
                let start_in_line_raw = start.saturating_sub(line_start).min(line_len);
                let end_in_line_raw = end.saturating_sub(line_start).min(line_len);
                let start_in_line =
                  snap_char_idx_down_to_grapheme_boundary(&grapheme_boundaries, start_in_line_raw);
                let end_in_line =
                  snap_char_idx_up_to_grapheme_boundary(&grapheme_boundaries, end_in_line_raw);
                let start = line_start.saturating_add(start_in_line).min(total_chars);
                let end = line_start.saturating_add(end_in_line).min(total_chars);

                let start_pos = crate::text::caret::caret_stop_index(
                  &stops,
                  start_in_line,
                  CaretAffinity::Downstream,
                );
                // The selection end boundary is after the selected text, so prefer an upstream stop
                // when the boundary is a split caret. This prevents collapsing a selection to the
                // wrong visual edge at bidi boundaries (e.g. selecting an LTR run that ends where
                // an RTL run begins).
                let end_pos = crate::text::caret::caret_stop_index(
                  &stops,
                  end_in_line,
                  CaretAffinity::Upstream,
                );

                if let (Some(start_pos), Some(end_pos)) = (start_pos, end_pos) {
                  let (left_edge, right_edge) = if start_pos <= end_pos {
                    (
                      (start, stops[start_pos].affinity),
                      (end, stops[end_pos].affinity),
                    )
                  } else {
                    (
                      (end, stops[end_pos].affinity),
                      (start, stops[start_pos].affinity),
                    )
                  };
                  let (next_caret, next_affinity) = if move_left { left_edge } else { right_edge };
                  edit.set_caret_with_affinity_and_maybe_extend_selection(
                    next_caret,
                    next_affinity,
                    false,
                  );
                } else {
                  edit.set_caret_and_maybe_extend_selection(
                    if move_left { start } else { end },
                    false,
                  );
                }
              } else {
                // Selection spans multiple lines; fall back to logical collapse.
                let grapheme_boundaries = grapheme_cluster_boundaries_char_idx(&current);
                let next = if move_left {
                  snap_char_idx_down_to_grapheme_boundary(&grapheme_boundaries, start)
                } else {
                  snap_char_idx_up_to_grapheme_boundary(&grapheme_boundaries, end)
                };
                edit.set_caret_and_maybe_extend_selection(next, false);
              }
            } else if let Some(cur_idx) =
              crate::text::caret::caret_stop_index(&stops, caret_in_line, edit.caret_affinity)
            {
              // Move caret within the current line, falling back to crossing a newline when there is
              // no further visual stop in the requested direction.
              let next_idx = if move_left {
                (0..cur_idx).rev().find(|&idx| {
                  is_grapheme_cluster_boundary(&grapheme_boundaries, stops[idx].char_idx)
                })
              } else {
                ((cur_idx + 1)..stops.len()).find(|&idx| {
                  is_grapheme_cluster_boundary(&grapheme_boundaries, stops[idx].char_idx)
                })
              };
              if let Some(next_idx) = next_idx {
                let stop = stops.get(next_idx).copied().unwrap_or(stops[cur_idx]);
                edit.set_caret_with_affinity_and_maybe_extend_selection(
                  line_start.saturating_add(stop.char_idx).min(total_chars),
                  stop.affinity,
                  extend_selection,
                );
              } else {
                let next = if move_left {
                  prev_grapheme_cluster(&current, caret)
                    .map(|(start, _)| start)
                    .unwrap_or(caret)
                } else {
                  next_grapheme_cluster(&current, caret)
                    .map(|(_, end)| end)
                    .unwrap_or(caret)
                };
                edit.set_caret_and_maybe_extend_selection(next, extend_selection);
              }
            } else {
              let next = if move_left {
                prev_grapheme_cluster(&current, caret)
                  .map(|(start, _)| start)
                  .unwrap_or(caret)
              } else {
                next_grapheme_cluster(&current, caret)
                  .map(|(_, end)| end)
                  .unwrap_or(caret)
              };
              edit.set_caret_and_maybe_extend_selection(next, extend_selection);
            }
          } else {
            // Single-line visual caret movement for `<input>` (and single-line textareas).
            let text = if focused_is_text_input {
              &display_text
            } else {
              &current
            };

            let fallback_advance = fallback_text_advance(text, shape_style);
            let runs = shape_text_runs_for_interaction(text, shape_style)
              .unwrap_or_else(|| Arc::new(Vec::new()));
            let total_advance = shaped_total_advance(runs.as_ref(), fallback_advance);
            let stops = crate::text::caret::caret_stops_for_runs(text, runs.as_ref(), total_advance);
            // Grapheme cluster boundary indices are based on the underlying value, not on the
            // display text (e.g. password inputs render bullets), so we don't allow the caret to be
            // placed within a multi-scalar grapheme cluster.
            let grapheme_boundaries = grapheme_cluster_boundaries_char_idx(&current);

            if let Some((start, end)) = selection.filter(|_| !extend_selection) {
              let start = snap_char_idx_down_to_grapheme_boundary(
                &grapheme_boundaries,
                start.min(current_len),
              );
              let end =
                snap_char_idx_up_to_grapheme_boundary(&grapheme_boundaries, end.min(current_len));
              let start_pos =
                crate::text::caret::caret_stop_index(&stops, start, CaretAffinity::Downstream);
              let end_pos = crate::text::caret::caret_stop_index(
                &stops,
                end,
                // Prefer upstream at the end boundary to avoid "teleporting" across split-caret
                // bidi boundaries when collapsing selections with ArrowLeft/Right.
                CaretAffinity::Upstream,
              );

              if let (Some(start_pos), Some(end_pos)) = (start_pos, end_pos) {
                let (left_edge, right_edge) = if start_pos <= end_pos {
                  (
                    (start, stops[start_pos].affinity),
                    (end, stops[end_pos].affinity),
                  )
                } else {
                  (
                    (end, stops[end_pos].affinity),
                    (start, stops[start_pos].affinity),
                  )
                };
                let (next_caret, next_affinity) = if move_left { left_edge } else { right_edge };
                edit.set_caret_with_affinity_and_maybe_extend_selection(
                  next_caret,
                  next_affinity,
                  false,
                );
              } else {
                edit
                  .set_caret_and_maybe_extend_selection(if move_left { start } else { end }, false);
              }
            } else if let Some(cur_idx) = crate::text::caret::caret_stop_index(
              &stops,
              edit.caret.min(current_len),
              edit.caret_affinity,
            ) {
              let next_idx = if move_left {
                (0..cur_idx).rev().find(|&idx| {
                  is_grapheme_cluster_boundary(&grapheme_boundaries, stops[idx].char_idx)
                })
              } else {
                ((cur_idx + 1)..stops.len()).find(|&idx| {
                  is_grapheme_cluster_boundary(&grapheme_boundaries, stops[idx].char_idx)
                })
              };
              if let Some(next_idx) = next_idx {
                let stop = stops.get(next_idx).copied().unwrap_or(stops[cur_idx]);
                edit.set_caret_with_affinity_and_maybe_extend_selection(
                  stop.char_idx.min(current_len),
                  stop.affinity,
                  extend_selection,
                );
              }
            } else {
              let caret = edit.caret.min(current_len);
              let next_caret = if move_left {
                prev_grapheme_cluster(&current, caret)
                  .map(|(start, _)| start)
                  .unwrap_or(caret)
              } else {
                next_grapheme_cluster(&current, caret)
                  .map(|(_, end)| end)
                  .unwrap_or(caret)
              };
              edit.set_caret_and_maybe_extend_selection(next_caret, extend_selection);
            }
          }
        }
        KeyAction::Home | KeyAction::End | KeyAction::ShiftHome | KeyAction::ShiftEnd => {
          let is_home = matches!(key, KeyAction::Home | KeyAction::ShiftHome);
          let extend_selection = matches!(key, KeyAction::ShiftHome | KeyAction::ShiftEnd);

          let mut next = if is_home { 0usize } else { current_len };

          if focused_is_textarea {
            let total_chars = current_len;
            let caret = edit.caret.min(total_chars);

            // Prefer a soft-wrap-aware visual line mapping when layout artifacts are available.
            let mut visual_line_bounds: Option<(usize, usize)> = None;
            if let (Some(box_tree), Some(fragment_tree)) = (box_tree, fragment_tree) {
              if let Some((textarea_box_id, style)) =
                textarea_control_snapshot_from_box_tree(box_tree, focused)
              {
                if let Some(border_rect) = fragment_rect_for_box_id(fragment_tree, textarea_box_id) {
                  let style = style.as_ref();
                  let viewport_size = fragment_tree.viewport_size();
                  let content_rect =
                    content_rect_for_border_rect(border_rect, style, viewport_size);
                  let rect = inset_rect_uniform(content_rect, 2.0);
                  if rect.width() > 0.0 && rect.width().is_finite() {
                    let chars_per_line =
                      crate::textarea::textarea_chars_per_line(style, rect.width());
                    let layout =
                      crate::textarea::build_textarea_visual_lines(&current, chars_per_line);
                    let line_idx = crate::textarea::textarea_visual_line_index_for_caret(
                      &current,
                      &layout,
                      caret,
                    );
                    if let Some(line) = layout.lines.get(line_idx).copied() {
                      visual_line_bounds = Some((line.start_char, line.end_char));
                    }
                  }
                }
              }
            }

            // Fallback: newline-delimited line boundaries (no soft-wrap support).
            if visual_line_bounds.is_none() {
              let mut line_starts: Vec<usize> = vec![0];
              let mut idx = 0usize;
              for ch in current.chars() {
                if ch == '\n' {
                  line_starts.push(idx + 1);
                }
                idx += 1;
              }
              let total = idx;
              let caret = caret.min(total);
              let line_idx = line_starts
                .partition_point(|&start| start <= caret)
                .saturating_sub(1);
              let line_start = *line_starts.get(line_idx).unwrap_or(&0);
              let line_end = if let Some(next_start) = line_starts.get(line_idx + 1) {
                next_start.saturating_sub(1)
              } else {
                total
              };
              visual_line_bounds = Some((line_start, line_end));
            }

            if let Some((start, end)) = visual_line_bounds {
              next = if is_home { start } else { end };
            }
          }

          edit.set_caret_and_maybe_extend_selection(next, extend_selection);
        }
        KeyAction::SelectAll => {
          edit.preferred_x = None;
          if current_len == 0 {
            edit.selection_anchor = None;
            edit.caret = 0;
            edit.caret_affinity = CaretAffinity::Downstream;
          } else {
            edit.selection_anchor = Some(0);
            edit.caret = current_len;
            edit.caret_affinity = CaretAffinity::Downstream;
          }
        }
        KeyAction::ArrowUp
        | KeyAction::ArrowDown
        | KeyAction::ShiftArrowUp
        | KeyAction::ShiftArrowDown => {
          let move_up = matches!(key, KeyAction::ArrowUp | KeyAction::ShiftArrowUp);
          let extend_selection = matches!(key, KeyAction::ShiftArrowUp | KeyAction::ShiftArrowDown);

          if let Some((start, end)) = edit.selection().filter(|_| !extend_selection) {
            // Like ArrowLeft/Right, ArrowUp/Down should collapse an active selection to the
            // boundary in the direction of travel before attempting any further movement.
            let (next, next_affinity) = if move_up {
              (start, CaretAffinity::Downstream)
            } else {
              // Selection end should attach to the preceding text, which maps to the upstream side
              // at split-caret boundaries.
              (end, CaretAffinity::Upstream)
            };
            if next == edit.caret {
              // Preserve the current caret affinity when collapsing to the caret edge (important at
              // split-caret bidi boundaries).
              edit.set_caret_with_affinity_and_maybe_extend_selection(
                next,
                edit.caret_affinity,
                false,
              );
            } else {
              edit.set_caret_with_affinity_and_maybe_extend_selection(next, next_affinity, false);
            }
          } else if focused_is_textarea {
            if extend_selection && edit.selection_anchor.is_none() {
              // When beginning a Shift selection, keep the anchor at the caret's original position.
              edit.selection_anchor = Some(edit.caret);
            }

            let mut moved = false;

            if let (Some(box_tree), Some(fragment_tree)) = (box_tree, fragment_tree) {
              if let Some((textarea_box_id, style)) =
                textarea_control_snapshot_from_box_tree(box_tree, focused)
              {
                if let Some(border_rect) = fragment_rect_for_box_id(fragment_tree, textarea_box_id)
                {
                  let style = style.as_ref();
                  let viewport_size = fragment_tree.viewport_size();
                  let content_rect =
                    content_rect_for_border_rect(border_rect, style, viewport_size);
                  let rect = inset_rect_uniform(content_rect, 2.0);

                  if rect.width() > 0.0 && rect.height() > 0.0 {
                    let metrics =
                      if matches!(style.line_height, crate::style::types::LineHeight::Normal) {
                        super::resolve_scaled_metrics_for_interaction(style)
                      } else {
                        None
                      };
                    let line_height = compute_line_height_with_metrics_viewport(
                      style,
                      metrics.as_ref(),
                      Some(viewport_size),
                      None,
                    );

                    if line_height.is_finite() && line_height > 0.0 {
                      let total_chars = current_len;
                      let caret = edit.caret.min(total_chars);
                      let chars_per_line =
                        crate::textarea::textarea_chars_per_line(style, rect.width());
                      let layout =
                        crate::textarea::build_textarea_visual_lines(&current, chars_per_line);

                      let line_idx = crate::textarea::textarea_visual_line_index_for_caret(
                        &current, &layout, caret,
                      );

                      let target_idx = (if move_up {
                        line_idx.checked_sub(1)
                      } else {
                        Some(line_idx.saturating_add(1))
                      })
                      .filter(|idx| *idx < layout.lines.len());

                      if let Some(target_idx) = target_idx {
                        let line_rect =
                          Rect::from_xywh(rect.x(), rect.y(), rect.width(), line_height);

                        // Maintain a preferred x position across vertical moves (like browsers).
                        let preferred_x = if let Some(px) = edit.preferred_x {
                          px
                        } else {
                          let cur_line = layout
                            .lines
                            .get(line_idx)
                            .copied()
                            .unwrap_or(layout.lines[0]);
                          let cur_text = cur_line.text(&current);
                          let caret_in_line = caret
                            .saturating_sub(cur_line.start_char)
                            .min(cur_line.len_chars());

                          let fallback_advance = fallback_text_advance(cur_text, style);
                          let runs = shape_text_runs_for_interaction(cur_text, style)
                            .unwrap_or_else(|| Arc::new(Vec::new()));
                          let total_advance = shaped_total_advance(runs.as_ref(), fallback_advance);
                          let start_x = aligned_text_start_x(style, line_rect, total_advance);
                          let stops = crate::text::caret::caret_stops_for_runs(
                            cur_text,
                            runs.as_ref(),
                            total_advance,
                          );
                          let caret_x_local = crate::text::caret::caret_x_for_position(
                            &stops,
                            caret_in_line,
                            edit.caret_affinity,
                          )
                          .unwrap_or(0.0);
                          let mut px = start_x + caret_x_local - rect.x();
                          if !px.is_finite() {
                            px = 0.0;
                          }
                          px = px.clamp(0.0, rect.width().max(0.0));
                          edit.preferred_x = Some(px);
                          px
                        };

                        let target_line = layout
                          .lines
                          .get(target_idx)
                          .copied()
                          .unwrap_or(layout.lines[0]);
                        let target_text = target_line.text(&current);

                        let x = rect.x() + preferred_x;
                        let x = if x.is_finite() { x } else { rect.x() };
                        let (caret_in_line, affinity) = caret_position_for_x_in_text(
                          target_text,
                          target_text,
                          style,
                          line_rect,
                          x,
                        );

                        edit.caret = target_line
                          .start_char
                          .saturating_add(caret_in_line)
                          .min(total_chars);
                        edit.caret_affinity = affinity;
                        if !extend_selection {
                          edit.selection_anchor = None;
                        }
                        moved = true;
                      }
                    }
                  }
                }
              }
            }

            if !moved {
              // Fallback: vertical caret movement between newline-separated lines.
              let mut line_starts: Vec<usize> = vec![0];
              let mut idx = 0usize;
              for ch in current.chars() {
                if ch == '\n' {
                  line_starts.push(idx + 1);
                }
                idx += 1;
              }
              let total_chars = idx;

              let caret = edit.caret.min(total_chars);
              let line_idx = line_starts
                .partition_point(|&start| start <= caret)
                .saturating_sub(1);
              let line_start = *line_starts.get(line_idx).unwrap_or(&0);
              let col = caret.saturating_sub(line_start);

              let char_advance = box_tree
                .and_then(|tree| style_for_styled_node_id(tree, focused))
                .map(|style| (style.font_size * 0.6).max(f32::EPSILON))
                .unwrap_or(1.0);
              let preferred_col = edit
                .preferred_x
                .and_then(|x| {
                  (x / char_advance)
                    .is_finite()
                    .then_some((x / char_advance).round() as usize)
                })
                .unwrap_or(col);

              let target_line = (if move_up {
                line_idx.checked_sub(1)
              } else {
                Some(line_idx + 1)
              })
              .filter(|&idx| idx < line_starts.len());

              if let Some(target_idx) = target_line {
                let target_start = line_starts[target_idx];
                let target_end = if let Some(next_start) = line_starts.get(target_idx + 1) {
                  next_start.saturating_sub(1)
                } else {
                  total_chars
                };
                let target_len = target_end.saturating_sub(target_start);
                let target_caret_raw = target_start.saturating_add(preferred_col.min(target_len));
                let grapheme_boundaries = grapheme_cluster_boundaries_char_idx(&current);
                // Even in the fallback (no-layout) vertical movement path, ensure the caret never
                // lands inside a grapheme cluster (e.g. ZWJ emoji sequences).
                let down =
                  snap_char_idx_down_to_grapheme_boundary(&grapheme_boundaries, target_caret_raw);
                let up = snap_char_idx_up_to_grapheme_boundary(&grapheme_boundaries, target_caret_raw);
                edit.caret = if target_caret_raw.saturating_sub(down)
                  <= up.saturating_sub(target_caret_raw)
                {
                  down
                } else {
                  up
                };
                edit.caret_affinity = CaretAffinity::Downstream;
                if !extend_selection {
                  edit.selection_anchor = None;
                }
                edit.preferred_x = Some(preferred_col as f32 * char_advance);
              }
            }
          }
        }
        KeyAction::PageUp | KeyAction::PageDown => {
          // Native text controls typically consume PageUp/PageDown for internal scrolling/caret
          // navigation. We do not implement textarea scrolling yet, so treat these keys as a no-op
          // rather than falling back to viewport scrolling.
        }
        KeyAction::Space | KeyAction::ShiftSpace => {
          // Handled by `key_activate` (may trigger navigation).
        }
        KeyAction::Tab | KeyAction::ShiftTab => debug_assert!(false, "handled above"),
      }

      if edit != original {
        self.text_edit = Some(edit);
        changed = true;
      }

      changed |= self.sync_text_edit_paint_state();
      return changed;
    }

    // Non-text-control keyboard actions.
    //
    // Some windowed backends (e.g. winit) encode Shift-modified arrow and Home/End keys as distinct
    // `KeyAction` variants. For non-text controls like `<input type=range>` and `<select>`, these
    // Shift variants should behave like their base keys (they do not extend text selection).
    //
    // Note: Text-control Shift semantics are handled earlier in this method and must remain intact.
    let focused_is_range_input = index.node(focused).is_some_and(is_range_input);
    let focused_is_select = index.node(focused).is_some_and(is_select);
    let key = if focused_is_range_input || focused_is_select {
      match key {
        KeyAction::ShiftArrowLeft => KeyAction::ArrowLeft,
        KeyAction::ShiftArrowRight => KeyAction::ArrowRight,
        KeyAction::ShiftArrowUp => KeyAction::ArrowUp,
        KeyAction::ShiftArrowDown => KeyAction::ArrowDown,
        KeyAction::ShiftHome => KeyAction::Home,
        KeyAction::ShiftEnd => KeyAction::End,
        _ => key,
      }
    } else {
      key
    };

    match key {
      KeyAction::ArrowUp
      | KeyAction::ArrowDown
      | KeyAction::ArrowLeft
      | KeyAction::ArrowRight
      | KeyAction::Home
      | KeyAction::End => {
        if focused_is_range_input {
          if node_or_ancestor_is_inert(&index, focused)
            || node_is_disabled(&index, focused)
            || node_is_readonly(&index, focused)
          {
            return changed;
          }
          let bounds = if matches!(key, KeyAction::Home | KeyAction::End) {
            index.node(focused).and_then(crate::dom::input_range_bounds)
          } else {
            None
          };
          self.ensure_form_default_snapshot_for_control(&index, focused);
          if let Some(node_mut) = index.node_mut(focused) {
            let dom_changed = match key {
              KeyAction::ArrowUp | KeyAction::ArrowRight => {
                dom_mutation::step_range_value(node_mut, 1)
              }
              KeyAction::ArrowDown | KeyAction::ArrowLeft => {
                dom_mutation::step_range_value(node_mut, -1)
              }
              KeyAction::Home => bounds
                .map(|(min, _)| dom_mutation::set_range_value(node_mut, min))
                .unwrap_or(false),
              KeyAction::End => bounds
                .map(|(_, max)| dom_mutation::set_range_value(node_mut, max))
                .unwrap_or(false),
              _ => false,
            };
            changed |= dom_changed;
            if dom_changed {
              changed |= self.mark_user_validity(focused);
            }
          }
        } else if matches!(
          key,
          KeyAction::ArrowUp | KeyAction::ArrowDown | KeyAction::Home | KeyAction::End
        ) && focused_is_select
          && !is_disabled_or_inert(&index, focused)
        {
          let is_multiple = index
            .node(focused)
            .is_some_and(|node| node.get_attribute_ref("multiple").is_some());

          if matches!(key, KeyAction::Home | KeyAction::End)
            && is_multiple
          {
            // Home/End selection is only supported for single-select controls.
            return changed;
          }

          // Prefer the `BoxTree`'s `SelectControl` snapshot when available so keyboard navigation
          // matches what is painted (e.g. skipping `display:none` options). Fall back to DOM order
          // before the first render.
          let options: Vec<(usize, bool)> = if let Some(box_tree) = box_tree {
            if let Some((_, control, computed_disabled, _)) =
              select_control_snapshot_from_box_tree(box_tree, focused)
            {
              if computed_disabled {
                return changed;
              }
              control
                .items
                .iter()
                .filter_map(|item| match item {
                  SelectItem::Option {
                    node_id, disabled, ..
                  } => Some((*node_id, *disabled)),
                  _ => None,
                })
                .collect()
            } else {
              collect_select_option_nodes_dom(&index, focused)
            }
          } else {
            collect_select_option_nodes_dom(&index, focused)
          };

          if options.is_empty() {
            return changed;
          }

          let mut last_selected_idx: Option<usize> = None;
          let mut first_enabled_idx: Option<usize> = None;
          let mut last_enabled_idx: Option<usize> = None;
          for (idx, (id, disabled)) in options.iter().enumerate() {
            if index
              .node(*id)
              .and_then(|node| node.get_attribute_ref("selected"))
              .is_some()
            {
              last_selected_idx = Some(idx);
            }
            if !*disabled {
              if first_enabled_idx.is_none() {
                first_enabled_idx = Some(idx);
              }
              last_enabled_idx = Some(idx);
            }
          }

          let Some(first_enabled_idx) = first_enabled_idx else {
            return changed;
          };
          let last_enabled_idx = last_enabled_idx.unwrap_or(first_enabled_idx);

          // Anchor: last selected option; fallback to first enabled option.
          let anchor_idx = last_selected_idx.unwrap_or(first_enabled_idx);

          let next_idx = match key {
            KeyAction::ArrowDown => {
              let mut found = None;
              for idx in (anchor_idx + 1)..options.len() {
                if !options[idx].1 {
                  found = Some(idx);
                  break;
                }
              }
              found.unwrap_or(last_enabled_idx)
            }
            KeyAction::ArrowUp => {
              let mut found = None;
              for idx in (0..anchor_idx).rev() {
                if !options[idx].1 {
                  found = Some(idx);
                  break;
                }
              }
              found.unwrap_or(first_enabled_idx)
            }
            KeyAction::Home => first_enabled_idx,
            KeyAction::End => last_enabled_idx,
            _ => anchor_idx,
          };

          // If we clamped and the anchor was already selected, treat as a no-op (avoids clearing
          // unrelated selections in multi-select).
          if next_idx == anchor_idx && last_selected_idx.is_some() {
            return changed;
          }

          let Some(option_node_id) = options.get(next_idx).map(|(id, _)| *id) else {
            return changed;
          };
          // Keep the Shift range-selection anchor consistent with keyboard-driven selection.
          self.select_listbox_anchor.insert(focused, option_node_id);

          if is_multiple {
            // MVP multi-select arrow-key behaviour: treat the last selected option as the active
            // one, and move only that selection to the next enabled option without affecting any
            // unrelated selected options.
            let from_option_node_id = options[anchor_idx].0;

            self.ensure_form_default_snapshot_for_control(&index, focused);
            let dom_changed = dom_mutation::move_select_option_selection(
              dom,
              focused,
              from_option_node_id,
              option_node_id,
            );
            changed |= dom_changed;
            if dom_changed {
              changed |= self.mark_user_validity(focused);
            }
          } else {
            changed |= self.activate_select_option(dom, focused, option_node_id, false);
          }
        }
      }
      KeyAction::Tab | KeyAction::ShiftTab => debug_assert!(false, "handled above"),
      _ => {}
    }

    changed
  }

  /// Handle keyboard activation for the currently focused element.
  ///
  /// This is similar to `pointer_up` but uses the focused element as the target.
  ///
  /// - Backspace edits text controls (input/textarea).
  /// - Enter inserts a newline in a focused textarea; otherwise it activates the element.
  /// - Space activates/toggles choice controls (checkbox/radio).
  pub fn key_activate(
    &mut self,
    dom: &mut DomNode,
    key: KeyAction,
    document_url: &str,
    base_url: &str,
  ) -> (bool, InteractionAction) {
    self.key_activate_with_box_tree(dom, None, key, document_url, base_url)
  }

  /// Like [`InteractionEngine::key_activate`], but optionally supplies the cached [`BoxTree`].
  pub fn key_activate_with_box_tree(
    &mut self,
    dom: &mut DomNode,
    box_tree: Option<&BoxTree>,
    key: KeyAction,
    document_url: &str,
    base_url: &str,
  ) -> (bool, InteractionAction) {
    self.last_form_submitter = None;
    self.last_form_submitter_element_id = None;
    let prev_focus = self.state.focused;

    self.modality = InputModality::Keyboard;

    // Delegate text-editing keys to `key_action` so behaviour stays consistent.
    match key {
      KeyAction::Backspace
      | KeyAction::Delete
      | KeyAction::WordBackspace
      | KeyAction::WordDelete
      | KeyAction::ArrowLeft
      | KeyAction::ArrowRight
      | KeyAction::WordLeft
      | KeyAction::WordRight
      | KeyAction::ShiftWordLeft
      | KeyAction::ShiftWordRight
      | KeyAction::ShiftArrowLeft
      | KeyAction::ShiftArrowRight
      | KeyAction::ShiftArrowUp
      | KeyAction::ShiftArrowDown
      | KeyAction::ShiftHome
      | KeyAction::ShiftEnd
      | KeyAction::SelectAll
      | KeyAction::Undo
      | KeyAction::Redo => {
        return (
          self.key_action_with_box_tree(dom, box_tree, key),
          InteractionAction::None,
        );
      }
      KeyAction::Tab | KeyAction::ShiftTab => {
        let dom_changed = self.key_action_with_box_tree(dom, box_tree, key);
        let action = if self.state.focused != prev_focus {
          InteractionAction::FocusChanged {
            node_id: self.state.focused,
          }
        } else {
          InteractionAction::None
        };
        return (dom_changed, action);
      }
      KeyAction::Enter => {
        let Some(focused) = self.state.focused else {
          return (false, InteractionAction::None);
        };
        let index = DomIndexMut::new(dom);
        if index.node(focused).is_some_and(is_textarea) {
          return (
            self.key_action_with_box_tree(dom, box_tree, KeyAction::Enter),
            InteractionAction::None,
          );
        }
      }
      KeyAction::Space | KeyAction::ShiftSpace => {}
      KeyAction::ArrowUp
      | KeyAction::ArrowDown
      | KeyAction::Home
      | KeyAction::End
      | KeyAction::PageUp
      | KeyAction::PageDown => {
        return (
          self.key_action_with_box_tree(dom, box_tree, key),
          InteractionAction::None,
        );
      }
    }

    let Some(focused) = self.state.focused else {
      return (false, InteractionAction::None);
    };

    // Ensure focus-visible when activation is driven by the keyboard.
    let mut index = DomIndexMut::new(dom);
    let mut changed = false;
    changed |= self.set_focus(&mut index, Some(focused), true);

    let mut action = InteractionAction::None;

    match key {
      KeyAction::Enter => {
        if node_or_ancestor_is_inert(&index, focused) {
          // Inert subtrees are not interactive.
        } else if let Some(details_id) = details_owner_for_summary(&index, focused) {
          changed |= toggle_details_open(&mut index, details_id);
        } else if let Some(kind) = index
          .node(focused)
          .and_then(|node| date_time_input_kind(node))
        {
          if !node_is_disabled(&index, focused) && !node_is_readonly(&index, focused) {
            action = InteractionAction::OpenDateTimePicker {
              input_node_id: focused,
              kind,
            };
          }
        } else if index.node(focused).is_some_and(is_color_input) {
          if !node_is_disabled(&index, focused) {
            action = InteractionAction::OpenColorPicker {
              input_node_id: focused,
            };
          }
        } else if index.node(focused).is_some_and(is_file_input) {
          if !node_is_disabled(&index, focused) {
            let multiple = index
              .node(focused)
              .is_some_and(|node| node.get_attribute_ref("multiple").is_some());
            let accept = index
              .node(focused)
              .and_then(|node| node.get_attribute_ref("accept"))
              .map(trim_ascii_whitespace)
              .filter(|v| !v.is_empty())
              .map(|v| v.to_string());
            action = InteractionAction::OpenFilePicker {
              input_node_id: focused,
              multiple,
              accept,
            };
          }
        } else if index.node(focused).is_some_and(is_select) {
          let mut computed_disabled = false;
          let control = match box_tree
            .and_then(|box_tree| select_control_snapshot_from_box_tree(box_tree, focused))
          {
            Some((_, control, disabled, _)) => {
              computed_disabled = disabled;
              Some(control)
            }
            None => select_control_snapshot_from_dom(&index, focused),
          };
          if let Some(control) = control {
            let disabled = is_disabled_or_inert(&index, focused) || computed_disabled;
            if !disabled {
              let is_dropdown = !control.multiple && control.size == 1;
              if is_dropdown {
                action = InteractionAction::OpenSelectDropdown {
                  select_node_id: focused,
                  control,
                };
              }
            }
          }
        } else if let Some(href) = index
          .node(focused)
          .filter(|node| is_anchor_with_href(node))
          .and_then(|node| node.get_attribute_ref("href"))
        {
          if let Some(resolved) = resolve_url(base_url, href) {
            changed |= self.state.insert_visited_link(focused);
            let download_attr = index.node(focused).and_then(|node| node.get_attribute_ref("download"));
            let is_download = download_attr.is_some();
            let download_name = download_attr
              .map(trim_ascii_whitespace)
              .filter(|v| !v.is_empty())
              .map(|v| v.to_string());
            let target_blank = index
              .node(focused)
              .and_then(|node| node.get_attribute_ref("target"))
              .is_some_and(|target| trim_ascii_whitespace(target).eq_ignore_ascii_case("_blank"));
            action = if is_download {
              InteractionAction::Download {
                href: resolved,
                file_name: download_name,
              }
            } else if target_blank {
              InteractionAction::OpenInNewTab { href: resolved }
            } else {
              InteractionAction::Navigate { href: resolved }
            };
          }
        } else if index.node(focused).is_some_and(is_checkbox_input) {
          if !node_is_disabled(&index, focused) {
            self.ensure_form_default_snapshot_for_control(&index, focused);
            if let Some(node_mut) = index.node_mut(focused) {
              let dom_changed = dom_mutation::toggle_checkbox(node_mut);
              changed |= dom_changed;
              if dom_changed {
                changed |= self.mark_user_validity(focused);
              }
            }
          }
        } else if index.node(focused).is_some_and(is_radio_input) {
          if !node_is_disabled(&index, focused) {
            self.ensure_form_default_snapshot_for_control(&index, focused);
            let dom_changed = dom_mutation::activate_radio(dom, focused);
            changed |= dom_changed;
            if dom_changed {
              changed |= self.mark_user_validity(focused);
            }
          }
        } else if index.node(focused).is_some_and(is_reset_control) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled reset controls do not reset.
          } else {
            changed |= self.perform_form_reset(&mut index, focused);
          }
        } else if index.node(focused).is_some_and(is_submit_control) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled submit controls do not submit.
          } else {
            // A form submission attempt flips HTML "user validity" so `:user-invalid` matches.
            changed |= self.mark_user_validity(focused);
            changed |= self.mark_form_user_validity(&index, focused);
            let image_coords = index
              .node(focused)
              .is_some_and(is_image_submit_input)
              .then_some((0, 0));
            if let Some(submission) = form_submission(
              dom,
              focused,
              image_coords,
              document_url,
              base_url,
              Some(&self.state),
            ) {
              self.last_form_submitter = Some(focused);
              self.last_form_submitter_element_id = element_id_for_node(&index, focused);
              let target_blank = resolve_form_owner(&index, focused)
                .is_some_and(|form_id| submission_target_is_blank(&index, Some(focused), form_id));
              match submission.method {
                FormSubmissionMethod::Get => {
                  if target_blank {
                    action = InteractionAction::OpenInNewTab {
                      href: submission.url,
                    };
                  } else {
                    action = InteractionAction::Navigate {
                      href: submission.url,
                    };
                  }
                }
                FormSubmissionMethod::Post => {
                  if target_blank {
                    action = InteractionAction::OpenInNewTabRequest {
                      request: submission,
                    };
                  } else {
                    action = InteractionAction::NavigateRequest {
                      request: submission,
                    };
                  }
                }
              }
            }
          }
        } else if index.node(focused).is_some_and(is_text_input) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled controls do not submit.
          } else {
            // Pressing Enter in a text field can submit the form; flip user validity as well.
            changed |= self.mark_user_validity(focused);
            changed |= self.mark_form_user_validity(&index, focused);
            if let Some(form_id) = resolve_form_owner(&index, focused) {
              let submitter_id = find_default_form_submitter(&index, form_id);
              let target_blank = submission_target_is_blank(&index, submitter_id, form_id);
              let submission = match submitter_id {
                Some(submitter_id) => {
                  let image_coords = index
                    .node(submitter_id)
                    .is_some_and(is_image_submit_input)
                    .then_some((0, 0));
                  form_submission(
                    dom,
                    submitter_id,
                    image_coords,
                    document_url,
                    base_url,
                    Some(&self.state),
                  )
                }
                None => form_submission_without_submitter(
                  dom,
                  form_id,
                  document_url,
                  base_url,
                  Some(&self.state),
                ),
              };
                if let Some(submission) = submission {
                  if let Some(submitter_id) = submitter_id {
                    self.last_form_submitter = Some(submitter_id);
                    self.last_form_submitter_element_id = element_id_for_node(&index, submitter_id);
                  }
                  match submission.method {
                  FormSubmissionMethod::Get => {
                    if target_blank {
                      action = InteractionAction::OpenInNewTab {
                        href: submission.url,
                      };
                    } else {
                      action = InteractionAction::Navigate {
                        href: submission.url,
                      };
                    }
                  }
                  FormSubmissionMethod::Post => {
                    if target_blank {
                      action = InteractionAction::OpenInNewTabRequest {
                        request: submission,
                      };
                    } else {
                      action = InteractionAction::NavigateRequest {
                        request: submission,
                      };
                    }
                  }
                }
              }
            }
          }
        }
      }
      KeyAction::Space | KeyAction::ShiftSpace => {
        if node_or_ancestor_is_inert(&index, focused) {
          // Inert subtrees are not interactive.
        } else if let Some(details_id) = details_owner_for_summary(&index, focused) {
          changed |= toggle_details_open(&mut index, details_id);
        } else if let Some(kind) = index
          .node(focused)
          .and_then(|node| date_time_input_kind(node))
        {
          if !node_is_disabled(&index, focused) && !node_is_readonly(&index, focused) {
            action = InteractionAction::OpenDateTimePicker {
              input_node_id: focused,
              kind,
            };
          }
        } else if index.node(focused).is_some_and(is_color_input) {
          if !node_is_disabled(&index, focused) {
            action = InteractionAction::OpenColorPicker {
              input_node_id: focused,
            };
          }
        } else if index.node(focused).is_some_and(is_file_input) {
          if !node_is_disabled(&index, focused) {
            let multiple = index
              .node(focused)
              .is_some_and(|node| node.get_attribute_ref("multiple").is_some());
            let accept = index
              .node(focused)
              .and_then(|node| node.get_attribute_ref("accept"))
              .map(trim_ascii_whitespace)
              .filter(|v| !v.is_empty())
              .map(|v| v.to_string());
            action = InteractionAction::OpenFilePicker {
              input_node_id: focused,
              multiple,
              accept,
            };
          }
        } else if index.node(focused).is_some_and(is_select) {
          let mut computed_disabled = false;
          let control = match box_tree
            .and_then(|box_tree| select_control_snapshot_from_box_tree(box_tree, focused))
          {
            Some((_, control, disabled, _)) => {
              computed_disabled = disabled;
              Some(control)
            }
            None => select_control_snapshot_from_dom(&index, focused),
          };
          if let Some(control) = control {
            let disabled = is_disabled_or_inert(&index, focused) || computed_disabled;
            if !disabled {
              let is_dropdown = !control.multiple && control.size == 1;
              if is_dropdown {
                action = InteractionAction::OpenSelectDropdown {
                  select_node_id: focused,
                  control,
                };
              }
            }
          }
        } else if index.node(focused).is_some_and(is_checkbox_input) {
          if !node_is_disabled(&index, focused) {
            self.ensure_form_default_snapshot_for_control(&index, focused);
            if let Some(node_mut) = index.node_mut(focused) {
              let dom_changed = dom_mutation::toggle_checkbox(node_mut);
              changed |= dom_changed;
              if dom_changed {
                changed |= self.mark_user_validity(focused);
              }
            }
          }
        } else if index.node(focused).is_some_and(is_radio_input) {
          if !node_is_disabled(&index, focused) {
            self.ensure_form_default_snapshot_for_control(&index, focused);
            let dom_changed = dom_mutation::activate_radio(dom, focused);
            changed |= dom_changed;
            if dom_changed {
              changed |= self.mark_user_validity(focused);
            }
          }
        } else if index.node(focused).is_some_and(is_reset_control) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled reset controls do not reset.
          } else {
            changed |= self.perform_form_reset(&mut index, focused);
          }
        } else if index.node(focused).is_some_and(is_submit_control) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled submit controls do not submit.
          } else {
            changed |= self.mark_user_validity(focused);
            changed |= self.mark_form_user_validity(&index, focused);
            let image_coords = index
              .node(focused)
              .is_some_and(is_image_submit_input)
              .then_some((0, 0));
            if let Some(submission) = form_submission(
              dom,
              focused,
              image_coords,
              document_url,
              base_url,
              Some(&self.state),
            ) {
              self.last_form_submitter = Some(focused);
              self.last_form_submitter_element_id = element_id_for_node(&index, focused);
              let target_blank = resolve_form_owner(&index, focused)
                .is_some_and(|form_id| submission_target_is_blank(&index, Some(focused), form_id));
              match submission.method {
                FormSubmissionMethod::Get => {
                  if target_blank {
                    action = InteractionAction::OpenInNewTab {
                      href: submission.url,
                    };
                  } else {
                    action = InteractionAction::Navigate {
                      href: submission.url,
                    };
                  }
                }
                FormSubmissionMethod::Post => {
                  if target_blank {
                    action = InteractionAction::OpenInNewTabRequest {
                      request: submission,
                    };
                  } else {
                    action = InteractionAction::NavigateRequest {
                      request: submission,
                    };
                  }
                }
              }
            }
          }
        } else if index.node(focused).is_some_and(is_button) {
          // MVP: no-op for non-submit buttons (no JS event dispatch yet).
        }
      }
      _ => {}
    }

    if !matches!(
      action,
      InteractionAction::Navigate { .. }
        | InteractionAction::OpenInNewTab { .. }
        | InteractionAction::OpenInNewTabRequest { .. }
        | InteractionAction::Download { .. }
        | InteractionAction::NavigateRequest { .. }
    ) && self.state.focused != prev_focus
    {
      action = InteractionAction::FocusChanged {
        node_id: self.state.focused,
      };
    }

    (changed, action)
  }

  /// Like [`InteractionEngine::key_activate_with_box_tree`], but also supplies the cached
  /// [`FragmentTree`].
  ///
  /// This enables activation paths that depend on layout geometry, such as soft-wrap-aware
  /// `<textarea>` caret movement in response to ArrowUp/ArrowDown.
  pub fn key_activate_with_layout_artifacts(
    &mut self,
    dom: &mut DomNode,
    box_tree: Option<&BoxTree>,
    fragment_tree: &FragmentTree,
    key: KeyAction,
    document_url: &str,
    base_url: &str,
  ) -> (bool, InteractionAction) {
    self.last_form_submitter = None;
    self.last_form_submitter_element_id = None;
    let prev_focus = self.state.focused;

    self.modality = InputModality::Keyboard;

    // Delegate text-editing keys to `key_action` so behaviour stays consistent.
    match key {
      KeyAction::Backspace
      | KeyAction::Delete
      | KeyAction::WordBackspace
      | KeyAction::WordDelete
      | KeyAction::ArrowLeft
      | KeyAction::ArrowRight
      | KeyAction::WordLeft
      | KeyAction::WordRight
      | KeyAction::ShiftWordLeft
      | KeyAction::ShiftWordRight
      | KeyAction::ShiftArrowLeft
      | KeyAction::ShiftArrowRight
      | KeyAction::ShiftArrowUp
      | KeyAction::ShiftArrowDown
      | KeyAction::ShiftHome
      | KeyAction::ShiftEnd
      | KeyAction::SelectAll
      | KeyAction::Undo
      | KeyAction::Redo => {
        return (
          self.key_action_with_layout_artifacts(dom, box_tree, fragment_tree, key),
          InteractionAction::None,
        );
      }
      KeyAction::Tab | KeyAction::ShiftTab => {
        let dom_changed = self.key_action_with_layout_artifacts(dom, box_tree, fragment_tree, key);
        let action = if self.state.focused != prev_focus {
          InteractionAction::FocusChanged {
            node_id: self.state.focused,
          }
        } else {
          InteractionAction::None
        };
        return (dom_changed, action);
      }
      KeyAction::Enter => {
        let Some(focused) = self.state.focused else {
          return (false, InteractionAction::None);
        };
        let index = DomIndexMut::new(dom);
        if index.node(focused).is_some_and(is_textarea) {
          return (
            self.key_action_with_layout_artifacts(dom, box_tree, fragment_tree, KeyAction::Enter),
            InteractionAction::None,
          );
        }
      }
      KeyAction::Space | KeyAction::ShiftSpace => {}
      KeyAction::ArrowUp
      | KeyAction::ArrowDown
      | KeyAction::Home
      | KeyAction::End
      | KeyAction::PageUp
      | KeyAction::PageDown => {
        return (
          self.key_action_with_layout_artifacts(dom, box_tree, fragment_tree, key),
          InteractionAction::None,
        );
      }
    }

    let Some(focused) = self.state.focused else {
      return (false, InteractionAction::None);
    };

    // Ensure focus-visible when activation is driven by the keyboard.
    let mut index = DomIndexMut::new(dom);
    let mut changed = false;
    changed |= self.set_focus(&mut index, Some(focused), true);

    let mut action = InteractionAction::None;

    match key {
      KeyAction::Enter => {
        if node_or_ancestor_is_inert(&index, focused) {
          // Inert subtrees are not interactive.
        } else if let Some(details_id) = details_owner_for_summary(&index, focused) {
          changed |= toggle_details_open(&mut index, details_id);
        } else if let Some(kind) = index
          .node(focused)
          .and_then(|node| date_time_input_kind(node))
        {
          if !node_is_disabled(&index, focused) && !node_is_readonly(&index, focused) {
            action = InteractionAction::OpenDateTimePicker {
              input_node_id: focused,
              kind,
            };
          }
        } else if index.node(focused).is_some_and(is_color_input) {
          if !node_is_disabled(&index, focused) {
            action = InteractionAction::OpenColorPicker {
              input_node_id: focused,
            };
          }
        } else if index.node(focused).is_some_and(is_file_input) {
          if !node_is_disabled(&index, focused) {
            let multiple = index
              .node(focused)
              .is_some_and(|node| node.get_attribute_ref("multiple").is_some());
            let accept = index
              .node(focused)
              .and_then(|node| node.get_attribute_ref("accept"))
              .map(trim_ascii_whitespace)
              .filter(|v| !v.is_empty())
              .map(|v| v.to_string());
            action = InteractionAction::OpenFilePicker {
              input_node_id: focused,
              multiple,
              accept,
            };
          }
        } else if index.node(focused).is_some_and(is_select) {
          let mut computed_disabled = false;
          let control = match box_tree
            .and_then(|box_tree| select_control_snapshot_from_box_tree(box_tree, focused))
          {
            Some((_, control, disabled, _)) => {
              computed_disabled = disabled;
              Some(control)
            }
            None => select_control_snapshot_from_dom(&index, focused),
          };
          if let Some(control) = control {
            let disabled = is_disabled_or_inert(&index, focused) || computed_disabled;
            if !disabled {
              let is_dropdown = !control.multiple && control.size == 1;
              if is_dropdown {
                action = InteractionAction::OpenSelectDropdown {
                  select_node_id: focused,
                  control,
                };
              }
            }
          }
        } else if let Some(href) = index
          .node(focused)
          .filter(|node| is_anchor_with_href(node))
          .and_then(|node| node.get_attribute_ref("href"))
        {
          if let Some(resolved) = resolve_url(base_url, href) {
            changed |= self.state.insert_visited_link(focused);
            let target_blank = index
              .node(focused)
              .and_then(|node| node.get_attribute_ref("target"))
              .is_some_and(|target| trim_ascii_whitespace(target).eq_ignore_ascii_case("_blank"));
            action = if target_blank {
              InteractionAction::OpenInNewTab { href: resolved }
            } else {
              InteractionAction::Navigate { href: resolved }
            };
          }
        } else if index.node(focused).is_some_and(is_checkbox_input) {
          if !node_is_disabled(&index, focused) {
            self.ensure_form_default_snapshot_for_control(&index, focused);
            if let Some(node_mut) = index.node_mut(focused) {
              let dom_changed = dom_mutation::toggle_checkbox(node_mut);
              changed |= dom_changed;
              if dom_changed {
                changed |= self.mark_user_validity(focused);
              }
            }
          }
        } else if index.node(focused).is_some_and(is_radio_input) {
          if !node_is_disabled(&index, focused) {
            self.ensure_form_default_snapshot_for_control(&index, focused);
            let dom_changed = dom_mutation::activate_radio(dom, focused);
            changed |= dom_changed;
            if dom_changed {
              changed |= self.mark_user_validity(focused);
            }
          }
        } else if index.node(focused).is_some_and(is_reset_control) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled reset controls do not reset.
          } else {
            changed |= self.perform_form_reset(&mut index, focused);
          }
        } else if index.node(focused).is_some_and(is_submit_control) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled submit controls do not submit.
          } else {
            // A form submission attempt flips HTML "user validity" so `:user-invalid` matches.
            changed |= self.mark_user_validity(focused);
            changed |= self.mark_form_user_validity(&index, focused);
            let image_coords = index
              .node(focused)
              .is_some_and(is_image_submit_input)
              .then_some((0, 0));
            if let Some(submission) = form_submission(
              dom,
              focused,
              image_coords,
              document_url,
              base_url,
              Some(&self.state),
            ) {
              self.last_form_submitter = Some(focused);
              self.last_form_submitter_element_id = element_id_for_node(&index, focused);
              let target_blank = resolve_form_owner(&index, focused)
                .is_some_and(|form_id| submission_target_is_blank(&index, Some(focused), form_id));
              match submission.method {
                FormSubmissionMethod::Get => {
                  if target_blank {
                    action = InteractionAction::OpenInNewTab {
                      href: submission.url,
                    };
                  } else {
                    action = InteractionAction::Navigate {
                      href: submission.url,
                    };
                  }
                }
                FormSubmissionMethod::Post => {
                  if target_blank {
                    action = InteractionAction::OpenInNewTabRequest {
                      request: submission,
                    };
                  } else {
                    action = InteractionAction::NavigateRequest {
                      request: submission,
                    };
                  }
                }
              }
            }
          }
        } else if index.node(focused).is_some_and(is_text_input) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled controls do not submit.
          } else {
            // Pressing Enter in a text field can submit the form; flip user validity as well.
            changed |= self.mark_user_validity(focused);
            changed |= self.mark_form_user_validity(&index, focused);
            if let Some(form_id) = resolve_form_owner(&index, focused) {
              let submitter_id = find_default_form_submitter(&index, form_id);
              let target_blank = submission_target_is_blank(&index, submitter_id, form_id);
              let submission = match submitter_id {
                Some(submitter_id) => {
                  let image_coords = index
                    .node(submitter_id)
                    .is_some_and(is_image_submit_input)
                    .then_some((0, 0));
                  form_submission(
                    dom,
                    submitter_id,
                    image_coords,
                    document_url,
                    base_url,
                    Some(&self.state),
                  )
                }
                None => form_submission_without_submitter(
                  dom,
                  form_id,
                  document_url,
                  base_url,
                  Some(&self.state),
                ),
              };
              if let Some(submission) = submission {
                if let Some(submitter_id) = submitter_id {
                  self.last_form_submitter = Some(submitter_id);
                  self.last_form_submitter_element_id = element_id_for_node(&index, submitter_id);
                }
                match submission.method {
                  FormSubmissionMethod::Get => {
                    if target_blank {
                      action = InteractionAction::OpenInNewTab {
                        href: submission.url,
                      };
                    } else {
                      action = InteractionAction::Navigate {
                        href: submission.url,
                      };
                    }
                  }
                  FormSubmissionMethod::Post => {
                    if target_blank {
                      action = InteractionAction::OpenInNewTabRequest {
                        request: submission,
                      };
                    } else {
                      action = InteractionAction::NavigateRequest {
                        request: submission,
                      };
                    }
                  }
                }
              }
            }
          }
        }
      }
      KeyAction::Space | KeyAction::ShiftSpace => {
        if node_or_ancestor_is_inert(&index, focused) {
          // Inert subtrees are not interactive.
        } else if let Some(details_id) = details_owner_for_summary(&index, focused) {
          changed |= toggle_details_open(&mut index, details_id);
        } else if let Some(kind) = index
          .node(focused)
          .and_then(|node| date_time_input_kind(node))
        {
          if !node_is_disabled(&index, focused) && !node_is_readonly(&index, focused) {
            action = InteractionAction::OpenDateTimePicker {
              input_node_id: focused,
              kind,
            };
          }
        } else if index.node(focused).is_some_and(is_color_input) {
          if !node_is_disabled(&index, focused) {
            action = InteractionAction::OpenColorPicker {
              input_node_id: focused,
            };
          }
        } else if index.node(focused).is_some_and(is_file_input) {
          if !node_is_disabled(&index, focused) {
            let multiple = index
              .node(focused)
              .is_some_and(|node| node.get_attribute_ref("multiple").is_some());
            let accept = index
              .node(focused)
              .and_then(|node| node.get_attribute_ref("accept"))
              .map(trim_ascii_whitespace)
              .filter(|v| !v.is_empty())
              .map(|v| v.to_string());
            action = InteractionAction::OpenFilePicker {
              input_node_id: focused,
              multiple,
              accept,
            };
          }
        } else if index.node(focused).is_some_and(is_select) {
          let mut computed_disabled = false;
          let control = match box_tree
            .and_then(|box_tree| select_control_snapshot_from_box_tree(box_tree, focused))
          {
            Some((_, control, disabled, _)) => {
              computed_disabled = disabled;
              Some(control)
            }
            None => select_control_snapshot_from_dom(&index, focused),
          };
          if let Some(control) = control {
            let disabled = is_disabled_or_inert(&index, focused) || computed_disabled;
            if !disabled {
              let is_dropdown = !control.multiple && control.size == 1;
              if is_dropdown {
                action = InteractionAction::OpenSelectDropdown {
                  select_node_id: focused,
                  control,
                };
              }
            }
          }
        } else if index.node(focused).is_some_and(is_checkbox_input) {
          if !node_is_disabled(&index, focused) {
            self.ensure_form_default_snapshot_for_control(&index, focused);
            if let Some(node_mut) = index.node_mut(focused) {
              let dom_changed = dom_mutation::toggle_checkbox(node_mut);
              changed |= dom_changed;
              if dom_changed {
                changed |= self.mark_user_validity(focused);
              }
            }
          }
        } else if index.node(focused).is_some_and(is_radio_input) {
          if !node_is_disabled(&index, focused) {
            self.ensure_form_default_snapshot_for_control(&index, focused);
            let dom_changed = dom_mutation::activate_radio(dom, focused);
            changed |= dom_changed;
            if dom_changed {
              changed |= self.mark_user_validity(focused);
            }
          }
        } else if index.node(focused).is_some_and(is_reset_control) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled reset controls do not reset.
          } else {
            changed |= self.perform_form_reset(&mut index, focused);
          }
        } else if index.node(focused).is_some_and(is_submit_control) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled submit controls do not submit.
          } else {
            changed |= self.mark_user_validity(focused);
            changed |= self.mark_form_user_validity(&index, focused);
            let image_coords = index
              .node(focused)
              .is_some_and(is_image_submit_input)
              .then_some((0, 0));
            if let Some(submission) = form_submission(
              dom,
              focused,
              image_coords,
              document_url,
              base_url,
              Some(&self.state),
            ) {
              self.last_form_submitter = Some(focused);
              self.last_form_submitter_element_id = element_id_for_node(&index, focused);
              let target_blank = resolve_form_owner(&index, focused)
                .is_some_and(|form_id| submission_target_is_blank(&index, Some(focused), form_id));
              match submission.method {
                FormSubmissionMethod::Get => {
                  if target_blank {
                    action = InteractionAction::OpenInNewTab {
                      href: submission.url,
                    };
                  } else {
                    action = InteractionAction::Navigate {
                      href: submission.url,
                    };
                  }
                }
                FormSubmissionMethod::Post => {
                  if target_blank {
                    action = InteractionAction::OpenInNewTabRequest {
                      request: submission,
                    };
                  } else {
                    action = InteractionAction::NavigateRequest {
                      request: submission,
                    };
                  }
                }
              }
            }
          }
        } else if index.node(focused).is_some_and(is_button) {
          // MVP: no-op for non-submit buttons (no JS event dispatch yet).
        }
      }
      _ => {}
    }

    if !matches!(
      action,
      InteractionAction::Navigate { .. }
        | InteractionAction::OpenInNewTab { .. }
        | InteractionAction::OpenInNewTabRequest { .. }
        | InteractionAction::NavigateRequest { .. }
    ) && self.state.focused != prev_focus
    {
      action = InteractionAction::FocusChanged {
        node_id: self.state.focused,
      };
    }

    (changed, action)
  }
}

#[cfg(test)]
mod pointer_tests;

#[cfg(test)]
mod datalist_tests;

#[cfg(test)]
mod fuzz_tests;
