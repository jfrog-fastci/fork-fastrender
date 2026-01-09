use std::sync::Arc;

use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::FlexDirection;
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;

fn fixed_block(width: f32) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = Some(Length::px(width));
  style.height = Some(Length::px(10.0));
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
}

#[test]
fn flex_intrinsic_inline_size_includes_column_gap_between_items() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.grid_column_gap = Length::px(8.0);
  container_style.grid_column_gap_is_normal = false;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![fixed_block(10.0), fixed_block(20.0), fixed_block(30.0)],
  );

  let fc = FlexFormattingContext::new();
  let width = fc
    .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic inline size");

  // Flexbox intrinsic sizing considers the sum of item contributions plus the column gaps between
  // them (2 gaps at 8px each).
  assert!((width - 76.0).abs() < 0.01, "expected width≈76, got {width}");
}

