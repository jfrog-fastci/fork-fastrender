use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::position::Position;
use crate::style::types::GridTrack;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::BoxNode;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn grid_abspos_static_position_resolves_autorepeat_named_lines() {
  let fc = GridFormattingContext::new();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.position = Position::Relative;
  grid_style.width = Some(Length::px(80.0));
  grid_style.height = Some(Length::px(20.0));
  grid_style.grid_template_columns = vec![GridTrack::RepeatAutoFill {
    tracks: vec![GridTrack::Length(Length::px(20.0))],
    line_names: vec![vec!["col".to_string()], Vec::new()],
  }];
  // Computed styles only store the unexpanded template names. Static positioning should use
  // Taffy's expanded names so `col 3` resolves to the third auto-repeat line.
  grid_style.grid_column_line_names = vec![vec!["col".to_string()], Vec::new()];
  grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(20.0))];
  grid_style.grid_row_line_names = vec![Vec::new(), Vec::new()];
  let grid_style = Arc::new(grid_style);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_raw = Some("col 3 / col 4".to_string());
  abs_style.grid_row_start = 1;
  abs_style.grid_row_end = 2;
  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![abs_child]);

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(80.0, 20.0))
    .expect("grid layout");

  let abs_fragment = fragment
    .iter_fragments()
    .find(|node| {
      matches!(
        node.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert_approx(
    abs_fragment.bounds.x(),
    40.0,
    "abspos child should align to `col 3` (x=40px)",
  );
}
