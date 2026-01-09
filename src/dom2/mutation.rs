use crate::dom::HTML_NAMESPACE;

use super::DomError;
use super::{Document, NodeId, NodeKind};

impl Document {
  fn node_checked(&self, id: NodeId) -> Result<&super::Node, DomError> {
    self
      .nodes
      .get(id.index())
      .ok_or(DomError::NotFoundError)
  }

  fn validate_insert_hierarchy(&self, parent: NodeId, child: NodeId) -> Result<(), DomError> {
    // NodeId validation is performed by callers, but keep this self-contained for internal use.
    let parent_kind = &self.node_checked(parent)?.kind;
    let child_kind = &self.node_checked(child)?.kind;

    // The document root cannot be inserted anywhere.
    if child == self.root() {
      return Err(DomError::HierarchyRequestError);
    }

    // Non-root `Document` nodes should never exist.
    if matches!(child_kind, NodeKind::Document { .. }) {
      return Err(DomError::InvalidNodeType);
    }

    // Leaf nodes cannot accept children.
    if matches!(parent_kind, NodeKind::Text { .. }) {
      return Err(DomError::HierarchyRequestError);
    }

    match child_kind {
      NodeKind::ShadowRoot { .. } => match parent_kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
        _ => return Err(DomError::HierarchyRequestError),
      },
      NodeKind::Slot { .. } => match parent_kind {
        NodeKind::Element { .. } => {}
        _ => return Err(DomError::HierarchyRequestError),
      },
      _ => {}
    }

