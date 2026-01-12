use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::GridTrack;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

fn find_fragment_abs_origin(
  fragment: &fastrender::tree::fragment_tree::FragmentNode,
  id: usize,
) -> (f32, f32) {
  fn walk(
    node: &fastrender::tree::fragment_tree::FragmentNode,
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
fn subgrid_abspos_auto_placement_inherits_named_lines_for_static_position_columns() {
  let fc = GridFormattingContext::new();

  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.position = Position::Relative;
  parent_style.width = Some(Length::px(150.0));
  parent_style.height = Some(Length::px(50.0));
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(50.0)),
    GridTrack::Length(Length::px(50.0)),
    GridTrack::Length(Length::px(50.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Length(Length::px(50.0))];
  parent_style.grid_column_line_names = vec![
    vec!["a".to_string()],
    vec!["b".to_string()],
    vec!["c".to_string()],
    vec!["d".to_string()],
  ];
  let parent_style = Arc::new(parent_style);

  // Occupy the first column so the subgrid (auto / span 2) starts at column 2.
  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  first_style.width = Some(Length::px(1.0));
  first_style.height = Some(Length::px(1.0));
  first_style.grid_column_start = 1;
  first_style.grid_column_end = 2;
  first_style.grid_row_start = 1;
  first_style.grid_row_end = 2;
  let mut first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  first.id = 1;

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.position = Position::Relative;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_raw = Some("auto / span 2".to_string());
  let subgrid_style = Arc::new(subgrid_style);

  let mut placeholder_style = ComputedStyle::default();
  placeholder_style.display = Display::Block;
  placeholder_style.width = Some(Length::px(1.0));
  placeholder_style.height = Some(Length::px(1.0));
  let placeholder = BoxNode::new_block(
    Arc::new(placeholder_style),
    FormattingContextType::Block,
    vec![],
  );

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_raw = Some("c / d".to_string());
  abs_style.grid_row_start = 1;
  abs_style.grid_row_end = 2;
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 3;

  let mut subgrid = BoxNode::new_block(
    subgrid_style,
    FormattingContextType::Grid,
    vec![placeholder, abs_child],
  );
  subgrid.id = 2;

  let parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Grid,
    vec![first, subgrid],
  );

  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite(150.0, 50.0))
    .expect("layout should succeed");

  let abs_origin = find_fragment_abs_origin(&fragment, 3);
  assert_approx(
    abs_origin.0,
    100.0,
    "abspos child should align to the parent grid's third column start",
  );
}

#[test]
fn subgrid_abspos_auto_placement_inherits_named_lines_for_static_position_rows() {
  let fc = GridFormattingContext::new();

  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.position = Position::Relative;
  parent_style.width = Some(Length::px(50.0));
  parent_style.height = Some(Length::px(60.0));
  parent_style.grid_template_columns = vec![GridTrack::Length(Length::px(50.0))];
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(20.0)),
  ];
  parent_style.grid_row_line_names = vec![
    vec!["a".to_string()],
    vec!["b".to_string()],
    vec!["c".to_string()],
    vec!["d".to_string()],
  ];
  let parent_style = Arc::new(parent_style);

  // Occupy the first row so the row subgrid (auto / span 2) starts at row 2.
  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  first_style.width = Some(Length::px(1.0));
  first_style.height = Some(Length::px(1.0));
  first_style.grid_row_start = 1;
  first_style.grid_row_end = 2;
  first_style.grid_column_start = 1;
  first_style.grid_column_end = 2;
  let mut first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  first.id = 4;

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.position = Position::Relative;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_row_raw = Some("auto / span 2".to_string());
  subgrid_style.grid_template_columns = vec![GridTrack::Length(Length::px(50.0))];
  let subgrid_style = Arc::new(subgrid_style);

  let mut placeholder_style = ComputedStyle::default();
  placeholder_style.display = Display::Block;
  placeholder_style.width = Some(Length::px(1.0));
  placeholder_style.height = Some(Length::px(1.0));
  let placeholder = BoxNode::new_block(
    Arc::new(placeholder_style),
    FormattingContextType::Block,
    vec![],
  );

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_row_raw = Some("c / d".to_string());
  abs_style.grid_column_start = 1;
  abs_style.grid_column_end = 2;
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 6;

  let mut subgrid = BoxNode::new_block(
    subgrid_style,
    FormattingContextType::Grid,
    vec![placeholder, abs_child],
  );
  subgrid.id = 5;

  let parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Grid,
    vec![first, subgrid],
  );

  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite(50.0, 60.0))
    .expect("layout should succeed");

  let abs_origin = find_fragment_abs_origin(&fragment, 6);
  assert_approx(
    abs_origin.1,
    40.0,
    "abspos child should align to the parent grid's third row start",
  );
}
