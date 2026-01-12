use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::{AlignItems, FlexDirection, Overflow};
use fastrender::style::values::Length;
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;

#[test]
fn flex_auto_height_respects_max_height() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.align_items = AlignItems::Center;
  container_style.max_height = Some(Length::px(50.0));
  container_style.max_height_keyword = None;
  container_style.overflow_y = Overflow::Hidden;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(20.0));
  child_style.height = Some(Length::px(100.0));
  child_style.width_keyword = None;
  child_style.height_keyword = None;

  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  // Indefinite height models `height:auto` sizing to content.
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(200.0))
    .expect("layout succeeds");

  let eps = 1e-3;
  assert!(
    (fragment.bounds.height() - 50.0).abs() < eps,
    "expected flex container auto height to clamp to max-height, got {}",
    fragment.bounds.height()
  );
}
