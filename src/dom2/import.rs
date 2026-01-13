use crate::dom::{DomNode, DomNodeType};
use selectors::context::QuirksMode;

use super::{Attribute, Document, DocumentKind, NodeId, NodeKind, SlotAssignmentMode};

fn import_renderer_attributes(attributes: &[(String, String)]) -> Vec<Attribute> {
  attributes
    .iter()
    .map(|(name, value)| Attribute::new_no_namespace(name, value))
    .collect()
}

struct Frame {
  src: *const DomNode,
  dst: NodeId,
  next_child: usize,
}

fn push_imported_node(doc: &mut Document, parent: NodeId, src: &DomNode) -> NodeId {
  // `DomNode::template_contents_are_inert()` is an HTML-document-only semantic; XML documents treat
  // `<template>` as an ordinary element.
  let inert_subtree = doc.is_html_document() && src.template_contents_are_inert();
  let kind = match &src.node_type {
    DomNodeType::Document { quirks_mode, .. } => NodeKind::Document {
      quirks_mode: *quirks_mode,
    },
    DomNodeType::ShadowRoot {
      mode,
      delegates_focus,
    } => NodeKind::ShadowRoot {
      mode: *mode,
      delegates_focus: *delegates_focus,
      slot_assignment: SlotAssignmentMode::Named,
      clonable: false,
      serializable: false,
      declarative: false,
    },
    DomNodeType::Slot {
      namespace,
      attributes,
      assigned,
    } => {
      if doc.is_html_document() {
        NodeKind::Slot {
          namespace: namespace.clone(),
          attributes: import_renderer_attributes(attributes),
          assigned: *assigned,
        }
      } else {
        NodeKind::Element {
          tag_name: "slot".to_string(),
          namespace: namespace.clone(),
          prefix: None,
          attributes: import_renderer_attributes(attributes),
        }
      }
    }
    DomNodeType::Element {
      tag_name,
      namespace,
      attributes,
    } => NodeKind::Element {
      tag_name: tag_name.clone(),
      namespace: namespace.clone(),
      prefix: None,
      attributes: import_renderer_attributes(attributes),
    },
    DomNodeType::Text { content } => NodeKind::Text {
      content: content.clone(),
    },
  };
  doc.push_node(kind, Some(parent), inert_subtree)
}

fn import_subtree(doc: &mut Document, parent: NodeId, root: &DomNode) -> NodeId {
  let root_id = push_imported_node(doc, parent, root);
  let mut stack: Vec<Frame> = vec![Frame {
    src: root as *const DomNode,
    dst: root_id,
    next_child: 0,
  }];

  while let Some(mut frame) = stack.pop() {
    // Safety: all pointers are into the `root` (borrowed) tree and we never mutate that tree.
    let src = unsafe { &*frame.src };
    if frame.next_child < src.children.len() {
      let child = &src.children[frame.next_child];
      frame.next_child += 1;
      let parent_id = frame.dst;
      stack.push(frame);

      let child_id = push_imported_node(doc, parent_id, child);
      stack.push(Frame {
        src: child as *const DomNode,
        dst: child_id,
        next_child: 0,
      });
    }
  }

  root_id
}

/// Import a renderer [`DomNode`] subtree into an existing `dom2` [`Document`], attaching it as a
/// child of an arbitrary `dom2` parent node.
///
/// This is used by DOM mutation APIs (e.g. `innerHTML`) that parse renderer DOM fragments and need
/// to import them under a temporary `DocumentFragment`.
pub fn import_domnode_into_parent(doc: &mut Document, parent: NodeId, root: &DomNode) -> NodeId {
  match &root.node_type {
    DomNodeType::Document { .. } => {
      // Import each document child directly under `parent`. Returning `parent` avoids exposing an
      // extra synthetic wrapper node that does not exist in `dom2`.
      for child in &root.children {
        import_subtree(doc, parent, child);
      }
      parent
    }
    _ => import_subtree(doc, parent, root),
  }
}

