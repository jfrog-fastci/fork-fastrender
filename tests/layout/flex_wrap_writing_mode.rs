use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::AlignContent;
use fastrender::style::types::AlignItems;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::FlexWrap;
use fastrender::style::types::JustifyContent;
use fastrender::style::types::WritingMode;
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn fixed_block(width: f32, height: f32) -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = Some(Length::px(width));
  style.height = Some(Length::px(height));
  style.width_keyword = None;
  style.height_keyword = None;
  // Avoid flexing so line breaks are driven by the authored main sizes.
  style.flex_shrink = 0.0;
  Arc::new(style)
}

fn fragment_box_id(fragment: &fastrender::FragmentNode) -> Option<usize> {
  match &fragment.content {
    FragmentContent::Block { box_id }
    | FragmentContent::Inline { box_id, .. }
    | FragmentContent::Replaced { box_id, .. }
    | FragmentContent::Text { box_id, .. } => *box_id,
    FragmentContent::Line { .. } | FragmentContent::RunningAnchor { .. } => None,
  }
}

#[test]
fn flex_wrap_vertical_rl_stacks_lines_from_right_to_left() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row; // inline axis (vertical in vertical-rl)
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(25.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;

  let mut child1 = BoxNode::new_block(fixed_block(10.0, 20.0), FormattingContextType::Block, vec![]);
  child1.id = 1;
  let mut child2 = BoxNode::new_block(fixed_block(10.0, 20.0), FormattingContextType::Block, vec![]);
  child2.id = 2;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child1, child2],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 25.0))
    .expect("layout succeeds");

  let mut x_by_id = [None, None];
  for child in fragment.children.iter() {
    match fragment_box_id(child) {
      Some(1) => {
        x_by_id[0] = Some(child.bounds.x());
      }
      Some(2) => {
        x_by_id[1] = Some(child.bounds.x());
      }
      _ => {}
    }
  }
  let first_x = x_by_id[0].expect("first child present");
  let second_x = x_by_id[1].expect("second child present");

  assert!(
    (first_x - 90.0).abs() < 1e-3,
    "first flex line should start at the block-start (right) edge in vertical-rl, got x={first_x:.2}"
  );
  assert!(
    (second_x - 80.0).abs() < 1e-3,
    "second flex line should stack to the left of the first in vertical-rl, got x={second_x:.2}"
  );
}

#[test]
fn flex_wrap_reverse_vertical_rl_stacks_lines_left_to_right() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row; // inline axis (vertical in vertical-rl)
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.align_content = AlignContent::FlexStart;
  container_style.align_items = AlignItems::FlexStart;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(25.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;

  let mut child1 = BoxNode::new_block(fixed_block(10.0, 20.0), FormattingContextType::Block, vec![]);
  child1.id = 1;
  let mut child2 = BoxNode::new_block(fixed_block(10.0, 20.0), FormattingContextType::Block, vec![]);
  child2.id = 2;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child1, child2],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 25.0))
    .expect("layout succeeds");

  let mut x_by_id = [None, None];
  for child in fragment.children.iter() {
    match fragment_box_id(child) {
      Some(1) => x_by_id[0] = Some(child.bounds.x()),
      Some(2) => x_by_id[1] = Some(child.bounds.x()),
      _ => {}
    }
  }

  let first_x = x_by_id[0].expect("first child present");
  let second_x = x_by_id[1].expect("second child present");

  assert!(
    (first_x - 0.0).abs() < 1e-3,
    "wrap-reverse should swap cross-start to the left edge in vertical-rl, got x={first_x:.2}"
  );
  assert!(
    (second_x - 10.0).abs() < 1e-3,
    "wrap-reverse should stack subsequent lines to the right, got x={second_x:.2}"
  );
}
