use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::AlignContent;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::FlexWrap;
use fastrender::style::types::WritingMode;
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
  writing_mode: WritingMode,
  flex_direction: FlexDirection,
  flex_wrap: FlexWrap,
  container_width: f32,
  container_height: f32,
  row_gap: f32,
  column_gap: f32,
  item_width: f32,
  item_height: f32,
) -> BoxNode {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = flex_direction;
  container_style.flex_wrap = flex_wrap;
  container_style.align_content = align_content;
  container_style.writing_mode = writing_mode;
  container_style.width = Some(Length::px(container_width));
  container_style.height = Some(Length::px(container_height));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  container_style.grid_row_gap = Length::px(row_gap);
  container_style.grid_column_gap = Length::px(column_gap);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  // In real CSS, writing-mode is inherited. The test harness constructs computed styles manually,
  // so set the child writing-mode explicitly to avoid accidental cross-writing-mode alignment
  // behaviour masking gap/align-content regressions.
  item_style.writing_mode = writing_mode;
  item_style.width = Some(Length::px(item_width));
  item_style.height = Some(Length::px(item_height));
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

  let container = build_multiline_container(
    AlignContent::SpaceEvenly,
    WritingMode::HorizontalTb,
    FlexDirection::Row,
    FlexWrap::Wrap,
    60.0,
    50.0,
    5.0,
    0.0,
    30.0,
    10.0,
  );
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

  let container = build_multiline_container(
    AlignContent::SpaceAround,
    WritingMode::HorizontalTb,
    FlexDirection::Row,
    FlexWrap::Wrap,
    60.0,
    50.0,
    5.0,
    0.0,
    30.0,
    10.0,
  );
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
fn align_content_space_between_respects_row_gap_between_lines() {
  let fc = FlexFormattingContext::new();

  let container = build_multiline_container(
    AlignContent::SpaceBetween,
    WritingMode::HorizontalTb,
    FlexDirection::Row,
    FlexWrap::Wrap,
    60.0,
    55.0,
    5.0,
    0.0,
    30.0,
    10.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(60.0, 55.0))
    .expect("layout succeeds");

  // Container: 55px tall
  // Two flex lines (each 10px), plus a 5px row-gap => used cross size = 25px, free = 30px.
  // align-content: space-between => first line y = 0, second line y = 10 + 5 + 30 = 45.
  let epsilon = 0.6;
  let first_line_y = 0.0;
  let second_line_y = 10.0 + 5.0 + 30.0;

  assert_approx(find_block_child(&fragment, 1).bounds.y(), first_line_y, epsilon, "child1 y");
  assert_approx(find_block_child(&fragment, 2).bounds.y(), first_line_y, epsilon, "child2 y");
  assert_approx(find_block_child(&fragment, 3).bounds.y(), second_line_y, epsilon, "child3 y");
}

#[test]
fn column_gap_affects_main_axis_spacing_not_cross_axis_offsets() {
  let fc = FlexFormattingContext::new();

  // Make the column-gap big enough to observe in x positions, while keeping the items on the
  // same line (30 + 15 + 30 = 75).
  let container = build_multiline_container(
    AlignContent::SpaceEvenly,
    WritingMode::HorizontalTb,
    FlexDirection::Row,
    FlexWrap::Wrap,
    75.0,
    50.0,
    0.0,
    15.0,
    30.0,
    10.0,
  );
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

#[test]
fn vertical_writing_mode_space_evenly_respects_row_gap_between_lines() {
  let fc = FlexFormattingContext::new();

  // In vertical writing-mode, `flex-direction: row` maps the main axis to the physical Y axis.
  // Wrapping therefore creates new *columns* along the physical X axis (the block axis).
  let container = build_multiline_container(
    AlignContent::SpaceEvenly,
    WritingMode::VerticalLr,
    FlexDirection::Row,
    FlexWrap::Wrap,
    50.0,
    60.0,
    5.0,
    0.0,
    10.0,
    30.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(50.0, 60.0))
    .expect("layout succeeds");

  // Container: 50px wide
  // Two flex lines (each 10px), plus a 5px row-gap => used cross size = 25px, free = 25px.
  // align-content: space-evenly => first line offset = free/(lines+1) = 25/3 = 8.333.
  // second line offset = 8.333 + 10 + 5 + 8.333 = 31.666.
  let epsilon = 0.6;
  let first_line_x = 25.0 / 3.0;
  let second_line_x = first_line_x + 10.0 + 5.0 + first_line_x;

  assert_approx(find_block_child(&fragment, 1).bounds.x(), first_line_x, epsilon, "child1 x");
  assert_approx(find_block_child(&fragment, 2).bounds.x(), first_line_x, epsilon, "child2 x");
  assert_approx(find_block_child(&fragment, 3).bounds.x(), second_line_x, epsilon, "child3 x");
}

#[test]
fn row_reverse_space_evenly_respects_row_gap_between_lines() {
  let fc = FlexFormattingContext::new();

  // `flex-direction: row-reverse` flips main-axis direction but should not affect cross-axis line
  // offsets from `align-content`.
  let container = build_multiline_container(
    AlignContent::SpaceEvenly,
    WritingMode::HorizontalTb,
    FlexDirection::RowReverse,
    FlexWrap::Wrap,
    60.0,
    50.0,
    5.0,
    0.0,
    30.0,
    10.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(60.0, 50.0))
    .expect("layout succeeds");

  let epsilon = 0.6;
  let first_line_y = 25.0 / 3.0;
  let second_line_y = first_line_y + 10.0 + 5.0 + first_line_y;

  assert_approx(find_block_child(&fragment, 1).bounds.y(), first_line_y, epsilon, "child1 y");
  assert_approx(find_block_child(&fragment, 2).bounds.y(), first_line_y, epsilon, "child2 y");
  assert_approx(find_block_child(&fragment, 3).bounds.y(), second_line_y, epsilon, "child3 y");
}

#[test]
fn column_reverse_space_evenly_respects_column_gap_between_lines() {
  let fc = FlexFormattingContext::new();

  let container = build_multiline_container(
    AlignContent::SpaceEvenly,
    WritingMode::HorizontalTb,
    FlexDirection::ColumnReverse,
    FlexWrap::Wrap,
    50.0,
    60.0,
    0.0,
    5.0,
    10.0,
    30.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(50.0, 60.0))
    .expect("layout succeeds");

  // Same geometry as `column_direction_space_evenly_respects_column_gap_between_lines`, but with
  // column-reverse: items in each column are stacked from bottom to top, while the cross-axis line
  // offsets should remain the same.
  let epsilon = 0.6;
  let first_line_x = 25.0 / 3.0;
  let second_line_x = first_line_x + 10.0 + 5.0 + first_line_x;

  assert_approx(find_block_child(&fragment, 1).bounds.x(), first_line_x, epsilon, "child1 x");
  assert_approx(find_block_child(&fragment, 2).bounds.x(), first_line_x, epsilon, "child2 x");
  assert_approx(find_block_child(&fragment, 3).bounds.x(), second_line_x, epsilon, "child3 x");

  // Column-reverse stacks items from the bottom edge when the column is exactly filled.
  assert_approx(find_block_child(&fragment, 1).bounds.y(), 30.0, epsilon, "child1 y");
  assert_approx(find_block_child(&fragment, 2).bounds.y(), 0.0, epsilon, "child2 y");
  assert_approx(find_block_child(&fragment, 3).bounds.y(), 30.0, epsilon, "child3 y");
}

#[test]
fn vertical_writing_mode_column_direction_space_evenly_respects_column_gap_between_lines() {
  let fc = FlexFormattingContext::new();

  // In vertical writing-mode, `flex-direction: column` maps the main axis to the physical X axis.
  // Wrapping therefore creates new *rows* along the physical Y axis (the inline axis). The
  // cross-axis gap for line stacking is `column-gap` (inline axis).
  let container = build_multiline_container(
    AlignContent::SpaceEvenly,
    WritingMode::VerticalLr,
    FlexDirection::Column,
    FlexWrap::Wrap,
    60.0,
    50.0,
    0.0,
    5.0,
    30.0,
    10.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(60.0, 50.0))
    .expect("layout succeeds");

  // Container cross size is 50px (physical Y).
  // Two flex lines (each 10px), plus a 5px column-gap => used cross size = 25px, free = 25px.
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
fn vertical_writing_mode_column_direction_row_gap_affects_main_axis_spacing() {
  let fc = FlexFormattingContext::new();

  // `row-gap` follows the block axis. In vertical writing-mode, the block axis is physical X.
  // `flex-direction: column` uses the block axis as its main axis, so row-gap should affect item
  // x positions without changing cross-axis (y) line offsets.
  let container = build_multiline_container(
    AlignContent::SpaceEvenly,
    WritingMode::VerticalLr,
    FlexDirection::Column,
    FlexWrap::Wrap,
    75.0,
    50.0,
    15.0,
    0.0,
    30.0,
    10.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(75.0, 50.0))
    .expect("layout succeeds");

  // With column-gap=0, used cross size is 20px and free space is 30px. For space-evenly:
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

  // Row gap is on the main axis for a column-direction flex container in vertical writing mode.
  assert_approx(child1.bounds.x(), 0.0, 1e-3, "child1 x");
  assert_approx(child2.bounds.x(), 45.0, 1e-3, "child2 x");
  assert_approx(child3.bounds.x(), 0.0, 1e-3, "child3 x");
}

#[test]
fn vertical_writing_mode_column_gap_affects_main_axis_spacing() {
  let fc = FlexFormattingContext::new();

  // `column-gap` follows the inline axis. In vertical writing-mode, the inline axis is physical Y.
  // Ensure it spaces items within a column without affecting the cross-axis (X) line offsets.
  let container = build_multiline_container(
    AlignContent::SpaceEvenly,
    WritingMode::VerticalLr,
    FlexDirection::Row,
    FlexWrap::Wrap,
    50.0,
    75.0,
    0.0,
    15.0,
    10.0,
    30.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(50.0, 75.0))
    .expect("layout succeeds");

  // With row-gap=0, used cross size is 20px and free space is 30px. For space-evenly:
  // first line x = free/(lines+1) = 30/3 = 10px.
  // second line x = 10 + 10 + 0 + 10 = 30px.
  let epsilon = 0.6;
  let first_line_x = 30.0 / 3.0;
  let second_line_x = first_line_x + 10.0 + first_line_x;

  let child1 = find_block_child(&fragment, 1);
  let child2 = find_block_child(&fragment, 2);
  let child3 = find_block_child(&fragment, 3);

  assert_approx(child1.bounds.x(), first_line_x, epsilon, "child1 x");
  assert_approx(child2.bounds.x(), first_line_x, epsilon, "child2 x");
  assert_approx(child3.bounds.x(), second_line_x, epsilon, "child3 x");

  // Column gap is on the main axis for a row-direction flex container in vertical writing mode.
  assert_approx(child1.bounds.y(), 0.0, 1e-3, "child1 y");
  assert_approx(child2.bounds.y(), 45.0, 1e-3, "child2 y");
  assert_approx(child3.bounds.y(), 0.0, 1e-3, "child3 y");
}

#[test]
fn vertical_rl_writing_mode_space_evenly_respects_row_gap_between_lines() {
  let fc = FlexFormattingContext::new();

  // Same scenario as `vertical_writing_mode_space_evenly_respects_row_gap_between_lines`, but in
  // `VerticalRl` the block axis points in the negative physical X direction (cross-start is on the
  // right edge). FastRender mirrors wrapped line placement to emulate that.
  let container = build_multiline_container(
    AlignContent::SpaceEvenly,
    WritingMode::VerticalRl,
    FlexDirection::Row,
    FlexWrap::Wrap,
    50.0,
    60.0,
    5.0,
    0.0,
    10.0,
    30.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(50.0, 60.0))
    .expect("layout succeeds");

  // Container: 50px wide
  // Two flex lines (each 10px), plus a 5px row-gap => used cross size = 25px, free = 25px.
  // align-content: space-evenly => first line offset = free/(lines+1) = 25/3 = 8.333.
  // second line offset = 8.333 + 10 + 5 + 8.333 = 31.666.
  //
  // In `VerticalRl`, these offsets are measured from the right edge (block-start), so the physical
  // x positions are mirrored within the 50px container.
  let epsilon = 0.6;
  let first_line_from_left = 25.0 / 3.0;
  let second_line_from_left = first_line_from_left + 10.0 + 5.0 + first_line_from_left;
  let first_line_x = 50.0 - 10.0 - first_line_from_left;
  let second_line_x = 50.0 - 10.0 - second_line_from_left;

  assert_approx(find_block_child(&fragment, 1).bounds.x(), first_line_x, epsilon, "child1 x");
  assert_approx(find_block_child(&fragment, 2).bounds.x(), first_line_x, epsilon, "child2 x");
  assert_approx(find_block_child(&fragment, 3).bounds.x(), second_line_x, epsilon, "child3 x");
}

#[test]
fn vertical_rl_writing_mode_align_content_start_packs_lines_against_cross_start() {
  let fc = FlexFormattingContext::new();

  // In `VerticalRl`, the cross-start edge for a row-direction flex container is on the physical
  // right edge. `align-content: start` should pack the line box against that edge, leaving all
  // remaining free space on the left.
  let container = build_multiline_container(
    AlignContent::Start,
    WritingMode::VerticalRl,
    FlexDirection::Row,
    FlexWrap::Wrap,
    50.0,
    60.0,
    5.0,
    0.0,
    10.0,
    30.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(50.0, 60.0))
    .expect("layout succeeds");

  // Two lines (10px wide) plus 5px row-gap => total cross span 25px.
  // Packed against cross-start (right): line 1 at x=40, line 2 at x=25.
  let epsilon = 0.6;
  assert_approx(find_block_child(&fragment, 1).bounds.x(), 40.0, epsilon, "child1 x");
  assert_approx(find_block_child(&fragment, 2).bounds.x(), 40.0, epsilon, "child2 x");
  assert_approx(find_block_child(&fragment, 3).bounds.x(), 25.0, epsilon, "child3 x");
}

#[test]
fn vertical_rl_writing_mode_align_content_end_packs_lines_against_cross_end() {
  let fc = FlexFormattingContext::new();

  // In `VerticalRl`, the cross-start edge for a row-direction flex container is on the physical
  // right edge. `align-content: end` should instead pack the line box against the cross-end edge
  // on the physical left, leaving all remaining free space on the right.
  let container = build_multiline_container(
    AlignContent::End,
    WritingMode::VerticalRl,
    FlexDirection::Row,
    FlexWrap::Wrap,
    50.0,
    60.0,
    5.0,
    0.0,
    10.0,
    30.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(50.0, 60.0))
    .expect("layout succeeds");

  // Two lines (10px wide) plus 5px row-gap => total cross span 25px.
  // Packed against cross-end (left): second line at x=0, first line at x=15.
  let epsilon = 0.6;
  assert_approx(find_block_child(&fragment, 1).bounds.x(), 15.0, epsilon, "child1 x");
  assert_approx(find_block_child(&fragment, 2).bounds.x(), 15.0, epsilon, "child2 x");
  assert_approx(find_block_child(&fragment, 3).bounds.x(), 0.0, epsilon, "child3 x");
}

#[test]
fn vertical_lr_writing_mode_align_content_start_packs_lines_against_cross_start() {
  let fc = FlexFormattingContext::new();

  // In `VerticalLr`, the cross-start edge for a row-direction flex container is on the physical
  // left edge. `align-content: start` should pack the line box against that edge, leaving all
  // remaining free space on the right.
  let container = build_multiline_container(
    AlignContent::Start,
    WritingMode::VerticalLr,
    FlexDirection::Row,
    FlexWrap::Wrap,
    50.0,
    60.0,
    5.0,
    0.0,
    10.0,
    30.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(50.0, 60.0))
    .expect("layout succeeds");

  // Two lines (10px wide) plus 5px row-gap => total cross span 25px.
  // Packed against cross-start (left): first line at x=0, second line at x=15.
  let epsilon = 0.6;
  assert_approx(find_block_child(&fragment, 1).bounds.x(), 0.0, epsilon, "child1 x");
  assert_approx(find_block_child(&fragment, 2).bounds.x(), 0.0, epsilon, "child2 x");
  assert_approx(find_block_child(&fragment, 3).bounds.x(), 15.0, epsilon, "child3 x");
}

#[test]
fn vertical_lr_writing_mode_align_content_end_packs_lines_against_cross_end() {
  let fc = FlexFormattingContext::new();

  // In `VerticalLr`, the cross-start edge for a row-direction flex container is on the physical
  // left edge. `align-content: end` should instead pack the line box against the cross-end edge on
  // the physical right, leaving all remaining free space on the left.
  let container = build_multiline_container(
    AlignContent::End,
    WritingMode::VerticalLr,
    FlexDirection::Row,
    FlexWrap::Wrap,
    50.0,
    60.0,
    5.0,
    0.0,
    10.0,
    30.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(50.0, 60.0))
    .expect("layout succeeds");

  // Two lines (10px wide) plus 5px row-gap => total cross span 25px.
  // Packed against cross-end (right): first line at x=25, second line at x=40.
  let epsilon = 0.6;
  assert_approx(find_block_child(&fragment, 1).bounds.x(), 25.0, epsilon, "child1 x");
  assert_approx(find_block_child(&fragment, 2).bounds.x(), 25.0, epsilon, "child2 x");
  assert_approx(find_block_child(&fragment, 3).bounds.x(), 40.0, epsilon, "child3 x");
}

#[test]
fn space_evenly_respects_padding_and_row_gap_between_lines() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.align_content = AlignContent::SpaceEvenly;
  container_style.writing_mode = WritingMode::HorizontalTb;
  container_style.width = Some(Length::px(60.0));
  container_style.height = Some(Length::px(60.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  container_style.padding_top = Length::px(4.0);
  container_style.padding_bottom = Length::px(6.0);
  container_style.grid_row_gap = Length::px(5.0);
  container_style.grid_column_gap = Length::px(0.0);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.writing_mode = WritingMode::HorizontalTb;
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

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child1, child2, child3],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(60.0, 60.0))
    .expect("layout succeeds");

  // With the default `box-sizing: content-box`, `height: 60px` sets the content box height.
  // Two flex lines (each 10px), plus a 5px row-gap => used cross size = 25px, free = 35px.
  // align-content: space-evenly => first line offset = free/(lines+1) = 35/3 = 11.666.
  // second line offset = 11.666 + 10 + 5 + 11.666 = 38.333 (from the content box start).
  let epsilon = 0.6;
  let content_start = 4.0;
  let first_line_offset = 35.0 / 3.0;
  let second_line_offset = first_line_offset + 10.0 + 5.0 + first_line_offset;

  assert_approx(
    find_block_child(&fragment, 1).bounds.y(),
    content_start + first_line_offset,
    epsilon,
    "child1 y",
  );
  assert_approx(
    find_block_child(&fragment, 2).bounds.y(),
    content_start + first_line_offset,
    epsilon,
    "child2 y",
  );
  assert_approx(
    find_block_child(&fragment, 3).bounds.y(),
    content_start + second_line_offset,
    epsilon,
    "child3 y",
  );
}

#[test]
fn wrap_reverse_space_evenly_respects_row_gap_between_lines() {
  let fc = FlexFormattingContext::new();

  let container = build_multiline_container(
    AlignContent::SpaceEvenly,
    WritingMode::HorizontalTb,
    FlexDirection::Row,
    FlexWrap::WrapReverse,
    60.0,
    50.0,
    5.0,
    0.0,
    30.0,
    10.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(60.0, 50.0))
    .expect("layout succeeds");

  // Same geometry as `align_content_space_evenly_respects_row_gap_between_lines`, but with
  // wrap-reverse: the cross-axis stacking order of the lines is flipped.
  let epsilon = 0.6;
  let first_line_y = 25.0 / 3.0;
  let second_line_y = first_line_y + 10.0 + 5.0 + first_line_y;

  assert_approx(find_block_child(&fragment, 1).bounds.y(), second_line_y, epsilon, "child1 y");
  assert_approx(find_block_child(&fragment, 2).bounds.y(), second_line_y, epsilon, "child2 y");
  assert_approx(find_block_child(&fragment, 3).bounds.y(), first_line_y, epsilon, "child3 y");
}

#[test]
fn wrap_reverse_space_around_respects_row_gap_between_lines() {
  let fc = FlexFormattingContext::new();

  let container = build_multiline_container(
    AlignContent::SpaceAround,
    WritingMode::HorizontalTb,
    FlexDirection::Row,
    FlexWrap::WrapReverse,
    60.0,
    50.0,
    5.0,
    0.0,
    30.0,
    10.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(60.0, 50.0))
    .expect("layout succeeds");

  // Same geometry as `align_content_space_around_respects_row_gap_between_lines`, but with
  // wrap-reverse: the cross-axis stacking order of the lines is flipped.
  let epsilon = 0.6;
  let space_per_line = 25.0 / 2.0;
  let first_line_y = space_per_line / 2.0;
  let second_line_y = first_line_y + 10.0 + 5.0 + space_per_line;

  assert_approx(find_block_child(&fragment, 1).bounds.y(), second_line_y, epsilon, "child1 y");
  assert_approx(find_block_child(&fragment, 2).bounds.y(), second_line_y, epsilon, "child2 y");
  assert_approx(find_block_child(&fragment, 3).bounds.y(), first_line_y, epsilon, "child3 y");
}

#[test]
fn wrap_reverse_space_between_respects_row_gap_between_lines() {
  let fc = FlexFormattingContext::new();

  let container = build_multiline_container(
    AlignContent::SpaceBetween,
    WritingMode::HorizontalTb,
    FlexDirection::Row,
    FlexWrap::WrapReverse,
    60.0,
    55.0,
    5.0,
    0.0,
    30.0,
    10.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(60.0, 55.0))
    .expect("layout succeeds");

  // Container: 55px tall
  // Two flex lines (each 10px), plus a 5px row-gap => used cross size = 25px, free = 30px.
  // align-content: space-between => distribute the free space between lines, in addition to the
  // row-gap. Under wrap-reverse, the first line is placed at cross-start (bottom).
  //
  // First line (with items 1 & 2) at y = 55 - 10 = 45.
  // Second line at y = 45 - (10 + 5 + 30) = 0.
  let epsilon = 0.6;
  let bottom_line_y = 55.0 - 10.0;
  let top_line_y = 0.0;

  assert_approx(find_block_child(&fragment, 1).bounds.y(), bottom_line_y, epsilon, "child1 y");
  assert_approx(find_block_child(&fragment, 2).bounds.y(), bottom_line_y, epsilon, "child2 y");
  assert_approx(find_block_child(&fragment, 3).bounds.y(), top_line_y, epsilon, "child3 y");
}

#[test]
fn column_direction_space_evenly_respects_column_gap_between_lines() {
  let fc = FlexFormattingContext::new();

  // flex-direction: column + wrap creates multiple columns along the cross axis; the cross-axis
  // gap is `column-gap` (inline axis).
  let container = build_multiline_container(
    AlignContent::SpaceEvenly,
    WritingMode::HorizontalTb,
    FlexDirection::Column,
    FlexWrap::Wrap,
    50.0,
    60.0,
    0.0,
    5.0,
    10.0,
    30.0,
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(50.0, 60.0))
    .expect("layout succeeds");

  // Container: 50px wide
  // Two flex lines/columns (each 10px), plus a 5px column-gap => used cross size = 25px, free = 25px.
  // align-content: space-evenly => first line offset = free/(lines+1) = 25/3 = 8.333.
  // second line offset = 8.333 + 10 + 5 + 8.333 = 31.666.
  let epsilon = 0.6;
  let first_line_x = 25.0 / 3.0;
  let second_line_x = first_line_x + 10.0 + 5.0 + first_line_x;

  assert_approx(find_block_child(&fragment, 1).bounds.x(), first_line_x, epsilon, "child1 x");
  assert_approx(find_block_child(&fragment, 2).bounds.x(), first_line_x, epsilon, "child2 x");
  assert_approx(find_block_child(&fragment, 3).bounds.x(), second_line_x, epsilon, "child3 x");

  // Wrap should start the new column at y=0; only the cross-axis x offsets change with
  // `align-content`.
  assert_approx(find_block_child(&fragment, 1).bounds.y(), 0.0, epsilon, "child1 y");
  assert_approx(find_block_child(&fragment, 2).bounds.y(), 30.0, epsilon, "child2 y");
  assert_approx(find_block_child(&fragment, 3).bounds.y(), 0.0, epsilon, "child3 y");
}
