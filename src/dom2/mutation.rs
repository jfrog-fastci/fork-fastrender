use super::error::{DomError, Result};
use super::{Document, NodeId, NodeKind};

fn node_allows_children(kind: &NodeKind) -> bool {
  !matches!(kind, NodeKind::Text { .. })
}

impl Document {
  pub fn create_element(&mut self, tag_name: &str, namespace: &str) -> NodeId {
    let inert_subtree = tag_name.eq_ignore_ascii_case("template");
    self.push_node(
      NodeKind::Element {
        tag_name: tag_name.to_string(),
        namespace: namespace.to_string(),
        attributes: Vec::new(),
      },
      None,
      inert_subtree,
    )
  }

  pub fn create_text_node(&mut self, text: &str) -> NodeId {
    self.push_node(
      NodeKind::Text {
        content: text.to_string(),
      },
      None,
      /* inert_subtree */ false,
    )
  }

  pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<bool> {
    self.insert_before(parent, child, None)
  }

  pub fn insert_before(
    &mut self,
    parent: NodeId,
    new_child: NodeId,
    reference: Option<NodeId>,
  ) -> Result<bool> {
    if new_child == self.root {
      return Err(DomError::HierarchyRequest);
    }

    if !node_allows_children(&self.node(parent).kind) {
      return Err(DomError::HierarchyRequest);
    }

    if let Some(reference_id) = reference {
      if self.node(reference_id).parent != Some(parent) {
        return Err(DomError::HierarchyRequest);
      }
    }

    if parent == new_child {
      return Err(DomError::HierarchyRequest);
    }

    // Cycle check: inserting an ancestor into its descendant would create a loop. We only need to
    // do this walk if `new_child` can be an ancestor at all (i.e. it has children already).
    if !self.node(new_child).children.is_empty() {
      let mut current = Some(parent);
      while let Some(id) = current {
        if id == new_child {
          return Err(DomError::HierarchyRequest);
        }
        current = self.node(id).parent;
      }
    }

    let old_parent = self.node(new_child).parent;

    let parent_children = &self.node(parent).children;
    let mut reference = reference;

    let current_index = if old_parent == Some(parent) {
      let idx = parent_children
        .iter()
        .position(|&id| id == new_child)
        .ok_or(DomError::NotFound)?;

      // DOM spec: if the reference child is the node we're moving, use its next sibling instead.
      if reference == Some(new_child) {
        reference = parent_children.get(idx + 1).copied();
      }

      Some(idx)
    } else {
      None
    };

    let reference_index = match reference {
      Some(reference_id) => parent_children
        .iter()
        .position(|&id| id == reference_id)
        .ok_or(DomError::HierarchyRequest)?,
      None => parent_children.len(),
    };

    let target_index = if let Some(current_index) = current_index {
      if reference_index > current_index {
        reference_index - 1
      } else {
        reference_index
      }
    } else {
      reference_index
    };

    let changed = if let Some(current_index) = current_index {
      target_index != current_index
    } else {
      true
    };

    if !changed {
      return Ok(false);
    }

    if let Some(old_parent_id) = old_parent {
      if old_parent_id == parent {
        let current_index = current_index.expect("missing current_index for parent move");
        self.nodes[parent.0].children.remove(current_index);
      } else {
        let old_idx = self.nodes[old_parent_id.0]
          .children
          .iter()
          .position(|&id| id == new_child)
          .ok_or(DomError::NotFound)?;
        self.nodes[old_parent_id.0].children.remove(old_idx);
      }
      self.nodes[new_child.0].parent = None;
    }

    self.nodes[parent.0].children.insert(target_index, new_child);
    self.nodes[new_child.0].parent = Some(parent);

    Ok(true)
  }

