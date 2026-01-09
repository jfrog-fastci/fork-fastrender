use crate::dom::{DomNode, DomNodeType, ShadowRootMode};
use crate::web::dom::selectors::{node_matches_selector_list, parse_selector_list};
use crate::web::dom::DomException;
use selectors::context::QuirksMode;
use selectors::matching::SelectorCaches;
use selectors::OpaqueElement;

mod attrs;
mod class_list;
mod error;
pub mod import;
pub mod events;
mod html5ever_tree_sink;
mod traversal;
mod shadow_dom;

pub use error::DomError;
pub use html5ever_tree_sink::Dom2TreeSink;

#[cfg(test)]
mod class_list_tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(usize);

impl NodeId {
  pub fn index(self) -> usize {
    self.0
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
  Document {
    quirks_mode: QuirksMode,
  },
  ShadowRoot {
    mode: ShadowRootMode,
    delegates_focus: bool,
  },
  Slot {
    namespace: String,
    attributes: Vec<(String, String)>,
    assigned: bool,
  },
  Element {
    tag_name: String,
    namespace: String,
    attributes: Vec<(String, String)>,
  },
  Text {
    content: String,
  },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
  pub kind: NodeKind,
  pub parent: Option<NodeId>,
  pub children: Vec<NodeId>,
  /// Whether this node's children should be treated as inert for selector matching.
  ///
  /// This currently mirrors `crate::dom::DomNode::template_contents_are_inert()`, which represents
  /// inert template contents by keeping template descendants in `children` while skipping them for
  /// selector matching and other traversals.
  pub inert_subtree: bool,
}

#[derive(Debug, Clone)]
pub struct Document {
  nodes: Vec<Node>,
  root: NodeId,
  events: events::EventListenerRegistry,
}

#[derive(Debug, Clone)]
pub struct RendererDomMapping {
  /// 1-based pre-order id (as produced by `crate::dom::enumerate_dom_ids`) -> `dom2` [`NodeId`].
  ///
  /// Index 0 is always `None` so renderer ids can be used directly as indexes.
  preorder_to_node_id: Vec<Option<NodeId>>,
  /// `dom2` [`NodeId`] index -> 1-based pre-order id.
  ///
  /// Uses 0 for nodes that are not reachable from the document root.
  node_id_to_preorder: Vec<usize>,
}

impl RendererDomMapping {
  /// Translate a 1-based renderer pre-order id (as produced by [`crate::dom::enumerate_dom_ids`])
  /// back into a `dom2` [`NodeId`].
  pub fn node_id_for_preorder(&self, preorder_id: usize) -> Option<NodeId> {
    self
      .preorder_to_node_id
      .get(preorder_id)
      .copied()
      .flatten()
  }

  /// Translate a `dom2` [`NodeId`] to its 1-based renderer pre-order id.
  ///
  /// Returns `None` for nodes that are not reachable from the document root (detached subtrees).
  pub fn preorder_for_node_id(&self, node_id: NodeId) -> Option<usize> {
    self
      .node_id_to_preorder
      .get(node_id.index())
      .copied()
      .and_then(|v| (v != 0).then_some(v))
  }
}

#[derive(Debug, Clone)]
struct SelectorDomMapping {
  preorder_to_node_id: Vec<Option<NodeId>>,
  node_id_to_preorder: Vec<usize>,
}

impl SelectorDomMapping {
  pub fn node_id_for_preorder(&self, preorder_id: usize) -> Option<NodeId> {
    self
      .preorder_to_node_id
      .get(preorder_id)
      .copied()
      .flatten()
  }

  /// Translate a `dom2` [`NodeId`] to its selector-matching pre-order id.
  ///
  /// Returns `None` when the node is either detached or lives under an inert `<template>` subtree
  /// that is skipped for selector matching.
  pub fn preorder_for_node_id(&self, node_id: NodeId) -> Option<usize> {
    self
      .node_id_to_preorder
      .get(node_id.index())
      .copied()
      .and_then(|v| (v != 0).then_some(v))
  }
}

#[derive(Debug, Clone)]
pub struct RendererDomSnapshot {
  pub dom: DomNode,
  pub mapping: RendererDomMapping,
}

impl Document {
  pub fn new(quirks_mode: QuirksMode) -> Self {
    let mut doc = Self {
      nodes: Vec::new(),
      root: NodeId(0),
      events: events::EventListenerRegistry::default(),
    };
    let root = doc.push_node(
      NodeKind::Document { quirks_mode },
      None,
      /* inert_subtree */ false,
    );
    debug_assert_eq!(root, NodeId(0));
    doc.root = root;
    doc
  }

