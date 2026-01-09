use fastrender::geometry::Size;
use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::FlexDirection;
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

#[test]
fn flex_auto_min_height_uses_definite_cross_size_when_available() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.overflow_x = Overflow::Visible;
  child_style.overflow_y = Overflow::Visible;

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  text_style.font_size = 16.0;
  let text_style = Arc::new(text_style);

  // Many short words so the min-content inline size is tiny and would force extreme wrapping if
  // auto-min-height incorrectly probes at the min-content width.
  let text = "word ".repeat(64);
  let child = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Block,
    vec![BoxNode::new_text(text_style, text)],
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child.clone()],
  );

  let width = 200.0;
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(width), AvailableSpace::Indefinite);

  let flex_fc = FlexFormattingContext::new();
  let flex_fragment = flex_fc
    .layout(&container, &constraints)
    .expect("flex layout should succeed");
  let flex_child = flex_fragment.children.first().expect("flex child fragment");
  let flex_child_height = flex_child.bounds.height();

  let block_fc = BlockFormattingContext::new();
  let block_fragment = block_fc
    .layout(&child, &constraints)
    .expect("block layout should succeed");
  let block_height = block_fragment.bounds.height();

  let eps = 0.5;
  assert!(
    (flex_child_height - block_height).abs() <= eps,
    "expected flex auto min-height to be computed at the definite cross size; flex={flex_child_height} block={block_height} (eps={eps})"
  );
}
