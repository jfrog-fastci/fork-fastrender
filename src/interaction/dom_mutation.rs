use crate::dom::DomNode;
use crate::dom::DomNodeType;
use crate::dom::HTML_NAMESPACE;
use crate::dom::{format_number, input_range_bounds};

use super::dom_index::DomIndex;

fn is_html_element(node: &DomNode) -> bool {
  matches!(node.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE)
}

fn node_attrs_mut(node: &mut DomNode) -> Option<(&mut Vec<(String, String)>, bool)> {
  match &mut node.node_type {
    DomNodeType::Element {
      namespace,
      attributes,
      ..
    } => Some((attributes, namespace.is_empty() || namespace == HTML_NAMESPACE)),
    DomNodeType::Slot {
      namespace,
      attributes,
      ..
    } => Some((attributes, namespace.is_empty() || namespace == HTML_NAMESPACE)),
    _ => None,
  }
}

fn name_matches(existing: &str, query: &str, is_html: bool) -> bool {
  if is_html {
    existing.eq_ignore_ascii_case(query)
  } else {
    existing == query
  }
}

fn is_disabled_or_inert(node: &DomNode) -> bool {
  if node.get_attribute_ref("disabled").is_some() {
    return true;
  }
  if node.get_attribute_ref("inert").is_some() {
    return true;
  }
  node
    .get_attribute_ref("data-fastr-inert")
    .map(|v| v.eq_ignore_ascii_case("true"))
    .unwrap_or(false)
}

fn is_input_of_type(node: &DomNode, ty: &str) -> bool {
  node
    .tag_name()
    .is_some_and(|t| t.eq_ignore_ascii_case("input") && is_html_element(node))
    && node
      .get_attribute_ref("type")
      .unwrap_or("text")
      .eq_ignore_ascii_case(ty)
}

fn radio_group_name(node: &DomNode) -> Option<&str> {
  node
    .get_attribute_ref("name")
    .filter(|name| !name.is_empty())
}

fn is_text_like_input(node: &DomNode) -> bool {
  if !node
    .tag_name()
    .is_some_and(|t| t.eq_ignore_ascii_case("input") && is_html_element(node))
  {
    return false;
  }

  let ty = node.get_attribute_ref("type").unwrap_or("text");
  !ty.eq_ignore_ascii_case("checkbox")
    && !ty.eq_ignore_ascii_case("radio")
    && !ty.eq_ignore_ascii_case("button")
    && !ty.eq_ignore_ascii_case("submit")
    && !ty.eq_ignore_ascii_case("reset")
    && !ty.eq_ignore_ascii_case("range")
    && !ty.eq_ignore_ascii_case("color")
    && !ty.eq_ignore_ascii_case("file")
    && !ty.eq_ignore_ascii_case("hidden")
}

pub fn set_attr(node: &mut DomNode, name: &str, value: &str) -> bool {
  let Some((attrs, is_html)) = node_attrs_mut(node) else {
    return false;
  };

  if let Some((_, val)) = attrs
    .iter_mut()
    .find(|(k, _)| name_matches(k.as_str(), name, is_html))
  {
    if val == value {
      return false;
    }
    val.clear();
    val.push_str(value);
    return true;
  }

  attrs.push((name.to_string(), value.to_string()));
  true
}

pub fn remove_attr(node: &mut DomNode, name: &str) -> bool {
  let Some((attrs, is_html)) = node_attrs_mut(node) else {
    return false;
  };

  if let Some(idx) = attrs
    .iter()
    .position(|(k, _)| name_matches(k.as_str(), name, is_html))
  {
    attrs.remove(idx);
    return true;
  }

  false
}

pub fn set_bool_attr(node: &mut DomNode, name: &str, enabled: bool) -> bool {
  if enabled {
    let Some((attrs, is_html)) = node_attrs_mut(node) else {
      return false;
    };
    if attrs
      .iter()
      .any(|(k, _)| name_matches(k.as_str(), name, is_html))
    {
      return false;
    }
    attrs.push((name.to_string(), String::new()));
    true
  } else {
    remove_attr(node, name)
  }
}

pub fn set_hover(node: &mut DomNode, enabled: bool) {
  if enabled {
    let _ = set_attr(node, "data-fastr-hover", "true");
  } else {
    let _ = remove_attr(node, "data-fastr-hover");
  }
}

