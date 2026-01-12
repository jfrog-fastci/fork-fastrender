use crate::layout::constraints::AvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::FlexDirection;
use crate::style::types::GridAutoFlow;
use crate::style::types::GridTrack;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn grid_item_percent_height_does_not_resolve_against_auto_track() {
  // Regression test for percentage height resolution inside grid items.
  //
  // A common pattern (e.g. horizontal carousels) is a grid with implicit `auto` rows plus in-flow
  // descendants using `height: 100%`. The containing block's block size is not definite in this
  // case, so CSS2.1 requires percentage heights to compute to `auto` rather than expanding to a
  // viewport/probe size.

  let mut fixed_child_style = ComputedStyle::default();
  fixed_child_style.display = Display::Block;
  fixed_child_style.height = Some(Length::px(10.0));
  fixed_child_style.height_keyword = None;
  let fixed_child = BoxNode::new_block(
    Arc::new(fixed_child_style),
    FormattingContextType::Block,
    vec![],
  );

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Column;
  flex_style.height = Some(Length::percent(100.0));
  flex_style.height_keyword = None;
  let flex_box = BoxNode::new_block(
    Arc::new(flex_style),
    FormattingContextType::Flex,
    vec![fixed_child],
  );

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  let item = BoxNode::new_block(
    Arc::new(item_style),
    FormattingContextType::Block,
    vec![flex_box],
  );

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.grid_auto_flow = GridAutoFlow::Column;
  grid_style.grid_auto_columns = vec![GridTrack::Length(Length::px(100.0))].into();
  // Leave rows as the default implicit `auto` track.
  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![item],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(
      &grid,
      // The grid container is in normal flow (`height:auto`). Even if the outer layout provides a
      // definite viewport block size, CSS block formatting context does not constrain the grid's
      // used height. Ensure we don't treat that definite available height as a sizing constraint.
      &LayoutConstraints::new(
        AvailableSpace::Definite(100.0),
        AvailableSpace::Definite(500.0),
      ),
    )
    .expect("layout should succeed");

  let item_fragment = fragment.children.first().expect("grid item fragment");
  let flex_fragment = item_fragment.children.first().expect("flex fragment");

  assert!(
    (flex_fragment.bounds.height() - 10.0).abs() < 0.5,
    "expected `height:100%` to compute to `auto` in auto-sized grid tracks (got {})",
    flex_fragment.bounds.height()
  );
}
