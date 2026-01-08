use crate::dom::DomNode;
use crate::dom::DomNodeType;
use crate::dom::HTML_NAMESPACE;

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

  changed
}

pub fn activate_radio(root: &mut DomNode, radio_node_id: usize) -> bool {
  let mut index = DomIndex::build(root);

  let Some((ok, group_name)) = index.with_node_mut(radio_node_id, |node| {
    if !is_input_of_type(node, "radio") {
      return (false, None);
    }
    if is_disabled_or_inert(node) {
      return (false, None);
    }
    (true, node.get_attribute_ref("name").map(str::to_string))
  }) else {
    return false;
  };

  if !ok {
    return false;
  }

  let mut changed = index
    .with_node_mut(radio_node_id, |node| set_bool_attr(node, "checked", true))
    .unwrap_or(false);

  let Some(group_name) = group_name else {
    return changed;
  };

  for id in 1..=index.len() {
    if id == radio_node_id {
      continue;
    }
    changed |= index
      .with_node_mut(id, |node| {
        if !is_input_of_type(node, "radio") {
          return false;
        }
        if node.get_attribute_ref("name") != Some(group_name.as_str()) {
          return false;
        }
        remove_attr(node, "checked")
      })
      .unwrap_or(false);
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

  let Some((attrs, is_html)) = node_attrs_mut(node) else {
    return false;
  };

  if let Some((_, val)) = attrs
    .iter_mut()
    .find(|(k, _)| name_matches(k.as_str(), "value", is_html))
  {
    val.push_str(text);
    return true;
  }

  attrs.push(("value".to_string(), text.to_string()));
  true
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

  let Some((_, val)) = attrs
    .iter_mut()
    .find(|(k, _)| name_matches(k.as_str(), "value", is_html))
  else {
    return false;
  };

  val.pop().is_some()
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

  if let Some(last_text) = node.children.iter_mut().rev().find_map(|child| {
    if let DomNodeType::Text { content } = &mut child.node_type {
      Some(content)
    } else {
      None
    }
  }) {
    last_text.push_str(text);
    return true;
  }

  node.children.push(DomNode {
    node_type: DomNodeType::Text {
      content: text.to_string(),
    },
    children: Vec::new(),
  });
  true
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

  for child in node.children.iter_mut().rev() {
    if let DomNodeType::Text { content } = &mut child.node_type {
      if content.pop().is_some() {
        return true;
      }
    }
  }

  false
}

