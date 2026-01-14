use crate::error::{Error, ParseError, Result};
use crate::dom::HTML_NAMESPACE;
use crate::xml::{markup_for_roxmltree_with_doctypes, ExtractedDoctype};
use roxmltree::Document as RoDocument;

use super::{Attribute, Document, NodeId, NodeKind, NULL_NAMESPACE};

#[derive(Clone, Copy)]
enum TopLevel<'a, 'input> {
  RoNode(roxmltree::Node<'a, 'input>),
  Doctype(&'a ExtractedDoctype),
}

fn roxmltree_node_start(node: roxmltree::Node<'_, '_>) -> Option<usize> {
  let range = node.range();
  (!range.is_empty()).then_some(range.start)
}

fn import_roxmltree_subtree(doc: &mut Document, parent: NodeId, root: roxmltree::Node<'_, '_>) {
  struct Frame<'a, 'input> {
    parent: NodeId,
    iter: roxmltree::Children<'a, 'input>,
  }

  fn push_child(doc: &mut Document, parent: NodeId, kind: NodeKind) -> NodeId {
    // Create the node detached first so we can run live-range pre-insert steps before mutating the
    // parent's child list and the node's parent pointer.
    let id = doc.push_node(kind, None, /* inert_subtree */ false);
    let idx = doc.nodes[parent.index()].children.len();
    if !doc.ranges.is_empty() {
      doc.live_range_pre_insert_steps(
        parent,
        doc.tree_child_index_from_raw_index_for_range(parent, idx),
        doc.inserted_tree_children_count_for_range(parent, &[id]),
      );
    }
    doc.nodes[parent.index()].children.push(id);
    doc.nodes[id.index()].parent = Some(parent);
    id
  }

  fn push_from_roxmltree(doc: &mut Document, parent: NodeId, node: roxmltree::Node<'_, '_>) -> Option<NodeId> {
    if node.is_element() {
      let tag = node.tag_name();
      let (mut prefix, local_name) = match tag.name().split_once(':') {
        Some((prefix, local_name)) => (Some(prefix.to_string()), local_name.to_string()),
        None => (None, tag.name().to_string()),
      };
      let namespace = match tag.namespace() {
        None => NULL_NAMESPACE.to_string(),
        Some(ns) if ns == HTML_NAMESPACE => String::new(),
        Some(ns) => ns.to_string(),
      };
      if namespace == NULL_NAMESPACE {
        prefix = None;
      }
      let mut attributes: Vec<Attribute> = Vec::new();
      attributes.reserve(node.attributes().len());
      for attr in node.attributes() {
        let (mut prefix, local_name) = match attr.name().split_once(':') {
          Some((prefix, local_name)) => (Some(prefix.to_string()), local_name.to_string()),
          None => (None, attr.name().to_string()),
        };
        let namespace = match attr.namespace() {
          None => NULL_NAMESPACE.to_string(),
          Some(ns) if ns == HTML_NAMESPACE => String::new(),
          Some(ns) => ns.to_string(),
        };
        if namespace == NULL_NAMESPACE {
          prefix = None;
        }
        attributes.push(Attribute {
          namespace,
          prefix,
          local_name,
          value: attr.value().to_string(),
        });
      }
      let kind =
        if namespace.is_empty() && local_name.eq_ignore_ascii_case("slot") && prefix.is_none() {
          NodeKind::Slot {
            namespace: namespace.clone(),
            attributes,
            assigned: false,
          }
        } else {
          NodeKind::Element {
            tag_name: local_name,
            namespace,
            prefix,
            attributes,
          }
        };
      let id = push_child(doc, parent, kind);
      return Some(id);
    }

    if node.is_text() {
      let Some(text) = node.text() else {
        return None;
      };
      let id = push_child(
        doc,
        parent,
        NodeKind::Text {
          content: text.to_string(),
        },
      );
      return Some(id);
    }

    if node.is_comment() {
      let text = node.text().unwrap_or("");
      let id = push_child(
        doc,
        parent,
        NodeKind::Comment {
          content: text.to_string(),
        },
      );
      return Some(id);
    }

    if node.is_pi() {
      let Some(pi) = node.pi() else {
        return None;
      };
      let id = push_child(
        doc,
        parent,
        NodeKind::ProcessingInstruction {
          target: pi.target.to_string(),
          data: pi.value.unwrap_or("").to_string(),
        },
      );
      return Some(id);
    }

    None
  }

  let Some(root_id) = push_from_roxmltree(doc, parent, root) else {
    return;
  };

  let mut stack: Vec<Frame<'_, '_>> = vec![Frame {
    parent: root_id,
    iter: root.children(),
  }];

  while let Some(mut frame) = stack.pop() {
    let Some(child) = frame.iter.next() else {
      continue;
    };

    // Continue iterating siblings after processing this child.
    let parent_id = frame.parent;
    stack.push(frame);

    let Some(child_id) = push_from_roxmltree(doc, parent_id, child) else {
      continue;
    };

    // Only element nodes can have meaningful child content in the DOM tree; comments and PIs don't
    // have children in `roxmltree` but calling `.children()` is cheap.
    stack.push(Frame {
      parent: child_id,
      iter: child.children(),
    });
  }
}

fn create_doctype_node(doc: &mut Document, parent: NodeId, doctype: &ExtractedDoctype) -> NodeId {
  // `parse_xml` can insert doctypes after other top-level nodes have already been imported. Keep
  // live ranges up to date by running the live-range pre-insert steps before mutating the child
  // list / parent pointer, even though XML documents currently have scripting disabled.
  let id = doc.push_node(
    NodeKind::Doctype {
      name: doctype.name.clone(),
      public_id: doctype.public_id.clone(),
      system_id: doctype.system_id.clone(),
    },
    None,
    /* inert_subtree */ false,
  );
  let idx = doc.nodes[parent.index()].children.len();
  if !doc.ranges.is_empty() {
    doc.live_range_pre_insert_steps(
      parent,
      doc.tree_child_index_from_raw_index_for_range(parent, idx),
      doc.inserted_tree_children_count_for_range(parent, &[id]),
    );
  }
  doc.nodes[parent.index()].children.push(id);
  doc.nodes[id.index()].parent = Some(parent);
  id
}

/// Parse XML into a [`dom2::Document`](crate::dom2::Document) using `roxmltree`.
///
/// This is used for `DOMParser` XML flavors (`text/xml`, `application/xml`, `image/svg+xml`, ...).
///
/// Notes:
/// - `roxmltree` rejects `<!DOCTYPE ...>` declarations; we sanitize them by replacing their bytes
///   with spaces and re-inject the extracted doctype metadata into the resulting `dom2` tree.
pub fn parse_xml(xml: &str) -> Result<Document> {
  let (sanitized, doctypes) = markup_for_roxmltree_with_doctypes(xml);

  let ro_doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    RoDocument::parse(sanitized.as_ref())
  })) {
    Ok(Ok(doc)) => doc,
    Ok(Err(err)) => {
      return Err(Error::Parse(ParseError::InvalidHtml {
        message: format!("Invalid XML: {err}"),
        line: 0,
      }))
    }
    Err(_) => {
      return Err(Error::Parse(ParseError::InvalidHtml {
        message: "XML parser panicked".to_string(),
        line: 0,
      }))
    }
  };

  // DOMParser XML flavors return an XML document with scripting disabled.
  let mut doc = Document::new_xml();
  let doc_root = doc.root();

  // Collect top-level nodes.
  let ro_root = ro_doc.root();
  let mut ro_children: Vec<roxmltree::Node<'_, '_>> = Vec::new();
  let mut all_have_ranges = true;
  for child in ro_root.children() {
    if child.is_text() && child.text().is_some_and(|t| t.trim().is_empty()) {
      // Per DOM, `Document` cannot have Text children. Ignore whitespace between top-level nodes.
      continue;
    }
    all_have_ranges &= roxmltree_node_start(child).is_some();
    ro_children.push(child);
  }

  if all_have_ranges {
    let mut items: Vec<(usize, TopLevel<'_, '_>)> = Vec::with_capacity(ro_children.len() + doctypes.len());
    for node in &ro_children {
      let Some(start) = roxmltree_node_start(*node) else {
        continue;
      };
      items.push((start, TopLevel::RoNode(*node)));
    }
    for doctype in &doctypes {
      items.push((doctype.start, TopLevel::Doctype(doctype)));
    }

    items.sort_by_key(|(start, _)| *start);
    for (_start, item) in items {
      match item {
        TopLevel::RoNode(node) => import_roxmltree_subtree(&mut doc, doc_root, node),
        TopLevel::Doctype(doctype) => {
          let _ = create_doctype_node(&mut doc, doc_root, doctype);
        }
      }
    }

    return Ok(doc);
  }

  // Fallback: preserve the original roxmltree top-level order but insert doctypes before the first
  // document element.
  for node in ro_children {
    import_roxmltree_subtree(&mut doc, doc_root, node);
  }
  if doctypes.is_empty() {
    return Ok(doc);
  }

  let Some(doc_el) = doc.document_element() else {
    // No element children: append doctypes at the end.
    for doctype in doctypes {
      let _ = create_doctype_node(&mut doc, doc_root, &doctype);
    }
    return Ok(doc);
  };

  let insert_at = doc.nodes[doc_root.index()]
    .children
    .iter()
    .position(|&child| child == doc_el)
    .unwrap_or(doc.nodes[doc_root.index()].children.len());

  let mut idx = insert_at;
  for doctype in doctypes {
    let id = doc.push_node(
      NodeKind::Doctype {
        name: doctype.name,
        public_id: doctype.public_id,
        system_id: doctype.system_id,
      },
      None,
      /* inert_subtree */ false,
    );
    doc.live_range_pre_insert_steps(doc_root, idx, 1);
    doc.nodes[doc_root.index()].children.insert(idx, id);
    doc.nodes[id.index()].parent = Some(doc_root);
    idx += 1;
  }

  Ok(doc)
}
