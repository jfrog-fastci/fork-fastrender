use std::sync::Once;

use fastrender::animation;
use fastrender::api::FastRender;
use fastrender::style::types::{BasicShape, ClipPath, ShapeRadius};
use fastrender::{BoxNode, FragmentNode, FragmentTree, RenderOptions};

static INIT_ENV: Once = Once::new();

fn ensure_test_env() {
  INIT_ENV.call_once(|| {
    // FastRender uses Rayon for parallel layout/paint. Rayon defaults to the host CPU count, which
    // can exceed sandbox thread budgets and cause the global pool init to fail.
    if std::env::var("RAYON_NUM_THREADS").is_err() {
      std::env::set_var("RAYON_NUM_THREADS", "1");
    }
  });
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
        other => panic!("expected resolved circle length radius, got {other:?}"),
      },
      other => panic!("expected circle clip-path, got {other:?}"),
    },
    other => panic!("expected basic shape clip-path, got {other:?}"),
  }
}

#[test]
fn transitions_interpolate_circle_keyword_radius_over_time() {
  ensure_test_env();
  let mut renderer = FastRender::new().expect("renderer");
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
  animation::apply_transitions(&mut start, 0.0, viewport);
  let radius_start = fragment_circle_radius_px(&start, box_id);
  assert!(
    (radius_start - 50.0).abs() < 1e-3,
    "expected closest-side resolved to ~50px at t=0, got {radius_start}"
  );

  let mut mid = prepared.fragment_tree().clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  let radius_mid = fragment_circle_radius_px(&mid, box_id);
  assert!(
    (radius_mid - 75.0).abs() < 1e-3,
    "expected ~75px at t=500ms, got {radius_mid}"
  );
}

