use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::AlignItems;
use fastrender::style::types::Direction;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::FlexWrap;
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
    FragmentContent::Line { .. }
    | FragmentContent::RunningAnchor { .. }
    | FragmentContent::FootnoteAnchor { .. } => None,
  }
}

#[test]
fn flex_wrap_stretch_does_not_collapse_lines() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.width = Some(Length::px(80.0));
  container_style.width_keyword = None;

  let child1 = BoxNode::new_block(fixed_block(50.0, 10.0), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(fixed_block(50.0, 10.0), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child1, child2],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(80.0))
    .expect("layout succeeds");

  assert!(
    fragment.children.len() == 2,
    "expected two flex items, got {}",
    fragment.children.len()
  );

  let second = &fragment.children[1];
  assert!(
    second.bounds.x().abs() < 1e-3,
    "second item should start a new line at x=0, got {:.2}",
    second.bounds.x()
  );
  assert!(
    second.bounds.y() >= 9.0,
    "second item should wrap onto a new line (y > 0), got {:.2}",
    second.bounds.y()
  );
}

#[test]
fn flex_wrap_center_does_not_collapse_lines() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.align_items = AlignItems::Center;
  container_style.width = Some(Length::px(80.0));
  container_style.width_keyword = None;

  let child1 = BoxNode::new_block(fixed_block(50.0, 10.0), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(fixed_block(50.0, 10.0), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child1, child2],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(80.0))
    .expect("layout succeeds");

  let second = &fragment.children[1];
  assert!(
    second.bounds.x().abs() < 1e-3,
    "second item should start a new line at x=0, got {:.2}",
    second.bounds.x()
  );
  assert!(
    second.bounds.y() >= 9.0,
    "second item should wrap onto a new line (y > 0), got {:.2}",
    second.bounds.y()
  );
}

#[test]
fn flex_wrap_reverse_stacks_lines_in_reverse_order() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::WrapReverse;
  container_style.width = Some(Length::px(80.0));
  container_style.width_keyword = None;

  let mut child1 =
    BoxNode::new_block(fixed_block(50.0, 10.0), FormattingContextType::Block, vec![]);
  child1.id = 1;
  let mut child2 =
    BoxNode::new_block(fixed_block(50.0, 10.0), FormattingContextType::Block, vec![]);
  child2.id = 2;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child1, child2],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(80.0))
    .expect("layout succeeds");

  let mut y_by_id = [None, None];
  for child in fragment.children.iter() {
    match fragment_box_id(child) {
      Some(1) => y_by_id[0] = Some(child.bounds.y()),
      Some(2) => y_by_id[1] = Some(child.bounds.y()),
      _ => {}
    }
  }
  let first_y = y_by_id[0].expect("first child present");
  let second_y = y_by_id[1].expect("second child present");
  assert!(
    first_y > second_y,
    "wrap-reverse should place the first line below the second (y0 > y1), got y0={:.2} y1={:.2}",
    first_y,
    second_y
  );
}

#[test]
fn flex_item_align_self_axes_use_container_direction() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.direction = Direction::Rtl;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(30.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;

  let mut child_style = (*fixed_block(10.0, 10.0)).clone();
  child_style.align_self = Some(AlignItems::Start);
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 30.0))
    .expect("layout succeeds");

  assert!(
    (fragment.children[0].bounds.x() - 90.0).abs() < 1e-3,
    "RTL columns should align start to the right edge (x=90), got {:.2}",
    fragment.children[0].bounds.x()
  );
}

#[test]
fn flex_item_self_start_uses_item_direction() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.direction = Direction::Ltr;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(30.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;

  let mut child_style = (*fixed_block(10.0, 10.0)).clone();
  child_style.align_self = Some(AlignItems::SelfStart);
  child_style.direction = Direction::Rtl;
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 30.0))
    .expect("layout succeeds");

  assert!(
    (fragment.children[0].bounds.x() - 90.0).abs() < 1e-3,
    "self-start should resolve against the item's direction (x=90), got {:.2}",
    fragment.children[0].bounds.x()
  );
}

#[test]
fn flex_align_items_self_start_uses_item_direction() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.align_items = AlignItems::SelfStart;
  container_style.direction = Direction::Ltr;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(30.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;

  let mut child_style = (*fixed_block(10.0, 10.0)).clone();
  child_style.direction = Direction::Rtl;
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 30.0))
    .expect("layout succeeds");

  assert!(
    (fragment.children[0].bounds.x() - 90.0).abs() < 1e-3,
    "align-items:self-start should resolve against the item's direction (x=90), got {:.2}",
    fragment.children[0].bounds.x()
  );
}
