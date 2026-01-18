#![cfg(test)]

use crate::debug::runtime::RuntimeToggles;
use crate::style::types::{BasicShape, ClipPath, ShapeRadius};
use crate::{BoxNode, FastRender, FontConfig, FragmentNode, FragmentTree, RenderOptions, ResourcePolicy};

fn create_test_renderer() -> FastRender {
  crate::testing::init_rayon_for_tests(1);
  FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .resource_policy(ResourcePolicy::default().allow_http(false).allow_https(false))
    // Avoid host `FASTR_*` env vars influencing deterministic unit test results.
    .runtime_toggles(RuntimeToggles::default())
    .build()
    .expect("renderer")
}

fn find_box_id_by_dom_id(node: &BoxNode, id: &str) -> Option<usize> {
  if node.debug_info.as_ref().and_then(|info| info.id.as_deref()) == Some(id) {
    return Some(node.id);
  }
  node
    .children
    .iter()
    .find_map(|child| find_box_id_by_dom_id(child, id))
}

fn find_fragment_by_box_id<'a>(tree: &'a FragmentTree, box_id: usize) -> Option<&'a FragmentNode> {
  fn rec<'a>(node: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
    if node.box_id() == Some(box_id) {
      return Some(node);
    }
    node.children.iter().find_map(|child| rec(child, box_id))
  }

  rec(&tree.root, box_id).or_else(|| {
    tree
      .additional_fragments
      .iter()
      .find_map(|frag| rec(frag, box_id))
  })
}

fn fragment_circle_radius_px(tree: &FragmentTree, box_id: usize) -> f32 {
  let frag = find_fragment_by_box_id(tree, box_id).expect("fragment present");
  let style = frag.style.as_deref().expect("style present");
  match &style.clip_path {
    ClipPath::BasicShape(shape, _) => match shape.as_ref() {
      BasicShape::Circle { radius, .. } => match radius {
        ShapeRadius::Length(len) => len.to_px(),
        other => unreachable!("expected resolved circle length radius, got {other:?}"),
      },
      other => unreachable!("expected circle clip-path, got {other:?}"),
    },
    other => unreachable!("expected basic shape clip-path, got {other:?}"),
  }
}

#[test]
fn transitions_interpolate_circle_keyword_radius_over_time() {
  let mut renderer = create_test_renderer();
  let options = RenderOptions::new().with_viewport(200, 200);
  let html = r#"
    <style>
      @starting-style { #box { clip-path: circle(closest-side); } }
      #box {
        width: 100px;
        height: 100px;
        clip-path: circle(100%);
        transition: clip-path 1000ms linear;
      }
    </style>
    <div id="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let box_id = find_box_id_by_dom_id(&prepared.box_tree().root, "box").expect("box_id");

  let mut start = prepared.fragment_tree().clone();
  let viewport = start.viewport_size();
  super::apply_transitions(&mut start, 0.0, viewport);
  let radius_start = fragment_circle_radius_px(&start, box_id);
  assert!(
    (radius_start - 50.0).abs() < 1e-3,
    "expected closest-side resolved to ~50px at t=0, got {radius_start}"
  );

  let mut mid = prepared.fragment_tree().clone();
  let viewport = mid.viewport_size();
  super::apply_transitions(&mut mid, 500.0, viewport);
  let radius_mid = fragment_circle_radius_px(&mid, box_id);
  assert!(
    (radius_mid - 75.0).abs() < 1e-3,
    "expected ~75px at t=500ms, got {radius_mid}"
  );
}
