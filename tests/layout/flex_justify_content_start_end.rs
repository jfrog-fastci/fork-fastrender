use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::{Direction, FlexDirection, JustifyContent, WritingMode};
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;

fn layout_child_x(justify_content: JustifyContent) -> f32 {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = justify_content;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(20.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let root = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(100.0, 20.0))
    .expect("layout succeeds");

  let child_fragment = fragment
    .children
    .iter()
    .find(|fragment| {
      matches!(
        fragment.content,
        FragmentContent::Block { box_id: Some(box_id) }
          | FragmentContent::Inline { box_id: Some(box_id), .. }
          | FragmentContent::Text { box_id: Some(box_id), .. }
          | FragmentContent::Replaced { box_id: Some(box_id), .. }
          if box_id == 1
      )
    })
    .unwrap_or_else(|| panic!("missing child fragment: {fragment:#?}"));

  child_fragment.bounds.x()
}

fn layout_child_x_rtl_row(justify_content: JustifyContent) -> f32 {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.justify_content = justify_content;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(20.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let root = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(100.0, 20.0))
    .expect("layout succeeds");

  let child_fragment = fragment
    .children
    .iter()
    .find(|fragment| {
      matches!(
        fragment.content,
        FragmentContent::Block { box_id: Some(box_id) }
          | FragmentContent::Inline { box_id: Some(box_id), .. }
          | FragmentContent::Text { box_id: Some(box_id), .. }
          | FragmentContent::Replaced { box_id: Some(box_id), .. }
          if box_id == 1
      )
    })
    .unwrap_or_else(|| panic!("missing child fragment: {fragment:#?}"));

  child_fragment.bounds.x()
}

fn layout_child_y_vertical_rl_row_reverse(justify_content: JustifyContent) -> f32 {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = justify_content;
  container_style.width = Some(Length::px(20.0));
  container_style.height = Some(Length::px(100.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let root = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(20.0, 100.0))
    .expect("layout succeeds");

  let child_fragment = fragment
    .children
    .iter()
    .find(|fragment| {
      matches!(
        fragment.content,
        FragmentContent::Block { box_id: Some(box_id) }
          | FragmentContent::Inline { box_id: Some(box_id), .. }
          | FragmentContent::Text { box_id: Some(box_id), .. }
          | FragmentContent::Replaced { box_id: Some(box_id), .. }
          if box_id == 1
      )
    })
    .unwrap_or_else(|| panic!("missing child fragment: {fragment:#?}"));

  child_fragment.bounds.y()
}

fn layout_child_x_vertical_rl_column_reverse(justify_content: JustifyContent) -> f32 {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = justify_content;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(20.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let root = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(100.0, 20.0))
    .expect("layout succeeds");

  let child_fragment = fragment
    .children
    .iter()
    .find(|fragment| {
      matches!(
        fragment.content,
        FragmentContent::Block { box_id: Some(box_id) }
          | FragmentContent::Inline { box_id: Some(box_id), .. }
          | FragmentContent::Text { box_id: Some(box_id), .. }
          | FragmentContent::Replaced { box_id: Some(box_id), .. }
          if box_id == 1
      )
    })
    .unwrap_or_else(|| panic!("missing child fragment: {fragment:#?}"));

  child_fragment.bounds.x()
}

#[test]
fn justify_content_start_end_are_distinct_from_flex_start_end() {
  let flex_start_x = layout_child_x(JustifyContent::FlexStart);
  let start_x = layout_child_x(JustifyContent::Start);
  assert!(
    (flex_start_x - 90.0).abs() < 1e-3,
    "justify-content:flex-start in row-reverse should align to the main-start edge (right), got {flex_start_x}"
  );
  assert!(
    (start_x - 0.0).abs() < 1e-3,
    "justify-content:start in row-reverse should align to the container start edge (left), got {start_x}"
  );

  let flex_end_x = layout_child_x(JustifyContent::FlexEnd);
  let end_x = layout_child_x(JustifyContent::End);
  assert!(
    (flex_end_x - 0.0).abs() < 1e-3,
    "justify-content:flex-end in row-reverse should align to the main-end edge (left), got {flex_end_x}"
  );
  assert!(
    (end_x - 90.0).abs() < 1e-3,
    "justify-content:end in row-reverse should align to the container end edge (right), got {end_x}"
  );
}

#[test]
fn justify_content_start_end_in_rtl_row_align_to_inline_edges() {
  let start_x = layout_child_x_rtl_row(JustifyContent::Start);
  assert!(
    (start_x - 90.0).abs() < 1e-3,
    "justify-content:start in rtl row should align to inline-start edge (right), got {start_x}"
  );

  let end_x = layout_child_x_rtl_row(JustifyContent::End);
  assert!(
    (end_x - 0.0).abs() < 1e-3,
    "justify-content:end in rtl row should align to inline-end edge (left), got {end_x}"
  );
}

#[test]
fn justify_content_start_end_vertical_rl_row_reverse_are_distinct_from_flex_start_end() {
  let flex_start_y = layout_child_y_vertical_rl_row_reverse(JustifyContent::FlexStart);
  let start_y = layout_child_y_vertical_rl_row_reverse(JustifyContent::Start);
  assert!(
    (flex_start_y - 90.0).abs() < 1e-3,
    "justify-content:flex-start in vertical-rl row-reverse should align to the main-start edge (bottom), got {flex_start_y}"
  );
  assert!(
    (start_y - 0.0).abs() < 1e-3,
    "justify-content:start in vertical-rl row-reverse should align to inline-start edge (top), got {start_y}"
  );

  let flex_end_y = layout_child_y_vertical_rl_row_reverse(JustifyContent::FlexEnd);
  let end_y = layout_child_y_vertical_rl_row_reverse(JustifyContent::End);
  assert!(
    (flex_end_y - 0.0).abs() < 1e-3,
    "justify-content:flex-end in vertical-rl row-reverse should align to the main-end edge (top), got {flex_end_y}"
  );
  assert!(
    (end_y - 90.0).abs() < 1e-3,
    "justify-content:end in vertical-rl row-reverse should align to inline-end edge (bottom), got {end_y}"
  );
}

#[test]
fn justify_content_start_end_vertical_rl_column_reverse_are_distinct_from_flex_start_end() {
  let flex_start_x = layout_child_x_vertical_rl_column_reverse(JustifyContent::FlexStart);
  let start_x = layout_child_x_vertical_rl_column_reverse(JustifyContent::Start);
  assert!(
    (flex_start_x - 0.0).abs() < 1e-3,
    "justify-content:flex-start in vertical-rl column-reverse should align to the main-start edge (left), got {flex_start_x}"
  );
  assert!(
    (start_x - 90.0).abs() < 1e-3,
    "justify-content:start in vertical-rl column-reverse should align to block-start edge (right), got {start_x}"
  );

  let flex_end_x = layout_child_x_vertical_rl_column_reverse(JustifyContent::FlexEnd);
  let end_x = layout_child_x_vertical_rl_column_reverse(JustifyContent::End);
  assert!(
    (flex_end_x - 90.0).abs() < 1e-3,
    "justify-content:flex-end in vertical-rl column-reverse should align to the main-end edge (right), got {flex_end_x}"
  );
  assert!(
    (end_x - 0.0).abs() < 1e-3,
    "justify-content:end in vertical-rl column-reverse should align to block-end edge (left), got {end_x}"
  );
}
