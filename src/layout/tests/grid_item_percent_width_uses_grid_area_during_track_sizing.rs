use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::types::GridTrack;
use crate::style::values::Length;
use crate::{BoxNode, ComputedStyle, FormattingContextType};
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn grid_item_percent_width_does_not_force_fr_tracks_to_overflow() {
  // Regression for grid track sizing interacting with percentage widths.
  //
  // In CSS Grid, `width: 100%` on a grid item resolves against its *grid area* size. During track
  // sizing, that area size is not yet known, so percentages must behave as `auto` (rather than
  // incorrectly resolving against an ancestor width).
  //
  // When percentage widths are resolved too early, each `1fr` track can incorrectly take on the
  // full container width, causing the grid to overflow instead of distributing space.
  let fc = GridFormattingContext::new();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(101.0));
  grid_style.height = Some(Length::px(20.0));
  grid_style.grid_template_columns = vec![
    GridTrack::Fr(1.0),
    GridTrack::MaxContent,
    GridTrack::Fr(1.0),
  ];
  grid_style.grid_template_rows = vec![GridTrack::Auto];
  let grid_style = Arc::new(grid_style);

  let mut left_style = ComputedStyle::default();
  left_style.display = Display::Block;
  left_style.width = Some(Length::percent(100.0));
  left_style.height = Some(Length::px(10.0));
  left_style.grid_column_start = 1;
  left_style.grid_column_end = 2;
  let left = BoxNode::new_block(Arc::new(left_style), FormattingContextType::Block, vec![]);

  let mut divider_style = ComputedStyle::default();
  divider_style.display = Display::Block;
  divider_style.width = Some(Length::px(1.0));
  divider_style.height = Some(Length::px(10.0));
  divider_style.grid_column_start = 2;
  divider_style.grid_column_end = 3;
  let divider = BoxNode::new_block(
    Arc::new(divider_style),
    FormattingContextType::Block,
    vec![],
  );

  let mut right_style = ComputedStyle::default();
  right_style.display = Display::Block;
  right_style.width = Some(Length::percent(100.0));
  right_style.height = Some(Length::px(10.0));
  right_style.grid_column_start = 3;
  right_style.grid_column_end = 4;
  let right = BoxNode::new_block(Arc::new(right_style), FormattingContextType::Block, vec![]);

  let mut grid = BoxNode::new_block(
    grid_style,
    FormattingContextType::Grid,
    vec![left, divider, right],
  );
  grid.id = 1;

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(101.0, 20.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 3);

  let left = &fragment.children[0];
  let divider = &fragment.children[1];
  let right = &fragment.children[2];

  assert_approx(fragment.bounds.width(), 101.0, "grid width");

  // The max-content track should size to the 1px divider, leaving 100px for the two `1fr` tracks.
  assert_approx(left.bounds.x(), 0.0, "left start");
  assert_approx(left.bounds.width(), 50.0, "left width");
  assert_approx(divider.bounds.x(), 50.0, "divider start");
  assert_approx(divider.bounds.width(), 1.0, "divider width");
  assert_approx(right.bounds.x(), 51.0, "right start");
  assert_approx(right.bounds.width(), 50.0, "right width");
}
