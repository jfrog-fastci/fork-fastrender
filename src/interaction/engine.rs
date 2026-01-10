use crate::dom::DomNode;
use crate::dom::DomNodeType;
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::layout::contexts::inline::baseline::compute_line_height_with_metrics_viewport;
use crate::layout::contexts::inline::line_builder::TextItem;
use crate::scroll::ScrollState;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use crate::tree::box_tree::BoxTree;
use crate::tree::box_tree::BoxType;
use crate::tree::box_tree::FormControlKind;
use crate::tree::box_tree::ReplacedType;
use crate::tree::box_tree::SelectControl;
use crate::tree::box_tree::SelectItem;
use crate::tree::fragment_tree::FragmentTree;
use crate::ui::messages::{PointerButton, PointerModifiers};
use std::collections::HashMap;
use std::sync::Arc;

use super::dom_mutation;
use super::fragment_geometry::content_rect_for_border_rect;
use super::form_submit::{form_submission, FormSubmission, FormSubmissionMethod};
use super::hit_test::hit_test_dom;
use super::image_maps;
use super::resolve_url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputModality {
  Pointer,
  Keyboard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionAction {
  None,
  Navigate { href: String },
  OpenInNewTab { href: String },
  /// Navigation that carries an explicit HTTP method and optional body (used for form POST).
  NavigateRequest { request: FormSubmission },
  FocusChanged { node_id: Option<usize> },
  OpenSelectDropdown {
    select_node_id: usize,
    control: crate::tree::box_tree::SelectControl,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
  Backspace,
  Delete,
  Enter,
  Tab,
  ShiftTab,
  Space,
  ArrowLeft,
  ArrowRight,
  ShiftArrowLeft,
  ShiftArrowRight,
  ArrowUp,
  ArrowDown,
  Home,
  End,
  ShiftHome,
  ShiftEnd,
  SelectAll,
}

#[derive(Debug, Clone)]
pub struct InteractionEngine {
  hover_chain: Vec<usize>,
  active_chain: Vec<usize>,
  pointer_down_target: Option<usize>,
  range_drag: Option<RangeDragState>,
  text_drag: Option<TextDragState>,
  focused: Option<usize>,
  ime_composition: Option<ImeCompositionState>,
  text_edit: Option<TextEditState>,
  modality: InputModality,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RangeDragState {
  node_id: usize,
  box_id: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImeCompositionState {
  node_id: usize,
  text: String,
  cursor: Option<(usize, usize)>,
}

const IME_PREEDIT_ATTR: &str = "data-fastr-ime-preedit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextEditState {
  /// The pre-order DOM id of the focused `<input>`/`<textarea>`.
  node_id: usize,
  /// Insertion point in character indices (not bytes).
  caret: usize,
  /// Anchor for selection extension. When present and differs from `caret`, the control has an
  /// active selection.
  selection_anchor: Option<usize>,
  /// Preferred column (character offset within the current line) for textarea vertical movement.
  preferred_column: Option<usize>,
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
    self.preferred_column = None;
  }

  fn set_caret_and_maybe_extend_selection(&mut self, caret: usize, extend_selection: bool) {
    if extend_selection {
      if self.selection_anchor.is_none() {
        self.selection_anchor = Some(self.caret);
      }
      self.caret = caret;
    } else {
      self.selection_anchor = None;
      self.caret = caret;
    }
    self.preferred_column = None;
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

  fn has_preedit_attr(dom: &mut DomNode, node_id: usize) -> bool {
    let index = DomIndexMut::new(dom);
    index
      .node(node_id)
      .and_then(|node| node.get_attribute_ref(IME_PREEDIT_ATTR))
      .is_some()
  }

  fn set_text_selection_caret(engine: &mut InteractionEngine, dom: &mut DomNode, node_id: usize, caret: usize) {
    engine.text_edit = Some(TextEditState {
      node_id,
      caret,
      selection_anchor: None,
      preferred_column: None,
    });
    let mut index = DomIndexMut::new(dom);
    write_text_edit_data_attrs(&mut index, node_id, caret, None);
  }

  fn set_text_selection_range(
    engine: &mut InteractionEngine,
    dom: &mut DomNode,
    node_id: usize,
    start: usize,
    end: usize,
  ) {
    engine.text_edit = Some(TextEditState {
      node_id,
      caret: end,
      selection_anchor: Some(start),
      preferred_column: None,
    });
    let mut index = DomIndexMut::new(dom);
    write_text_edit_data_attrs(&mut index, node_id, end, Some((start, end)));
  }

  #[test]
  fn ime_preedit_sets_composition_without_mutating_value() {
    let mut dom = crate::dom::parse_html("<html><body><input value=\"a\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    engine.ime_preedit(&mut dom, "あ", Some((0, 1)));

    let comp = engine.ime_composition.as_ref().expect("composition state");
    assert_eq!(comp.node_id, input_id);
    assert_eq!(comp.text, "あ");
    assert_eq!(comp.cursor, Some((0, 1)));

    assert_eq!(input_value(&mut dom, input_id), "a");
    assert!(has_preedit_attr(&mut dom, input_id));
  }

  #[test]
  fn ime_commit_inserts_text_and_clears_preedit() {
    let mut dom = crate::dom::parse_html("<html><body><input value=\"a\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    engine.ime_preedit(&mut dom, "あ", Some((0, 1)));
    assert!(has_preedit_attr(&mut dom, input_id));

    engine.ime_commit(&mut dom, "あ");

    assert!(engine.ime_composition.is_none());
    assert!(!has_preedit_attr(&mut dom, input_id));
    assert_eq!(input_value(&mut dom, input_id), "aあ");
  }

  #[test]
  fn ime_cancel_clears_preedit_without_mutating_value() {
    let mut dom = crate::dom::parse_html("<html><body><input value=\"a\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    engine.ime_preedit(&mut dom, "あ", Some((0, 1)));
    assert!(has_preedit_attr(&mut dom, input_id));

    engine.ime_cancel(&mut dom);

    assert!(engine.ime_composition.is_none());
    assert!(!has_preedit_attr(&mut dom, input_id));
    assert_eq!(input_value(&mut dom, input_id), "a");
  }

  #[test]
  fn ime_commit_updates_textarea_value() {
    let mut dom = crate::dom::parse_html("<html><body><textarea>hi</textarea></body></html>").expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(has_preedit_attr(&mut dom, textarea_id));

    engine.ime_commit(&mut dom, "あ");

    assert!(engine.ime_composition.is_none());
    assert!(!has_preedit_attr(&mut dom, textarea_id));
    assert_eq!(textarea_value(&mut dom, textarea_id), "hiあ");
  }

  #[test]
  fn clipboard_paste_cancels_ime_preedit_for_input() {
    let mut dom = crate::dom::parse_html("<html><body><input value=\"hello\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.ime_composition.is_some());
    assert!(has_preedit_attr(&mut dom, input_id));

    assert!(engine.clipboard_paste(&mut dom, "X"));
    assert!(engine.ime_composition.is_none());
    assert!(!has_preedit_attr(&mut dom, input_id));
    assert_eq!(input_value(&mut dom, input_id), "helloX");
  }

  #[test]
  fn clipboard_cut_cancels_ime_preedit_for_input() {
    let mut dom = crate::dom::parse_html("<html><body><input value=\"hello\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&mut dom, "input");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(input_id), true);
    engine.clipboard_select_all(&mut dom);

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.ime_composition.is_some());
    assert!(has_preedit_attr(&mut dom, input_id));

    let (changed, text) = engine.clipboard_cut(&mut dom);
    assert!(changed);
    assert_eq!(text.as_deref(), Some("hello"));
    assert!(engine.ime_composition.is_none());
    assert!(!has_preedit_attr(&mut dom, input_id));
    assert_eq!(input_value(&mut dom, input_id), "");
  }

  #[test]
  fn clipboard_paste_cancels_ime_preedit_for_textarea() {
    let mut dom = crate::dom::parse_html("<html><body><textarea>hello</textarea></body></html>").expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.ime_composition.is_some());
    assert!(has_preedit_attr(&mut dom, textarea_id));

    assert!(engine.clipboard_paste(&mut dom, "X"));
    assert!(engine.ime_composition.is_none());
    assert!(!has_preedit_attr(&mut dom, textarea_id));
    assert_eq!(textarea_value(&mut dom, textarea_id), "helloX");
  }

  #[test]
  fn clipboard_cut_cancels_ime_preedit_for_textarea() {
    let mut dom =
      crate::dom::parse_html("<html><body><textarea>hello</textarea></body></html>").expect("parse");
    let textarea_id = find_element_node_id(&mut dom, "textarea");

    let mut engine = InteractionEngine::new();
    engine.focus_node_id(&mut dom, Some(textarea_id), true);
    engine.clipboard_select_all(&mut dom);

    engine.ime_preedit(&mut dom, "あ", None);
    assert!(engine.ime_composition.is_some());
    assert!(has_preedit_attr(&mut dom, textarea_id));

    let (changed, text) = engine.clipboard_cut(&mut dom);
    assert!(changed);
    assert_eq!(text.as_deref(), Some("hello"));
    assert!(engine.ime_composition.is_none());
    assert!(!has_preedit_attr(&mut dom, textarea_id));
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
    let mut dom =
      crate::dom::parse_html("<html><body><textarea>hello</textarea></body></html>").expect("parse");
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
  let Some(idx) = attrs.iter().position(|(k, _)| k.eq_ignore_ascii_case(name)) else {
    return false;
  };
  attrs.remove(idx);
  true
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

fn set_data_flag(index: &mut DomIndexMut, node_id: usize, name: &str, on: bool) -> bool {
  let Some(node) = index.node_mut(node_id) else {
    return false;
  };
  if on {
    set_node_attr(node, name, "true")
  } else {
    remove_node_attr(node, name)
  }
}

fn clear_text_edit_data_attrs(index: &mut DomIndexMut, node_id: usize) -> bool {
  let Some(node) = index.node_mut(node_id) else {
    return false;
  };
  let mut changed = false;
  changed |= remove_node_attr(node, "data-fastr-caret");
  changed |= remove_node_attr(node, "data-fastr-selection-start");
  changed |= remove_node_attr(node, "data-fastr-selection-end");
  changed
}

fn write_text_edit_data_attrs(
  index: &mut DomIndexMut,
  node_id: usize,
  caret: usize,
  selection: Option<(usize, usize)>,
) -> bool {
  let Some(node) = index.node_mut(node_id) else {
    return false;
  };
  let mut changed = false;
  changed |= set_node_attr(node, "data-fastr-caret", &caret.to_string());
  match selection {
    Some((start, end)) if start != end => {
      changed |= set_node_attr(node, "data-fastr-selection-start", &start.to_string());
      changed |= set_node_attr(node, "data-fastr-selection-end", &end.to_string());
    }
    _ => {
      changed |= remove_node_attr(node, "data-fastr-selection-start");
      changed |= remove_node_attr(node, "data-fastr-selection-end");
    }
  }
  changed
}

fn diff_flag_chain(
  index: &mut DomIndexMut,
  attr: &str,
  old_chain: &[usize],
  new_chain: &[usize],
) -> bool {
  let mut changed = false;
  for id in old_chain.iter().copied() {
    if !new_chain.contains(&id) {
      changed |= set_data_flag(index, id, attr, false);
    }
  }
  for id in new_chain.iter().copied() {
    if !old_chain.contains(&id) {
      changed |= set_data_flag(index, id, attr, true);
    }
  }
  changed
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

fn is_submit_input(node: &DomNode) -> bool {
  is_input(node) && input_type(node).eq_ignore_ascii_case("submit")
}

fn is_submit_button(node: &DomNode) -> bool {
  is_button(node) && button_type(node).eq_ignore_ascii_case("submit")
}

fn is_submit_control(node: &DomNode) -> bool {
  is_submit_input(node) || is_submit_button(node)
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

fn is_disabled_or_inert(index: &DomIndexMut, mut node_id: usize) -> bool {
  while node_id != 0 {
    let Some(node) = index.node(node_id) else {
      return false;
    };

    if node.get_attribute_ref("disabled").is_some() {
      return true;
    }
    if node_is_inert_like(node) {
      return true;
    }

    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }

  false
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
  // Template contents are always inert and should never be interactable.
  if node.template_contents_are_inert() {
    return true;
  }
  if node.get_attribute_ref("inert").is_some() {
    return true;
  }
  node
    .get_attribute_ref("data-fastr-inert")
    .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn node_or_ancestor_is_inert(index: &DomIndexMut, mut node_id: usize) -> bool {
  while node_id != 0 {
    let Some(node) = index.node(node_id) else {
      return false;
    };
    if node.is_element() && node_is_inert_like(node) {
      return true;
    }
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  false
}

fn node_self_is_tab_inert(node: &DomNode) -> bool {
  // `<template>` contents are inert and should not be reachable via Tab.
  node_is_inert_like(node)
    // MVP: treat `disabled` as making the subtree unreachable via Tab, matching our other
    // interaction pruning which skips disabled ancestors.
    || node.get_attribute_ref("disabled").is_some()
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
  // Stack-safe derived state: `inert[id]` is true if this node is in an inert subtree, driven by
  // `inert`/`data-fastr-inert=true` or a `<template>` ancestor.
  let mut inert = vec![false; index.id_to_node.len()];
  for node_id in 1..index.id_to_node.len() {
    let parent_id = index.parent.get(node_id).copied().unwrap_or(0);
    let parent_inert = inert.get(parent_id).copied().unwrap_or(false);
    let self_inert = index.node(node_id).is_some_and(node_self_is_tab_inert);
    inert[node_id] = parent_inert || self_inert;
  }
  inert
}

fn is_tab_focusable(index: &DomIndexMut, inert: &[bool], node_id: usize) -> bool {
  if inert.get(node_id).copied().unwrap_or(true) {
    return false;
  }
  let Some(node) = index.node(node_id) else {
    return false;
  };
  if !node.is_element() {
    return false;
  }
  if node.get_attribute_ref("disabled").is_some() {
    return false;
  }
  if is_input(node) && input_type(node).eq_ignore_ascii_case("hidden") {
    return false;
  }

  // MVP tabindex support:
  // - `tabindex < 0` => skipped
  // - `tabindex >= 0` => focusable (but we intentionally *ignore* the positive ordering rules and
  //   keep DOM tree order).
  // - parse failure => ignored (treated as unset)
  if let Some(tabindex) = parse_tabindex(node) {
    return tabindex >= 0;
  }

  is_focusable_anchor(node)
    || is_input(node)
    || is_textarea(node)
    || is_select(node)
    || is_button(node)
}

fn collect_tab_focusables(index: &DomIndexMut) -> Vec<usize> {
  let inert = collect_inert_subtree_flags(index);
  let mut focusables = Vec::new();
  for node_id in 1..index.id_to_node.len() {
    if is_tab_focusable(index, &inert, node_id) {
      focusables.push(node_id);
    }
  }
  focusables
}

fn next_tab_focus(current: Option<usize>, focusables: &[usize]) -> Option<usize> {
  if focusables.is_empty() {
    return None;
  }
  let Some(current) = current else {
    return Some(focusables[0]);
  };
  // `focusables` is in DOM pre-order (increasing node id). Find the first focusable element after
  // the current focused node. If none exists, wrap to the first.
  focusables
    .iter()
    .copied()
    .find(|id| *id > current)
    .or_else(|| focusables.first().copied())
}

fn prev_tab_focus(current: Option<usize>, focusables: &[usize]) -> Option<usize> {
  if focusables.is_empty() {
    return None;
  }
  let Some(current) = current else {
    return focusables.last().copied();
  };
  // `focusables` is in DOM pre-order (increasing node id). Find the last focusable element before
  // the current focused node. If none exists, wrap to the last.
  focusables
    .iter()
    .copied()
    .rev()
    .find(|id| *id < current)
    .or_else(|| focusables.last().copied())
}

fn node_is_disabled(index: &DomIndexMut, node_id: usize) -> bool {
  index
    .node(node_id)
    .and_then(|node| node.get_attribute_ref("disabled"))
    .is_some()
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

fn inset_rect_uniform(rect: Rect, inset: f32) -> Rect {
  Rect::from_xywh(
    rect.x() + inset,
    rect.y() + inset,
    (rect.width() - inset * 2.0).max(0.0),
    (rect.height() - inset * 2.0).max(0.0),
  )
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

fn shaped_prefix_advance_for_byte(
  runs: &[crate::text::pipeline::ShapedRun],
  target_byte: usize,
) -> f32 {
  let mut x = 0.0f32;
  for run in runs {
    if target_byte <= run.start {
      break;
    }
    if target_byte >= run.end {
      x += run.advance;
      continue;
    }

    let local_byte = target_byte.saturating_sub(run.start);
    if run.direction.is_rtl() {
      // TODO: Proper bidi caret mapping. For now, fall back to clamping to either edge of the run.
      x += if local_byte == 0 { 0.0 } else { run.advance };
      break;
    }

    for glyph in &run.glyphs {
      if (glyph.cluster as usize) >= local_byte {
        break;
      }
      x += glyph.x_advance;
    }
    break;
  }

  if x.is_finite() {
    x.max(0.0)
  } else {
    0.0
  }
}

fn shaped_prefix_advance_for_char_idx(
  text: &str,
  runs: &[crate::text::pipeline::ShapedRun],
  char_idx: usize,
  total_advance: f32,
  fallback_advance: f32,
) -> f32 {
  if char_idx == 0 {
    return 0.0;
  }
  let max_chars = text.chars().count();
  if char_idx >= max_chars {
    return total_advance;
  }
  let byte = byte_offset_for_char_idx(text, char_idx);
  let x = shaped_prefix_advance_for_byte(runs, byte);
  if x > 0.0 || x.is_finite() {
    x.min(total_advance)
  } else {
    // Fall back to proportional mapping when shaping fails.
    let avg = if max_chars > 0 {
      (fallback_advance / max_chars as f32).max(0.0)
    } else {
      0.0
    };
    (avg * char_idx as f32).min(total_advance)
  }
}

fn caret_index_for_x_in_text(
  text: &str,
  style: &ComputedStyle,
  rect: Rect,
  x: f32,
) -> usize {
  let char_count = text.chars().count();
  if char_count == 0 {
    return 0;
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

  let mut lo = 0usize;
  let mut hi = char_count;
  while lo < hi {
    let mid = (lo + hi) / 2;
    let mid_x = shaped_prefix_advance_for_char_idx(
      text,
      &runs,
      mid,
      total_advance,
      fallback_advance,
    );
    if mid_x < local_x {
      lo = mid + 1;
    } else {
      hi = mid;
    }
  }

  let upper = lo.min(char_count);
  if upper == 0 {
    return 0;
  }
  let lower = upper - 1;

  let lower_x = shaped_prefix_advance_for_char_idx(
    text,
    &runs,
    lower,
    total_advance,
    fallback_advance,
  );
  let upper_x = shaped_prefix_advance_for_char_idx(
    text,
    &runs,
    upper,
    total_advance,
    fallback_advance,
  );

  if (local_x - lower_x) <= (upper_x - local_x) {
    lower
  } else {
    upper
  }
}

fn caret_index_for_text_control_point(
  index: &DomIndexMut,
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
  node_id: usize,
  box_id: usize,
  page_point: Point,
) -> Option<usize> {
  let node = index.node(node_id)?;
  let box_node = box_node_by_id(box_tree, box_id)?;
  let style = box_node.style.as_ref();

  let border_rect = fragment_rect_for_box_id(fragment_tree, box_id)?;
  let viewport_size = fragment_tree.viewport_size();
  let content_rect = content_rect_for_border_rect(border_rect, style, viewport_size);

  if is_textarea(node) {
    let value = textarea_value_for_editing(node);
    if value.is_empty() {
      return Some(0);
    }

    let rect = inset_rect_uniform(content_rect, 2.0);
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
      return Some(0);
    }

    let metrics = if matches!(style.line_height, crate::style::types::LineHeight::Normal) {
      super::resolve_scaled_metrics_for_interaction(style)
    } else {
      None
    };
    let line_height =
      compute_line_height_with_metrics_viewport(style, metrics.as_ref(), Some(viewport_size));
    if line_height <= 0.0 || !line_height.is_finite() {
      return Some(0);
    }

    let mut local_y = page_point.y - rect.y();
    if !local_y.is_finite() {
      local_y = 0.0;
    }
    local_y = local_y.clamp(0.0, rect.height().max(0.0));

    let lines: Vec<&str> = value.split('\n').collect();
    let line_idx = ((local_y / line_height).floor() as isize).max(0) as usize;
    let line_idx = line_idx.min(lines.len().saturating_sub(1));

    let caret_line = lines.get(line_idx).copied().unwrap_or("");
    let line_y = rect.y() + line_idx as f32 * line_height;
    let line_rect = Rect::from_xywh(rect.x(), line_y, rect.width(), line_height);
    let caret_in_line = caret_index_for_x_in_text(caret_line, style, line_rect, page_point.x);

    // Map line-local caret index to global character index (including newline characters).
    let mut global = 0usize;
    for (idx, line) in lines.iter().enumerate() {
      if idx == line_idx {
        break;
      }
      global += line.chars().count();
      // Account for the '\n' separator between lines.
      global += 1;
    }

    let total_chars = value.chars().count();
    let caret = (global + caret_in_line).min(total_chars);
    return Some(caret);
  }

  if is_text_input(node) {
    let value = node.get_attribute_ref("value").unwrap_or("").to_string();
    if value.is_empty() {
      return Some(0);
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

    // Mirror the painter's reserved affordance space for some input types.
    let affordance_space = if input_type.eq_ignore_ascii_case("number") {
      14.0
    } else if matches!(
      input_type.to_ascii_lowercase().as_str(),
      "date" | "datetime-local" | "month" | "week" | "time"
    ) {
      12.0
    } else {
      0.0
    };
    if affordance_space > 0.0 {
      if style.direction == crate::style::types::Direction::Rtl {
        rect = Rect::from_xywh(
          rect.x() + affordance_space,
          rect.y(),
          (rect.width() - affordance_space).max(0.0),
          rect.height(),
        );
      } else {
        rect = Rect::from_xywh(
          rect.x(),
          rect.y(),
          (rect.width() - affordance_space).max(0.0),
          rect.height(),
        );
      }
    }

    let caret = caret_index_for_x_in_text(&display_text, style, rect, page_point.x);
    let total_chars = value.chars().count();
    return Some(caret.min(total_chars));
  }

  None
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
    let rect = Rect::from_xywh(abs_origin.x, abs_origin.y, node.bounds.width(), node.bounds.height());
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
    compute_line_height_with_metrics_viewport(style, metrics.as_ref(), Some(viewport_size));
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
      node_id,
      disabled,
      ..
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
            return Some((node.id, control.clone(), form_control.disabled, node.style.clone()));
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

fn find_ancestor_form(index: &DomIndexMut, mut node_id: usize) -> Option<usize> {
  while node_id != 0 {
    let node = index.node(node_id)?;
    if is_form(node) {
      return Some(node_id);
    }
    // Shadow roots are tree root boundaries for form owner resolution; do not walk out into the
    // shadow host tree.
    if matches!(node.node_type, DomNodeType::ShadowRoot { .. } | DomNodeType::Document { .. }) {
      break;
    }
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  None
}

fn tree_root_boundary_id(index: &DomIndexMut, mut node_id: usize) -> Option<usize> {
  while node_id != 0 {
    let node = index.node(node_id)?;
    if matches!(node.node_type, DomNodeType::Document { .. } | DomNodeType::ShadowRoot { .. }) {
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
    return index.node(referenced).is_some_and(is_form).then_some(referenced);
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

fn apply_select_keyboard_action(dom: &mut DomNode, index: &DomIndexMut, select_id: usize, key: KeyAction) -> bool {
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
      hover_chain: Vec::new(),
      active_chain: Vec::new(),
      pointer_down_target: None,
      range_drag: None,
      text_drag: None,
      focused: None,
      ime_composition: None,
      text_edit: None,
      modality: InputModality::Pointer,
    }
  }

  #[cfg(test)]
  fn set_text_selection_caret(&mut self, node_id: usize, caret: usize) {
    self.text_edit = Some(TextEditState {
      node_id,
      caret,
      selection_anchor: None,
      preferred_column: None,
    });
  }

  #[cfg(test)]
  fn set_text_selection_range(&mut self, node_id: usize, start: usize, end: usize) {
    if start == end {
      self.set_text_selection_caret(node_id, end);
      return;
    }
    self.text_edit = Some(TextEditState {
      node_id,
      caret: end,
      selection_anchor: Some(start),
      preferred_column: None,
    });
  }

  fn set_focus(
    &mut self,
    index: &mut DomIndexMut,
    new_focused: Option<usize>,
    focus_visible: bool,
  ) -> bool {
    let mut changed = false;
    if self.focused != new_focused {
      // Any focus change cancels an in-progress IME composition.
      if let Some(composition) = self.ime_composition.take() {
        if let Some(node_mut) = index.node_mut(composition.node_id) {
          changed |= remove_node_attr(node_mut, IME_PREEDIT_ATTR);
        }
      }

      if let Some(old) = self.focused {
        changed |= set_data_flag(index, old, "data-fastr-focus", false);
        changed |= set_data_flag(index, old, "data-fastr-focus-visible", false);
        changed |= clear_text_edit_data_attrs(index, old);
      }
      self.text_edit = None;
      self.text_drag = None;
    }

    if let Some(new_id) = new_focused {
      changed |= set_data_flag(index, new_id, "data-fastr-focus", true);
      changed |= set_data_flag(index, new_id, "data-fastr-focus-visible", focus_visible);

      if self.focused != new_focused {
        // Initialize text editing state for focused text controls.
        //
        // We keep the canonical caret/selection state in `InteractionEngine`, but we also mirror it
        // onto the DOM as `data-fastr-*` attributes so the box tree / painter can render the caret
        // and selection without a separate state channel.
        if index.node(new_id).is_some_and(|node| is_text_input(node) || is_textarea(node)) {
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
            selection_anchor: None,
            preferred_column: None,
          });
          changed |= write_text_edit_data_attrs(index, new_id, caret, None);
        }
      }
    }

    self.focused = new_focused;
    changed
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

    let prev_focus = self.focused;
    let mut index = DomIndexMut::new(dom);

    let node_id = node_id.filter(|&id| index.node(id).is_some_and(DomNode::is_element));
    let changed = self.set_focus(&mut index, node_id, focus_visible);

    let action = if self.focused != prev_focus {
      InteractionAction::FocusChanged {
        node_id: self.focused,
      }
    } else {
      InteractionAction::None
    };

    (changed, action)
  }

  pub fn set_text_selection_caret(&mut self, node_id: usize, caret: usize) {
    if self.focused != Some(node_id) {
      return;
    }
    self.text_drag = None;
    match self.text_edit.as_mut() {
      Some(edit) if edit.node_id == node_id => {
        edit.caret = caret;
        edit.selection_anchor = None;
        edit.preferred_column = None;
      }
      _ => {
        self.text_edit = Some(TextEditState {
          node_id,
          caret,
          selection_anchor: None,
          preferred_column: None,
        });
      }
    }
  }

  pub fn set_text_selection_range(&mut self, node_id: usize, start: usize, end: usize) {
    if self.focused != Some(node_id) {
      return;
    }
    let (start, end) = if start <= end { (start, end) } else { (end, start) };
    if start == end {
      self.set_text_selection_caret(node_id, start);
      return;
    }
    self.text_drag = None;
    match self.text_edit.as_mut() {
      Some(edit) if edit.node_id == node_id => {
        edit.caret = end;
        edit.selection_anchor = Some(start);
        edit.preferred_column = None;
      }
      _ => {
        self.text_edit = Some(TextEditState {
          node_id,
          caret: end,
          selection_anchor: Some(start),
          preferred_column: None,
        });
      }
    }
  }

  pub fn clear_pointer_state(&mut self, dom: &mut DomNode) -> bool {
    let mut index = DomIndexMut::new(dom);
    let hover_changed =
      diff_flag_chain(&mut index, "data-fastr-hover", &self.hover_chain, &[]);
    let active_changed =
      diff_flag_chain(&mut index, "data-fastr-active", &self.active_chain, &[]);
    self.hover_chain.clear();
    self.active_chain.clear();
    self.pointer_down_target = None;
    self.range_drag = None;
    self.text_drag = None;
    hover_changed | active_changed
  }

  pub fn clear_pointer_state_without_dom(&mut self) {
    self.hover_chain.clear();
    self.active_chain.clear();
    self.pointer_down_target = None;
    self.range_drag = None;
    self.text_drag = None;
  }

  /// Update hover state (data-fastr-hover on target + ancestors).
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
        dom_changed |= update_range_value_from_pointer(
          &mut index,
          fragment_tree,
          state.node_id,
          state.box_id,
          page_point,
        );
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
        if let Some(edit) = self.text_edit.as_mut().filter(|edit| edit.node_id == state.node_id) {
          if let Some(caret) = caret_index_for_text_control_point(
            &index,
            box_tree,
            fragment_tree,
            state.node_id,
            state.box_id,
            page_point,
          ) {
            edit.preferred_column = None;
            edit.caret = caret;
            if caret == state.anchor {
              edit.selection_anchor = None;
            } else {
              edit.selection_anchor = Some(state.anchor);
            }
            dom_changed |= write_text_edit_data_attrs(
              &mut index,
              state.node_id,
              edit.caret,
              edit.selection(),
            );
          }
        }
      }
    }

    let hit = hit_test_dom(dom, box_tree, fragment_tree, page_point);
    let new_chain = hit
      .and_then(|hit| nearest_element_ancestor(&index, hit.styled_node_id))
      .map(|target| collect_element_chain(&index, target))
      .unwrap_or_default();

    let changed = diff_flag_chain(
      &mut index,
      "data-fastr-hover",
      &self.hover_chain,
      &new_chain,
    );
    self.hover_chain = new_chain;
    dom_changed | changed
  }

  /// Begin active state (data-fastr-active on target + ancestors) and set modality=Pointer.
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
    self.modality = InputModality::Pointer;

    self.range_drag = None;
    self.text_drag = None;

    let page_point = viewport_point.translate(scroll.viewport);

    let down_hit = hit_test_dom(dom, box_tree, fragment_tree, page_point);
    let down_target = down_hit.as_ref().map(|hit| hit.dom_node_id);
    let mut index = DomIndexMut::new(dom);
    let new_chain = down_target
      .map(|target| collect_element_chain(&index, target))
      .unwrap_or_default();

    let changed = diff_flag_chain(
      &mut index,
      "data-fastr-active",
      &self.active_chain,
      &new_chain,
    );
    self.active_chain = new_chain;
    self.pointer_down_target = down_target;

    let mut dom_changed = changed;
    if let Some(hit) = down_hit.as_ref() {
      if index.node(hit.dom_node_id).is_some_and(is_range_input) {
        self.range_drag = Some(RangeDragState {
          node_id: hit.dom_node_id,
          box_id: hit.box_id,
        });
        dom_changed |= update_range_value_from_pointer(
          &mut index,
          fragment_tree,
          hit.dom_node_id,
          hit.box_id,
          page_point,
        );
      }

      // Click-to-place caret / begin selection dragging for focused text controls.
      if index
        .node(hit.dom_node_id)
        .is_some_and(|node| is_text_input(node) || is_textarea(node))
      {
        let focus_before = self.focused;
        if is_focusable_interactive_element(&index, hit.dom_node_id) {
          dom_changed |= self.set_focus(&mut index, Some(hit.dom_node_id), false);
        }

        // Only update caret/selection state when the text control is (now) focused.
        if self.focused == Some(hit.dom_node_id) {
          let caret = caret_index_for_text_control_point(
            &index,
            box_tree,
            fragment_tree,
            hit.dom_node_id,
            hit.box_id,
            page_point,
          )
          .unwrap_or(0);

          match self.text_edit.as_mut().filter(|state| state.node_id == hit.dom_node_id) {
            Some(state) => {
              state.set_caret(caret);
              state.clear_selection();
            }
            None => {
              self.text_edit = Some(TextEditState {
                node_id: hit.dom_node_id,
                caret,
                selection_anchor: None,
                preferred_column: None,
              });
            }
          }

          dom_changed |= write_text_edit_data_attrs(&mut index, hit.dom_node_id, caret, None);
          self.text_drag = Some(TextDragState {
            node_id: hit.dom_node_id,
            box_id: hit.box_id,
            anchor: caret,
            focus_before,
          });
        }
      }
    }

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

    remap_vec(&mut self.hover_chain, old_index, new_ids);
    remap_vec(&mut self.active_chain, old_index, new_ids);
    remap_opt(&mut self.pointer_down_target, old_index, new_ids);
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
    remap_opt(&mut self.focused, old_index, new_ids);

    if let Some(state) = &mut self.ime_composition {
      let new_node_id = old_index
        .id_to_node
        .get(state.node_id)
        .copied()
        .filter(|ptr| !ptr.is_null())
        .and_then(|ptr| new_ids.get(&(ptr as *const DomNode)).copied());
      match new_node_id {
        Some(id) => state.node_id = id,
        None => self.ime_composition = None,
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
    let range_drag = self.range_drag.take();
    let text_drag = self.text_drag.take();
    let prev_focus = text_drag
      .as_ref()
      .map(|state| state.focus_before)
      .unwrap_or(self.focused);

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
        dom_changed |= update_range_value_from_pointer(
          &mut index,
          fragment_tree,
          state.node_id,
          state.box_id,
          page_point,
        );
      }
    }
    for id in self.active_chain.iter().copied() {
      dom_changed |= set_data_flag(&mut index, id, "data-fastr-active", false);
    }
    self.active_chain.clear();
    self.pointer_down_target = None;

    let click_qualifies = match (down_semantic, up_semantic) {
      (Some(down), Some(up)) => down == up || is_ancestor_or_self(&index, down, up),
      (None, None) => true,
      _ => false,
    };

    let mut action = InteractionAction::None;

    let mut click_target = if click_qualifies { down_semantic } else { None };
    if let Some(target_id) = click_target {
      if index.node(target_id).is_some_and(is_label) {
        if let Some(control) = find_label_associated_control(&index, target_id) {
          click_target = Some(control);
        }
      }
    }

    if click_qualifies {
      if let Some(target_id) = click_target {
        if node_or_ancestor_is_inert(&index, target_id) {
          // Inert subtrees are not interactive: do not navigate, focus, or mutate form state.
        } else if index.node(target_id).is_some_and(is_select) {
          let snapshot = select_control_snapshot_from_box_tree(box_tree, target_id);
          let computed_disabled = snapshot.as_ref().is_some_and(|(_, _, disabled, _)| *disabled);
          if is_focusable_interactive_element(&index, target_id) && !computed_disabled {
            dom_changed |= self.set_focus(&mut index, Some(target_id), false);
          }

          let disabled = is_disabled_or_inert(&index, target_id) || computed_disabled;

          if !disabled {
            if let Some((select_box_id, control, _, style)) = snapshot.as_ref() {
              dom_changed |= apply_select_listbox_click(
                dom,
                fragment_tree,
                page_point,
                target_id,
                *select_box_id,
                scroll,
                control,
                style,
              );
            }
          }

          if !disabled {
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
          if is_focusable_interactive_element(&index, target_id) {
            dom_changed |= self.set_focus(&mut index, Some(target_id), false);
          }

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
                      if let Some(img_point) =
                        image_maps::local_point_in_fragment(fragment_tree, img_fragment, page_point)
                      {
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
              dom_changed |= set_data_flag(&mut index, target_id, "data-fastr-visited", true);

              let target_blank = index
                .node(target_id)
                .and_then(|node| node.get_attribute_ref("target"))
                .is_some_and(|target| trim_ascii_whitespace(target).eq_ignore_ascii_case("_blank"));

              let gesture_new_tab = matches!(button, PointerButton::Middle)
                || (matches!(button, PointerButton::Primary) && (modifiers.ctrl() || modifiers.meta()));

              action = if target_blank || gesture_new_tab {
                InteractionAction::OpenInNewTab { href: resolved }
              } else {
                InteractionAction::Navigate { href: resolved }
              };
            }
          } else if index.node(target_id).is_some_and(is_checkbox_input) {
            if !node_is_disabled(&index, target_id) {
              if let Some(node_mut) = index.node_mut(target_id) {
                dom_changed |= dom_mutation::toggle_checkbox(node_mut);
              }
            }
          } else if index.node(target_id).is_some_and(is_radio_input) {
            if !node_is_disabled(&index, target_id) {
              dom_changed |= dom_mutation::activate_radio(dom, target_id);
            }
          } else if index.node(target_id).is_some_and(is_submit_control) {
            if node_is_disabled(&index, target_id) {
              // Disabled submit controls do not submit.
            } else {
              // A form submission attempt flips HTML "user validity" so `:user-invalid` matches.
              if let Some(node_mut) = index.node_mut(target_id) {
                dom_changed |= dom_mutation::mark_user_validity(node_mut);
              }
              dom_changed |= dom_mutation::mark_form_user_validity(dom, target_id);
              if let Some(submission) = form_submission(dom, target_id, document_url, base_url) {
                match submission.method {
                  FormSubmissionMethod::Get => {
                    action = InteractionAction::Navigate { href: submission.url };
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

      // Blur when clicking outside focusable controls.
      //
      // If the pointer down started on a focusable target but the click was cancelled (pointer up
      // happened elsewhere / outside the page), we should not clear focus: typical browser UX
      // keeps the previously focused element focused in that case.
      let clicked_focusable =
        click_target.is_some_and(|id| is_focusable_interactive_element(&index, id));
      let down_prevents_blur = down_semantic.is_some_and(|id| {
        is_focusable_interactive_element(&index, id) || index.node(id).is_some_and(is_label)
      });
      if !clicked_focusable && !down_prevents_blur && prev_focus.is_some() {
        dom_changed |= self.set_focus(&mut index, None, false);
      }
    }

    // `OpenSelectDropdown` includes the focus update; do not replace it with `FocusChanged`.
    if matches!(action, InteractionAction::None) && self.focused != prev_focus {
      action = InteractionAction::FocusChanged {
        node_id: self.focused,
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
    let Some(focused) = self.focused else {
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
    changed |= self.ime_cancel_with_index(&mut index);

    let mut edit = self.text_edit.unwrap_or(TextEditState {
      node_id: focused,
      caret: current_len,
      selection_anchor: None,
      preferred_column: None,
    });
    if edit.node_id != focused {
      edit = TextEditState {
        node_id: focused,
        caret: current_len,
        selection_anchor: None,
        preferred_column: None,
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
    let changed_value = if focused_is_text_input {
      set_node_attr(node_mut, "value", &next)
    } else {
      set_node_attr(node_mut, "data-fastr-value", &next)
    };
    changed |= changed_value;
    if changed_value {
      changed |= dom_mutation::mark_user_validity(node_mut);
    }

    self.text_edit = Some(TextEditState {
      node_id: focused,
      caret: next_caret,
      selection_anchor: None,
      preferred_column: None,
    });
    changed |= write_text_edit_data_attrs(&mut index, focused, next_caret, None);

    changed
  }

  fn ime_cancel_with_index(&mut self, index: &mut DomIndexMut) -> bool {
    let Some(composition) = self.ime_composition.take() else {
      return false;
    };
    let Some(node_mut) = index.node_mut(composition.node_id) else {
      return false;
    };
    remove_node_attr(node_mut, IME_PREEDIT_ATTR)
  }

  /// Update the active IME preedit (composition) string for the focused text control.
  ///
  /// This should *not* mutate the actual DOM value; it stores the in-progress text as a
  /// `data-fastr-ime-preedit` attribute so the painter can render it at the caret with styling.
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

    let Some(focused) = self.focused else {
      return false;
    };

    let mut index = DomIndexMut::new(dom);

    // Ensure focus-visible when the keyboard/IME is used.
    let mut changed = self.set_focus(&mut index, Some(focused), true);

    // Only text inputs and textareas participate in IME composition.
    let is_text_control =
      index.node(focused).is_some_and(is_text_input) || index.node(focused).is_some_and(is_textarea);
    if !is_text_control {
      changed |= self.ime_cancel_with_index(&mut index);
      return changed;
    }

    if node_or_ancestor_is_inert(&index, focused)
      || node_is_disabled(&index, focused)
      || node_is_readonly(&index, focused)
    {
      changed |= self.ime_cancel_with_index(&mut index);
      return changed;
    }

    // Update internal state.
    match self.ime_composition.as_mut() {
      Some(existing) if existing.node_id == focused => {
        if existing.text != text || existing.cursor != cursor {
          existing.text.clear();
          existing.text.push_str(text);
          existing.cursor = cursor;
        }
      }
      _ => {
        self.ime_composition = Some(ImeCompositionState {
          node_id: focused,
          text: text.to_string(),
          cursor,
        });
      }
    }

    // Mirror the preedit text into the DOM as a paint hint.
    if let Some(node_mut) = index.node_mut(focused) {
      changed |= set_node_attr(node_mut, IME_PREEDIT_ATTR, text);
    }

    changed
  }

  /// Commit IME text into the focused text control, clearing any active preedit.
  pub fn ime_commit(&mut self, dom: &mut DomNode, text: &str) -> bool {
    self.modality = InputModality::Keyboard;
    let Some(focused) = self.focused else {
      return false;
    };

    let mut index = DomIndexMut::new(dom);
    // Ensure focus-visible when the IME is used.
    let mut changed = self.set_focus(&mut index, Some(focused), true);
    // Clear any in-flight preedit before inserting committed text.
    changed |= self.ime_cancel_with_index(&mut index);

    if text.is_empty() {
      return changed;
    }

    // Drop the index before delegating to `text_input`; it will re-index the DOM.
    drop(index);
    changed | self.text_input(dom, text)
  }

  /// Cancel any active IME preedit string without mutating the DOM value.
  pub fn ime_cancel(&mut self, dom: &mut DomNode) -> bool {
    let mut index = DomIndexMut::new(dom);
    self.ime_cancel_with_index(&mut index)
  }

  /// Select all text in the currently focused text control (`<input>`/`<textarea>`).
  ///
  /// This does not mutate the DOM; it only updates the internal selection range used by clipboard
  /// and text-editing actions.
  pub fn clipboard_select_all(&mut self, dom: &mut DomNode) -> bool {
    self.modality = InputModality::Keyboard;
    let Some(focused) = self.focused else {
      return false;
    };

    let mut index = DomIndexMut::new(dom);

    // Ensure focus-visible when the keyboard is used.
    let mut changed = self.set_focus(&mut index, Some(focused), true);

    if node_or_ancestor_is_inert(&index, focused) || node_is_disabled(&index, focused) {
      return changed;
    }

    let Some(node) = index.node(focused) else {
      return changed;
    };
    let is_text_input = is_text_input(node);
    let is_textarea = is_textarea(node);
    if !(is_text_input || is_textarea) {
      // Not a text control: clear any existing caret/selection attributes.
      changed |= clear_text_edit_data_attrs(&mut index, focused);
      self.text_edit = None;
      return changed;
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
      selection_anchor: None,
      preferred_column: None,
    });
    if edit.node_id != focused {
      edit = TextEditState {
        node_id: focused,
        caret: len,
        selection_anchor: None,
        preferred_column: None,
      };
    }
    edit.preferred_column = None;

    if len == 0 {
      edit.caret = 0;
      edit.selection_anchor = None;
    } else {
      edit.caret = len;
      edit.selection_anchor = Some(0);
    }

    self.text_edit = Some(edit);
    changed |= write_text_edit_data_attrs(&mut index, focused, edit.caret, edit.selection());

    changed
  }

  /// Return the current selection text for a focused text control (`<input>`/`<textarea>`), if any.
  ///
  /// This does not mutate the DOM.
  pub fn clipboard_copy(&mut self, dom: &mut DomNode) -> Option<String> {
    self.modality = InputModality::Keyboard;
    let Some(focused) = self.focused else {
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

  /// Cut the current selection into the clipboard, deleting it when the control is editable.
  ///
  /// Returns `(dom_changed, clipboard_text)`.
  pub fn clipboard_cut(&mut self, dom: &mut DomNode) -> (bool, Option<String>) {
    self.modality = InputModality::Keyboard;
    let Some(focused) = self.focused else {
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
      selection_anchor: None,
      preferred_column: None,
    });
    if edit.node_id != focused {
      edit = TextEditState {
        node_id: focused,
        caret: current_len,
        selection_anchor: None,
        preferred_column: None,
      };
    }
    edit.caret = edit.caret.min(current_len);
    edit.selection_anchor = edit.selection_anchor.map(|a| a.min(current_len));

    let Some((start, end)) = edit.selection() else {
      return (dom_changed, None);
    };

    let start_byte = byte_offset_for_char_idx(&current, start);
    let end_byte = byte_offset_for_char_idx(&current, end);
    if start_byte >= end_byte {
      return (dom_changed, None);
    }

    let selected = Some(current[start_byte..end_byte].to_string());
    if node_is_readonly(&index, focused) {
      return (dom_changed, selected);
    }

    dom_changed |= self.ime_cancel_with_index(&mut index);

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
      dom_changed |= dom_mutation::mark_user_validity(node_mut);
    }

    edit.caret = start.min(next_len);
    edit.selection_anchor = None;
    edit.preferred_column = None;
    self.text_edit = Some(edit);
    dom_changed |= write_text_edit_data_attrs(&mut index, focused, edit.caret, None);

    (dom_changed, selected)
  }

  /// Paste text into the focused text control (`<input>`/`<textarea>`), replacing any selection.
  pub fn clipboard_paste(&mut self, dom: &mut DomNode, text: &str) -> bool {
    self.modality = InputModality::Keyboard;
    let Some(focused) = self.focused else {
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

    if node_or_ancestor_is_inert(&index, focused)
      || node_is_disabled(&index, focused)
      || node_is_readonly(&index, focused)
    {
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

    changed |= self.ime_cancel_with_index(&mut index);

    let mut edit = self.text_edit.unwrap_or(TextEditState {
      node_id: focused,
      caret: current_len,
      selection_anchor: None,
      preferred_column: None,
    });
    if edit.node_id != focused {
      edit = TextEditState {
        node_id: focused,
        caret: current_len,
        selection_anchor: None,
        preferred_column: None,
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
    let changed_value = if focused_is_text_input {
      set_node_attr(node_mut, "value", &next)
    } else {
      set_node_attr(node_mut, "data-fastr-value", &next)
    };
    changed |= changed_value;
    if changed_value {
      changed |= dom_mutation::mark_user_validity(node_mut);
    }

    self.text_edit = Some(TextEditState {
      node_id: focused,
      caret: next_caret,
      selection_anchor: None,
      preferred_column: None,
    });
    changed |= write_text_edit_data_attrs(&mut index, focused, next_caret, None);

    changed
  }

  /// Handle keyboard actions that mutate the DOM without performing navigation.
  ///
  /// Element activation (links, form submission, etc.) is handled by [`InteractionEngine::key_activate`].
  pub fn key_action(&mut self, dom: &mut DomNode, key: KeyAction) -> bool {
    self.key_action_with_box_tree(dom, None, key)
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
    self.modality = InputModality::Keyboard;
    if matches!(key, KeyAction::Tab | KeyAction::ShiftTab) {
      // Focus traversal (wraps at ends).
      let mut index = DomIndexMut::new(dom);
      let focusables = collect_tab_focusables(&index);
      let next_focus = match key {
        KeyAction::Tab => next_tab_focus(self.focused, &focusables),
        KeyAction::ShiftTab => prev_tab_focus(self.focused, &focusables),
        _ => None,
      };
      let Some(next_focus) = next_focus else {
        return false;
      };
      return self.set_focus(&mut index, Some(next_focus), true);
    }

    let Some(focused) = self.focused else {
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
        selection_anchor: None,
        preferred_column: None,
      });
      if edit.node_id != focused {
        edit = TextEditState {
          node_id: focused,
          caret: current_len,
          selection_anchor: None,
          preferred_column: None,
        };
      }
      edit.caret = edit.caret.min(current_len);
      edit.selection_anchor = edit.selection_anchor.map(|a| a.min(current_len));

      let original = edit;

      match key {
        KeyAction::Backspace | KeyAction::Delete => {
          if !can_edit_value {
            return changed;
          }
          let selection = edit.selection();
          let (delete_start, delete_end, next_caret) = if let Some((start, end)) = selection {
            (start, end, start)
          } else if matches!(key, KeyAction::Backspace) {
            if edit.caret == 0 {
              return changed;
            }
            (edit.caret - 1, edit.caret, edit.caret - 1)
          } else {
            if edit.caret >= current_len {
              return changed;
            }
            (edit.caret, edit.caret + 1, edit.caret)
          };

          // Any direct text mutation cancels an in-progress IME preedit string.
          changed |= self.ime_cancel_with_index(&mut index);

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
          edit.selection_anchor = None;
          edit.preferred_column = None;

          if let Some(node_mut) = index.node_mut(focused) {
            let changed_value = if focused_is_text_input {
              set_node_attr(node_mut, "value", &next)
            } else {
              set_node_attr(node_mut, "data-fastr-value", &next)
            };
            changed |= changed_value;
            if changed_value {
              changed |= dom_mutation::mark_user_validity(node_mut);
            }
          }
        }
        KeyAction::Enter => {
          if focused_is_textarea {
            return changed | self.text_input(dom, "\n");
          }
        }
        KeyAction::ArrowLeft | KeyAction::ArrowRight => {
          let selection = edit.selection();
          let len = current_len;
          if let Some((start, end)) = selection {
            edit.set_caret(if matches!(key, KeyAction::ArrowLeft) { start } else { end });
          } else {
            let next = match key {
              KeyAction::ArrowLeft => edit.caret.saturating_sub(1),
              KeyAction::ArrowRight => (edit.caret + 1).min(len),
              _ => edit.caret,
            };
            edit.set_caret(next);
          }
        }
        KeyAction::ShiftArrowLeft | KeyAction::ShiftArrowRight => {
          let len = current_len;
          let next = match key {
            KeyAction::ShiftArrowLeft => edit.caret.saturating_sub(1),
            KeyAction::ShiftArrowRight => (edit.caret + 1).min(len),
            _ => edit.caret,
          };
          edit.set_caret_and_maybe_extend_selection(next, true);
        }
        KeyAction::Home | KeyAction::End => {
          let next = if matches!(key, KeyAction::Home) {
            0usize
          } else {
            current_len
          };
          edit.set_caret(next);
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
          edit.preferred_column = None;
          if current_len == 0 {
            edit.selection_anchor = None;
            edit.caret = 0;
          } else {
            edit.selection_anchor = Some(0);
            edit.caret = current_len;
          }
        }
        KeyAction::ArrowUp | KeyAction::ArrowDown => {
          if focused_is_textarea {
            // Vertical caret movement between newline-separated lines (no soft-wrap support yet).
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
            let preferred = edit.preferred_column.unwrap_or(col);

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
              edit.caret = target_start + preferred.min(target_len);
              edit.selection_anchor = None;
              edit.preferred_column = Some(preferred);
            }
          }
        }
        KeyAction::Space => {
          // Handled by `key_activate` (may trigger navigation).
        }
        KeyAction::Tab | KeyAction::ShiftTab => debug_assert!(false, "handled above"),
      }

      if edit != original {
        self.text_edit = Some(edit);
        changed |= write_text_edit_data_attrs(&mut index, focused, edit.caret, edit.selection());
      }

      return changed;
    }

    // Non-text-control keyboard actions.
    match key {
      KeyAction::ArrowUp | KeyAction::ArrowDown | KeyAction::Home | KeyAction::End => {
        if matches!(key, KeyAction::ArrowUp | KeyAction::ArrowDown) && index.node(focused).is_some_and(is_range_input)
        {
          if node_or_ancestor_is_inert(&index, focused)
            || node_is_disabled(&index, focused)
            || node_is_readonly(&index, focused)
          {
            return changed;
          }
          if let Some(node_mut) = index.node_mut(focused) {
            let delta = match key {
              KeyAction::ArrowUp => 1,
              KeyAction::ArrowDown => -1,
              _ => 0,
            };
            changed |= dom_mutation::step_range_value(node_mut, delta);
          }
        } else if index.node(focused).is_some_and(is_select) && !is_disabled_or_inert(&index, focused) {
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
          changed |= dom_mutation::activate_select_option(dom, focused, option_node_id, false);
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
    let prev_focus = self.focused;

    self.modality = InputModality::Keyboard;

    // Delegate text-editing keys to `key_action` so behaviour stays consistent.
    match key {
      KeyAction::Backspace
      | KeyAction::Delete
      | KeyAction::ArrowLeft
      | KeyAction::ArrowRight
      | KeyAction::ShiftArrowLeft
      | KeyAction::ShiftArrowRight
      | KeyAction::ShiftHome
      | KeyAction::ShiftEnd
      | KeyAction::SelectAll => {
        return (
          self.key_action_with_box_tree(dom, box_tree, key),
          InteractionAction::None,
        );
      }
      KeyAction::Tab | KeyAction::ShiftTab => {
        let dom_changed = self.key_action_with_box_tree(dom, box_tree, key);
        let action = if self.focused != prev_focus {
          InteractionAction::FocusChanged {
            node_id: self.focused,
          }
        } else {
          InteractionAction::None
        };
        return (dom_changed, action);
      }
      KeyAction::Enter => {
        let Some(focused) = self.focused else {
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
      KeyAction::Space => {}
      KeyAction::ArrowUp | KeyAction::ArrowDown | KeyAction::Home | KeyAction::End => {
        return (
          self.key_action_with_box_tree(dom, box_tree, key),
          InteractionAction::None,
        );
      }
    }

    let Some(focused) = self.focused else {
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
        } else if let Some(href) = index
          .node(focused)
          .filter(|node| is_anchor_with_href(node))
          .and_then(|node| node.get_attribute_ref("href"))
        {
          if let Some(resolved) = resolve_url(base_url, href) {
            changed |= set_data_flag(&mut index, focused, "data-fastr-visited", true);
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
              changed |= dom_mutation::toggle_checkbox(node_mut);
            }
          }
        } else if index.node(focused).is_some_and(is_radio_input) {
          if !node_is_disabled(&index, focused) {
            changed |= dom_mutation::activate_radio(dom, focused);
          }
        } else if index.node(focused).is_some_and(is_submit_control) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled submit controls do not submit.
          } else {
            // A form submission attempt flips HTML "user validity" so `:user-invalid` matches.
            if let Some(node_mut) = index.node_mut(focused) {
              changed |= dom_mutation::mark_user_validity(node_mut);
            }
            changed |= dom_mutation::mark_form_user_validity(dom, focused);
            if let Some(submission) = form_submission(dom, focused, document_url, base_url) {
              match submission.method {
                FormSubmissionMethod::Get => {
                  action = InteractionAction::Navigate { href: submission.url };
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
            if let Some(node_mut) = index.node_mut(focused) {
              changed |= dom_mutation::mark_user_validity(node_mut);
            }
            changed |= dom_mutation::mark_form_user_validity(dom, focused);
            if let Some(form_id) = resolve_form_owner(&index, focused) {
              let submitter_id = find_default_form_submitter(&index, form_id);
              if let Some(submitter_id) = submitter_id {
                if let Some(submission) = form_submission(dom, submitter_id, document_url, base_url) {
                  match submission.method {
                    FormSubmissionMethod::Get => {
                      action = InteractionAction::Navigate { href: submission.url };
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
      }
      KeyAction::Space => {
        if node_or_ancestor_is_inert(&index, focused) {
          // Inert subtrees are not interactive.
        } else if index.node(focused).is_some_and(is_checkbox_input) {
          if !node_is_disabled(&index, focused) {
            if let Some(node_mut) = index.node_mut(focused) {
              changed |= dom_mutation::toggle_checkbox(node_mut);
            }
          }
        } else if index.node(focused).is_some_and(is_radio_input) {
          if !node_is_disabled(&index, focused) {
            changed |= dom_mutation::activate_radio(dom, focused);
          }
        } else if index.node(focused).is_some_and(is_submit_control) {
          if is_disabled_or_inert(&index, focused) {
            // Disabled submit controls do not submit.
          } else {
            if let Some(node_mut) = index.node_mut(focused) {
              changed |= dom_mutation::mark_user_validity(node_mut);
            }
            changed |= dom_mutation::mark_form_user_validity(dom, focused);
            if let Some(submission) = form_submission(dom, focused, document_url, base_url) {
              match submission.method {
                FormSubmissionMethod::Get => {
                  action = InteractionAction::Navigate { href: submission.url };
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
    ) && self.focused != prev_focus
    {
      action = InteractionAction::FocusChanged {
        node_id: self.focused,
      };
    }

    (changed, action)
  }
}
