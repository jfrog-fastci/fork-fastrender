use fastrender::dom::{enumerate_dom_ids, DomNode};
use fastrender::interaction::dom_mutation;
use fastrender::interaction::element_geometry::element_geometry_for_styled_node_id;
use fastrender::interaction::InteractionState;
use fastrender::{BrowserDocument, RenderOptions, Result};

use super::support;

fn find_node_by_id_attr<'a>(node: &'a DomNode, id_attr: &str) -> Option<&'a DomNode> {
  if node
    .get_attribute_ref("id")
    .is_some_and(|id| id == id_attr)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_node_by_id_attr(child, id_attr) {
      return Some(found);
    }
  }
  None
}

fn find_node_mut_by_id_attr<'a>(node: &'a mut DomNode, id_attr: &str) -> Option<&'a mut DomNode> {
  if node
    .get_attribute_ref("id")
    .is_some_and(|id| id == id_attr)
  {
    return Some(node);
  }
  for child in node.children.iter_mut() {
    if let Some(found) = find_node_mut_by_id_attr(child, id_attr) {
      return Some(found);
    }
  }
  None
}

#[test]
fn scroll_anchoring_focus_priority_candidate_keeps_focused_input_stable() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          input { display: block; height: 20px; }
        </style>
      </head>
      <body>
        <div id="spacer" style="height: 1200px;"></div>
        <input id="target" type="text" value="hi">
      </body>
    </html>"#;

  let options = RenderOptions::new().with_viewport(200, 100);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;

  // Prime the layout cache so we can query geometry and then scroll.
  doc.render_frame()?;

  let ids = enumerate_dom_ids(doc.dom());
  let input = find_node_by_id_attr(doc.dom(), "target").expect("#target input");
  let input_id = *ids
    .get(&(input as *const DomNode))
    .expect("input node id");

  // Scroll so the input is visible with a stable reference position in the viewport.
  let prepared = doc.prepared().expect("prepared");
  let (geom, _) = element_geometry_for_styled_node_id(
    prepared.box_tree(),
    prepared.fragment_tree(),
    input_id,
  )
  .expect("input geometry");

  let desired_viewport_y = 20.0;
  let scroll_y = (geom.border_box.y() - desired_viewport_y).max(0.0);
  doc.set_scroll(0.0, scroll_y);

  let interaction_state = InteractionState {
    focused: Some(input_id),
    ..InteractionState::default()
  };

  doc.render_frame_with_scroll_state_and_interaction_state(Some(&interaction_state))?;
  let before_scroll_y = doc.scroll_state().viewport.y;
  let prepared_before = doc.prepared().expect("prepared");
  let (geom_before, _) = element_geometry_for_styled_node_id(
    prepared_before.box_tree(),
    prepared_before.fragment_tree(),
    input_id,
  )
  .expect("input geometry after initial scroll");
  let before_viewport_y = geom_before.border_box.y() - before_scroll_y;

  // Grow the spacer above the focused input; scroll anchoring should compensate by adjusting the
  // scroll offset so the focused element stays in the same viewport position.
  let changed = doc.mutate_dom(|dom| {
    let spacer = find_node_mut_by_id_attr(dom, "spacer").expect("#spacer");
    dom_mutation::set_attr(spacer, "style", "height: 1400px;")
  });
  assert!(changed, "expected spacer style mutation to report a change");

  doc.render_frame_with_scroll_state_and_interaction_state(Some(&interaction_state))?;
  let after_scroll_y = doc.scroll_state().viewport.y;
  let prepared_after = doc.prepared().expect("prepared");
  let (geom_after, _) = element_geometry_for_styled_node_id(
    prepared_after.box_tree(),
    prepared_after.fragment_tree(),
    input_id,
  )
  .expect("input geometry after spacer growth");
  let after_viewport_y = geom_after.border_box.y() - after_scroll_y;

  let delta_scroll_y = after_scroll_y - before_scroll_y;
  assert!(
    (delta_scroll_y - 200.0).abs() < 1.0,
    "expected scroll anchoring to adjust scroll_y by ~200px, got {delta_scroll_y}"
  );
  assert!(
    (after_viewport_y - before_viewport_y).abs() < 1.0,
    "expected focused input to stay stable in viewport (before={before_viewport_y}, after={after_viewport_y})"
  );

  Ok(())
}

