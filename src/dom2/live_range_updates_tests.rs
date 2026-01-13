#![cfg(test)]

use super::Document;
use selectors::context::QuirksMode;

#[test]
fn live_range_pre_insert_shifts_offsets_in_parent() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let a = doc.create_element("a", "");
  let b = doc.create_element("b", "");
  let c = doc.create_element("c", "");
  doc.append_child(parent, a).unwrap();
  doc.append_child(parent, b).unwrap();
  doc.append_child(parent, c).unwrap();

  // Two live ranges anchored in the same parent.
  let r1 = doc.create_range();
  doc.range_set_start(r1, parent, 0).unwrap();
  doc.range_set_end(r1, parent, 3).unwrap();

  let r2 = doc.create_range();
  doc.range_set_start(r2, parent, 2).unwrap();
  doc.range_set_end(r2, parent, 2).unwrap();

  // Insert a node before `b` (index 1). Offsets greater than 1 shift right by 1.
  let inserted = doc.create_element("x", "");
  assert!(doc.insert_before(parent, inserted, Some(b)).unwrap());

  assert_eq!(doc.range_start_offset(r1).unwrap(), 0);
  assert_eq!(doc.range_end_offset(r1).unwrap(), 4);

  assert_eq!(doc.range_start_offset(r2).unwrap(), 3);
  assert_eq!(doc.range_end_offset(r2).unwrap(), 3);
}

#[test]
fn live_range_replace_data_clamps_and_shifts_offsets() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();
  let text = doc.create_text("0123456789");
  doc.append_child(parent, text).unwrap();

  let r1 = doc.create_range();
  doc.range_set_start(r1, text, 4).unwrap(); // inside replaced region
  doc.range_set_end(r1, text, 9).unwrap(); // after replaced region

  let r2 = doc.create_range();
  doc.range_set_start(r2, text, 7).unwrap(); // == offset + count
  doc.range_set_end(r2, text, 7).unwrap();

  // Replace 4 code units starting at offset 3 with 2 code units ("XX").
  // Offsets in (3, 7] clamp to 3. Offsets > 7 shift by (2 - 4) = -2.
  assert!(doc.replace_data(text, 3, 4, "XX").unwrap());

  assert_eq!(doc.range_start_offset(r1).unwrap(), 3);
  assert_eq!(doc.range_end_offset(r1).unwrap(), 7);

  assert_eq!(doc.range_start_offset(r2).unwrap(), 3);
  assert_eq!(doc.range_end_offset(r2).unwrap(), 3);
}

#[test]
fn live_range_split_text_moves_boundary_points_to_new_node() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let text = doc.create_text("abcdef");
  let sibling = doc.create_element("span", "");
  doc.append_child(parent, text).unwrap();
  doc.append_child(parent, sibling).unwrap();

  // Ranges anchored in the text node beyond the split point should be moved to the new node.
  let r1 = doc.create_range();
  doc.range_set_start(r1, text, 4).unwrap();
  doc.range_set_end(r1, text, 6).unwrap();

  // A range boundary point at (parent, index(text) + 1) must be shifted so it stays after the
  // entire original text node (now split into two nodes).
  let r2 = doc.create_range();
  doc.range_set_start(r2, parent, 1).unwrap();
  doc.range_set_end(r2, parent, 1).unwrap();

  let new_text = doc.split_text(text, 2).unwrap();

  assert_eq!(doc.text_data(text).unwrap(), "ab");
  assert_eq!(doc.text_data(new_text).unwrap(), "cdef");

  assert_eq!(doc.range_start_container(r1).unwrap(), new_text);
  assert_eq!(doc.range_start_offset(r1).unwrap(), 2);
  assert_eq!(doc.range_end_container(r1).unwrap(), new_text);
  assert_eq!(doc.range_end_offset(r1).unwrap(), 4);

  assert_eq!(doc.range_start_container(r2).unwrap(), parent);
  assert_eq!(doc.range_start_offset(r2).unwrap(), 2);
  assert_eq!(doc.range_end_container(r2).unwrap(), parent);
  assert_eq!(doc.range_end_offset(r2).unwrap(), 2);
}

