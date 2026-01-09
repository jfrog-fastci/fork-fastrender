use crate::dom::enumerate_dom_ids;
use crate::dom::DomNode;
use crate::dom::DomNodeType;
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::tree::box_tree::BoxNode;
use crate::tree::box_tree::BoxTree;
use crate::tree::box_tree::BoxType;
use crate::tree::box_tree::FormControlKind;
use crate::tree::box_tree::ReplacedType;
use crate::tree::box_tree::SelectControl;
use crate::tree::fragment_tree::FragmentTree;
use std::collections::HashMap;
use url::form_urlencoded;
use url::Url;

use super::dom_mutation;
use super::hit_test::hit_test_dom;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputModality {
  Pointer,
  Keyboard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionAction {
  None,
  Navigate { href: String },
  FocusChanged { node_id: Option<usize> },
  OpenSelectDropdown {
    select_node_id: usize,
    control: crate::tree::box_tree::SelectControl,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
  Backspace,
  Enter,
  Space,
  Tab,
}

#[derive(Debug, Clone)]
pub struct InteractionEngine {
  hover_chain: Vec<usize>,
  active_chain: Vec<usize>,
  pointer_down_target: Option<usize>,
  range_drag: Option<RangeDragState>,
  focused: Option<usize>,
  modality: InputModality,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RangeDragState {
  node_id: usize,
  box_id: usize,
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
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("a"))
    && node.get_attribute_ref("href").is_some_and(|href| {
      let href = trim_ascii_whitespace(href);
      !href.is_empty()
        && !href
          .as_bytes()
          .get(.."javascript:".len())
          .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"javascript:"))
    })
}

fn is_label(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("label"))
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
  trim_ascii_whitespace(node.get_attribute_ref("type").unwrap_or("text"))
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

fn button_type(node: &DomNode) -> &str {
  // HTML <button> defaults to submit.
  node.get_attribute_ref("type").unwrap_or("submit")
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
    if node.get_attribute_ref("inert").is_some() {
      return true;
    }
    if node
      .get_attribute_ref("data-fastr-inert")
      .is_some_and(|v| v.eq_ignore_ascii_case("true"))
    {
      return true;
    }

    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }

  false
}

fn has_tabindex_minus_one(node: &DomNode) -> bool {
  let Some(tabindex) = node.get_attribute_ref("tabindex") else {
    return false;
  };
  let trimmed = trim_ascii_whitespace(tabindex);
  if trimmed.is_empty() {
    return false;
  }
  trimmed
    .parse::<i32>()
    .ok()
    .is_some_and(|value| value == -1)
}

/// MVP focusable predicate for pointer focus / blur decisions.
///
/// This intentionally only covers native interactive elements we currently support.
fn is_focusable_interactive_element(index: &DomIndexMut, node_id: usize) -> bool {
  let Some(node) = index.node(node_id) else {
    return false;
  };

  if is_disabled_or_inert(index, node_id) {
    return false;
  }

  // If tabindex=-1 is used to remove an element from focus traversal, also skip focusing it via
  // pointer interaction for MVP consistency.
  if has_tabindex_minus_one(node) {
    return false;
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

fn resolve_url(base_url: &str, href: &str) -> Option<String> {
  let href = trim_ascii_whitespace(href);
  if href.is_empty() {
    return None;
  }
  if href
    .as_bytes()
    .get(.."javascript:".len())
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"javascript:"))
  {
    return None;
  }

  if let Ok(base) = Url::parse(base_url) {
    if let Ok(joined) = base.join(href) {
      if joined.scheme().eq_ignore_ascii_case("javascript") {
        return None;
      }
      return Some(joined.to_string());
    }
  }

  let absolute = Url::parse(href).ok()?;
  (!absolute.scheme().eq_ignore_ascii_case("javascript")).then(|| absolute.to_string())
}

fn find_element_by_id_attr(index: &DomIndexMut, html_id: &str) -> Option<usize> {
  for (node_id, ptr) in index.id_to_node.iter().copied().enumerate().skip(1) {
    if ptr.is_null() {
      continue;
    }
    let node = unsafe { &*ptr };
    if !node.is_element() {
      continue;
    }
    if node.get_attribute_ref("id") == Some(html_id) {
      return Some(node_id);
    }
  }
  None
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
    // Spec-ish: `for` matches element IDs in the same tree.
    return find_element_by_id_attr(index, for_attr);
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
    if is_input(node)
      || is_textarea(node)
      || node
        .tag_name()
        .is_some_and(|t| t.eq_ignore_ascii_case("select"))
    {
      return Some(id);
    }
  }

  None
}

