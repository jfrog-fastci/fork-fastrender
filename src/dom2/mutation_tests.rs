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

  let svg_template = doc.create_element("template", crate::dom::SVG_NAMESPACE);
  assert!(
    !doc.node(svg_template).inert_subtree,
    "non-HTML <template> elements must not mark inert_subtree"
  );

  let div = doc.create_element("div", "");
  assert!(!doc.node(div).inert_subtree);

  let slot = doc.create_element("slot", "");
  assert!(matches!(doc.node(slot).kind, NodeKind::Slot { .. }));
}

#[test]
fn create_element_sets_script_force_async_for_html_scripts() {
  let mut doc = Document::new(QuirksMode::NoQuirks);

  let script = doc.create_element("script", "");
  assert!(
    doc.node(script).script_force_async,
    "createElement('script') should default HTMLScriptElement.async to true via force_async"
  );

  let svg_script = doc.create_element("script", crate::dom::SVG_NAMESPACE);
  assert!(
    !doc.node(svg_script).script_force_async,
    "non-HTML <script> elements must not set force_async"
  );
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
fn inserting_document_fragment_moves_children_and_empties_fragment() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let frag = doc.create_document_fragment();
  let a = doc.create_element("a", "");
  let b = doc.create_text("b");
  doc.append_child(frag, a).unwrap();
  doc.append_child(frag, b).unwrap();

  assert_eq!(doc.append_child(parent, frag).unwrap(), true);

  assert_eq!(doc.parent(frag).unwrap(), None);
  assert_eq!(doc.children(frag).unwrap(), &[]);
  assert_eq!(doc.children(parent).unwrap(), &[a, b]);
  assert_eq!(doc.parent(a).unwrap(), Some(parent));
  assert_eq!(doc.parent(b).unwrap(), Some(parent));
  assert_parent_child_invariants(&doc);
}

#[test]
fn inserting_document_fragment_into_document_is_atomic() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let frag = doc.create_document_fragment();
  let a = doc.create_element("a", "");
  let b = doc.create_element("b", "");
  doc.append_child(frag, a).unwrap();
  doc.append_child(frag, b).unwrap();

  let frag_children = doc.children(frag).unwrap().to_vec();
  assert_eq!(
    doc.append_child(root, frag),
    Err(DomError::HierarchyRequestError)
  );

  assert_eq!(doc.children(root).unwrap(), &[]);
  assert_eq!(doc.children(frag).unwrap(), frag_children.as_slice());
  assert_eq!(doc.parent(a).unwrap(), Some(frag));
  assert_eq!(doc.parent(b).unwrap(), Some(frag));
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
fn mutation_log_append_child_records_inserted_node_id() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let child = doc.create_element("div", "");
  assert!(doc.append_child(root, child).unwrap());

  let mutations = doc.take_mutations();
  assert!(
    mutations.nodes_inserted.contains(&child),
    "append_child() should record inserted node ids"
  );
  assert!(
    mutations.nodes_removed.is_empty(),
    "append_child() should not record removals"
  );
}

#[test]
fn mutation_log_insert_before_records_inserted_node_id() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  let reference = doc.create_element("span", "");
  assert!(doc.append_child(root, parent).unwrap());
  assert!(doc.append_child(parent, reference).unwrap());

  // Clear the insertion mutations so we only observe the insert_before() effects.
  let _ = doc.take_mutations();

  let inserted = doc.create_element("p", "");
  assert!(doc.insert_before(parent, inserted, Some(reference)).unwrap());

  let mutations = doc.take_mutations();
  assert!(
    mutations.nodes_inserted.contains(&inserted),
    "insert_before() should record inserted node ids"
  );
  assert!(
    mutations.nodes_removed.is_empty(),
    "insert_before() should not record removals"
  );
}

