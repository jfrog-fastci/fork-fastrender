use crate::dom::HTML_NAMESPACE;
use selectors::context::QuirksMode;

use super::{clone_node_into_document, Document, DomError, NodeId, NodeKind};

fn id_attribute(kind: &NodeKind) -> Option<&str> {
  match kind {
    NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes
      .iter()
      .find(|(k, _)| k.eq_ignore_ascii_case("id"))
      .map(|(_, v)| v.as_str()),
    _ => None,
  }
}

fn find_in_subtree_by_id(doc: &Document, root: NodeId, id: &str) -> Option<NodeId> {
  doc
    .subtree_preorder(root)
    .find(|&node_id| id_attribute(&doc.node(node_id).kind) == Some(id))
}

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

fn assert_node_kind_equivalent(src: &NodeKind, dst: &NodeKind) {
  match (src, dst) {
    (
      NodeKind::Slot {
        namespace: src_ns,
        attributes: src_attrs,
        ..
      },
      NodeKind::Slot {
        namespace: dst_ns,
        attributes: dst_attrs,
        ..
      },
    ) => {
      // `assigned` is derived state; it is not required to be preserved by cloning/importing/adopting.
      assert_eq!(src_ns, dst_ns, "slot namespace mismatch");
      assert_eq!(src_attrs, dst_attrs, "slot attributes mismatch");
    }
    _ => assert_eq!(src, dst, "node kind mismatch"),
  }
}

fn assert_subtree_kinds_match(src: &Document, src_root: NodeId, dst: &Document, dst_root: NodeId) {
  let mut stack: Vec<(NodeId, NodeId)> = vec![(src_root, dst_root)];
  while let Some((src_id, dst_id)) = stack.pop() {
    let src_node = src.node(src_id);
    let dst_node = dst.node(dst_id);
    assert_node_kind_equivalent(&src_node.kind, &dst_node.kind);
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

fn find_first_shadow_root(doc: &Document) -> NodeId {
  doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| {
      matches!(&node.kind, NodeKind::ShadowRoot { .. }).then_some(NodeId::from_index(idx))
    })
    .expect("expected a ShadowRoot node")
}

fn count_subtree_nodes(doc: &Document, root: NodeId) -> usize {
  let mut count = 0usize;
  let mut stack: Vec<NodeId> = vec![root];
  while let Some(id) = stack.pop() {
    count += 1;
    stack.extend_from_slice(&doc.node(id).children);
  }
  count
}

fn build_shadow_host_source_document() -> (Document, NodeId) {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host data-x=y>",
    "<!--c-->",
    "<template shadowroot=open shadowrootdelegatesfocus>",
    "<slot id=slot name=s><span id=fallback>fallback</span></slot>",
    "<span id=shadow_span>shadow</span>",
    "</template>",
    "<p id=light>light</p>",
    "<script id=s></script>",
    "</div>",
    "</body></html>"
  );
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let host = doc.get_element_by_id("host").expect("host element not found");

  // Ensure the subtree contains a ProcessingInstruction node kind (not produced by the HTML parser).
  let pi = doc.push_node(
    NodeKind::ProcessingInstruction {
      target: "xml".to_string(),
      data: "version=\"1.0\"".to_string(),
    },
    None,
    /* inert_subtree */ false,
  );
  doc.append_child(host, pi).unwrap();

  (doc, host)
}

#[test]
fn clone_basic_element_and_text_subtree_across_documents() {
  let mut src = Document::new(QuirksMode::NoQuirks);
  let div = src.create_element("div", HTML_NAMESPACE);
  src.set_attribute(div, "class", "a").unwrap();
  let text = src.create_text("hello");
  src.append_child(div, text).unwrap();

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let cloned = dst.clone_node_from(&src, div, /* deep */ true).unwrap();

  assert_eq!(dst.parent(cloned).unwrap(), None);
  assert_eq!(dst.get_attribute(cloned, "class").unwrap(), Some("a"));

  let children = dst.children(cloned).unwrap();
  assert_eq!(children.len(), 1);
  let child = children[0];
  assert_eq!(dst.parent(child).unwrap(), Some(cloned));
  assert_eq!(dst.text_data(child).unwrap(), "hello");
}