  pub fn root(&self) -> NodeId {
    self.root
  }

  pub fn node(&self, id: NodeId) -> &Node {
    &self.nodes[id.0]
  }

  pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
    &mut self.nodes[id.0]
  }

  pub fn nodes(&self) -> &[Node] {
    &self.nodes
  }

  pub fn nodes_len(&self) -> usize {
    self.nodes.len()
  }

  fn push_node(&mut self, kind: NodeKind, parent: Option<NodeId>, inert_subtree: bool) -> NodeId {
    let id = NodeId(self.nodes.len());
    self.nodes.push(Node {
      kind,
      parent,
      children: Vec::new(),
      inert_subtree,
    });
    if let Some(parent_id) = parent {
      self.nodes[parent_id.0].children.push(id);
    }
    id
  }

  /// Snapshot this `dom2` document back into the renderer's immutable [`DomNode`] representation.
  ///
  /// This is used for tests and incremental adoption (e.g. import into `dom2`, mutate, then render
  /// via existing code that consumes `DomNode`).
  pub fn to_renderer_dom(&self) -> DomNode {
    struct Frame {
      src: NodeId,
      dst: *mut DomNode,
      next_child: usize,
    }

    fn node_kind_to_dom_node_type(kind: &NodeKind) -> DomNodeType {
      match kind {
        NodeKind::Document { quirks_mode } => DomNodeType::Document {
          quirks_mode: *quirks_mode,
        },
        NodeKind::ShadowRoot {
          mode,
          delegates_focus,
        } => DomNodeType::ShadowRoot {
          mode: *mode,
          delegates_focus: *delegates_focus,
        },
        NodeKind::Slot {
          namespace,
          attributes,
          assigned,
        } => DomNodeType::Slot {
          namespace: namespace.clone(),
          attributes: attributes.clone(),
          assigned: *assigned,
        },
        NodeKind::Element {
          tag_name,
          namespace,
          attributes,
        } => DomNodeType::Element {
          tag_name: tag_name.clone(),
          namespace: namespace.clone(),
          attributes: attributes.clone(),
        },
        NodeKind::Text { content } => DomNodeType::Text {
          content: content.clone(),
        },
      }
    }

    let root_id = self.root;
    let root_src = self.node(root_id);
    let mut root = DomNode {
      node_type: node_kind_to_dom_node_type(&root_src.kind),
      children: Vec::with_capacity(root_src.children.len()),
    };

    let mut stack: Vec<Frame> = vec![Frame {
      src: root_id,
      dst: &mut root as *mut DomNode,
      next_child: 0,
    }];

    while let Some(mut frame) = stack.pop() {
      let src = self.node(frame.src);
      // Safety: `dst` always points into `root` (the output tree). We only mutate the children vec
      // of a node after pushing its frame, and we never move nodes out of the output tree.
      let dst = unsafe { &mut *frame.dst };

      if frame.next_child < src.children.len() {
        let child_id = src.children[frame.next_child];
        frame.next_child += 1;
        stack.push(frame);

        let child_src = self.node(child_id);
        dst.children.push(DomNode {
          node_type: node_kind_to_dom_node_type(&child_src.kind),
          children: Vec::with_capacity(child_src.children.len()),
        });
        let child_dst = dst
          .children
          .last_mut()
          .map(|node| node as *mut DomNode)
          .expect("child node missing after push");
        stack.push(Frame {
          src: child_id,
          dst: child_dst,
          next_child: 0,
        });
      }
    }

    root
  }

  fn build_renderer_preorder_mapping(&self) -> RendererDomMapping {
    // Preorder ids are 1-based (index 0 unused), matching `crate::dom::enumerate_dom_ids` and the
    // debug inspector.
    let mut preorder_to_node_id: Vec<Option<NodeId>> = Vec::with_capacity(self.nodes.len() + 1);
    preorder_to_node_id.push(None);
    let mut node_id_to_preorder: Vec<usize> = vec![0; self.nodes.len()];

    let mut stack: Vec<NodeId> = vec![self.root];
    while let Some(id) = stack.pop() {
      let preorder_id = preorder_to_node_id.len();
      preorder_to_node_id.push(Some(id));
      node_id_to_preorder[id.0] = preorder_id;

      let node = self.node(id);
      // Push children in reverse so we traverse in tree order.
      for child in node.children.iter().rev() {
        stack.push(*child);
      }
    }

    RendererDomMapping {
      preorder_to_node_id,
      node_id_to_preorder,
    }
  }

