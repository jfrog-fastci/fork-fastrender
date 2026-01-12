use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::AlignItems;
use crate::style::types::FlexDirection;
use crate::style::types::GridTrack;
use crate::style::types::IntrinsicSizeKeyword;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn grid_item_height_fit_content_keyword_with_definite_width_uses_that_width_for_sizing() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(200.0));
  container_style.grid_template_columns = vec![GridTrack::Fr(1.0)];
  container_style.grid_template_rows = vec![GridTrack::MinContent];
  let container_style = Arc::new(container_style);

  // A zero-height box whose padding-top is a percentage of its containing block width.
  // With a 200px wide grid area, this should result in a 100px tall box.
  let mut ratio_style = ComputedStyle::default();
  ratio_style.display = Display::Block;
  ratio_style.width = Some(Length::percent(100.0));
  ratio_style.padding_top = Length::percent(50.0);
  let mut ratio = BoxNode::new_block(Arc::new(ratio_style), FormattingContextType::Block, vec![]);
  ratio.id = 3;

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Flex;
  item_style.flex_direction = FlexDirection::Column;
  item_style.align_items = AlignItems::Stretch;
  item_style.height_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
  item_style.grid_column_start = 1;
  item_style.grid_column_end = 2;
  item_style.grid_row_start = 1;
  item_style.grid_row_end = 2;
  let mut item = BoxNode::new_block(
    Arc::new(item_style),
    FormattingContextType::Flex,
    vec![ratio],
  );
  item.id = 2;

  let mut grid = BoxNode::new_block(container_style, FormattingContextType::Grid, vec![item]);
  grid.id = 1;

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 1000.0))
    .expect("grid layout");

  let expected = 100.0;
  let actual = fragment.children[0].bounds.height();
  assert!(
    (actual - expected).abs() <= 0.5,
    "expected fit-content grid item height of {expected}, got {actual}",
  );
}
