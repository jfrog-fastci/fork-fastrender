use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::style::display::Display;
use crate::style::types::AlignItems;
use crate::style::types::GridTrack;
use crate::style::values::Length;
use crate::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType, IntrinsicSizingMode};
use std::sync::Arc;

// Regression test for a grid item whose formatting context is Flex.
//
// In the Tesco fixture, the header grid centers its children vertically (`align-items:center`).
// FastRender incorrectly reported a 0px-tall fragment for the flex grid item, which caused the
// contents (a 42px-tall search form) to appear ~21px lower than Chrome (half the content height).
#[test]
fn grid_align_items_center_uses_measured_flex_item_height() {
  // First item fixes the row height to 74px.
  let mut fixed_style = ComputedStyle::default();
  fixed_style.display = Display::Block;
  fixed_style.width = Some(Length::px(100.0));
  fixed_style.height = Some(Length::px(74.0));
  let fixed = BoxNode::new_block(Arc::new(fixed_style), FormattingContextType::Block, vec![]);

  // Second item is a flex container with an auto height determined by its child (42px).
  let mut flex_child_style = ComputedStyle::default();
  flex_child_style.display = Display::Block;
  flex_child_style.width = Some(Length::px(100.0));
  flex_child_style.height = Some(Length::px(42.0));
  let flex_child =
    BoxNode::new_block(Arc::new(flex_child_style), FormattingContextType::Block, vec![]);

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  // Leave height as `auto` so it is content-sized (42px).
  // Leave width as `auto` so grid intrinsic sizing can probe it (mirrors Tesco's header search
  // container, which has an auto width with a max-width clamp).
  let flex_item =
    BoxNode::new_block(Arc::new(flex_style), FormattingContextType::Flex, vec![flex_child]);

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(200.0));
  grid_style.grid_template_columns =
    vec![GridTrack::Length(Length::px(100.0)), GridTrack::Length(Length::px(100.0))];
  grid_style.align_items = AlignItems::Center;

  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![fixed, flex_item],
  );

  let fc = GridFormattingContext::new();

  // Prime the grid container's cached Taffy tree with an intrinsic sizing probe before running the
  // normal layout pass. Prior to the Tesco fix, the intrinsic probe would cache a 0px block size
  // for the flex grid item (due to the "indefinite" available height sentinel), and the subsequent
  // layout would incorrectly center the item as if it were 0px tall.
  let _ = fc
    .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic sizing succeeds");
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite_width(200.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 2);
  let flex_fragment = &fragment.children[1];

  assert!(
    (flex_fragment.bounds.height() - 42.0).abs() <= 0.5,
    "expected flex grid item height to match contents (42px), got {}",
    flex_fragment.bounds.height()
  );
  assert!(
    (flex_fragment.bounds.y() - 16.0).abs() <= 0.5,
    "expected centered flex grid item y=(74-42)/2=16px, got {}",
    flex_fragment.bounds.y()
  );
}