  fn build_selector_preorder_mapping(&self) -> SelectorDomMapping {
    // Preorder ids are 1-based (index 0 unused), matching selector-matching traversals in this
    // module (e.g. `query_selector`).
    let mut preorder_to_node_id: Vec<Option<NodeId>> = Vec::with_capacity(self.nodes.len() + 1);
    preorder_to_node_id.push(None);
    let mut node_id_to_preorder: Vec<usize> = vec![0; self.nodes.len()];

    let mut stack: Vec<NodeId> = vec![self.root];
    while let Some(id) = stack.pop() {
      let preorder_id = preorder_to_node_id.len();
      preorder_to_node_id.push(Some(id));
      node_id_to_preorder[id.0] = preorder_id;

      let node = self.node(id);
      if node.inert_subtree {
        continue;
      }
      // Push children in reverse so we traverse in tree order.
      for child in node.children.iter().rev() {
        stack.push(*child);
      }
    }

    SelectorDomMapping {
      preorder_to_node_id,
      node_id_to_preorder,
    }
  }

  pub fn to_renderer_dom_with_mapping(&self) -> RendererDomSnapshot {
    RendererDomSnapshot {
      dom: self.to_renderer_dom(),
      mapping: self.build_renderer_preorder_mapping(),
    }
  }

  pub fn query_selector(
    &mut self,
    selectors: &str,
    scope: Option<NodeId>,
  ) -> Result<Option<NodeId>, DomException> {
    let selector_list = parse_selector_list(selectors)?;
    let snapshot = self.to_renderer_dom();
    let mapping = self.build_selector_preorder_mapping();
    let quirks_mode = snapshot.document_quirks_mode();

    let mut selector_caches = SelectorCaches::default();
    selector_caches.set_epoch(crate::dom::next_selector_cache_epoch());

    let scope_preorder = scope.and_then(|id| mapping.preorder_for_node_id(id));
    if scope.is_some() && scope_preorder.is_none() {
      return Ok(None);
    }

    struct StackItem<'a> {
      node: &'a DomNode,
      exiting: bool,
      node_id: NodeId,
    }

    let mut ancestors: Vec<&DomNode> = Vec::new();
    let mut stack: Vec<StackItem<'_>> = Vec::new();
    stack.push(StackItem {
      node: &snapshot,
      exiting: false,
      node_id: NodeId(usize::MAX),
    });
    let mut next_preorder_id = 1usize;
    let mut scope_active = scope.is_none();
    let mut scope_anchor: Option<OpaqueElement> = None;

    while let Some(item) = stack.pop() {
      if item.exiting {
        ancestors.pop();
        if scope.is_some() && item.node_id == scope.unwrap() {
          break;
        }
        continue;
      }

      let preorder_id = next_preorder_id;
      next_preorder_id += 1;
      let dom2_id = mapping
        .node_id_for_preorder(preorder_id)
        .unwrap_or(self.root);

      if scope == Some(dom2_id) {
        scope_active = true;
        if item.node.is_element() {
          scope_anchor = Some(OpaqueElement::new(item.node));
        }
      }

      if scope_active && item.node.is_element() {
        if node_matches_selector_list(
          item.node,
          &ancestors,
          &selector_list,
          &mut selector_caches,
          quirks_mode,
          scope_anchor,
        ) {
          return Ok(Some(dom2_id));
        }
      }

      stack.push(StackItem {
        node: item.node,
        exiting: true,
        node_id: dom2_id,
      });
      ancestors.push(item.node);

      if !self.node(dom2_id).inert_subtree {
        for child in item.node.children.iter().rev() {
          stack.push(StackItem {
            node: child,
            exiting: false,
            node_id: NodeId(usize::MAX),
          });
        }
      }
    }

