use fastrender::api::{FastRender, RenderOptions};
use fastrender::geometry::Point;
use fastrender::scroll::ScrollState;
use fastrender::tree::box_tree::BoxNode;
use fastrender::FragmentNode;

fn box_id_by_element_id(node: &BoxNode, target_id: &str) -> Option<usize> {
  if let Some(debug) = node.debug_info.as_ref() {
    if debug.id.as_deref() == Some(target_id) {
      return Some(node.id);
    }
  }

  node
    .children
    .iter()
    .find_map(|child| box_id_by_element_id(child, target_id))
}

fn find_fragment_origin_by_box_id<'a>(
  node: &'a FragmentNode,
  origin: Point,
  target_id: usize,
) -> Option<(Point, &'a FragmentNode)> {
  let abs_origin = Point::new(origin.x + node.bounds.x(), origin.y + node.bounds.y());
  if node.box_id() == Some(target_id) {
    return Some((abs_origin, node));
  }

  for child in node.children.iter() {
    if let Some(found) = find_fragment_origin_by_box_id(child, abs_origin, target_id) {
      return Some(found);
    }
  }

  None
}

#[test]
fn sticky_top_does_not_shift_when_border_box_within_view_rect() {
  let html = r#"
    <style>
      html, body { margin: 0; padding: 0; }
      #spacer { height: 5px; }
      #sticky {
        position: sticky;
        top: 15px;
        margin-top: 40px;
        height: 20px;
        background: rgb(255, 0, 0);
      }
    </style>
    <div id="spacer"></div>
    <div id="sticky"></div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(200, 200);
  let prepared = renderer.prepare_html(html, options).expect("prepare html");

  let sticky_id = box_id_by_element_id(&prepared.box_tree().root, "sticky").expect("sticky box id");

  let mut tree = prepared.fragment_tree().clone();
  let (before_origin, _) =
    find_fragment_origin_by_box_id(&tree.root, Point::ZERO, sticky_id).expect("sticky fragment");

  renderer.apply_sticky_offsets_to_tree_with_scroll_state(&mut tree, &ScrollState::default());
  let (after_origin, sticky_fragment) =
    find_fragment_origin_by_box_id(&tree.root, Point::ZERO, sticky_id).expect("sticky fragment");

  assert!(
    (after_origin.y - before_origin.y).abs() < 0.1,
    "sticky should not shift at scroll=0 when its border box is already within the sticky view rectangle; before_y={:.2} after_y={:.2} bounds={:?}",
    before_origin.y,
    after_origin.y,
    sticky_fragment.bounds
  );
}

#[test]
fn sticky_clamp_does_not_reduce_margins_that_are_within_containing_block() {
  let html = r#"
    <style>
      html, body { margin: 0; padding: 0; }
      #container { height: 100px; }
      #pre { height: 40px; }
      #sticky {
        position: sticky;
        top: 0;
        height: 20px;
        margin-bottom: 40px;
        background: rgb(255, 0, 0);
      }
    </style>
    <div id="container">
      <div id="pre"></div>
      <div id="sticky"></div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(200, 200);
  let prepared = renderer.prepare_html(html, options).expect("prepare html");

  let sticky_id = box_id_by_element_id(&prepared.box_tree().root, "sticky").expect("sticky box id");

  let mut tree = prepared.fragment_tree().clone();
  let (before_origin, _) =
    find_fragment_origin_by_box_id(&tree.root, Point::ZERO, sticky_id).expect("sticky fragment");

  let scroll_state = ScrollState::with_viewport(Point::new(0.0, 50.0));
  renderer.apply_sticky_offsets_to_tree_with_scroll_state(&mut tree, &scroll_state);
  let (after_origin, sticky_fragment) =
    find_fragment_origin_by_box_id(&tree.root, Point::ZERO, sticky_id).expect("sticky fragment");

  assert!(
    (after_origin.y - before_origin.y).abs() < 0.1,
    "sticky should be clamped by the containing block using its original margin box when the margin box is already within the containing block; before_y={:.2} after_y={:.2} bounds={:?}",
    before_origin.y,
    after_origin.y,
    sticky_fragment.bounds
  );
}
