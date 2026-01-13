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

fn assert_range_collapsed(doc: &Document, range: super::RangeId, node: super::NodeId, offset: usize) {
  assert_eq!(doc.range_start_container(range).unwrap(), node);
  assert_eq!(doc.range_end_container(range).unwrap(), node);
  assert_eq!(doc.range_start_offset(range).unwrap(), offset);
  assert_eq!(doc.range_end_offset(range).unwrap(), offset);
}

#[test]
fn range_endpoints_update_after_replace_data_deletion_text() {
  // This matches the early-return CharacterData-only path in Range.deleteContents() /
  // Range.extractContents(): it performs a CharacterData replaceData/deleteData operation and
  // relies on the live-range "replace data" steps to collapse the range.
  let mut doc: Document = parse_html("<!doctype html><html></html>").unwrap();

  let text = doc.create_text("hello");
  let range = doc.create_range();
  doc.range_set_start(range, text, 1).unwrap();
  doc.range_set_end(range, text, 4).unwrap();

  // Delete the contents between the boundary points.
  doc.replace_data(text, 1, 3, "").unwrap();

  assert_range_collapsed(&doc, range, text, 1);
}

#[test]
fn range_endpoints_update_after_replace_data_deletion_comment() {
  let mut doc: Document = parse_html("<!doctype html><html></html>").unwrap();

  let comment = doc.create_comment("hello");
  let range = doc.create_range();
  doc.range_set_start(range, comment, 1).unwrap();
  doc.range_set_end(range, comment, 4).unwrap();

  doc.replace_data(comment, 1, 3, "").unwrap();

  assert_range_collapsed(&doc, range, comment, 1);
}

#[test]
fn range_endpoints_update_after_replace_data_deletion_processing_instruction() {
  let mut doc: Document = parse_html("<!doctype html><html></html>").unwrap();

  let pi = doc.create_processing_instruction("x", "hello");
  let range = doc.create_range();
  doc.range_set_start(range, pi, 1).unwrap();
  doc.range_set_end(range, pi, 4).unwrap();

  doc.replace_data(pi, 1, 3, "").unwrap();

  assert_range_collapsed(&doc, range, pi, 1);
}

#[test]
fn range_clone_extract_does_not_leak_persistent_subranges() {
  let html = "<!doctype html><div id=root><b id=b>hello</b><span id=mid>mid</span><i id=i>world</i></div>";

  // cloneContents should not allocate persistent subranges.
  {
    let mut doc: Document = parse_html(html).unwrap();
    let b = doc.get_element_by_id("b").unwrap();
    let i = doc.get_element_by_id("i").unwrap();
    let b_text = doc.node(b).children[0];
    let i_text = doc.node(i).children[0];

    let range = doc.create_range();
    doc.range_set_start(range, b_text, 1).unwrap();
    doc.range_set_end(range, i_text, 3).unwrap();

    let ranges_len = doc.ranges.len();
    for _ in 0..200 {
      let _ = doc.range_clone_contents(range).unwrap();
      assert_eq!(
        doc.ranges.len(),
        ranges_len,
        "range_clone_contents must not allocate persistent subranges"
      );
    }
  }

  // extractContents should not allocate persistent subranges either.
  {
    let mut doc: Document = parse_html(html).unwrap();
    let b = doc.get_element_by_id("b").unwrap();
    let i = doc.get_element_by_id("i").unwrap();
    let b_text = doc.node(b).children[0];
    let i_text = doc.node(i).children[0];

    let range = doc.create_range();
    doc.range_set_start(range, b_text, 1).unwrap();
    doc.range_set_end(range, i_text, 3).unwrap();

    let ranges_len = doc.ranges.len();
    for _ in 0..200 {
      let _ = doc.range_extract_contents(range).unwrap();
      assert_eq!(
        doc.ranges.len(),
        ranges_len,
        "range_extract_contents must not allocate persistent subranges"
      );
    }
  }
}

#[test]
fn range_common_ancestor_container_matches_dom_algorithm() {
  let html = "<!doctype html><div id=host><span id=a></span><span id=b></span></div>";
  let mut doc: Document = parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").expect("host element missing");
  let a = doc.get_element_by_id("a").expect("#a missing");
  let b = doc.get_element_by_id("b").expect("#b missing");

  // Different branch containers should yield their closest common ancestor.
  let range = doc.create_range();
  doc.range_set_start(range, a, 0).unwrap();
  doc.range_set_end(range, b, 0).unwrap();
  assert_eq!(doc.range_common_ancestor_container(range).unwrap(), host);

  // Identical containers should yield that container.
  doc.range_set_start(range, a, 0).unwrap();
  doc.range_set_end(range, a, 0).unwrap();
  assert_eq!(doc.range_common_ancestor_container(range).unwrap(), a);

  // When one container is an ancestor of the other, the ancestor is returned.
  doc.range_set_start(range, host, 0).unwrap();
  doc.range_set_end(range, a, 0).unwrap();
  assert_eq!(doc.range_common_ancestor_container(range).unwrap(), host);

}

