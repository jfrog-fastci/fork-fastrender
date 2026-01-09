use fastrender::geometry::Size;
use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::AspectRatio;
use fastrender::style::types::FlexDirection;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::ReplacedType;
use std::sync::Arc;

#[test]
fn flex_replaced_percentage_height_is_auto_when_container_height_is_indefinite() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::percent(100.0));
  child_style.height = Some(Length::percent(100.0));
  child_style.aspect_ratio = AspectRatio::Ratio(16.0 / 9.0);

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
      // The container has a definite available height, but an auto used height. Percent heights on
      // descendants must behave like `auto` (CSS2.1 §10.6.2), allowing `aspect-ratio` to determine
      // the replaced element's used height.
      &LayoutConstraints::new(AvailableSpace::Definite(500.0), AvailableSpace::Definite(500.0))
        .with_used_border_box_size(Some(200.0), None),
    )
    .expect("layout should succeed");

  let child = fragment.children.first().expect("child fragment");
  let width = child.bounds.width();
  let height = child.bounds.height();
  let eps = 0.5;
  assert!(
    (width - 200.0).abs() <= eps,
    "expected replaced flex item width to be 100% of the 200px container width, got {width}"
  );

  let expected_height = 200.0 / (16.0 / 9.0);
  assert!(
    (height - expected_height).abs() <= eps,
    "expected replaced flex item height to follow aspect-ratio ({}px), got {height}",
    expected_height
  );
}

