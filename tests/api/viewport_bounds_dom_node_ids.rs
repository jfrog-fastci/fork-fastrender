use fastrender::api::{FastRenderBuilder, RenderOptions};
use fastrender::dom::DomNode;
use fastrender::geometry::Point;
use fastrender::interaction::dom_geometry::viewport_bounds_for_dom_node_ids;
use fastrender::scroll::ScrollState;
use fastrender::text::font_db::FontConfig;

fn find_dom_node_by_element_id<'a>(root: &'a DomNode, target_id: &str) -> Option<&'a DomNode> {
  let mut stack: Vec<&DomNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node
      .get_attribute_ref("id")
      .is_some_and(|id| id.eq_ignore_ascii_case(target_id))
    {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn viewport_bounds_for_dom_node_ids_handles_viewport_fixed_and_fixed_containing_blocks(
) -> fastrender::Result<()> {
  // Avoid CI flakes from Rayon's default "one thread per CPU" global pool configuration.
  crate::common::init_rayon_for_tests(1);

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #fixed {
            position: fixed;
            top: 10px;
            left: 0;
            width: 50px;
            height: 20px;
          }
          #transform_container {
            transform: translateZ(0);
            height: 40px;
          }
          #fixed_cb {
            position: fixed;
            top: 10px;
            left: 0;
            width: 50px;
            height: 20px;
          }
          #spacer { height: 200px; }
          #normal { width: 50px; height: 20px; }
        </style>
      </head>
      <body>
        <div id="fixed"></div>
        <div id="transform_container"><div id="fixed_cb"></div></div>
        <div id="spacer"></div>
        <div id="normal"></div>
      </body>
    </html>"#;

  let mut renderer = FastRenderBuilder::new()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(200, 100))?;

  let dom_ids = fastrender::dom::enumerate_dom_ids(prepared.dom());
  let normal_node = find_dom_node_by_element_id(prepared.dom(), "normal").expect("normal node");
  let fixed_node = find_dom_node_by_element_id(prepared.dom(), "fixed").expect("fixed node");
  let fixed_cb_node = find_dom_node_by_element_id(prepared.dom(), "fixed_cb").expect("fixed_cb node");

  let normal_id = *dom_ids
    .get(&(normal_node as *const DomNode))
    .expect("normal id");
  let fixed_id = *dom_ids.get(&(fixed_node as *const DomNode)).expect("fixed id");
  let fixed_cb_id = *dom_ids
    .get(&(fixed_cb_node as *const DomNode))
    .expect("fixed_cb id");

  let ids = [normal_id, fixed_id, fixed_cb_id];
  let eps = 1e-3;

  let scroll0 = ScrollState::with_viewport(Point::new(0.0, 0.0));
  let bounds0 = viewport_bounds_for_dom_node_ids(&prepared, &scroll0, &ids);
  let normal0 = *bounds0.get(&normal_id).expect("normal bounds at scroll0");
  let fixed0 = *bounds0.get(&fixed_id).expect("fixed bounds at scroll0");
  let fixed_cb0 = *bounds0.get(&fixed_cb_id).expect("fixed_cb bounds at scroll0");

  assert!(
    (normal0.y() - 240.0).abs() < eps,
    "expected normal element viewport y ≈ 240 at scroll0, got {normal0:?}"
  );
  assert!(
    (fixed0.y() - 10.0).abs() < eps,
    "expected fixed element viewport y ≈ 10 at scroll0, got {fixed0:?}"
  );
  assert!(
    (fixed_cb0.y() - 10.0).abs() < eps,
    "expected fixed-in-fixed-cb element viewport y ≈ 10 at scroll0, got {fixed_cb0:?}"
  );

  let scroll50 = ScrollState::with_viewport(Point::new(0.0, 50.0));
  let bounds50 = viewport_bounds_for_dom_node_ids(&prepared, &scroll50, &ids);
  let normal50 = *bounds50.get(&normal_id).expect("normal bounds at scroll50");
  let fixed50 = *bounds50.get(&fixed_id).expect("fixed bounds at scroll50");
  let fixed_cb50 = *bounds50.get(&fixed_cb_id).expect("fixed_cb bounds at scroll50");

  // Normal elements scroll with the viewport (page-space bounds minus viewport scroll).
  assert!(
    (normal50.y() - 190.0).abs() < eps,
    "expected normal element viewport y ≈ 190 at scroll50, got {normal50:?}"
  );

  // Viewport-fixed elements are not translated by viewport scroll.
  assert!(
    (fixed50.y() - 10.0).abs() < eps,
    "expected fixed element viewport y ≈ 10 at scroll50, got {fixed50:?}"
  );

  // Fixed elements under an ancestor fixed containing block scroll away like normal content.
  assert!(
    (fixed_cb50.y() + 40.0).abs() < eps,
    "expected fixed element under fixed CB viewport y ≈ -40 at scroll50, got {fixed_cb50:?}"
  );

  Ok(())
}