/// Import multiple renderer DOM nodes under a single `dom2` parent.
pub fn import_domnodes_into_parent(
  doc: &mut Document,
  parent: NodeId,
  nodes: &[DomNode],
) -> Vec<NodeId> {
  let mut imported: Vec<NodeId> = Vec::new();
  for node in nodes {
    match &node.node_type {
      DomNodeType::Document { .. } => {
        for child in &node.children {
          imported.push(import_subtree(doc, parent, child));
        }
      }
      _ => {
        imported.push(import_subtree(doc, parent, node));
      }
    }
  }
  imported
}

struct Dom2Frame {
  src: NodeId,
  dst: NodeId,
  next_child: usize,
}

fn push_imported_dom2_node(dst_doc: &mut Document, parent: NodeId, src_doc: &Document, src: NodeId) -> NodeId {
  let src_node = src_doc.node(src);

  let kind = match &src_node.kind {
    NodeKind::Document { quirks_mode } => NodeKind::Document {
      quirks_mode: *quirks_mode,
    },
    NodeKind::DocumentFragment => NodeKind::DocumentFragment,
    NodeKind::Comment { content } => NodeKind::Comment {
      content: content.clone(),
    },
    NodeKind::ProcessingInstruction { target, data } => NodeKind::ProcessingInstruction {
      target: target.clone(),
      data: data.clone(),
    },
    NodeKind::Doctype {
      name,
      public_id,
      system_id,
    } => NodeKind::Doctype {
      name: name.clone(),
      public_id: public_id.clone(),
      system_id: system_id.clone(),
    },
    NodeKind::ShadowRoot {
      mode,
      delegates_focus,
      slot_assignment,
      clonable,
      serializable,
      declarative,
    } => NodeKind::ShadowRoot {
      mode: *mode,
      delegates_focus: *delegates_focus,
      slot_assignment: *slot_assignment,
      clonable: *clonable,
      serializable: *serializable,
      declarative: *declarative,
    },
    NodeKind::Slot {
      namespace,
      attributes,
      assigned,
    } => NodeKind::Slot {
      namespace: namespace.clone(),
      attributes: attributes.clone(),
      assigned: *assigned,
    },
    NodeKind::Element {
      tag_name,
      namespace,
      prefix,
      attributes,
    } => NodeKind::Element {
      tag_name: tag_name.clone(),
      namespace: namespace.clone(),
      prefix: prefix.clone(),
      attributes: attributes.clone(),
    },
    NodeKind::Text { content } => NodeKind::Text {
      content: content.clone(),
    },
  };

  let id = dst_doc.push_node(kind, Some(parent), src_node.inert_subtree);
  let dst_node = &mut dst_doc.nodes[id.index()];
  dst_node.inert_subtree = src_node.inert_subtree;
  dst_node.script_already_started = src_node.script_already_started;
  dst_node.script_parser_document = src_node.script_parser_document;
  dst_node.script_force_async = src_node.script_force_async;
  dst_node.mathml_annotation_xml_integration_point = src_node.mathml_annotation_xml_integration_point;
  id
}

fn import_dom2_subtree(dst_doc: &mut Document, parent: NodeId, src_doc: &Document, root: NodeId) -> NodeId {
  let root_id = push_imported_dom2_node(dst_doc, parent, src_doc, root);

  let mut stack: Vec<Dom2Frame> = vec![Dom2Frame {
    src: root,
    dst: root_id,
    next_child: 0,
  }];

  while let Some(mut frame) = stack.pop() {
    let child_src = src_doc
      .node(frame.src)
      .children
      .get(frame.next_child)
      .copied();
    let Some(child_src) = child_src else {
      continue;
    };

    let src_parent = frame.src;
    frame.next_child += 1;
    let parent_dst = frame.dst;
    stack.push(frame);

    // Only import children that are actually connected to their parent.
    if src_doc.node(child_src).parent != Some(src_parent) {
      continue;
    }

    let child_dst = push_imported_dom2_node(dst_doc, parent_dst, src_doc, child_src);
    stack.push(Dom2Frame {
      src: child_src,
      dst: child_dst,
      next_child: 0,
    });
  }

  root_id
}

