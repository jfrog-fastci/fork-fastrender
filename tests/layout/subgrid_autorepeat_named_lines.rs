use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::GridTrack;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{}: got {} expected {}",
    msg,
    val,
    expected
  );
}

#[test]
fn subgrid_inherits_autorepeat_named_lines_columns() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.width = Some(Length::px(80.0));
  parent_style.grid_template_columns = vec![GridTrack::RepeatAutoFill {
    tracks: vec![GridTrack::Length(Length::px(20.0))],
    line_names: vec![vec!["col".to_string()], Vec::new()],
  }];
  parent_style.grid_column_line_names = vec![vec!["col".to_string()], Vec::new()];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_row_line_names = vec![Vec::new(), Vec::new()];

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 4;

  let mut child1_style = ComputedStyle::default();
  child1_style.display = Display::Block;
  child1_style.height = Some(Length::px(10.0));
  child1_style.grid_column_raw = Some("col 2 / col 3".into());

  let mut child2_style = ComputedStyle::default();
  child2_style.display = Display::Block;
  child2_style.height = Some(Length::px(10.0));
  child2_style.grid_column_raw = Some("col 3 / col 4".into());

  let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(80.0, 40.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_eq!(subgrid_fragment.children.len(), 2);
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];

  assert_approx(first.bounds.x(), 20.0, "col 2 starts at 20px");
  assert_approx(second.bounds.x(), 40.0, "col 3 starts at 40px");
}

#[test]
fn subgrid_inherits_autorepeat_named_lines_rows() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.height = Some(Length::px(80.0));
  parent_style.grid_template_rows = vec![GridTrack::RepeatAutoFill {
    tracks: vec![GridTrack::Length(Length::px(20.0))],
    line_names: vec![vec!["row".to_string()], Vec::new()],
  }];
  parent_style.grid_row_line_names = vec![vec!["row".to_string()], Vec::new()];
  parent_style.grid_template_columns = vec![GridTrack::Auto];
  parent_style.grid_column_line_names = vec![Vec::new(), Vec::new()];

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 4;

  let mut child1_style = ComputedStyle::default();
  child1_style.display = Display::Block;
  child1_style.width = Some(Length::px(10.0));
  child1_style.grid_row_raw = Some("row 2 / row 3".into());

  let mut child2_style = ComputedStyle::default();
  child2_style.display = Display::Block;
  child2_style.width = Some(Length::px(10.0));
  child2_style.grid_row_raw = Some("row 3 / row 4".into());

  let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(40.0, 80.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_eq!(subgrid_fragment.children.len(), 2);
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];

  assert_approx(first.bounds.y(), 20.0, "row 2 starts at 20px");
  assert_approx(second.bounds.y(), 40.0, "row 3 starts at 40px");
}
