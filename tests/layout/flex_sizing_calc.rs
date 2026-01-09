use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::values::CalcLength;
use fastrender::style::values::Length;
use fastrender::style::values::LengthUnit;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn calc_percent_plus_px(percent: f32, px: f32) -> Length {
  let calc = CalcLength::single(LengthUnit::Percent, percent)
    .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
    .expect("calc expression should be representable");
  Length::calc(calc)
}

#[test]
fn flex_item_width_calc_percent_resolves_against_container_inner_width() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.flex_grow = 0.0;
  child_style.flex_shrink = 0.0;
  child_style.width = Some(calc_percent_plus_px(50.0, 10.0));

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![],
    )],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child_fragment = fragment.children.get(0).expect("child fragment");
  assert!(
    (child_fragment.bounds.width() - 110.0).abs() < 0.5,
    "expected child width ≈ 110px, got {}",
    child_fragment.bounds.width()
  );
}

#[test]
fn flex_item_max_width_calc_percent_clamps_border_box_width() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.flex_grow = 0.0;
  child_style.flex_shrink = 0.0;
  child_style.width = Some(Length::px(200.0));
  child_style.max_width = Some(calc_percent_plus_px(50.0, 10.0));

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![],
    )],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child_fragment = fragment.children.get(0).expect("child fragment");
  assert!(
    (child_fragment.bounds.width() - 110.0).abs() < 0.5,
    "expected child width clamped to ≈ 110px, got {}",
    child_fragment.bounds.width()
  );
}