#[test]
fn clone_node_into_document_returns_complete_mapping_for_element_subtree() {
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

  let expected_ids: Vec<NodeId> = src.subtree_preorder(div).collect();

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let (cloned, mapping) = clone_node_into_document(&src, div, &mut dst, /* deep */ true).unwrap();

  assert_eq!(dst.parent(cloned).unwrap(), None);
  assert_subtree_kinds_match(&src, div, &dst, cloned);

  assert_eq!(
    mapping.len(),
    expected_ids.len(),
    "expected mapping to contain one entry per cloned node"
  );
  let mut map = std::collections::HashMap::<NodeId, NodeId>::new();
  for (old, new) in mapping {
    map.insert(old, new);
  }
  assert_eq!(map.len(), expected_ids.len(), "source ids must be unique");
  assert_eq!(map.get(&div).copied(), Some(cloned));

  for old_id in expected_ids {
    assert!(
      map.contains_key(&old_id),
      "missing mapping for cloned node id {old_id:?}"
    );
  }
}

#[test]
fn clone_node_into_document_returns_complete_mapping_for_document_fragment() {
  let mut src = Document::new(QuirksMode::NoQuirks);
  let frag = src.create_document_fragment();
  let a = src.create_element("a", HTML_NAMESPACE);
  let a_text = src.create_text("link");
  src.append_child(a, a_text).unwrap();
  let b = src.create_element("b", HTML_NAMESPACE);
  let b_text = src.create_text("bold");
  src.append_child(b, b_text).unwrap();
  src.append_child(frag, a).unwrap();
  src.append_child(frag, b).unwrap();

  let expected_ids: Vec<NodeId> = src.subtree_preorder(frag).collect();

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let (cloned, mapping) = clone_node_into_document(&src, frag, &mut dst, /* deep */ true).unwrap();

  assert_eq!(dst.parent(cloned).unwrap(), None);
  assert_subtree_kinds_match(&src, frag, &dst, cloned);
  assert!(matches!(dst.node(cloned).kind, NodeKind::DocumentFragment));

  assert_eq!(mapping.len(), expected_ids.len());
  let mut map = std::collections::HashMap::<NodeId, NodeId>::new();
  for (old, new) in mapping {
    map.insert(old, new);
  }
  assert_eq!(map.len(), expected_ids.len());
  assert_eq!(map.get(&frag).copied(), Some(cloned));
  for old_id in expected_ids {
    assert!(
      map.contains_key(&old_id),
      "missing mapping for cloned node id {old_id:?}"
    );
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
fn adopt_node_from_returns_mapping_and_detaches_source_subtree() {
  let mut src = Document::new(QuirksMode::NoQuirks);
  let src_root = src.root();

  let div = src.create_element("div", HTML_NAMESPACE);
  src.set_attribute(div, "id", "a").unwrap();
  let span = src.create_element("span", HTML_NAMESPACE);
  let text = src.create_text("hi");
  src.append_child(span, text).unwrap();
  src.append_child(div, span).unwrap();
  src.append_child(src_root, div).unwrap();

  let expected_size = count_subtree_nodes(&src, div);

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let adopted = dst.adopt_node_from(&mut src, div).unwrap();

  assert_eq!(adopted.mapping.len(), expected_size);
  assert_eq!(dst.parent(adopted.new_root).unwrap(), None);

  assert!(
    adopted
      .mapping
      .iter()
      .any(|(old, new)| *old == div && *new == adopted.new_root),
    "expected mapping to include adopted root"
  );

  assert!(!src.children(src_root).unwrap().contains(&div));
  assert_eq!(src.parent(div).unwrap(), None);
  assert_eq!(src.parent(span).unwrap(), None);
  assert_eq!(src.parent(text).unwrap(), None);
}

#[test]
fn import_node_from_node_kind_coverage_including_shadow_dom_and_script_flags() {
  let (mut src, host) = build_shadow_host_source_document();

  // Detach the host so the imported nodes originate from a disconnected subtree.
  let host_parent = src.parent(host).unwrap().expect("host should be connected");
  src.remove_child(host_parent, host).unwrap();

  let src_comment = src
    .subtree_preorder(host)
    .find(|&id| matches!(src.node(id).kind, NodeKind::Comment { .. }))
    .expect("comment node not found");
  let src_pi = src
    .subtree_preorder(host)
    .find(|&id| matches!(src.node(id).kind, NodeKind::ProcessingInstruction { .. }))
    .expect("processing instruction node not found");
  let src_slot = src
    .subtree_preorder(host)
    .find(|&id| matches!(src.node(id).kind, NodeKind::Slot { .. }))
    .expect("slot node not found");
  let src_script = find_in_subtree_by_id(&src, host, "s").expect("script node not found");
  assert!(
    src.node(src_script).script_parser_document,
    "expected parser-inserted script in source subtree"
  );

  let src_text = src.create_text("hello");
  let src_doctype = src.create_doctype("html", "", "");

  let src_fragment = {
    let frag = src.create_document_fragment();
    let div = src.create_element("div", HTML_NAMESPACE);
    src.set_attribute(div, "id", "frag_div").unwrap();
    src.append_child(frag, div).unwrap();
    let t = src.create_text("frag");
    src.append_child(div, t).unwrap();
    frag
  };

  // Element root: deep=false.
  {
    let mut dst = Document::new(QuirksMode::NoQuirks);
    let imported = dst.import_node_from(&src, host, /* deep */ false).unwrap();
    assert_eq!(dst.parent(imported).unwrap(), None, "imported root must be detached");
    assert_node_kind_equivalent(&src.node(host).kind, &dst.node(imported).kind);
    assert!(
      dst.node(imported).children.is_empty(),
      "deep=false should not clone children"
    );
  }

  // Element root: deep=true (should clone ShadowRoot+Slot descendants and clear script flags).
  {
    let mut dst = Document::new(QuirksMode::NoQuirks);
    let imported = dst.import_node_from(&src, host, /* deep */ true).unwrap();
    assert_eq!(dst.parent(imported).unwrap(), None, "imported root must be detached");
    assert_subtree_kinds_match(&src, host, &dst, imported);

    assert!(
      dst
        .subtree_preorder(imported)
        .any(|id| matches!(dst.node(id).kind, NodeKind::ShadowRoot { .. })),
      "expected ShadowRoot node in imported subtree"
    );
    assert!(
      dst
        .subtree_preorder(imported)
        .any(|id| matches!(dst.node(id).kind, NodeKind::Slot { .. })),
      "expected Slot node in imported subtree"
    );

    let imported_script =
      find_in_subtree_by_id(&dst, imported, "s").expect("imported script node not found");
    assert!(
      !dst.node(imported_script).script_parser_document,
      "imported scripts must not be parser-inserted"
    );
  }

  // Text.
  {
    let mut dst = Document::new(QuirksMode::NoQuirks);
    let imported = dst.import_node_from(&src, src_text, /* deep */ false).unwrap();
    assert_eq!(dst.parent(imported).unwrap(), None);
    assert_node_kind_equivalent(&src.node(src_text).kind, &dst.node(imported).kind);
  }

  // Comment.
  {
    let mut dst = Document::new(QuirksMode::NoQuirks);
    let imported = dst.import_node_from(&src, src_comment, /* deep */ false).unwrap();
    assert_eq!(dst.parent(imported).unwrap(), None);
    assert_node_kind_equivalent(&src.node(src_comment).kind, &dst.node(imported).kind);
  }

  // ProcessingInstruction.
  {
    let mut dst = Document::new(QuirksMode::NoQuirks);
    let imported = dst.import_node_from(&src, src_pi, /* deep */ false).unwrap();
    assert_eq!(dst.parent(imported).unwrap(), None);
    assert_node_kind_equivalent(&src.node(src_pi).kind, &dst.node(imported).kind);
  }

  // Doctype.
  {
    let mut dst = Document::new(QuirksMode::NoQuirks);
    let imported = dst
      .import_node_from(&src, src_doctype, /* deep */ false)
      .unwrap();
    assert_eq!(dst.parent(imported).unwrap(), None);
    assert_node_kind_equivalent(&src.node(src_doctype).kind, &dst.node(imported).kind);
  }

  // DocumentFragment deep=false.
  {
    let mut dst = Document::new(QuirksMode::NoQuirks);
    let imported = dst
      .import_node_from(&src, src_fragment, /* deep */ false)
      .unwrap();
    assert_eq!(dst.parent(imported).unwrap(), None);
    assert_node_kind_equivalent(&src.node(src_fragment).kind, &dst.node(imported).kind);
    assert!(
      dst.node(imported).children.is_empty(),
      "deep=false should not clone fragment children"
    );
  }

  // DocumentFragment deep=true.
  {
    let mut dst = Document::new(QuirksMode::NoQuirks);
    let imported = dst
      .import_node_from(&src, src_fragment, /* deep */ true)
      .unwrap();
    assert_eq!(dst.parent(imported).unwrap(), None);
    assert_subtree_kinds_match(&src, src_fragment, &dst, imported);
  }

  // Slot deep=false.
  {
    let mut dst = Document::new(QuirksMode::NoQuirks);
    let imported = dst.import_node_from(&src, src_slot, /* deep */ false).unwrap();
    assert_eq!(dst.parent(imported).unwrap(), None);
    assert_node_kind_equivalent(&src.node(src_slot).kind, &dst.node(imported).kind);
    assert!(
      dst.node(imported).children.is_empty(),
      "deep=false should not clone slot fallback children"
    );
  }

  // Slot deep=true.
  {
    let mut dst = Document::new(QuirksMode::NoQuirks);
    let imported = dst.import_node_from(&src, src_slot, /* deep */ true).unwrap();
    assert_eq!(dst.parent(imported).unwrap(), None);
    assert_subtree_kinds_match(&src, src_slot, &dst, imported);
    let fallback =
      find_in_subtree_by_id(&dst, imported, "fallback").expect("slot fallback not found");
    assert!(
      matches!(dst.node(fallback).kind, NodeKind::Element { .. }),
      "expected element fallback content under slot"
    );
  }
}

#[test]
fn adopt_node_from_mapping_is_complete_and_preserves_node_kinds_for_shadow_dom_subtree() {
  let (mut src, host) = build_shadow_host_source_document();
  let old_parent = src.parent(host).unwrap().expect("host should be connected");

  let old_ids: Vec<NodeId> = src.subtree_preorder(host).collect();
  assert!(
    !old_ids.is_empty(),
    "expected at least the root node in the adopted subtree"
  );

  let src_script = find_in_subtree_by_id(&src, host, "s").expect("source script not found");
  src.set_script_already_started(src_script, true).unwrap();
  let src_script_flags = {
    let node = src.node(src_script);
    (
      node.script_parser_document,
      node.script_force_async,
      node.script_already_started,
    )
  };
  assert!(
    src_script_flags.0,
    "expected parser-inserted script in source document"
  );

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let adopted = dst.adopt_node_from(&mut src, host).unwrap();

  assert_eq!(dst.parent(adopted.new_root).unwrap(), None);
  assert!(
    !src.children(old_parent).unwrap().contains(&host),
    "adopted node should be removed from its old parent's child list"
  );
  assert_eq!(src.parent(host).unwrap(), None, "adopted source root must be detached");

  // Convert mapping list into an indexable lookup for tests.
  let mut old_to_new = std::collections::HashMap::<NodeId, NodeId>::new();
  for (old, new) in &adopted.mapping {
    old_to_new.insert(*old, *new);
  }

  assert_eq!(
    old_to_new.get(&host).copied(),
    Some(adopted.new_root),
    "expected mapping to include adopted root"
  );

  for old_id in &old_ids {
    let new_id = old_to_new
      .get(old_id)
      .copied()
      .unwrap_or_else(|| panic!("missing mapping for old node id {old_id:?}"));
    assert_node_kind_equivalent(&src.node(*old_id).kind, &dst.node(new_id).kind);
  }

  assert_eq!(
    old_to_new.len(),
    old_ids.len(),
    "expected mapping to contain one entry per node in the adopted subtree"
  );

  assert_subtree_kinds_match(&src, host, &dst, adopted.new_root);

  assert!(
    dst
      .subtree_preorder(adopted.new_root)
      .any(|id| matches!(dst.node(id).kind, NodeKind::ShadowRoot { .. })),
    "expected ShadowRoot node in adopted subtree"
  );
  assert!(
    dst
      .subtree_preorder(adopted.new_root)
      .any(|id| matches!(dst.node(id).kind, NodeKind::Slot { .. })),
    "expected Slot node in adopted subtree"
  );

  let adopted_script =
    find_in_subtree_by_id(&dst, adopted.new_root, "s").expect("adopted script not found");
  let adopted_flags = {
    let node = dst.node(adopted_script);
    (
      node.script_parser_document,
      node.script_force_async,
      node.script_already_started,
    )
  };
  assert_eq!(
    adopted_flags, src_script_flags,
    "adopted scripts should preserve internal slot state"
  );
}

#[test]
fn adopt_node_from_document_fragment_root_includes_mapping_for_all_descendants() {
  let mut src = Document::new(QuirksMode::NoQuirks);
  let frag = src.create_document_fragment();
  let div = src.create_element("div", HTML_NAMESPACE);
  src.set_attribute(div, "id", "frag_div").unwrap();
  src.append_child(frag, div).unwrap();
  let text = src.create_text("frag");
  src.append_child(div, text).unwrap();

  let old_ids: Vec<NodeId> = src.subtree_preorder(frag).collect();

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let adopted = dst.adopt_node_from(&mut src, frag).unwrap();
  assert_eq!(dst.parent(adopted.new_root).unwrap(), None);

  let mut old_to_new = std::collections::HashMap::<NodeId, NodeId>::new();
  for (old, new) in &adopted.mapping {
    old_to_new.insert(*old, *new);
  }

  for old_id in &old_ids {
    let new_id = old_to_new
      .get(old_id)
      .copied()
      .unwrap_or_else(|| panic!("missing mapping for old node id {old_id:?}"));
    assert_node_kind_equivalent(&src.node(*old_id).kind, &dst.node(new_id).kind);
  }

  assert_subtree_kinds_match(&src, frag, &dst, adopted.new_root);
}

#[test]
fn adopt_node_from_doctype_detaches_from_source_document() {
  let mut src = Document::new(QuirksMode::NoQuirks);
  let root = src.root();
  let doctype = src.create_doctype("html", "", "");
  src.append_child(root, doctype).unwrap();
  assert_eq!(src.parent(doctype).unwrap(), Some(root));

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let adopted = dst.adopt_node_from(&mut src, doctype).unwrap();
  assert_eq!(dst.parent(adopted.new_root).unwrap(), None);
  assert_eq!(src.parent(doctype).unwrap(), None, "source doctype should be detached");
  assert_node_kind_equivalent(&src.node(doctype).kind, &dst.node(adopted.new_root).kind);
  assert!(
    adopted.mapping.iter().any(|(old, new)| *old == doctype && *new == adopted.new_root),
    "expected mapping to contain adopted doctype"
  );
}

#[test]
fn create_doctype_can_be_inserted_under_document_root() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let doctype = doc.create_doctype("html", "", "");
  assert!(matches!(
    &doc.node(doctype).kind,
    NodeKind::Doctype { name, .. } if name == "html"
  ));

  assert_eq!(doc.append_child(root, doctype).unwrap(), true);

  let html = doc.create_element("html", HTML_NAMESPACE);
  assert_eq!(doc.append_child(root, html).unwrap(), true);

  let mut doc2 = Document::new(QuirksMode::NoQuirks);
  let root2 = doc2.root();
  let html2 = doc2.create_element("html", HTML_NAMESPACE);
  doc2.append_child(root2, html2).unwrap();
  let doctype_after = doc2.create_doctype("html", "", "");
  assert_eq!(
    doc2.append_child(root2, doctype_after),
    Err(DomError::HierarchyRequestError)
  );
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

#[test]
fn import_shadow_root_node_is_not_supported() {
  let html = concat!(
    "<!doctype html>",
    "<div id=host>",
    "<template shadowroot=open><span>shadow</span></template>",
    "<p>light</p>",
    "</div>",
  );
  let src = crate::dom2::parse_html(html).unwrap();
  let shadow_root = find_first_shadow_root(&src);

  let mut dst = Document::new(QuirksMode::NoQuirks);
  assert_eq!(
    dst.import_node_from(&src, shadow_root, /* deep */ false)
      .unwrap_err(),
    DomError::NotSupportedError
  );
  assert_eq!(
    dst.import_node_from(&src, shadow_root, /* deep */ true)
      .unwrap_err(),
    DomError::NotSupportedError
  );
}

#[test]
fn import_shadow_host_element_deep_clones_shadow_root_descendants() {
  let html = concat!(
    "<!doctype html>",
    "<div id=host>",
    "<template shadowroot=open><span id=shadow>shadow</span></template>",
    "<p id=light>light</p>",
    "</div>",
  );
  let src = crate::dom2::parse_html(html).unwrap();
  let host = src.get_element_by_id("host").expect("host element not found");

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let imported = dst.import_node_from(&src, host, /* deep */ true).unwrap();
  assert_eq!(dst.parent(imported).unwrap(), None);
  assert_subtree_kinds_match(&src, host, &dst, imported);

  assert!(
    dst
      .node(imported)
      .children
      .iter()
      .copied()
      .any(|child| matches!(dst.node(child).kind, NodeKind::ShadowRoot { .. })),
    "expected imported host subtree to contain a ShadowRoot child"
  );
}

#[test]
fn adopt_preserves_html_script_internal_state() {
  // Unlike `cloneNode`/`importNode`, `adoptNode` moves the same node to a new document without
  // running per-element cloning steps. In our cross-document approximation, that means copying
  // per-node internal state as-is.
  let html = "<!doctype html><html><head><script id=s></script></head></html>";
  let mut src = crate::dom2::parse_html(html).unwrap();
  let script = find_first_html_script(&src);
  src.set_script_already_started(script, true).unwrap();

  let src_flags = {
    let node = src.node(script);
    (
      node.script_parser_document,
      node.script_force_async,
      node.script_already_started,
    )
  };
  assert!(
    src_flags.0,
    "expected parser-inserted script to have script_parser_document=true"
  );

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let adopted = dst.adopt_node_from(&mut src, script).unwrap();

  assert_eq!(
    src.parent(script).unwrap(),
    None,
    "adoptNode should remove the node from its old parent"
  );

  assert_eq!(dst.parent(adopted.new_root).unwrap(), None);
  assert!(
    adopted
      .mapping
      .iter()
      .any(|(old, new)| *old == script && *new == adopted.new_root),
    "expected mapping to include adopted root"
  );

  let adopted_node = dst.node(adopted.new_root);
  assert_eq!(
    (
      adopted_node.script_parser_document,
      adopted_node.script_force_async,
      adopted_node.script_already_started,
    ),
    src_flags
  );
  assert_eq!(dst.get_attribute(adopted.new_root, "id").unwrap(), Some("s"));
}

#[test]
fn adopt_resets_slot_assignment_state() {
  // Slot assignment is derived from being connected. `adoptNode` removes the node first, so the
  // adopted copy should be detached with `assigned=false`.
  let mut src = Document::new(QuirksMode::NoQuirks);
  let slot = src.create_element("slot", HTML_NAMESPACE);
  src.append_child(src.root(), slot).unwrap();
  match &mut src.node_mut(slot).kind {
    NodeKind::Slot { assigned, .. } => *assigned = true,
    _ => panic!("expected a Slot node"),
  }

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let adopted = dst.adopt_node_from(&mut src, slot).unwrap();

  assert_eq!(dst.parent(adopted.new_root).unwrap(), None);
  match &dst.node(adopted.new_root).kind {
    NodeKind::Slot { assigned, .. } => assert!(!assigned),
    _ => panic!("expected a Slot node"),
  }
}

#[test]
fn adopt_shadow_root_throws_hierarchy_request_error() {
  let html = concat!(
    "<!doctype html>",
    "<div id=host>",
    "<template shadowroot=open><span>shadow</span></template>",
    "<p>light</p>",
    "</div>",
  );
  let mut src = crate::dom2::parse_html(html).unwrap();
  let shadow_root = find_first_shadow_root(&src);

  let mut dst = Document::new(QuirksMode::NoQuirks);
  assert_eq!(
    dst.adopt_node_from(&mut src, shadow_root).unwrap_err(),
    DomError::HierarchyRequestError
  );
}