pub fn set_active(node: &mut DomNode, enabled: bool) {
  if enabled {
    let _ = set_attr(node, "data-fastr-active", "true");
  } else {
    let _ = remove_attr(node, "data-fastr-active");
  }
}

pub fn set_focus(node: &mut DomNode, focused: bool, focus_visible: bool) {
  if focused {
    let _ = set_attr(node, "data-fastr-focus", "true");
    if focus_visible {
      let _ = set_attr(node, "data-fastr-focus-visible", "true");
    } else {
      let _ = remove_attr(node, "data-fastr-focus-visible");
    }
  } else {
    let _ = remove_attr(node, "data-fastr-focus");
    let _ = remove_attr(node, "data-fastr-focus-visible");
  }
}

pub fn set_visited(node: &mut DomNode, enabled: bool) {
  if enabled {
    let _ = set_attr(node, "data-fastr-visited", "true");
  } else {
    let _ = remove_attr(node, "data-fastr-visited");
  }
}

pub fn mark_user_validity(node: &mut DomNode) -> bool {
  set_attr(node, "data-fastr-user-validity", "true")
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn is_form_element(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|t| t.eq_ignore_ascii_case("form") && is_html_element(node))
}

pub fn mark_form_user_validity(root: &mut DomNode, control_node_id: usize) -> bool {
  let mut index = DomIndex::build(root);

  let form_attr = index
    .with_node_mut(control_node_id, |control| {
      control
        .is_element()
        .then(|| control.get_attribute_ref("form").map(trim_ascii_whitespace))
        .flatten()
        .filter(|v| !v.is_empty())
        .map(str::to_string)
    })
    .flatten();

  let mut form_owner_id = None;

  if let Some(form_attr) = form_attr.as_deref() {
    if let Some(id) = index.id_by_element_id.get(form_attr).copied() {
      if index
        .with_node_mut(id, |node| is_form_element(node))
        .unwrap_or(false)
      {
        form_owner_id = Some(id);
      }
    }
  }

  if form_owner_id.is_none() {
    let mut current = control_node_id;
    while current != 0 {
      current = *index.parent.get(current).unwrap_or(&0);
      if current == 0 {
        break;
      }
      if index
        .with_node_mut(current, |node| is_form_element(node))
        .unwrap_or(false)
      {
        form_owner_id = Some(current);
        break;
      }
    }
  }

  let Some(form_id) = form_owner_id else {
    return false;
  };

  index
    .with_node_mut(form_id, |form| mark_user_validity(form))
    .unwrap_or(false)
}

pub fn toggle_checkbox(node: &mut DomNode) -> bool {
  if !is_input_of_type(node, "checkbox") {
    return false;
  }
  if is_disabled_or_inert(node) {
    return false;
  }

  let was_checked = node.get_attribute_ref("checked").is_some();
  let mut changed = set_bool_attr(node, "checked", !was_checked);

  changed |= remove_attr(node, "indeterminate");

  if node
    .get_attribute_ref("aria-checked")
    .is_some_and(|v| v.eq_ignore_ascii_case("mixed"))
  {
    changed |= remove_attr(node, "aria-checked");
  }

  if changed {
    changed |= mark_user_validity(node);
  }

  changed
}

fn parse_finite_number(value: &str) -> Option<f64> {
  trim_ascii_whitespace(value)
    .parse::<f64>()
    .ok()
    .filter(|v| v.is_finite())
}

pub fn set_range_value_from_ratio(node: &mut DomNode, ratio: f32) -> bool {
  if !is_input_of_type(node, "range") {
    return false;
  }

  if is_disabled_or_inert(node) || node.get_attribute_ref("readonly").is_some() {
    return false;
  }

  if !ratio.is_finite() {
    return false;
  }

  let Some((min, max)) = input_range_bounds(node) else {
    return false;
  };

  let ratio = (ratio as f64).clamp(0.0, 1.0);
  let resolved = min + (max - min) * ratio;
  let clamped = resolved.clamp(min, max);

  let step_attr = node.get_attribute_ref("step");
  let value = if matches!(
    step_attr,
    Some(step) if trim_ascii_whitespace(step).eq_ignore_ascii_case("any")
  ) {
    clamped
  } else {
    let step = step_attr
      .and_then(parse_finite_number)
      .filter(|step| *step > 0.0)
      .unwrap_or(1.0);

    // The allowed value step base for range inputs is the minimum value (defaulting to zero).
    let step_base = min;
    let steps_to_value = ((clamped - step_base) / step).round();
    let mut aligned = step_base + steps_to_value * step;

    let max_aligned = step_base + ((max - step_base) / step).floor() * step;
    if aligned > max_aligned {
      aligned = max_aligned;
    }
    if aligned < step_base {
      aligned = step_base;
    }

    aligned.clamp(min, max)
  };

  let value_attr = format_number(value);
  let changed_value = set_attr(node, "value", &value_attr);
  let mut changed = changed_value;
  if changed_value {
    changed |= mark_user_validity(node);
  }
  changed
}

