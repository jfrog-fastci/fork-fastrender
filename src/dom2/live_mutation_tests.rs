#![cfg(test)]

use super::live_mutation::{LiveMutationEvent, LiveMutationTestRecorder};
use super::Document;
use super::DomError;
use super::NodeKind;
use selectors::context::QuirksMode;

#[test]
fn insert_before_emits_pre_insert() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let recorder = LiveMutationTestRecorder::default();
  doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();
  let _ = recorder.take();

  let child = doc.create_element("span", "");
  assert!(doc.insert_before(parent, child, None).unwrap());

  assert_eq!(
    recorder.take(),
    vec![LiveMutationEvent::PreInsert {
      parent,
      index: 0,
      count: 1,
    }]
  );
}

#[test]
fn remove_child_emits_pre_remove_with_old_parent_and_index() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let recorder = LiveMutationTestRecorder::default();
  doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();
  let a = doc.create_element("a", "");
  let b = doc.create_element("b", "");
  doc.append_child(parent, a).unwrap();
  doc.append_child(parent, b).unwrap();
  let _ = recorder.take();

  assert!(doc.remove_child(parent, b).unwrap());
  assert_eq!(
    recorder.take(),
    vec![LiveMutationEvent::PreRemove {
      node: b,
      old_parent: parent,
      old_index: 1,
    }]
  );
}

#[test]
fn fragment_insertion_emits_pre_remove_for_fragment_children_then_pre_insert() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let recorder = LiveMutationTestRecorder::default();
  doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let x = doc.create_element("x", "");
  let y = doc.create_element("y", "");
  doc.append_child(parent, x).unwrap();
  doc.append_child(parent, y).unwrap();

  let frag = doc.create_document_fragment();
  let a = doc.create_element("a", "");
  let b = doc.create_element("b", "");
  doc.append_child(frag, a).unwrap();
  doc.append_child(frag, b).unwrap();
  let _ = recorder.take();

  assert!(doc.insert_before(parent, frag, Some(y)).unwrap());

  assert_eq!(
    recorder.take(),
    vec![
      LiveMutationEvent::PreRemove {
        node: a,
        old_parent: frag,
        old_index: 0,
      },
      LiveMutationEvent::PreRemove {
        node: b,
        old_parent: frag,
        old_index: 1,
      },
      LiveMutationEvent::PreInsert {
        parent,
        index: 1,
        count: 2,
      },
    ]
  );
}

#[test]
fn replace_child_emits_pre_remove_then_pre_insert() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let recorder = LiveMutationTestRecorder::default();
  doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let old_child = doc.create_element("old", "");
  let keep = doc.create_element("keep", "");
  doc.append_child(parent, old_child).unwrap();
  doc.append_child(parent, keep).unwrap();
  let _ = recorder.take();

  let replacement = doc.create_element("new", "");
  assert!(doc.replace_child(parent, replacement, old_child).unwrap());

  assert_eq!(
    recorder.take(),
    vec![
      LiveMutationEvent::PreRemove {
        node: old_child,
        old_parent: parent,
        old_index: 0,
      },
      LiveMutationEvent::PreInsert {
        parent,
        index: 0,
        count: 1,
      },
    ]
  );
}

#[test]
fn set_text_data_emits_replace_data() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let recorder = LiveMutationTestRecorder::default();
  doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let text = doc.create_text("hi");
  doc.append_child(parent, text).unwrap();
  let _ = recorder.take();

  assert!(doc.set_text_data(text, "bye").unwrap());

  assert_eq!(
    recorder.take(),
    vec![LiveMutationEvent::ReplaceData {
      node: text,
      offset: 0,
      removed_len: 2,
      inserted_len: 3,
    }]
  );
}

#[test]
fn replace_data_emits_replace_data() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let recorder = LiveMutationTestRecorder::default();
  doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let text = doc.create_text("hi");
  doc.append_child(parent, text).unwrap();
  let _ = recorder.take();

  assert!(doc.replace_data(text, 0, usize::MAX, "bye").unwrap());

  assert_eq!(
    recorder.take(),
    vec![LiveMutationEvent::ReplaceData {
      node: text,
      offset: 0,
      removed_len: 2,
      inserted_len: 3,
    }]
  );
}

