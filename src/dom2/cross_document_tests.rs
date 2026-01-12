use crate::dom::HTML_NAMESPACE;
use selectors::context::QuirksMode;

use super::{Document, DomError, NodeId, NodeKind};

fn find_first_html_script(doc: &Document) -> NodeId {
  doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| match &node.kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } if tag_name.eq_ignore_ascii_case("script")
        && (namespace.is_empty() || namespace == HTML_NAMESPACE) =>
      {
        Some(NodeId::from_index(idx))
      }
      _ => None,
    })
    .expect("expected an HTML <script> element")
}

fn assert_subtree_kinds_match(src: &Document, src_root: NodeId, dst: &Document, dst_root: NodeId) {
  let mut stack: Vec<(NodeId, NodeId)> = vec![(src_root, dst_root)];
  while let Some((src_id, dst_id)) = stack.pop() {
    let src_node = src.node(src_id);
    let dst_node = dst.node(dst_id);
    assert_eq!(
      src_node.kind, dst_node.kind,
      "kind mismatch for src={src_id:?} dst={dst_id:?}"
    );
    assert_eq!(
      src_node.children.len(),
      dst_node.children.len(),
      "child count mismatch for src={src_id:?} dst={dst_id:?}"
    );
    for (&src_child, &dst_child) in src_node.children.iter().zip(dst_node.children.iter()) {
      assert_eq!(
        dst.node(dst_child).parent,
        Some(dst_id),
        "dst child must point back to parent"
      );
      stack.push((src_child, dst_child));
    }
  }
}

#[test]
fn import_basic_element_and_text_subtree() {
  let mut src = Document::new(QuirksMode::NoQuirks);
  let div = src.create_element("div", HTML_NAMESPACE);
  src.set_attribute(div, "id", "a").unwrap();
  src.append_child(src.root(), div).unwrap();
  let text1 = src.create_text("Hello");
  src.append_child(div, text1).unwrap();
  let span = src.create_element("span", HTML_NAMESPACE);
  src.append_child(div, span).unwrap();
  let text2 = src.create_text("world");
  src.append_child(span, text2).unwrap();

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let imported = dst.import_node_from(&src, div, /* deep */ true).unwrap();
  assert_eq!(dst.parent(imported).unwrap(), None);
  assert_subtree_kinds_match(&src, div, &dst, imported);
}

#[test]
fn import_document_node_is_not_supported() {
  let mut src = Document::new(QuirksMode::NoQuirks);
  let detached_doc = src.clone_node(src.root(), /* deep */ false).unwrap();

  let mut dst = Document::new(QuirksMode::NoQuirks);
  assert_eq!(
    dst.import_node_from(&src, src.root(), /* deep */ false)
      .unwrap_err(),
    DomError::NotSupportedError
  );
  assert_eq!(
    dst.import_node_from(&src, detached_doc, /* deep */ false)
      .unwrap_err(),
    DomError::NotSupportedError
  );
}

#[test]
fn import_html_script_matches_clone_semantics() {
  // Parser-inserted scripts have `parser_document=true` and `force_async=false`, but cloning must
  // clear those and recompute `force_async` from the presence of an `async` attribute.
  let html = "<!doctype html><html><head><script id=s></script></head></html>";
  let mut src = crate::dom2::parse_html(html).unwrap();
  let script = find_first_html_script(&src);
  assert!(
    src.node(script).script_parser_document && !src.node(script).script_force_async,
    "expected parser-inserted defaults"
  );
  src.set_script_already_started(script, true).unwrap();

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let imported = dst.import_node_from(&src, script, /* deep */ false).unwrap();

  let imported_node = dst.node(imported);
  assert!(imported_node.script_already_started);
  assert!(imported_node.script_force_async);
  assert!(!imported_node.script_parser_document);
  assert_eq!(dst.get_attribute(imported, "id").unwrap(), Some("s"));
  assert!(!dst.has_attribute(imported, "async").unwrap());

  // Case-insensitive `async` attribute detection.
  let mut src2 = Document::new(QuirksMode::NoQuirks);
  let script2 = src2.create_element("script", HTML_NAMESPACE);
  src2.set_attribute(script2, "ASYNC", "").unwrap();
  src2.set_script_parser_document(script2, true).unwrap();
  src2.set_script_already_started(script2, true).unwrap();
  // Ensure import does not just copy the source flag.
  src2.node_mut(script2).script_force_async = true;

  let mut dst2 = Document::new(QuirksMode::NoQuirks);
  let imported2 = dst2.import_node_from(&src2, script2, /* deep */ false).unwrap();

  let imported_node2 = dst2.node(imported2);
  assert!(imported_node2.script_already_started);
  assert!(!imported_node2.script_force_async);
  assert!(!imported_node2.script_parser_document);
  assert!(dst2.has_attribute(imported2, "async").unwrap());
  match &imported_node2.kind {
    NodeKind::Element { attributes, .. } => {
      assert!(
        attributes.iter().any(|(k, v)| k == "ASYNC" && v.is_empty()),
        "expected attributes to be preserved exactly"
      );
    }
    _ => panic!("expected script to be an Element node"),
  }
}

#[test]
fn import_handles_deep_trees_without_recursion_overflow() {
  // A depth that would almost certainly overflow recursive import on typical test stacks.
  const DEPTH: usize = 50_000;

  let mut src = Document::new(QuirksMode::NoQuirks);
  let mut current = src.create_element("div", HTML_NAMESPACE);
  src.append_child(src.root(), current).unwrap();
  let root = current;

  for _ in 1..DEPTH {
    let child = src.create_element("div", HTML_NAMESPACE);
    src.append_child(current, child).unwrap();
    current = child;
  }
  let leaf = src.create_text("leaf");
  src.append_child(current, leaf).unwrap();

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let imported = dst.import_node_from(&src, root, /* deep */ true).unwrap();

  // Destination doc root + imported subtree (DEPTH elements + leaf text)
  assert_eq!(dst.nodes_len(), DEPTH + 2);

  // Walk down the chain to validate parent pointers and ensure no stack overflows.
  let mut current = imported;
  for _ in 1..DEPTH {
    let children = dst.children(current).unwrap();
    assert_eq!(children.len(), 1);
    let child = children[0];
    assert_eq!(dst.parent(child).unwrap(), Some(current));
    current = child;
    assert!(matches!(dst.node(current).kind, NodeKind::Element { .. }));
  }
  let children = dst.children(current).unwrap();
  assert_eq!(children.len(), 1);
  let leaf = children[0];
  assert_eq!(dst.parent(leaf).unwrap(), Some(current));
  assert_eq!(dst.text_data(leaf).unwrap(), "leaf");
}

