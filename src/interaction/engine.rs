use crate::dom::DomNode;
use crate::dom::DomNodeType;
use crate::geometry::Point;
use crate::geometry::Rect;
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
use crate::ui::messages::{PointerButton, PointerModifiers};
use crate::interaction::selection_serialize::{
  serialize_document_selection, DocumentSelection, DocumentSelectionPoint, DocumentSelectionRange,
};
use std::collections::HashMap;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

use super::dom_mutation;
use super::form_submit::{
  form_submission, form_submission_without_submitter, FormSubmission, FormSubmissionMethod,
};
use super::fragment_geometry::content_rect_for_border_rect;
use super::hit_test::{hit_test_dom, HitTestKind};
use super::image_maps;
use super::resolve_url;
use super::state::{
  DocumentSelectionRanges, DocumentSelectionState, ImePreeditState, InteractionState,
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
  ArrowLeft,
  ArrowRight,
  WordLeft,
  WordRight,
  ShiftArrowLeft,
  ShiftArrowRight,
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

#[derive(Debug, Clone)]
pub struct InteractionEngine {
  state: InteractionState,
  pointer_down_target: Option<usize>,
  range_drag: Option<RangeDragState>,
  number_spin: Option<NumberSpinState>,
  text_drag: Option<TextDragState>,
  document_drag: Option<DocumentDragState>,
  text_edit: Option<TextEditState>,
  text_undo: HashMap<usize, TextUndoHistory>,
  modality: InputModality,
  last_click_target: Option<usize>,
  last_form_submitter: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RangeDragState {
  node_id: usize,
  box_id: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DocumentDragState {
  anchor: DocumentSelectionPoint,
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
  focus_before: Option<usize>,
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
    let mut dom = crate::dom::parse_html(
      "<html><body><input dir=\"ltr\" value=\"ABC אבג DEF\"></body></html>",
    )
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

    let mut dom = crate::dom::parse_html(
      "<html><body><label>Label <input id=\"c\"></label></body></html>",
    )
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
    )
    .expect("submission");
    assert_eq!(
      submission.url,
      "https://example.com/submit?a=1",
      "controls in the first <legend> should not be considered disabled when collecting form data"
    );
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

fn is_anchor_with_href(node: &DomNode) -> bool {
  // MVP: treat <a href> and <area href> as focusable/navigable "links".
  node.tag_name().is_some_and(|tag| {
    (tag.eq_ignore_ascii_case("a") || tag.eq_ignore_ascii_case("area"))
      && node.get_attribute_ref("href").is_some_and(|href| {
        let href = trim_ascii_whitespace(href);
        !href.is_empty()
          && !href
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
  if href.is_empty() {
    return false;
  }
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

fn is_disabled_or_inert(index: &DomIndexMut, node_id: usize) -> bool {
  node_or_ancestor_is_inert(index, node_id) || node_is_disabled(index, node_id)
}

/// MVP focusable predicate for pointer focus / blur decisions.
///
/// This covers native interactive elements we currently support, plus `tabindex>=0` focusability.
fn is_focusable_interactive_element(index: &DomIndexMut, node_id: usize) -> bool {
  let Some(node) = index.node(node_id) else {
    return false;
  };

  if is_disabled_or_inert(index, node_id) {
    return false;
  }

  // MVP tabindex support: treat `tabindex < 0` as not focusable via pointer, and `tabindex >= 0`
  // as focusable (even for non-interactive elements).
  if let Some(tabindex) = parse_tabindex(node) {
    if tabindex < 0 {
      return false;
    }
    // `input type=hidden` is never focusable, even if tabindex is set.
    if is_input(node) && input_type(node).eq_ignore_ascii_case("hidden") {
      return false;
    }
    return true;
  }

  if is_anchor_with_href(node) {
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
  node_is_inert_like(node)
    || super::effective_disabled::node_self_is_hidden(node)
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
      GeneralCategory::NonspacingMark | GeneralCategory::SpacingMark | GeneralCategory::EnclosingMark
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

fn is_word_char(ch: char) -> bool {
  ch.is_alphanumeric() || ch == '_'
}

fn word_char_classes(text: &str) -> Vec<bool> {
  let mut out = Vec::with_capacity(text.chars().count());
  for segment in text.split_word_bounds() {
    for ch in segment.chars() {
      out.push(is_word_char(ch));
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
) -> Option<Vec<crate::text::pipeline::ShapedRun>> {
  if text.is_empty() {
    return Some(Vec::new());
  }
  let runs = super::shaping_pipeline_for_interaction()
    .shape(text, style, super::font_context_for_interaction())
    .ok()?;
  let mut runs = runs;
  TextItem::apply_spacing_to_runs(&mut runs, text, style.letter_spacing, style.word_spacing);
  Some(runs)
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
  style: &ComputedStyle,
  rect: Rect,
  x: f32,
) -> (usize, CaretAffinity) {
  let char_count = text.chars().count();
  if char_count == 0 {
    return (0, CaretAffinity::Downstream);
  }

  let fallback_advance = fallback_text_advance(text, style);
  let runs = shape_text_runs_for_interaction(text, style).unwrap_or_default();
  let total_advance = shaped_total_advance(&runs, fallback_advance);
  let start_x = aligned_text_start_x(style, rect, total_advance);

  let mut local_x = x - start_x;
  if !local_x.is_finite() {
    local_x = 0.0;
  }
  local_x = local_x.clamp(0.0, total_advance);

  let stops = crate::text::caret::caret_stops_for_runs(text, &runs, total_advance);
  let Some(best) = stops
    .iter()
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
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
  scroll: &ScrollState,
  node_id: usize,
  box_id: usize,
  page_point: Point,
) -> Option<(usize, CaretAffinity)> {
  let node = index.node(node_id)?;
  let box_node = box_node_by_id(box_tree, box_id)?;
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

    let line = layout.lines.get(line_idx).copied().unwrap_or(crate::textarea::TextareaVisualLine {
      start_char: 0,
      end_char: 0,
      start_byte: 0,
      end_byte: 0,
    });
    let caret_line = line.text(&value);
    let line_y = rect.y() + line_idx as f32 * line_height - scroll_y;
    let line_rect = Rect::from_xywh(rect.x(), line_y, rect.width(), line_height);
    let (caret_in_line, affinity) =
      caret_position_for_x_in_text(caret_line, style, line_rect, page_point.x);

    let total_chars = value.chars().count();
    let caret = line.start_char.saturating_add(caret_in_line).min(total_chars);
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

    let (caret, affinity) = caret_position_for_x_in_text(&display_text, style, rect, page_point.x);
    let total_chars = value.chars().count();
    return Some((caret.min(total_chars), affinity));
  }

  None
}

fn box_is_selectable_for_document_selection(box_node: &BoxNode) -> bool {
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
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
  page_point: Point,
) -> Option<DocumentSelectionPoint> {
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

  let box_node = box_node_by_id(box_tree, box_id)?;
  if !box_is_selectable_for_document_selection(box_node) {
    return None;
  }
  let node_id = box_node.styled_node_id?;

  let BoxType::Text(text_box) = &box_node.box_type else {
    return None;
  };

  let local_x = page_point.x - abs_origin.x;
  let runs: &[crate::text::pipeline::ShapedRun] = shaped
    .as_deref()
    .map(|runs| runs.as_slice())
    .unwrap_or(&[]);
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

  Some(DocumentSelectionPoint { node_id, char_offset })
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
  fragment_tree: &FragmentTree,
  node_id: usize,
  box_id: usize,
  page_point: Point,
) -> bool {
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

  let Some(node_mut) = index.node_mut(node_id) else {
    return false;
  };
  dom_mutation::set_range_value_from_ratio(node_mut, fraction)
}

fn number_input_spin_direction_at_point(
  index: &DomIndexMut,
  box_tree: &BoxTree,
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

  let box_node = box_node_by_id(box_tree, box_id)?;
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
      dom_mutation::activate_select_option(dom, select_id, *node_id, control.multiple)
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

fn select_control_snapshot_from_dom(index: &DomIndexMut, select_node_id: usize) -> Option<SelectControl> {
  let select_node = index.node(select_node_id)?;
  if !select_node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("select"))
  {
    return None;
  }

  fn collect_descendant_text_content(node: &DomNode) -> String {
    let mut text = String::new();
    let mut stack: Vec<&DomNode> = vec![node];
    while let Some(node) = stack.pop() {
      match &node.node_type {
        DomNodeType::Text { content } => text.push_str(content),
        DomNodeType::Element { tag_name, namespace, .. } => {
          if tag_name.eq_ignore_ascii_case("script")
            && (namespace.is_empty() || namespace == crate::dom::HTML_NAMESPACE || namespace == crate::dom::SVG_NAMESPACE)
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
    .unwrap_or_else(|| options[first_enabled_idx].0);

  dom_mutation::activate_select_option(dom, select_id, option_id, false)
}
impl InteractionEngine {
  pub fn new() -> Self {
    Self {
      state: InteractionState::default(),
      pointer_down_target: None,
      range_drag: None,
      number_spin: None,
      text_drag: None,
      document_drag: None,
      text_edit: None,
      text_undo: HashMap::new(),
      modality: InputModality::Pointer,
      last_click_target: None,
      last_form_submitter: None,
    }
  }

  pub fn interaction_state(&self) -> &InteractionState {
    &self.state
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

    for &id in &self.state.focus_chain {
      check_node_id("focus_chain", id);
    }
    for &id in &self.state.hover_chain {
      check_node_id("hover_chain", id);
    }
    for &id in &self.state.active_chain {
      check_node_id("active_chain", id);
    }

    if let Some(id) = self.pointer_down_target {
      check_node_id("pointer_down_target", id);
    }
    if let Some(state) = self.range_drag {
      check_node_id("range_drag", state.node_id);
    }
    if let Some(state) = self.text_drag {
      check_node_id("text_drag", state.node_id);
    }
    if let Some(id) = self.last_click_target {
      check_node_id("last_click_target", id);
    }
    if let Some(id) = self.last_form_submitter {
      check_node_id("last_form_submitter", id);
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
    self.state.user_validity.insert(node_id)
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

  fn step_number_input(&mut self, index: &mut DomIndexMut, node_id: usize, delta_steps: i32) -> bool {
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
      if let Some(edit) = self.text_edit.as_mut().filter(|edit| edit.node_id == node_id) {
        let prev = (edit.caret, edit.caret_affinity, edit.selection_anchor, edit.preferred_x);
        edit.caret = new_len;
        edit.caret_affinity = CaretAffinity::Downstream;
        edit.selection_anchor = None;
        edit.preferred_x = None;
        if (edit.caret, edit.caret_affinity, edit.selection_anchor, edit.preferred_x) != prev {
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
      if ok { trimmed } else { "" }
    };

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

  pub fn focused_node_id(&self) -> Option<usize> {
    self.state.focused
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
    self.state.text_edit = next;
    changed
  }

  /// Returns the most recent click target (pre-order DOM node id) produced by
  /// [`InteractionEngine::pointer_up_with_scroll`].
  ///
  /// This is a UI-layer hook that allows external code to dispatch higher-level click events
  /// (e.g. JavaScript DOM `"click"` listeners) using the same hit-test/label remapping semantics
  /// as the interaction engine's built-in default actions.
  pub fn take_last_click_target(&mut self) -> Option<usize> {
    self.last_click_target.take()
  }
  /// Returns the most recent form submitter (pre-order DOM node id) that produced a submission
  /// navigation request during user activation.
  ///
  /// This is an integration hook for higher-level layers (e.g. browser UI workers) that need to
  /// dispatch JS `"submit"` events and honor `event.preventDefault()` before committing the
  /// navigation.
  pub fn take_last_form_submitter(&mut self) -> Option<usize> {
    self.last_form_submitter.take()
  }
  fn set_focus(
    &mut self,
    index: &mut DomIndexMut,
    new_focused: Option<usize>,
    focus_visible: bool,
  ) -> bool {
    let prev_focused = self.state.focused;
    let prev_focus_visible = self.state.focus_visible;
    let prev_focus_chain = self.state.focus_chain.clone();
    let mut changed = false;

    // Any focus change cancels an in-progress IME composition and resets text-editing state.
    if prev_focused != new_focused {
      if self.state.ime_preedit.is_some() {
        changed = true;
      }
      self.state.ime_preedit = None;
      self.text_edit = None;
      self.text_drag = None;
      self.document_drag = None;
      // Focus changes collapse any existing document selection (e.g. a prior Ctrl+A selection).
      self.state.document_selection = None;
    }

    self.state.focused = new_focused;
    self.state.focus_visible = new_focused.is_some() && focus_visible;
    self.state.focus_chain = new_focused
      .map(|id| collect_element_chain(index, id))
      .unwrap_or_default();

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
      || prev_focus_chain != self.state.focus_chain
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
    self.modality = if focus_visible {
      InputModality::Keyboard
    } else {
      InputModality::Pointer
    };

    let prev_focus = self.state.focused;
    let mut index = DomIndexMut::new(dom);

    let node_id = node_id.filter(|&id| index.node(id).is_some_and(DomNode::is_element));
    let changed = self.set_focus(&mut index, node_id, focus_visible);

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
    self.text_drag = None;
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
    let (start, end) = if start <= end {
      (start, end)
    } else {
      (end, start)
    };
    self.text_drag = None;
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

  pub fn clear_pointer_state(&mut self, _dom: &mut DomNode) -> bool {
    let hover_changed = !self.state.hover_chain.is_empty();
    let active_changed = !self.state.active_chain.is_empty();
    self.state.hover_chain.clear();
    self.state.active_chain.clear();
    self.pointer_down_target = None;
    self.range_drag = None;
    self.number_spin = None;
    self.text_drag = None;
    self.document_drag = None;
    hover_changed | active_changed
  }

  pub fn clear_pointer_state_without_dom(&mut self) {
    self.state.hover_chain.clear();
    self.state.active_chain.clear();
    self.pointer_down_target = None;
    self.range_drag = None;
    self.number_spin = None;
    self.text_drag = None;
    self.document_drag = None;
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
    let page_point = viewport_point.translate(scroll.viewport);
    let mut index = DomIndexMut::new(dom);
    let mut dom_changed = false;
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
        let changed = update_range_value_from_pointer(
          &mut index,
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
        if let Some(edit) = self
          .text_edit
          .as_mut()
          .filter(|edit| edit.node_id == state.node_id)
        {
           if let Some((caret, affinity)) = caret_index_for_text_control_point(
             &index,
             box_tree,
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
            edit.caret = caret;
            edit.caret_affinity = affinity;
            if caret == state.anchor {
              edit.selection_anchor = None;
            } else {
              edit.selection_anchor = Some(state.anchor);
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

    if self.document_drag.is_some() {
      // Mirror the sentinel handling in other drags: when the pointer leaves the page image the UI
      // sends a negative page-point. Do not treat that as dragging the selection to the start of the
      // document; keep the last in-page selection instead.
      if page_point.x.is_finite()
        && page_point.y.is_finite()
        && page_point.x >= 0.0
        && page_point.y >= 0.0
      {
        if let Some(point) =
          document_selection_point_at_page_point(box_tree, fragment_tree, page_point)
        {
          if let Some(DocumentSelectionState::Ranges(ranges)) = self.state.document_selection.as_mut()
          {
            let before = ranges.clone();
            ranges.focus = point;
            if ranges.primary < ranges.ranges.len() {
              ranges.ranges[ranges.primary] = DocumentSelectionRange {
                start: ranges.anchor,
                end: ranges.focus,
              }
              .normalized();
            }
            ranges.normalize();
            if *ranges != before {
              dom_changed = true;
            }
          }
        }
      }
    }

    dom_changed |= self.sync_text_edit_paint_state();

    let hit = hit_test_dom(dom, box_tree, fragment_tree, page_point);
    let new_chain = hit
      .and_then(|hit| nearest_element_ancestor(&index, hit.styled_node_id))
      .map(|target| collect_element_chain_with_label_associated_controls(&index, target))
      .unwrap_or_default();

    let changed = self.state.hover_chain != new_chain;
    self.state.hover_chain = new_chain;
    dom_changed | changed
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
    let hit = hit_test_dom(dom, box_tree, fragment_tree, page_point)?;
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

  /// Like [`InteractionEngine::pointer_down`], but allows the UI layer to provide click metadata
  /// needed for browser-like text selection gestures in `<input>`/`<textarea>`.
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
    self.modality = InputModality::Pointer;

    self.range_drag = None;
    self.number_spin = None;
    self.text_drag = None;
    self.document_drag = None;
    let prev_doc_selection = self.state.document_selection.clone();

    let page_point = viewport_point.translate(scroll.viewport);

    let down_hit = hit_test_dom(dom, box_tree, fragment_tree, page_point);
    let down_target = down_hit.as_ref().map(|hit| hit.dom_node_id);
    let mut index = DomIndexMut::new(dom);
    let new_chain = down_target
      .map(|target| collect_element_chain_with_label_associated_controls(&index, target))
      .unwrap_or_default();

    let changed = self.state.active_chain != new_chain;
    self.state.active_chain = new_chain;
    self.pointer_down_target = down_target;

    let mut dom_changed = changed;
    if let Some(hit) = down_hit.as_ref() {
      if matches!(button, PointerButton::Primary) && index.node(hit.dom_node_id).is_some_and(is_range_input)
      {
        self.range_drag = Some(RangeDragState {
          node_id: hit.dom_node_id,
          box_id: hit.box_id,
        });
        let changed = update_range_value_from_pointer(
          &mut index,
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
          box_tree,
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
            return dom_changed;
          }

          let (caret, caret_affinity) = caret_index_for_text_control_point(
            &index,
            box_tree,
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

          let click_count = click_count.clamp(1, 3);
          let click_count = if click_count > 1 && focus_before != Some(hit.dom_node_id) {
            1
          } else {
            click_count
          };

          let shift_extend = modifiers.shift() && focus_before == Some(hit.dom_node_id);

          let text_edit_changed = if let Some(state) = self
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

          if text_edit_changed {
            dom_changed = true;
          }
          dom_changed |= self.sync_text_edit_paint_state();

          let drag_anchor = self
            .text_edit
            .as_ref()
            .filter(|state| state.node_id == hit.dom_node_id)
            .map(|state| state.selection_anchor.unwrap_or(state.caret))
            .unwrap_or(caret.min(current_len));
          self.text_drag = Some(TextDragState {
            node_id: hit.dom_node_id,
            box_id: hit.box_id,
            anchor: drag_anchor,
            focus_before,
          });
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
      if let Some(point) = document_selection_point_at_page_point(box_tree, fragment_tree, page_point)
      {
        // Starting a document selection drag should blur the currently-focused control when the
        // gesture begins outside that focused subtree (so subsequent keyboard input does not keep
        // editing the previous control).
        if let Some(focused) = self.state.focused {
          let click_within_focused = down_target
            .is_some_and(|target| is_ancestor_or_self(&index, focused, target));
          if !click_within_focused {
            dom_changed |= self.set_focus(&mut index, None, false);
          }
        }

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
            _ => Some(DocumentSelectionState::Ranges(DocumentSelectionRanges::collapsed(point))),
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
            _ => Some(DocumentSelectionState::Ranges(DocumentSelectionRanges::collapsed(point))),
          }
        } else {
          Some(DocumentSelectionState::Ranges(DocumentSelectionRanges::collapsed(point)))
        };

        self.state.document_selection = next;
        if let Some(DocumentSelectionState::Ranges(ranges)) = self.state.document_selection.as_ref() {
          self.document_drag = Some(DocumentDragState { anchor: ranges.anchor });
        }
      } else if !modifiers.shift() && !modifiers.command() {
        // Plain click away from selectable text clears the selection.
        self.state.document_selection = None;
      }
    }
    dom_changed |= prev_doc_selection != self.state.document_selection;
    dom_changed
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

    remap_vec(&mut self.state.hover_chain, old_index, new_ids);
    remap_vec(&mut self.state.active_chain, old_index, new_ids);
    remap_opt(&mut self.pointer_down_target, old_index, new_ids);
    remap_opt(&mut self.last_click_target, old_index, new_ids);
    remap_opt(&mut self.last_form_submitter, old_index, new_ids);
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
    remap_vec(&mut self.state.focus_chain, old_index, new_ids);

    // Remap visited links.
    if !self.state.visited_links.is_empty() {
      let mut remapped = rustc_hash::FxHashSet::default();
      remapped.reserve(self.state.visited_links.len());
      for old in self.state.visited_links.iter().copied() {
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
      self.state.visited_links = remapped;
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

    if let Some(state) = &mut self.document_drag {
      let ptr = old_index
        .id_to_node
        .get(state.anchor.node_id)
        .copied()
        .unwrap_or(std::ptr::null_mut());
      if ptr.is_null() {
        self.document_drag = None;
      } else if let Some(&new_id) = new_ids.get(&(ptr as *const DomNode)) {
        state.anchor.node_id = new_id;
      } else {
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
    }

    // Ensure the paint-only caret/selection state stays in sync with the remapped internal edit
    // state.
    let _ = self.sync_text_edit_paint_state();
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
    document_url: &str,
    base_url: &str,
  ) -> (bool, InteractionAction) {
    self.last_click_target = None;
    self.last_form_submitter = None;

    let range_drag = self.range_drag.take();
    let number_spin = self.number_spin.take();
    let text_drag = self.text_drag.take();
    let document_drag = self.document_drag.take();
    let suppress_click = document_drag.is_some()
      && self
        .state
        .document_selection
        .as_ref()
        .is_some_and(|sel| sel.has_highlight());
    let prev_focus = text_drag
      .as_ref()
      .map(|state| state.focus_before)
      .unwrap_or(self.state.focused);

    let page_point = viewport_point.translate(scroll.viewport);

    let up_hit = hit_test_dom(dom, box_tree, fragment_tree, page_point);
    let up_semantic = up_hit.as_ref().map(|hit| hit.dom_node_id);
    let mut index = DomIndexMut::new(dom);

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
        let changed = update_range_value_from_pointer(
          &mut index,
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
    let active_changed = !self.state.active_chain.is_empty();
    self.state.active_chain.clear();
    self.pointer_down_target = None;
    dom_changed |= active_changed;

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

    let mut click_target = if click_qualifies { down_semantic } else { None };
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
              let changed = apply_select_listbox_click(
                dom,
                fragment_tree,
                page_point,
                target_id,
                *select_box_id,
                scroll,
                control,
                style,
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
        } else {
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
                if self.state.visited_links.insert(target_id) {
                  dom_changed = true;
                }

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
                    box_tree,
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
                let changed = dom_mutation::activate_radio(dom, target_id);
                dom_changed |= changed;
                if changed {
                  dom_changed |= self.mark_user_validity(target_id);
                }
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
            } else if index.node(target_id).is_some_and(is_submit_control) {
              if node_is_disabled(&index, target_id) {
                // Disabled submit controls do not submit.
              } else {
                // A form submission attempt flips HTML "user validity" so `:user-invalid` matches.
                dom_changed |= self.mark_user_validity(target_id);
                dom_changed |= self.mark_form_user_validity(&index, target_id);
                if let Some(submission) = form_submission(dom, target_id, document_url, base_url) {
                  self.last_form_submitter = Some(target_id);
                  match submission.method {
                    FormSubmissionMethod::Get => {
                      action = InteractionAction::Navigate {
                        href: submission.url,
                      };
                    }
                    FormSubmissionMethod::Post => {
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

    // `OpenSelectDropdown` includes the focus update; do not replace it with `FocusChanged`.
    if matches!(action, InteractionAction::None) && self.state.focused != prev_focus {
      action = InteractionAction::FocusChanged {
        node_id: self.state.focused,
      };
    }

    (dom_changed, action)
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
      document_url,
      base_url,
    )
  }

  /// Insert typed text into focused text control (input/textarea) and set focus-visible.
  pub fn text_input(&mut self, dom: &mut DomNode, text: &str) -> bool {
    self.modality = InputModality::Keyboard;
    let Some(focused) = self.state.focused else {
      return false;
    };

    let mut index = DomIndexMut::new(dom);

    // Ensure focus-visible when the keyboard is used.
    let mut changed = self.set_focus(&mut index, Some(focused), true);

    let focused_is_text_input = index.node(focused).is_some_and(is_text_input);
    let focused_is_textarea = index.node(focused).is_some_and(is_textarea);
    if !(focused_is_text_input || focused_is_textarea) {
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

    let mut next = String::with_capacity(
      current
        .len()
        .saturating_sub(end_byte.saturating_sub(start_byte))
        .saturating_add(text.len()),
    );
    next.push_str(&current[..start_byte]);
    next.push_str(text);
    next.push_str(&current[end_byte..]);

    let next_len = next.chars().count();
    let inserted_chars = text.chars().count();
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
    self.state.ime_preedit = None;
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

    // Update internal state.
    match self.state.ime_preedit.as_mut() {
      Some(existing) if existing.node_id == focused => {
        if existing.text != text || existing.cursor != cursor {
          existing.text.clear();
          existing.text.push_str(text);
          existing.cursor = cursor;
          changed = true;
        }
      }
      _ => {
        self.state.ime_preedit = Some(ImePreeditState {
          node_id: focused,
          text: text.to_string(),
          cursor,
        });
        changed = true;
      }
    }

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
              self.state.document_selection = None;
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
    self.state.document_selection = Some(DocumentSelectionState::All);
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
          let part = serialize_document_selection(
            box_tree,
            fragment_tree,
            DocumentSelection::Range(*range),
          );
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
    let Some(focused) = self.state.focused else {
      return (false, None);
    };

    let mut index = DomIndexMut::new(dom);
    // Ensure focus-visible when the keyboard is used.
    let mut dom_changed = self.set_focus(&mut index, Some(focused), true);

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
    if matches!(key, KeyAction::Tab | KeyAction::ShiftTab) {
      // Focus traversal (wraps at ends).
      let mut index = DomIndexMut::new(dom);
      let focusables = collect_tab_stops(&index);
      let next_focus = match key {
        KeyAction::Tab => next_tab_focus(self.state.focused, &focusables),
        KeyAction::ShiftTab => prev_tab_focus(self.state.focused, &focusables),
        _ => None,
      };
      let Some(next_focus) = next_focus else {
        return false;
      };
      return self.set_focus(&mut index, Some(next_focus), true);
    }

    let Some(focused) = self.state.focused else {
      return false;
    };

    let mut index = DomIndexMut::new(dom);
    let mut changed = false;

    // Ensure focus-visible when the keyboard is used.
    changed |= self.set_focus(&mut index, Some(focused), true);

    let focused_is_text_input = index.node(focused).is_some_and(is_text_input);
    let focused_is_textarea = index.node(focused).is_some_and(is_textarea);

    if focused_is_text_input || focused_is_textarea {
      if node_or_ancestor_is_inert(&index, focused) || node_is_disabled(&index, focused) {
        return changed;
      }

      let can_edit_value = !node_is_readonly(&index, focused);

      // `<input type=number>` uses ArrowUp/ArrowDown to increment/decrement (like browsers).
      if focused_is_text_input
        && matches!(key, KeyAction::ArrowUp | KeyAction::ArrowDown)
        && index
          .node(focused)
          .is_some_and(|node| input_type(node).eq_ignore_ascii_case("number"))
      {
        if !can_edit_value {
          return changed;
        }
        let delta_steps = if matches!(key, KeyAction::ArrowUp) { 1 } else { -1 };
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
        KeyAction::WordLeft | KeyAction::WordRight => {
          let move_left = matches!(key, KeyAction::WordLeft);
          if let Some((start, end)) = edit.selection() {
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
                false,
              );
            } else {
              edit.set_caret_and_maybe_extend_selection(next, false);
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
            let runs = shape_text_runs_for_interaction(line_text, shape_style).unwrap_or_default();
            let total_advance = shaped_total_advance(&runs, fallback_advance);
            let stops = crate::text::caret::caret_stops_for_runs(line_text, &runs, total_advance);

            if let Some((start, end)) = selection.filter(|_| !extend_selection) {
              // Collapse selection without shift.
              if start >= line_start && end <= line_end {
                let start_in_line = start.saturating_sub(line_start).min(line_len);
                let end_in_line = end.saturating_sub(line_start).min(line_len);

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
                edit
                  .set_caret_and_maybe_extend_selection(if move_left { start } else { end }, false);
              }
            } else if let Some(cur_idx) =
              crate::text::caret::caret_stop_index(&stops, caret_in_line, edit.caret_affinity)
            {
              // Move caret within the current line, falling back to crossing a newline when there is
              // no further visual stop in the requested direction.
              let next_idx = if move_left {
                cur_idx.saturating_sub(1)
              } else {
                (cur_idx + 1).min(stops.len().saturating_sub(1))
              };
              if next_idx != cur_idx {
                let stop = stops.get(next_idx).copied().unwrap_or(stops[cur_idx]);
                edit.set_caret_with_affinity_and_maybe_extend_selection(
                  line_start.saturating_add(stop.char_idx).min(total_chars),
                  stop.affinity,
                  extend_selection,
                );
              } else {
                let next = if move_left {
                  caret.saturating_sub(1)
                } else {
                  (caret + 1).min(total_chars)
                };
                edit.set_caret_and_maybe_extend_selection(next, extend_selection);
              }
            } else {
              let next = if move_left {
                caret.saturating_sub(1)
              } else {
                (caret + 1).min(total_chars)
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
            let runs = shape_text_runs_for_interaction(text, shape_style).unwrap_or_default();
            let total_advance = shaped_total_advance(&runs, fallback_advance);
            let stops = crate::text::caret::caret_stops_for_runs(text, &runs, total_advance);

            if let Some((start, end)) = selection.filter(|_| !extend_selection) {
              let start_pos = crate::text::caret::caret_stop_index(
                &stops,
                start.min(current_len),
                CaretAffinity::Downstream,
              );
              let end_pos = crate::text::caret::caret_stop_index(
                &stops,
                end.min(current_len),
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
                cur_idx.saturating_sub(1)
              } else {
                (cur_idx + 1).min(stops.len().saturating_sub(1))
              };
              let stop = stops.get(next_idx).copied().unwrap_or(stops[cur_idx]);
              edit.set_caret_with_affinity_and_maybe_extend_selection(
                stop.char_idx.min(current_len),
                stop.affinity,
                extend_selection,
              );
            } else {
              let next_caret = if move_left {
                edit.caret.saturating_sub(1)
              } else {
                (edit.caret + 1).min(current_len)
              };
              edit.set_caret_and_maybe_extend_selection(next_caret, extend_selection);
            }
          }
        }
        KeyAction::Home | KeyAction::End => {
          let next = if matches!(key, KeyAction::Home) {
            0usize
          } else {
            current_len
          };
          edit.set_caret_and_maybe_extend_selection(next, false);
        }
        KeyAction::ShiftHome | KeyAction::ShiftEnd => {
          let next = if matches!(key, KeyAction::ShiftHome) {
            0usize
          } else {
            current_len
          };
          edit.set_caret_and_maybe_extend_selection(next, true);
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
        KeyAction::ArrowUp | KeyAction::ArrowDown => {
          if let Some((start, end)) = edit.selection() {
            // Like ArrowLeft/Right, ArrowUp/Down should collapse an active selection to the
            // boundary in the direction of travel before attempting any further movement.
            let (next, next_affinity) = if matches!(key, KeyAction::ArrowUp) {
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
            let mut moved = false;

            if let (Some(box_tree), Some(fragment_tree)) = (box_tree, fragment_tree) {
              if let Some((textarea_box_id, style)) =
                textarea_control_snapshot_from_box_tree(box_tree, focused)
              {
                if let Some(border_rect) = fragment_rect_for_box_id(fragment_tree, textarea_box_id) {
                  let style = style.as_ref();
                  let viewport_size = fragment_tree.viewport_size();
                  let content_rect = content_rect_for_border_rect(border_rect, style, viewport_size);
                  let rect = inset_rect_uniform(content_rect, 2.0);

                  if rect.width() > 0.0 && rect.height() > 0.0 {
                    let metrics = if matches!(
                      style.line_height,
                      crate::style::types::LineHeight::Normal
                    ) {
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
                      let chars_per_line = crate::textarea::textarea_chars_per_line(style, rect.width());
                      let layout = crate::textarea::build_textarea_visual_lines(&current, chars_per_line);

                      let line_idx =
                        crate::textarea::textarea_visual_line_index_for_caret(&current, &layout, caret);

                      let target_idx = match key {
                        KeyAction::ArrowUp => line_idx.checked_sub(1),
                        KeyAction::ArrowDown => Some(line_idx.saturating_add(1)),
                        _ => None,
                      }
                      .filter(|idx| *idx < layout.lines.len());

                      if let Some(target_idx) = target_idx {
                        let line_rect = Rect::from_xywh(rect.x(), rect.y(), rect.width(), line_height);

                        // Maintain a preferred x position across vertical moves (like browsers).
                        let preferred_x = if let Some(px) = edit.preferred_x {
                          px
                        } else {
                          let cur_line =
                            layout.lines.get(line_idx).copied().unwrap_or(layout.lines[0]);
                          let cur_text = cur_line.text(&current);
                          let caret_in_line =
                            caret.saturating_sub(cur_line.start_char).min(cur_line.len_chars());

                          let fallback_advance = fallback_text_advance(cur_text, style);
                          let runs = shape_text_runs_for_interaction(cur_text, style).unwrap_or_default();
                          let total_advance = shaped_total_advance(&runs, fallback_advance);
                          let start_x = aligned_text_start_x(style, line_rect, total_advance);
                          let stops =
                            crate::text::caret::caret_stops_for_runs(cur_text, &runs, total_advance);
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

                        let target_line = layout.lines.get(target_idx).copied().unwrap_or(layout.lines[0]);
                        let target_text = target_line.text(&current);

                        let x = rect.x() + preferred_x;
                        let x = if x.is_finite() { x } else { rect.x() };
                        let (caret_in_line, affinity) =
                          caret_position_for_x_in_text(target_text, style, line_rect, x);

                        edit.caret =
                          target_line.start_char.saturating_add(caret_in_line).min(total_chars);
                        edit.caret_affinity = affinity;
                        edit.selection_anchor = None;
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
                .and_then(|x| (x / char_advance).is_finite().then_some((x / char_advance).round() as usize))
                .unwrap_or(col);

              let target_line = match key {
                KeyAction::ArrowUp => line_idx.checked_sub(1),
                KeyAction::ArrowDown => Some(line_idx + 1),
                _ => None,
              }
              .filter(|&idx| idx < line_starts.len());

              if let Some(target_idx) = target_line {
                let target_start = line_starts[target_idx];
                let target_end = if let Some(next_start) = line_starts.get(target_idx + 1) {
                  next_start.saturating_sub(1)
                } else {
                  total_chars
                };
                let target_len = target_end.saturating_sub(target_start);
                edit.caret = target_start + preferred_col.min(target_len);
                edit.caret_affinity = CaretAffinity::Downstream;
                edit.selection_anchor = None;
                edit.preferred_x = Some(preferred_col as f32 * char_advance);
              }
            }
          }
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
    match key {
      KeyAction::ArrowUp
      | KeyAction::ArrowDown
      | KeyAction::ArrowLeft
      | KeyAction::ArrowRight
      | KeyAction::Home
      | KeyAction::End => {
        if index.node(focused).is_some_and(is_range_input) {
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
          if let Some(node_mut) = index.node_mut(focused) {
            let dom_changed = match key {
              KeyAction::ArrowUp | KeyAction::ArrowRight => dom_mutation::step_range_value(node_mut, 1),
              KeyAction::ArrowDown | KeyAction::ArrowLeft => dom_mutation::step_range_value(node_mut, -1),
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
        ) && index.node(focused).is_some_and(is_select)
          && !is_disabled_or_inert(&index, focused)
        {
          if matches!(key, KeyAction::Home | KeyAction::End)
            && index
              .node(focused)
              .is_some_and(|node| node.get_attribute_ref("multiple").is_some())
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

          let option_node_id = options[next_idx].0;
          changed |= self.activate_select_option(dom, focused, option_node_id, false);
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
      | KeyAction::ShiftArrowLeft
      | KeyAction::ShiftArrowRight
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
      KeyAction::ArrowUp | KeyAction::ArrowDown | KeyAction::Home | KeyAction::End => {
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
            if self.state.visited_links.insert(focused) {
              changed = true;
            }
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
            let dom_changed = dom_mutation::activate_radio(dom, focused);
            changed |= dom_changed;
            if dom_changed {
              changed |= self.mark_user_validity(focused);
            }
          }
        } else if index.node(focused).is_some_and(is_submit_control) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled submit controls do not submit.
          } else {
            // A form submission attempt flips HTML "user validity" so `:user-invalid` matches.
            changed |= self.mark_user_validity(focused);
            changed |= self.mark_form_user_validity(&index, focused);
            if let Some(submission) = form_submission(dom, focused, document_url, base_url) {
              self.last_form_submitter = Some(focused);
              match submission.method {
                FormSubmissionMethod::Get => {
                  action = InteractionAction::Navigate {
                    href: submission.url,
                  };
                }
                FormSubmissionMethod::Post => {
                  action = InteractionAction::NavigateRequest {
                    request: submission,
                  };
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
              let submission = match submitter_id {
                Some(submitter_id) => form_submission(dom, submitter_id, document_url, base_url),
                None => form_submission_without_submitter(dom, form_id, document_url, base_url),
              };
              if let Some(submission) = submission {
                if let Some(submitter_id) = submitter_id {
                  self.last_form_submitter = Some(submitter_id);
                }
                match submission.method {
                  FormSubmissionMethod::Get => {
                    action = InteractionAction::Navigate {
                      href: submission.url,
                    };
                  }
                  FormSubmissionMethod::Post => {
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
      KeyAction::Space | KeyAction::ShiftSpace => {
        if node_or_ancestor_is_inert(&index, focused) {
          // Inert subtrees are not interactive.
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
            let dom_changed = dom_mutation::activate_radio(dom, focused);
            changed |= dom_changed;
            if dom_changed {
              changed |= self.mark_user_validity(focused);
            }
          }
        } else if index.node(focused).is_some_and(is_submit_control) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled submit controls do not submit.
          } else {
            changed |= self.mark_user_validity(focused);
            changed |= self.mark_form_user_validity(&index, focused);
            if let Some(submission) = form_submission(dom, focused, document_url, base_url) {
              self.last_form_submitter = Some(focused);
              match submission.method {
                FormSubmissionMethod::Get => {
                  action = InteractionAction::Navigate {
                    href: submission.url,
                  };
                }
                FormSubmissionMethod::Post => {
                  action = InteractionAction::NavigateRequest {
                    request: submission,
                  };
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
      | KeyAction::ShiftArrowLeft
      | KeyAction::ShiftArrowRight
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
      KeyAction::ArrowUp | KeyAction::ArrowDown | KeyAction::Home | KeyAction::End => {
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
            if self.state.visited_links.insert(focused) {
              changed = true;
            }
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
            let dom_changed = dom_mutation::activate_radio(dom, focused);
            changed |= dom_changed;
            if dom_changed {
              changed |= self.mark_user_validity(focused);
            }
          }
        } else if index.node(focused).is_some_and(is_submit_control) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled submit controls do not submit.
          } else {
            // A form submission attempt flips HTML "user validity" so `:user-invalid` matches.
            changed |= self.mark_user_validity(focused);
            changed |= self.mark_form_user_validity(&index, focused);
            if let Some(submission) = form_submission(dom, focused, document_url, base_url) {
              self.last_form_submitter = Some(focused);
              match submission.method {
                FormSubmissionMethod::Get => {
                  action = InteractionAction::Navigate {
                    href: submission.url,
                  };
                }
                FormSubmissionMethod::Post => {
                  action = InteractionAction::NavigateRequest { request: submission };
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
              let submission = match submitter_id {
                Some(submitter_id) => form_submission(dom, submitter_id, document_url, base_url),
                None => form_submission_without_submitter(dom, form_id, document_url, base_url),
              };
              if let Some(submission) = submission {
                if let Some(submitter_id) = submitter_id {
                  self.last_form_submitter = Some(submitter_id);
                }
                match submission.method {
                  FormSubmissionMethod::Get => {
                    action = InteractionAction::Navigate {
                      href: submission.url,
                    };
                  }
                  FormSubmissionMethod::Post => {
                    action = InteractionAction::NavigateRequest { request: submission };
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
            let dom_changed = dom_mutation::activate_radio(dom, focused);
            changed |= dom_changed;
            if dom_changed {
              changed |= self.mark_user_validity(focused);
            }
          }
        } else if index.node(focused).is_some_and(is_submit_control) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled submit controls do not submit.
          } else {
            changed |= self.mark_user_validity(focused);
            changed |= self.mark_form_user_validity(&index, focused);
            if let Some(submission) = form_submission(dom, focused, document_url, base_url) {
              self.last_form_submitter = Some(focused);
              match submission.method {
                FormSubmissionMethod::Get => {
                  action = InteractionAction::Navigate {
                    href: submission.url,
                  };
                }
                FormSubmissionMethod::Post => {
                  action = InteractionAction::NavigateRequest { request: submission };
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
mod fuzz_tests;