  pub fn remove_child(&mut self, parent: NodeId, child: NodeId) -> Result<bool> {
    if self.node(child).parent != Some(parent) {
      return Err(DomError::NotFound);
    }

    let idx = self.nodes[parent.0]
      .children
      .iter()
      .position(|&id| id == child)
      .ok_or(DomError::NotFound)?;

    self.nodes[parent.0].children.remove(idx);
    self.nodes[child.0].parent = None;
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom::SVG_NAMESPACE;
  use selectors::context::QuirksMode;

  fn assert_parent_child_invariants(doc: &Document) {
    assert!(doc.node(doc.root()).parent.is_none(), "root must be detached");

    for (idx, node) in doc.nodes().iter().enumerate() {
      let id = NodeId(idx);

      if let Some(parent) = node.parent {
        let parent_node = doc.node(parent);
        assert!(
          parent_node.children.contains(&id),
          "node's parent must contain node in child list"
        );
      }

      for &child in &node.children {
        let child_node = doc.node(child);
        assert_eq!(
          child_node.parent,
          Some(id),
          "child must point back to parent"
        );
      }
    }
  }

  #[test]
  fn tree_mutation_maintains_parent_child_invariants() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let root = doc.root();

    let a = doc.create_element("a", "");
    let b = doc.create_element("b", "");
    let c = doc.create_element("c", "");
    let d = doc.create_element("d", "");

    assert!(doc.append_child(root, a).unwrap());
    assert!(doc.append_child(root, b).unwrap());
    assert!(doc.append_child(root, c).unwrap());
    assert_eq!(doc.node(root).children, vec![a, b, c]);

    assert!(doc.insert_before(root, d, Some(b)).unwrap());
    assert_eq!(doc.node(root).children, vec![a, d, b, c]);

    assert!(doc.append_child(root, a).unwrap());
    assert_eq!(doc.node(root).children, vec![d, b, c, a]);

    // No-op insert: `c` is already immediately before `a`.
    assert!(!doc.insert_before(root, c, Some(a)).unwrap());
    // No-op append: `a` is already the last child.
    assert!(!doc.append_child(root, a).unwrap());

    assert!(doc.remove_child(root, b).unwrap());
    assert_eq!(doc.node(root).children, vec![d, c, a]);
    assert!(doc.node(b).parent.is_none(), "removed node must be detached");

    assert!(doc.append_child(a, b).unwrap());
    assert_eq!(doc.node(a).children, vec![b]);

    assert!(doc.append_child(b, d).unwrap());
    assert_eq!(doc.node(root).children, vec![c, a]);
    assert_eq!(doc.node(b).children, vec![d]);

    assert_parent_child_invariants(&doc);
  }

  #[test]
  fn attribute_names_are_case_insensitive_for_html_elements() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let node = doc.create_element("div", "");

    assert!(doc.set_attribute(node, "CLASS", "a").unwrap());
    assert_eq!(doc.get_attribute(node, "class"), Some("a"));

    assert!(!doc.set_attribute(node, "class", "a").unwrap());
    assert!(doc.set_attribute(node, "class", "b").unwrap());
    assert_eq!(doc.get_attribute(node, "CLASS"), Some("b"));

    assert!(doc.remove_attribute(node, "ClAsS").unwrap());
    assert_eq!(doc.get_attribute(node, "class"), None);

    // Non-HTML namespaces should preserve case sensitivity.
    let svg = doc.create_element("svg", SVG_NAMESPACE);
    assert!(doc.set_attribute(svg, "viewBox", "0 0 10 10").unwrap());
    assert_eq!(doc.get_attribute(svg, "viewbox"), None);
  }

  #[test]
  fn deep_tree_mutations_do_not_overflow() {
    // A depth that would almost certainly overflow recursive mutation on typical test stacks.
    const DEPTH: usize = 50_000;

    let mut doc = Document::new(QuirksMode::NoQuirks);
    let root = doc.root();

    let first = doc.create_element("div", "");
    assert!(doc.append_child(root, first).unwrap());

    let mut current = first;
    for _ in 0..DEPTH {
      let next = doc.create_element("div", "");
      assert!(doc.append_child(current, next).unwrap());
      current = next;
    }

    let leaf = doc.create_text_node("leaf");
    assert!(doc.append_child(current, leaf).unwrap());

    // Move the leaf back to the root and then remove it.
    assert!(doc.append_child(root, leaf).unwrap());
    assert!(doc.remove_child(root, leaf).unwrap());

    assert_parent_child_invariants(&doc);
  }

  #[test]
  fn inserting_ancestor_into_descendant_is_hierarchy_error() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let root = doc.root();

    let a = doc.create_element("div", "");
    let b = doc.create_element("div", "");
    let c = doc.create_element("div", "");

    assert!(doc.append_child(root, a).unwrap());
    assert!(doc.append_child(a, b).unwrap());
    assert!(doc.append_child(b, c).unwrap());

    let err = doc.append_child(c, a).unwrap_err();
    assert_eq!(err, DomError::HierarchyRequest);
  }
}
