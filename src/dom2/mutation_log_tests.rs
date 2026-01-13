#![cfg(test)]

use super::Document;
use selectors::context::QuirksMode;

#[test]
fn mutation_log_records_attribute_names_per_node() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let div = doc.create_element("div", "");
  assert!(doc.append_child(root, div).unwrap());

  // Ignore insertion mutations so we observe only attribute bookkeeping.
  let _ = doc.take_mutations();

  assert!(doc.set_attribute(div, "ClAsS", "a").unwrap());
  assert!(doc.set_attribute(div, "data-X", "1").unwrap());
  assert!(doc.remove_attribute(div, "CLASS").unwrap());

  // DOMTokenList / style shims should also record attribute names.
  assert!(doc.class_list_add(div, &["foo"]).unwrap());
  assert!(doc.style_set_property(div, "display", "none").unwrap());

  let mutations = doc.take_mutations();
  let attrs = mutations
    .attribute_changed
    .get(&div)
    .expect("expected div entry in attribute_changed map");

  // HTML attribute names are ASCII-case-insensitive; they should be normalized to lowercase.
  assert!(attrs.contains("class"));
  assert!(attrs.contains("data-x"));
  assert!(attrs.contains("style"));
}

