#![cfg(feature = "a11y_accesskit")]

use fastrender::accessibility;
use fastrender::accessibility::accesskit_bridge::tree_update_from_accessibility_tree;
use fastrender::accessibility::accesskit_ids::accesskit_id_for_dom2;
use fastrender::api::{BrowserDocumentDom2, RenderOptions};
use fastrender::dom::HTML_NAMESPACE;

#[test]
fn accesskit_node_ids_are_stable_across_dom2_insertions() {
  let html = r#"<!doctype html>
    <html>
      <body>
        <div id="container">
          <button id="target">Target</button>
        </div>
      </body>
    </html>"#;

  let options = RenderOptions::new().with_viewport(800, 600);
  let mut doc = BrowserDocumentDom2::from_html(html, options).expect("create BrowserDocumentDom2");
  doc.render_frame().expect("render initial frame");

  let target_dom2 = doc
    .dom()
    .get_element_by_id("target")
    .expect("target element exists");

  let mapping_before = doc.last_dom_mapping().expect("renderer dom mapping exists");
  let preorder_before = mapping_before
    .preorder_for_node_id(target_dom2)
    .expect("target has preorder id");

  let expected_id = accesskit_id_for_dom2(target_dom2);

  let prepared = doc.prepared().expect("prepared document exists");
  let a11y = accessibility::build_accessibility_tree(prepared.styled_tree(), None)
    .expect("build accessibility tree");
  let update_before = tree_update_from_accessibility_tree(&a11y, Some(mapping_before));

  assert!(
    update_before.nodes.iter().any(|(id, _)| *id == expected_id),
    "expected AccessKit tree to include target element node id"
  );

  // Insert a new sibling *before* the target to force renderer preorder renumbering.
  doc.mutate_dom(|dom| {
    let container = dom.get_element_by_id("container").expect("container exists");
    let target = dom.get_element_by_id("target").expect("target exists");
    let inserted = dom.create_element("button", HTML_NAMESPACE);
    dom
      .set_attribute(inserted, "id", "inserted")
      .expect("set inserted id");
    dom
      .insert_before(container, inserted, Some(target))
      .expect("insert sibling before target");
    true
  });

  doc.render_frame().expect("render mutated frame");
  let mapping_after = doc.last_dom_mapping().expect("renderer dom mapping exists");
  let preorder_after = mapping_after
    .preorder_for_node_id(target_dom2)
    .expect("target has preorder id after mutation");

  assert_ne!(
    preorder_before, preorder_after,
    "expected DOM insertion to renumber renderer preorder ids"
  );

  let prepared = doc.prepared().expect("prepared document exists after mutation");
  let a11y = accessibility::build_accessibility_tree(prepared.styled_tree(), None)
    .expect("build accessibility tree");
  let update_after = tree_update_from_accessibility_tree(&a11y, Some(mapping_after));

  assert!(
    update_after.nodes.iter().any(|(id, _)| *id == expected_id),
    "expected AccessKit node id for target element to remain stable across renumbering"
  );
}

