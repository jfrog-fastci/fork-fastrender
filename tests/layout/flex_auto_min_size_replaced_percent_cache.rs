use fastrender::geometry::Size;
use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::AspectRatio;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::Overflow;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::ReplacedType;
use std::sync::Arc;

#[test]
fn flex_auto_min_size_replaced_percentage_aspect_ratio_caches_by_container_size() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::percent(100.0));
  child_style.height = Some(Length::percent(100.0));
  child_style.aspect_ratio = AspectRatio::Ratio(16.0 / 9.0);
  child_style.overflow_x = Overflow::Clip;
  child_style.overflow_y = Overflow::Clip;

  // Use a non-zero id so flex auto min-size memoization is active (it is keyed by box id).
  let mut child = BoxNode::new_replaced(
    Arc::new(child_style),
    ReplacedType::Canvas,
    Some(Size::new(1104.0, 620.0)),
    None,
  );
  child.id = 1;

  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );
  container.id = 2;

  let fc = FlexFormattingContext::new();

  // First layout: wide container causes the transferred size suggestion to exceed the intrinsic
  // height, so the auto min-size resolves to the intrinsic height (~620px).
  fc.layout(
    &container,
    &LayoutConstraints::new(
      AvailableSpace::Definite(2000.0),
      AvailableSpace::Definite(2000.0),
    )
    .with_used_border_box_size(Some(1600.0), None),
  )
  .expect("wide layout should succeed");

  // Second layout: narrower container should allow the replaced element to shrink well below its
  // intrinsic height based on aspect-ratio.
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(
        AvailableSpace::Definite(500.0),
        AvailableSpace::Definite(500.0),
      )
      .with_used_border_box_size(Some(200.0), None),
    )
    .expect("narrow layout should succeed");

  let child = fragment.children.first().expect("child fragment");
  let height = child.bounds.height();
  let expected_height = 200.0 / (16.0 / 9.0);
  let eps = 0.5;
  assert!(
    (height - expected_height).abs() <= eps,
    "expected replaced flex item to size via aspect-ratio (~{}px), got {height}",
    expected_height
  );
}

