use fastrender::animation;
use fastrender::interaction::dom_index::DomIndex;
use fastrender::interaction::dom_mutation;
use fastrender::{BrowserDocument, RenderOptions, Result};
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::{FragmentNode, FragmentTree};
use fastrender::style::cascade::StyledNode;

fn styled_node_id_by_id(styled: &StyledNode, target_id: &str) -> Option<usize> {
  if styled
    .node
    .get_attribute("id")
    .is_some_and(|id| id.eq_ignore_ascii_case(target_id))
  {
    return Some(styled.node_id);
  }
  for child in &styled.children {
    if let Some(id) = styled_node_id_by_id(child, target_id) {
      return Some(id);
    }
  }
  None
}

fn box_id_for_styled(box_node: &BoxNode, styled_id: usize) -> Option<usize> {
  if box_node.styled_node_id == Some(styled_id) {
    return Some(box_node.id);
  }
  for child in &box_node.children {
    if let Some(id) = box_id_for_styled(child, styled_id) {
      return Some(id);
    }
  }
  if let Some(body) = box_node.footnote_body.as_deref() {
    if let Some(id) = box_id_for_styled(body, styled_id) {
      return Some(id);
    }
  }
  None
}

fn find_fragment<'a>(fragment: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
  if fragment.box_id() == Some(box_id) {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment(child, box_id) {
      return Some(found);
    }
  }
  if let fastrender::tree::fragment_tree::FragmentContent::RunningAnchor { snapshot, .. }
  | fastrender::tree::fragment_tree::FragmentContent::FootnoteAnchor { snapshot } = &fragment.content
  {
    return find_fragment(snapshot, box_id);
  }
  None
}

fn fragment_opacity(tree: &FragmentTree, box_id: usize) -> f32 {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  frag
    .style
    .as_ref()
    .map(|s| s.opacity)
    .expect("style present")
}

fn sample_opacity(prepared: &fastrender::api::PreparedDocument, time_ms: f32, box_id: usize) -> f32 {
  let mut sampled = prepared.fragment_tree().clone();
  let viewport = sampled.viewport_size();
  animation::apply_transitions(&mut sampled, time_ms, viewport);
  fragment_opacity(&sampled, box_id)
}

fn box_id_for_prepared(prepared: &fastrender::api::PreparedDocument, element_id: &str) -> usize {
  let styled_id = styled_node_id_by_id(prepared.styled_tree(), element_id).expect("styled node id");
  box_id_for_styled(&prepared.box_tree().root, styled_id).expect("box id")
}

#[test]
fn browser_document_transition_state_tracks_style_changes_across_frames() -> Result<()> {
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; background: white; }
          #box { width: 10px; height: 10px; background: black; opacity: 0; transition: opacity 1000ms linear; }
          #box.on { opacity: 1; }
        </style>
      </head>
      <body>
        <div id="box"></div>
      </body>
    </html>"#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new().with_viewport(16, 16).with_animation_time(0.0),
  )?;

  // First frame seeds layout; no transitions yet because style hasn't changed.
  doc.render_frame()?;
  let prepared = doc.prepared().expect("prepared document");
  let _box_id = box_id_for_prepared(prepared, "box");

  // Apply a style change at t=100ms and ensure the transition starts at that timestamp.
  doc.set_animation_time_ms(100.0);
  doc.mutate_dom(|dom| {
    let mut index = DomIndex::build(dom);
    let node_id = *index
      .id_by_element_id
      .get("box")
      .expect("expected #box element");
    index
      .with_node_mut(node_id, |node| dom_mutation::set_attr(node, "class", "on"))
      .unwrap_or(false)
  });
  doc.render_frame()?;
  let prepared = doc.prepared().expect("prepared document");
  let box_id = box_id_for_prepared(prepared, "box");
  assert!((sample_opacity(prepared, 100.0, box_id) - 0.0).abs() < 1e-3);

  // Advance time without restyling; the transition should progress without restarting.
  doc.set_animation_time_ms(600.0);
  doc.render_if_needed()?.expect("expected repaint at new time");
  let prepared = doc.prepared().expect("prepared document");
  let box_id = box_id_for_prepared(prepared, "box");
  assert!((sample_opacity(prepared, 600.0, box_id) - 0.5).abs() < 1e-3);

  // Interrupt the transition mid-flight (toggle class off at t=800ms) and ensure the new transition
  // starts from the interpolated value, not the previous end value.
  doc.set_animation_time_ms(800.0);
  doc.mutate_dom(|dom| {
    let mut index = DomIndex::build(dom);
    let node_id = *index
      .id_by_element_id
      .get("box")
      .expect("expected #box element");
    index
      .with_node_mut(node_id, |node| dom_mutation::remove_attr(node, "class"))
      .unwrap_or(false)
  });
  doc.render_frame()?;
  let prepared = doc.prepared().expect("prepared document");
  let box_id = box_id_for_prepared(prepared, "box");
  assert!((sample_opacity(prepared, 800.0, box_id) - 0.7).abs() < 1e-3);
  // CSS transitions apply a "reversing shortening factor": reversing back to the original value
  // shortens the duration in proportion to how far the prior transition had progressed (here:
  // 700ms of a 1000ms transition => 700ms duration when reversing). At t=1300ms, the reverse
  // transition has progressed 500/700 ≈ 0.714, yielding opacity ≈ 0.2.
  assert!((sample_opacity(prepared, 1300.0, box_id) - 0.2).abs() < 1e-3);

  Ok(())
}
