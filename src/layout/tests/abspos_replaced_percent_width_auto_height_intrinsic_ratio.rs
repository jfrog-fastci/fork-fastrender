use crate::geometry::{Point, Size};
use crate::{
  AbsoluteLayout, AbsoluteLayoutInput, ContainingBlock, EdgeOffsets, LengthOrAuto, Position,
  PositionedStyle,
};

fn default_style() -> PositionedStyle {
  PositionedStyle {
    border_width: EdgeOffsets::ZERO,
    ..Default::default()
  }
}

#[test]
fn abspos_replaced_height_auto_uses_intrinsic_ratio_when_width_is_not_auto() {
  // Regression test: absolutely positioned replaced elements were using their intrinsic *height*
  // when `height:auto` even when `width` is specified, instead of deriving the used height from the
  // intrinsic aspect ratio.
  let abs = AbsoluteLayout::new();
  let cb = ContainingBlock::viewport(Size::new(300.0, 168.75));

  let mut style = default_style();
  style.position = Position::Absolute;
  style.left = LengthOrAuto::px(0.0);
  style.top = LengthOrAuto::px(0.0);
  style.width = LengthOrAuto::percent(100.0);
  style.height = LengthOrAuto::Auto;

  let mut input = AbsoluteLayoutInput::new(style, Size::new(600.0, 337.0), Point::ZERO);
  input.is_replaced = true;

  let result = abs.layout_absolute(&input, &cb).expect("layout");

  assert!(
    (result.size.width - 300.0).abs() < 0.01,
    "expected width 300px (got {})",
    result.size.width
  );

  let expected_height = 300.0 * 337.0 / 600.0;
  assert!(
    (result.size.height - expected_height).abs() < 0.01,
    "expected height {expected_height}px from intrinsic ratio (got {})",
    result.size.height
  );
}
