use crate::geometry::Size;
use crate::layout::constraints::AvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::FlexDirection;
use crate::style::types::Overflow;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use crate::tree::box_tree::ReplacedType;
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
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(width), AvailableSpace::Indefinite);

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

#[test]
fn flex_auto_min_size_allows_shrink_when_replaced_descendant_is_percentage_sized() {
  // Regression for pages like `etsy.com` where a flex item contains a replaced element with
  // `width/height:100%` plus a max-height clamp. When intrinsic sizing probes treat unresolved
  // percentages as `auto`, the replaced element appears unshrinkable (its intrinsic size wins),
  // causing flexbox `min-width:auto` to prevent shrinking and inflating the flex line's height.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(200.0));

  let mut wrapper_style = ComputedStyle::default();
  wrapper_style.display = Display::Block;
  wrapper_style.overflow_x = Overflow::Visible;
  wrapper_style.overflow_y = Overflow::Visible;
  wrapper_style.flex_shrink = 1.0;

  let mut replaced_style = ComputedStyle::default();
  replaced_style.display = Display::Block;
  replaced_style.width = Some(Length::percent(100.0));
  replaced_style.height = Some(Length::percent(100.0));
  replaced_style.max_height = Some(Length::px(400.0));

  let replaced = BoxNode::new_replaced(
    Arc::new(replaced_style),
    ReplacedType::Canvas,
    Some(Size::new(1260.0, 1000.0)),
    None,
  );
  let wrapper = BoxNode::new_block(
    Arc::new(wrapper_style),
    FormattingContextType::Block,
    vec![replaced],
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![wrapper],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let wrapper = fragment.children.first().expect("wrapper fragment");
  let width = wrapper.bounds.width();
  let height = wrapper.bounds.height();
  let eps = 0.5;
  assert!(
    width <= 200.0 + eps,
    "expected flex item containing percentage-sized replaced content to shrink to the container (got {width})"
  );
  assert!(
    height < 400.0 - eps,
    "expected flex item height to follow the shrunken replaced content (<400px), got {height}"
  );
}
