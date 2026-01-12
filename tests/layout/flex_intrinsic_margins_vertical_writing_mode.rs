use std::sync::Arc;

use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{FlexDirection, WritingMode};
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::{ComputedStyle, FormattingContext};

fn fixed_block_with_vertical_margins(
  height: f32,
  margin_top: f32,
  margin_bottom: f32,
  writing_mode: WritingMode,
) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.writing_mode = writing_mode;
  style.width = Some(Length::px(10.0));
  style.height = Some(Length::px(height));
  style.margin_top = Some(Length::px(margin_top));
  style.margin_bottom = Some(Length::px(margin_bottom));
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
}

#[test]
fn flex_intrinsic_margins_vertical_writing_mode_include_top_bottom() {
  // Regression: In vertical writing modes the inline axis is physical Y, so margins that live on
  // the physical top/bottom edges of the flex items must contribute to the container's intrinsic
  // inline size. Previously we always summed left/right margins, which ignored these.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::Row;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![
      fixed_block_with_vertical_margins(10.0, 1.0, 2.0, WritingMode::VerticalLr),
      fixed_block_with_vertical_margins(20.0, 3.0, 4.0, WritingMode::VerticalLr),
      fixed_block_with_vertical_margins(30.0, 5.0, 6.0, WritingMode::VerticalLr),
    ],
  );

  let fc = FlexFormattingContext::new();
  let inline = fc
    .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic inline size");

  // Sum of (height + margin-top + margin-bottom) across all items:
  // (10 + 1 + 2) + (20 + 3 + 4) + (30 + 5 + 6) = 81
  assert!(
    (inline - 81.0).abs() < 0.01,
    "expected inline-size≈81, got {inline}"
  );
}