#[test]
fn replace_data_uses_utf16_code_units() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let recorder = LiveMutationTestRecorder::default();
  doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  // U+1F600 GRINNING FACE is encoded as a surrogate pair in UTF-16 (2 code units).
  let text = doc.create_text("😀");
  doc.append_child(parent, text).unwrap();
  let _ = recorder.take();

  assert!(doc.set_text_data(text, "a").unwrap());

  assert_eq!(
    recorder.take(),
    vec![LiveMutationEvent::ReplaceData {
      node: text,
      offset: 0,
      removed_len: 2,
      inserted_len: 1,
    }]
  );
}

#[test]
fn replace_data_offset_uses_utf16_code_units() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let recorder = LiveMutationTestRecorder::default();
  doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  // U+1F600 GRINNING FACE is encoded as a surrogate pair in UTF-16 (2 code units).
  let text = doc.create_text("😀");
  doc.append_child(parent, text).unwrap();
  let _ = recorder.take();

  // Insert after the emoji. If `offset` were interpreted as a byte index (4 bytes for 😀), this
  // would target the middle of the UTF-8 encoding and fail.
  assert!(doc.replace_data(text, 2, 0, "a").unwrap());

  assert_eq!(
    recorder.take(),
    vec![LiveMutationEvent::ReplaceData {
      node: text,
      offset: 2,
      removed_len: 0,
      inserted_len: 1,
    }]
  );
  assert_eq!(doc.text_data(text).unwrap(), "😀a");
}

#[test]
fn set_comment_data_emits_replace_data() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let recorder = LiveMutationTestRecorder::default();
  doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let comment = doc.create_comment("hi");
  doc.append_child(parent, comment).unwrap();
  let _ = recorder.take();

  assert!(doc.set_comment_data(comment, "bye").unwrap());

  assert_eq!(
    recorder.take(),
    vec![LiveMutationEvent::ReplaceData {
      node: comment,
      offset: 0,
      removed_len: 2,
      inserted_len: 3,
    }]
  );
}

#[test]
fn set_processing_instruction_data_emits_replace_data() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let recorder = LiveMutationTestRecorder::default();
  doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

  let pi = doc.push_node(
    NodeKind::ProcessingInstruction {
      target: "x".to_string(),
      data: "hi".to_string(),
    },
    None,
    /* inert_subtree */ false,
  );
  let _ = recorder.take();

  assert!(doc.set_processing_instruction_data(pi, "bye").unwrap());

  assert_eq!(
    recorder.take(),
    vec![LiveMutationEvent::ReplaceData {
      node: pi,
      offset: 0,
      removed_len: 2,
      inserted_len: 3,
    }]
  );
}

#[test]
fn move_between_parents_emits_pre_remove_then_pre_insert() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let recorder = LiveMutationTestRecorder::default();
  doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

  let root = doc.root();
  let p1 = doc.create_element("div", "");
  let p2 = doc.create_element("div", "");
  doc.append_child(root, p1).unwrap();
  doc.append_child(root, p2).unwrap();

  let child = doc.create_text("x");
  doc.append_child(p1, child).unwrap();
  let _ = recorder.take();

  assert!(doc.append_child(p2, child).unwrap());

  assert_eq!(
    recorder.take(),
    vec![
      LiveMutationEvent::PreRemove {
        node: child,
        old_parent: p1,
        old_index: 0,
      },
      LiveMutationEvent::PreInsert {
        parent: p2,
        index: 0,
        count: 1,
      },
    ]
  );
}

#[test]
fn live_traversal_registry_is_gc_safe_and_sweeps_dead_entries() -> Result<(), vm_js::VmError> {
  use vm_js::{Heap, HeapLimits, Value, WeakGcObject};

  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024));
  let mut scope = heap.scope();

  let obj = scope.alloc_object()?;
  let weak = WeakGcObject::from(obj);
  let root = scope.heap_mut().add_root(Value::Object(obj))?;

  let mut doc = Document::new(QuirksMode::NoQuirks);
  let id = doc.register_live_range(scope.heap(), obj);
  assert_eq!(doc.live_mutation.live_range_len(), 1);
  assert_eq!(doc.range_start_container(id).unwrap(), doc.root());
  assert_eq!(doc.range_start_offset(id).unwrap(), 0);

  // Drop the last root and force a GC; the registry must not keep the JS object alive.
  scope.heap_mut().remove_root(root);
  scope.heap_mut().collect_garbage();
  assert!(
    weak.upgrade(scope.heap()).is_none(),
    "registered WeakGcObject must not prevent collection"
  );

  // After a GC run, sweeping should prune the dead registry entry.
  doc.sweep_dead_live_traversals_if_needed(scope.heap());
  assert_eq!(doc.live_mutation.live_range_len(), 0);
  assert!(matches!(
    doc.range_start_container(id),
    Err(super::DomError::NotFoundError)
  ));
  Ok(())
}

