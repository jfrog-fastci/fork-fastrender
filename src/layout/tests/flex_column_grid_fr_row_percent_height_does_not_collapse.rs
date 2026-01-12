use std::sync::Arc;

use crate::layout::constraints::AvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::AlignItems;
use crate::style::types::FlexDirection;
use crate::style::types::GridTrack;
use crate::style::types::Overflow;
use crate::style::values::Length;
use crate::tree::box_tree::BoxNode;
use crate::ComputedStyle;

#[test]
fn flex_column_grid_fr_row_percent_height_does_not_collapse() {
  // Regression test for flex item measurement of nested grid containers.
  //
  // On wired.com, sticky nav rows are flex items with:
  //   height: 100%;
  //   grid-template-rows: 1fr;
  // where the percentage height computes to `auto` because the flex container has an indefinite
  // height (CSS2.1 §10.5 / Flexbox percent resolution). If the flex measurement path treats a
  // spurious 0px "known" block size from Taffy as definite, the grid container is laid out at 0px
  // tall, the `1fr` row collapses, and overflow-hidden descendants clip the nav content.

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Flex;
  outer_style.flex_direction = FlexDirection::Column;
  outer_style.align_items = AlignItems::Stretch;
  outer_style.width = Some(Length::px(200.0));

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.height = Some(Length::percent(100.0));
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
  grid_style.grid_template_rows = vec![GridTrack::Fr(1.0)];

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Block;
  inner_style.height = Some(Length::px(80.0));
  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.overflow_x = Overflow::Hidden;
  item_style.overflow_y = Overflow::Hidden;
  let item = BoxNode::new_block(
    Arc::new(item_style),
    FormattingContextType::Block,
    vec![inner],
  );

  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![item],
  );

  let mut sibling_style = ComputedStyle::default();
  sibling_style.display = Display::Block;
  sibling_style.height = Some(Length::px(10.0));
  sibling_style.width = Some(Length::px(100.0));
  let sibling = BoxNode::new_block(
    Arc::new(sibling_style),
    FormattingContextType::Block,
    vec![],
  );

  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Flex,
    vec![grid, sibling],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &outer,
      &LayoutConstraints::new(
        AvailableSpace::Definite(200.0),
        AvailableSpace::Definite(500.0),
      )
      .with_used_border_box_size(Some(200.0), None),
    )
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 2);
  let grid_fragment = &fragment.children[0];
  let sibling_fragment = &fragment.children[1];

  assert!(
    grid_fragment.bounds.height() > 79.0,
    "expected grid flex item to have non-zero height, got {}",
    grid_fragment.bounds.height()
  );
  assert!(
    sibling_fragment.bounds.y() >= grid_fragment.bounds.height() - 0.5,
    "expected sibling to be placed after grid flex item; grid_height={} sibling_y={}",
    grid_fragment.bounds.height(),
    sibling_fragment.bounds.y(),
  );
}
