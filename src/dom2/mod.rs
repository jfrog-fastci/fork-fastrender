use crate::dom::{DomNode, DomNodeType, ShadowRootMode};
use selectors::context::QuirksMode;

pub mod import;

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
}

impl Document {
  pub fn new(quirks_mode: QuirksMode) -> Self {
    let mut doc = Self {
      nodes: Vec::new(),
      root: NodeId(0),
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
}

