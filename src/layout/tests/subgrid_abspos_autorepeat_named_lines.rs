use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::position::Position;
use crate::style::types::GridTrack;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

fn find_fragment_abs_origin(
  fragment: &crate::tree::fragment_tree::FragmentNode,
  id: usize,
) -> (f32, f32) {
  fn walk(
    node: &crate::tree::fragment_tree::FragmentNode,
    acc_x: f32,
    acc_y: f32,
    id: usize,
  ) -> Option<(f32, f32)> {
    let x = acc_x + node.bounds.x();
    let y = acc_y + node.bounds.y();
    if node.box_id() == Some(id) {
      return Some((x, y));
    }
    for child in node.children.iter() {
      if let Some(found) = walk(child, x, y, id) {
        return Some(found);
      }
    }
    None
  }

  walk(fragment, 0.0, 0.0, id).unwrap_or_else(|| panic!("fragment with id {id} not found"))
}

#[test]
fn subgrid_abspos_static_position_inherits_autorepeat_named_lines() {
  let fc = GridFormattingContext::new();

  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.position = Position::Relative;
  parent_style.width = Some(Length::px(80.0));
  parent_style.height = Some(Length::px(20.0));
  parent_style.grid_template_columns = vec![GridTrack::RepeatAutoFill {
    tracks: vec![GridTrack::Length(Length::px(20.0))],
    line_names: vec![vec!["col".to_string()], Vec::new()],
  }];
  parent_style.grid_column_line_names = vec![vec!["col".to_string()], Vec::new()];
  parent_style.grid_template_rows = vec![GridTrack::Length(Length::px(20.0))];
  parent_style.grid_row_line_names = vec![Vec::new(), Vec::new()];
  let parent_style = Arc::new(parent_style);

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.position = Position::Relative;
  subgrid_style.grid_column_subgrid = true;
  // Span the full auto-filled grid so the third `col` line resolves to x=40px from the parent.
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 5;
  subgrid_style.grid_template_rows = vec![GridTrack::Length(Length::px(20.0))];

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_raw = Some("col 3 / col 4".to_string());
  abs_style.grid_row_start = 1;
  abs_style.grid_row_end = 2;
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 2;

  let mut subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![abs_child],
  );
  subgrid.id = 1;

  let parent = BoxNode::new_block(parent_style, FormattingContextType::Grid, vec![subgrid]);

  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite(80.0, 20.0))
    .expect("layout should succeed");

  let abs_origin = find_fragment_abs_origin(&fragment, 2);
  assert_approx(
    abs_origin.0,
    40.0,
    "abspos child should resolve `col 3` inside the subgrid (x=40px)",
  );
}
