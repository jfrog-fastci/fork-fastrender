use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::style::display::Display;
use crate::style::types::{
  AlignContent, FlexDirection, GridTrack, IntrinsicSizeKeyword, JustifyContent,
};
use crate::style::values::Length;
use crate::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;

#[test]
fn grid_item_min_height_min_content_does_not_blow_up_from_min_content_inline_wrapping() {
  // Regression test for pages like shopify.com where a grid item uses `min-height: min-content`.
  //
  // Grid track sizing provides the item's *used* inline size (the grid area width). However, our
  // intrinsic block size APIs compute block sizes under the box's intrinsic inline sizes (which can
  // be extremely narrow for text like "a a a ..."). Pre-resolving `min-height:min-content` using
  // those intrinsic APIs can therefore force a grid item to become thousands of pixels tall.
  //
  // Ensure grid layout does not resolve `min-height:min-content` into a huge definite minimum on
  // the block axis before the used inline size is known.
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(200.0));
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
  grid_style.grid_template_rows = vec![GridTrack::Auto];
  // Avoid `justify-content/align-content: stretch` distributing leftover free space into tracks,
  // which could mask the failure by stretching the grid for unrelated reasons.
  grid_style.justify_content = JustifyContent::Start;
  grid_style.align_content = AlignContent::Start;

  // Use a flex container (like shopify.com's hero container) so the block-axis min-height keyword
  // is resolved by the grid container rather than being consumed by block layout itself.
  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Flex;
  item_style.flex_direction = FlexDirection::Column;
  item_style.min_height_keyword = Some(IntrinsicSizeKeyword::MinContent);

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  let text = "a ".repeat(80);
  let child = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Block,
    vec![BoxNode::new_text(Arc::new(ComputedStyle::default()), text)],
  );
  let item = BoxNode::new_block(
    Arc::new(item_style),
    FormattingContextType::Flex,
    vec![child],
  );

  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![item],
  );
  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 400.0))
    .expect("layout");

  assert_eq!(fragment.children.len(), 1);
  let item_fragment = &fragment.children[0];
  let height = item_fragment.bounds.height();
  assert!(
    height.is_finite() && height > 0.0,
    "expected a finite positive item height, got {height}"
  );
  assert!(
    height < 500.0,
    "expected `min-height:min-content` to avoid huge wrapped min-content block sizes (got {height})"
  );
}
