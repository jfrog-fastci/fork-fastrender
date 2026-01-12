use std::sync::Arc;

use crate::layout::contexts::flex::FlexFormattingContext;
use crate::layout::formatting_context::IntrinsicSizingMode;
use crate::style::display::{Display, FormattingContextType};
use crate::style::types::{FlexDirection, WritingMode};
use crate::style::values::Length;
use crate::tree::box_tree::BoxNode;
use crate::{ComputedStyle, FormattingContext};

fn fixed_block(height: f32, writing_mode: WritingMode) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.writing_mode = writing_mode;
  // Ensure the block has a definite size in both physical axes so intrinsic sizing stays trivial.
  style.width = Some(Length::px(10.0));
  style.height = Some(Length::px(height));
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
}

#[test]
fn flex_intrinsic_gap_vertical_writing_mode_uses_column_gap() {
  // Regression: For intrinsic inline-size calculations, flex containers must account for the
  // inline-axis gap using `column-gap` even in vertical writing modes (where the inline axis is
  // physical Y). Previously we incorrectly swapped to `row-gap`, which ignored authored column-gap
  // values and under-measured the container.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::Row;
  container_style.grid_column_gap = Length::px(8.0);
  container_style.grid_column_gap_is_normal = false;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![
      fixed_block(10.0, WritingMode::VerticalLr),
      fixed_block(20.0, WritingMode::VerticalLr),
      fixed_block(30.0, WritingMode::VerticalLr),
    ],
  );

  let fc = FlexFormattingContext::new();
  let inline = fc
    .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic inline size");

  // In `writing-mode: vertical-lr`, the container's inline axis is physical height.
  // Sum of heights (10 + 20 + 30) + 2 gaps at 8px each.
  assert!(
    (inline - 76.0).abs() < 0.01,
    "expected inline-size≈76, got {inline}"
  );
}