    Ok(())
  }

  fn validate_no_cycles(&self, parent: NodeId, child: NodeId) -> Result<(), DomError> {
    if parent == child {
      return Err(DomError::HierarchyRequestError);
    }

    // A leaf node (no children) cannot be an ancestor of `parent` unless `parent == child` which is
    // handled above. This fast path keeps common insertions O(1) on deep trees.
    if self.node_checked(child)?.children.is_empty() {
      return Ok(());
    }

    let mut current = Some(parent);
    while let Some(id) = current {
      if id == child {
        return Err(DomError::HierarchyRequestError);
      }
      current = self.node_checked(id)?.parent;
    }

    Ok(())
  }

  fn index_of_child_internal(
    &self,
    parent: NodeId,
    child: NodeId,
  ) -> Result<Option<usize>, DomError> {
    self.node_checked(parent)?;
    self.node_checked(child)?;
    Ok(
      self.nodes[parent.index()]
        .children
        .iter()
        .position(|&c| c == child),
    )
  }

  fn detach_from_parent(&mut self, child: NodeId) -> Result<Option<NodeId>, DomError> {
    self.node_checked(child)?;
    let Some(old_parent) = self.nodes[child.index()].parent else {
      return Ok(None);
    };

    self.node_checked(old_parent)?;
    let pos = self.nodes[old_parent.index()]
      .children
      .iter()
      .position(|&c| c == child)
      .ok_or(DomError::NotFoundError)?;
    self.nodes[old_parent.index()].children.remove(pos);
    self.nodes[child.index()].parent = None;
    Ok(Some(old_parent))
  }

  pub fn create_element(&mut self, tag_name: &str, namespace: &str) -> NodeId {
    let is_html_ns = namespace.is_empty() || namespace == HTML_NAMESPACE;
    // Normalise HTML namespace to the empty string, matching the renderer DOM representation.
    let namespace = if namespace == HTML_NAMESPACE {
      ""
    } else {
      namespace
    };

    let inert_subtree = tag_name.eq_ignore_ascii_case("template");
    let kind = if is_html_ns && tag_name.eq_ignore_ascii_case("slot") {
      NodeKind::Slot {
        namespace: namespace.to_string(),
        attributes: Vec::new(),
        assigned: false,
      }
    } else {
      NodeKind::Element {
        tag_name: tag_name.to_string(),
        namespace: namespace.to_string(),
        attributes: Vec::new(),
      }
    };

    self.push_node(kind, None, inert_subtree)
  }

  pub fn create_text(&mut self, data: &str) -> NodeId {
    self.push_node(
      NodeKind::Text {
        content: data.to_string(),
      },
      None,
      /* inert_subtree */ false,
    )
  }

  pub fn text_data(&self, node: NodeId) -> Result<&str, DomError> {
    let node = self.node_checked(node)?;
    match &node.kind {
      NodeKind::Text { content } => Ok(content.as_str()),
      _ => Err(DomError::InvalidNodeType),
    }
  }

  pub fn set_text_data(&mut self, node: NodeId, data: &str) -> Result<bool, DomError> {
    let node = self
      .nodes
      .get_mut(node.index())
      .ok_or(DomError::NotFoundError)?;
    match &mut node.kind {
      NodeKind::Text { content } => {
        if content == data {
          return Ok(false);
        }
        content.clear();
        content.push_str(data);
        Ok(true)
      }
      _ => Err(DomError::InvalidNodeType),
    }
  }

  pub fn parent(&self, node: NodeId) -> Result<Option<NodeId>, DomError> {
    Ok(self.node_checked(node)?.parent)
  }

  pub fn children(&self, node: NodeId) -> Result<&[NodeId], DomError> {
    Ok(self.node_checked(node)?.children.as_slice())
  }

  pub fn index_of_child(
    &self,
    parent: NodeId,
    child: NodeId,
  ) -> Result<Option<usize>, DomError> {
    self.index_of_child_internal(parent, child)
  }

  pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<bool, DomError> {
    self.insert_before(parent, child, None)
  }

  pub fn insert_before(
    &mut self,
    parent: NodeId,
    new_child: NodeId,
    reference: Option<NodeId>,
  ) -> Result<bool, DomError> {
    self.node_checked(parent)?;
    self.node_checked(new_child)?;
    if let Some(reference) = reference {
      self.node_checked(reference)?;
    }

    self.validate_insert_hierarchy(parent, new_child)?;
    self.validate_no_cycles(parent, new_child)?;

    let mut insertion_idx = match reference {
      Some(reference) => self
        .index_of_child_internal(parent, reference)?
        .ok_or(DomError::NotFoundError)?,
      None => self.nodes[parent.index()].children.len(),
    };

    let current_parent = self.nodes[new_child.index()].parent;

    if current_parent == Some(parent) {
      // Move within the same parent.
      let current_idx = self
        .index_of_child_internal(parent, new_child)?
        .ok_or(DomError::NotFoundError)?;

      // If the node is being removed from a position before the insertion point, the insertion
      // index shifts left by one.
      if current_idx < insertion_idx {
        insertion_idx -= 1;
      }

      if current_idx == insertion_idx {
        return Ok(false);
      }

      self.nodes[parent.index()].children.remove(current_idx);
      self.nodes[parent.index()].children.insert(insertion_idx, new_child);
      return Ok(true);
    }

    if current_parent.is_some() {
      self.detach_from_parent(new_child)?;
    }

    self.nodes[parent.index()]
      .children
      .insert(insertion_idx, new_child);
    self.nodes[new_child.index()].parent = Some(parent);
    Ok(true)
  }

  pub fn remove_child(&mut self, parent: NodeId, child: NodeId) -> Result<bool, DomError> {
    self.node_checked(parent)?;
    self.node_checked(child)?;

    if self.nodes[child.index()].parent != Some(parent) {
      return Err(DomError::NotFoundError);
    }
    let idx = self
      .index_of_child_internal(parent, child)?
      .ok_or(DomError::NotFoundError)?;
    self.nodes[parent.index()].children.remove(idx);
    self.nodes[child.index()].parent = None;
    Ok(true)
  }

  pub fn replace_child(
    &mut self,
    parent: NodeId,
    new_child: NodeId,
    old_child: NodeId,
  ) -> Result<bool, DomError> {
    self.node_checked(parent)?;
    self.node_checked(new_child)?;
    self.node_checked(old_child)?;

    if new_child == old_child {
      return Ok(false);
    }

    self.validate_insert_hierarchy(parent, new_child)?;
    self.validate_no_cycles(parent, new_child)?;

    // Ensure `old_child` is actually a child of `parent`.
    if self.nodes[old_child.index()].parent != Some(parent) {
      return Err(DomError::NotFoundError);
    }
    self
      .index_of_child_internal(parent, old_child)?
      .ok_or(DomError::NotFoundError)?;

    let current_parent = self.nodes[new_child.index()].parent;
    if current_parent == Some(parent) {
      // Remove the existing instance so we can insert at the replacement index.
      let idx = self
        .index_of_child_internal(parent, new_child)?
        .ok_or(DomError::NotFoundError)?;
      self.nodes[parent.index()].children.remove(idx);
    } else if current_parent.is_some() {
      self.detach_from_parent(new_child)?;
    }

    let replacement_idx = self
      .index_of_child_internal(parent, old_child)?
      .ok_or(DomError::NotFoundError)?;
    self.nodes[parent.index()].children.remove(replacement_idx);
    self.nodes[old_child.index()].parent = None;

    self.nodes[parent.index()]
      .children
      .insert(replacement_idx, new_child);
    self.nodes[new_child.index()].parent = Some(parent);
    Ok(true)
  }
}
