use super::{Document, NodeId};

impl Document {
  #[inline]
  fn get_node(&self, id: NodeId) -> Option<&super::Node> {
    self.nodes.get(id.index())
  }

  #[inline]
  fn contains_node(&self, id: NodeId) -> bool {
    id.index() < self.nodes.len()
  }

  pub fn parent_node(&self, node: NodeId) -> Option<NodeId> {
    let parent = self.get_node(node)?.parent?;
    self.contains_node(parent).then_some(parent)
  }

  pub fn first_child(&self, node: NodeId) -> Option<NodeId> {
    let node = self.get_node(node)?;
    node
      .children
      .iter()
      .copied()
      .find(|&child| self.contains_node(child))
  }

  pub fn last_child(&self, node: NodeId) -> Option<NodeId> {
    let node = self.get_node(node)?;
    node
      .children
      .iter()
      .rev()
      .copied()
      .find(|&child| self.contains_node(child))
  }

  pub fn previous_sibling(&self, node: NodeId) -> Option<NodeId> {
    let parent = self.parent_node(node)?;
    let parent_node = self.get_node(parent)?;

    let pos = parent_node.children.iter().position(|&c| c == node)?;
    parent_node
      .children
      .iter()
      .take(pos)
      .rev()
      .copied()
      .find(|&sib| self.contains_node(sib))
  }

  pub fn next_sibling(&self, node: NodeId) -> Option<NodeId> {
    let parent = self.parent_node(node)?;
    let parent_node = self.get_node(parent)?;

    let pos = parent_node.children.iter().position(|&c| c == node)?;
    parent_node
      .children
      .iter()
      .skip(pos + 1)
      .copied()
      .find(|&sib| self.contains_node(sib))
  }

  pub fn is_connected(&self, node: NodeId) -> bool {
    let root = self.root();
    self.ancestors(node).any(|ancestor| ancestor == root)
  }

  pub fn ancestors(&self, node: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    Ancestors {
      doc: self,
      next: self.contains_node(node).then_some(node),
      remaining: self.nodes.len() + 1,
    }
  }

  pub fn subtree_preorder(&self, root: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    SubtreePreorder {
      doc: self,
      stack: self.contains_node(root).then_some(root).into_iter().collect(),
      remaining: self.nodes.len() + 1,
    }
  }
}

struct Ancestors<'a> {
  doc: &'a Document,
  next: Option<NodeId>,
  remaining: usize,
}

impl Iterator for Ancestors<'_> {
  type Item = NodeId;

  fn next(&mut self) -> Option<Self::Item> {
    if self.remaining == 0 {
      self.next = None;
      return None;
    }

    let current = self.next?;
    self.remaining -= 1;
    self.next = self.doc.parent_node(current);
    Some(current)
  }
}

struct SubtreePreorder<'a> {
  doc: &'a Document,
  stack: Vec<NodeId>,
  remaining: usize,
}

impl Iterator for SubtreePreorder<'_> {
  type Item = NodeId;

  fn next(&mut self) -> Option<Self::Item> {
    while let Some(id) = self.stack.pop() {
      if self.remaining == 0 {
        self.stack.clear();
        return None;
      }

      let Some(node) = self.doc.get_node(id) else {
        continue;
      };
      self.remaining -= 1;
      for &child in node.children.iter().rev() {
        if self.doc.contains_node(child) {
          self.stack.push(child);
        }
      }

      return Some(id);
    }
    None
  }
}
