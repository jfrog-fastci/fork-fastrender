use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::FlexWrap;
use fastrender::style::types::JustifyContent;
use fastrender::style::values::Length;
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
  // Ensure overflow comes from the authored size (no shrink-to-fit).
  style.flex_shrink = 0.0;
  Arc::new(style)
}

fn find_child<'a>(
  fragment: &'a fastrender::FragmentNode,
  box_id: usize,
) -> &'a fastrender::FragmentNode {
  fragment
    .children
    .iter()
    .find(|child| child.box_id() == Some(box_id))
    .unwrap_or_else(|| panic!("missing fragment for box_id={box_id}"))
}

#[test]
fn flex_wrap_allows_single_oversized_item_without_clamping() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;

  let mut child = BoxNode::new_block(
    fixed_block(200.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  child.id = 2;

  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );
  container.id = 1;

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(100.0))
    .expect("layout succeeds");

  let child_fragment = find_child(&fragment, 2);
  assert!(
    (child_fragment.bounds.width() - 200.0).abs() < 0.1,
    "expected child width≈200px (overflow allowed), got {:.2}",
    child_fragment.bounds.width()
  );
  assert!(
    child_fragment.bounds.x().abs() < 0.1,
    "expected child x≈0px, got {:.2}",
    child_fragment.bounds.x()
  );
  assert!(
    child_fragment.bounds.y().abs() < 0.1,
    "expected child y≈0px, got {:.2}",
    child_fragment.bounds.y()
  );
}

#[test]
fn flex_wrap_allows_oversized_first_item_and_wraps_second_item() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;

  let mut first = BoxNode::new_block(
    fixed_block(150.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  first.id = 2;
  let mut second = BoxNode::new_block(
    fixed_block(50.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  second.id = 3;

  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![first, second],
  );
  container.id = 1;

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(100.0))
    .expect("layout succeeds");

  let first_fragment = find_child(&fragment, 2);
  let second_fragment = find_child(&fragment, 3);

  assert!(
    (first_fragment.bounds.width() - 150.0).abs() < 0.1,
    "expected first child width≈150px (overflow allowed), got {:.2}",
    first_fragment.bounds.width()
  );
  assert!(
    (second_fragment.bounds.width() - 50.0).abs() < 0.1,
    "expected second child width≈50px, got {:.2}",
    second_fragment.bounds.width()
  );
  assert!(
    first_fragment.bounds.x().abs() < 0.1,
    "expected first child x≈0px, got {:.2}",
    first_fragment.bounds.x()
  );
  assert!(
    first_fragment.bounds.y().abs() < 0.1,
    "expected first child y≈0px, got {:.2}",
    first_fragment.bounds.y()
  );
  assert!(
    second_fragment.bounds.x().abs() < 0.1,
    "expected second child to wrap to the start edge (x≈0px), got {:.2}",
    second_fragment.bounds.x()
  );
  assert!(
    second_fragment.bounds.y() >= 10.0 - 0.6,
    "expected second child to wrap to the next line (y>=10px), got {:.2}",
    second_fragment.bounds.y()
  );
}

#[test]
fn flex_wrap_preserves_negative_justify_content_offset_for_single_oversized_item() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.justify_content = JustifyContent::Center;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;

  let mut child = BoxNode::new_block(
    fixed_block(200.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  child.id = 2;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(100.0))
    .expect("layout succeeds");

  let child_fragment = find_child(&fragment, 2);
  assert!(
    (child_fragment.bounds.width() - 200.0).abs() < 0.1,
    "expected child width≈200px (overflow allowed), got {:.2}",
    child_fragment.bounds.width()
  );
  assert!(
    (child_fragment.bounds.x() + 50.0).abs() < 0.8,
    "expected justify-content:center to offset oversized child by ~-50px, got x={:.2}",
    child_fragment.bounds.x()
  );
}

#[test]
fn flex_wrap_preserves_large_negative_justify_content_offset_for_single_oversized_item() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.justify_content = JustifyContent::Center;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;

  let mut child = BoxNode::new_block(
    fixed_block(500.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  child.id = 2;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(100.0))
    .expect("layout succeeds");

  let child_fragment = find_child(&fragment, 2);
  assert!(
    (child_fragment.bounds.width() - 500.0).abs() < 0.1,
    "expected child width≈500px (overflow allowed), got {:.2}",
    child_fragment.bounds.width()
  );
  assert!(
    (child_fragment.bounds.x() + 200.0).abs() < 1.0,
    "expected justify-content:center to offset oversized child by ~-200px, got x={:.2}",
    child_fragment.bounds.x()
  );
}