#[test]
fn live_range_state_is_pruned_without_affecting_other_live_ranges() -> Result<(), vm_js::VmError> {
  use vm_js::{Heap, HeapLimits, Value, WeakGcObject};

  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024));
  let mut scope = heap.scope();

  let mut doc = Document::new(QuirksMode::NoQuirks);

  // Register two independent live ranges.
  let obj1 = scope.alloc_object()?;
  let weak1 = WeakGcObject::from(obj1);
  let root1 = scope.heap_mut().add_root(Value::Object(obj1))?;
  let id1 = doc.register_live_range(scope.heap(), obj1);

  let obj2 = scope.alloc_object()?;
  let _root2 = scope.heap_mut().add_root(Value::Object(obj2))?;
  let id2 = doc.register_live_range(scope.heap(), obj2);

  assert_eq!(doc.live_mutation.live_range_len(), 2);
  assert!(doc.range_start_container(id1).is_ok());
  assert!(doc.range_start_container(id2).is_ok());

  // GC only wrapper 1.
  scope.heap_mut().remove_root(root1);
  scope.heap_mut().collect_garbage();
  assert!(weak1.upgrade(scope.heap()).is_none());

  // Sweeping should prune range state for wrapper 1 without touching wrapper 2.
  doc.sweep_dead_live_traversals_if_needed(scope.heap());
  assert!(matches!(
    doc.range_start_container(id1),
    Err(super::DomError::NotFoundError)
  ));
  assert_eq!(doc.range_start_container(id2).unwrap(), doc.root());

  // Register a new range after tombstoning; existing ids must remain valid.
  let obj3 = scope.alloc_object()?;
  let _root3 = scope.heap_mut().add_root(Value::Object(obj3))?;
  let id3 = doc.register_live_range(scope.heap(), obj3);

  assert_eq!(doc.live_mutation.live_range_len(), 2);
  assert_eq!(doc.range_start_container(id2).unwrap(), doc.root());
  assert_eq!(doc.range_start_container(id3).unwrap(), doc.root());
  Ok(())
}

#[test]
fn node_iterator_registry_is_gc_safe_and_prunes_rust_state() -> Result<(), vm_js::VmError> {
  use vm_js::{Heap, HeapLimits, Value, WeakGcObject};

  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024));
  let mut scope = heap.scope();

  let obj = scope.alloc_object()?;
  let weak = WeakGcObject::from(obj);
  let root = scope.heap_mut().add_root(Value::Object(obj))?;

  let mut doc = Document::new(QuirksMode::NoQuirks);
  let id = doc.create_node_iterator(doc.root());
  doc.register_node_iterator_wrapper(scope.heap(), id, obj);
  assert_eq!(doc.live_mutation.node_iterator_wrapper_len(), 1);
  assert_eq!(doc.node_iterator_root(id), Some(doc.root()));

  // Drop the last root and force a GC; the registry must not keep the JS object alive.
  scope.heap_mut().remove_root(root);
  scope.heap_mut().collect_garbage();
  assert!(
    weak.upgrade(scope.heap()).is_none(),
    "registered WeakGcObject must not prevent collection"
  );

  // After a GC run, sweeping should prune both the weak registry entry and the Document's
  // NodeIterator traversal state.
  doc.sweep_dead_live_traversals_if_needed(scope.heap());
  assert_eq!(doc.live_mutation.node_iterator_wrapper_len(), 0);
  assert_eq!(doc.node_iterator_root(id), None);
  Ok(())
}

