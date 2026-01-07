use fastrender::api::FastRender;
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaType;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::Length;
use std::collections::HashMap;

fn layout_intermediates_for(html: &str) -> fastrender::api::LayoutIntermediates {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed dom");
  renderer
    .layout_document_for_media_intermediates(&dom, 800, 600, MediaType::Screen)
    .expect("laid out")
}

fn layout_intermediates_for_with_toggles(
  html: &str,
  toggles: RuntimeToggles,
) -> fastrender::api::LayoutIntermediates {
  let mut renderer = FastRender::builder()
    .runtime_toggles(toggles)
    .build()
    .expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed dom");
  renderer
    .layout_document_for_media_intermediates(&dom, 800, 600, MediaType::Screen)
    .expect("laid out")
}

fn styled_tree_for(html: &str) -> StyledNode {
  layout_intermediates_for(html).styled_tree
}

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .map(|value| value == id)
    .unwrap_or(false)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn box_id_for_styled_node_id(node: &BoxNode, styled_node_id: usize) -> Option<usize> {
  if node.styled_node_id == Some(styled_node_id) && node.generated_pseudo.is_none() {
    return Some(node.id);
  }
  node
    .children
    .iter()
    .find_map(|child| box_id_for_styled_node_id(child, styled_node_id))
}

fn find_fragment_by_box_id<'a>(fragment: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
  fragment.iter_fragments().find(|node| match &node.content {
    FragmentContent::Block { box_id: Some(id) } => *id == box_id,
    FragmentContent::Inline { box_id: Some(id), .. } => *id == box_id,
    FragmentContent::Text { box_id: Some(id), .. } => *id == box_id,
    FragmentContent::Replaced { box_id: Some(id), .. } => *id == box_id,
    _ => false,
  })
}

#[test]
fn container_query_units_in_inline_style_trigger_container_pass() {
  let html = r#"
    <style>
      .container { width: 400px; height: 80px; background: #eee; }
      .child { height: 20px; background: red; }
    </style>
    <div class="container" style="container-type: inline-size">
      <div id="target" class="child" style="width: 50cqw"></div>
    </div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target element");

  assert_eq!(target.styles.width, Some(Length::px(200.0)));
}

#[test]
fn container_query_units_update_flex_layout_when_cache_enabled() {
  let html = r#"
    <style>
      #container { display: flex; width: 400px; height: 80px; background: #eee; }
      #target { height: 20px; background: red; flex: none; }
    </style>
    <div id="container" style="container-type: inline-size">
      <div id="target" style="width: 50cqw"></div>
    </div>
  "#;

  let render_target_width = |toggles: RuntimeToggles| -> f32 {
    let intermediates = layout_intermediates_for_with_toggles(html, toggles);
    let target = find_by_id(&intermediates.styled_tree, "target").expect("target element");
    assert_eq!(target.styles.width, Some(Length::px(200.0)));

    let styled_node_id = target.node_id;
    let target_box_id =
      box_id_for_styled_node_id(&intermediates.box_tree.root, styled_node_id).expect("target box");
    let target_fragment =
      find_fragment_by_box_id(&intermediates.fragment_tree.root, target_box_id).expect("target fragment");
    target_fragment.bounds.width()
  };

  let width_without_cache = render_target_width(RuntimeToggles::from_map(HashMap::from([
    ("FASTR_DISABLE_LAYOUT_CACHE".to_string(), "1".to_string()),
    ("FASTR_DISABLE_FLEX_CACHE".to_string(), "1".to_string()),
  ])));

  let width_with_cache = render_target_width(RuntimeToggles::from_map(HashMap::from([
    ("FASTR_DISABLE_LAYOUT_CACHE".to_string(), "0".to_string()),
    ("FASTR_DISABLE_FLEX_CACHE".to_string(), "0".to_string()),
  ])));

  assert!(
    (width_without_cache - 200.0).abs() < 0.5,
    "expected baseline flex item width resolved via cqw (want 200, got {width_without_cache})"
  );
  assert!(
    (width_with_cache - width_without_cache).abs() < 0.5,
    "layout cache should not change cqw flex item width (no_cache={width_without_cache}, with_cache={width_with_cache})"
  );
}
