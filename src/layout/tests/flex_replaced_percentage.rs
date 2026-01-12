use fastrender::geometry::Size;
use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::FlexDirection;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::ReplacedType;
use std::sync::Arc;

#[test]
fn flex_replaced_percentage_width_resolves_against_container_not_viewport() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::percent(100.0));

  let child = BoxNode::new_replaced(
    Arc::new(child_style),
    ReplacedType::Canvas,
    Some(Size::new(100.0, 50.0)),
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
      &LayoutConstraints::new(AvailableSpace::Definite(500.0), AvailableSpace::Indefinite)
        .with_used_border_box_size(Some(200.0), None),
    )
    .expect("layout should succeed");

  let child = fragment.children.first().expect("child fragment");
  let width = child.bounds.width();
  let eps = 0.5;
  assert!(
    (width - 200.0).abs() <= eps,
    "expected replaced flex item to resolve 100% against the container width (200px), got {width}"
  );
}
