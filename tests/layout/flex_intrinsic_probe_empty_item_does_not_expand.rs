use std::sync::Arc;

use fastrender::geometry::Size;
use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::AlignItems;
use fastrender::style::types::FlexDirection;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;

#[test]
fn flex_intrinsic_probe_empty_item_does_not_expand() {
  let fc = FlexFormattingContext::with_viewport(Size::new(400.0, 200.0));

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.align_items = AlignItems::Baseline;
  container_style.width = Some(Length::px(200.0));
  container_style.width_keyword = None;

  let mut left_style = ComputedStyle::default();
  left_style.display = Display::Block;
  left_style.width = Some(Length::px(100.0));
  left_style.height = Some(Length::px(10.0));
  left_style.width_keyword = None;
  left_style.height_keyword = None;
  left_style.flex_shrink = 0.0;
  let mut left = BoxNode::new_block(Arc::new(left_style), FormattingContextType::Block, vec![]);
  left.id = 1;

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.width_keyword = None;
  abs_style.height_keyword = None;
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 3;

  let mut empty_style = ComputedStyle::default();
  empty_style.display = Display::Block;
  empty_style.height = Some(Length::px(10.0));
  empty_style.height_keyword = None;
  empty_style.width_keyword = None;
  empty_style.flex_shrink = 0.0;
  let mut empty = BoxNode::new_block(
    Arc::new(empty_style),
    FormattingContextType::Block,
    vec![abs_child],
  );
  empty.id = 2;

  let mut right_style = ComputedStyle::default();
  right_style.display = Display::Block;
  right_style.width = Some(Length::px(40.0));
  right_style.height = Some(Length::px(10.0));
  right_style.width_keyword = None;
  right_style.height_keyword = None;
  right_style.flex_shrink = 0.0;
  let mut right = BoxNode::new_block(Arc::new(right_style), FormattingContextType::Block, vec![]);
  right.id = 4;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![left, empty, right],
  );

  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let eps = 0.5;
  let container_width = fragment.bounds.width();
  assert!(
    (container_width - 200.0).abs() <= eps,
    "expected container width 200px, got {container_width}"
  );

  let empty_fragment = fragment.children.get(1).expect("empty fragment");
  let empty_width = empty_fragment.bounds.width();
  assert!(
    empty_width <= eps,
    "expected empty flex item to measure ~0px wide, got {empty_width}"
  );

  let right_fragment = fragment.children.get(2).expect("right fragment");
  let right_edge = right_fragment.bounds.x() + right_fragment.bounds.width();
  assert!(
    right_edge <= container_width + eps,
    "expected trailing flex item to stay within container, right edge {right_edge} > container {container_width}"
  );
}

