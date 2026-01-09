#![cfg(test)]

use super::{Document, DomError, NodeId, NodeKind};
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
fn create_element_marks_template_inert_and_slot_kind() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let template = doc.create_element("template", "");
  assert!(doc.node(template).inert_subtree);

  let template_upper = doc.create_element("TEMPLATE", "");
  assert!(doc.node(template_upper).inert_subtree);

  let div = doc.create_element("div", "");
  assert!(!doc.node(div).inert_subtree);

  let slot = doc.create_element("slot", "");
  assert!(matches!(doc.node(slot).kind, NodeKind::Slot { .. }));
}

#[test]
fn create_comment_is_a_leaf_node_and_can_be_inserted() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let comment = doc.create_comment("hello");
  assert!(doc.node(comment).children.is_empty());
  assert!(matches!(&doc.node(comment).kind, NodeKind::Comment { .. }));
  if let NodeKind::Comment { content } = &doc.node(comment).kind {
    assert_eq!(content, "hello");
  }

  doc.append_child(parent, comment).unwrap();
  assert_eq!(doc.parent(comment).unwrap(), Some(parent));

  let child = doc.create_text("x");
  assert_eq!(
    doc.append_child(comment, child),
    Err(DomError::HierarchyRequestError)
  );
}

#[test]
fn create_document_fragment_is_parentless() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let frag = doc.create_document_fragment();
  assert_eq!(doc.parent(frag).unwrap(), None);
  assert_eq!(doc.children(frag).unwrap(), &[]);
}

#[test]
fn inserting_empty_document_fragment_is_a_noop() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let frag = doc.create_document_fragment();
  assert_eq!(doc.append_child(root, frag).unwrap(), false);
  assert_eq!(doc.append_child(parent, frag).unwrap(), false);

  assert_eq!(doc.children(root).unwrap(), &[parent]);
  assert_eq!(doc.children(parent).unwrap(), &[]);
  assert_eq!(doc.parent(frag).unwrap(), None);
  assert_eq!(doc.children(frag).unwrap(), &[]);
  assert_parent_child_invariants(&doc);
}

#[test]
fn append_child_sets_parent_and_updates_children() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  let text = doc.create_text("hi");

  assert_eq!(doc.append_child(root, parent).unwrap(), true);
  assert_eq!(doc.append_child(parent, text).unwrap(), true);

  assert_eq!(doc.parent(parent).unwrap(), Some(root));
  assert_eq!(doc.parent(text).unwrap(), Some(parent));

  assert_eq!(doc.children(root).unwrap(), &[parent]);
  assert_eq!(doc.children(parent).unwrap(), &[text]);

  assert_eq!(doc.index_of_child(root, parent).unwrap(), Some(0));
  assert_eq!(doc.index_of_child(parent, text).unwrap(), Some(0));

  assert_parent_child_invariants(&doc);
}

#[test]
fn insert_before_reference_not_child_returns_not_found() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  let child = doc.create_element("span", "");
  let ref_not_child = doc.create_element("b", "");
  doc.append_child(root, parent).unwrap();

  // Insert with a reference that is not under `parent`.
  assert_eq!(
    doc.insert_before(parent, child, Some(ref_not_child)),
    Err(DomError::NotFoundError)
  );

  // Ensure no partial mutation occurred.
  assert_eq!(doc.parent(child).unwrap(), None);
  assert_eq!(doc.children(parent).unwrap(), &[]);
  assert_parent_child_invariants(&doc);
}

#[test]
fn remove_child_with_wrong_parent_returns_not_found() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let container = doc.create_element("div", "");
  let p1 = doc.create_element("div", "");
  let p2 = doc.create_element("div", "");
  let child = doc.create_element("span", "");

  doc.append_child(root, container).unwrap();
  doc.append_child(container, p1).unwrap();
  doc.append_child(container, p2).unwrap();
  doc.append_child(p1, child).unwrap();

  assert_eq!(doc.remove_child(p2, child), Err(DomError::NotFoundError));
  assert_eq!(doc.parent(child).unwrap(), Some(p1));
  assert_eq!(doc.children(p1).unwrap(), &[child]);

  assert_eq!(doc.remove_child(p1, child).unwrap(), true);
  assert_eq!(doc.parent(child).unwrap(), None);
  assert_eq!(doc.children(p1).unwrap(), &[]);

  // Removing again is still NotFound.
  assert_eq!(doc.remove_child(p1, child), Err(DomError::NotFoundError));

  assert_parent_child_invariants(&doc);
}