#[test]
fn mutation_log_remove_child_records_removed_node_id() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  let child = doc.create_element("span", "");
  assert!(doc.append_child(root, parent).unwrap());
  assert!(doc.append_child(parent, child).unwrap());

  // Clear the insertion mutations so we only observe remove_child() effects.
  let _ = doc.take_mutations();

  assert!(doc.remove_child(parent, child).unwrap());
  let mutations = doc.take_mutations();
  assert!(
    mutations.nodes_removed.contains(&child),
    "remove_child() should record removed node ids"
  );
  assert!(
    mutations.nodes_inserted.is_empty(),
    "remove_child() should not record insertions"
  );
}

#[test]
fn mutation_log_does_not_record_noop_inserts() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let parent = doc.create_element("div", "");
  let child = doc.create_element("span", "");
  assert!(doc.append_child(root, parent).unwrap());
  assert!(doc.append_child(parent, child).unwrap());

  // Clear the insertion mutations so the subsequent no-op doesn't get masked.
  let _ = doc.take_mutations();

  // Re-appending the last child is a no-op.
  assert!(!doc.append_child(parent, child).unwrap());
  let mutations = doc.take_mutations();
  assert!(mutations.is_empty(), "no-op appendChild must not record mutations");
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
  assert_eq!(doc.append_child(c, a), Err(DomError::HierarchyRequestError));
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

#[test]
fn clone_node_clones_document_nodes() {
  let mut doc = Document::new(QuirksMode::LimitedQuirks);
  let root = doc.root();

  // Build a document that exercises document-specific child ordering:
  // <!doctype html><html id=orig><body><div id=src></div></body></html>
  let doctype = doc.push_node(
    NodeKind::Doctype {
      name: "html".to_string(),
      public_id: "pub".to_string(),
      system_id: "sys".to_string(),
    },
    None,
    /* inert_subtree */ false,
  );
  doc.append_child(root, doctype).unwrap();

  let html = doc.create_element("html", "");
  doc.set_attribute(html, "id", "orig").unwrap();
  doc.append_child(root, html).unwrap();

  let body = doc.create_element("body", "");
  doc.append_child(html, body).unwrap();
  let div = doc.create_element("div", "");
  doc.set_attribute(div, "id", "src").unwrap();
  doc.append_child(body, div).unwrap();

  // Shallow document clone has no children but preserves document kind data.
  let shallow = doc.clone_node(root, false).unwrap();
  assert_eq!(doc.parent(shallow).unwrap(), None);
  assert_eq!(doc.children(shallow).unwrap(), &[]);
  match &doc.node(shallow).kind {
    NodeKind::Document { quirks_mode } => assert_eq!(*quirks_mode, QuirksMode::LimitedQuirks),
    other => panic!("expected Document clone, got {other:?}"),
  }

  // Deep clone preserves child ordering and produces independent nodes.
  let deep = doc.clone_node(root, true).unwrap();
  assert_eq!(doc.parent(deep).unwrap(), None);
  let deep_children = doc.children(deep).unwrap();
  assert_eq!(deep_children.len(), 2);

  let cloned_doctype = deep_children[0];
  let cloned_html = deep_children[1];
  assert_ne!(cloned_doctype, doctype);
  assert_ne!(cloned_html, html);
  match &doc.node(cloned_doctype).kind {
    NodeKind::Doctype {
      name,
      public_id,
      system_id,
    } => {
      assert_eq!(name, "html");
      assert_eq!(public_id, "pub");
      assert_eq!(system_id, "sys");
    }
    other => panic!("expected Doctype clone, got {other:?}"),
  }
  assert_eq!(doc.get_attribute(cloned_html, "id").unwrap(), Some("orig"));

  // Mutating the clone must not affect the original.
  doc.set_attribute(cloned_html, "id", "cloned").unwrap();
  assert_eq!(doc.get_attribute(html, "id").unwrap(), Some("orig"));
}

