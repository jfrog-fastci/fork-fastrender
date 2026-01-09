use crate::dom::HTML_NAMESPACE;

use super::DomError;
use super::{Document, NodeId, NodeKind};

impl Document {
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
    if matches!(
      parent_kind,
      NodeKind::Text { .. }
        | NodeKind::Comment { .. }
        | NodeKind::ProcessingInstruction { .. }
        | NodeKind::Doctype { .. }
    ) {
      return Err(DomError::HierarchyRequestError);
    }

    if matches!(parent_kind, NodeKind::Document { .. }) && matches!(child_kind, NodeKind::Text { .. })
    {
      return Err(DomError::HierarchyRequestError);
    }

    match child_kind {
      NodeKind::Doctype { .. } => match parent_kind {
        NodeKind::Document { .. } => {}
        _ => return Err(DomError::HierarchyRequestError),
      },
      NodeKind::ShadowRoot { .. } => match parent_kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
        _ => return Err(DomError::HierarchyRequestError),
      },
      NodeKind::Slot { .. } => match parent_kind {
        NodeKind::Element { .. } | NodeKind::ShadowRoot { .. } | NodeKind::DocumentFragment => {}
        _ => return Err(DomError::HierarchyRequestError),
      },
      _ => {}
    }

    Ok(())
  }

  fn validate_document_insertion(
    &self,
    parent: NodeId,
    new_child: NodeId,
    reference: Option<NodeId>,
    insertion_idx: usize,
  ) -> Result<(), DomError> {
    let parent_kind = &self.node_checked(parent)?.kind;
    if !matches!(parent_kind, NodeKind::Document { .. }) {
      return Ok(());
    }

    fn is_element_child(kind: &NodeKind) -> bool {
      matches!(kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
    }

    let children = self.node_checked(parent)?.children.as_slice();
    let has_element_child = children.iter().any(|&id| {
      self
        .nodes
        .get(id.index())
        .is_some_and(|node| is_element_child(&node.kind))
    });
    let has_doctype_child = children.iter().any(|&id| {
      self
        .nodes
        .get(id.index())
        .is_some_and(|node| matches!(node.kind, NodeKind::Doctype { .. }))
    });

    let new_kind = &self.node_checked(new_child)?.kind;
    match new_kind {
      NodeKind::Element { .. } | NodeKind::Slot { .. } => {
        if has_element_child {
          return Err(DomError::HierarchyRequestError);
        }

        if reference.is_some()
          && children[insertion_idx..].iter().any(|&id| {
            self
              .nodes
              .get(id.index())
              .is_some_and(|node| matches!(node.kind, NodeKind::Doctype { .. }))
          })
        {
          return Err(DomError::HierarchyRequestError);
        }
      }
      NodeKind::Doctype { .. } => {
        if has_doctype_child {
          return Err(DomError::HierarchyRequestError);
        }

        if reference.is_some() {
          if children[..insertion_idx].iter().any(|&id| {
            self
              .nodes
              .get(id.index())
              .is_some_and(|node| is_element_child(&node.kind))
          }) {
            return Err(DomError::HierarchyRequestError);
          }
        } else if has_element_child {
          return Err(DomError::HierarchyRequestError);
        }
      }
      _ => {}
    }

    Ok(())
  }

  fn validate_document_replacement(
    &self,
    parent: NodeId,
    new_child: NodeId,
    old_child: NodeId,
    old_child_idx: usize,
  ) -> Result<(), DomError> {
    let parent_kind = &self.node_checked(parent)?.kind;
    if !matches!(parent_kind, NodeKind::Document { .. }) {
      return Ok(());
    }

    fn is_element_child(kind: &NodeKind) -> bool {
      matches!(kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
    }

    let children = self.node_checked(parent)?.children.as_slice();
    let new_kind = &self.node_checked(new_child)?.kind;

    match new_kind {
      NodeKind::Element { .. } | NodeKind::Slot { .. } => {
        if children.iter().any(|&id| {
          id != old_child
            && self
              .node_checked(id)
              .is_ok_and(|node| is_element_child(&node.kind))
        }) {
          return Err(DomError::HierarchyRequestError);
        }

        if old_child_idx + 1 < children.len()
          && children[old_child_idx + 1..].iter().any(|&id| {
            self
              .node_checked(id)
              .is_ok_and(|node| matches!(node.kind, NodeKind::Doctype { .. }))
          })
        {
          return Err(DomError::HierarchyRequestError);
        }
      }
      NodeKind::Doctype { .. } => {
        if children.iter().any(|&id| {
          id != old_child
            && self
              .node_checked(id)
              .is_ok_and(|node| matches!(node.kind, NodeKind::Doctype { .. }))
        }) {
          return Err(DomError::HierarchyRequestError);
        }

        if children[..old_child_idx].iter().any(|&id| {
          self
            .node_checked(id)
            .is_ok_and(|node| is_element_child(&node.kind))
        }) {
          return Err(DomError::HierarchyRequestError);
        }
      }
      _ => {}
    }

    Ok(())
  }

  fn validate_document_fragment_splice(
    &self,
    parent: NodeId,
    prefix: &[NodeId],
    inserted: &[NodeId],
    suffix: &[NodeId],
  ) -> Result<(), DomError> {
    let parent_kind = &self.node_checked(parent)?.kind;
    if !matches!(parent_kind, NodeKind::Document { .. }) {
      return Ok(());
    }

    let mut element_count = 0usize;
    let mut doctype_count = 0usize;
    let mut first_element_pos: Option<usize> = None;
    let mut first_doctype_pos: Option<usize> = None;

    let mut pos = 0usize;
    for &id in prefix.iter().chain(inserted.iter()).chain(suffix.iter()) {
      let kind = &self.node_checked(id)?.kind;
      match kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => {
          element_count += 1;
          if element_count > 1 {
            return Err(DomError::HierarchyRequestError);
          }
          if first_element_pos.is_none() {
            first_element_pos = Some(pos);
          }
        }
        NodeKind::Doctype { .. } => {
          doctype_count += 1;
          if doctype_count > 1 {
            return Err(DomError::HierarchyRequestError);
          }
          if first_doctype_pos.is_none() {
            first_doctype_pos = Some(pos);
          }
        }
        _ => {}
      }
      pos += 1;
    }

    if let (Some(doctype_pos), Some(element_pos)) = (first_doctype_pos, first_element_pos) {
      if doctype_pos > element_pos {
        return Err(DomError::HierarchyRequestError);
      }
    }

    Ok(())
  }

  fn validate_document_fragment_insertion(
    &self,
    parent: NodeId,
    insertion_idx: usize,
    fragment_children: &[NodeId],
  ) -> Result<(), DomError> {
    let children = self.node_checked(parent)?.children.as_slice();
    let (prefix, suffix) = children.split_at(insertion_idx);
    self.validate_document_fragment_splice(parent, prefix, fragment_children, suffix)
  }

  fn validate_document_fragment_replacement(
    &self,
    parent: NodeId,
    old_child_idx: usize,
    fragment_children: &[NodeId],
  ) -> Result<(), DomError> {
    let children = self.node_checked(parent)?.children.as_slice();
    let prefix = &children[..old_child_idx];
    let suffix = &children[old_child_idx + 1..];
    self.validate_document_fragment_splice(parent, prefix, fragment_children, suffix)
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

  pub fn create_comment(&mut self, data: &str) -> NodeId {
    self.push_node(
      NodeKind::Comment {
        content: data.to_string(),
      },
      None,
      /* inert_subtree */ false,
    )
  }

  pub fn create_document_fragment(&mut self) -> NodeId {
    self.push_node(
      NodeKind::DocumentFragment,
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
    let node = self.node_checked_mut(node)?;
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

    if matches!(self.nodes[new_child.index()].kind, NodeKind::DocumentFragment) {
      // DocumentFragment insertion is transparent: insert its children in order, then empty it.
      // Pre-validate all children before mutating to ensure atomicity.
      let frag_children_len = self.nodes[new_child.index()].children.len();
      for idx in 0..frag_children_len {
        let child = self.nodes[new_child.index()].children[idx];
        self.validate_insert_hierarchy(parent, child)?;
        self.validate_no_cycles(parent, child)?;
      }

      if frag_children_len == 0 {
        return Ok(false);
      }

      self.validate_document_fragment_insertion(
        parent,
        insertion_idx,
        self.nodes[new_child.index()].children.as_slice(),
      )?;

      let children_to_move = std::mem::take(&mut self.nodes[new_child.index()].children);
      // Fragments are always detached.
      self.nodes[new_child.index()].parent = None;

      for &child in &children_to_move {
        self.nodes[child.index()].parent = Some(parent);
      }

      self.nodes[parent.index()]
        .children
        .splice(insertion_idx..insertion_idx, children_to_move);
      return Ok(true);
    }

    self.validate_document_insertion(parent, new_child, reference, insertion_idx)?;

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
    let mut old_child_idx = self
      .index_of_child_internal(parent, old_child)?
      .ok_or(DomError::NotFoundError)?;

    if matches!(self.nodes[new_child.index()].kind, NodeKind::DocumentFragment) {
      // DocumentFragment insertion is transparent: insert its children before `old_child`, then
      // remove `old_child`.
      //
      // Pre-validate all children before mutating to ensure atomicity.
      let frag_children_len = self.nodes[new_child.index()].children.len();
      for idx in 0..frag_children_len {
        let child = self.nodes[new_child.index()].children[idx];
        self.validate_insert_hierarchy(parent, child)?;
        self.validate_no_cycles(parent, child)?;
      }

      self.validate_document_fragment_replacement(
        parent,
        old_child_idx,
        self.nodes[new_child.index()].children.as_slice(),
      )?;

      let children_to_move = std::mem::take(&mut self.nodes[new_child.index()].children);
      self.nodes[new_child.index()].parent = None;

      for &child in &children_to_move {
        self.nodes[child.index()].parent = Some(parent);
      }

      let inserted_len = children_to_move.len();
      self.nodes[parent.index()]
        .children
        .splice(old_child_idx..old_child_idx, children_to_move);

      // `old_child` has been shifted to the right by `inserted_len`.
      self.nodes[parent.index()].children.remove(old_child_idx + inserted_len);
      self.nodes[old_child.index()].parent = None;

      return Ok(true);
    }

    self.validate_document_replacement(parent, new_child, old_child, old_child_idx)?;

    let current_parent = self.nodes[new_child.index()].parent;
    if current_parent == Some(parent) {
      // Remove the existing instance so we can insert at the replacement index.
      let idx = self
        .index_of_child_internal(parent, new_child)?
        .ok_or(DomError::NotFoundError)?;
      self.nodes[parent.index()].children.remove(idx);
      if idx < old_child_idx {
        old_child_idx -= 1;
      }
    } else if current_parent.is_some() {
      self.detach_from_parent(new_child)?;
    }

    self.nodes[parent.index()].children.remove(old_child_idx);
    self.nodes[old_child.index()].parent = None;

    self.nodes[parent.index()]
      .children
      .insert(old_child_idx, new_child);
    self.nodes[new_child.index()].parent = Some(parent);
    Ok(true)
  }
}