#[test]
fn move_within_same_parent_returns_true_only_if_position_changes() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  let a = doc.create_element("a", "");
  let b = doc.create_element("b", "");
  let c = doc.create_element("c", "");

  doc.append_child(root, parent).unwrap();
  doc.append_child(parent, a).unwrap();
  doc.append_child(parent, b).unwrap();
  doc.append_child(parent, c).unwrap();

  // b is already before c.
  assert_eq!(doc.insert_before(parent, b, Some(c)).unwrap(), false);
  // c is already last.
  assert_eq!(doc.append_child(parent, c).unwrap(), false);

  // Move a to the end.
  assert_eq!(doc.append_child(parent, a).unwrap(), true);
  assert_eq!(doc.children(parent).unwrap(), &[b, c, a]);

  // Move a to the front.
  assert_eq!(doc.insert_before(parent, a, Some(b)).unwrap(), true);
  assert_eq!(doc.children(parent).unwrap(), &[a, b, c]);

  assert_parent_child_invariants(&doc);
}

#[test]
fn moving_across_parents_detaches_from_old_parent() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let container = doc.create_element("div", "");
  let p1 = doc.create_element("div", "");
  let p2 = doc.create_element("div", "");
  let child = doc.create_text("x");

  doc.append_child(root, container).unwrap();
  doc.append_child(container, p1).unwrap();
  doc.append_child(container, p2).unwrap();
  doc.append_child(p1, child).unwrap();

  assert_eq!(doc.append_child(p2, child).unwrap(), true);
  assert_eq!(doc.parent(child).unwrap(), Some(p2));
  assert_eq!(doc.children(p1).unwrap(), &[]);
  assert_eq!(doc.children(p2).unwrap(), &[child]);

  assert_parent_child_invariants(&doc);
}

#[test]
fn cycle_prevention_disallows_inserting_ancestor_into_descendant() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let a = doc.create_element("a", "");
  let b = doc.create_element("b", "");
  let c = doc.create_element("c", "");

  doc.append_child(root, a).unwrap();
  doc.append_child(a, b).unwrap();
  doc.append_child(b, c).unwrap();

  // Attempt to insert `a` under `c`, which would create a cycle.
  assert_eq!(
    doc.append_child(c, a),
    Err(DomError::HierarchyRequestError)
  );
  assert_eq!(doc.parent(a).unwrap(), Some(root));
  assert_eq!(doc.parent(b).unwrap(), Some(a));
  assert_eq!(doc.parent(c).unwrap(), Some(b));

  assert_parent_child_invariants(&doc);
}

#[test]
fn invalid_nodeid_returns_deterministic_errors_instead_of_panicking() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let child = doc.create_element("div", "");

  let bogus_parent = NodeId(999_999);
  let bogus_child = NodeId(888_888);

  assert_eq!(
    doc.append_child(bogus_parent, child),
    Err(DomError::NotFoundError)
  );
  assert_eq!(
    doc.append_child(root, bogus_child),
    Err(DomError::NotFoundError)
  );

  // Invalid references should also be reported as NotFoundError.
  doc.append_child(root, child).unwrap();
  assert_eq!(
    doc.insert_before(root, child, Some(NodeId(777_777))),
    Err(DomError::NotFoundError)
  );
}

#[test]
fn leaf_nodes_reject_children() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let comment = doc.push_node(
    NodeKind::Comment {
      content: "hi".to_string(),
    },
    Some(root),
    /* inert_subtree */ false,
  );
  let pi = doc.push_node(
    NodeKind::ProcessingInstruction {
      target: "xml".to_string(),
      data: "version=\"1.0\"".to_string(),
    },
    Some(root),
    /* inert_subtree */ false,
  );
  let doctype = doc.push_node(
    NodeKind::Doctype {
      name: "html".to_string(),
      public_id: String::new(),
      system_id: String::new(),
    },
    Some(root),
    /* inert_subtree */ false,
  );

  for leaf in [comment, pi, doctype] {
    let child = doc.create_element("div", "");
    assert_eq!(
      doc.append_child(leaf, child),
      Err(DomError::HierarchyRequestError)
    );
    assert_eq!(doc.parent(child).unwrap(), None);
  }

  assert_parent_child_invariants(&doc);
}

