use crate::dom::{DomNode, DomNodeType};

fn find_by_element_id_mut<'a>(root: &'a mut DomNode, element_id: &str) -> Option<&'a mut DomNode> {
  // Avoid recursion for very deep trees.
  let mut stack: Vec<*mut DomNode> = vec![root as *mut DomNode];
  while let Some(ptr) = stack.pop() {
    // SAFETY: `DomNode` values are never moved while they are reachable via `root`. We do not
    // mutate `children` vectors while pointers to their elements are on `stack` (we only mutate the
    // final returned node, after exiting the loop).
    let node = unsafe { &mut *ptr };

    if node.get_attribute_ref("id") == Some(element_id) {
      return Some(node);
    }

    // Traverse in a deterministic pre-order (left-to-right).
    for child in node.children.iter_mut().rev() {
      stack.push(child as *mut DomNode);
    }
  }

  None
}

/// Replace an element's `textContent` by HTML element id.
///
/// Returns `true` if the DOM was modified.
pub fn set_text_by_element_id(dom: &mut DomNode, element_id: &str, text: &str) -> bool {
  let Some(node) = find_by_element_id_mut(dom, element_id) else {
    return false;
  };

  // Mirror `Node.textContent` semantics:
  // - setting to "" removes all children
  // - otherwise replace children with a single text node
  if text.is_empty() {
    if node.children.is_empty() {
      return false;
    }
    node.children.clear();
    return true;
  }

  if node.children.len() == 1 {
    if let DomNodeType::Text { content } = &mut node.children[0].node_type {
      if content == text {
        return false;
      }
      content.clear();
      content.push_str(text);
      return true;
    }
  }

  node.children.clear();
  node.children.push(DomNode {
    node_type: DomNodeType::Text {
      content: text.to_string(),
    },
    children: Vec::new(),
  });
  true
}

/// Set (or remove) an attribute by HTML element id.
///
/// - When `value` is `Some(v)`, sets `name="v"`.
/// - When `value` is `None`, removes the attribute.
///
/// Returns `true` if the DOM was modified.
pub fn set_attribute_by_element_id(
  dom: &mut DomNode,
  element_id: &str,
  name: &str,
  value: Option<&str>,
) -> bool {
  let Some(node) = find_by_element_id_mut(dom, element_id) else {
    return false;
  };

  match value {
    Some(value) => {
      if node.get_attribute_ref(name) == Some(value) {
        return false;
      }
      node.set_attribute(name, value);
      true
    }
    None => {
      if node.get_attribute_ref(name).is_none() {
        return false;
      }
      node.remove_attribute(name);
      true
    }
  }
}

/// Add/remove a class token in an element's `class` attribute by HTML element id.
///
/// Returns `true` if the DOM was modified.
pub fn toggle_class_by_element_id(
  dom: &mut DomNode,
  element_id: &str,
  class: &str,
  enabled: bool,
) -> bool {
  let Some(node) = find_by_element_id_mut(dom, element_id) else {
    return false;
  };

  let existing = node.get_attribute_ref("class").unwrap_or("");
  let mut classes: Vec<&str> = existing.split_ascii_whitespace().collect();
  let already_present = classes.iter().any(|c| *c == class);

  if enabled {
    if already_present {
      return false;
    }
    classes.push(class);
    node.set_attribute("class", &classes.join(" "));
    return true;
  }

  if !already_present {
    return false;
  }

  classes.retain(|c| *c != class);
  if classes.is_empty() {
    node.remove_attribute("class");
  } else {
    node.set_attribute("class", &classes.join(" "));
  }
  true
}
