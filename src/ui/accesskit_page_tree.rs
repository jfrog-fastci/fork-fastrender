#![cfg(feature = "browser_ui")]

use accesskit::{Node, NodeBuilder, NodeClassSet, NodeId, Role};

fn cleared_node_with_role(role: Role) -> Node {
  // AccessKit 0.11 does not support explicit node removal. We instead send a best-effort "detach"
  // update by clearing the subtree root's children so the adapter can drop descendants when
  // possible.
  let mut builder = NodeBuilder::new(role);
  builder.set_children(Vec::new());
  // `NodeBuilder::build` requires a `NodeClassSet` even if we don't use custom classes.
  let mut classes = NodeClassSet::default();
  builder.build(&mut classes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CurrentRoot {
  id: NodeId,
  role: Role,
  tab_id: Option<crate::ui::TabId>,
}

/// Tracks the currently-attached page accessibility subtree and emits incremental updates that
/// detach the previous subtree root when a navigation replaces the document.
#[derive(Debug, Default)]
pub struct PageAccessKitTree {
  current_root: Option<CurrentRoot>,
}

/// Result of updating the page accessibility subtree for a frame.
#[derive(Debug, Default)]
pub struct PageAccessKitUpdate {
  /// Root of the current document subtree, to be attached as a child of the page host widget.
  pub document_root: Option<NodeId>,
  /// Nodes that should be appended to egui's `accesskit_update.nodes` for the frame.
  pub nodes: Vec<(NodeId, Node)>,
}

impl PageAccessKitTree {
  pub fn new() -> Self {
    Self::default()
  }

  /// Set the currently-attached document subtree root.
  ///
  /// This does **not** create/update the page subtree nodes themselves; those are typically
  /// generated elsewhere (e.g. by the render worker) and merged into egui's AccessKit update.
  ///
  /// When the root changes *within the same tab* (which usually implies a navigation or a page
  /// accessibility-tree generation bump), this emits a best-effort prune update for the previous
  /// root by clearing its children list. This helps AccessKit adapters drop unreachable nodes when
  /// page node ids include a tree generation.
  pub fn set_document_root(&mut self, root: Option<(NodeId, Role)>) -> PageAccessKitUpdate {
    let next = root.map(|(id, role)| CurrentRoot {
      id,
      role,
      tab_id: crate::ui::decode_page_node_id(id).map(|(tab_id, _gen, _dom)| tab_id),
    });
    self.replace_root(next)
  }

  /// Detach any currently attached document subtree.
  pub fn clear_document(&mut self) -> PageAccessKitUpdate {
    self.replace_root(None)
  }

  fn replace_root(&mut self, next: Option<CurrentRoot>) -> PageAccessKitUpdate {
    let prev = self.current_root;
    self.current_root = next;

    let mut nodes = Vec::new();

    // Best-effort pruning strategy:
    // - Always prune when the document is being cleared (tab closed / crash / no active tab).
    // - Otherwise, only prune when the root changes *within the same tab*. This avoids breaking
    //   accessibility when switching between tabs: inactive tabs can keep their current document
    //   subtree cached in the adapter for fast switching.
    if let Some(prev) = prev {
      let should_prune_prev = match self.current_root {
        None => true,
        Some(next) => {
          if prev.id == next.id {
            false
          } else {
            match (prev.tab_id, next.tab_id) {
              (Some(prev_tab), Some(next_tab)) => prev_tab == next_tab,
              // If the node ids don't decode as page nodes, be conservative and prune to avoid
              // retaining unreachable nodes.
              _ => true,
            }
          }
        }
      };
      if should_prune_prev {
        nodes.push((prev.id, cleared_node_with_role(prev.role)));
      }
    }

    PageAccessKitUpdate {
      document_root: self.current_root.map(|c| c.id),
      nodes,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::accessibility::{AccessibilityNode, AccessibilityState};
  use crate::ui::page_accesskit_subtree::accesskit_subtree_for_page;
  use crate::ui::{decode_page_node_id, TabId};

  fn synthetic_accessibility_tree(node_count: usize) -> AccessibilityNode {
    assert!(node_count >= 1);
    let mut children = Vec::new();
    for dom in 2..=node_count {
      children.push(AccessibilityNode {
        node_id: dom,
        role: "statictext".to_string(),
        role_description: None,
        name: Some(format!("Node {dom}")),
        description: None,
        value: None,
        level: None,
        html_tag: None,
        id: None,
        dom_node_id: dom,
        relations: None,
        states: AccessibilityState::default(),
        children: Vec::new(),
        #[cfg(any(debug_assertions, feature = "a11y_debug"))]
        debug: None,
      });
    }

    AccessibilityNode {
      node_id: 1,
      role: "document".to_string(),
      role_description: None,
      name: Some("Document".to_string()),
      description: None,
      value: None,
      level: None,
      html_tag: None,
      id: None,
      dom_node_id: 1,
      relations: None,
      states: AccessibilityState::default(),
      children,
      #[cfg(any(debug_assertions, feature = "a11y_debug"))]
      debug: None,
    }
  }

  #[test]
  fn accesskit_page_updates_do_not_accumulate_old_generations() {
    let mut tree = PageAccessKitTree::new();
    let tab_id = TabId(42);
    const DOC_NODES: usize = 64;
    const NAVS: u32 = 64;

    for gen in 1..=NAVS {
      let accessibility = synthetic_accessibility_tree(DOC_NODES);
      let subtree = accesskit_subtree_for_page(tab_id, gen, &accessibility);
      let root_id = subtree.root_id;

      let update = tree.set_document_root(Some((root_id, Role::Document)));

      // Bounded: current doc nodes + at most one extra node (previous root with cleared children).
      let total_nodes = subtree.nodes.len() + update.nodes.len();
      assert!(
        total_nodes <= DOC_NODES + 1,
        "expected update nodes to stay bounded; got {total_nodes} nodes on gen={gen}"
      );

      // All nodes in the new subtree must be in the current generation.
      for (id, _node) in &subtree.nodes {
        let decoded = decode_page_node_id(*id).expect("expected page node id");
        assert_eq!(decoded.0, tab_id);
        assert_eq!(decoded.1, gen);
      }

      if gen == 1 {
        assert!(
          update.nodes.is_empty(),
          "expected no prune nodes on first document"
        );
        continue;
      }

      assert_eq!(
        update.nodes.len(),
        1,
        "expected exactly one prune node for previous root"
      );
      let (stale_id, stale_node) = &update.nodes[0];
      let decoded = decode_page_node_id(*stale_id).expect("expected stale page node id");
      assert_eq!(decoded.0, tab_id);
      assert_eq!(decoded.1, gen - 1);
      assert_eq!(decoded.2, 1, "expected stale node to be previous document root");
      assert!(
        stale_node.children().is_empty(),
        "expected stale root node children to be cleared"
      );
    }
  }
}
