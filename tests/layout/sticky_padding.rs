use std::collections::HashMap;

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
fn sticky_top_clamps_to_scroll_container_padding() {
  let html = r#"
    <style>
      html, body { margin: 0; padding: 0; }
      #scroller {
        width: 100px;
        height: 50px;
        overflow: auto;
        padding-top: 10px;
        border-top: 5px solid rgb(0, 0, 0);
      }
      #sticky {
        position: sticky;
        top: 0;
        height: 10px;
        background: rgb(0, 255, 0);
      }
      #filler { height: 200px; }
    </style>
    <div id="scroller">
      <div id="sticky"></div>
      <div id="filler"></div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(120, 80);
  let prepared = renderer.prepare_html(html, options).expect("prepare html");

  let scroller_id = box_id_by_element_id(&prepared.box_tree().root, "scroller")
    .expect("scroller box id");
  let sticky_id =
    box_id_by_element_id(&prepared.box_tree().root, "sticky").expect("sticky box id");

  let scroll = Point::new(0.0, 30.0);
  let scroll_state = ScrollState::from_parts(Point::ZERO, HashMap::from([(scroller_id, scroll)]));

  let mut tree = prepared.fragment_tree().clone();
  renderer.apply_sticky_offsets_to_tree_with_scroll_state(&mut tree, &scroll_state);

  let (scroller_origin, _) =
    find_fragment_origin_by_box_id(&tree.root, Point::ZERO, scroller_id)
      .expect("scroller fragment");
  let (sticky_origin, sticky_fragment) =
    find_fragment_origin_by_box_id(&tree.root, Point::ZERO, sticky_id).expect("sticky fragment");

  let sticky_screen_y = sticky_origin.y - scroll.y;
  let expected_screen_y = scroller_origin.y + 5.0 + 10.0;
  assert!(
    (sticky_screen_y - expected_screen_y).abs() < 0.1,
    "sticky screen y should clamp to border+padding (expected {:.1}, got {:.1}); abs_y={:.1} scroll_y={:.1} bounds={:?}",
    expected_screen_y,
    sticky_screen_y,
    sticky_origin.y,
    scroll.y,
    sticky_fragment.bounds
  );
}

#[test]
fn sticky_left_clamps_to_scroll_container_padding() {
  let html = r#"
    <style>
      html, body { margin: 0; padding: 0; }
      #scroller {
        width: 50px;
        height: 30px;
        overflow: auto;
        padding-left: 10px;
      }
      #sticky {
        position: sticky;
        left: 0;
        width: 10px;
        height: 10px;
        background: rgb(0, 255, 0);
      }
      #wide { width: 200px; height: 10px; }
    </style>
    <div id="scroller">
      <div id="sticky"></div>
      <div id="wide"></div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(80, 40);
  let prepared = renderer.prepare_html(html, options).expect("prepare html");

  let scroller_id = box_id_by_element_id(&prepared.box_tree().root, "scroller")
    .expect("scroller box id");
  let sticky_id =
    box_id_by_element_id(&prepared.box_tree().root, "sticky").expect("sticky box id");

  let scroll = Point::new(30.0, 0.0);
  let scroll_state = ScrollState::from_parts(Point::ZERO, HashMap::from([(scroller_id, scroll)]));

  let mut tree = prepared.fragment_tree().clone();
  renderer.apply_sticky_offsets_to_tree_with_scroll_state(&mut tree, &scroll_state);

  let (scroller_origin, _) =
    find_fragment_origin_by_box_id(&tree.root, Point::ZERO, scroller_id)
      .expect("scroller fragment");
  let (sticky_origin, sticky_fragment) =
    find_fragment_origin_by_box_id(&tree.root, Point::ZERO, sticky_id).expect("sticky fragment");

  let sticky_screen_x = sticky_origin.x - scroll.x;
  let expected_screen_x = scroller_origin.x + 10.0;
  assert!(
    (sticky_screen_x - expected_screen_x).abs() < 0.1,
    "sticky screen x should clamp to padding-left (expected {:.1}, got {:.1}); abs_x={:.1} scroll_x={:.1} bounds={:?}",
    expected_screen_x,
    sticky_screen_x,
    sticky_origin.x,
    scroll.x,
    sticky_fragment.bounds
  );
}