struct PreorderFrame {
  ptr: *mut DomNode,
  next_child: usize,
  node_id: usize,
}

fn find_node_ptr_with_ancestors_by_preorder_id(
  root: &mut DomNode,
  target_id: usize,
) -> Option<(*mut DomNode, Vec<*mut DomNode>)> {
  if target_id == 0 {
    return None;
  }

  // Depth-first pre-order traversal, matching `crate::dom::enumerate_dom_ids`.
  let mut stack: Vec<PreorderFrame> = Vec::new();
  stack.push(PreorderFrame {
    ptr: root as *mut DomNode,
    next_child: 0,
    node_id: 1,
  });
  let mut next_id = 2usize;

  while !stack.is_empty() {
    let last_idx = stack.len() - 1;

    if stack[last_idx].node_id == target_id {
      let ptr = stack[last_idx].ptr;
      let ancestors = stack[..last_idx].iter().map(|f| f.ptr).collect::<Vec<_>>();
      return Some((ptr, ancestors));
    }

    let node_ptr = stack[last_idx].ptr;
    let next_child = stack[last_idx].next_child;

    // Safety: pointers are into `root`'s tree and we do not mutate any `children` vectors during
    // this traversal.
    let node = unsafe { &mut *node_ptr };
    if next_child < node.children.len() {
      // Safety: `next_child` is in bounds.
      let child_ptr = unsafe { node.children.as_mut_ptr().add(next_child) };
      stack[last_idx].next_child = next_child + 1;
      let node_id = next_id;
      next_id = next_id.saturating_add(1);
      stack.push(PreorderFrame {
        ptr: child_ptr,
        next_child: 0,
        node_id,
      });
    } else {
      stack.pop();
    }
  }

  None
}

fn is_disabled_or_inert_with_ancestors(target: &DomNode, ancestors: &[*mut DomNode]) -> bool {
  if is_disabled_or_inert(target) {
    return true;
  }
  for ancestor_ptr in ancestors {
    // Safety: pointers are into the same DOM tree; only read access.
    let ancestor = unsafe { &**ancestor_ptr };
    if is_disabled_or_inert(ancestor) {
      return true;
    }
  }
  false
}

fn radio_group_root_ptr(target_ptr: *mut DomNode, ancestors: &[*mut DomNode]) -> *mut DomNode {
  // Radio group membership is scoped to the nearest ancestor <form> within the current tree root.
  // Shadow roots act as tree-root boundaries, so radios inside shadow trees never group with
  // light-DOM radios, even if the shadow host is itself inside a <form>.
  for &ancestor_ptr in ancestors.iter().rev() {
    // Safety: only read access.
    let ancestor = unsafe { &*ancestor_ptr };
    if ancestor
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("form"))
    {
      return ancestor_ptr;
    }
    if matches!(
      ancestor.node_type,
      DomNodeType::Document { .. } | DomNodeType::ShadowRoot { .. }
    ) {
      return ancestor_ptr;
    }
  }

  // Fallback for incomplete ancestor chains (e.g. unit tests constructing partial DOM trees).
  ancestors.first().copied().unwrap_or(target_ptr)
}

