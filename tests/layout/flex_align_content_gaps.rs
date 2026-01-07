use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::AlignContent;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::FlexWrap;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use fastrender::FragmentNode;
use std::sync::Arc;

fn assert_approx(actual: f32, expected: f32, epsilon: f32, msg: &str) {
  assert!(
    (actual - expected).abs() <= epsilon,
    "{msg}: expected {expected}, got {actual}"
  );
}

fn find_block_child<'a>(fragment: &'a FragmentNode, box_id: usize) -> &'a FragmentNode {
  fragment
    .children
    .iter()
    .find(|child| child.box_id() == Some(box_id))
    .unwrap_or_else(|| {
      panic!(
        "missing fragment for box_id={box_id}; got children ids={:?}",
        fragment.children.iter().map(|c| c.box_id()).collect::<Vec<_>>()
      )
    })
}

fn build_multiline_container(
  align_content: AlignContent,
  container_width: f32,
  container_height: f32,
  row_gap: f32,
  column_gap: f32,
) -> BoxNode {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.align_content = align_content;
  container_style.width = Some(Length::px(container_width));
  container_style.height = Some(Length::px(container_height));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  container_style.grid_row_gap = Length::px(row_gap);
  container_style.grid_column_gap = Length::px(column_gap);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.width = Some(Length::px(30.0));
  item_style.height = Some(Length::px(10.0));
  item_style.width_keyword = None;
  item_style.height_keyword = None;
  item_style.flex_shrink = 0.0;
  let item_style = Arc::new(item_style);

  let mut child1 = BoxNode::new_block(item_style.clone(), FormattingContextType::Block, vec![]);
  child1.id = 1;
  let mut child2 = BoxNode::new_block(item_style.clone(), FormattingContextType::Block, vec![]);
  child2.id = 2;
  let mut child3 = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);
  child3.id = 3;

  BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child1, child2, child3],
  )
}

#[test]
fn align_content_space_evenly_respects_row_gap_between_lines() {
  let fc = FlexFormattingContext::new();

  let container =
    build_multiline_container(AlignContent::SpaceEvenly, 60.0, 50.0, 5.0, 0.0);
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(60.0, 50.0))
    .expect("layout succeeds");

  // Container: 50px tall
  // Two flex lines (each 10px), plus a 5px row-gap => used cross size = 25px, free = 25px.
  // align-content: space-evenly => first line offset = free/(lines+1) = 25/3 = 8.333.
  // second line offset = 8.333 + 10 + 5 + 8.333 = 31.666.
  let epsilon = 0.6;
  let first_line_y = 25.0 / 3.0;
  let second_line_y = first_line_y + 10.0 + 5.0 + first_line_y;

  assert_approx(find_block_child(&fragment, 1).bounds.y(), first_line_y, epsilon, "child1 y");
  assert_approx(find_block_child(&fragment, 2).bounds.y(), first_line_y, epsilon, "child2 y");
  assert_approx(find_block_child(&fragment, 3).bounds.y(), second_line_y, epsilon, "child3 y");
}

#[test]
fn align_content_space_around_respects_row_gap_between_lines() {
  let fc = FlexFormattingContext::new();

  let container =
    build_multiline_container(AlignContent::SpaceAround, 60.0, 50.0, 5.0, 0.0);
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(60.0, 50.0))
    .expect("layout succeeds");

  // Container: 50px tall
  // Two flex lines (each 10px), plus a 5px row-gap => used cross size = 25px, free = 25px.
  // align-content: space-around => space per line = free/lines = 12.5px.
  // First line offset = 12.5/2 = 6.25.
  // Second line offset = 6.25 + 10 + 5 + 12.5 = 33.75.
  let epsilon = 0.6;
  let space_per_line = 25.0 / 2.0;
  let first_line_y = space_per_line / 2.0;
  let second_line_y = first_line_y + 10.0 + 5.0 + space_per_line;

  assert_approx(find_block_child(&fragment, 1).bounds.y(), first_line_y, epsilon, "child1 y");
  assert_approx(find_block_child(&fragment, 2).bounds.y(), first_line_y, epsilon, "child2 y");
  assert_approx(find_block_child(&fragment, 3).bounds.y(), second_line_y, epsilon, "child3 y");
}

#[test]
fn column_gap_affects_main_axis_spacing_not_cross_axis_offsets() {
  let fc = FlexFormattingContext::new();

  // Make the column-gap big enough to observe in x positions, while keeping the items on the
  // same line (30 + 15 + 30 = 75).
  let container =
    build_multiline_container(AlignContent::SpaceEvenly, 75.0, 50.0, 0.0, 15.0);
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(75.0, 50.0))
    .expect("layout succeeds");

  // With row-gap=0, used cross size is 20px and free space is 30px. For space-evenly:
  // first line y = free/(lines+1) = 30/3 = 10px.
  // second line y = 10 + 10 + 0 + 10 = 30px.
  let epsilon = 0.6;
  let first_line_y = 30.0 / 3.0;
  let second_line_y = first_line_y + 10.0 + first_line_y;

  let child1 = find_block_child(&fragment, 1);
  let child2 = find_block_child(&fragment, 2);
  let child3 = find_block_child(&fragment, 3);

  assert_approx(child1.bounds.y(), first_line_y, epsilon, "child1 y");
  assert_approx(child2.bounds.y(), first_line_y, epsilon, "child2 y");
  assert_approx(child3.bounds.y(), second_line_y, epsilon, "child3 y");

  // Column gap is on the main axis for a row-direction flex container.
  assert_approx(child1.bounds.x(), 0.0, 1e-3, "child1 x");
  assert_approx(child2.bounds.x(), 45.0, 1e-3, "child2 x");
  assert_approx(child3.bounds.x(), 0.0, 1e-3, "child3 x");
}

