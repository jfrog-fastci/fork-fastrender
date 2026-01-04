mod r#ref;

use fastrender::animation;
use fastrender::api::{FastRender, RenderOptions};
use fastrender::css::types::BoxShadow;
use fastrender::image_output::{encode_image, OutputFormat};
use fastrender::style::cascade::StyledNode;
use fastrender::style::types::{BasicShape, ClipPath};
use fastrender::tree::box_tree::{BoxNode, BoxTree};
use fastrender::tree::fragment_tree::{FragmentNode, FragmentTree};
use r#ref::image_compare::{compare_config_from_env, compare_pngs, CompareEnvVars};
use std::fs;
use std::path::PathBuf;

fn prepare(html: &str, width: u32, height: u32) -> (BoxTree, FragmentTree, StyledNode) {
  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(width, height);
  let prepared = renderer.prepare_html(html, options).expect("prepare");
  (
    prepared.box_tree().clone(),
    prepared.fragment_tree().clone(),
    prepared.styled_tree().clone(),
  )
}

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
  if let FragmentNode {
    content: fastrender::tree::fragment_tree::FragmentContent::RunningAnchor { snapshot, .. },
    ..
  } = fragment
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

fn fragment_transform_x(tree: &FragmentTree, box_id: usize) -> f32 {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  match style.transform.as_slice() {
    [fastrender::css::types::Transform::TranslateX(len)] => len.to_px(),
    _ => 0.0,
  }
}

fn fragment_box_shadows(tree: &FragmentTree, box_id: usize) -> Vec<BoxShadow> {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  frag
    .style
    .as_ref()
    .map(|s| s.box_shadow.clone())
    .expect("style present")
}

fn fragment_clip_shape(tree: &FragmentTree, box_id: usize) -> BasicShape {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  match &style.clip_path {
    ClipPath::BasicShape(shape, _) => shape.as_ref().clone(),
    other => panic!("expected basic shape clip-path, got {other:?}"),
  }
}