/// Deep-copy a list of dom2 nodes from `src_doc` under `dst_parent` in `dst_doc`.
///
/// This is used by HTML fragment parsing APIs (`innerHTML`, `insertAdjacentHTML`,
/// `Range.createContextualFragment`) to import a temporary parsed fragment into the live document.
pub(super) fn import_dom2_nodes_into_parent(
  dst_doc: &mut Document,
  dst_parent: NodeId,
  src_doc: &Document,
  src_roots: &[NodeId],
) -> Vec<NodeId> {
  let mut imported: Vec<NodeId> = Vec::new();
  for &root in src_roots {
    imported.push(import_dom2_subtree(dst_doc, dst_parent, src_doc, root));
  }
  imported
}

/// Import an immutable renderer [`DomNode`] tree into a fresh `dom2` [`Document`].
///
/// This enables incremental adoption of the spec-ish mutable DOM by starting from the renderer's
/// existing parsed DOM.
impl Document {
  pub fn from_renderer_dom(root: &DomNode) -> Document {
    let (quirks_mode, scripting_enabled, is_html_document) = match &root.node_type {
      DomNodeType::Document {
        quirks_mode,
        scripting_enabled,
        is_html_document,
        ..
      } => (*quirks_mode, *scripting_enabled, *is_html_document),
      _ => (QuirksMode::NoQuirks, true, true),
    };
    let mut doc = Document::new_with_scripting(quirks_mode, scripting_enabled);
    doc.kind = if is_html_document {
      DocumentKind::Html
    } else {
      DocumentKind::Xml
    };
    if !is_html_document {
      doc.has_window_event_parent = false;
    }
    let doc_root = doc.root();

    // The renderer DOM snapshot format (currently) does not preserve the document's doctype node;
    // it only stores the computed quirks mode. However, `document.doctype` is observable from JS
    // and required by WPT and real-world scripts.
    //
    // Materialize a default HTML doctype node for non-quirks documents so `document.doctype` is
    // non-null in the common case (e.g. `<!doctype html>` documents parsed through the renderer).
    if quirks_mode != QuirksMode::Quirks {
      doc.push_node(
        NodeKind::Doctype {
          name: "html".to_string(),
          public_id: String::new(),
          system_id: String::new(),
        },
        Some(doc_root),
        /* inert_subtree */ false,
      );
    }

    match &root.node_type {
      DomNodeType::Document { .. } => {
        for child in &root.children {
          import_subtree(&mut doc, doc_root, child);
        }
      }
      _ => {
        import_subtree(&mut doc, doc_root, root);
      }
    }

    if matches!(&root.node_type, DomNodeType::Document { .. }) {
      let nodes_len = doc.nodes.len();
      for idx in 0..nodes_len {
        let is_html_script = match &doc.nodes[idx].kind {
          NodeKind::Element {
            tag_name,
            namespace,
            ..
          } => {
            tag_name.eq_ignore_ascii_case("script")
              && doc.is_html_case_insensitive_namespace(namespace)
          }
          _ => false,
        };
        if !is_html_script {
          continue;
        }
        doc.nodes[idx].script_parser_document = true;
        doc.nodes[idx].script_force_async = false;
      }
    }

    doc
  }
}

