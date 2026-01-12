#![cfg(test)]

use super::live_mutation::{LiveMutationEvent, LiveMutationTestRecorder};
use super::Document;
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
      count: 1
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
      old_index: 1
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
        old_index: 0
      },
      LiveMutationEvent::PreRemove {
        node: b,
        old_parent: frag,
        old_index: 1
      },
      LiveMutationEvent::PreInsert {
        parent,
        index: 1,
        count: 2
      }
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
        old_index: 0
      },
      LiveMutationEvent::PreInsert {
        parent,
        index: 0,
        count: 1
      }
    ]
  );
}

#[test]
fn set_text_data_emits_replace_data_with_byte_lengths() {
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
      inserted_len: 3
    }]
  );
}

