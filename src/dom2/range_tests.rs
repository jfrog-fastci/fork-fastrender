#![cfg(test)]

use super::{parse_html, Document, DomError, NodeKind, SlotAssignmentMode};
use crate::dom::{ShadowRootMode, HTML_NAMESPACE};
use selectors::context::QuirksMode;
use std::collections::HashMap;

#[test]
fn range_set_start_end_reject_doctype_with_invalid_node_type_error() {
  let mut doc: Document = Document::new(QuirksMode::NoQuirks);
  let range = doc.create_range();
  let doctype = doc.create_doctype("html", "", "");

  assert_eq!(
    doc.range_set_start(range, doctype, 0),
    Err(DomError::InvalidNodeTypeError)
  );
  assert_eq!(
    doc.range_set_end(range, doctype, 0),
    Err(DomError::InvalidNodeTypeError)
  );
}

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
fn range_collapse_updates_boundary_points() {
  let html = "<!doctype html><html><body><p id=p>Hello</p></body></html>";
  let mut doc: Document = parse_html(html).unwrap();
  let p = doc.get_element_by_id("p").unwrap();
  let text = doc.node(p).children[0];

  let range = doc.create_range();
  doc.range_set_start(range, text, 1).unwrap();
  doc.range_set_end(range, text, 4).unwrap();

  doc.range_collapse(range, /* to_start */ true).unwrap();
  assert_range_collapsed(&doc, range, text, 1);

  // Move end elsewhere and collapse to end.
  doc.range_set_end(range, text, 3).unwrap();
  doc.range_collapse(range, /* to_start */ false).unwrap();
  assert_range_collapsed(&doc, range, text, 3);
}

#[test]
fn range_detach_is_noop() {
  let html = "<!doctype html><html><body><p id=p>Hello</p></body></html>";
  let mut doc: Document = parse_html(html).unwrap();
  let p = doc.get_element_by_id("p").unwrap();
  let text = doc.node(p).children[0];

  let range = doc.create_range();
  doc.range_set_start(range, text, 0).unwrap();
  doc.range_set_end(range, text, 2).unwrap();

  doc.range_detach(range).unwrap();
  assert_eq!(doc.range_start_offset(range).unwrap(), 0);
  assert_eq!(doc.range_end_offset(range).unwrap(), 2);
}

#[test]
fn range_clone_range_produces_independent_range() {
  let html = "<!doctype html><html><body><p id=p>Hello</p></body></html>";
  let mut doc: Document = parse_html(html).unwrap();
  let p = doc.get_element_by_id("p").unwrap();
  let text = doc.node(p).children[0];

  let range = doc.create_range();
  doc.range_set_start(range, text, 1).unwrap();
  doc.range_set_end(range, text, 4).unwrap();

  let cloned = doc.range_clone_range(range).unwrap();
  assert_ne!(range, cloned);
  assert_eq!(doc.range_start(range).unwrap(), doc.range_start(cloned).unwrap());
  assert_eq!(doc.range_end(range).unwrap(), doc.range_end(cloned).unwrap());

  // Mutating one range must not affect the other.
  doc.range_set_start(range, text, 0).unwrap();
  assert_eq!(doc.range_start_offset(range).unwrap(), 0);
  assert_eq!(doc.range_start_offset(cloned).unwrap(), 1);

  doc.range_set_end(cloned, text, 5).unwrap();
  assert_eq!(doc.range_end_offset(cloned).unwrap(), 5);
  assert_eq!(doc.range_end_offset(range).unwrap(), 4);
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
fn range_replace_data_shifts_offsets_after_replaced_region() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();
  let text = doc.create_text("hello");
  doc.append_child(parent, text).unwrap();

  let range = doc.create_range();
  doc.range_set_start(range, text, 4).unwrap();
  doc.range_set_end(range, text, 5).unwrap();

  // Replace "e" with "XYZ" at offset 1.
  assert!(doc.replace_data(text, 1, 1, "XYZ").unwrap());

  // Offsets after offset+removed_len (2) are shifted by inserted_len-removed_len (2).
  assert_eq!(doc.range_start_offset(range).unwrap(), 6);
  assert_eq!(doc.range_end_offset(range).unwrap(), 7);
}

#[test]
fn range_replace_data_collapses_offsets_in_replaced_region() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();
  let text = doc.create_text("hello");
  doc.append_child(parent, text).unwrap();

  let range = doc.create_range();
  doc.range_set_start(range, text, 2).unwrap();
  // End offset exactly at offset+removed_len should also collapse.
  doc.range_set_end(range, text, 3).unwrap();

  // Replace "el" with "X" at offset 1.
  assert!(doc.replace_data(text, 1, 2, "X").unwrap());

  assert_eq!(doc.range_start_offset(range).unwrap(), 1);
  assert_eq!(doc.range_end_offset(range).unwrap(), 1);
}

#[test]
fn range_delete_data_updates_offsets() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();
  let text = doc.create_text("hello");
  doc.append_child(parent, text).unwrap();

  let range = doc.create_range();
  doc.range_set_start(range, text, 4).unwrap();
  doc.range_set_end(range, text, 5).unwrap();

  assert!(doc.delete_data(text, 1, 2).unwrap());

  // Removed_len is clamped to 2; offsets after offset+removed_len (3) shift by -2.
  assert_eq!(doc.range_start_offset(range).unwrap(), 2);
  assert_eq!(doc.range_end_offset(range).unwrap(), 3);
}

#[test]
fn range_set_text_data_collapses_offsets_to_zero() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();
  let text = doc.create_text("hello");
  doc.append_child(parent, text).unwrap();

  let range = doc.create_range();
  doc.range_set_start(range, text, 2).unwrap();
  doc.range_set_end(range, text, 4).unwrap();

  assert!(doc.set_text_data(text, "bye").unwrap());

  // Setting `data` is specified as replaceData(0, length, ...), which collapses offsets > 0.
  assert_eq!(doc.range_start_offset(range).unwrap(), 0);
  assert_eq!(doc.range_end_offset(range).unwrap(), 0);
}

