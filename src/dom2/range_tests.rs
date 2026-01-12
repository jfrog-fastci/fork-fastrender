#![cfg(test)]

use super::{parse_html, Document, NodeKind};

#[test]
fn range_tree_root_stops_at_shadow_root_and_pre_remove_does_not_cross_shadow_boundary() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>",
    "<template shadowrootmode=open>",
    "<span id=inside></span>",
    "</template>",
    "</div>",
    "<p id=light></p>",
    "</body></html>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let light = doc.get_element_by_id("light").expect("light node not found");
  let host = doc.get_element_by_id("host").expect("host node not found");

  let shadow_root = doc.node(host).children[0];
  assert!(
    matches!(doc.node(shadow_root).kind, NodeKind::ShadowRoot { .. }),
    "expected host to have an attached ShadowRoot"
  );
  let inside = doc.node(shadow_root).children[0];

  let range = doc.create_range();
  doc.range_set_start(range, inside, 0).unwrap();

  let start_container = doc.range_start_container(range).unwrap();
  assert_eq!(
    doc.tree_root_for_range(start_container),
    shadow_root,
    "Range start container inside a shadow tree should have the ShadowRoot as its tree root"
  );
  assert_ne!(
    doc.tree_root_for_range(start_container),
    doc.root(),
    "ShadowRoot tree root must differ from the Document root"
  );

  // Per DOM, setting an endpoint to a different tree root collapses/adjusts the other endpoint so
  // both boundary points end up in the same root.
  doc.range_set_end(range, light, 0).unwrap();
  assert_eq!(doc.range_start_container(range).unwrap(), light);
  assert_eq!(doc.range_end_container(range).unwrap(), light);

  // Move the range back into the shadow tree (also exercises root mismatch handling in setStart).
  doc.range_set_start(range, inside, 0).unwrap();
  assert_eq!(doc.range_start_container(range).unwrap(), inside);
  assert_eq!(doc.range_end_container(range).unwrap(), inside);

  // Removing the shadow host from the document must not rewrite ranges in its shadow tree.
  let body = doc.body().expect("expected body element");
  assert!(doc.remove_child(body, host).unwrap());
  assert_eq!(doc.range_start_container(range).unwrap(), inside);
  assert_eq!(doc.range_end_container(range).unwrap(), inside);
}