#[test]
fn doctype_nodes_can_only_be_inserted_under_document() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let doctype = doc.push_node(
    NodeKind::Doctype {
      name: "html".to_string(),
      public_id: String::new(),
      system_id: String::new(),
    },
    None,
    /* inert_subtree */ false,
  );

  assert_eq!(
    doc.append_child(parent, doctype),
    Err(DomError::HierarchyRequestError)
  );
  assert_eq!(doc.parent(doctype).unwrap(), None);
  assert_parent_child_invariants(&doc);
}

#[test]
fn document_rejects_text_node_children() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let text = doc.create_text("hi");
  assert_eq!(
    doc.append_child(root, text),
    Err(DomError::HierarchyRequestError)
  );
  assert_eq!(doc.parent(text).unwrap(), None);
  assert_parent_child_invariants(&doc);
}

#[test]
fn document_rejects_multiple_element_children() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let a = doc.create_element("html", "");
  let b = doc.create_element("div", "");
  doc.append_child(root, a).unwrap();
  assert_eq!(
    doc.append_child(root, b),
    Err(DomError::HierarchyRequestError)
  );
  assert_eq!(doc.parent(b).unwrap(), None);
  assert_parent_child_invariants(&doc);
}

#[test]
fn document_rejects_multiple_doctypes() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let a = doc.push_node(
    NodeKind::Doctype {
      name: "html".to_string(),
      public_id: String::new(),
      system_id: String::new(),
    },
    None,
    /* inert_subtree */ false,
  );
  let b = doc.push_node(
    NodeKind::Doctype {
      name: "html".to_string(),
      public_id: String::new(),
      system_id: String::new(),
    },
    None,
    /* inert_subtree */ false,
  );

  doc.append_child(root, a).unwrap();
  assert_eq!(
    doc.append_child(root, b),
    Err(DomError::HierarchyRequestError)
  );
  assert_eq!(doc.parent(b).unwrap(), None);
  assert_parent_child_invariants(&doc);
}

#[test]
fn document_rejects_element_insertion_before_doctype() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let doctype = doc.push_node(
    NodeKind::Doctype {
      name: "html".to_string(),
      public_id: String::new(),
      system_id: String::new(),
    },
    None,
    /* inert_subtree */ false,
  );
  doc.append_child(root, doctype).unwrap();

  let element = doc.create_element("html", "");
  assert_eq!(
    doc.insert_before(root, element, Some(doctype)),
    Err(DomError::HierarchyRequestError)
  );
  assert_eq!(doc.parent(element).unwrap(), None);
  assert_parent_child_invariants(&doc);
}

#[test]
fn doctype_can_be_inserted_before_the_first_element_child() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let element = doc.create_element("html", "");
  doc.append_child(root, element).unwrap();

  let doctype = doc.push_node(
    NodeKind::Doctype {
      name: "html".to_string(),
      public_id: String::new(),
      system_id: String::new(),
    },
    None,
    /* inert_subtree */ false,
  );

  doc.insert_before(root, doctype, Some(element)).unwrap();
  assert_eq!(doc.children(root).unwrap(), &[doctype, element]);
  assert_parent_child_invariants(&doc);
}

#[test]
fn document_rejects_doctype_insertion_after_an_element() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let element = doc.create_element("html", "");
  doc.append_child(root, element).unwrap();

  let doctype = doc.push_node(
    NodeKind::Doctype {
      name: "html".to_string(),
      public_id: String::new(),
      system_id: String::new(),
    },
    None,
    /* inert_subtree */ false,
  );
  assert_eq!(
    doc.append_child(root, doctype),
    Err(DomError::HierarchyRequestError)
  );
  assert_eq!(doc.parent(doctype).unwrap(), None);
  assert_parent_child_invariants(&doc);
}