#[test]
fn range_delete_data_uses_utf16_code_units() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  // U+1F600 GRINNING FACE is 2 UTF-16 code units.
  let text = doc.create_text("😀b");
  doc.append_child(parent, text).unwrap();

  let range = doc.create_range();
  // Boundary after the entire text ("😀b") is at UTF-16 offset 3.
  doc.range_set_start(range, text, 3).unwrap();
  doc.range_set_end(range, text, 3).unwrap();

  // Delete the emoji (2 code units) and ensure the range shifts to remain after "b" (offset 1).
  assert!(doc.delete_data(text, 0, 2).unwrap());
  assert_eq!(doc.text_data(text).unwrap(), "b");
  assert_eq!(doc.range_start_offset(range).unwrap(), 1);
  assert_eq!(doc.range_end_offset(range).unwrap(), 1);
}

#[test]
fn range_clone_extract_does_not_leak_persistent_subranges() {
  let html =
    "<!doctype html><div id=root><b id=b>hello</b><span id=mid>mid</span><i id=i>world</i></div>";

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
fn range_offsets_exclude_shadow_root_children_for_host_elements() {
  // `dom2` stores ShadowRoot nodes as children of their host element so the renderer can traverse
  // them, but DOM Range boundary point offsets are defined in terms of the light DOM tree children
  // (which must exclude ShadowRoot).
  let html = concat!(
    "<!doctype html>",
    "<div id=host>",
    "<template shadowrootmode=open></template>",
    "<span id=light></span>",
    "</div>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").expect("host not found");
  let light = doc.get_element_by_id("light").expect("light child not found");

  // In JS: host.childNodes.length == 1, so offset 1 refers to the position *after* `light`.
  let range = doc.create_range();
  doc.range_set_start(range, host, 1).unwrap();

  // Setting the end to a boundary point before the start must collapse the range to the new end.
  doc.range_set_end(range, light, 0).unwrap();

  assert_eq!(doc.range_start_container(range).unwrap(), light);
  assert_eq!(doc.range_start_offset(range).unwrap(), 0);
  assert_eq!(doc.range_end_container(range).unwrap(), light);
  assert_eq!(doc.range_end_offset(range).unwrap(), 0);
}

#[test]
fn live_range_updates_use_tree_child_indices_excluding_shadow_root() {
  let html = concat!(
    "<!doctype html>",
    "<div id=host>",
    "<template shadowrootmode=open></template>",
    "<span id=a></span>",
    "<span id=b></span>",
    "</div>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").expect("host not found");
  let a = doc.get_element_by_id("a").expect("a not found");

  // In JS: host.childNodes == [a, b], so offset 1 is the boundary point between them.
  let range = doc.create_range();
  doc.range_set_start(range, host, 1).unwrap();

  // Insert a new light DOM child before `a` (tree index 0, raw index 1 due to ShadowRoot).
  let x = doc.create_element("span", "");
  assert!(doc.insert_before(host, x, Some(a)).unwrap());
  assert_eq!(doc.range_start_container(range).unwrap(), host);
  assert_eq!(doc.range_start_offset(range).unwrap(), 2);
  assert_eq!(doc.range_end_container(range).unwrap(), host);
  assert_eq!(doc.range_end_offset(range).unwrap(), 2);

  // Removing `a` (now tree index 1) must shift the boundary point left by one.
  assert!(doc.remove_child(host, a).unwrap());
  assert_eq!(doc.range_start_container(range).unwrap(), host);
  assert_eq!(doc.range_start_offset(range).unwrap(), 1);
  assert_eq!(doc.range_end_container(range).unwrap(), host);
  assert_eq!(doc.range_end_offset(range).unwrap(), 1);
}

#[test]
fn range_tree_child_mapping_excludes_shadow_root() {
  let html = concat!(
    "<!doctype html>",
    "<div id=host>",
    "<template shadowrootmode=open></template>",
    "<span id=a></span>",
    "<span id=b></span>",
    "</div>",
  );
  let doc: Document = parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").expect("host not found");
  let a = doc.get_element_by_id("a").expect("a not found");
  let b = doc.get_element_by_id("b").expect("b not found");
  assert_eq!(
    doc.tree_child_for_range(host, 0),
    Some(a),
    "offset 0 should map to the first light DOM child (not the shadow root)"
  );
  assert_eq!(doc.tree_child_for_range(host, 1), Some(b));
  assert_eq!(doc.tree_child_for_range(host, 2), None);
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

#[test]
fn range_offsets_ignore_shadow_root_pseudo_child() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>",
    "<template shadowrootmode=open></template>",
    "<p id=a></p>",
    "<p id=b></p>",
    "</div>",
    "</body></html>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").expect("host node not found");
  let a = doc.get_element_by_id("a").expect("a node not found");
  let b = doc.get_element_by_id("b").expect("b node not found");

  // dom2 stores the attached shadow root as a child of the host at index 0 for renderer traversal.
  let shadow_root = doc.node(host).children[0];
  assert!(
    matches!(doc.node(shadow_root).kind, NodeKind::ShadowRoot { .. }),
    "expected host to have an attached ShadowRoot"
  );
  assert_eq!(
    doc.node(host).children[1],
    a,
    "expected light DOM child to remain as a host child after ShadowRoot promotion"
  );

  // Range boundary offsets on the host must ignore the ShadowRoot pseudo-child.
  let range = doc.create_range();
  doc.range_set_end(range, a, 0).unwrap();
  // With correct tree-child semantics, offset=2 is after the two light DOM children, so setting the
  // start there must collapse the range (end becomes start).
  doc.range_set_start(range, host, 2).unwrap();
  assert_eq!(doc.range_start_container(range).unwrap(), host);
  assert_eq!(doc.range_start_offset(range).unwrap(), 2);
  assert_eq!(doc.range_end_container(range).unwrap(), host);
  assert_eq!(doc.range_end_offset(range).unwrap(), 2);

  // Offsets beyond the light-child count must be rejected. (The ShadowRoot does not contribute.)
  assert!(matches!(
    doc.range_set_start(range, host, 3),
    Err(DomError::IndexSizeError)
  ));

  // `extractContents()` should remove the first light DOM child when selecting [0, 1], and must
  // not treat the ShadowRoot pseudo-child as part of the host's offset space.
  let extract_range = doc.create_range();
  doc.range_set_start(extract_range, host, 0).unwrap();
  doc.range_set_end(extract_range, host, 1).unwrap();

  let fragment = doc.range_extract_contents(extract_range).unwrap();
  assert_eq!(
    doc.node(a).parent,
    Some(fragment),
    "expected extracted node to move into the returned fragment"
  );
  assert_eq!(
    doc.node(host).children.as_slice(),
    &[shadow_root, b],
    "expected host to retain its ShadowRoot pseudo-child at index 0 and keep remaining light children"
  );
}

