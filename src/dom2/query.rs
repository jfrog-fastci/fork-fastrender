use super::{Document, NodeId, NodeKind};

impl Document {
  pub fn document_element(&self) -> Option<NodeId> {
    let root = self.root();
    let node = self.nodes.get(root.index())?;
    node.children.iter().copied().find(|&child| {
      self
        .nodes
        .get(child.index())
        .is_some_and(|node| matches!(node.kind, NodeKind::Element { .. } | NodeKind::Slot { .. }))
    })
  }

  pub fn get_element_by_id(&self, id: &str) -> Option<NodeId> {
    if id.is_empty() {
      return None;
    }

    for node_id in self.subtree_preorder(self.root()) {
      let Some(node) = self.nodes.get(node_id.index()) else {
        continue;
      };
      let attributes = match &node.kind {
        NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
        _ => continue,
      };
      if attributes
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("id") && value == id)
      {
        return Some(node_id);
      }
    }

    None
  }
}

