use crate::dom2::{parse_xml, NodeKind};
use crate::dom::{DomNodeType};
use selectors::context::QuirksMode;

#[test]
fn parse_xml_returns_xml_document_with_scripting_disabled() {
  let doc = parse_xml(r#"<root/>"#).unwrap();
  assert!(!doc.is_html_document());

  let snapshot = doc.to_renderer_dom();
  match &snapshot.node_type {
    DomNodeType::Document {
      quirks_mode,
      scripting_enabled,
    } => {
      assert_eq!(*quirks_mode, QuirksMode::NoQuirks);
      assert!(!*scripting_enabled);
    }
    other => panic!("expected document root, got {other:?}"),
  }
}

fn find_first_doctype(doc: &crate::dom2::Document) -> Option<crate::dom2::NodeId> {
  let root = doc.root();
  doc.node(root).children.iter().copied().find(|&child| {
    doc
      .nodes()
      .get(child.index())
      .is_some_and(|n| n.parent == Some(root) && matches!(n.kind, NodeKind::Doctype { .. }))
  })
}

#[test]
fn xml_doctype_system_preserved() {
  let doc = parse_xml(r#"<!DOCTYPE root SYSTEM "x"><root/>"#).unwrap();
  let root = doc.document_element().expect("expected documentElement");
  match &doc.node(root).kind {
    NodeKind::Element { tag_name, .. } => assert_eq!(tag_name, "root"),
    other => panic!("expected element, got {other:?}"),
  }

  let doctype = find_first_doctype(&doc).expect("expected doctype node");
  match &doc.node(doctype).kind {
    NodeKind::Doctype {
      name,
      public_id,
      system_id,
    } => {
      assert_eq!(name, "root");
      assert_eq!(public_id, "");
      assert_eq!(system_id, "x");
    }
    other => panic!("expected doctype, got {other:?}"),
  }

  let children: Vec<_> = doc.node(doc.root()).children.clone();
  let doctype_pos = children
    .iter()
    .position(|&id| id == doctype)
    .expect("doctype pos");
  let root_pos = children.iter().position(|&id| id == root).expect("root pos");
  assert!(
    doctype_pos < root_pos,
    "expected doctype before documentElement (doctype_pos={doctype_pos}, root_pos={root_pos})"
  );
}

#[test]
fn xml_doctype_public_preserved() {
  let doc = parse_xml(r#"<!DOCTYPE root PUBLIC "pub" "sys"><root/>"#).unwrap();
  let root = doc.document_element().expect("expected documentElement");
  match &doc.node(root).kind {
    NodeKind::Element { tag_name, .. } => assert_eq!(tag_name, "root"),
    other => panic!("expected element, got {other:?}"),
  }

  let doctype = find_first_doctype(&doc).expect("expected doctype node");
  match &doc.node(doctype).kind {
    NodeKind::Doctype {
      name,
      public_id,
      system_id,
    } => {
      assert_eq!(name, "root");
      assert_eq!(public_id, "pub");
      assert_eq!(system_id, "sys");
    }
    other => panic!("expected doctype, got {other:?}"),
  }

  let children: Vec<_> = doc.node(doc.root()).children.clone();
  let doctype_pos = children
    .iter()
    .position(|&id| id == doctype)
    .expect("doctype pos");
  let root_pos = children.iter().position(|&id| id == root).expect("root pos");
  assert!(
    doctype_pos < root_pos,
    "expected doctype before documentElement (doctype_pos={doctype_pos}, root_pos={root_pos})"
  );
}
