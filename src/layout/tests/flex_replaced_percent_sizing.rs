use crate::geometry::Size;
use crate::layout::constraints::{AvailableSpace, LayoutConstraints};
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::{Display, FormattingContextType};
use crate::style::types::{AspectRatio, BoxSizing, FlexDirection};
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::{BoxNode, ReplacedType};
use std::sync::Arc;

#[test]
fn flex_replaced_percent_width_resolves_against_container_content_width() {
  // Regression for Fortune cards: percentage-sized replaced elements (e.g. `img { width/max-width:
  // 100% }`) inside a column flex container must resolve percentages against the flex container's
  // *content box*, not the viewport.

  let viewport = Size::new(1000.0, 800.0);
  let fc = FlexFormattingContext::with_viewport(viewport);

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.box_sizing = BoxSizing::BorderBox;
  container_style.width = Some(Length::px(200.0));
  container_style.padding_left = Length::px(10.0);
  container_style.padding_right = Length::px(10.0);

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::percent(100.0));
  child_style.max_width = Some(Length::percent(100.0));
  // Ensure the image's height can shrink based on the resolved percentage width; Flexbox's
  // `min-height:auto` should use the transferred size suggestion (via aspect-ratio) instead of
  // forcing the intrinsic height.
  child_style.aspect_ratio = AspectRatio::Ratio(3.0 / 2.0);

  let child = BoxNode::new_replaced(
    Arc::new(child_style),
    ReplacedType::Canvas,
    Some(Size::new(1000.0, 500.0)),
    None,
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  // Give the flex container a wider containing block; it should honor its own `width: 200px`.
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(1000.0), AvailableSpace::Indefinite),
    )
    .expect("layout succeeds");

  assert!(
    (fragment.bounds.width() - 200.0).abs() < 0.5,
    "expected flex container border-box width 200, got {}",
    fragment.bounds.width()
  );

  let child_fragment = fragment.children.first().expect("expected child fragment");
  let expected_width = 200.0 - 20.0;
  assert!(
    (child_fragment.bounds.width() - expected_width).abs() < 0.5,
    "expected replaced flex item width ≈ {expected_width}, got {}",
    child_fragment.bounds.width()
  );

  let expected_height = expected_width / (3.0 / 2.0);
  assert!(
    (child_fragment.bounds.height() - expected_height).abs() < 0.5,
    "expected replaced flex item height ≈ {expected_height}, got {}",
    child_fragment.bounds.height()
  );
}