#[test]
fn live_range_pre_insert_shifts_boundary_points_in_parent_for_single_node_insertion() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=parent>",
    "<span id=a></span><span id=b></span><span id=c></span>",
    "</div>",
    "</body></html>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let parent = doc.get_element_by_id("parent").unwrap();
  let a = doc.get_element_by_id("a").unwrap();
  let b = doc.get_element_by_id("b").unwrap();

  // Three live ranges anchored in the same parent at different offsets.
  let r_all = doc.create_range();
  doc.range_set_start(r_all, parent, 0).unwrap();
  doc.range_set_end(r_all, parent, 3).unwrap();

  let r_after_b = doc.create_range();
  doc.range_set_start(r_after_b, parent, 2).unwrap();
  doc.range_set_end(r_after_b, parent, 2).unwrap();

  let r_at_insertion_point = doc.create_range();
  doc
    .range_set_start(r_at_insertion_point, parent, 1)
    .unwrap();
  doc
    .range_set_end(r_at_insertion_point, parent, 1)
    .unwrap();

  // A range whose boundary point is inside a child should not be affected.
  let r_inside_child = doc.create_range();
  doc.range_set_start(r_inside_child, a, 0).unwrap();
  doc.range_set_end(r_inside_child, a, 0).unwrap();

  let new_node = doc.create_element("span", "");
  assert!(doc.insert_before(parent, new_node, Some(b)).unwrap());

  assert_eq!(doc.range_start_container(r_all).unwrap(), parent);
  assert_eq!(doc.range_start_offset(r_all).unwrap(), 0);
  assert_eq!(doc.range_end_container(r_all).unwrap(), parent);
  assert_eq!(doc.range_end_offset(r_all).unwrap(), 4);

  assert_eq!(doc.range_start_container(r_after_b).unwrap(), parent);
  assert_eq!(doc.range_start_offset(r_after_b).unwrap(), 3);
  assert_eq!(doc.range_end_container(r_after_b).unwrap(), parent);
  assert_eq!(doc.range_end_offset(r_after_b).unwrap(), 3);

  // Inserting at the boundary point should not move the boundary.
  assert_eq!(
    doc.range_start_offset(r_at_insertion_point).unwrap(),
    1
  );
  assert_eq!(doc.range_end_offset(r_at_insertion_point).unwrap(), 1);

  assert_eq!(doc.range_start_container(r_inside_child).unwrap(), a);
  assert_eq!(doc.range_start_offset(r_inside_child).unwrap(), 0);
  assert_eq!(doc.range_end_container(r_inside_child).unwrap(), a);
  assert_eq!(doc.range_end_offset(r_inside_child).unwrap(), 0);
}

#[test]
fn live_range_pre_insert_shifts_boundary_points_by_fragment_child_count() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=parent>",
    "<span id=a></span><span id=b></span><span id=c></span>",
    "</div>",
    "</body></html>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let parent = doc.get_element_by_id("parent").unwrap();
  let b = doc.get_element_by_id("b").unwrap();

  let r_end_after_all_children = doc.create_range();
  doc
    .range_set_start(r_end_after_all_children, parent, 0)
    .unwrap();
  doc
    .range_set_end(r_end_after_all_children, parent, 3)
    .unwrap();

  let r_after_b = doc.create_range();
  doc.range_set_start(r_after_b, parent, 2).unwrap();
  doc.range_set_end(r_after_b, parent, 2).unwrap();

  let frag = doc.create_document_fragment();
  let x = doc.create_element("span", "");
  let y = doc.create_element("span", "");
  assert!(doc.append_child(frag, x).unwrap());
  assert!(doc.append_child(frag, y).unwrap());

  assert!(doc.insert_before(parent, frag, Some(b)).unwrap());

  // Two nodes inserted before index 1 => offsets > 1 increase by 2.
  assert_eq!(doc.range_end_offset(r_end_after_all_children).unwrap(), 5);
  assert_eq!(doc.range_start_offset(r_after_b).unwrap(), 4);
  assert_eq!(doc.range_end_offset(r_after_b).unwrap(), 4);
}