    Ok(None)
  }

  pub fn query_selector_all(
    &mut self,
    selectors: &str,
    scope: Option<NodeId>,
  ) -> Result<Vec<NodeId>, DomException> {
    let selector_list = parse_selector_list(selectors)?;
    let snapshot = self.to_renderer_dom();
    let mapping = self.build_selector_preorder_mapping();
    let quirks_mode = snapshot.document_quirks_mode();

    let mut selector_caches = SelectorCaches::default();
    selector_caches.set_epoch(crate::dom::next_selector_cache_epoch());

    let scope_preorder = scope.and_then(|id| mapping.preorder_for_node_id(id));
    if scope.is_some() && scope_preorder.is_none() {
      return Ok(Vec::new());
    }

    struct StackItem<'a> {
      node: &'a DomNode,
      exiting: bool,
      node_id: NodeId,
    }

    let mut results: Vec<NodeId> = Vec::new();
    let mut ancestors: Vec<&DomNode> = Vec::new();
    let mut stack: Vec<StackItem<'_>> = Vec::new();
    stack.push(StackItem {
      node: &snapshot,
      exiting: false,
      node_id: NodeId(usize::MAX),
    });
    let mut next_preorder_id = 1usize;
    let mut scope_active = scope.is_none();
    let mut scope_anchor: Option<OpaqueElement> = None;

    while let Some(item) = stack.pop() {
      if item.exiting {
        ancestors.pop();
        if scope.is_some() && item.node_id == scope.unwrap() {
          break;
        }
        continue;
      }

      let preorder_id = next_preorder_id;
      next_preorder_id += 1;
      let dom2_id = mapping
        .node_id_for_preorder(preorder_id)
        .unwrap_or(self.root);

      if scope == Some(dom2_id) {
        scope_active = true;
        if item.node.is_element() {
          scope_anchor = Some(OpaqueElement::new(item.node));
        }
      }

      if scope_active && item.node.is_element() {
        if node_matches_selector_list(
          item.node,
          &ancestors,
          &selector_list,
          &mut selector_caches,
          quirks_mode,
          scope_anchor,
        ) {
          results.push(dom2_id);
        }
      }

      stack.push(StackItem {
        node: item.node,
        exiting: true,
        node_id: dom2_id,
      });
      ancestors.push(item.node);

      if !self.node(dom2_id).inert_subtree {
        for child in item.node.children.iter().rev() {
          stack.push(StackItem {
            node: child,
            exiting: false,
            node_id: NodeId(usize::MAX),
          });
        }
      }
    }

    Ok(results)
  }

  pub fn matches_selector(
    &mut self,
    element: NodeId,
    selectors: &str,
  ) -> Result<bool, DomException> {
    let selector_list = parse_selector_list(selectors)?;
    if element.index() >= self.nodes.len() {
      return Ok(false);
    }
    match &self.node(element).kind {
      NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
      _ => return Ok(false),
    }

    let snapshot = self.to_renderer_dom();
    let mapping = self.build_selector_preorder_mapping();
    let quirks_mode = snapshot.document_quirks_mode();

    let mut selector_caches = SelectorCaches::default();
    selector_caches.set_epoch(crate::dom::next_selector_cache_epoch());

    let element_preorder = mapping.preorder_for_node_id(element);
    let Some(target_preorder) = element_preorder else {
      // The element lives in an inert subtree that is not traversed for selector matching.
      return Ok(false);
    };

    struct StackItem<'a> {
      node: &'a DomNode,
      exiting: bool,
      node_id: NodeId,
    }

    let mut ancestors: Vec<&DomNode> = Vec::new();
    let mut stack: Vec<StackItem<'_>> = Vec::new();
    stack.push(StackItem {
      node: &snapshot,
      exiting: false,
      node_id: NodeId(usize::MAX),
    });
    let mut next_preorder_id = 1usize;

    while let Some(item) = stack.pop() {
      if item.exiting {
        ancestors.pop();
        continue;
      }

      let preorder_id = next_preorder_id;
      next_preorder_id += 1;
      let dom2_id = mapping
        .node_id_for_preorder(preorder_id)
        .unwrap_or(self.root);

      stack.push(StackItem {
        node: item.node,
        exiting: true,
        node_id: dom2_id,
      });
      ancestors.push(item.node);

      if dom2_id == element {
        let anchor = Some(OpaqueElement::new(item.node));
        let matched = node_matches_selector_list(
          item.node,
          &ancestors[..ancestors.len().saturating_sub(1)],
          &selector_list,
          &mut selector_caches,
          quirks_mode,
          anchor,
        );
        return Ok(matched);
      }

      if preorder_id >= target_preorder {
        // If we've passed the target preorder id without finding it, the mapping/traversal is out of
        // sync; bail out defensively.
        return Ok(false);
      }

      if !self.node(dom2_id).inert_subtree {
        for child in item.node.children.iter().rev() {
          stack.push(StackItem {
            node: child,
            exiting: false,
            node_id: NodeId(usize::MAX),
          });
        }
      }
    }

    Ok(false)
  }
}

#[cfg(test)]
mod mapping_tests;