/// Import a renderer [`DomNode`] tree into an existing `dom2` [`Document`], attaching it as a child
/// of the document root.
pub fn import_domnode(doc: &mut Document, root: &DomNode) -> NodeId {
  let doc_root = doc.root();
  match &root.node_type {
    DomNodeType::Document { .. } => {
      for child in &root.children {
        import_subtree(doc, doc_root, child);
      }
      doc_root
    }
    _ => import_subtree(doc, doc_root, root),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::snapshot::snapshot_dom;

  fn assert_parent_child_invariants(doc: &Document) {
    for (idx, node) in doc.nodes().iter().enumerate() {
      let id = NodeId(idx);
      if id == doc.root() {
        assert!(node.parent.is_none(), "root node must have no parent");
      } else {
        assert!(
          node.parent.is_some(),
          "non-root node must have a parent: {id:?}"
        );
      }
      for &child in &node.children {
        let child_node = doc.node(child);
        assert_eq!(
          child_node.parent,
          Some(id),
          "child must point back to parent"
        );
      }
    }
  }

  #[test]
  fn import_basic_dom_roundtrips_via_renderer_snapshot() {
    let html = concat!(
      "<!DOCTYPE html>",
      "<html><head><title>x</title></head>",
      "<body><div id=a class=b>Hello<span>world</span></div></body></html>"
    );
    let root = crate::dom::parse_html(html).unwrap();
    let doc = Document::from_renderer_dom(&root);
    assert_parent_child_invariants(&doc);
    assert!(
      doc.node(doc.root()).children.iter().any(|&id| {
        matches!(&doc.node(id).kind, NodeKind::Doctype { name, .. } if name == "html")
      }),
      "expected Document::from_renderer_dom to materialize an HTML doctype node"
    );

    let roundtrip = doc.to_renderer_dom();
    assert_eq!(snapshot_dom(&root), snapshot_dom(&roundtrip));
  }

  #[test]
  fn import_preserves_template_children_and_marks_inert_subtree() {
    let html = concat!(
      "<!DOCTYPE html>",
      "<html><body><template><span>in</span></template><div>out</div></body></html>"
    );
    let root = crate::dom::parse_html(html).unwrap();
    let doc = Document::from_renderer_dom(&root);

    let template_id = doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| match &node.kind {
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("template") => {
          Some(NodeId(idx))
        }
        _ => None,
      })
      .expect("template element not found");

    let template_node = doc.node(template_id);
    assert!(
      template_node.inert_subtree,
      "template should mark inert_subtree"
    );
    assert!(
      !template_node.children.is_empty(),
      "template contents must still be present in the tree"
    );
  }

  #[test]
  fn import_includes_shadow_root_and_slot_nodes() {
    let html = concat!(
      "<!DOCTYPE html>",
      "<html><body>",
      "<div id=host>",
      "<template shadowroot=open>",
      "<slot name=s></slot><span>shadow</span>",
      "</template>",
      "<p>light</p>",
      "</div>",
      "</body></html>"
    );
    let root = crate::dom::parse_html(html).unwrap();
    let doc = Document::from_renderer_dom(&root);

    let mut saw_shadow_root = false;
    let mut saw_slot = false;
    for node in doc.nodes() {
      match &node.kind {
        NodeKind::ShadowRoot { .. } => saw_shadow_root = true,
        NodeKind::Slot { .. } => saw_slot = true,
        _ => {}
      }
    }
    assert!(saw_shadow_root, "expected a ShadowRoot node");
    assert!(saw_slot, "expected a Slot node");

    // Ensure the imported tree can round-trip through renderer snapshots.
    let roundtrip = doc.to_renderer_dom();
    assert_eq!(snapshot_dom(&root), snapshot_dom(&roundtrip));
  }

  #[test]
  fn import_preserves_script_force_async_false_for_parser_inserted_scripts() {
    let html = "<!doctype html><html><body><script id=s></script></body></html>";
    let root = crate::dom::parse_html(html).unwrap();
    let doc = Document::from_renderer_dom(&root);
    let script = doc
      .get_element_by_id("s")
      .expect("script element not found");
    assert!(
      !doc.node(script).script_force_async,
      "scripts parsed from HTML should have force_async=false when imported into dom2"
    );
  }

  #[test]
  fn import_handles_deep_trees_without_recursion_overflow() {
    // A depth that would almost certainly overflow recursive import on typical test stacks.
    const DEPTH: usize = 50_000;

    let mut node = DomNode {
      node_type: DomNodeType::Text {
        content: "leaf".to_string(),
      },
      children: Vec::new(),
    };

    for _ in 0..DEPTH {
      node = DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: "".to_string(),
          attributes: Vec::new(),
        },
        children: vec![node],
      };
    }

    let root = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
        scripting_enabled: true,
        is_html_document: true,
      },
      children: vec![node],
    };

    let doc = Document::from_renderer_dom(&root);
    // Document root + DEPTH elements + leaf text
    assert_eq!(doc.nodes_len(), DEPTH + 2);
    assert_parent_child_invariants(&doc);
  }
}
