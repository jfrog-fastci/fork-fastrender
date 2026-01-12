use fastrender::geometry::{Point, Size};
use fastrender::{
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
fn abspos_replaced_auto_height_between_insets_shrinks_to_fit() {
  // Regression test: absolutely positioned *replaced* elements should not fill the available block
  // size when `top`/`bottom` are specified and `height:auto` (CSS 2.1 §10.6.5). Instead they use
  // their intrinsic size, similar to the horizontal shrink-to-fit behavior in §10.3.8.
  let abs = AbsoluteLayout::new();
  let cb = ContainingBlock::viewport(Size::new(300.0, 200.0));

  let mut style = default_style();
  style.position = Position::Absolute;
  style.top = LengthOrAuto::px(10.0);
  style.bottom = LengthOrAuto::px(20.0);
  style.height = LengthOrAuto::Auto;

  let mut input = AbsoluteLayoutInput::new(style, Size::new(80.0, 120.0), Point::ZERO);
  input.is_replaced = true;

  let result = abs.layout_absolute(&input, &cb).unwrap();
  assert!(
    (result.size.height - 120.0).abs() < 0.001,
    "expected replaced abspos height:auto to shrink-to-fit intrinsic height (got {})",
    result.size.height
  );
  assert!(
    (result.position.y - 10.0).abs() < 0.001,
    "expected top inset to remain at 10px (got {})",
    result.position.y
  );
}

#[test]
fn abspos_replaced_between_insets_resolves_auto_margin_top() {
  // When both `top` and `bottom` are specified and the used height is definite (intrinsic for
  // replaced elements), `margin-top:auto` participates in the constraint equation and is solved
  // to keep the bottom inset satisfied (CSS 2.1 §10.6.5).
  let abs = AbsoluteLayout::new();
  let cb = ContainingBlock::viewport(Size::new(300.0, 200.0));

  let mut style = default_style();
  style.position = Position::Absolute;
  style.top = LengthOrAuto::px(10.0);
  style.bottom = LengthOrAuto::px(20.0);
  style.height = LengthOrAuto::Auto;
  style.margin_top_auto = true;

  let mut input = AbsoluteLayoutInput::new(style, Size::new(80.0, 120.0), Point::ZERO);
  input.is_replaced = true;

  let result = abs.layout_absolute(&input, &cb).unwrap();
  assert!(
    (result.size.height - 120.0).abs() < 0.001,
    "expected intrinsic height (got {})",
    result.size.height
  );
  assert!(
    (result.margins.top - 50.0).abs() < 0.001,
    "expected margin-top:auto to resolve to remaining space (got {})",
    result.margins.top
  );
  assert!(
    (result.position.y - 60.0).abs() < 0.001,
    "expected y = top + resolved margin-top (got {})",
    result.position.y
  );
}
