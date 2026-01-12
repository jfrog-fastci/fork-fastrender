use crate::layout::constraints::AvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::values::CalcLength;
use crate::style::values::Length;
use crate::style::values::LengthUnit;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

fn calc_percent_plus_px(percent: f32, px: f32) -> Length {
  let calc = CalcLength::single(LengthUnit::Percent, percent)
    .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
    .expect("calc expression should be representable");
  Length::calc(calc)
}

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

fn inter_item_gap_px(fragment: &crate::tree::fragment_tree::FragmentNode) -> f32 {
  assert!(
    fragment.children.len() >= 2,
    "expected >= 2 flex item fragments, got {}",
    fragment.children.len()
  );
  let first = &fragment.children[0];
  let second = &fragment.children[1];
  second.bounds.x() - (first.bounds.x() + first.bounds.width())
}

#[test]
fn flex_calc_gap_is_resolved_per_layout_after_template_cache() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.grid_column_gap_is_normal = false;
  container_style.grid_column_gap = calc_percent_plus_px(10.0, -5.0); // calc(10% - 5px)

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.flex_grow = 0.0;
  child_style.flex_shrink = 0.0;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));

  let child_a = BoxNode::new_block(
    Arc::new(child_style.clone()),
    FormattingContextType::Block,
    vec![],
  );
  let child_b = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child_a, child_b],
  );

  // Reuse the same formatting context so the style→Taffy template cache is exercised.
  let fc = FlexFormattingContext::new();

  let fragment_200 = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");
  assert_approx(
    inter_item_gap_px(&fragment_200),
    15.0,
    "gap should resolve against 200px base",
  );

  let fragment_400 = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(400.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");
  assert_approx(
    inter_item_gap_px(&fragment_400),
    35.0,
    "gap should resolve against 400px base",
  );
}
