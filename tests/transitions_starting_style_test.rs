mod r#ref;

use fastrender::animation;
use fastrender::api::{FastRender, RenderOptions};
use fastrender::css::types::{BoxShadow, TextShadow};
use fastrender::image_output::{encode_image, OutputFormat};
use fastrender::style::cascade::StyledNode;
use fastrender::style::computed::Visibility;
use fastrender::style::types::{
  BackgroundPosition, BackgroundSize, BackgroundSizeComponent, BasicShape, BorderStyle,
  ClipComponent, ClipPath, ClipRect, FillRule, FilterFunction, OutlineStyle,
};
use fastrender::style::values::CustomPropertyTypedValue;
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

fn fragment_visibility(tree: &FragmentTree, box_id: usize) -> Visibility {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  frag
    .style
    .as_ref()
    .map(|s| s.visibility)
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

fn fragment_blur_filter(tree: &FragmentTree, box_id: usize) -> Option<f32> {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  match style.filter.as_slice() {
    [] => None,
    [FilterFunction::Blur(len)] => Some(len.to_px()),
    other => panic!("expected blur filter, got {other:?}"),
  }
}

fn fragment_border_top_left_radius_x(tree: &FragmentTree, box_id: usize) -> f32 {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  style.border_top_left_radius.x.to_px()
}

fn fragment_border_top_right_radius_x(tree: &FragmentTree, box_id: usize) -> f32 {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  style.border_top_right_radius.x.to_px()
}

fn fragment_border_top_width(tree: &FragmentTree, box_id: usize) -> f32 {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  style.border_top_width.to_px()
}

fn fragment_border_top_style(tree: &FragmentTree, box_id: usize) -> BorderStyle {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  style.border_top_style
}

fn fragment_border_top_color(tree: &FragmentTree, box_id: usize) -> fastrender::Rgba {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  style.border_top_color
}

fn fragment_clip_rect(tree: &FragmentTree, box_id: usize) -> Option<ClipRect> {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  frag.style.as_ref().and_then(|s| s.clip.clone())
}

fn fragment_mask_position_x(tree: &FragmentTree, box_id: usize) -> f32 {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  match style.mask_positions.as_ref() {
    [BackgroundPosition::Position { x, .. }, ..] => x.offset.to_px(),
    other => panic!("expected mask-position list, got {other:?}"),
  }
}

fn fragment_mask_size(tree: &FragmentTree, box_id: usize) -> (f32, f32) {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  match style.mask_sizes.as_ref() {
    [BackgroundSize::Explicit(
      BackgroundSizeComponent::Length(x),
      BackgroundSizeComponent::Length(y),
    ), ..] => (x.to_px(), y.to_px()),
    other => panic!("expected mask-size list, got {other:?}"),
  }
}

fn fragment_transform_origin(tree: &FragmentTree, box_id: usize) -> (f32, f32) {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  (
    style.transform_origin.x.to_px(),
    style.transform_origin.y.to_px(),
  )
}

fn fragment_perspective_origin(tree: &FragmentTree, box_id: usize) -> (f32, f32) {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  (
    style.perspective_origin.x.to_px(),
    style.perspective_origin.y.to_px(),
  )
}

fn fragment_outline_color(tree: &FragmentTree, box_id: usize) -> (fastrender::Rgba, bool) {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  style.outline_color.resolve(style.color)
}

fn fragment_outline_width(tree: &FragmentTree, box_id: usize) -> f32 {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  style.outline_width.to_px()
}

fn fragment_outline_style(tree: &FragmentTree, box_id: usize) -> OutlineStyle {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  style.outline_style
}

fn fragment_box_shadows(tree: &FragmentTree, box_id: usize) -> Vec<BoxShadow> {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  frag
    .style
    .as_ref()
    .map(|s| s.box_shadow.clone())
    .expect("style present")
}

fn fragment_text_shadows(tree: &FragmentTree, box_id: usize) -> Vec<TextShadow> {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  frag
    .style
    .as_ref()
    .map(|s| s.text_shadow.as_ref().to_vec())
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
fn transitions_interpolate_registered_custom_properties_over_time() {
  let html = r#"
    <style>
      @property --x {
        syntax: "<number>";
        inherits: false;
        initial-value: 0;
      }

      @starting-style { #box { --x: 0; } }
      #box {
        width: 100px;
        height: 100px;
        opacity: var(--x);
        --x: 1;
        transition: --x 1000ms linear;
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
fn transitions_interpolate_registered_custom_properties_with_transition_all() {
  let html = r#"
    <style>
      @property --x {
        syntax: "<number>";
        inherits: false;
        initial-value: 0;
      }

      @starting-style { #box { --x: 0; } }
      #box {
        width: 100px;
        height: 100px;
        --x: 1;
        transition: all 1000ms linear;
      }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);

  let frag = find_fragment(&mid.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  let value = style
    .custom_properties
    .get("--x")
    .expect("custom property present");
  match value.typed.as_ref() {
    Some(CustomPropertyTypedValue::Number(v)) => assert!((v - 0.5).abs() < 1e-3, "v={v}"),
    other => panic!("expected typed number, got {other:?}"),
  }
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
fn transition_behavior_gates_value_pair_discrete_box_shadow_inset_mismatch() {
  let html = r#"
    <style>
      @starting-style { #box { box-shadow: inset 0 0 0 0 rgb(255, 0, 0); } }
      #box { width: 100px; height: 100px; box-shadow: 10px 0 0 0 rgb(255, 0, 0); transition: box-shadow 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut sampled = fragment_tree.clone();
  let viewport = sampled.viewport_size();
  animation::apply_transitions(&mut sampled, 400.0, viewport);
  let shadows = fragment_box_shadows(&sampled, box_id);
  assert_eq!(shadows.len(), 1);
  let shadow = &shadows[0];
  assert!(!shadow.inset, "expected after-change box-shadow");
  assert!((shadow.offset_x.to_px() - 10.0).abs() < 1e-3);

  let html_allow = r#"
    <style>
      @starting-style { #box { box-shadow: inset 0 0 0 0 rgb(255, 0, 0); } }
      #box { width: 100px; height: 100px; box-shadow: 10px 0 0 0 rgb(255, 0, 0); transition: box-shadow 1000ms linear allow-discrete; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html_allow, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut sampled = fragment_tree.clone();
  let viewport = sampled.viewport_size();
  animation::apply_transitions(&mut sampled, 400.0, viewport);
  let shadows = fragment_box_shadows(&sampled, box_id);
  assert_eq!(shadows.len(), 1);
  let shadow = &shadows[0];
  assert!(shadow.inset, "expected discrete transition to preserve inset shadow");
  assert!((shadow.offset_x.to_px() - 0.0).abs() < 1e-3);
}

#[test]
fn transitions_interpolate_text_shadow_over_time() {
  let html = r#"
    <style>
      @starting-style { #box { text-shadow: none; } }
      #box { width: 100px; height: 100px; text-shadow: 10px 0px 0px rgba(255, 0, 0, 1); transition: text-shadow 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut start = fragment_tree.clone();
  let viewport = start.viewport_size();
  animation::apply_transitions(&mut start, 0.0, viewport);
  assert!(fragment_text_shadows(&start, box_id).is_empty());

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  let shadows = fragment_text_shadows(&mid, box_id);
  assert_eq!(shadows.len(), 1);
  let shadow = &shadows[0];
  assert!((shadow.offset_x.to_px() - 5.0).abs() < 1e-3);
  let color = shadow.color.expect("resolved color");
  assert!((color.a - 0.5).abs() < 1e-6);
}

#[test]
fn transitions_interpolate_filter_from_none() {
  let html = r#"
    <style>
      @starting-style { #box { filter: none; } }
      #box { width: 100px; height: 100px; filter: blur(10px); transition: filter 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut start = fragment_tree.clone();
  let viewport = start.viewport_size();
  animation::apply_transitions(&mut start, 0.0, viewport);
  assert!(fragment_blur_filter(&start, box_id).is_none());

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  let blur = fragment_blur_filter(&mid, box_id).expect("blur filter");
  assert!((blur - 5.0).abs() < 1e-3);
}

#[test]
fn transitions_interpolate_outline_color_over_time() {
  let html = r#"
    <style>
      @starting-style { #box { outline-color: rgb(255, 0, 0); outline-style: solid; outline-width: 4px; } }
      #box { width: 100px; height: 100px; outline-color: rgb(0, 0, 255); outline-style: solid; outline-width: 4px; transition: outline-color 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut start = fragment_tree.clone();
  let viewport = start.viewport_size();
  animation::apply_transitions(&mut start, 0.0, viewport);
  let (color, invert) = fragment_outline_color(&start, box_id);
  assert!(!invert);
  assert_eq!(color, fastrender::Rgba::new(255, 0, 0, 1.0));

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  let (color, invert) = fragment_outline_color(&mid, box_id);
  assert!(!invert);
  assert_eq!(color, fastrender::Rgba::new(128, 0, 128, 1.0));
}

#[test]
fn transition_behavior_gates_visibility_transitions() {
  let html = r#"
    <style>
      @starting-style { #box { visibility: visible; } }
      #box { width: 10px; height: 10px; visibility: hidden; transition: visibility 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 50, 50);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  // Default `transition-behavior: normal` => no transition, so jump to after-change immediately.
  let mut start = fragment_tree.clone();
  let viewport = start.viewport_size();
  animation::apply_transitions(&mut start, 0.0, viewport);
  assert_eq!(fragment_visibility(&start, box_id), Visibility::Hidden);

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 400.0, viewport);
  assert_eq!(fragment_visibility(&mid, box_id), Visibility::Hidden);

  let html_allow = r#"
    <style>
      @starting-style { #box { visibility: visible; } }
      #box { width: 10px; height: 10px; visibility: hidden; transition: visibility 1000ms linear allow-discrete; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html_allow, 50, 50);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut start = fragment_tree.clone();
  let viewport = start.viewport_size();
  animation::apply_transitions(&mut start, 0.0, viewport);
  assert_eq!(fragment_visibility(&start, box_id), Visibility::Visible);

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 400.0, viewport);
  assert_eq!(fragment_visibility(&mid, box_id), Visibility::Visible);

  let mut end = fragment_tree.clone();
  let viewport = end.viewport_size();
  animation::apply_transitions(&mut end, 1000.0, viewport);
  assert_eq!(fragment_visibility(&end, box_id), Visibility::Hidden);
}

#[test]
fn transition_behavior_gates_outline_color_invert_transitions() {
  let html = r#"
    <style>
      @starting-style { #box { outline-color: invert; outline-style: solid; outline-width: 4px; } }
      #box { width: 10px; height: 10px; outline-color: rgb(255, 0, 0); outline-style: solid; outline-width: 4px; transition: outline-color 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 50, 50);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 400.0, viewport);
  let (color, invert) = fragment_outline_color(&mid, box_id);
  assert!(!invert);
  assert_eq!(color, fastrender::Rgba::new(255, 0, 0, 1.0));

  let html_allow = r#"
    <style>
      @starting-style { #box { outline-color: invert; outline-style: solid; outline-width: 4px; } }
      #box { width: 10px; height: 10px; outline-color: rgb(255, 0, 0); outline-style: solid; outline-width: 4px; transition: outline-color 1000ms linear allow-discrete; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html_allow, 50, 50);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut early = fragment_tree.clone();
  let viewport = early.viewport_size();
  animation::apply_transitions(&mut early, 400.0, viewport);
  let (_color, invert) = fragment_outline_color(&early, box_id);
  assert!(invert);

  let mut late = fragment_tree.clone();
  let viewport = late.viewport_size();
  animation::apply_transitions(&mut late, 600.0, viewport);
  let (color, invert) = fragment_outline_color(&late, box_id);
  assert!(!invert);
  assert_eq!(color, fastrender::Rgba::new(255, 0, 0, 1.0));
}

#[test]
fn transitions_interpolate_outline_shorthand_over_time() {
  let html = r#"
    <style>
      @starting-style { #box { outline: 0px solid rgb(255, 0, 0); } }
      #box { width: 100px; height: 100px; outline: 10px solid rgb(0, 0, 255); transition: outline 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  assert!((fragment_outline_width(&mid, box_id) - 5.0).abs() < 1e-3);
  let (color, invert) = fragment_outline_color(&mid, box_id);
  assert!(!invert);
  assert_eq!(color, fastrender::Rgba::new(128, 0, 128, 1.0));
}

#[test]
fn transitions_interpolate_outline_shorthand_style_is_not_animated_without_allow_discrete() {
  let html = r#"
    <style>
      @starting-style { #box { outline: 0px solid rgb(255, 0, 0); } }
      #box { width: 100px; height: 100px; outline: 10px dashed rgb(0, 0, 255); transition: outline 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut start = fragment_tree.clone();
  let viewport = start.viewport_size();
  animation::apply_transitions(&mut start, 0.0, viewport);
  assert!((fragment_outline_width(&start, box_id) - 0.0).abs() < 1e-3);
  assert_eq!(fragment_outline_style(&start, box_id), OutlineStyle::Dashed);
  let (color, invert) = fragment_outline_color(&start, box_id);
  assert!(!invert);
  assert_eq!(color, fastrender::Rgba::new(255, 0, 0, 1.0));

  let mut early = fragment_tree.clone();
  let viewport = early.viewport_size();
  animation::apply_transitions(&mut early, 400.0, viewport);
  assert!((fragment_outline_width(&early, box_id) - 4.0).abs() < 1e-3);
  assert_eq!(fragment_outline_style(&early, box_id), OutlineStyle::Dashed);
  let (color, invert) = fragment_outline_color(&early, box_id);
  assert!(!invert);
  assert_eq!(color, fastrender::Rgba::new(153, 0, 102, 1.0));
}

#[test]
fn transitions_interpolate_outline_shorthand_style_flips_with_allow_discrete() {
  let html = r#"
    <style>
      @starting-style { #box { outline: 0px solid rgb(255, 0, 0); } }
      #box { width: 100px; height: 100px; outline: 10px dashed rgb(0, 0, 255); transition: outline 1000ms linear; transition-behavior: allow-discrete; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut early = fragment_tree.clone();
  let viewport = early.viewport_size();
  animation::apply_transitions(&mut early, 400.0, viewport);
  assert_eq!(fragment_outline_style(&early, box_id), OutlineStyle::Solid);

  let mut late = fragment_tree.clone();
  let viewport = late.viewport_size();
  animation::apply_transitions(&mut late, 600.0, viewport);
  assert_eq!(fragment_outline_style(&late, box_id), OutlineStyle::Dashed);
}

#[test]
fn transitions_interpolate_border_shorthand_over_time_without_allow_discrete() {
  let html = r#"
    <style>
      @starting-style { #box { border: 0px solid rgb(255, 0, 0); } }
      #box { width: 100px; height: 100px; border: 10px dashed rgb(0, 0, 255); transition: border 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut start = fragment_tree.clone();
  let viewport = start.viewport_size();
  animation::apply_transitions(&mut start, 0.0, viewport);
  assert!((fragment_border_top_width(&start, box_id) - 0.0).abs() < 1e-3);
  assert_eq!(fragment_border_top_style(&start, box_id), BorderStyle::Dashed);
  assert_eq!(
    fragment_border_top_color(&start, box_id),
    fastrender::Rgba::new(255, 0, 0, 1.0)
  );

  let mut early = fragment_tree.clone();
  let viewport = early.viewport_size();
  animation::apply_transitions(&mut early, 400.0, viewport);
  assert!((fragment_border_top_width(&early, box_id) - 4.0).abs() < 1e-3);
  assert_eq!(fragment_border_top_style(&early, box_id), BorderStyle::Dashed);
  assert_eq!(
    fragment_border_top_color(&early, box_id),
    fastrender::Rgba::new(153, 0, 102, 1.0)
  );
}

#[test]
fn transitions_interpolate_border_shorthand_over_time_with_allow_discrete() {
  let html = r#"
    <style>
      @starting-style { #box { border: 0px solid rgb(255, 0, 0); } }
      #box { width: 100px; height: 100px; border: 10px dashed rgb(0, 0, 255); transition: border 1000ms linear; transition-behavior: allow-discrete; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut start = fragment_tree.clone();
  let viewport = start.viewport_size();
  animation::apply_transitions(&mut start, 0.0, viewport);
  assert!((fragment_border_top_width(&start, box_id) - 0.0).abs() < 1e-3);
  assert_eq!(
    fragment_border_top_style(&start, box_id),
    BorderStyle::Solid
  );
  assert_eq!(
    fragment_border_top_color(&start, box_id),
    fastrender::Rgba::new(255, 0, 0, 1.0)
  );

  let mut early = fragment_tree.clone();
  let viewport = early.viewport_size();
  animation::apply_transitions(&mut early, 400.0, viewport);
  assert!((fragment_border_top_width(&early, box_id) - 4.0).abs() < 1e-3);
  assert_eq!(
    fragment_border_top_style(&early, box_id),
    BorderStyle::Solid
  );
  assert_eq!(
    fragment_border_top_color(&early, box_id),
    fastrender::Rgba::new(153, 0, 102, 1.0)
  );

  let mut late = fragment_tree.clone();
  let viewport = late.viewport_size();
  animation::apply_transitions(&mut late, 600.0, viewport);
  assert!((fragment_border_top_width(&late, box_id) - 6.0).abs() < 1e-3);
  assert_eq!(
    fragment_border_top_style(&late, box_id),
    BorderStyle::Dashed
  );
  assert_eq!(
    fragment_border_top_color(&late, box_id),
    fastrender::Rgba::new(102, 0, 153, 1.0)
  );
}

#[test]
fn transitions_do_not_start_border_style_transition_without_allow_discrete() {
  let html = r#"
    <style>
      @starting-style { #box { border-style: solid; } }
      #box {
        width: 100px;
        height: 100px;
        border-width: 4px;
        border-color: rgb(0, 0, 0);
        border-style: dashed;
        transition: border-style 1000ms linear;
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
  assert_eq!(fragment_border_top_style(&start, box_id), BorderStyle::Dashed);
  assert!((fragment_border_top_width(&start, box_id) - 4.0).abs() < 1e-3);
  assert_eq!(
    fragment_border_top_color(&start, box_id),
    fastrender::Rgba::new(0, 0, 0, 1.0)
  );
}

#[test]
fn transitions_interpolate_border_style_over_time_with_allow_discrete() {
  let html = r#"
    <style>
      @starting-style { #box { border-style: solid; } }
      #box {
        width: 100px;
        height: 100px;
        border-width: 4px;
        border-color: rgb(0, 0, 0);
        border-style: dashed;
        transition: border-style 1000ms linear;
        transition-behavior: allow-discrete;
      }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut early = fragment_tree.clone();
  let viewport = early.viewport_size();
  animation::apply_transitions(&mut early, 400.0, viewport);
  assert_eq!(fragment_border_top_style(&early, box_id), BorderStyle::Solid);

  let mut late = fragment_tree.clone();
  let viewport = late.viewport_size();
  animation::apply_transitions(&mut late, 600.0, viewport);
  assert_eq!(fragment_border_top_style(&late, box_id), BorderStyle::Dashed);
}

#[test]
fn transition_behavior_blocks_border_style_by_default() {
  let html = r#"
    <style>
      @starting-style { #box { border-style: solid; } }
      #box {
        width: 100px;
        height: 100px;
        border-width: 4px;
        border-color: rgb(0, 0, 0);
        border-style: dashed;
        transition: border-style 1000ms linear;
      }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 400.0, viewport);
  assert_eq!(fragment_border_top_style(&mid, box_id), BorderStyle::Dashed);
  assert!((fragment_border_top_width(&mid, box_id) - 4.0).abs() < 1e-3);
  assert_eq!(
    fragment_border_top_color(&mid, box_id),
    fastrender::Rgba::new(0, 0, 0, 1.0)
  );
}

#[test]
fn transition_behavior_list_repeats_last_value_for_discrete_properties() {
  let html = r#"
    <style>
      @starting-style { #box { border-style: solid; visibility: visible; } }
      #box {
        width: 100px;
        height: 100px;
        border-width: 4px;
        border-color: rgb(0, 0, 0);
        border-style: dashed;
        visibility: hidden;
        transition-property: border-style, visibility;
        transition-duration: 1000ms;
        transition-timing-function: linear;
        transition-behavior: allow-discrete;
      }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut early = fragment_tree.clone();
  let viewport = early.viewport_size();
  animation::apply_transitions(&mut early, 400.0, viewport);
  assert_eq!(fragment_border_top_style(&early, box_id), BorderStyle::Solid);
  assert_eq!(fragment_visibility(&early, box_id), Visibility::Visible);

  let mut late = fragment_tree.clone();
  let viewport = late.viewport_size();
  animation::apply_transitions(&mut late, 600.0, viewport);
  assert_eq!(fragment_border_top_style(&late, box_id), BorderStyle::Dashed);
  assert_eq!(fragment_visibility(&late, box_id), Visibility::Visible);

  let mut end = fragment_tree.clone();
  let viewport = end.viewport_size();
  animation::apply_transitions(&mut end, 1000.0, viewport);
  assert_eq!(fragment_visibility(&end, box_id), Visibility::Hidden);
}

#[test]
fn transition_behavior_list_respects_per_property_indexing() {
  let html = r#"
    <style>
      @starting-style { #box { border-style: solid; visibility: visible; } }
      #box {
        width: 100px;
        height: 100px;
        border-width: 4px;
        border-color: rgb(0, 0, 0);
        border-style: dashed;
        visibility: hidden;
        transition-property: border-style, visibility;
        transition-duration: 1000ms;
        transition-timing-function: linear;
        transition-behavior: allow-discrete, normal;
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
  assert_eq!(fragment_border_top_style(&start, box_id), BorderStyle::Solid);
  assert_eq!(fragment_visibility(&start, box_id), Visibility::Hidden);

  let mut late = fragment_tree.clone();
  let viewport = late.viewport_size();
  animation::apply_transitions(&mut late, 600.0, viewport);
  assert_eq!(fragment_border_top_style(&late, box_id), BorderStyle::Dashed);
  assert_eq!(fragment_visibility(&late, box_id), Visibility::Hidden);
}

#[test]
fn transition_behavior_blocks_untyped_custom_properties_by_default() {
  let html = r#"
    <style>
      @starting-style { #box { --x: 0; } }
      #box {
        width: 100px;
        height: 100px;
        --x: 1;
        transition: --x 1000ms linear;
      }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 400.0, viewport);

  let frag = find_fragment(&mid.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  let value = style
    .custom_properties
    .get("--x")
    .expect("custom property present");
  assert!(value.typed.is_none());
  assert_eq!(value.value.trim(), "1");
}

#[test]
fn transition_behavior_allows_untyped_custom_properties_when_opted_in() {
  let html = r#"
    <style>
      @starting-style { #box { --x: 0; } }
      #box {
        width: 100px;
        height: 100px;
        --x: 1;
        transition: --x 1000ms linear allow-discrete;
      }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut early = fragment_tree.clone();
  let viewport = early.viewport_size();
  animation::apply_transitions(&mut early, 400.0, viewport);
  let frag = find_fragment(&early.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  let value = style
    .custom_properties
    .get("--x")
    .expect("custom property present");
  assert!(value.typed.is_none());
  assert_eq!(value.value.trim(), "0");

  let mut late = fragment_tree.clone();
  let viewport = late.viewport_size();
  animation::apply_transitions(&mut late, 600.0, viewport);
  let frag = find_fragment(&late.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  let value = style
    .custom_properties
    .get("--x")
    .expect("custom property present");
  assert!(value.typed.is_none());
  assert_eq!(value.value.trim(), "1");
}

#[test]
fn transition_property_all_includes_custom_properties() {
  let html = r#"
    <style>
      @starting-style { #box { --x: 0; opacity: 0; } }
      #box {
        width: 100px;
        height: 100px;
        opacity: 1;
        --x: 1;
        transition-property: all;
        transition-duration: 1000ms;
        transition-timing-function: linear;
        transition-behavior: allow-discrete;
      }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut early = fragment_tree.clone();
  let viewport = early.viewport_size();
  animation::apply_transitions(&mut early, 400.0, viewport);
  assert!((fragment_opacity(&early, box_id) - 0.4).abs() < 1e-3);
  let frag = find_fragment(&early.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  let value = style
    .custom_properties
    .get("--x")
    .expect("custom property present");
  assert!(value.typed.is_none());
  assert_eq!(value.value.trim(), "0");

  let mut late = fragment_tree.clone();
  let viewport = late.viewport_size();
  animation::apply_transitions(&mut late, 600.0, viewport);
  assert!((fragment_opacity(&late, box_id) - 0.6).abs() < 1e-3);
  let frag = find_fragment(&late.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  let value = style
    .custom_properties
    .get("--x")
    .expect("custom property present");
  assert!(value.typed.is_none());
  assert_eq!(value.value.trim(), "1");
}

#[test]
fn transitions_interpolate_transform_origin_over_time() {
  let html = r#"
    <style>
      @starting-style { #box { transform-origin: 0% 0%; } }
      #box { width: 200px; height: 100px; transform-origin: 100% 100%; transition: transform-origin 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 300, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut start = fragment_tree.clone();
  let viewport = start.viewport_size();
  animation::apply_transitions(&mut start, 0.0, viewport);
  assert_eq!(fragment_transform_origin(&start, box_id), (0.0, 0.0));

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  let (x, y) = fragment_transform_origin(&mid, box_id);
  assert!((x - 100.0).abs() < 1e-3, "x={x}");
  assert!((y - 50.0).abs() < 1e-3, "y={y}");
}

#[test]
fn transitions_interpolate_perspective_origin_over_time() {
  let html = r#"
    <style>
      @starting-style { #box { perspective-origin: 0% 0%; } }
      #box { width: 200px; height: 100px; perspective-origin: 100% 100%; transition: perspective-origin 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 300, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut start = fragment_tree.clone();
  let viewport = start.viewport_size();
  animation::apply_transitions(&mut start, 0.0, viewport);
  assert_eq!(fragment_perspective_origin(&start, box_id), (0.0, 0.0));

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  let (x, y) = fragment_perspective_origin(&mid, box_id);
  assert!((x - 100.0).abs() < 1e-3, "x={x}");
  assert!((y - 50.0).abs() < 1e-3, "y={y}");
}

#[test]
fn transitions_interpolate_clip_rect_over_time() {
  let html = r#"
    <style>
      @starting-style { #box { clip: rect(0px, 10px, 10px, 0px); } }
      #box { width: 100px; height: 100px; clip: rect(0px, 20px, 20px, 0px); transition: clip 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  let rect = fragment_clip_rect(&mid, box_id).expect("clip rect");
  assert_eq!(rect.top, ClipComponent::Length(fastrender::Length::px(0.0)));
  assert_eq!(
    rect.left,
    ClipComponent::Length(fastrender::Length::px(0.0))
  );
  assert_eq!(
    rect.right,
    ClipComponent::Length(fastrender::Length::px(15.0))
  );
  assert_eq!(
    rect.bottom,
    ClipComponent::Length(fastrender::Length::px(15.0))
  );
}

#[test]
fn transitions_interpolate_mask_position_over_time() {
  let html = r#"
    <style>
      @starting-style { #box { mask-position: 0px 0px; } }
      #box { width: 100px; height: 100px; mask-position: 100px 0px; transition: mask-position 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  assert!((fragment_mask_position_x(&mid, box_id) - 50.0).abs() < 1e-3);
}

#[test]
fn transitions_interpolate_mask_size_over_time() {
  let html = r#"
    <style>
      @starting-style { #box { mask-size: 0px 0px; } }
      #box { width: 100px; height: 100px; mask-size: 100px 50px; transition: mask-size 1000ms linear; }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  let (x, y) = fragment_mask_size(&mid, box_id);
  assert!((x - 50.0).abs() < 1e-3);
  assert!((y - 25.0).abs() < 1e-3);
}

#[test]
fn transition_property_filters_border_corner_radii() {
  let html = r#"
    <style>
      @starting-style { #box { border-top-left-radius: 0px; border-top-right-radius: 0px; } }
      #box {
        width: 100px;
        height: 100px;
        border-top-left-radius: 100px;
        border-top-right-radius: 50px;
        transition-property: border-top-left-radius;
        transition-duration: 1000ms;
        transition-timing-function: linear;
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
  assert!((fragment_border_top_left_radius_x(&start, box_id) - 0.0).abs() < 1e-3);
  // Should not transition (not listed in transition-property).
  assert!((fragment_border_top_right_radius_x(&start, box_id) - 50.0).abs() < 1e-3);

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  assert!((fragment_border_top_left_radius_x(&mid, box_id) - 50.0).abs() < 1e-3);
  assert!((fragment_border_top_right_radius_x(&mid, box_id) - 50.0).abs() < 1e-3);
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
        transition: clip-path 1000ms linear allow-discrete;
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
fn transitions_interpolate_clip_path_polygon_over_time() {
  let html = r#"
    <style>
      @starting-style {
        #box {
          clip-path: polygon(0% 0%, 100% 0%, 100% 100%, 0% 100%);
        }
      }

      #box {
        width: 100px;
        height: 100px;
        clip-path: polygon(0% 0%, 50% 0%, 50% 50%, 0% 50%);
        transition: clip-path 1000ms linear;
      }
    </style>
    <div id="box"></div>
  "#;
  let (box_tree, fragment_tree, styled_tree) = prepare(html, 200, 200);
  let node_id = styled_node_id_by_id(&styled_tree, "box").expect("styled id");
  let box_id = box_id_for_styled(&box_tree.root, node_id).expect("box id");

  let mut mid = fragment_tree.clone();
  let viewport = mid.viewport_size();
  animation::apply_transitions(&mut mid, 500.0, viewport);
  let shape = fragment_clip_shape(&mid, box_id);
  let (fill, points) = match shape {
    BasicShape::Polygon { fill, points } => (fill, points),
    other => panic!("expected polygon clip-path, got {other:?}"),
  };
  assert_eq!(fill, FillRule::NonZero);
  assert_eq!(points.len(), 4);

  let eps = 1e-3;
  assert!((points[0].0.to_px() - 0.0).abs() < eps);
  assert!((points[0].1.to_px() - 0.0).abs() < eps);
  assert!((points[1].0.to_px() - 75.0).abs() < eps);
  assert!((points[1].1.to_px() - 0.0).abs() < eps);
  assert!((points[2].0.to_px() - 75.0).abs() < eps);
  assert!((points[2].1.to_px() - 75.0).abs() < eps);
  assert!((points[3].0.to_px() - 0.0).abs() < eps);
  assert!((points[3].1.to_px() - 75.0).abs() < eps);
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
