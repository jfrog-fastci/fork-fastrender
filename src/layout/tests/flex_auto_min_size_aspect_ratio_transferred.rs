use crate::layout::constraints::AvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::AspectRatio;
use crate::style::types::Overflow;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_auto_min_size_aspect_ratio_uses_transferred_size_suggestion() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(50.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(40.0));
  child_style.aspect_ratio = AspectRatio::Ratio(2.0);
  child_style.overflow_x = Overflow::Visible;
  child_style.flex_shrink = 1.0;

  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
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
  assert!(
    child.bounds.width() >= 79.0,
    "aspect-ratio item should overflow rather than shrink below transferred size suggestion; got width {}",
    child.bounds.width()
  );
}

#[test]
fn flex_auto_min_size_auto_ratio_uses_transferred_size_suggestion() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(50.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(40.0));
  child_style.aspect_ratio = AspectRatio::AutoRatio(2.0);
  child_style.overflow_x = Overflow::Visible;
  child_style.flex_shrink = 1.0;

  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
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
  assert!(
    child.bounds.width() >= 79.0,
    "aspect-ratio item should overflow rather than shrink below transferred size suggestion; got width {}",
    child.bounds.width()
  );
}

#[test]
fn flex_auto_min_size_aspect_ratio_transferred_suggestion_clamped_by_max_width() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(50.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(40.0));
  child_style.aspect_ratio = AspectRatio::Ratio(2.0);
  child_style.max_width = Some(Length::px(60.0));
  child_style.overflow_x = Overflow::Visible;
  child_style.flex_shrink = 1.0;

  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
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
    (width - 60.0).abs() < 0.5,
    "definite max-width should clamp the transferred size suggestion; expected ~60, got {width}"
  );
}

#[test]
fn flex_auto_min_size_auto_ratio_transferred_suggestion_clamped_by_max_width() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(50.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(40.0));
  child_style.aspect_ratio = AspectRatio::AutoRatio(2.0);
  child_style.max_width = Some(Length::px(60.0));
  child_style.overflow_x = Overflow::Visible;
  child_style.flex_shrink = 1.0;

  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
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
    (width - 60.0).abs() < 0.5,
    "definite max-width should clamp the transferred size suggestion; expected ~60, got {width}"
  );
}
