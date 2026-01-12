use std::sync::Arc;

use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{FlexDirection, WritingMode};
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::{ComputedStyle, FormattingContext};

fn horizontal_block(width: f32, height: f32) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  // Default writing mode is horizontal-tb.
  style.width = Some(Length::px(width));
  style.height = Some(Length::px(height));
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
}

#[test]
fn flex_intrinsic_mixed_writing_mode_uses_child_physical_axis() {
  // Regression: The flex container's intrinsic *inline* size must aggregate child contributions
  // along the *physical axis* that corresponds to the container's inline axis. If a child has a
  // different writing mode, its inline axis may not match and we need to query the child's
  // intrinsic block-size instead.
  //
  // Here, the container is vertical-lr (inline axis = physical Y) while the children are the
  // default horizontal-tb (inline axis = physical X). The container's intrinsic inline size should
  // therefore sum the children's *heights*, not their widths.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::Row;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![horizontal_block(100.0, 10.0), horizontal_block(200.0, 20.0)],
  );

  let fc = FlexFormattingContext::new();
  let inline = fc
    .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic inline size");

  assert!(
    (inline - 30.0).abs() < 0.01,
    "expected inline-size≈30, got {inline}"
  );
}
