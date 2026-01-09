use crate::dom::{DomNode, DomNodeType, ShadowRootMode};
use crate::dom::HTML_NAMESPACE;
use crate::web::dom::selectors::{node_matches_selector_list, parse_selector_list};
use crate::web::events;
use crate::web::dom::DomException;
use selectors::context::QuirksMode;
use selectors::matching::SelectorCaches;
use selectors::OpaqueElement;

mod attrs;
mod class_list;
mod error;
pub use error::{DomError, Result as DomResult};

mod mutation;
mod js_shims;
mod style_attr;
pub mod import;
mod html5ever_tree_sink;
mod traversal;
mod shadow_dom;
pub use html5ever_tree_sink::Dom2TreeSink;

#[cfg(test)]
mod class_list_tests;
#[cfg(test)]
mod wbr_tests;

/// Convenience helper mirroring `Document.getElementById`.
///
/// This is intentionally a very small utility used by integration tests and early JS plumbing.
pub fn get_element_by_id(doc: &Document, id: &str) -> Option<NodeId> {
  doc.get_element_by_id(id)
}

/// Convenience helper for attribute reflection.
///
/// Returns `false` on invalid node types or when the attribute value is unchanged.
pub fn set_attribute(doc: &mut Document, node: NodeId, name: &str, value: &str) -> bool {
  doc.set_attribute(node, name, value).unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(usize);

impl NodeId {
  /// Construct a `NodeId` from its raw index.
  ///
  /// This is intended for bindings/FFI layers that need to round-trip node handles through an
  /// integer representation. `NodeId` values are only meaningful within a specific `dom2::Document`
  /// instance; most `dom2` APIs validate node IDs and return `DomError::NotFoundError` for invalid
  /// indices.
  #[inline]
  pub fn from_index(index: usize) -> Self {
    Self(index)
  }

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
  pub script_already_started: bool,
  pub mathml_annotation_xml_integration_point: bool,
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
  ///
  /// Note: the renderer snapshot may include synthetic nodes that do not exist in the `dom2` tree
  /// (currently: `<wbr>` synthesizes a zero-width break text node). For these nodes, the returned
  /// `NodeId` will be the corresponding real `dom2` node (e.g., the parent `<wbr>` element),
  /// meaning multiple renderer preorder ids can map to the same `NodeId`.
  pub fn node_id_for_preorder(&self, preorder_id: usize) -> Option<NodeId> {
    self
      .preorder_to_node_id
      .get(preorder_id)
      .copied()
      .flatten()
  }

  /// Translate a `dom2` [`NodeId`] to its 1-based renderer pre-order id.
  ///
  /// Returns `None` for nodes that are not reachable from the document root (detached subtrees). If
  /// a `dom2` node corresponds to multiple renderer preorder ids (e.g. `<wbr>` + its synthetic text
  /// child), this returns the preorder id of the real node (the `<wbr>` element) and not the
  /// synthetic one.
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

  pub fn preorder_for_node_id(&self, node_id: NodeId) -> Option<usize> {
    self
      .node_id_to_preorder
      .get(node_id.index())
      .copied()
      .and_then(|v| (v != 0).then_some(v))
  }
}

pub struct RendererDomSnapshot {
  pub dom: DomNode,
  pub mapping: RendererDomMapping,
}

impl Document {
  fn should_inject_wbr_zwsp(&self, node_id: NodeId) -> bool {
    let node = self.node(node_id);
    let NodeKind::Element {
      tag_name,
      namespace,
      ..
    } = &node.kind
    else {
      return false;
    };
    if !tag_name.eq_ignore_ascii_case("wbr") {
      return false;
    }
    if !(namespace.is_empty() || namespace == HTML_NAMESPACE) {
      return false;
    }

    // Avoid duplicating the renderer's historical `<wbr>` behaviour when importing from an
    // existing renderer DOM tree that may already contain a ZWSP text node child.
    for &child in &node.children {
      if let NodeKind::Text { content } = &self.node(child).kind {
        if content == "\u{200B}" {
          return false;
        }
      }
    }

    true
  }

  pub fn new(quirks_mode: QuirksMode) -> Self {
    let mut doc = Self {
      nodes: Vec::new(),
      root: NodeId(0),
      events: events::EventListenerRegistry::new(),
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

  pub fn events(&self) -> &events::EventListenerRegistry {
    &self.events
  }

  pub fn events_mut(&mut self) -> &mut events::EventListenerRegistry {
    &mut self.events
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

  /// Returns the document element.
  ///
  /// This is the first child of the document root that is an element (including `<slot>`),
  /// in tree order.
  pub fn document_element(&self) -> Option<NodeId> {
    let root = self.root();
    let root_node = self.nodes.get(root.index())?;
    root_node.children.iter().copied().find(|&child| {
      self
        .nodes
        .get(child.index())
        .is_some_and(|node| node.parent == Some(root))
        && matches!(
          self.nodes[child.index()].kind,
          NodeKind::Element { .. } | NodeKind::Slot { .. }
        )
    })
  }

  /// Returns the first element in tree order whose `id` attribute matches `id`.
  ///
  /// This query:
  /// - returns `None` for an empty `id`,
  /// - ignores detached subtrees, and
  /// - ignores nodes inside inert `<template>` contents (`Node::inert_subtree`).
  pub fn get_element_by_id(&self, id: &str) -> Option<NodeId> {
    if id.is_empty() {
      return None;
    }

    for node_id in self.dom_connected_preorder() {
      let Some(node) = self.nodes.get(node_id.index()) else {
        continue;
      };
      let (namespace, attributes) = match &node.kind {
        NodeKind::Element {
          namespace,
          attributes,
          ..
        }
        | NodeKind::Slot {
          namespace,
          attributes,
          ..
        } => (namespace.as_str(), attributes.as_slice()),
        _ => continue,
      };

      let is_html = namespace.is_empty() || namespace == HTML_NAMESPACE;
      if attributes.iter().any(|(name, value)| {
        (if is_html {
          name.eq_ignore_ascii_case("id")
        } else {
          name == "id"
        }) && value == id
      }) {
        return Some(node_id);
      }
    }

    None
  }

  #[inline]
  fn is_html_element(&self, node_id: NodeId, tag: &str) -> bool {
    let Some(node) = self.nodes.get(node_id.index()) else {
      return false;
    };
    match &node.kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } if (namespace.is_empty() || namespace == HTML_NAMESPACE) && tag_name.eq_ignore_ascii_case(tag) => {
        true
      }
      _ => false,
    }
  }

  /// Returns the document's HTML `<head>` element, if any.
  ///
  /// Minimal HTML-ish semantics:
  /// - If `documentElement` exists and is an HTML `<html>` element, return the first HTML `<head>`
  ///   child element (tree order).
  /// - Otherwise return `None`.
  pub fn head(&self) -> Option<NodeId> {
    let html = self.document_element()?;
    if !self.is_html_element(html, "html") {
      return None;
    }
    let html_node = self.nodes.get(html.index())?;
    html_node.children.iter().copied().find(|&child| {
      self
        .nodes
        .get(child.index())
        .is_some_and(|node| node.parent == Some(html))
        && self.is_html_element(child, "head")
    })
  }

  /// Returns the document's HTML `<body>` element, if any.
  ///
  /// Minimal HTML-ish semantics:
  /// - If `documentElement` exists and is an HTML `<html>` element, return the first HTML `<body>`
  ///   child element (tree order).
  /// - Otherwise return `None`.
  pub fn body(&self) -> Option<NodeId> {
    let html = self.document_element()?;
    if !self.is_html_element(html, "html") {
      return None;
    }
    let html_node = self.nodes.get(html.index())?;
    html_node.children.iter().copied().find(|&child| {
      self
        .nodes
        .get(child.index())
        .is_some_and(|node| node.parent == Some(html))
        && self.is_html_element(child, "body")
    })
  }

  fn push_node(&mut self, kind: NodeKind, parent: Option<NodeId>, inert_subtree: bool) -> NodeId {
    let id = NodeId(self.nodes.len());
    self.nodes.push(Node {
      kind,
      parent,
      children: Vec::new(),
      inert_subtree,
      script_already_started: false,
      mathml_annotation_xml_integration_point: false,
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
        let extra_capacity = usize::from(self.should_inject_wbr_zwsp(child_id));
        dst.children.push(DomNode {
          node_type: node_kind_to_dom_node_type(&child_src.kind),
          children: Vec::with_capacity(child_src.children.len() + extra_capacity),
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
      } else if self.should_inject_wbr_zwsp(frame.src) {
        // HTML <wbr> elements represent optional break opportunities. Synthesize a zero-width break
        // text node so line breaking can consider the opportunity while still allowing the element
        // to be styled/selected.
        dst.children.push(DomNode {
          node_type: DomNodeType::Text {
            content: "\u{200B}".to_string(),
          },
          children: Vec::new(),
        });
      }
    }

    root
  }

  fn to_renderer_dom_subtree(&self, root_id: NodeId) -> Option<DomNode> {
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

    let root_src = self.nodes.get(root_id.index())?;
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
        let extra_capacity = usize::from(self.should_inject_wbr_zwsp(child_id));
        dst.children.push(DomNode {
          node_type: node_kind_to_dom_node_type(&child_src.kind),
          children: Vec::with_capacity(child_src.children.len() + extra_capacity),
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
      } else if self.should_inject_wbr_zwsp(frame.src) {
        dst.children.push(DomNode {
          node_type: DomNodeType::Text {
            content: "\u{200B}".to_string(),
          },
          children: Vec::new(),
        });
      }
    }

    Some(root)
  }

  fn build_renderer_preorder_mapping(&self) -> RendererDomMapping {
    // Preorder ids are 1-based (index 0 unused), matching `crate::dom::enumerate_dom_ids` and the
    // debug inspector.
    let mut preorder_to_node_id: Vec<Option<NodeId>> = Vec::with_capacity(self.nodes.len() + 1);
    preorder_to_node_id.push(None);
    let mut node_id_to_preorder: Vec<usize> = vec![0; self.nodes.len()];
 
    enum StackItem {
      Real(NodeId),
      SyntheticWbrZwsp(NodeId),
    }

    let mut stack: Vec<StackItem> = vec![StackItem::Real(self.root)];
    while let Some(item) = stack.pop() {
      match item {
        StackItem::Real(id) => {
          let preorder_id = preorder_to_node_id.len();
          preorder_to_node_id.push(Some(id));
          node_id_to_preorder[id.0] = preorder_id;

          let node = self.node(id);
          // Push children in reverse so we traverse in tree order.
          //
          // For `<wbr>` we also synthesize a trailing ZWSP text child in the renderer snapshot;
          // insert a synthetic stack item so preorder ids stay aligned.
          if self.should_inject_wbr_zwsp(id) {
            stack.push(StackItem::SyntheticWbrZwsp(id));
          }
          for child in node.children.iter().rev() {
            stack.push(StackItem::Real(*child));
          }
        }
        StackItem::SyntheticWbrZwsp(parent) => {
          // Synthetic ZWSP nodes map back to their parent `<wbr>` element `NodeId`.
          preorder_to_node_id.push(Some(parent));
          // Do not overwrite `node_id_to_preorder` for the `<wbr>` element; it should remain the
          // preorder id of the element itself (the first mapping entry).
        }
      }
    }

    RendererDomMapping {
      preorder_to_node_id,
      node_id_to_preorder,
    }
  }

  fn build_selector_preorder_mapping(&self) -> SelectorDomMapping {
    self
      .build_selector_preorder_mapping_from(self.root)
      .expect("dom2 document root missing")
  }

  fn build_selector_preorder_mapping_from(&self, root: NodeId) -> Option<SelectorDomMapping> {
    if root.index() >= self.nodes.len() {
      return None;
    }

    // Preorder ids are 1-based (index 0 unused), matching `crate::dom::enumerate_dom_ids`.
    let mut preorder_to_node_id: Vec<Option<NodeId>> = Vec::with_capacity(self.nodes.len() + 1);
    preorder_to_node_id.push(None);
    let mut node_id_to_preorder: Vec<usize> = vec![0; self.nodes.len()];

    enum StackItem {
      Real(NodeId),
      SyntheticWbrZwsp(NodeId),
    }

    let mut stack: Vec<StackItem> = vec![StackItem::Real(root)];
    while let Some(item) = stack.pop() {
      match item {
        StackItem::Real(id) => {
          if id.index() >= self.nodes.len() {
            continue;
          }

          let preorder_id = preorder_to_node_id.len();
          preorder_to_node_id.push(Some(id));
          node_id_to_preorder[id.index()] = preorder_id;

          let node = self.node(id);

          if self.should_inject_wbr_zwsp(id) {
            stack.push(StackItem::SyntheticWbrZwsp(id));
          }

          // For selector matching we treat inert subtrees (currently: `<template>` contents) as
          // disconnected, mirroring how template `.content` is not in the light DOM tree.
          if !node.inert_subtree {
            for child in node.children.iter().rev() {
              stack.push(StackItem::Real(*child));
            }
          }
        }
        StackItem::SyntheticWbrZwsp(parent) => {
          // Synthetic ZWSP nodes map back to their parent `<wbr>` element `NodeId`.
          preorder_to_node_id.push(Some(parent));
        }
      }
    }

    Some(SelectorDomMapping {
      preorder_to_node_id,
      node_id_to_preorder,
    })
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
    let quirks_mode = match &self.node(self.root()).kind {
      NodeKind::Document { quirks_mode } => *quirks_mode,
      _ => QuirksMode::NoQuirks,
    };

    let mut selector_caches = SelectorCaches::default();
    selector_caches.set_epoch(crate::dom::next_selector_cache_epoch());

    let use_document_snapshot = scope.is_none() || scope.is_some_and(|id| self.is_connected_for_scripting(id));
    let (snapshot_dom, mapping) = if use_document_snapshot {
      (self.to_renderer_dom(), self.build_selector_preorder_mapping())
    } else {
      let scope_id = scope.expect("scope is Some when not using document snapshot");
      let Some(dom) = self.to_renderer_dom_subtree(scope_id) else {
        return Ok(None);
      };
      let Some(mapping) = self.build_selector_preorder_mapping_from(scope_id) else {
        return Ok(None);
      };
      (dom, mapping)
    };

    // If we're searching the full document snapshot, ensure the scope is reachable inside the
    // selector snapshot mapping. Detached/inert scopes use subtree snapshots instead.
    if use_document_snapshot {
      let scope_preorder = scope.and_then(|id| mapping.preorder_for_node_id(id));
      if scope.is_some() && scope_preorder.is_none() {
        return Ok(None);
      }
    }

    struct StackItem<'a> {
      node: &'a DomNode,
      exiting: bool,
      node_id: Option<NodeId>,
    }

    let mut ancestors: Vec<&DomNode> = Vec::new();
    let mut stack: Vec<StackItem<'_>> = Vec::new();
    stack.push(StackItem {
      node: &snapshot_dom,
      exiting: false,
      node_id: None,
    });
    let mut next_preorder_id = 1usize;
    let mut scope_active = scope.is_none() || !use_document_snapshot;
    let mut scope_anchor: Option<OpaqueElement> =
      (!use_document_snapshot && snapshot_dom.is_element()).then_some(OpaqueElement::new(&snapshot_dom));

    while let Some(item) = stack.pop() {
      if item.exiting {
        ancestors.pop();
        if let Some(scope_id) = scope {
          if item.node_id == Some(scope_id) {
            break;
          }
        }
        continue;
      }

      let preorder_id = next_preorder_id;
      next_preorder_id += 1;
      let dom2_id = mapping.node_id_for_preorder(preorder_id);

      if let Some(dom2_id) = dom2_id {
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
      }

      stack.push(StackItem {
        node: item.node,
        exiting: true,
        node_id: dom2_id,
      });
      ancestors.push(item.node);

      let mut descend = true;
      if let Some(dom2_id) = dom2_id {
        descend = !self.node(dom2_id).inert_subtree;
      }

      if descend {
        for child in item.node.children.iter().rev() {
          stack.push(StackItem {
            node: child,
            exiting: false,
            node_id: None,
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
    let quirks_mode = match &self.node(self.root()).kind {
      NodeKind::Document { quirks_mode } => *quirks_mode,
      _ => QuirksMode::NoQuirks,
    };

    let mut selector_caches = SelectorCaches::default();
    selector_caches.set_epoch(crate::dom::next_selector_cache_epoch());

    let use_document_snapshot = scope.is_none() || scope.is_some_and(|id| self.is_connected_for_scripting(id));
    let (snapshot_dom, mapping) = if use_document_snapshot {
      (self.to_renderer_dom(), self.build_selector_preorder_mapping())
    } else {
      let scope_id = scope.expect("scope is Some when not using document snapshot");
      let Some(dom) = self.to_renderer_dom_subtree(scope_id) else {
        return Ok(Vec::new());
      };
      let Some(mapping) = self.build_selector_preorder_mapping_from(scope_id) else {
        return Ok(Vec::new());
      };
      (dom, mapping)
    };

    if use_document_snapshot {
      let scope_preorder = scope.and_then(|id| mapping.preorder_for_node_id(id));
      if scope.is_some() && scope_preorder.is_none() {
        return Ok(Vec::new());
      }
    }

    struct StackItem<'a> {
      node: &'a DomNode,
      exiting: bool,
      node_id: Option<NodeId>,
    }

    let mut results: Vec<NodeId> = Vec::new();
    let mut ancestors: Vec<&DomNode> = Vec::new();
    let mut stack: Vec<StackItem<'_>> = Vec::new();
    stack.push(StackItem {
      node: &snapshot_dom,
      exiting: false,
      node_id: None,
    });
    let mut next_preorder_id = 1usize;
    let mut scope_active = scope.is_none() || !use_document_snapshot;
    let mut scope_anchor: Option<OpaqueElement> =
      (!use_document_snapshot && snapshot_dom.is_element()).then_some(OpaqueElement::new(&snapshot_dom));

    while let Some(item) = stack.pop() {
      if item.exiting {
        ancestors.pop();
        if let Some(scope_id) = scope {
          if item.node_id == Some(scope_id) {
            break;
          }
        }
        continue;
      }

      let preorder_id = next_preorder_id;
      next_preorder_id += 1;
      let dom2_id = mapping.node_id_for_preorder(preorder_id);

      if let Some(dom2_id) = dom2_id {
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
      }

      stack.push(StackItem {
        node: item.node,
        exiting: true,
        node_id: dom2_id,
      });
      ancestors.push(item.node);

      let mut descend = true;
      if let Some(dom2_id) = dom2_id {
        descend = !self.node(dom2_id).inert_subtree;
      }

      if descend {
        for child in item.node.children.iter().rev() {
          stack.push(StackItem {
            node: child,
            exiting: false,
            node_id: None,
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

    let quirks_mode = match &self.node(self.root()).kind {
      NodeKind::Document { quirks_mode } => *quirks_mode,
      _ => QuirksMode::NoQuirks,
    };

    let mut selector_caches = SelectorCaches::default();
    selector_caches.set_epoch(crate::dom::next_selector_cache_epoch());

    let (snapshot_dom, mapping) = if self.is_connected_for_scripting(element) {
      (self.to_renderer_dom(), self.build_selector_preorder_mapping())
    } else {
      // If the element is disconnected (either detached or inside inert `<template>` contents), we
      // still want to be able to match selectors against it and its connected ancestors within that
      // disconnected region. Stop at the first detached ancestor or at the boundary of an inert
      // subtree so selectors do not cross template `.content` boundaries.
      let mut root = element;
      while let Some(parent) = self.parent_node(root) {
        if self.node(parent).inert_subtree {
          break;
        }
        root = parent;
      }

      let Some(dom) = self.to_renderer_dom_subtree(root) else {
        return Ok(false);
      };
      let Some(mapping) = self.build_selector_preorder_mapping_from(root) else {
        return Ok(false);
      };
      (dom, mapping)
    };

    let Some(target_preorder) = mapping.preorder_for_node_id(element) else {
      return Ok(false);
    };

    struct StackItem<'a> {
      node: &'a DomNode,
      exiting: bool,
      node_id: Option<NodeId>,
    }

    let mut ancestors: Vec<&DomNode> = Vec::new();
    let mut stack: Vec<StackItem<'_>> = Vec::new();
    stack.push(StackItem {
      node: &snapshot_dom,
      exiting: false,
      node_id: None,
    });
    let mut next_preorder_id = 1usize;

    while let Some(item) = stack.pop() {
      if item.exiting {
        ancestors.pop();
        continue;
      }

      let preorder_id = next_preorder_id;
      next_preorder_id += 1;
      let dom2_id = mapping.node_id_for_preorder(preorder_id);

      stack.push(StackItem {
        node: item.node,
        exiting: true,
        node_id: dom2_id,
      });
      ancestors.push(item.node);

      if let Some(dom2_id) = dom2_id {
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
          // If we've passed the target preorder id without finding it, the mapping/traversal is out
          // of sync; bail out defensively.
          return Ok(false);
        }
      }

      let mut descend = true;
      if let Some(dom2_id) = dom2_id {
        descend = !self.node(dom2_id).inert_subtree;
      }

      if descend {
        for child in item.node.children.iter().rev() {
          stack.push(StackItem {
            node: child,
            exiting: false,
            node_id: None,
          });
        }
      }
    }

    Ok(false)
  }
}

#[cfg(test)]
mod attrs_tests;
#[cfg(test)]
mod mapping_tests;
#[cfg(test)]
mod mutation_tests;
#[cfg(test)]
mod query_tests;
#[cfg(test)]
mod selector_query_tests;

#[cfg(test)]
mod helper_tests {
  use super::*;
  use crate::dom::parse_html;

  fn find_tag(doc: &Document, id: NodeId) -> Option<&str> {
    match &doc.node(id).kind {
      NodeKind::Element { tag_name, .. } => Some(tag_name.as_str()),
      _ => None,
    }
  }

  #[test]
  fn head_and_body_return_elements_under_html_root() {
    let root = parse_html("<!doctype html><html><head></head><body></body></html>").unwrap();
    let doc = Document::from_renderer_dom(&root);

    let head = doc.head().expect("expected head");
    let body = doc.body().expect("expected body");
    assert!(find_tag(&doc, head).is_some_and(|t| t.eq_ignore_ascii_case("head")));
    assert!(find_tag(&doc, body).is_some_and(|t| t.eq_ignore_ascii_case("body")));
  }

  #[test]
  fn head_and_body_are_none_without_html_document_element() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    doc.push_node(
      NodeKind::Element {
        tag_name: "div".to_string(),
        namespace: String::new(),
        attributes: Vec::new(),
      },
      Some(doc.root()),
      /* inert_subtree */ false,
    );

    assert_eq!(doc.head(), None);
    assert_eq!(doc.body(), None);
  }

  #[test]
  fn head_and_body_return_first_matching_child() {
    let root = parse_html(
      "<!doctype html><html><head id=a></head><head id=b></head><body id=c></body><body id=d></body></html>",
    )
    .unwrap();
    let doc = Document::from_renderer_dom(&root);

    let head = doc.head().expect("expected head");
    let body = doc.body().expect("expected body");
    assert_eq!(doc.get_attribute(head, "id").unwrap(), Some("a"));
    assert_eq!(doc.get_attribute(body, "id").unwrap(), Some("c"));
  }
}
