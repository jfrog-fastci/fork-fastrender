use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::{AlignItems, FlexDirection};
use fastrender::style::values::Length;
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;

#[test]
fn flex_unrounded_layout_preserves_subpixel_sizes() {
  // Regresses: `FlexFormattingContext` was reading Taffy's rounded layout results, which drops
  // fractional CSS pixels (e.g. 22.4px -> 22.0px). When repeated across many items this accumulates
  // and produces visible drift (observed on docs.rs list separators).
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.align_items = AlignItems::FlexStart;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(22.4));
  child_style.width_keyword = None;
  child_style.height_keyword = None;

  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(100.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 1);
  let child_fragment = &fragment.children[0];
  assert!(
    (child_fragment.bounds.height() - 22.4).abs() < 0.05,
    "expected flex item to preserve fractional CSS pixels, got {:.2}",
    child_fragment.bounds.height()
  );
}

