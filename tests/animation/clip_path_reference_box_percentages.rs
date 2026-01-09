use fastrender::animation as fr_animation;
use fastrender::api::{FastRender, RenderOptions};
use fastrender::style::cascade::StyledNode;
use fastrender::style::types::{BasicShape, ClipPath, ShapeRadius};
use fastrender::tree::box_tree::{BoxNode, BoxTree};
use fastrender::tree::fragment_tree::{FragmentNode, FragmentTree};

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

fn box_id_for_styled(node: &BoxNode, styled_id: usize) -> Option<usize> {
  if node.styled_node_id == Some(styled_id) {
    return Some(node.id);
  }
  for child in &node.children {
    if let Some(id) = box_id_for_styled(child, styled_id) {
      return Some(id);
    }
  }
  None
}

fn find_fragment<'a>(node: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
  if node.box_id() == Some(box_id) {
    return Some(node);
  }
  for child in node.children() {
    if let Some(found) = find_fragment(child, box_id) {
      return Some(found);
    }
  }
  if let fastrender::tree::fragment_tree::FragmentContent::RunningAnchor { snapshot, .. } =
    &node.content
  {
    return find_fragment(snapshot, box_id);
  }
  None
}

#[test]
fn clip_path_reference_boxes_affect_percentage_sampling() {
  let html = r#"
    <style>
      html, body { margin: 0; }
      @starting-style { #box { clip-path: circle(0%) content-box; } }
      #box {
        width: 100px;
        height: 100px;
        padding: 10px;
        border: 10px solid black;
        clip-path: circle(50%) content-box;
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
  fr_animation::apply_transitions(&mut mid, 500.0, viewport);

  let frag = find_fragment(&mid.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  let ClipPath::BasicShape(shape, _) = &style.clip_path else {
    panic!("expected basic-shape clip-path, got {:?}", style.clip_path);
  };
  let BasicShape::Circle { radius, .. } = shape.as_ref() else {
    panic!("expected circle clip-path, got {:?}", shape);
  };
  let ShapeRadius::Length(len) = radius else {
    panic!("expected length circle radius, got {:?}", radius);
  };

  // `circle(50%) content-box` resolves percentages against the content box size. With a 100px
  // content box, the end radius is 50px. Mid-transition should land at 25px.
  let eps = 1e-3;
  assert!((len.to_px() - 25.0).abs() < eps, "radius_px={}", len.to_px());
}