fn clear_checked_in_radio_group(
  group_root_ptr: *mut DomNode,
  target_ptr: *mut DomNode,
  group_name: &str,
) -> bool {
  let mut changed = false;

  struct ScanFrame {
    ptr: *mut DomNode,
    blocked: bool,
  }

  let mut stack: Vec<ScanFrame> = Vec::new();
  stack.push(ScanFrame {
    ptr: group_root_ptr,
    blocked: false,
  });

  while let Some(frame) = stack.pop() {
    // Safety: pointers are into the same DOM tree and we never mutate `children` vectors while
    // iterating.
    let node_ptr = frame.ptr;
    let node = unsafe { &mut *frame.ptr };
    let blocked = frame.blocked || is_disabled_or_inert(node);

    if is_input_of_type(node, "radio")
      && node_ptr != target_ptr
      && node.get_attribute_ref("name") == Some(group_name)
    {
      if !blocked {
        changed |= remove_attr(node, "checked");
      }
    }

    // Forms and shadow roots are group boundaries:
    // - Controls in different <form> elements are never in the same group.
    // - Shadow roots define independent trees; do not traverse into shadow DOM when scanning a
    //   light DOM radio group (or vice-versa).
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("form"))
      && node_ptr != group_root_ptr
    {
      continue;
    }

    if node.is_template_element() {
      continue;
    }

    let len = node.children.len();
    let children_ptr = node.children.as_mut_ptr();
    for idx in (0..len).rev() {
      // Safety: `idx` is in bounds.
      let child_ptr = unsafe { children_ptr.add(idx) };
      // Safety: read access to inspect node type before pushing.
      let child = unsafe { &*child_ptr };
      if matches!(child.node_type, DomNodeType::ShadowRoot { .. }) {
        continue;
      }
      stack.push(ScanFrame {
        ptr: child_ptr,
        blocked,
      });
    }
  }

  changed
}

pub fn activate_radio(root: &mut DomNode, radio_node_id: usize) -> bool {
  let Some((target_ptr, ancestors)) =
    find_node_ptr_with_ancestors_by_preorder_id(root, radio_node_id)
  else {
    return false;
  };

  // Safety: pointer is within `root` and we only borrow it mutably for this scope.
  let target = unsafe { &mut *target_ptr };

  if !is_input_of_type(target, "radio") {
    return false;
  }

  if is_disabled_or_inert_with_ancestors(target, &ancestors) {
    return false;
  }

  let group_name = radio_group_name(target).map(str::to_string);
  let group_root_ptr = radio_group_root_ptr(target_ptr, &ancestors);

  let mut changed = set_bool_attr(target, "checked", true);

  let Some(group_name) = group_name else {
    return changed;
  };

  changed |= clear_checked_in_radio_group(group_root_ptr, target_ptr, &group_name);

  if changed {
    changed |= mark_user_validity(target);
  }

  changed
}

pub fn append_text_to_input(node: &mut DomNode, text: &str) -> bool {
  if text.is_empty() {
    return false;
  }
  if !is_text_like_input(node) {
    return false;
  }
  if is_disabled_or_inert(node) {
    return false;
  }

  let changed = {
    let Some((attrs, is_html)) = node_attrs_mut(node) else {
      return false;
    };

    if let Some((_, val)) = attrs
      .iter_mut()
      .find(|(k, _)| name_matches(k.as_str(), "value", is_html))
    {
      val.push_str(text);
      true
    } else {
      attrs.push(("value".to_string(), text.to_string()));
      true
    }
  };

  if changed {
    let _ = mark_user_validity(node);
  }

  changed
}

pub fn backspace_input(node: &mut DomNode) -> bool {
  if !is_text_like_input(node) {
    return false;
  }
  if is_disabled_or_inert(node) {
    return false;
  }

  let Some((attrs, is_html)) = node_attrs_mut(node) else {
    return false;
  };

  let changed = attrs
    .iter_mut()
    .find(|(k, _)| name_matches(k.as_str(), "value", is_html))
    .and_then(|(_, val)| val.pop())
    .is_some();

  if changed {
    let _ = mark_user_validity(node);
  }

  changed
}

pub fn append_text_to_textarea(node: &mut DomNode, text: &str) -> bool {
  if text.is_empty() {
    return false;
  }
  if !node
    .tag_name()
    .is_some_and(|t| t.eq_ignore_ascii_case("textarea") && is_html_element(node))
  {
    return false;
  }
  if is_disabled_or_inert(node) {
    return false;
  }

  let changed = if let Some(last_text) = node.children.iter_mut().rev().find_map(|child| {
    if let DomNodeType::Text { content } = &mut child.node_type {
      Some(content)
    } else {
      None
    }
  }) {
    last_text.push_str(text);
    true
  } else {
    node.children.push(DomNode {
      node_type: DomNodeType::Text {
        content: text.to_string(),
      },
      children: Vec::new(),
    });
    true
  };

  if changed {
    let _ = mark_user_validity(node);
  }

  changed
}

