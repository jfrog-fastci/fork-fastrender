use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::GridTrack;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn grid_item_percent_height_resolves_against_definite_fr_track() {
  // Regression test:
  // - Grid container has a definite height from its own `height` property.
  // - Rows are `fr` tracks, which become definite in that case.
  // - Grid items stretch to fill the grid area.
  // - Nested percentage heights inside the item should resolve against that stretched size.

  let mut fixed_child_style = ComputedStyle::default();
  fixed_child_style.display = Display::Block;
  fixed_child_style.height = Some(Length::px(10.0));
  fixed_child_style.height_keyword = None;
  let fixed_child =
    BoxNode::new_block(Arc::new(fixed_child_style), FormattingContextType::Block, vec![]);

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Column;
  flex_style.height = Some(Length::percent(100.0));
  flex_style.height_keyword = None;
  let flex_box =
    BoxNode::new_block(Arc::new(flex_style), FormattingContextType::Flex, vec![fixed_child]);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![flex_box]);

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(100.0));
  grid_style.height = Some(Length::px(200.0));
  grid_style.height_keyword = None;
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
  grid_style.grid_template_rows = vec![GridTrack::Fr(1.0)];
  let grid = BoxNode::new_block(Arc::new(grid_style), FormattingContextType::Grid, vec![item]);

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(
      &grid,
      // Simulate normal block flow: definite inline size, indefinite block size available.
      &LayoutConstraints::definite_width(1000.0),
    )
    .expect("layout should succeed");

  assert!(
    (fragment.bounds.height() - 200.0).abs() < 0.5,
    "grid container should honor its definite height (got {})",
    fragment.bounds.height()
  );

  let item_fragment = fragment.children.first().expect("grid item fragment");
  assert!(
    (item_fragment.bounds.height() - 200.0).abs() < 0.5,
    "grid item should stretch to fill the definite fr track (got {})",
    item_fragment.bounds.height()
  );
  let flex_fragment = item_fragment.children.first().expect("flex fragment");

  assert!(
    (flex_fragment.bounds.height() - 200.0).abs() < 0.5,
    "expected `height:100%` to resolve against the definite fr track (got {})",
    flex_fragment.bounds.height()
  );
}
