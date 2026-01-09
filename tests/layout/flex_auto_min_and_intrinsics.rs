use fastrender::geometry::Size;
use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::Overflow;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::ReplacedType;
use std::sync::Arc;

#[test]
fn flex_auto_min_size_replaced_intrinsic_prevents_shrink_for_non_scrollable_overflow() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(50.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.overflow_x = Overflow::Visible;
  child_style.overflow_y = Overflow::Visible;
  child_style.flex_shrink = 1.0;

  let child = BoxNode::new_replaced(
    Arc::new(child_style),
    ReplacedType::Canvas,
    Some(Size::new(100.0, 10.0)),
    None,
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(50.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child = fragment.children.first().expect("child fragment");
  let width = child.bounds.width();
  assert!(
    width >= 99.0,
    "expected flex item to keep its intrinsic replaced width via min-width:auto (got {width})"
  );
}

#[test]
fn flex_auto_min_size_replaced_intrinsic_allows_shrink_for_scrollable_overflow() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(50.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.overflow_x = Overflow::Hidden;
  child_style.overflow_y = Overflow::Hidden;
  child_style.flex_shrink = 1.0;

  let child = BoxNode::new_replaced(
    Arc::new(child_style),
    ReplacedType::Canvas,
    Some(Size::new(100.0, 10.0)),
    None,
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(50.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let child = fragment.children.first().expect("child fragment");
  let width = child.bounds.width();
  let eps = 0.5;
  assert!(
    width <= 50.0 + eps,
    "overflow:hidden should make the flex item a scroll container so it can shrink; expected <= 50.0+{eps}, got {width}"
  );
}