#[test]
fn range_offsets_ignore_shadow_root_when_shadow_template_is_not_first_child() {
  // Ensure tree-child offset semantics are correct even when the declarative shadow root template is
  // not the first light DOM child (the shadow root is still stored at index 0 in dom2).
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>",
    "<p id=before></p>",
    "<template shadowrootmode=open></template>",
    "<p id=after></p>",
    "</div>",
    "</body></html>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").expect("host node not found");
  let before = doc.get_element_by_id("before").expect("before node not found");
  let after = doc.get_element_by_id("after").expect("after node not found");

  let shadow_root = doc.node(host).children[0];
  assert!(
    matches!(doc.node(shadow_root).kind, NodeKind::ShadowRoot { .. }),
    "expected host to have an attached ShadowRoot"
  );
  assert_eq!(doc.node(host).children[1], before);
  assert_eq!(doc.node(host).children[2], after);

  // Offset=1 refers to the boundary after the first light child (`before`), not after the ShadowRoot.
  let range = doc.create_range();
  doc.range_set_start(range, host, 1).unwrap();
  doc.range_set_end(range, before, 0).unwrap();
  assert_range_collapsed(&doc, range, before, 0);

  // Offset 2 is after both light DOM children; offset 3 must be rejected (ShadowRoot does not count).
  let range = doc.create_range();
  doc.range_set_start(range, host, 2).unwrap();
  assert!(matches!(
    doc.range_set_start(range, host, 3),
    Err(DomError::IndexSizeError)
  ));
}

#[test]
fn live_range_pre_insert_increments_offsets_and_remove_roundtrips() {
  let html = "<!doctype html><html><body><div id=c><span id=a></span><span id=b></span></div></body></html>";
  let mut doc: Document = parse_html(html).unwrap();

  let container = doc.get_element_by_id("c").expect("container not found");
  let a = doc.get_element_by_id("a").expect("a not found");

  // Collapsed range between <span id=a> and <span id=b>.
  let range = doc.create_range();
  doc.range_set_start(range, container, 1).unwrap();
  doc.range_set_end(range, container, 1).unwrap();

  let inserted = doc.create_element("i", "");
  assert!(doc.insert_before(container, inserted, Some(a)).unwrap());

  assert_eq!(doc.range_start_container(range).unwrap(), container);
  assert_eq!(doc.range_end_container(range).unwrap(), container);
  assert_eq!(doc.range_start_offset(range).unwrap(), 2);
  assert_eq!(doc.range_end_offset(range).unwrap(), 2);

  // Removing the inserted node should decrement the offsets back to their original position.
  assert!(doc.remove_child(container, inserted).unwrap());
  assert_eq!(doc.range_start_offset(range).unwrap(), 1);
  assert_eq!(doc.range_end_offset(range).unwrap(), 1);
}

#[test]
fn live_range_replace_data_clamps_and_shifts_offsets() {
  let html = "<!doctype html><html><body><div id=c>abcdef</div></body></html>";
  let mut doc: Document = parse_html(html).unwrap();

  let container = doc.get_element_by_id("c").expect("container not found");
  let text = doc.node(container).children[0];
  assert!(
    matches!(doc.node(text).kind, NodeKind::Text { .. }),
    "expected first child to be a text node"
  );

  let range = doc.create_range();
  doc.range_set_start(range, text, 3).unwrap(); // inside the replaced range
  doc.range_set_end(range, text, 5).unwrap(); // after the replaced range

  // Replace "cd" (offset=2,count=2) with "Z" (inserted_len=1, removed_len=2).
  assert!(doc.replace_data(text, 2, 2, "Z").unwrap());

  // start offset 3 -> clamped to 2, end offset 5 -> shifted by -1 to 4.
  assert_eq!(doc.range_start_container(range).unwrap(), text);
  assert_eq!(doc.range_end_container(range).unwrap(), text);
  assert_eq!(doc.range_start_offset(range).unwrap(), 2);
  assert_eq!(doc.range_end_offset(range).unwrap(), 4);
}

#[test]
fn range_split_text_shifts_parent_boundary_point_immediately_after_split_node() {
  let mut doc: Document =
    parse_html("<!doctype html><div id=host>hello<span id=after></span></div>").unwrap();

  let host = doc.get_element_by_id("host").expect("host node not found");
  let after = doc.get_element_by_id("after").expect("after node not found");
  let text = doc.node(host).children[0];
  assert_eq!(doc.node(host).children[1], after);

  // Boundary point is immediately after the text node, expressed in the parent.
  let range = doc.create_range();
  doc.range_set_start(range, host, 1).unwrap();
  doc.range_set_end(range, host, 1).unwrap();

  let _ = doc.split_text(text, 2).unwrap();

  assert_eq!(doc.range_start_container(range).unwrap(), host);
  assert_eq!(doc.range_start_offset(range).unwrap(), 2);
  assert_eq!(doc.range_end_container(range).unwrap(), host);
  assert_eq!(doc.range_end_offset(range).unwrap(), 2);
}

#[test]
fn range_split_text_moves_boundary_points_from_old_text_to_new_text() {
  let mut doc: Document = parse_html("<!doctype html><div id=host>hello</div>").unwrap();

  let host = doc.get_element_by_id("host").expect("host node not found");
  let text = doc.node(host).children[0];
  assert!(matches!(doc.node(text).kind, NodeKind::Text { .. }));

  let range = doc.create_range();
  doc.range_set_start(range, text, 1).unwrap();
  doc.range_set_end(range, text, 4).unwrap();

  let new_text = doc.split_text(text, 2).unwrap();

  assert_eq!(doc.text_data(text).unwrap(), "he");
  assert_eq!(doc.text_data(new_text).unwrap(), "llo");

  assert_eq!(doc.range_start_container(range).unwrap(), text);
  assert_eq!(doc.range_start_offset(range).unwrap(), 1);
  assert_eq!(doc.range_end_container(range).unwrap(), new_text);
  assert_eq!(doc.range_end_offset(range).unwrap(), 2);
}

