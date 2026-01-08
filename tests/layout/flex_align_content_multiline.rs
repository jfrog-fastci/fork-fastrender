use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::AlignContent;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::FlexWrap;
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn layout_with_align_content(align_content: AlignContent) -> (f32, f32) {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.align_content = align_content;
  container_style.width = Some(Length::px(60.0));
  container_style.height = Some(Length::px(50.0));

  let mut children = Vec::new();
  for id in 1..=3 {
    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.width = Some(Length::px(30.0));
    child_style.height = Some(Length::px(10.0));
    child_style.flex_shrink = 0.0;
    let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    child.id = id;
    children.push(child);
  }

  let last_id = children[2].id;
  let first_id = children[0].id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    children,
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(60.0, 50.0))
    .expect("layout succeeds");

  let mut first_y = None;
  let mut last_y = None;
  let mut debug_children = Vec::new();
  for child in fragment.children.iter() {
    let id = match &child.content {
      FragmentContent::Block { box_id }
      | FragmentContent::Inline { box_id, .. }
      | FragmentContent::Replaced { box_id, .. }
      | FragmentContent::Text { box_id, .. } => *box_id,
      FragmentContent::Line { .. }
      | FragmentContent::RunningAnchor { .. }
      | FragmentContent::FootnoteAnchor { .. } => None,
    };
    debug_children.push((
      id,
      child.bounds.x(),
      child.bounds.y(),
      child.bounds.width(),
      child.bounds.height(),
    ));
    match id {
      Some(id) if id == first_id => first_y = Some(child.bounds.y()),
      Some(id) if id == last_id => last_y = Some(child.bounds.y()),
      _ => {}
    }
  }

  let first_y = first_y.unwrap_or_else(|| panic!("first child present: {:?}", debug_children));
  let last_y = last_y.unwrap_or_else(|| panic!("last child present: {:?}", debug_children));

  (first_y, last_y)
}

#[test]
fn align_content_center_distributes_lines() {
  let (first_y, last_y) = layout_with_align_content(AlignContent::Center);
  let eps = 1e-3;
  assert!(
    (first_y - 15.0).abs() < eps,
    "first line should start at y=15, got {first_y}"
  );
  assert!(
    (last_y - 25.0).abs() < eps,
    "second line should start at y=25, got {last_y}"
  );
}

#[test]
fn align_content_space_between_distributes_lines() {
  let (first_y, last_y) = layout_with_align_content(AlignContent::SpaceBetween);
  let eps = 1e-3;
  assert!(
    (first_y - 0.0).abs() < eps,
    "first line should start at y=0, got {first_y}"
  );
  assert!(
    (last_y - 40.0).abs() < eps,
    "second line should start at y=40, got {last_y}"
  );
}

#[test]
fn align_content_flex_end_distributes_lines() {
  let (first_y, last_y) = layout_with_align_content(AlignContent::FlexEnd);
  let eps = 1e-3;
  assert!(
    (first_y - 30.0).abs() < eps,
    "first line should start at y=30, got {first_y}"
  );
  assert!(
    (last_y - 40.0).abs() < eps,
    "second line should start at y=40, got {last_y}"
  );
}

#[test]
fn align_content_stretch_stretches_line_cross_sizes() {
  let (first_y, last_y) = layout_with_align_content(AlignContent::Stretch);
  let eps = 1e-3;
  // Container height: 50px.
  // Two flex lines with 10px cross size each => free cross space = 30px.
  // align-content: stretch distributes the free space by increasing each line's cross size:
  // (10+15)=25px per line, so the second line starts at y=25.
  assert!(
    (first_y - 0.0).abs() < eps,
    "first line should start at y=0, got {first_y}"
  );
  assert!(
    (last_y - 25.0).abs() < eps,
    "second line should start at y=25 under stretch, got {last_y}"
  );
}