fn collect_text_children_value(node: &DomNode) -> String {
  let mut value = String::new();
  for child in &node.children {
    if let DomNodeType::Text { content } = &child.node_type {
      value.push_str(content);
    }
  }
  value
}

/// Set textarea value by updating (existing) text children. Returns `(changed, inserted_child)`.
fn set_textarea_text_children_value(node: &mut DomNode, value: &str) -> (bool, bool) {
  let mut found_text = false;
  let mut changed = false;

  for child in node.children.iter_mut() {
    if let DomNodeType::Text { content } = &mut child.node_type {
      if !found_text {
        found_text = true;
        if content != value {
          content.clear();
          content.push_str(value);
          changed = true;
        }
      } else if !content.is_empty() {
        content.clear();
        changed = true;
      }
    }
  }

  if found_text {
    return (changed, false);
  }

  // No existing text node. Insert one; this changes pre-order ids for subsequent nodes.
  node.children.push(DomNode {
    node_type: DomNodeType::Text {
      content: value.to_string(),
    },
    children: Vec::new(),
  });
  (true, true)
}

#[derive(Debug, Clone, Copy)]
enum SelectRow {
  OptGroupLabel { disabled: bool },
  Option { node_id: usize, disabled: bool },
}

fn has_disabled_optgroup_ancestor(index: &DomIndexMut, mut node_id: usize, root_id: usize) -> bool {
  while node_id != 0 && node_id != root_id {
    let parent = *index.parent.get(node_id).unwrap_or(&0);
    if parent == 0 || parent == root_id {
      break;
    }
    if index.node(parent).is_some_and(|node| {
      node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("optgroup"))
        && node.get_attribute_ref("disabled").is_some()
    }) {
      return true;
    }
    node_id = parent;
  }
  false
}