#[test]
fn clone_node_shallow_copies_element_kind_and_attributes_but_not_children() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let el = doc.create_element("div", "");
  doc.set_attribute(el, "data-x", "1").unwrap();
  doc.append_child(root, el).unwrap();
  let child = doc.create_text("hi");
  doc.append_child(el, child).unwrap();

  let cloned = doc.clone_node(el, false).unwrap();
  assert_eq!(doc.parent(cloned).unwrap(), None);
  assert_eq!(doc.children(cloned).unwrap(), &[]);

  match &doc.node(cloned).kind {
    NodeKind::Element {
      tag_name,
      namespace,
      prefix: _,
      attributes,
    } => {
      assert_eq!(tag_name, "div");
      assert_eq!(namespace, "");
      assert!(attributes.iter().any(|(k, v)| k == "data-x" && v == "1"));
    }
    other => panic!("expected cloned element kind, got {other:?}"),
  }
}

#[test]
fn clone_node_deep_clones_iteratively() {
  // Large enough to overflow typical call stacks if implemented recursively.
  const DEPTH: usize = 20_000;

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

  let cloned = doc.clone_node(top, true).unwrap();
  assert_eq!(doc.parent(cloned).unwrap(), None);

  let mut current = cloned;
  for _ in 0..DEPTH {
    let children = doc.children(current).unwrap();
    assert_eq!(children.len(), 1);
    current = children[0];
  }
  assert_eq!(doc.children(current).unwrap().len(), 0);
}

#[test]
fn create_document_type_creates_detached_doctype_node() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let doctype = doc.create_document_type("html", "pub", "sys");
  assert_eq!(doc.parent(doctype).unwrap(), None);
  assert_eq!(doc.children(doctype).unwrap(), &[]);
  match &doc.node(doctype).kind {
    NodeKind::Doctype {
      name,
      public_id,
      system_id,
    } => {
      assert_eq!(name, "html");
      assert_eq!(public_id, "pub");
      assert_eq!(system_id, "sys");
    }
    other => panic!("expected Doctype node, got {other:?}"),
  }
}

#[test]
fn import_node_from_document_clones_subtree_and_returns_mapping() {
  let mut src = Document::new(QuirksMode::NoQuirks);
  let src_root = src.root();

  let template = src.create_element("template", "");
  src.set_attribute(template, "id", "t").unwrap();
  src.append_child(src_root, template).unwrap();

  let span = src.create_element("span", "");
  src.set_attribute(span, "class", "b").unwrap();
  src.append_child(template, span).unwrap();
  let text = src.create_text("Hello");
  src.append_child(span, text).unwrap();

  let script = src.create_element("script", "");
  src.set_bool_attribute(script, "async", true).unwrap();
  src.set_script_already_started(script, true).unwrap();
  src.append_child(template, script).unwrap();

  // Shallow import should only clone the root.
  let mut dst_shallow = Document::new(QuirksMode::NoQuirks);
  let (dst_template_shallow, shallow_mapping) = dst_shallow
    .import_node_from_document(&src, template, false)
    .unwrap();
  assert_eq!(shallow_mapping, vec![(template, dst_template_shallow)]);
  assert_eq!(dst_shallow.parent(dst_template_shallow).unwrap(), None);
  assert_eq!(dst_shallow.children(dst_template_shallow).unwrap(), &[]);
  assert_eq!(dst_shallow.children(dst_shallow.root()).unwrap(), &[]);

  // Deep import should clone the entire subtree (iteratively).
  let mut dst = Document::new(QuirksMode::NoQuirks);
  let (dst_template, mapping) = dst.import_node_from_document(&src, template, true).unwrap();
  assert_eq!(dst.parent(dst_template).unwrap(), None);
  assert_eq!(
    dst.children(dst.root()).unwrap(),
    &[],
    "imported nodes must be detached until explicitly inserted"
  );

  assert_eq!(mapping.len(), 4);
  assert_eq!(mapping[0].0, template);
  assert_eq!(mapping[1].0, span);
  assert_eq!(mapping[2].0, text);
  assert_eq!(mapping[3].0, script);

  let dst_span = mapping[1].1;
  let dst_text = mapping[2].1;
  let dst_script = mapping[3].1;

  assert_eq!(dst.get_attribute(dst_template, "id").unwrap(), Some("t"));
  assert!(
    dst.node(dst_template).inert_subtree,
    "template elements should preserve inert_subtree when imported"
  );
  assert_eq!(dst.children(dst_template).unwrap(), &[dst_span, dst_script]);
  assert_eq!(dst.get_attribute(dst_span, "class").unwrap(), Some("b"));
  assert_eq!(dst.children(dst_span).unwrap(), &[dst_text]);
  assert_eq!(dst.text_data(dst_text).unwrap(), "Hello");

  assert!(
    dst.node(dst_script).script_already_started,
    "import should clone script 'already started' internal slot"
  );
  assert!(
    !dst.node(dst_script).script_force_async,
    "import should clone script force_async flag using clone_node semantics"
  );
}

