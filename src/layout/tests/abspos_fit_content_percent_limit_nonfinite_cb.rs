use crate::geometry::{Point, Rect, Size};
use crate::style::types::IntrinsicSizeKeyword;
use crate::{
  AbsoluteLayout, AbsoluteLayoutInput, ContainingBlock, EdgeOffsets, Length, LengthOrAuto, Position,
  PositionedStyle,
};

fn default_style() -> PositionedStyle {
  PositionedStyle {
    border_width: EdgeOffsets::ZERO,
    ..Default::default()
  }
}

#[test]
fn abspos_fit_content_percent_limit_with_nonfinite_containing_block_does_not_panic() {
  // Regression test for `AbsoluteLayout` robustness: some intrinsic sizing keyword paths used to
  // contain `.unwrap()` calls that assumed percentage resolution succeeded. If the containing block
  // size is non-finite, percentage bases may be unavailable (treated as `auto`), and the abspos
  // algorithm must not panic.
  let abs = AbsoluteLayout::new();

  let mut style = default_style();
  style.position = Position::Absolute;
  style.left = LengthOrAuto::px(10.0);
  style.right = LengthOrAuto::px(10.0);
  style.width = LengthOrAuto::Auto;
  style.width_keyword = Some(IntrinsicSizeKeyword::FitContent {
    limit: Some(Length::percent(50.0)),
  });
  style.top = LengthOrAuto::px(0.0);
  style.height = LengthOrAuto::px(10.0);

  let input = AbsoluteLayoutInput::new(style, Size::new(0.0, 10.0), Point::ZERO);

  // The viewport size remains finite (for vw/vh units), but the containing block inline size is
  // non-finite so percentage bases for intrinsic keyword limits may be unavailable.
  let cb_rect = Rect::new(Point::ZERO, Size::new(f32::NAN, 100.0));
  let cb = ContainingBlock::with_viewport_and_bases(cb_rect, Size::new(200.0, 100.0), None, None);

  let result = abs.layout_absolute(&input, &cb).unwrap();
  assert!(result.position.x.is_finite(), "position.x should be finite");
  assert!(result.position.y.is_finite(), "position.y should be finite");
  assert!(result.size.width.is_finite(), "width should be finite");
  assert!(result.size.height.is_finite(), "height should be finite");
}