#[test]
fn transitions_interpolate_over_time() {
  let html = r#"
    <style>
      @starting-style { #box { opacity: 0; } }
      #box { width: 100px; height: 100px; background: black; opacity: 1; transition: opacity 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut start = fragment_tree.clone();
  let viewport = start.viewport_size();
  animation::apply_transitions(&mut start, 0.0, viewport);
  assert!((fragment_opacity(&start, box_id) - 0.0).abs() < 1e-3);

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  assert!((fragment_opacity(&mid, box_id) - 0.5).abs() < 1e-3);

  let mut end = fragment_tree.clone();
  let viewport = end.viewport_size();
  animation::apply_transitions(&mut end, 1000.0, viewport);
  assert!((fragment_opacity(&end, box_id) - 1.0).abs() < 1e-3);
}

#[test]
fn transitions_interpolate_box_shadow_over_time() {
  let html = r#"
    <style>
      @starting-style { #box { box-shadow: none; } }
      #box { width: 100px; height: 100px; box-shadow: 10px 0px 0px 0px rgba(255, 0, 0, 1); transition: box-shadow 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut start = fragment_tree.clone();
  let viewport = start.viewport_size();
  animation::apply_transitions(&mut start, 0.0, viewport);
  assert!(fragment_box_shadows(&start, box_id).is_empty());

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  let shadows = fragment_box_shadows(&mid, box_id);
  assert_eq!(shadows.len(), 1);
  let shadow = &shadows[0];
  assert!((shadow.offset_x.to_px() - 5.0).abs() < 1e-3);
  assert!((shadow.color.a - 0.5).abs() < 1e-6);
}

#[test]
fn transition_delay_is_honored() {
  let html = r#"
    <style>
      @starting-style { #box { opacity: 0; } }
      #box { width: 100px; height: 100px; opacity: 1; transition: opacity 400ms linear 200ms; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut before_delay = fragment_tree.clone();
  let viewport = before_delay.viewport_size();
  animation::apply_transitions(&mut before_delay, 100.0, viewport);
  assert!((fragment_opacity(&before_delay, box_id) - 0.0).abs() < 1e-3);

  let mut during = fragment_tree.clone();
  let viewport = during.viewport_size();
  animation::apply_transitions(&mut during, 300.0, viewport);
  assert!((fragment_opacity(&during, box_id) - 0.25).abs() < 1e-3);
}

#[test]
fn transition_property_filters_supported_properties() {
  let html = r#"
    <style>
      @starting-style { #box { opacity: 0; transform: translateX(0px); } }
      #box {
        width: 100px; height: 100px;
        opacity: 1;
        transform: translateX(100px);
        transition-property: transform;
        transition-duration: 1000ms;
        transition-timing-function: linear;
      }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 300, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut start = fragment_tree.clone();
  let viewport = start.viewport_size();
  animation::apply_transitions(&mut start, 0.0, viewport);
  let frag = find_fragment(&start.root, box_id).expect("fragment");
  let style = frag.style.as_ref().expect("style");
  assert_eq!(style.opacity, 1.0, "opacity should not transition");

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  let translated = fragment_transform_x(&mid, box_id);
  assert!((translated - 50.0).abs() < 1e-3);
}

#[test]
fn zero_duration_disables_transition() {
  let html = r#"
    <style>
      @starting-style { #box { opacity: 0; } }
      #box { width: 50px; height: 50px; opacity: 1; transition: opacity 0s; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 100, 100);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut sampled = fragment_tree.clone();
  let viewport = sampled.viewport_size();
  animation::apply_transitions(&mut sampled, 0.0, viewport);
  assert!((fragment_opacity(&sampled, box_id) - 1.0).abs() < 1e-3);
}

#[test]
fn transitions_fall_back_to_discrete_when_interpolation_fails() {
  let html = r#"
    <style>
      @starting-style { #box { clip-path: inset(0%); } }
      #box {
        width: 100px;
        height: 100px;
        clip-path: circle(50%);
        transition: clip-path 1000ms linear;
      }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut start = fragment_tree.clone();
  let viewport = start.viewport_size();
  animation::apply_transitions(&mut start, 0.0, viewport);
  assert!(matches!(
    fragment_clip_shape(&start, box_id),
    BasicShape::Inset { .. }
  ));

  let mut quarter = fragment_tree.clone();
  let viewport = quarter.viewport_size();
  animation::apply_transitions(&mut quarter, 250.0, viewport);
  assert!(matches!(
    fragment_clip_shape(&quarter, box_id),
    BasicShape::Inset { .. }
  ));

  let mut half = fragment_tree.clone();
  let viewport = half.viewport_size();
  animation::apply_transitions(&mut half, 500.0, viewport);
  assert!(matches!(
    fragment_clip_shape(&half, box_id),
    BasicShape::Circle { .. }
  ));

  let mut late = fragment_tree.clone();
  let viewport = late.viewport_size();
  animation::apply_transitions(&mut late, 750.0, viewport);
  assert!(matches!(
    fragment_clip_shape(&late, box_id),
    BasicShape::Circle { .. }
  ));
}

#[test]
fn visual_fixture_matches_goldens() {
  std::env::set_var("FASTR_USE_BUNDLED_FONTS", "1");
  let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let html = fs::read_to_string(root.join("tests/fixtures/html/transition_starting_style.html"))
    .expect("fixture html");
  let compare_config = compare_config_from_env(CompareEnvVars::fixtures()).expect("compare config");
  let mut renderer = FastRender::new().expect("renderer");
  let prepared = renderer
    .prepare_html(&html, RenderOptions::new().with_viewport(260, 180))
    .expect("prepare");
  let cases = [
    ("transition_starting_style_0ms", 0.0),
    ("transition_starting_style_400ms", 400.0),
  ];
  for (name, time) in cases {
    let pixmap = prepared.paint_at_time(time).expect("render");
    let png = encode_image(&pixmap, OutputFormat::Png).expect("encode png");
    let golden_path = root
      .join("tests/fixtures/golden")
      .join(format!("{name}.png"));

    if std::env::var("UPDATE_GOLDEN").is_ok() {
      fs::create_dir_all(golden_path.parent().unwrap()).expect("golden dir");
      fs::write(&golden_path, &png).expect("write golden");
      continue;
    }

    let golden = fs::read(&golden_path).expect("golden png");
    let diff_dir = root.join("target/transition_starting_style_diffs");
    compare_pngs(name, &png, &golden, &compare_config, &diff_dir).expect("compare");
  }
}