#[test]
fn document_rejects_doctype_insertion_between_element_and_comment() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let element = doc.create_element("html", "");
  doc.append_child(root, element).unwrap();

  let comment = doc.push_node(
    NodeKind::Comment {
      content: "hi".to_string(),
    },
    Some(root),
    /* inert_subtree */ false,
  );

  let doctype = doc.push_node(
    NodeKind::Doctype {
      name: "html".to_string(),
      public_id: String::new(),
      system_id: String::new(),
    },
    None,
    /* inert_subtree */ false,
  );
  assert_eq!(
    doc.insert_before(root, doctype, Some(comment)),
    Err(DomError::HierarchyRequestError)
  );
  assert_eq!(doc.parent(doctype).unwrap(), None);
  assert_parent_child_invariants(&doc);
}

#[test]
fn replace_child_enforces_document_element_and_doctype_constraints() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let doctype = doc.push_node(
    NodeKind::Doctype {
      name: "html".to_string(),
      public_id: String::new(),
      system_id: String::new(),
    },
    None,
    /* inert_subtree */ false,
  );
  doc.append_child(root, doctype).unwrap();

  let element = doc.create_element("html", "");
  doc.append_child(root, element).unwrap();

  let replacement_doctype = doc.push_node(
    NodeKind::Doctype {
      name: "html".to_string(),
      public_id: String::new(),
      system_id: String::new(),
    },
    None,
    /* inert_subtree */ false,
  );
  doc
    .replace_child(root, replacement_doctype, doctype)
    .unwrap();
  assert_eq!(doc.children(root).unwrap(), &[replacement_doctype, element]);
  assert_eq!(doc.parent(doctype).unwrap(), None);

  let replacement_element = doc.create_element("svg", "");
  doc
    .replace_child(root, replacement_element, element)
    .unwrap();
  assert_eq!(
    doc.children(root).unwrap(),
    &[replacement_doctype, replacement_element]
  );
  assert_eq!(doc.parent(element).unwrap(), None);
  assert_parent_child_invariants(&doc);
}

#[test]
fn replace_child_rejects_replacing_doctype_with_element_when_element_exists() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let doctype = doc.push_node(
    NodeKind::Doctype {
      name: "html".to_string(),
      public_id: String::new(),
      system_id: String::new(),
    },
    None,
    /* inert_subtree */ false,
  );
  doc.append_child(root, doctype).unwrap();

  let element = doc.create_element("html", "");
  doc.append_child(root, element).unwrap();

  let replacement = doc.create_element("svg", "");
  assert_eq!(
    doc.replace_child(root, replacement, doctype),
    Err(DomError::HierarchyRequestError)
  );
  assert_eq!(doc.parent(replacement).unwrap(), None);
  assert_parent_child_invariants(&doc);
}

#[test]
fn replace_child_rejects_replacing_element_with_doctype_when_doctype_exists() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let doctype = doc.push_node(
    NodeKind::Doctype {
      name: "html".to_string(),
      public_id: String::new(),
      system_id: String::new(),
    },
    None,
    /* inert_subtree */ false,
  );
  doc.append_child(root, doctype).unwrap();

  let element = doc.create_element("html", "");
  doc.append_child(root, element).unwrap();

  let replacement = doc.push_node(
    NodeKind::Doctype {
      name: "html".to_string(),
      public_id: String::new(),
      system_id: String::new(),
    },
    None,
    /* inert_subtree */ false,
  );
  assert_eq!(
    doc.replace_child(root, replacement, element),
    Err(DomError::HierarchyRequestError)
  );
  assert_eq!(doc.parent(replacement).unwrap(), None);
  assert_parent_child_invariants(&doc);
}

#[test]
fn deep_tree_mutations_are_iterative() {
  const DEPTH: usize = 50_000;

  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let top = doc.create_element("div", "");
  doc.append_child(root, top).unwrap();

  let mut current = top;
  for _ in 0..DEPTH {
    let next = doc.create_element("div", "");
    doc.append_child(current, next).unwrap();
    current = next;
  }

  // Attempt to insert the ancestor `top` under the deepest node, which should be rejected via an
  // iterative ancestor walk (no recursion).
  assert_eq!(
    doc.append_child(current, top),
    Err(DomError::HierarchyRequestError)
  );
}
