use super::{Document, NodeId, NodeKind};
use crate::dom::ShadowRootMode;

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

  /// Returns the nearest ancestor [`NodeKind::ShadowRoot`] for `node` (including `node` itself).
  ///
  /// This is used by spec-shaped algorithms that need to know whether a node is inside a shadow
  /// tree. The returned shadow root is the tree root of the node tree that `node` belongs to.
  pub fn containing_shadow_root(&self, node: NodeId) -> Option<NodeId> {
    let mut remaining = self.nodes.len() + 1;
    let mut current = self.contains_node(node).then_some(node);
    while let Some(id) = current {
      if remaining == 0 {
        break;
      }
      remaining -= 1;

      let node = self.get_node(id)?;
      if matches!(node.kind, NodeKind::ShadowRoot { .. }) {
        return Some(id);
      }
      current = self.parent_node(id);
    }
    None
  }

  /// Returns the DOM parent when building an event dispatch path.
  ///
  /// This differs from [`Document::parent_node`] by treating certain subtrees as not
  /// document-connected for event dispatch:
  /// - detached nodes (no parent) have no parent in the event path
  /// - descendants of inert subtrees (currently: `<template>` contents) do not propagate events to
  ///   the inert subtree root or beyond
  pub fn dom_parent_for_event_path(&self, node: NodeId) -> Option<NodeId> {
    let parent = self.parent_node(node)?;
    // In `dom2`, template contents are represented as children of the `<template>` element, with
    // `inert_subtree=true` on that `<template>`. Nodes inside that inert subtree behave like they
    // are disconnected from the document for common web-platform algorithms (e.g., events,
    // scripting).
    if self.node(parent).inert_subtree {
      return None;
    }
    Some(parent)
  }

  /// Returns true if `node` is a `ShadowRoot` node.
  pub fn is_shadow_root(&self, node: NodeId) -> bool {
    self
      .get_node(node)
      .is_some_and(|node| matches!(node.kind, NodeKind::ShadowRoot { .. }))
  }

  /// Returns the mode of a `ShadowRoot` node, if `node` is a shadow root.
  pub fn shadow_root_mode(&self, node: NodeId) -> Option<ShadowRootMode> {
    match &self.get_node(node)?.kind {
      NodeKind::ShadowRoot { mode, .. } => Some(*mode),
      _ => None,
    }
  }

  /// Returns the host element of a `ShadowRoot` node.
  ///
  /// In dom2's tree representation, shadow roots are stored as children of their host element (at
  /// index 0).
  pub fn shadow_root_host(&self, shadow_root: NodeId) -> Option<NodeId> {
    if !self.is_shadow_root(shadow_root) {
      return None;
    }
    let host = self.parent_node(shadow_root)?;
    match self.get_node(host).map(|n| &n.kind) {
      Some(NodeKind::Element { .. } | NodeKind::Slot { .. }) => Some(host),
      _ => None,
    }
  }

  /// Returns the "tree root" used for WHATWG DOM event dispatch.
  ///
  /// Differences from `Node.getRootNode()` / naive ancestor traversal:
  /// - Treats `ShadowRoot` as a root boundary (nodes inside a shadow tree have that `ShadowRoot` as
  ///   their tree root, even though dom2 stores ShadowRoot as a child of its host).
  /// - Treats inert `<template>` contents as disconnected: traversal stops before the inert template
  ///   boundary.
  pub fn event_tree_root(&self, node: NodeId) -> NodeId {
    if !self.contains_node(node) {
      return node;
    }

    let mut current = node;
    // Defensive bound against accidental cycles.
    for _ in 0..=self.nodes.len() {
      if self.is_shadow_root(current) {
        return current;
      }

      let Some(parent) = self.parent_node(current) else {
        return current;
      };
      if self.node(parent).inert_subtree {
        return current;
      }
      current = parent;
    }
    current
  }

  /// Returns true if `ancestor` is a shadow-including inclusive ancestor of `node`.
  ///
  /// dom2 models shadow roots as children of their host elements, so the shadow-including ancestor
  /// relationship is equivalent to the normal ancestor relationship.
  pub fn is_shadow_including_inclusive_ancestor(&self, ancestor: NodeId, node: NodeId) -> bool {
    if !self.contains_node(ancestor) || !self.contains_node(node) {
      return false;
    }
    self.ancestors(node).any(|a| a == ancestor)
  }

  /// Returns the parent for a node when building a DOM Events dispatch path.
  ///
  /// This implements the relevant subset of WHATWG DOM's `get the parent` algorithms for
  /// `Node`/`ShadowRoot`:
  /// - For `Node`: assigned slot (when assigned) or parent, but stops at inert `<template>` boundaries.
  /// - For `ShadowRoot`: returns null iff `!event.composed` and this shadow root is the tree root of
  ///   the first invocation target; otherwise returns the host element.
  pub fn get_parent_for_event(
    &self,
    node: NodeId,
    event: &crate::web::events::Event,
    first_invocation_root: Option<NodeId>,
  ) -> Option<NodeId> {
    if !self.contains_node(node) {
      return None;
    }

    if self.is_shadow_root(node) {
      if !event.composed && first_invocation_root == Some(node) {
        return None;
      }
      return self.shadow_root_host(node);
    }

    // Slottables propagate events to their assigned slot when assigned (Shadow DOM slotting).
    //
    // For event dispatch we must traverse assigned slots regardless of shadow root mode (open/closed),
    // so `open=false`.
    if let Some(slot) = self.find_slot_for_slottable(node, /* open */ false) {
      return Some(slot);
    }

    let parent = self.parent_node(node)?;
    if self.node(parent).inert_subtree {
      return None;
    }
    Some(parent)
  }

  pub fn first_child(&self, node: NodeId) -> Option<NodeId> {
    let node = self.get_node(node)?;
    node
      .children
      .iter()
      .copied()
      .find(|&child| self.contains_node(child))
  }

  pub fn first_element_child(&self, node: NodeId) -> Option<NodeId> {
    let node_ref = self.get_node(node)?;
    if node_ref.inert_subtree {
      return None;
    }
    node_ref.children.iter().copied().find(|&child| {
      let Some(child_node) = self.get_node(child) else {
        return false;
      };
      if child_node.parent != Some(node) {
        return false;
      }
      matches!(
        &child_node.kind,
        super::NodeKind::Element { .. } | super::NodeKind::Slot { .. }
      )
    })
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

  pub fn last_element_child(&self, node: NodeId) -> Option<NodeId> {
    let node_ref = self.get_node(node)?;
    if node_ref.inert_subtree {
      return None;
    }
    node_ref.children.iter().rev().copied().find(|&child| {
      let Some(child_node) = self.get_node(child) else {
        return false;
      };
      if child_node.parent != Some(node) {
        return false;
      }
      matches!(
        &child_node.kind,
        super::NodeKind::Element { .. } | super::NodeKind::Slot { .. }
      )
    })
  }

  pub fn child_element_count(&self, node: NodeId) -> usize {
    let Some(node_ref) = self.get_node(node) else {
      return 0;
    };
    if node_ref.inert_subtree {
      return 0;
    }
    node_ref
      .children
      .iter()
      .copied()
      .filter(|&child| {
        let Some(child_node) = self.get_node(child) else {
          return false;
        };
        if child_node.parent != Some(node) {
          return false;
        }
        matches!(
          &child_node.kind,
          super::NodeKind::Element { .. } | super::NodeKind::Slot { .. }
        )
      })
      .count()
  }

  pub fn children_elements(&self, node: NodeId) -> Vec<NodeId> {
    let Some(node_ref) = self.get_node(node) else {
      return Vec::new();
    };
    if node_ref.inert_subtree {
      return Vec::new();
    }
    node_ref
      .children
      .iter()
      .copied()
      .filter(|&child| {
        let Some(child_node) = self.get_node(child) else {
          return false;
        };
        if child_node.parent != Some(node) {
          return false;
        }
        matches!(
          &child_node.kind,
          super::NodeKind::Element { .. } | super::NodeKind::Slot { .. }
        )
      })
      .collect()
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

  pub fn previous_element_sibling(&self, node: NodeId) -> Option<NodeId> {
    let parent = self.parent_node(node)?;
    let parent_node = self.get_node(parent)?;
    let pos = parent_node.children.iter().position(|&c| c == node)?;
    parent_node
      .children
      .iter()
      .take(pos)
      .rev()
      .copied()
      .find(|&sib| {
        let Some(sib_node) = self.get_node(sib) else {
          return false;
        };
        if sib_node.parent != Some(parent) {
          return false;
        }
        matches!(
          &sib_node.kind,
          super::NodeKind::Element { .. } | super::NodeKind::Slot { .. }
        )
      })
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

  pub fn next_element_sibling(&self, node: NodeId) -> Option<NodeId> {
    let parent = self.parent_node(node)?;
    let parent_node = self.get_node(parent)?;
    let pos = parent_node.children.iter().position(|&c| c == node)?;
    parent_node
      .children
      .iter()
      .skip(pos + 1)
      .copied()
      .find(|&sib| {
        let Some(sib_node) = self.get_node(sib) else {
          return false;
        };
        if sib_node.parent != Some(parent) {
          return false;
        }
        matches!(
          &sib_node.kind,
          super::NodeKind::Element { .. } | super::NodeKind::Slot { .. }
        )
      })
  }

  pub fn last_inclusive_descendant(&self, node: NodeId) -> NodeId {
    if !self.contains_node(node) {
      return node;
    }

    let mut current = node;
    let mut remaining = self.nodes.len() + 1;
    while remaining > 0 {
      remaining -= 1;
      let Some(last_child) = self.last_child(current) else {
        break;
      };
      current = last_child;
    }
    current
  }

  /// Returns the parent node for DOM tree traversal as used by JS-visible iterators (NodeIterator /
  /// TreeWalker) and related DOM algorithms.
  ///
  /// This differs from [`Document::parent_node`] because `dom2`'s internal tree representation
  /// includes:
  /// - `ShadowRoot` nodes as children of their host elements, and
  /// - `<template>` contents as children of the `<template>` element with `inert_subtree=true`.
  ///
  /// In the DOM Standard's *tree*:
  /// - `ShadowRoot` is the root of a separate node tree and does not have a parent node, and
  /// - template contents are in a separate `DocumentFragment` and are not descendants of the
  ///   `<template>` element.
  ///
  /// For traversal semantics we therefore treat:
  /// - `ShadowRoot` nodes as having no parent, and
  /// - nodes whose parent has `inert_subtree=true` as disconnected (no parent).
  pub(super) fn traversal_parent_node(&self, node: NodeId) -> Option<NodeId> {
    if !self.contains_node(node) {
      return None;
    }

    // `ShadowRoot` nodes are roots of their own node trees; they are not children of their hosts.
    if self.is_shadow_root(node) {
      return None;
    }

    let parent = self.parent_node(node)?;

    // Template contents are represented as children of the `<template>` element with
    // `inert_subtree=true`. Those descendants must behave as disconnected for traversal.
    if self.node(parent).inert_subtree {
      return None;
    }

    Some(parent)
  }

  /// Returns the first child for DOM tree traversal (skipping shadow roots and inert subtrees).
  fn traversal_first_child(&self, node: NodeId) -> Option<NodeId> {
    let node_ref = self.get_node(node)?;
    if node_ref.inert_subtree {
      return None;
    }
    node_ref
      .children
      .iter()
      .copied()
      .find(|&child| self.contains_node(child) && self.traversal_parent_node(child) == Some(node))
  }

  /// Returns the last child for DOM tree traversal (skipping shadow roots and inert subtrees).
  fn traversal_last_child(&self, node: NodeId) -> Option<NodeId> {
    let node_ref = self.get_node(node)?;
    if node_ref.inert_subtree {
      return None;
    }
    node_ref
      .children
      .iter()
      .rev()
      .copied()
      .find(|&child| self.contains_node(child) && self.traversal_parent_node(child) == Some(node))
  }

  pub(super) fn traversal_previous_sibling(&self, node: NodeId) -> Option<NodeId> {
    let parent = self.traversal_parent_node(node)?;
    let parent_node = self.get_node(parent)?;
    let pos = parent_node.children.iter().position(|&c| c == node)?;
    parent_node
      .children
      .iter()
      .take(pos)
      .rev()
      .copied()
      .find(|&sib| self.traversal_parent_node(sib) == Some(parent))
  }

  fn traversal_next_sibling(&self, node: NodeId) -> Option<NodeId> {
    let parent = self.traversal_parent_node(node)?;
    let parent_node = self.get_node(parent)?;
    let pos = parent_node.children.iter().position(|&c| c == node)?;
    parent_node
      .children
      .iter()
      .skip(pos + 1)
      .copied()
      .find(|&sib| self.traversal_parent_node(sib) == Some(parent))
  }

  /// Returns the last inclusive descendant in DOM tree order as used by JS-visible iterators.
  ///
  /// Unlike [`Document::last_inclusive_descendant`], this:
  /// - does not descend into nodes with `inert_subtree=true` (e.g. `<template>`), and
  /// - does not traverse into `ShadowRoot` subtrees when the entry point is in the light DOM.
  pub(super) fn traversal_last_inclusive_descendant(&self, node: NodeId) -> NodeId {
    if !self.contains_node(node) {
      return node;
    }

    let mut current = node;
    // Defensive bound against accidental cycles.
    for _ in 0..=self.nodes.len() {
      let Some(last_child) = self.traversal_last_child(current) else {
        break;
      };
      current = last_child;
    }
    current
  }

  pub(super) fn traversal_is_inclusive_descendant(&self, root: NodeId, node: NodeId) -> bool {
    if !self.contains_node(root) || !self.contains_node(node) {
      return false;
    }

    let mut current = Some(node);
    for _ in 0..=self.nodes.len() {
      let Some(id) = current else {
        break;
      };
      if id == root {
        return true;
      }
      current = self.traversal_parent_node(id);
    }
    false
  }

  /// Returns the node following `node` in DOM tree order within the subtree rooted at `root`.
  ///
  /// This matches WHATWG DOM's "following node" concept for NodeIterator/TreeWalker traversal:
  /// - Shadow roots are not part of their host's light DOM tree.
  /// - Inert subtrees (`inert_subtree=true`, currently `<template>` contents) are not traversed.
  pub(super) fn traversal_following_in_subtree(
    &self,
    root: NodeId,
    node: NodeId,
  ) -> Option<NodeId> {
    if !self.traversal_is_inclusive_descendant(root, node) {
      return None;
    }

    if let Some(first_child) = self.traversal_first_child(node) {
      return Some(first_child);
    }

    let mut current = node;
    for _ in 0..=self.nodes.len() {
      if current == root {
        return None;
      }
      if let Some(next_sibling) = self.traversal_next_sibling(current) {
        return Some(next_sibling);
      }
      current = self.traversal_parent_node(current)?;
    }

    None
  }

  /// Returns the node preceding `node` in DOM tree order within the subtree rooted at `root`.
  ///
  /// See [`Document::traversal_following_in_subtree`] for traversal semantics.
  #[allow(dead_code)]
  pub(super) fn traversal_preceding_in_subtree(
    &self,
    root: NodeId,
    node: NodeId,
  ) -> Option<NodeId> {
    if root == node {
      return None;
    }
    if !self.traversal_is_inclusive_descendant(root, node) {
      return None;
    }

    if let Some(previous_sibling) = self.traversal_previous_sibling(node) {
      return Some(self.traversal_last_inclusive_descendant(previous_sibling));
    }

    self.traversal_parent_node(node)
  }

  pub fn following_in_subtree(&self, root: NodeId, node: NodeId) -> Option<NodeId> {
    if !self.contains_node(root) || !self.contains_node(node) {
      return None;
    }

    if !self.ancestors(node).any(|ancestor| ancestor == root) {
      return None;
    }

    if let Some(first_child) = self.first_child(node) {
      return Some(first_child);
    }

    let mut current = node;
    let mut remaining = self.nodes.len() + 1;
    while remaining > 0 {
      remaining -= 1;
      if current == root {
        return None;
      }
      if let Some(next_sibling) = self.next_sibling(current) {
        return Some(next_sibling);
      }
      current = self.parent_node(current)?;
    }

    None
  }

  pub fn preceding_in_subtree(&self, root: NodeId, node: NodeId) -> Option<NodeId> {
    if !self.contains_node(root) || !self.contains_node(node) {
      return None;
    }

    if root == node {
      return None;
    }

    if !self.ancestors(node).any(|ancestor| ancestor == root) {
      return None;
    }

    if let Some(previous_sibling) = self.previous_sibling(node) {
      return Some(self.last_inclusive_descendant(previous_sibling));
    }

    self.parent_node(node)
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
      stack: self
        .contains_node(root)
        .then_some(root)
        .into_iter()
        .collect(),
      remaining: self.nodes.len() + 1,
    }
  }

  /// Preorder traversal over the DOM-connected subtree rooted at `root`.
  ///
  /// This iterator:
  /// - skips nodes that are not connected to the document root (i.e. detached subtrees), and
  /// - does not descend into inert subtrees (`Node::inert_subtree`), matching `<template>` inert
  ///   contents semantics for DOM queries.
  ///
  /// Traversal follows only edges where the child's `parent` pointer matches the current node. This
  /// keeps traversal robust against partially-detached nodes that are still present in a parent's
  /// `children` list.
  pub fn dom_connected_subtree_preorder(&self, root: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    DomConnectedSubtreePreorder {
      doc: self,
      stack: (self.contains_node(root) && self.is_connected_for_scripting(root))
        .then_some(root)
        .into_iter()
        .collect(),
      remaining: self.nodes.len() + 1,
    }
  }

  /// Convenience wrapper for `dom_connected_subtree_preorder(self.root())`.
  pub fn dom_connected_preorder(&self) -> impl Iterator<Item = NodeId> + '_ {
    self.dom_connected_subtree_preorder(self.root())
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

struct DomConnectedSubtreePreorder<'a> {
  doc: &'a Document,
  stack: Vec<NodeId>,
  remaining: usize,
}

impl Iterator for DomConnectedSubtreePreorder<'_> {
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

      if !node.inert_subtree {
        for &child in node.children.iter().rev() {
          let Some(child_node) = self.doc.get_node(child) else {
            continue;
          };
          if child_node.parent == Some(id) {
            self.stack.push(child);
          }
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
    let root =
      parse_html(r#"<!doctype html><html><body><script>live</script></body></html>"#).unwrap();
    let mut doc = Document::from_renderer_dom(&root);

    let script_id = doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| match &node.kind {
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("script") => {
          Some(NodeId(idx))
        }
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
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("script") => {
          Some(NodeId(idx))
        }
        _ => None,
      })
      .expect("script node not found");

    let div_id = doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| match &node.kind {
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("div") => {
          Some(NodeId(idx))
        }
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