#[test]
fn range_split_text_parent_offsets_ignore_shadow_root_pseudo_child() {
  let mut doc: Document = parse_html(concat!(
    "<!doctype html>",
    "<div id=host>",
    "<template shadowrootmode=open></template>",
    "hello",
    "<span id=after></span>",
    "</div>",
  ))
  .unwrap();

  let host = doc.get_element_by_id("host").expect("host node not found");
  let after = doc.get_element_by_id("after").expect("after node not found");

  let shadow_root = doc.node(host).children[0];
  assert!(
    matches!(doc.node(shadow_root).kind, NodeKind::ShadowRoot { .. }),
    "expected host to have an attached ShadowRoot pseudo-child"
  );

  let text = doc.node(host).children[1];
  assert!(matches!(doc.node(text).kind, NodeKind::Text { .. }));
  assert_eq!(
    doc.node(host).children[2],
    after,
    "expected the light DOM <span> to follow the text node"
  );

  // Boundary point is immediately after the text node, expressed in the parent in *tree child*
  // index space. With an attached ShadowRoot pseudo-child stored at raw index 0, this must still
  // be offset 1.
  let range = doc.create_range();
  doc.range_set_start(range, host, 1).unwrap();
  doc.range_set_end(range, host, 1).unwrap();

  let _ = doc.split_text(text, 2).unwrap();

  // The split inserts a new text node immediately after the original, so a boundary point at
  // (host, 1) must shift to (host, 2). This must ignore the ShadowRoot pseudo-child.
  assert_eq!(doc.range_start_container(range).unwrap(), host);
  assert_eq!(doc.range_start_offset(range).unwrap(), 2);
  assert_eq!(doc.range_end_container(range).unwrap(), host);
  assert_eq!(doc.range_end_offset(range).unwrap(), 2);
}

#[test]
fn range_compare_boundary_points_returns_not_supported_error_before_root_check() {
  let mut doc: Document =
    parse_html("<!doctype html><html><body><p id=a>abcd</p></body></html>").unwrap();

  let p = doc.get_element_by_id("a").expect("missing <p id=a>");
  let text = doc.node(p).children[0];

  let in_doc = doc.create_range();
  doc.range_set_start(in_doc, text, 0).unwrap();
  doc.range_set_end(in_doc, text, 1).unwrap();

  let detached_div = doc.create_element("div", HTML_NAMESPACE);
  let detached_text = doc.create_text("x");
  doc.append_child(detached_div, detached_text).unwrap();

  let in_detached = doc.create_range();
  doc.range_set_start(in_detached, detached_text, 0).unwrap();
  doc.range_set_end(in_detached, detached_text, 1).unwrap();

  // `how=4` is invalid; spec requires NotSupportedError before checking roots.
  let err = doc
    .range_compare_boundary_points(in_doc, 4, in_detached)
    .unwrap_err();
  assert_eq!(err, DomError::NotSupportedError);
}

#[test]
fn range_compare_boundary_points_returns_wrong_document_error_for_different_roots() {
  let mut doc: Document =
    parse_html("<!doctype html><html><body><p id=a>abcd</p></body></html>").unwrap();

  let p = doc.get_element_by_id("a").expect("missing <p id=a>");
  let text = doc.node(p).children[0];

  let in_doc = doc.create_range();
  doc.range_set_start(in_doc, text, 0).unwrap();
  doc.range_set_end(in_doc, text, 1).unwrap();

  let detached_div = doc.create_element("div", HTML_NAMESPACE);
  let detached_text = doc.create_text("x");
  doc.append_child(detached_div, detached_text).unwrap();

  let in_detached = doc.create_range();
  doc.range_set_start(in_detached, detached_text, 0).unwrap();
  doc.range_set_end(in_detached, detached_text, 1).unwrap();

  let err = doc
    .range_compare_boundary_points(in_doc, 0, in_detached)
    .unwrap_err();
  assert_eq!(err, DomError::WrongDocumentError);
}

#[test]
fn range_compare_boundary_points_returns_wrong_document_error_between_light_dom_and_shadow_tree() {
  let html = concat!(
    "<!doctype html>",
    "<div id=host>",
    "<template shadowrootmode=open><span id=inside></span></template>",
    "<p id=light></p>",
    "</div>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").expect("host node not found");
  let shadow_root = doc.node(host).children[0];
  assert!(
    matches!(doc.node(shadow_root).kind, NodeKind::ShadowRoot { .. }),
    "expected host to have an attached ShadowRoot"
  );
  let inside = doc.node(shadow_root).children[0];

  let in_light_dom = doc.create_range();
  doc.range_set_start(in_light_dom, host, 0).unwrap();
  doc.range_set_end(in_light_dom, host, 0).unwrap();

  let in_shadow = doc.create_range();
  doc.range_set_start(in_shadow, inside, 0).unwrap();
  doc.range_set_end(in_shadow, inside, 0).unwrap();

  let err = doc
    .range_compare_boundary_points(in_light_dom, 0, in_shadow)
    .unwrap_err();
  assert_eq!(
    err,
    DomError::WrongDocumentError,
    "ShadowRoot is the root of a separate tree for Range algorithms"
  );
}

#[test]
fn range_compare_boundary_points_orders_boundary_points() {
  let mut doc: Document =
    parse_html("<!doctype html><html><body><p id=a>abcd</p></body></html>").unwrap();

  let p = doc.get_element_by_id("a").expect("missing <p id=a>");
  let text = doc.node(p).children[0];

  let a = doc.create_range();
  doc.range_set_start(a, text, 0).unwrap();
  doc.range_set_end(a, text, 0).unwrap();

  let b = doc.create_range();
  doc.range_set_start(b, text, 1).unwrap();
  doc.range_set_end(b, text, 1).unwrap();

  assert_eq!(doc.range_compare_boundary_points(a, 0, b).unwrap(), -1);
  assert_eq!(doc.range_compare_boundary_points(b, 0, a).unwrap(), 1);
  assert_eq!(doc.range_compare_boundary_points(a, 0, a).unwrap(), 0);
}

