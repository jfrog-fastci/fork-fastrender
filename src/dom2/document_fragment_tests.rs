use super::{Document, DomError, NodeId};
use selectors::context::QuirksMode;

fn assert_parent_child_invariants(doc: &Document) {
  for (idx, node) in doc.nodes().iter().enumerate() {
    let id = NodeId(idx);
    if id == doc.root() {
      assert!(node.parent.is_none(), "root node must have no parent");
    }

    let mut seen = std::collections::HashSet::new();
    for &child in &node.children {
      assert!(
        seen.insert(child),
        "duplicate child {child:?} under parent {id:?}"
      );
      let child_node = doc.node(child);
      assert_eq!(
        child_node.parent,
        Some(id),
        "child must point back to parent"
      );
    }

    if let Some(parent) = node.parent {
      let parent_node = doc.node(parent);
      assert!(
        parent_node.children.contains(&id),
        "parent pointers must be the inverse of children vectors"
      );
    }
  }
}

#[test]
fn append_child_inserts_fragment_children_and_empties_fragment() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let frag = doc.create_document_fragment();
  let a = doc.create_element("a", "");
  let b = doc.create_element("b", "");
  doc.append_child(frag, a).unwrap();
  doc.append_child(frag, b).unwrap();

  assert_eq!(doc.append_child(parent, frag).unwrap(), true);
  assert_eq!(doc.children(parent).unwrap(), &[a, b]);
  assert_eq!(doc.children(frag).unwrap(), &[]);
  assert_eq!(doc.parent(frag).unwrap(), None);
  assert_eq!(doc.parent(a).unwrap(), Some(parent));
  assert_eq!(doc.parent(b).unwrap(), Some(parent));

  assert_parent_child_invariants(&doc);
}

#[test]
fn insert_before_inserts_fragment_children_before_reference() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let a = doc.create_element("a", "");
  let reference = doc.create_element("ref", "");
  let b = doc.create_element("b", "");
  doc.append_child(parent, a).unwrap();
  doc.append_child(parent, reference).unwrap();
  doc.append_child(parent, b).unwrap();

  let frag = doc.create_document_fragment();
  let x = doc.create_element("x", "");
  let y = doc.create_element("y", "");
  doc.append_child(frag, x).unwrap();
  doc.append_child(frag, y).unwrap();

  assert_eq!(
    doc.insert_before(parent, frag, Some(reference)).unwrap(),
    true
  );
  assert_eq!(doc.children(parent).unwrap(), &[a, x, y, reference, b]);
  assert_eq!(doc.children(frag).unwrap(), &[]);
  assert_eq!(doc.parent(frag).unwrap(), None);

  assert_parent_child_invariants(&doc);
}

#[test]
fn replace_child_with_fragment() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let old = doc.create_element("old", "");
  doc.append_child(parent, old).unwrap();

  let frag = doc.create_document_fragment();
  let x = doc.create_element("x", "");
  let y = doc.create_element("y", "");
  doc.append_child(frag, x).unwrap();
  doc.append_child(frag, y).unwrap();

  assert_eq!(doc.replace_child(parent, frag, old).unwrap(), true);
  assert_eq!(doc.children(parent).unwrap(), &[x, y]);
  assert_eq!(doc.parent(old).unwrap(), None);
  assert_eq!(doc.children(frag).unwrap(), &[]);
  assert_eq!(doc.parent(frag).unwrap(), None);
  assert_eq!(doc.parent(x).unwrap(), Some(parent));
  assert_eq!(doc.parent(y).unwrap(), Some(parent));

  assert_parent_child_invariants(&doc);
}

#[test]
fn atomicity_on_error() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let existing = doc.create_element("existing", "");
  doc.append_child(root, existing).unwrap();

  let frag = doc.create_document_fragment();
  let valid = doc.create_element("div", "");
  let invalid = doc.create_element("slot", "");
  doc.append_child(frag, valid).unwrap();
  doc.append_child(frag, invalid).unwrap();

  let root_children_before = doc.children(root).unwrap().to_vec();
  let frag_children_before = doc.children(frag).unwrap().to_vec();
  let valid_parent_before = doc.parent(valid).unwrap();
  let invalid_parent_before = doc.parent(invalid).unwrap();

  assert_eq!(
    doc.append_child(root, frag),
    Err(DomError::HierarchyRequestError)
  );

  assert_eq!(doc.children(root).unwrap(), root_children_before.as_slice());
  assert_eq!(doc.children(frag).unwrap(), frag_children_before.as_slice());
  assert_eq!(doc.parent(valid).unwrap(), valid_parent_before);
  assert_eq!(doc.parent(invalid).unwrap(), invalid_parent_before);

  assert_parent_child_invariants(&doc);
}

