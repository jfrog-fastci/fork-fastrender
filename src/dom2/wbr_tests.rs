use crate::debug::snapshot::snapshot_dom;
use crate::dom::{enumerate_dom_ids, DomNode, DomNodeType};
use selectors::context::QuirksMode;

use super::{Document, NodeId, NodeKind};

fn push_element(doc: &mut Document, parent: NodeId, tag_name: &str) -> NodeId {
  doc.push_node(
    NodeKind::Element {
      tag_name: tag_name.to_string(),
      namespace: String::new(),
      attributes: Vec::new(),
    },
    Some(parent),
    /* inert_subtree */ false,
  )
}

fn push_text(doc: &mut Document, parent: NodeId, content: &str) -> NodeId {
  doc.push_node(
    NodeKind::Text {
      content: content.to_string(),
    },
    Some(parent),
    /* inert_subtree */ false,
  )
}

fn build_dom2_doc_with_wbr() -> (Document, NodeId) {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let html = push_element(&mut doc, root, "html");
  let _head = push_element(&mut doc, html, "head");
  let body = push_element(&mut doc, html, "body");
  push_text(&mut doc, body, "a");
  let wbr = push_element(&mut doc, body, "wbr");
  push_text(&mut doc, body, "b");

  (doc, wbr)
}

#[test]
fn to_renderer_dom_injects_zwsp_for_wbr() {
  let (doc, _wbr) = build_dom2_doc_with_wbr();
  let snapshot = doc.to_renderer_dom();

  let expected = crate::dom::parse_html(concat!(
    "<!DOCTYPE html>",
    "<html><head></head><body>a<wbr>b</body></html>"
  ))
  .unwrap();

  assert_eq!(
    snapshot_dom(&expected),
    snapshot_dom(&snapshot),
    "dom2 snapshots should preserve renderer `<wbr>` ZWSP injection"
  );
}

#[test]
fn to_renderer_dom_does_not_double_inject_when_imported_from_renderer_dom() {
  let expected = crate::dom::parse_html(concat!(
    "<!DOCTYPE html>",
    "<html><head></head><body>a<wbr>b</body></html>"
  ))
  .unwrap();
  let doc = Document::from_renderer_dom(&expected);
  let roundtrip = doc.to_renderer_dom();

  assert_eq!(
    snapshot_dom(&expected),
    snapshot_dom(&roundtrip),
    "imported renderer DOM should not get an extra `<wbr>` ZWSP injected"
  );
}

#[test]
fn to_renderer_dom_with_mapping_accounts_for_synthetic_wbr_zwsp_node() {
  let (doc, wbr_id) = build_dom2_doc_with_wbr();
  let snapshot = doc.to_renderer_dom_with_mapping();

  let renderer_ids = enumerate_dom_ids(&snapshot.dom);
  assert_eq!(
    snapshot.mapping.preorder_to_node_id.len(),
    renderer_ids.len() + 1,
    "mapping should include a preorder slot for every renderer node (plus the unused 0 slot)"
  );
  assert_eq!(
    snapshot.mapping.node_id_to_preorder.len(),
    doc.nodes_len(),
    "mapping reverse table should be sized to the dom2 node arena"
  );

  // Find the `<wbr>` element and its injected ZWSP child in the renderer snapshot DOM.
  let mut stack: Vec<&DomNode> = vec![&snapshot.dom];
  let mut wbr_node: Option<&DomNode> = None;
  while let Some(node) = stack.pop() {
    if let DomNodeType::Element { tag_name, .. } = &node.node_type {
      if tag_name.eq_ignore_ascii_case("wbr") {
        wbr_node = Some(node);
        break;
      }
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  let wbr_node = wbr_node.expect("missing <wbr> node in renderer snapshot");
  assert_eq!(
    wbr_node.children.len(),
    1,
    "<wbr> should have one synthetic ZWSP text child in renderer snapshots"
  );
  let zwsp_node = &wbr_node.children[0];
  assert_eq!(zwsp_node.text_content(), Some("\u{200B}"));

  let wbr_preorder = *renderer_ids
    .get(&(wbr_node as *const DomNode))
    .expect("missing preorder id for <wbr>");
  let zwsp_preorder = *renderer_ids
    .get(&(zwsp_node as *const DomNode))
    .expect("missing preorder id for ZWSP child");
  assert_ne!(
    wbr_preorder, zwsp_preorder,
    "synthetic ZWSP child should have its own renderer preorder id"
  );

  assert_eq!(
    snapshot.mapping.node_id_for_preorder(wbr_preorder),
    Some(wbr_id),
    "<wbr> preorder id should map to the `<wbr>` dom2 node"
  );
  assert_eq!(
    snapshot.mapping.node_id_for_preorder(zwsp_preorder),
    Some(wbr_id),
    "synthetic ZWSP preorder id should map to the parent `<wbr>` dom2 node"
  );
  assert_eq!(
    snapshot.mapping.preorder_for_node_id(wbr_id),
    Some(wbr_preorder),
    "reverse mapping for `<wbr>` should point at the element itself, not the synthetic child"
  );
}

