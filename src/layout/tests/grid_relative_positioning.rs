use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::types::GridTrack;
use crate::style::types::InsetValue;
use crate::style::values::Length;
use crate::BoxNode;
use crate::ComputedStyle;
use crate::FormattingContextType;
use crate::Position;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn grid_item_relative_positioning_offsets_border_box() {
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(100.0)),
    GridTrack::Length(Length::px(100.0)),
  ];
  grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(100.0))];
  grid_style.width = Some(Length::px(200.0));
  grid_style.height = Some(Length::px(100.0));

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.grid_column_start = 2;
  item_style.grid_column_end = 3;
  item_style.grid_row_start = 1;
  item_style.grid_row_end = 2;
  item_style.width = Some(Length::px(100.0));
  item_style.height = Some(Length::px(100.0));
  item_style.position = Position::Relative;
  item_style.right = InsetValue::Length(Length::px(50.0));
  item_style.bottom = InsetValue::Length(Length::px(10.0));

  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![item],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 100.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 1);
  let child = &fragment.children[0];

  // Item placed in the second 100px track (x=100), then shifted left/up by `right` and `bottom`.
  assert_approx(child.bounds.x(), 50.0, "relative right offset shifts x");
  assert_approx(child.bounds.y(), -10.0, "relative bottom offset shifts y");
}