fn collect_select_rows(index: &DomIndexMut, select_id: usize) -> Vec<SelectRow> {
  // Like `build_select_control`, `<optgroup>` contributes a label row followed by its descendants.
  // This function operates directly on the DOM so it can recover DOM node ids for `<option>` rows
  // and mutate `selected` attributes.
  let mut end = select_id;
  for id in (select_id + 1)..index.id_to_node.len() {
    if is_ancestor_or_self(index, select_id, id) {
      end = id;
    } else {
      break;
    }
  }

  let mut rows = Vec::new();
  for id in (select_id + 1)..=end {
    let Some(node) = index.node(id) else {
      continue;
    };
    if !node.is_element() {
      continue;
    }
    let Some(tag) = node.tag_name() else {
      continue;
    };

    if tag.eq_ignore_ascii_case("optgroup") {
      let disabled = node.get_attribute_ref("disabled").is_some()
        || has_disabled_optgroup_ancestor(index, id, select_id);
      rows.push(SelectRow::OptGroupLabel { disabled });
      continue;
    }

    if tag.eq_ignore_ascii_case("option") {
      let disabled = node.get_attribute_ref("disabled").is_some()
        || has_disabled_optgroup_ancestor(index, id, select_id);
      rows.push(SelectRow::Option {
        node_id: id,
        disabled,
      });
    }
  }

  rows
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

fn parse_range_number_attr(node: &DomNode, name: &str, default: f32) -> f32 {
  node
    .get_attribute_ref(name)
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .and_then(|v| v.parse::<f32>().ok())
    .filter(|v| v.is_finite())
    .unwrap_or(default)
}

fn parse_range_min_max(node: &DomNode) -> (f32, f32) {
  let min = parse_range_number_attr(node, "min", 0.0);
  let mut max = parse_range_number_attr(node, "max", 100.0);
  if max < min {
    max = min;
  }
  (min, max)
}

fn format_range_value(value: f64) -> String {
  // Round to 6 decimal places (ignoring `step` for MVP) and format with a stable, compact
  // representation to avoid writing noisy float strings into the DOM.
  const SCALE: f64 = 1_000_000.0;
  let mut v = (value * SCALE).round() / SCALE;
  if v == 0.0 {
    // Canonicalize negative zero.
    v = 0.0;
  }
  v.to_string()
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

  let Some(node) = index.node(node_id) else {
    return false;
  };
  if !is_range_input(node) {
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

  let (min, max) = parse_range_min_max(node);
  let min = min as f64;
  let max = max as f64;
  let value = min + (max - min) * fraction as f64;
  if !value.is_finite() {
    return false;
  }

  let value_attr = format_range_value(value);
  let Some(node_mut) = index.node_mut(node_id) else {
    return false;
  };
  let changed_value = set_node_attr(node_mut, "value", &value_attr);
  let mut changed = changed_value;
  if changed_value {
    changed |= dom_mutation::mark_user_validity(node_mut);
  }
  changed
}

fn apply_select_listbox_click(
  index: &mut DomIndexMut,
  fragment_tree: &FragmentTree,
  page_point: Point,
  select_id: usize,
  hit_box_id: usize,
) -> bool {
  let Some(select_node) = index.node(select_id) else {
    return false;
  };

  let multiple = select_node.get_attribute_ref("multiple").is_some();
  let size = crate::dom::select_effective_size(select_node) as usize;
  let is_listbox = multiple || size > 1;
  if !is_listbox {
    return false;
  }

  let Some(select_rect) = fragment_rect_for_box_id_at_point(fragment_tree, page_point, hit_box_id) else {
    return false;
  };

  let rows = collect_select_rows(index, select_id);
  if rows.is_empty() {
    return false;
  }

  let row_height = if size == 0 {
    0.0
  } else {
    select_rect.height() / size as f32
  };
  if row_height <= 0.0 || !row_height.is_finite() {
    return false;
  }

  let local_y = page_point.y - select_rect.y();
  let mut row_idx = (local_y / row_height).floor() as isize;
  row_idx = row_idx.clamp(0, rows.len().saturating_sub(1) as isize);
  let row = rows[row_idx as usize];

  let mut changed = false;
  match row {
    SelectRow::OptGroupLabel { .. } => {}
    SelectRow::Option { node_id, disabled } => {
      if disabled {
        return false;
      }

      if multiple {
        let selected = index
          .node(node_id)
          .and_then(|node| node.get_attribute_ref("selected"))
          .is_some();
        if let Some(node_mut) = index.node_mut(node_id) {
          changed |= dom_mutation::set_bool_attr(node_mut, "selected", !selected);
        }
      } else {
        for row in rows {
          let SelectRow::Option { node_id: id, .. } = row else {
            continue;
          };
          if let Some(node_mut) = index.node_mut(id) {
            changed |= dom_mutation::set_bool_attr(node_mut, "selected", id == node_id);
          }
        }
      }
    }
  }

  changed
}

fn select_control_snapshot_from_box_tree(
  box_tree: &BoxTree,
  select_node_id: usize,
) -> Option<(SelectControl, bool)> {
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.styled_node_id == Some(select_node_id) {
      if let BoxType::Replaced(replaced) = &node.box_type {
        if let ReplacedType::FormControl(form_control) = &replaced.replaced_type {
          if let FormControlKind::Select(control) = &form_control.control {
            return Some((control.clone(), form_control.disabled));
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
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  None
}

// `SelectControl` uses Strings/Vecs and does not contain floats, so its derived `PartialEq` is a
// full equivalence relation. Mark it as `Eq` so interaction actions can remain `Eq` as well.
impl Eq for SelectControl {}

fn form_control_value(node: &DomNode) -> Option<(String, String)> {
  let name = node
    .get_attribute_ref("name")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())?;

  if is_checkbox_input(node) || is_radio_input(node) {
    let checked = node.get_attribute_ref("checked").is_some();
    if !checked {
      return None;
    }
    let value = node.get_attribute_ref("value").unwrap_or("on");
    return Some((name.to_string(), value.to_string()));
  }

  if is_input(node) {
    // Skip button-ish inputs.
    let t = input_type(node);
    if t.eq_ignore_ascii_case("submit")
      || t.eq_ignore_ascii_case("button")
      || t.eq_ignore_ascii_case("reset")
      || t.eq_ignore_ascii_case("image")
      || t.eq_ignore_ascii_case("file")
    {
      return None;
    }
    let value = node.get_attribute_ref("value").unwrap_or("");
    return Some((name.to_string(), value.to_string()));
  }

  if is_textarea(node) {
    let value = collect_text_children_value(node);
    return Some((name.to_string(), value));
  }

  None
}

fn build_get_form_submission_url(
  index: &DomIndexMut,
  form_id: usize,
  document_url: &str,
  base_url: &str,
) -> Option<String> {
  let form = index.node(form_id)?;

  let method = form.get_attribute_ref("method").unwrap_or("get");
  if !method.is_empty() && !method.eq_ignore_ascii_case("get") {
    return None;
  }

  let action_attr = form
    .get_attribute_ref("action")
    .map(trim_ascii_whitespace)
    .unwrap_or("");

  let action_url = if action_attr.is_empty() {
    let doc = trim_ascii_whitespace(document_url);
    if doc.is_empty() {
      return None;
    }
    doc.to_string()
  } else {
    resolve_url(base_url, action_attr)?
  };

  let mut url = Url::parse(&action_url).ok()?;

  // GET submissions set the query to the encoded form data.
  url.set_query(None);
  let mut serializer = form_urlencoded::Serializer::new(String::new());

  for id in 1..index.id_to_node.len() {
    if !is_ancestor_or_self(index, form_id, id) {
      continue;
    }
    let Some(node) = index.node(id) else {
      continue;
    };
    if !node.is_element() {
      continue;
    }
    if node_or_ancestor_is_inert(index, id) || node_is_disabled(index, id) {
      continue;
    }

    if let Some((name, value)) = form_control_value(node) {
      serializer.append_pair(&name, &value);
    }
  }

  let query = serializer.finish();
  if !query.is_empty() {
    url.set_query(Some(&query));
  }

  Some(url.to_string())
}

impl InteractionEngine {
  pub fn new() -> Self {
    Self {
      hover_chain: Vec::new(),
      active_chain: Vec::new(),
      pointer_down_target: None,
      range_drag: None,
      focused: None,
      modality: InputModality::Pointer,
    }
  }

  fn set_focus(
    &mut self,
    index: &mut DomIndexMut,
    new_focused: Option<usize>,
    focus_visible: bool,
  ) -> bool {
    let mut changed = false;
    if self.focused != new_focused {
      if let Some(old) = self.focused {
        changed |= set_data_flag(index, old, "data-fastr-focus", false);
        changed |= set_data_flag(index, old, "data-fastr-focus-visible", false);
      }
    }

    if let Some(new_id) = new_focused {
      changed |= set_data_flag(index, new_id, "data-fastr-focus", true);
      changed |= set_data_flag(index, new_id, "data-fastr-focus-visible", focus_visible);
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

  /// Update hover state (data-fastr-hover on target + ancestors).
  ///
  /// Note: For pages with scroll containers, pass a fragment tree with element scroll offsets
  /// applied (e.g. via `interaction::fragment_tree_with_scroll` / `scroll::apply_scroll_offsets`)
  /// so hit testing matches what is painted.
  pub fn pointer_move(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    page_point: Point,
  ) -> bool {
    let mut index = DomIndexMut::new(dom);
    let mut dom_changed = false;
    if let Some(state) = self.range_drag {
      dom_changed |=
        update_range_value_from_pointer(&mut index, fragment_tree, state.node_id, state.box_id, page_point);
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
  ///
  /// Note: For pages with scroll containers, pass a fragment tree with element scroll offsets
  /// applied (e.g. via `interaction::fragment_tree_with_scroll` / `scroll::apply_scroll_offsets`)
  /// so hit testing matches what is painted.
  pub fn pointer_down(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    page_point: Point,
  ) -> bool {
    self.modality = InputModality::Pointer;

    self.range_drag = None;

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
  }

  /// End active state, and if click qualifies, perform action:
  /// - link: return Navigate
  /// - checkbox/radio: toggle/activate
  /// - text control/textarea: focus
  /// - dropdown select: return OpenSelectDropdown (selection deferred to UI)
  ///
  /// Note: For pages with scroll containers, pass a fragment tree with element scroll offsets
  /// applied (e.g. via `interaction::fragment_tree_with_scroll` / `scroll::apply_scroll_offsets`)
  /// so hit testing matches what is painted.
  pub fn pointer_up(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    page_point: Point,
    base_url: &str,
  ) -> (bool, InteractionAction) {
    let range_drag = self.range_drag.take();
    let prev_focus = self.focused;

    let up_hit = hit_test_dom(dom, box_tree, fragment_tree, page_point);
    let up_semantic = up_hit.as_ref().map(|hit| hit.dom_node_id);
    let mut index = DomIndexMut::new(dom);

    let down_semantic = self.pointer_down_target;

    // Clear active chain unconditionally.
    let mut dom_changed = false;
    if let Some(state) = range_drag {
      dom_changed |= update_range_value_from_pointer(
        &mut index,
        fragment_tree,
        state.node_id,
        state.box_id,
        page_point,
      );
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
          let computed_disabled = snapshot.as_ref().is_some_and(|(_, disabled)| *disabled);
          if is_focusable_interactive_element(&index, target_id) && !computed_disabled {
            dom_changed |= self.set_focus(&mut index, Some(target_id), false);
          }

          let disabled = is_disabled_or_inert(&index, target_id) || computed_disabled;

          if !disabled {
            if let Some(hit) = up_hit.as_ref().filter(|hit| hit.dom_node_id == target_id) {
              dom_changed |= apply_select_listbox_click(
                &mut index,
                fragment_tree,
                page_point,
                target_id,
                hit.box_id,
              );
            }
          }

          if !disabled {
            if let Some((control, _)) = snapshot {
              let is_dropdown = !control.multiple && control.size == 1;
              if is_dropdown {
                action = InteractionAction::OpenSelectDropdown {
                  select_node_id: target_id,
                  control,
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
            if let Some(resolved) = resolve_url(base_url, href) {
              dom_changed |= set_data_flag(&mut index, target_id, "data-fastr-visited", true);
              action = InteractionAction::Navigate { href: resolved };
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
          } else if index.node(target_id).is_some_and(|node| {
            is_submit_input(node) || is_submit_button(node)
          }) {
            // A form submission attempt flips HTML "user validity" so `:user-invalid` matches.
            dom_changed |= dom_mutation::mark_form_user_validity(dom, target_id);
          }
        }
      }

      // Blur when clicking outside focusable controls.
      let clicked_focusable = click_target.is_some_and(|id| is_focusable_interactive_element(&index, id));
      if !clicked_focusable && prev_focus.is_some() {
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

  /// Insert typed text into focused text control (input/textarea) and set focus-visible.
  pub fn text_input(&mut self, dom: &mut DomNode, text: &str) -> bool {
    self.modality = InputModality::Keyboard;
    let Some(focused) = self.focused else {
      return false;
    };

    let mut index = DomIndexMut::new(dom);

    let mut changed = false;
    changed |= self.set_focus(&mut index, Some(focused), true);

    if index.node(focused).is_some_and(is_text_input) {
      if node_or_ancestor_is_inert(&index, focused)
        || node_is_disabled(&index, focused)
        || node_is_readonly(&index, focused)
      {
        return changed;
      }
      let current = index
        .node(focused)
        .and_then(|node| node.get_attribute_ref("value"))
        .unwrap_or("")
        .to_string();
      let mut next = current;
      next.push_str(text);
      let changed_value = set_node_attr(
        index.node_mut(focused).expect("node exists"),
        "value",
        &next,
      );
      changed |= changed_value;
      if changed_value {
        changed |= dom_mutation::mark_user_validity(index.node_mut(focused).expect("node exists"));
      }
      return changed;
    }

    if index.node(focused).is_some_and(is_textarea) {
      if node_or_ancestor_is_inert(&index, focused)
        || node_is_disabled(&index, focused)
        || node_is_readonly(&index, focused)
      {
        return changed;
      }
      let current = index
        .node(focused)
        .map(collect_text_children_value)
        .unwrap_or_default();
      let mut next = current;
      next.push_str(text);

      let old_index = index;
      let mut index = DomIndexMut::new(dom);
      let Some(node_mut) = index.node_mut(focused) else {
        return changed;
      };
      let (changed_value, inserted) = set_textarea_text_children_value(node_mut, &next);
      changed |= changed_value;
      if changed_value {
        changed |= dom_mutation::mark_user_validity(node_mut);
      }
      if inserted {
        let new_ids = enumerate_dom_ids(dom);
        self.remap_engine_ids_after_dom_change(&old_index, &new_ids);
      }
      return changed;
    }

    changed
  }

  /// Handle special keys that mutate focused text controls:
  /// - Backspace
  /// - Enter (textarea newline only)
  /// - Tab (optional focus traversal stub)
  pub fn key_action(&mut self, dom: &mut DomNode, key: KeyAction) -> bool {
    self.modality = InputModality::Keyboard;
    let Some(focused) = self.focused else {
      return false;
    };

    let mut index = DomIndexMut::new(dom);
    let mut changed = false;
    changed |= self.set_focus(&mut index, Some(focused), true);

    match key {
      KeyAction::Backspace => {
        if index.node(focused).is_some_and(is_text_input) {
          if node_or_ancestor_is_inert(&index, focused)
            || node_is_disabled(&index, focused)
            || node_is_readonly(&index, focused)
          {
            return changed;
          }
          let current = index
            .node(focused)
            .and_then(|node| node.get_attribute_ref("value"))
            .unwrap_or("")
            .to_string();
          let mut next = current;
          if next.pop().is_some() {
            let changed_value = set_node_attr(
              index.node_mut(focused).expect("node exists"),
              "value",
              &next,
            );
            changed |= changed_value;
            if changed_value {
              changed |=
                dom_mutation::mark_user_validity(index.node_mut(focused).expect("node exists"));
            }
          }
        } else if index.node(focused).is_some_and(is_textarea) {
          if node_or_ancestor_is_inert(&index, focused)
            || node_is_disabled(&index, focused)
            || node_is_readonly(&index, focused)
          {
            return changed;
          }
          let current = index
            .node(focused)
            .map(collect_text_children_value)
            .unwrap_or_default();
          let mut next = current;
          if next.pop().is_some() {
            let old_index = index;
            let mut index = DomIndexMut::new(dom);
            if let Some(node_mut) = index.node_mut(focused) {
              let (changed_value, inserted) = set_textarea_text_children_value(node_mut, &next);
              changed |= changed_value;
              if changed_value {
                changed |= dom_mutation::mark_user_validity(node_mut);
              }
              if inserted {
                let new_ids = enumerate_dom_ids(dom);
                self.remap_engine_ids_after_dom_change(&old_index, &new_ids);
              }
            }
          }
        }
      }
      KeyAction::Enter => {
        if index.node(focused).is_some_and(is_textarea) {
          if node_or_ancestor_is_inert(&index, focused)
            || node_is_disabled(&index, focused)
            || node_is_readonly(&index, focused)
          {
            return changed;
          }
          let current = index
            .node(focused)
            .map(collect_text_children_value)
            .unwrap_or_default();
          let mut next = current;
          next.push('\n');
          let old_index = index;
          let mut index = DomIndexMut::new(dom);
          if let Some(node_mut) = index.node_mut(focused) {
            let (changed_value, inserted) = set_textarea_text_children_value(node_mut, &next);
            changed |= changed_value;
            if changed_value {
              changed |= dom_mutation::mark_user_validity(node_mut);
            }
            if inserted {
              let new_ids = enumerate_dom_ids(dom);
              self.remap_engine_ids_after_dom_change(&old_index, &new_ids);
            }
          }
        }
      }
      KeyAction::Space => {
        // Keyboard activation (Space/Enter) can trigger navigation, so it's handled by
        // `InteractionEngine::key_activate`.
      }
      KeyAction::Tab => {
        // Focus traversal is intentionally left as a stub for MVP.
      }
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
    let prev_focus = self.focused;

    self.modality = InputModality::Keyboard;
    let Some(focused) = self.focused else {
      return (false, InteractionAction::None);
    };

    // Delegate text-editing keys to `key_action` so behaviour stays consistent.
    match key {
      KeyAction::Backspace => {
        return (self.key_action(dom, KeyAction::Backspace), InteractionAction::None);
      }
      KeyAction::Tab => {
        let dom_changed = self.key_action(dom, KeyAction::Tab);
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
        let index = DomIndexMut::new(dom);
        if index.node(focused).is_some_and(is_textarea) {
          return (self.key_action(dom, KeyAction::Enter), InteractionAction::None);
        }
      }
      KeyAction::Space => {}
    }

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
            action = InteractionAction::Navigate { href: resolved };
          }
        } else if index.node(focused).is_some_and(is_submit_control) {
          if node_is_disabled(&index, focused) {
            // Disabled submit controls do not submit.
          } else if let Some(form_id) = find_ancestor_form(&index, focused) {
            if let Some(url) = build_get_form_submission_url(&index, form_id, document_url, base_url) {
              action = InteractionAction::Navigate { href: url };
            }
          }
        } else if index.node(focused).is_some_and(is_text_input) {
          if node_is_disabled(&index, focused) {
            // Disabled controls do not submit.
          } else if let Some(form_id) = find_ancestor_form(&index, focused) {
            if let Some(url) = build_get_form_submission_url(&index, form_id, document_url, base_url) {
              action = InteractionAction::Navigate { href: url };
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
        } else if index.node(focused).is_some_and(is_button) {
          // MVP: no-op for non-submit buttons (no JS event dispatch yet).
        }
      }
      _ => {}
    }

    if !matches!(action, InteractionAction::Navigate { .. }) && self.focused != prev_focus {
      action = InteractionAction::FocusChanged {
        node_id: self.focused,
      };
    }

    (changed, action)
  }
}