#[test]
fn range_offsets_do_not_shift_when_shadow_root_attached_or_detached() {
  let mut doc: Document = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();

  let host = doc.create_element("div", "");
  doc.append_child(root, host).unwrap();

  let a = doc.create_element("span", "");
  let b = doc.create_element("span", "");
  doc.append_child(host, a).unwrap();
  doc.append_child(host, b).unwrap();

  // Collapsed range between `a` and `b` in tree-child offset space.
  let range = doc.create_range();
  doc.range_set_start(range, host, 1).unwrap();
  doc.range_set_end(range, host, 1).unwrap();

  // Attaching a shadow root inserts a ShadowRoot node as child index 0 in `dom2`, but that is not a
  // light-DOM tree child and must not affect Range offsets.
  let shadow_root = doc
    .attach_shadow_root(
      host,
      ShadowRootMode::Open,
      /* clonable */ false,
      /* serializable */ false,
      /* delegates_focus */ false,
      SlotAssignmentMode::Named,
    )
    .unwrap();
  assert!(matches!(doc.node(shadow_root).kind, NodeKind::ShadowRoot { .. }));
  assert_eq!(doc.node(host).children[0], shadow_root);
  assert_eq!(doc.node(host).children[1], a);
  assert_eq!(doc.node(host).children[2], b);

  assert_eq!(doc.range_start_container(range).unwrap(), host);
  assert_eq!(doc.range_start_offset(range).unwrap(), 1);
  assert_eq!(doc.range_end_container(range).unwrap(), host);
  assert_eq!(doc.range_end_offset(range).unwrap(), 1);

  // Offsets into the host must continue to be validated against the light-DOM child count.
  let check = doc.create_range();
  doc.range_set_start(check, host, 2).unwrap();
  assert_eq!(
    doc.range_set_start(check, host, 3),
    Err(DomError::IndexSizeError)
  );

  // Boundary point comparisons must use tree-child indices (ignoring the ShadowRoot pseudo-child).
  let collapse = doc.create_range();
  doc.range_set_start(collapse, host, 1).unwrap(); // after `a`
  doc.range_set_end(collapse, a, 0).unwrap(); // before `a` => collapse
  assert_range_collapsed(&doc, collapse, a, 0);

  // Removing the attached ShadowRoot from the host's child list must also not shift existing ranges.
  assert!(doc.remove_child(host, shadow_root).unwrap());
  assert_eq!(doc.node(host).children.as_slice(), &[a, b]);
  assert_eq!(doc.range_start_container(range).unwrap(), host);
  assert_eq!(doc.range_start_offset(range).unwrap(), 1);
  assert_eq!(doc.range_end_container(range).unwrap(), host);
  assert_eq!(doc.range_end_offset(range).unwrap(), 1);
}

#[test]
fn range_remap_node_ids_updates_boundary_points_for_cross_document_adopted_detached_subtree() {
  let mut src = Document::new(QuirksMode::NoQuirks);

  // Create a detached subtree and a Range anchored inside it.
  let detached_root = src.create_element("div", "");
  let text = src.create_text("hello");
  src.append_child(detached_root, text).unwrap();

  let range = src.create_range();
  src.range_set_start(range, text, 1).unwrap();
  src.range_set_end(range, text, 4).unwrap();

  // Adopt the subtree into a new document. `dom2` approximates adoption via clone+mapping, which
  // changes `NodeId` values.
  let mut dest = Document::new(QuirksMode::NoQuirks);
  // Ensure the adopted subtree receives fresh node ids that differ from the source document's ids.
  let _preexisting = dest.create_element("preexisting", "");
  let adopted = dest.adopt_node_from(&mut src, detached_root).unwrap();
  let mapping: HashMap<_, _> = adopted.mapping.into_iter().collect();
  let new_text = *mapping.get(&text).expect("expected text node to be remapped");
  assert_ne!(
    new_text, text,
    "expected cross-document adoption to allocate new NodeIds"
  );

  // Remap Range endpoints using the same old→new mapping used by wrapper identity remapping.
  src.range_remap_node_ids(&mapping);

  assert_eq!(src.range_start_container(range).unwrap(), new_text);
  assert_eq!(src.range_start_offset(range).unwrap(), 1);
  assert_eq!(src.range_end_container(range).unwrap(), new_text);
  assert_eq!(src.range_end_offset(range).unwrap(), 4);

  assert!(new_text.index() < dest.nodes_len());
  assert!(
    matches!(dest.node(new_text).kind, NodeKind::Text { .. }),
    "remapped endpoint should refer to the cloned text node in the destination document"
  );
}
#[test]
fn range_to_string_matches_dom_stringifier_expectations() {
  // Mirrors `tests/wpt_dom/tests/dom/ranges/Range-stringifier.html`.
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=test>Test div</div>\n",
    "<div id=another>Another div</div>\n",
    "<div id=last>Last div</div>",
    "</body></html>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let test_div = doc.get_element_by_id("test").unwrap();
  let another_div = doc.get_element_by_id("another").unwrap();
  let last_div = doc.get_element_by_id("last").unwrap();

  let text_node = doc.node(test_div).children[0];
  let last_text = doc.node(last_div).children[0];

  let range = doc.create_range();

  // Equivalent to `Range.selectNodeContents(testDiv)`.
  let end_offset = doc.node_length(test_div).unwrap();
  doc.range_set_start(range, test_div, 0).unwrap();
  doc.range_set_end(range, test_div, end_offset).unwrap();
  assert_eq!(doc.range_to_string(range).unwrap(), "Test div");

  doc.range_set_start(range, text_node, 5).unwrap();
  doc.range_set_end(range, text_node, 7).unwrap();
  assert_eq!(doc.range_to_string(range).unwrap(), "di");

  doc.range_set_start(range, test_div, 0).unwrap();
  doc.range_set_end(range, another_div, 0).unwrap();
  assert_eq!(doc.range_to_string(range).unwrap(), "Test div\n");

  doc.range_set_start(range, text_node, 5).unwrap();
  doc.range_set_end(range, last_text, 4).unwrap();
  assert_eq!(doc.range_to_string(range).unwrap(), "div\nAnother div\nLast");
}

