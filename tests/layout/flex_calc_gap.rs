use std::sync::Arc;

use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::FlexDirection;
use fastrender::style::values::CalcLength;
use fastrender::style::values::Length;
use fastrender::style::values::LengthUnit;
use fastrender::tree::box_tree::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;

fn assert_approx(actual: f32, expected: f32, epsilon: f32, msg: &str) {
  assert!(
    (actual - expected).abs() <= epsilon,
    "{msg}: expected {expected}, got {actual}"
  );
}

fn fixed_block(width: f32) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = Some(Length::px(width));
  style.height = Some(Length::px(10.0));
  style.width_keyword = None;
  style.height_keyword = None;
  style.flex_grow = 0.0;
  style.flex_shrink = 0.0;
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
}

fn calc_percent_plus_px(percent: f32, px: f32) -> Length {
  let calc = CalcLength::single(LengthUnit::Percent, percent)
    .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
    .expect("calc expression should be representable");
  Length::calc(calc)
}

#[test]
fn flex_column_gap_calc_percentage_resolves_against_container_inner_width() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.grid_column_gap_is_normal = false;
  container_style.grid_column_gap = calc_percent_plus_px(10.0, -5.0);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![fixed_block(40.0), fixed_block(40.0), fixed_block(40.0)],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 3);
  let first = &fragment.children[0];
  let second = &fragment.children[1];

  // For 200px container inline size: 10% = 20px; gap = calc(20px - 5px) = 15px.
  let actual_gap = second.bounds.x() - (first.bounds.x() + first.bounds.width());
  assert_approx(actual_gap, 15.0, 0.5, "gap between first and second flex items");
}

#[test]
fn flex_calc_percentage_gap_with_indefinite_percentage_base_is_zero_for_intrinsic_sizing() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.grid_column_gap_is_normal = false;
  // If `100%` cannot be resolved, the gap must not fall back to `100 - 360 = -260px`.
  container_style.grid_column_gap = calc_percent_plus_px(100.0, -360.0);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![fixed_block(50.0), fixed_block(50.0), fixed_block(50.0)],
  );

  let fc = FlexFormattingContext::new();
  let width = fc
    .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic inline size");

  // With an indefinite percentage base, treat the `calc(%)` gap as 0px.
  assert_approx(width, 150.0, 0.1, "max-content width ignores calc(%) gap");
}