#[test]
fn import_node_from_document_handles_deep_trees_iteratively() {
  // Large enough to overflow typical call stacks if implemented recursively.
  const DEPTH: usize = 10_000;

  let mut src = Document::new(QuirksMode::NoQuirks);
  let top = src.create_element("div", "");
  src.append_child(src.root(), top).unwrap();

  let mut current = top;
  for _ in 0..DEPTH {
    let next = src.create_element("div", "");
    src.append_child(current, next).unwrap();
    current = next;
  }

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let (cloned_top, mapping) = dst.import_node_from_document(&src, top, true).unwrap();
  assert_eq!(dst.parent(cloned_top).unwrap(), None);
  assert_eq!(mapping.len(), DEPTH + 1);

  let mut current = cloned_top;
  for _ in 0..DEPTH {
    let children = dst.children(current).unwrap();
    assert_eq!(children.len(), 1);
    current = children[0];
  }
  assert_eq!(dst.children(current).unwrap().len(), 0);
}

#[test]
fn import_node_from_document_clones_clonable_shadow_root_when_deep_false() {
  fn find_in_subtree_by_id(doc: &Document, root: NodeId, id: &str) -> Option<NodeId> {
    doc
      .subtree_preorder(root)
      .find(|&node_id| match &doc.node(node_id).kind {
        NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes
          .iter()
          .any(|(k, v)| k.eq_ignore_ascii_case("id") && v == id),
        _ => false,
      })
  }

  let html = concat!(
    "<!doctype html>",
    "<div id=host>",
    "<template shadowroot=open shadowrootclonable><span id=shadow>shadow</span></template>",
    "<p id=light>light</p>",
    "</div>",
  );
  let src = crate::dom2::parse_html(html).unwrap();
  let host = src
    .get_element_by_id("host")
    .expect("host element not found");
  let shadow_root = src
    .shadow_root_for_host(host)
    .expect("expected a shadow root child");
  let shadow_span = find_in_subtree_by_id(&src, host, "shadow").expect("shadow span not found");

  let mut dst = Document::new(QuirksMode::NoQuirks);
  let (imported, mapping) = dst
    .import_node_from_document(&src, host, /* deep */ false)
    .unwrap();
  assert_eq!(dst.parent(imported).unwrap(), None);

  // Mapping order should follow the source subtree's pre-order traversal (including the shadow root).
  assert_eq!(mapping.len(), 3);
  assert_eq!(mapping[0].0, host);
  assert_eq!(mapping[1].0, shadow_root);
  assert_eq!(mapping[2].0, shadow_span);

  let dst_shadow_root = mapping[1].1;
  let dst_shadow_span = mapping[2].1;
  assert_eq!(dst.parent(dst_shadow_root).unwrap(), Some(imported));
  assert_eq!(dst.parent(dst_shadow_span).unwrap(), Some(dst_shadow_root));

  match &dst.node(dst_shadow_root).kind {
    NodeKind::ShadowRoot { clonable, .. } => assert!(*clonable),
    other => panic!("expected ShadowRoot node, got {other:?}"),
  }

  // `deep=false` should not clone light DOM children.
  assert!(find_in_subtree_by_id(&dst, imported, "light").is_none());
  // Shadow DOM children are cloned shallowly.
  assert!(dst.children(dst_shadow_span).unwrap().is_empty());
}