#[test]
fn range_delete_contents_character_data_respects_utf16_offsets_and_collapses() {
  let mut doc: Document = parse_html("<!doctype html><html></html>").unwrap();

  // 😀 is a single Unicode scalar value but encoded as a surrogate pair in UTF-16.
  let text = doc.create_text("x😀y");
  let range = doc.create_range();
  doc.range_set_start(range, text, 1).unwrap();
  doc.range_set_end(range, text, 3).unwrap();

  doc.range_delete_contents(range).unwrap();

  assert_eq!(doc.text_data(text).unwrap(), "xy");
  assert_range_collapsed(&doc, range, text, 1);
}

#[test]
fn range_delete_contents_across_nodes_removes_contained_nodes_and_collapses_to_parent() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>hello<span id=mid>mid</span>world</div>",
    "</body></html>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").unwrap();
  let mid = doc.get_element_by_id("mid").unwrap();
  let start_text = doc.node(host).children[0];
  let end_text = doc.node(host).children[2];
  assert!(matches!(doc.node(start_text).kind, NodeKind::Text { .. }));
  assert!(matches!(doc.node(end_text).kind, NodeKind::Text { .. }));

  let range = doc.create_range();
  doc.range_set_start(range, start_text, 2).unwrap(); // he|llo
  doc.range_set_end(range, end_text, 3).unwrap(); // wor|ld

  doc.range_delete_contents(range).unwrap();

  assert_eq!(doc.text_data(start_text).unwrap(), "he");
  assert_eq!(doc.text_data(end_text).unwrap(), "ld");
  assert!(doc.node(mid).parent.is_none(), "expected contained <span> to be removed");
  assert_eq!(super::serialization::serialize_children(&doc, host), "held");

  // The deleteContents collapse point is the boundary point after the original start node in its
  // parent.
  assert_range_collapsed(&doc, range, host, 1);
}

#[test]
fn range_insert_node_splits_text_and_updates_end_when_collapsed() {
  let html = "<!doctype html><html><body><div id=host>hello</div></body></html>";
  let mut doc: Document = parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").unwrap();
  let text = doc.node(host).children[0];
  assert!(matches!(doc.node(text).kind, NodeKind::Text { .. }));

  let range = doc.create_range();
  doc.range_set_start(range, text, 2).unwrap();
  doc.range_set_end(range, text, 2).unwrap();

  let inserted = doc.create_element("span", "");
  doc.range_insert_node(range, inserted).unwrap();

  assert_eq!(
    super::serialization::serialize_children(&doc, host),
    "he<span></span>llo"
  );

  assert_eq!(doc.range_start_container(range).unwrap(), text);
  assert_eq!(doc.range_start_offset(range).unwrap(), 2);
  assert_eq!(doc.range_end_container(range).unwrap(), host);
  assert_eq!(doc.range_end_offset(range).unwrap(), 2);
}

#[test]
fn range_insert_node_document_fragment_moves_children_and_updates_end_offset() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host><span id=a></span><span id=b></span></div>",
    "</body></html>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").unwrap();
  let a = doc.get_element_by_id("a").unwrap();
  let b = doc.get_element_by_id("b").unwrap();

  // Insert at the boundary between #a and #b (tree-child offset 1).
  let range = doc.create_range();
  doc.range_set_start(range, host, 1).unwrap();
  doc.range_set_end(range, host, 1).unwrap();

  let frag = doc.create_document_fragment();
  let x = doc.create_element("i", "");
  let y = doc.create_element("i", "");
  assert!(doc.append_child(frag, x).unwrap());
  assert!(doc.append_child(frag, y).unwrap());

  doc.range_insert_node(range, frag).unwrap();

  assert!(
    doc.node(frag).children.is_empty(),
    "expected DocumentFragment to be emptied after insertion"
  );

  let children = doc.node(host).children.clone();
  assert_eq!(children, vec![a, x, y, b]);

  assert_eq!(doc.range_start_container(range).unwrap(), host);
  assert_eq!(doc.range_start_offset(range).unwrap(), 1);
  assert_eq!(doc.range_end_container(range).unwrap(), host);
  assert_eq!(doc.range_end_offset(range).unwrap(), 3);
}

#[test]
fn range_surround_contents_wraps_and_selects_new_parent() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host><span id=a></span><span id=b></span></div>",
    "</body></html>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").unwrap();
  let a = doc.get_element_by_id("a").unwrap();
  let b = doc.get_element_by_id("b").unwrap();

  let range = doc.create_range();
  doc.range_set_start(range, host, 0).unwrap();
  doc.range_set_end(range, host, 2).unwrap();

  let wrapper = doc.create_element("em", "");
  doc.range_surround_contents(range, wrapper).unwrap();

  assert_eq!(
    super::serialization::serialize_children(&doc, host),
    "<em><span id=\"a\"></span><span id=\"b\"></span></em>"
  );
  assert_eq!(doc.node(wrapper).parent, Some(host));
  assert_eq!(doc.node(a).parent, Some(wrapper));
  assert_eq!(doc.node(b).parent, Some(wrapper));

  // The final step selects the wrapper node.
  assert_eq!(doc.range_start_container(range).unwrap(), host);
  assert_eq!(doc.range_start_offset(range).unwrap(), 0);
  assert_eq!(doc.range_end_container(range).unwrap(), host);
  assert_eq!(doc.range_end_offset(range).unwrap(), 1);
}

#[test]
fn range_surround_contents_throws_for_partially_contained_non_text_node() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host><p id=p><b id=b>hello</b></p><span id=s>world</span></div>",
    "</body></html>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let b = doc.get_element_by_id("b").unwrap();
  let s = doc.get_element_by_id("s").unwrap();
  let b_text = doc.node(b).children[0];
  let s_text = doc.node(s).children[0];

  let range = doc.create_range();
  doc.range_set_start(range, b_text, 1).unwrap();
  doc.range_set_end(range, s_text, 1).unwrap();

  let wrapper = doc.create_element("em", "");
  let err = doc.range_surround_contents(range, wrapper).unwrap_err();
  assert_eq!(err, DomError::InvalidStateError);
}

