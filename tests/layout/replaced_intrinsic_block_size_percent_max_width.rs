use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::Display;
use fastrender::style::position::Position;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::{BoxNode, ReplacedType, SvgContent};
use fastrender::Size;
use std::sync::Arc;

fn assert_approx_eq(actual: f32, expected: f32) {
  assert!(
    (actual - expected).abs() <= 0.5,
    "got {actual}, expected {expected}"
  );
}

#[test]
fn intrinsic_block_size_does_not_clamp_replaced_to_zero_for_percent_max_width() {
  // `FormattingContext::compute_intrinsic_block_size` measures block-size by laying out the box with
  // an intrinsic inline constraint. When the containing block inline size is indefinite, percentage
  // width constraints (like `max-width: 100%`) behave as `auto` per CSS sizing rules; they should
  // not resolve against a 0px base and collapse the replaced element to 0x0.
  let fc = BlockFormattingContext::new();

  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.position = Position::Relative;
  style.width = Some(Length::px(200.0));
  style.max_width = Some(Length::percent(100.0));
  style.height = None;

  let box_node = BoxNode::new_replaced(
    Arc::new(style),
    ReplacedType::Svg {
      content: SvgContent::raw("<svg viewBox=\"0 0 2 1\"></svg>"),
    },
    Some(Size::new(300.0, 150.0)),
    Some(2.0),
  );

  let block = fc
    .compute_intrinsic_block_size(&box_node, IntrinsicSizingMode::MinContent)
    .expect("intrinsic block size");

  // 200px wide at a 2:1 aspect ratio => 100px tall.
  assert_approx_eq(block, 100.0);
}
