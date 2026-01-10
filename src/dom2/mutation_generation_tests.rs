#![cfg(test)]

use super::Document;
use selectors::context::QuirksMode;

#[test]
fn mutation_generation_increments_on_render_affecting_mutations() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  assert_eq!(doc.mutation_generation(), 0);

  let root = doc.root();
  let div = doc.create_element("div", "");
  let span = doc.create_element("span", "");
  assert_eq!(
    doc.mutation_generation(),
    0,
    "creating detached nodes must not affect generation"
  );

  assert!(doc.append_child(root, div).unwrap());
  let gen_after_append = doc.mutation_generation();
  assert_eq!(gen_after_append, 1);

  // Re-appending the already-last child is a no-op and must not bump the generation.
  assert!(doc.append_child(div, span).unwrap());
  assert_eq!(doc.mutation_generation(), gen_after_append + 1);

  assert!(!doc.append_child(div, span).unwrap());
  let gen_after_noop_append = doc.mutation_generation();
  assert_eq!(gen_after_noop_append, gen_after_append + 1);

  assert!(doc.set_attribute(span, "id", "a").unwrap());
  let gen_after_attr = doc.mutation_generation();
  assert_eq!(gen_after_attr, gen_after_noop_append + 1);

  // Setting the same value again is a no-op.
  assert!(!doc.set_attribute(span, "id", "a").unwrap());
  assert_eq!(doc.mutation_generation(), gen_after_attr);

  let text = doc.create_text("hi");
  assert!(doc.append_child(span, text).unwrap());
  let gen_after_text_insert = doc.mutation_generation();
  assert_eq!(gen_after_text_insert, gen_after_attr + 1);

  assert!(doc.set_text_data(text, "bye").unwrap());
  let gen_after_text_update = doc.mutation_generation();
  assert_eq!(gen_after_text_update, gen_after_text_insert + 1);

  assert!(!doc.set_text_data(text, "bye").unwrap());
  assert_eq!(doc.mutation_generation(), gen_after_text_update);
}
