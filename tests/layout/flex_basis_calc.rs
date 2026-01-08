use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::FlexBasis;
use fastrender::style::values::CalcLength;
use fastrender::style::values::Length;
use fastrender::style::values::LengthUnit;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_basis_calc_resolves_against_container_inner_main_size() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.padding_left = Length::px(10.0);
  container_style.padding_right = Length::px(10.0);

  let calc = |percent: f32, px: f32| -> Length {
    let calc = CalcLength::single(LengthUnit::Percent, percent)
      .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
      .expect("calc expression should be representable");
    Length::calc(calc)
  };

  let make_child = |basis: Length| -> BoxNode {
    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.flex_grow = 0.0;
    child_style.flex_shrink = 0.0;
    child_style.flex_basis = FlexBasis::Length(basis);
    BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![])
  };

  let child_a = make_child(calc(66.6, -8.0));
  let child_b = make_child(calc(33.3, -12.0));

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child_a, child_b],
  );

  let container_width = 1000.0;
  let inner_width = container_width - 20.0;
  let expected_a = inner_width * (66.6 / 100.0) - 8.0;
  let expected_b = inner_width * (33.3 / 100.0) - 12.0;

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(container_width), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let a = fragment.children.get(0).expect("child A fragment");
  let b = fragment.children.get(1).expect("child B fragment");
  assert!(
    (a.bounds.width() - expected_a).abs() < 1.0,
    "expected first item width ≈ {expected_a}, got {}",
    a.bounds.width()
  );
  assert!(
    (b.bounds.width() - expected_b).abs() < 1.0,
    "expected second item width ≈ {expected_b}, got {}",
    b.bounds.width()
  );
}