pub fn backspace_textarea(node: &mut DomNode) -> bool {
  if !node
    .tag_name()
    .is_some_and(|t| t.eq_ignore_ascii_case("textarea") && is_html_element(node))
  {
    return false;
  }
  if is_disabled_or_inert(node) {
    return false;
  }

  let mut changed = false;
  for child in node.children.iter_mut().rev() {
    if let DomNodeType::Text { content } = &mut child.node_type {
      if content.pop().is_some() {
        changed = true;
        break;
      }
    }
  }

  if changed {
    let _ = mark_user_validity(node);
  }

  changed
}

/// Activate/select an `<option>` descendant of a `<select>` element.
///
/// Returns `true` iff any DOM attributes were changed.
pub fn activate_select_option(
  root: &mut DomNode,
  select_node_id: usize,
  option_node_id: usize,
  toggle_for_multiple: bool,
) -> bool {
  let mut index = DomIndex::build(root);

  let Some((select_ok, select_multiple)) = index.with_node_mut(select_node_id, |node| {
    let is_select = node
      .tag_name()
      .is_some_and(|t| t.eq_ignore_ascii_case("select") && is_html_element(node));
    if !is_select {
      return (false, false);
    }
    if is_disabled_or_inert(node) {
      return (false, false);
    }
    (true, node.get_attribute_ref("multiple").is_some())
  }) else {
    return false;
  };
  if !select_ok {
    return false;
  }

  let Some((option_ok, option_selected, option_ptr)) = index.with_node_mut(option_node_id, |node| {
    let is_option = node
      .tag_name()
      .is_some_and(|t| t.eq_ignore_ascii_case("option") && is_html_element(node));
    if !is_option {
      return (false, false, std::ptr::null_mut());
    }
    if node.get_attribute_ref("disabled").is_some() {
      return (false, false, std::ptr::null_mut());
    }
    (true, node.get_attribute_ref("selected").is_some(), node as *mut DomNode)
  }) else {
    return false;
  };
  if !option_ok {
    return false;
  }

  // Verify `option` is a descendant of `select` and that no disabled `<optgroup>` exists between
  // them.
  let mut parent = index.parent.get(option_node_id).copied().unwrap_or(0);
  let mut found_select = false;
  while parent != 0 {
    if parent == select_node_id {
      found_select = true;
      break;
    }

    let disabled_optgroup = index
      .with_node_mut(parent, |node| {
        node
          .tag_name()
          .is_some_and(|t| t.eq_ignore_ascii_case("optgroup") && is_html_element(node))
          && node.get_attribute_ref("disabled").is_some()
      })
      .unwrap_or(false);
    if disabled_optgroup {
      return false;
    }

    parent = index.parent.get(parent).copied().unwrap_or(0);
  }
  if !found_select {
    return false;
  }

  let mut changed = if select_multiple && toggle_for_multiple {
    // Multiple-select toggle.
    index
      .with_node_mut(option_node_id, |node| set_bool_attr(node, "selected", !option_selected))
      .unwrap_or(false)
  } else {
    // Replacement selection (single-select and non-toggle multiple-select).
    if !select_multiple && option_selected {
      // Spec-ish: activating an already-selected option in single-select is a no-op.
      return false;
    }

    // Clear selected state from all other `<option>` descendants of this `<select>` (including under
    // optgroups).
    let mut changed = index
      .with_node_mut(select_node_id, |select| {
        // Avoid recursion for deeply nested `<optgroup>` trees.
        let mut changed = false;
        let mut stack: Vec<*mut DomNode> = vec![select as *mut DomNode];
        while let Some(ptr) = stack.pop() {
          // Safety: `select` is mutably borrowed for the duration of this traversal, and we never
          // mutate `children` vectors (only element attributes), so raw pointers remain stable.
          let current = unsafe { &mut *ptr };

          if current.is_template_element() {
            continue;
          }
          if ptr != option_ptr
            && current
              .tag_name()
              .is_some_and(|t| t.eq_ignore_ascii_case("option") && is_html_element(current))
          {
            changed |= remove_attr(current, "selected");
          }

          for child in current.children.iter_mut().rev() {
            stack.push(child as *mut DomNode);
          }
        }
        changed
      })
      .unwrap_or(false);

    // Ensure the activated option is selected.
    changed |= index
      .with_node_mut(option_node_id, |node| set_bool_attr(node, "selected", true))
      .unwrap_or(false);

    changed
  };

  if changed {
    changed |= index
      .with_node_mut(select_node_id, |node| mark_user_validity(node))
      .unwrap_or(false);
  }

  changed
}
