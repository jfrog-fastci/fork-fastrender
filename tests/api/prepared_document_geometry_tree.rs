use fastrender::api::{FastRenderBuilder, RenderOptions};
use fastrender::dom::DomNode;
use fastrender::geometry::Point;
use fastrender::interaction::absolute_bounds_for_box_id;
use fastrender::scroll::ScrollState;
use fastrender::text::font_db::FontConfig;
use fastrender::tree::box_tree::BoxNode;

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

fn find_box_id_for_styled_node_id(box_node: &BoxNode, styled_node_id: usize) -> Option<usize> {
  if box_node.styled_node_id == Some(styled_node_id) && box_node.generated_pseudo.is_none() {
    return Some(box_node.id);
  }
  for child in &box_node.children {
    if let Some(id) = find_box_id_for_styled_node_id(child, styled_node_id) {
      return Some(id);
    }
  }
  if let Some(body) = box_node.footnote_body.as_deref() {
    if let Some(id) = find_box_id_for_styled_node_id(body, styled_node_id) {
      return Some(id);
    }
  }
  None
}

#[test]
fn prepared_document_fragment_tree_for_geometry_applies_sticky_offsets() -> fastrender::Result<()> {
  // Avoid CI flakes from Rayon's default "one thread per CPU" global pool configuration.
  crate::common::init_rayon_for_tests(1);

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #spacer { height: 50px; }
          #sticky { position: sticky; top: 0; height: 10px; }
          #below { height: 200px; }
        </style>
      </head>
      <body>
        <div id="spacer"></div>
        <div id="sticky"></div>
        <div id="below"></div>
      </body>
    </html>"#;

  let mut renderer = FastRenderBuilder::new()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(200, 100))?;

  // Resolve the sticky element's styled node id via DOM preorder ids.
  let dom_ids = fastrender::dom::enumerate_dom_ids(prepared.dom());
  let sticky_node = find_dom_node_by_element_id(prepared.dom(), "sticky").expect("sticky node");
  let styled_node_id = *dom_ids
    .get(&(sticky_node as *const DomNode))
    .expect("sticky node id");

  // Find the element's principal box (non-pseudo).
  let sticky_box_id =
    find_box_id_for_styled_node_id(&prepared.box_tree().root, styled_node_id).expect("box id");

  let eps = 1e-3;

  // Without any scroll, the sticky element should be placed after the spacer.
  let scroll_state = ScrollState::with_viewport(Point::new(0.0, 0.0));
  let rect = absolute_bounds_for_box_id(prepared.fragment_tree(), sticky_box_id).expect("bounds");
  assert!(
    (rect.y() - 50.0).abs() < eps,
    "expected page y ≈ 50 at scroll {:?}, got {:?}",
    scroll_state.viewport,
    rect
  );

  // With viewport scroll, the sticky element should pin to the top of the viewport. The geometry
  // tree stays in page coordinates, so its y should equal the viewport scroll offset (60).
  let scroll_state = ScrollState::with_viewport(Point::new(0.0, 60.0));
  let tree = prepared.fragment_tree_for_geometry(&scroll_state);
  let rect = absolute_bounds_for_box_id(&tree, sticky_box_id).expect("bounds");
  assert!(
    (rect.y() - 60.0).abs() < eps,
    "expected page y ≈ 60 at scroll {:?}, got {:?}",
    scroll_state.viewport,
    rect
  );

  Ok(())
}
