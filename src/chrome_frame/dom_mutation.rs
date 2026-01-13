use crate::dom::{DomNode, DomNodeType};

pub use crate::interaction::dom_mutation::{remove_attr, set_attr, set_bool_attr};

/// Depth-first search for an element with `id=...`.
pub fn find_element_by_id_mut<'a>(node: &'a mut DomNode, id: &str) -> Option<&'a mut DomNode> {
  if node
    .get_attribute_ref("id")
    .is_some_and(|value| value == id)
  {
    return Some(node);
  }
  for child in node.children.iter_mut() {
    if let Some(found) = find_element_by_id_mut(child, id) {
      return Some(found);
    }
  }
  None
}

/// Replace an element's text content with `text`.
///
/// This is intentionally small/specialized for chrome-frame UI: most nodes are simple spans with a
/// single text child.
pub fn set_text_content(node: &mut DomNode, text: &str) -> bool {
  match &mut node.node_type {
    DomNodeType::Text { content } => {
      if content == text {
        return false;
      }
      content.clear();
      content.push_str(text);
      true
    }
    DomNodeType::Element { .. } | DomNodeType::Slot { .. } => {
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

      // Normalize to a single text child for determinism.
      let changed = !(node.children.len() == 1
        && matches!(node.children[0].node_type, DomNodeType::Text { .. })
        && node.children[0]
          .as_text()
          .is_some_and(|existing| existing == text));
      node.children.clear();
      node.children.push(DomNode {
        node_type: DomNodeType::Text {
          content: text.to_string(),
        },
        children: Vec::new(),
      });
      changed
    }
    _ => false,
  }
}

trait DomNodeExt {
  fn as_text(&self) -> Option<&str>;
}

impl DomNodeExt for DomNode {
  fn as_text(&self) -> Option<&str> {
    match &self.node_type {
      DomNodeType::Text { content } => Some(content.as_str()),
      _ => None,
    }
  }
}