#[test]
fn live_range_registry_prunes_rust_ranges_and_does_not_leak() -> Result<(), vm_js::VmError> {
  use vm_js::{Heap, HeapLimits, RootId, Value};

  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024));
  let mut scope = heap.scope();
  let mut doc = Document::new(QuirksMode::NoQuirks);

  fn create_n(
    scope: &mut vm_js::Scope<'_>,
    doc: &mut Document,
    n: usize,
  ) -> Result<Vec<RootId>, vm_js::VmError> {
    let mut roots: Vec<RootId> = Vec::with_capacity(n);
    for _ in 0..n {
      let wrapper = scope.alloc_object()?;
      let root = scope.heap_mut().add_root(Value::Object(wrapper))?;
      roots.push(root);
      let _range_id = doc.register_live_range(scope.heap(), wrapper);
    }
    Ok(roots)
  }

  const N: usize = 128;

  // Cycle 1: allocate ranges + wrappers.
  let wrapper1 = scope.alloc_object()?;
  let root1 = scope.heap_mut().add_root(Value::Object(wrapper1))?;
  let r1 = doc.register_live_range(scope.heap(), wrapper1);
  let roots = create_n(&mut scope, &mut doc, N - 1)?;
  assert_eq!(doc.ranges.len(), N);

  // Drop roots and collect.
  scope.heap_mut().remove_root(root1);
  for root in roots {
    scope.heap_mut().remove_root(root);
  }
  scope.heap_mut().collect_garbage();
  doc.sweep_dead_live_traversals_if_needed(scope.heap());
  assert_eq!(doc.ranges.len(), 0);
  assert!(
    matches!(doc.range_start_container(r1), Err(DomError::NotFoundError)),
    "RangeId should be tombstoned once its JS wrapper is collected"
  );

  // Cycle 2: allocate the same number again. Map size should not grow monotonically.
  let roots = create_n(&mut scope, &mut doc, N)?;
  assert_eq!(doc.ranges.len(), N);

  // Drop roots and collect again.
  for root in roots {
    scope.heap_mut().remove_root(root);
  }
  scope.heap_mut().collect_garbage();
  doc.sweep_dead_live_traversals_if_needed(scope.heap());
  assert_eq!(doc.ranges.len(), 0);
  Ok(())
}

#[test]
fn node_iterator_state_is_pruned_on_subsequent_registration() -> Result<(), vm_js::VmError> {
  use vm_js::{Heap, HeapLimits, Value, WeakGcObject};

  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024));
  let mut scope = heap.scope();

  let mut doc = Document::new(QuirksMode::NoQuirks);

  // Register a first NodeIterator wrapper.
  let obj1 = scope.alloc_object()?;
  let weak1 = WeakGcObject::from(obj1);
  let root1 = scope.heap_mut().add_root(Value::Object(obj1))?;
  let id1 = doc.create_node_iterator(doc.root());
  doc.register_node_iterator_wrapper(scope.heap(), id1, obj1);
  assert_eq!(doc.node_iterator_root(id1), Some(doc.root()));

  // Drop wrapper 1 and GC it.
  scope.heap_mut().remove_root(root1);
  scope.heap_mut().collect_garbage();
  assert!(weak1.upgrade(scope.heap()).is_none());

  // Registering a *new* wrapper should sweep dead entries and prune the stale NodeIterator state.
  let obj2 = scope.alloc_object()?;
  let _root2 = scope.heap_mut().add_root(Value::Object(obj2))?;
  let id2 = doc.create_node_iterator(doc.root());
  doc.register_node_iterator_wrapper(scope.heap(), id2, obj2);

  assert_eq!(doc.node_iterator_root(id1), None);
  assert_eq!(doc.node_iterator_root(id2), Some(doc.root()));
  Ok(())
}

#[test]
fn live_range_state_is_swept_and_does_not_grow_unbounded() -> Result<(), vm_js::VmError> {
  use vm_js::{Heap, HeapLimits, Value};

  let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 4 * 1024 * 1024));
  let mut scope = heap.scope();

  let mut doc = Document::new(QuirksMode::NoQuirks);

  // Keep one live range around across multiple GC cycles to ensure sweeping does not break
  // still-live range state.
  let live_wrapper = scope.alloc_object()?;
  let live_root = scope.heap_mut().add_root(Value::Object(live_wrapper))?;
  let live_range = doc.register_live_range(scope.heap(), live_wrapper);

  let base = doc.range_state_len_for_test();
  assert_eq!(base, 1, "expected only the persistent live range to be registered");

  for cycle in 0..3usize {
    // Allocate a batch of short-lived ranges whose wrappers become unreachable immediately.
    for _ in 0..1000usize {
      let wrapper = scope.alloc_object()?;
      let root = scope.heap_mut().add_root(Value::Object(wrapper))?;
      let _range = doc.register_live_range(scope.heap(), wrapper);
      scope.heap_mut().remove_root(root);
    }

    scope.heap_mut().collect_garbage();
    doc.sweep_dead_live_traversals_if_needed(scope.heap());

    // The persistent range should remain usable.
    assert_eq!(doc.range_start_container(live_range).unwrap(), doc.root());

    assert_eq!(
      doc.range_state_len_for_test(),
      base,
      "live range state should be reclaimed after GC+sweep (cycle {cycle})"
    );
  }

  scope.heap_mut().remove_root(live_root);
  Ok(())
}
