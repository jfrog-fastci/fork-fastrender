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

  /// Returns true when `node` is inside an inert `<template>` subtree.
  ///
  /// FastRender represents template contents by keeping descendants in the tree, but marking the
  /// `<template>` element as `inert_subtree`. The HTML script preparation algorithm must treat such
  /// scripts as "not connected" so they do not execute.
  pub fn is_descendant_of_inert_template(&self, node: NodeId) -> bool {
    // Today, `Node::inert_subtree` is used to represent `<template>` contents. Keep this predicate
    // based on the generic inert flag so future inert subtrees (if any) automatically become
    // disconnected for script preparation.
    self
      .ancestors(node)
      .skip(1)
      .any(|ancestor_id| self.node(ancestor_id).inert_subtree)
  }

  /// Connectedness predicate for `<script>` preparation/execution.
  ///
  /// Returns `false` when:
  /// - `node` is disconnected from the document root, or
  /// - `node` is inside an inert `<template>` subtree.
  pub fn is_connected_for_scripting(&self, node: NodeId) -> bool {
    self.is_connected(node) && !self.is_descendant_of_inert_template(node)
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom::parse_html;
  use crate::dom2::NodeKind;

  #[test]
  fn connected_for_scripting_skips_inert_template_scripts() {
    let root = parse_html(
      r#"<!doctype html>
      <html>
        <body>
          <template><script>inert</script></template>
          <script>live</script>
        </body>
      </html>"#,
    )
    .unwrap();
    let doc = Document::from_renderer_dom(&root);

    let mut inert_script: Option<NodeId> = None;
    let mut live_script: Option<NodeId> = None;

    for (idx, node) in doc.nodes().iter().enumerate() {
      let NodeKind::Element { tag_name, .. } = &node.kind else {
        continue;
      };
      if !tag_name.eq_ignore_ascii_case("script") {
        continue;
      }

      let id = NodeId(idx);
      if doc.is_descendant_of_inert_template(id) {
        inert_script = Some(id);
      } else {
        live_script = Some(id);
      }
    }

    let inert_script = inert_script.expect("expected a <script> inside <template>");
    let live_script = live_script.expect("expected a live <script>");

    assert!(
      !doc.is_connected_for_scripting(inert_script),
      "template script should not be connected for scripting"
    );
    assert!(
      doc.is_connected_for_scripting(live_script),
      "light-DOM script should be connected for scripting"
    );
  }

  #[test]
  fn connected_for_scripting_detects_detached_subtrees() {
    let root = parse_html(r#"<!doctype html><html><body><script>live</script></body></html>"#).unwrap();
    let mut doc = Document::from_renderer_dom(&root);

    let script_id = doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| match &node.kind {
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("script") => Some(NodeId(idx)),
        _ => None,
      })
      .expect("script node not found");

    assert!(
      doc.is_connected_for_scripting(script_id),
      "script should start connected"
    );

    // Detach by severing the parent pointer; this simulates DOM mutation logic that has removed the
    // node from the document tree.
    doc.node_mut(script_id).parent = None;
    assert!(
      !doc.is_connected_for_scripting(script_id),
      "detached script should not be connected for scripting"
    );
  }

  #[test]
  fn connected_for_scripting_respects_generic_inert_subtree_flags() {
    let root = parse_html(r#"<!doctype html><div><script>live</script></div>"#).unwrap();
    let mut doc = Document::from_renderer_dom(&root);

    let script_id = doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| match &node.kind {
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("script") => Some(NodeId(idx)),
        _ => None,
      })
      .expect("script node not found");

    let div_id = doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| match &node.kind {
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("div") => Some(NodeId(idx)),
        _ => None,
      })
      .expect("div node not found");

    assert!(doc.is_connected_for_scripting(script_id));

    doc.node_mut(div_id).inert_subtree = true;
    assert!(
      !doc.is_connected_for_scripting(script_id),
      "inert subtrees should disconnect descendants for scripting"
    );
  }
}
