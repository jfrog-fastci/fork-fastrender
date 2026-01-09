use fastrender::animation;
use fastrender::css::types::BoxShadow;
use fastrender::interaction::dom_index::DomIndex;
use fastrender::interaction::dom_mutation;
use fastrender::style::cascade::StyledNode;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use fastrender::{BrowserDocument, RenderOptions, Result};

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
  if let FragmentContent::RunningAnchor { snapshot, .. } = &fragment.content {
    return find_fragment(snapshot, box_id);
  }
  None
}

fn fragment_box_shadows(tree: &FragmentTree, box_id: usize) -> Vec<BoxShadow> {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  frag
    .style
    .as_ref()
    .map(|s| s.box_shadow.clone())
    .expect("style present")
}

fn first_box_shadow(tree: &FragmentTree, box_id: usize) -> BoxShadow {
  let shadows = fragment_box_shadows(tree, box_id);
  assert_eq!(shadows.len(), 1, "expected exactly one box shadow");
  shadows[0].clone()
}

fn build_document_with_transition(transition: &str) -> Result<BrowserDocument> {
  let html = format!(
    r#"
      <html>
        <head>
          <style>
            html, body {{ margin: 0; padding: 0; }}
            #box {{ width: 100px; height: 100px; transition: {transition}; }}
            .a {{ box-shadow: inset 0 0 0 0 rgb(255, 0, 0); }}
            .b {{ box-shadow: 10px 0 0 0 rgb(255, 0, 0); }}
          </style>
        </head>
        <body>
          <div id="box" class="a"></div>
        </body>
      </html>
    "#
  );
  BrowserDocument::from_html(
    &html,
    RenderOptions::new()
      .with_viewport(200, 200)
      .with_animation_time(0.0),
  )
}

fn render_after_class_change(transition: &str) -> Result<(FragmentTree, usize)> {
  let mut doc = build_document_with_transition(transition)?;

  // Initial render at t=0.
  doc.render_frame()?;

  let changed = doc.mutate_dom(|dom| {
    let mut index = DomIndex::build(dom);
    let node_id = *index
      .id_by_element_id
      .get("box")
      .expect("expected #box element");
    index
      .with_node_mut(node_id, |node| dom_mutation::set_attr(node, "class", "b"))
      .unwrap_or(false)
  });
  assert!(changed, "expected class mutation to report a change");

  // Re-render at the same logical time (t=0) so any transition start logic sees the
  // style-change event.
  doc.render_frame()?;

  let prepared = doc.prepared().expect("expected prepared document after render");
  let node_id = styled_node_id_by_id(prepared.styled_tree(), "box").expect("styled id");
  let box_id = box_id_for_styled(&prepared.box_tree().root, node_id).expect("box id");
  Ok((prepared.fragment_tree().clone(), box_id))
}

#[test]
fn dynamic_value_pair_discrete_box_shadow_does_not_start_without_allow_discrete() -> Result<()> {
  let (tree, box_id) = render_after_class_change("box-shadow 1000ms linear")?;

  let mut sampled = tree.clone();
  let viewport = sampled.viewport_size();
  animation::apply_transitions(&mut sampled, 400.0, viewport);

  // Value-pair-discrete (inset mismatch) should be gated by `transition-behavior: allow-discrete`.
  // Without allow-discrete, we should not start a transition and should jump immediately.
  let shadow = first_box_shadow(&sampled, box_id);
  assert!(!shadow.inset, "expected after-change box-shadow");
  assert!((shadow.offset_x.to_px() - 10.0).abs() < 1e-3);
  Ok(())
}

#[test]
fn dynamic_value_pair_discrete_box_shadow_flips_at_midpoint_with_allow_discrete() -> Result<()> {
  let (tree, box_id) = render_after_class_change("box-shadow 1000ms linear allow-discrete")?;

  let mut early = tree.clone();
  let viewport = early.viewport_size();
  animation::apply_transitions(&mut early, 400.0, viewport);
  let shadow = first_box_shadow(&early, box_id);
  assert!(shadow.inset, "expected inset box-shadow before midpoint");
  assert!((shadow.offset_x.to_px() - 0.0).abs() < 1e-3);

  let mut late = tree.clone();
  let viewport = late.viewport_size();
  animation::apply_transitions(&mut late, 600.0, viewport);
  let shadow = first_box_shadow(&late, box_id);
  assert!(!shadow.inset, "expected after-change box-shadow after midpoint");
  assert!((shadow.offset_x.to_px() - 10.0).abs() < 1e-3);
  Ok(())
}