#[test]
fn live_range_replace_data_deletion_clamps_and_shifts_offsets() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let t = doc.create_text("abcdef"); // UTF-16 length = 6

  // Delete 2 code units starting at offset 2 ("cd").
  let offset = 2usize;
  let count = 2usize;

  // Range whose start/end offsets are inside the deleted segment (including the end boundary).
  let r_inside = doc.create_range();
  doc.range_set_start(r_inside, t, 3).unwrap();
  doc.range_set_end(r_inside, t, 4).unwrap();

  // Range whose start is exactly at the deletion boundary, and end is at the segment end.
  let r_boundary = doc.create_range();
  doc.range_set_start(r_boundary, t, 2).unwrap();
  doc.range_set_end(r_boundary, t, 4).unwrap();

  // Range entirely after the deleted segment should shift left by `count`.
  let r_after = doc.create_range();
  doc.range_set_start(r_after, t, 5).unwrap();
  doc.range_set_end(r_after, t, 6).unwrap();

  assert!(doc.replace_data(t, offset, count, "").unwrap());
  assert_eq!(doc.text_data(t).unwrap(), "abef");

  assert_eq!(doc.range_start_container(r_inside).unwrap(), t);
  assert_eq!(doc.range_end_container(r_inside).unwrap(), t);
  assert_eq!(doc.range_start_offset(r_inside).unwrap(), offset);
  assert_eq!(doc.range_end_offset(r_inside).unwrap(), offset);

  assert_eq!(doc.range_start_offset(r_boundary).unwrap(), offset);
  assert_eq!(doc.range_end_offset(r_boundary).unwrap(), offset);

  assert_eq!(doc.range_start_offset(r_after).unwrap(), 3);
  assert_eq!(doc.range_end_offset(r_after).unwrap(), 4);
}

#[test]
fn live_range_replace_data_insertion_shifts_by_utf16_code_units() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let t = doc.create_text("abcd"); // UTF-16 length = 4

  let r = doc.create_range();
  doc.range_set_start(r, t, 4).unwrap();
  doc.range_set_end(r, t, 4).unwrap();

  // Insert a single emoji (UTF-16 length 2) at offset 2.
  assert!(doc.replace_data(t, 2, 0, "😀").unwrap());
  assert_eq!(doc.text_data(t).unwrap(), "ab😀cd");

  assert_eq!(doc.range_start_container(r).unwrap(), t);
  assert_eq!(doc.range_end_container(r).unwrap(), t);
  // End of text shifts by +2 code units.
  assert_eq!(doc.range_start_offset(r).unwrap(), 6);
  assert_eq!(doc.range_end_offset(r).unwrap(), 6);
}

#[test]
fn live_range_insert_steps_updates_collapsed_range_offsets() {
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

  let range = doc.create_range();
  doc.range_set_start(range, parent, 2).unwrap();
  doc.range_set_end(range, parent, 2).unwrap();

  // Insert at index 1 (before `b`); the collapsed boundary point at offset 2 should shift right.
  let inserted = doc.create_element("x", "");
  assert!(doc.insert_before(parent, inserted, Some(b)).unwrap());

  assert_eq!(doc.range_start_offset(range).unwrap(), 3);
  assert_eq!(doc.range_end_offset(range).unwrap(), 3);
}

#[test]
fn live_range_insert_steps_updates_range_offsets_span() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let parent = doc.create_element("div", "");
  doc.append_child(root, parent).unwrap();

  let a = doc.create_element("a", "");
  let b = doc.create_element("b", "");
  doc.append_child(parent, a).unwrap();
  doc.append_child(parent, b).unwrap();

  let range = doc.create_range();
  doc.range_set_start(range, parent, 1).unwrap();
  doc.range_set_end(range, parent, 2).unwrap();

  // Insert at index 0 (before `a`); both endpoints should shift right.
  let inserted = doc.create_element("x", "");
  assert!(doc.insert_before(parent, inserted, Some(a)).unwrap());

  assert_eq!(doc.range_start_offset(range).unwrap(), 2);
  assert_eq!(doc.range_end_offset(range).unwrap(), 3);
}

#[test]
fn range_delete_contents_character_data_collapses_to_start() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let text = doc.create_text("abcdef");

  let range = doc.create_range();
  doc.range_set_start(range, text, 2).unwrap();
  doc.range_set_end(range, text, 5).unwrap();

  doc.range_delete_contents(range).unwrap();

  assert_eq!(doc.text_data(text).unwrap(), "abf");
  assert_range_collapsed(&doc, range, text, 2);
}

#[test]
fn range_extract_contents_character_data_collapses_to_start() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let text = doc.create_text("abcdef");

  let range = doc.create_range();
  doc.range_set_start(range, text, 2).unwrap();
  doc.range_set_end(range, text, 5).unwrap();

  let fragment = doc.range_extract_contents(range).unwrap();

  assert_eq!(doc.text_data(text).unwrap(), "abf");
  assert_range_collapsed(&doc, range, text, 2);

  // The extracted DocumentFragment should contain a single Text child with the extracted substring.
  assert!(matches!(doc.node(fragment).kind, NodeKind::DocumentFragment));
  let child = doc.node(fragment).children[0];
  assert_eq!(doc.text_data(child).unwrap(), "cde");
}

