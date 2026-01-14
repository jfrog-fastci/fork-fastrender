use crate::dom2::{Document, NodeId, NodeKind};

fn direct_children(dom: &Document, parent: NodeId) -> Vec<NodeId> {
  dom
    .node(parent)
    .children
    .iter()
    .copied()
    .filter(|child| dom.node(*child).parent == Some(parent))
    .collect()
}

/// Replace an element's `textContent` by HTML element id.
///
/// Returns `true` if the DOM was modified.
pub fn set_text_by_element_id(dom: &mut Document, element_id: &str, text: &str) -> bool {
  let Some(element) = dom.get_element_by_id(element_id) else {
    return false;
  };

  let children = direct_children(dom, element);
  if children.len() == 1 {
    let child = children[0];
    if let NodeKind::Text { content } = &dom.node(child).kind {
      if content == text {
        return false;
      }
      return dom.set_text_data(child, text).unwrap_or(false);
    }
  }

  // Fallback: replace children with a single text node. Unlike the spec, we intentionally keep an
  // empty Text node when `text` is empty so subsequent updates do not allocate unbounded detached
  // nodes (dom2 nodes are not GC'd).
  let mut changed = false;
  for child in children {
    if dom.remove_child(element, child).unwrap_or(false) {
      changed = true;
    }
  }

  let text_node = dom.create_text(text);
  if dom.append_child(element, text_node).unwrap_or(false) {
    changed = true;
  }
  changed
}

/// Set (or remove) an attribute by HTML element id.
///
/// - When `value` is `Some(v)`, sets `name="v"`.
/// - When `value` is `None`, removes the attribute.
///
/// Returns `true` if the DOM was modified.
pub fn set_attribute_by_element_id(
  dom: &mut Document,
  element_id: &str,
  name: &str,
  value: Option<&str>,
) -> bool {
  let Some(node) = dom.get_element_by_id(element_id) else {
    return false;
  };

  match value {
    Some(value) => dom.set_attribute(node, name, value).unwrap_or(false),
    None => dom.remove_attribute(node, name).unwrap_or(false),
  }
}

/// Add/remove a class token in an element's `class` attribute by HTML element id.
///
/// Returns `true` if the DOM was modified.
pub fn toggle_class_by_element_id(
  dom: &mut Document,
  element_id: &str,
  class: &str,
  enabled: bool,
) -> bool {
  debug_assert!(
    !class.trim().is_empty() && class.split_ascii_whitespace().count() == 1,
    "toggle_class_by_element_id expects a single class token"
  );

  let Some(node) = dom.get_element_by_id(element_id) else {
    return false;
  };

  let existing = dom
    .get_attribute(node, "class")
    .ok()
    .flatten()
    .unwrap_or("");
  let mut classes: Vec<&str> = existing.split_ascii_whitespace().collect();
  let already_present = classes.iter().any(|c| *c == class);

  if enabled {
    if already_present {
      return false;
    }
    classes.push(class);
    return dom
      .set_attribute(node, "class", &classes.join(" "))
      .unwrap_or(false);
  }

  if !already_present {
    return false;
  }

  classes.retain(|c| *c != class);
  if classes.is_empty() {
    dom.remove_attribute(node, "class").unwrap_or(false)
  } else {
    dom
      .set_attribute(node, "class", &classes.join(" "))
      .unwrap_or(false)
  }
}
