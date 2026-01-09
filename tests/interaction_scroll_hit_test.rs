use fastrender::dom::{enumerate_dom_ids, DomNode};
use fastrender::interaction::{hit_test_dom_viewport_point, HitTestKind};
use fastrender::scroll::ScrollState;
use fastrender::{BoxNode, BoxTree, FastRender, FragmentNode, FragmentTree, Point, Rect, RenderOptions};
use std::collections::HashMap;

fn find_dom_ptr_by_id(root: &DomNode, id: &str) -> Option<*const DomNode> {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(id) {
      return Some(node as *const DomNode);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn find_box_id_for_styled_node(box_tree: &BoxTree, styled_node_id: usize) -> Option<usize> {
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.styled_node_id == Some(styled_node_id) {
      return Some(node.id);
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn find_fragment_rect_for_box_id(tree: &FragmentTree, box_id: usize) -> Option<Rect> {
  fn visit(node: &FragmentNode, parent_origin: Point, box_id: usize) -> Option<Rect> {
    let origin = parent_origin.translate(node.bounds.origin);
    if node.box_id() == Some(box_id) {
      return Some(Rect::new(origin, node.bounds.size));
    }
    for child in node.children.iter() {
      if let Some(found) = visit(child, origin, box_id) {
        return Some(found);
      }
    }
    None
  }

  if let Some(found) = visit(&tree.root, Point::ZERO, box_id) {
    return Some(found);
  }
  for root in &tree.additional_fragments {
    if let Some(found) = visit(root, Point::ZERO, box_id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn hit_test_dom_accounts_for_viewport_and_element_scroll() {
  let html = r#"<!DOCTYPE html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #spacer { height: 200px; }
      #scroller { overflow: scroll; height: 50px; width: 200px; }
      #inner { height: 200px; }
      #target { display: block; height: 20px; }
    </style>
  </head>
  <body>
    <div id="spacer"></div>
    <div id="scroller">
      <div id="inner"></div>
      <a id="target" href="/ok">Target</a>
    </div>
  </body>
</html>"#;

  let mut renderer = FastRender::new().expect("create renderer");
  let prepared = renderer
    .prepare_html(html, RenderOptions::new().with_viewport(250, 120))
    .expect("prepare");

  let dom_ids = enumerate_dom_ids(prepared.dom());
  let scroller_ptr = find_dom_ptr_by_id(prepared.dom(), "scroller").expect("find scroller node");
  let target_ptr = find_dom_ptr_by_id(prepared.dom(), "target").expect("find target node");

  let scroller_dom_id = *dom_ids.get(&scroller_ptr).expect("scroller node id");
  let target_dom_id = *dom_ids.get(&target_ptr).expect("target node id");

  let scroller_box_id =
    find_box_id_for_styled_node(prepared.box_tree(), scroller_dom_id).expect("scroller box id");
  let target_box_id =
    find_box_id_for_styled_node(prepared.box_tree(), target_dom_id).expect("target box id");

  let scroller_rect =
    find_fragment_rect_for_box_id(prepared.fragment_tree(), scroller_box_id).expect("scroller rect");
  let target_rect =
    find_fragment_rect_for_box_id(prepared.fragment_tree(), target_box_id).expect("target rect");

  let desired_y_in_scroller = 10.0;
  let target_rel_y = target_rect.y() - scroller_rect.y();
  let element_scroll_y = (target_rel_y - desired_y_in_scroller).max(0.0);

  let viewport_scroll_y = (scroller_rect.y() - 5.0).max(0.0);

  assert!(
    element_scroll_y > 0.0,
    "test requires non-zero element scroll; got {element_scroll_y}"
  );
  assert!(
    viewport_scroll_y > 0.0,
    "test requires non-zero viewport scroll; got {viewport_scroll_y}"
  );

  let mut elements = HashMap::new();
  elements.insert(scroller_box_id, Point::new(0.0, element_scroll_y));
  let scroll = ScrollState::from_parts(Point::new(0.0, viewport_scroll_y), elements);

  let rendered_target_rect = target_rect.translate(Point::new(0.0, -element_scroll_y));
  let viewport_point = Point::new(
    rendered_target_rect.x() + 1.0,
    rendered_target_rect.y() + 1.0 - viewport_scroll_y,
  );

  let result = hit_test_dom_viewport_point(&prepared, &scroll, viewport_point)
    .expect("expected a hit result");
  assert_eq!(result.kind, HitTestKind::Link);
  assert_eq!(result.href.as_deref(), Some("/ok"));
}