#[test]
fn range_delete_contents_detaches_nodes_removed_from_partially_contained_elements() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.create_element("div", "");
  doc.append_child(doc.root(), root).unwrap();

  // <div><b><i>hello</i><span>mid</span></b><u>world</u></div>
  let b = doc.create_element("b", "");
  doc.append_child(root, b).unwrap();
  let i = doc.create_element("i", "");
  doc.append_child(b, i).unwrap();
  let i_text = doc.create_text("hello");
  doc.append_child(i, i_text).unwrap();

  let span = doc.create_element("span", "");
  doc.append_child(b, span).unwrap();
  let span_text = doc.create_text("mid");
  doc.append_child(span, span_text).unwrap();

  let u = doc.create_element("u", "");
  doc.append_child(root, u).unwrap();
  let u_text = doc.create_text("world");
  doc.append_child(u, u_text).unwrap();

  let range = doc.create_range();
  doc.range_set_start(range, i_text, 1).unwrap(); // inside "hello"
  doc.range_set_end(range, u_text, 3).unwrap(); // inside "world"

  doc.range_delete_contents(range).unwrap();

  // The <span> node was fully contained in the range and should be removed from the document, with
  // `parent == None` (deleteContents must not keep removed nodes parented under an internal
  // DocumentFragment/cloned wrapper).
  assert!(doc.parent(span).unwrap().is_none());

  // Sanity: boundary text nodes should be updated.
  assert_eq!(doc.text_data(i_text).unwrap(), "h");
  assert_eq!(doc.text_data(u_text).unwrap(), "ld");

  // The range collapses to the computed collapse point (between <b> and <u>).
  assert_range_collapsed(&doc, range, root, 1);
}

#[test]
fn range_point_methods_basic_positions() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>",
    "<span id=a></span>",
    "<span id=b></span>",
    "<span id=c></span>",
    "</div>",
    "</body></html>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").expect("host node not found");
  let a = doc.get_element_by_id("a").expect("a node not found");
  let b = doc.get_element_by_id("b").expect("b node not found");
  let c = doc.get_element_by_id("c").expect("c node not found");

  // Select the middle <span id=b> node.
  let range = doc.create_range();
  doc.range_set_start(range, host, 1).unwrap();
  doc.range_set_end(range, host, 2).unwrap();

  assert_eq!(doc.range_compare_point(range, host, 0).unwrap(), -1);
  assert_eq!(doc.range_compare_point(range, a, 0).unwrap(), -1);
  assert_eq!(doc.range_compare_point(range, host, 1).unwrap(), 0);
  assert_eq!(doc.range_compare_point(range, b, 0).unwrap(), 0);
  assert_eq!(doc.range_compare_point(range, c, 0).unwrap(), 1);
  assert_eq!(doc.range_compare_point(range, host, 3).unwrap(), 1);

  assert!(!doc.range_is_point_in_range(range, host, 0).unwrap());
  assert!(doc.range_is_point_in_range(range, host, 1).unwrap());
  assert!(doc.range_is_point_in_range(range, b, 0).unwrap());
  assert!(doc.range_is_point_in_range(range, host, 2).unwrap());
  assert!(!doc.range_is_point_in_range(range, c, 0).unwrap());
  assert!(!doc.range_is_point_in_range(range, host, 3).unwrap());

  assert!(!doc.range_intersects_node(range, a).unwrap());
  assert!(doc.range_intersects_node(range, b).unwrap());
  assert!(!doc.range_intersects_node(range, c).unwrap());

  // Ancestors should intersect if they contain any of the range.
  assert!(doc.range_intersects_node(range, host).unwrap());
  assert!(doc.range_intersects_node(range, doc.root()).unwrap());
}

#[test]
fn range_point_methods_use_utf16_offsets_for_character_data() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>a😃b</div>",
    "</body></html>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let host = doc.get_element_by_id("host").expect("host node not found");
  let text = doc.first_child(host).expect("expected text child");

  // "a😃b" has UTF-16 length 4: "a" (1) + "😃" (2) + "b" (1).
  let range = doc.create_range();
  doc.range_set_start(range, text, 1).unwrap();
  doc.range_set_end(range, text, 3).unwrap();

  assert_eq!(doc.range_compare_point(range, text, 0).unwrap(), -1);
  assert_eq!(doc.range_compare_point(range, text, 2).unwrap(), 0); // inside surrogate pair
  assert_eq!(doc.range_compare_point(range, text, 4).unwrap(), 1);

  assert!(doc.range_is_point_in_range(range, text, 2).unwrap());
  assert!(!doc.range_is_point_in_range(range, text, 4).unwrap());

  assert!(matches!(
    doc.range_compare_point(range, text, 5),
    Err(DomError::IndexSizeError)
  ));
  assert!(matches!(
    doc.range_is_point_in_range(range, text, 5),
    Err(DomError::IndexSizeError)
  ));
}

#[test]
fn range_point_methods_handle_disconnected_and_other_roots() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>",
    "<template shadowrootmode=open><span id=inside></span></template>",
    "</div>",
    "<p id=light></p>",
    "</body></html>",
  );
  let mut doc: Document = parse_html(html).unwrap();

  let light = doc.get_element_by_id("light").expect("light node not found");
  let host = doc.get_element_by_id("host").expect("host node not found");
  let shadow_root = doc.node(host).children[0];
  let inside = doc.node(shadow_root).children[0];

  assert!(
    matches!(doc.node(shadow_root).kind, NodeKind::ShadowRoot { .. }),
    "expected host to have an attached ShadowRoot"
  );

  let range = doc.create_range();
  doc.range_set_start(range, light, 0).unwrap();
  doc.range_set_end(range, light, 0).unwrap();

  // Node inside a different tree root (shadow tree) should be treated as out-of-range.
  assert!(!doc.range_is_point_in_range(range, inside, 0).unwrap());
  assert!(matches!(
    doc.range_compare_point(range, inside, 0),
    Err(DomError::WrongDocumentError)
  ));
  assert!(!doc.range_intersects_node(range, inside).unwrap());

  // Disconnected nodes behave similarly: comparePoint throws, others return false.
  let detached = doc.create_element("div", "");
  assert!(!doc.range_is_point_in_range(range, detached, 999).unwrap());
  assert!(matches!(
    doc.range_compare_point(range, detached, 999),
    Err(DomError::WrongDocumentError)
  ));
  assert!(!doc.range_intersects_node(range, detached).unwrap());

  // Doctype is a valid node in the same root but is not a valid boundary point.
  let doctype = doc.doctype().expect("doctype node not found");
  assert!(matches!(
    doc.range_is_point_in_range(range, doctype, 0),
    Err(DomError::InvalidNodeTypeError)
  ));
  assert!(matches!(
    doc.range_compare_point(range, doctype, 0),
    Err(DomError::InvalidNodeTypeError)
  ));
}
