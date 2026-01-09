use std::collections::HashMap;

use fastrender::api::RenderOptions;
use fastrender::interaction::{hit_test_dom_viewport_point, HitTestKind};
use fastrender::scroll::ScrollState;
use fastrender::tree::box_tree::BoxNode;
use fastrender::{FastRender, Point};

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

#[test]
fn hit_testing_fixed_inside_scroller_ignores_element_scroll_offsets() {
  let html = r#"<!DOCTYPE html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #scroller { width: 100px; height: 50px; overflow: scroll; }
      #fixed {
        position: fixed;
        display: block;
        top: 0;
        left: 0;
        width: 100px;
        height: 20px;
        background: rgb(255, 0, 0);
      }
      #spacer { height: 200px; }
    </style>
  </head>
  <body>
    <div id="scroller">
      <a id="fixed" href="/ok">Target</a>
      <div id="spacer"></div>
    </div>
  </body>
</html>"#;

  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(100, 50);
  let prepared = renderer.prepare_html(html, options).expect("prepare html");

  let scroller_id =
    box_id_by_element_id(&prepared.box_tree().root, "scroller").expect("scroller box id");
  let scroll_state = ScrollState::from_parts(
    Point::ZERO,
    HashMap::from([(scroller_id, Point::new(0.0, 25.0))]),
  );

  let result = hit_test_dom_viewport_point(&prepared, &scroll_state, Point::new(5.0, 5.0))
    .expect("expected a hit result");
  assert_eq!(result.kind, HitTestKind::Link);
  assert_eq!(result.href.as_deref(), Some("/ok"));
}

