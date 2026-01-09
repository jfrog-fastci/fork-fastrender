use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::GridTrack;
use fastrender::style::types::WritingMode;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn absolute_child_in_subgrid_uses_grid_track_static_position() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.position = Position::Relative;
  parent_style.width = Some(Length::px(100.0));
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_start = 2;
  abs_style.grid_column_end = 3;

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![abs_child],
  );
  let container = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let constraints = LayoutConstraints::definite(100.0, 100.0);
  let fc = GridFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("grid layout");

  let abs_fragment = fragment
    .iter_fragments()
    .find(|node| {
      matches!(
        node.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert!(
    (abs_fragment.bounds.x() - 40.0).abs() < 0.1,
    "static position should align with second inherited grid column start (got x = {})",
    abs_fragment.bounds.x()
  );
}

#[test]
fn absolute_child_in_subgrid_uses_grid_track_static_position_from_raw_placement() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.position = Position::Relative;
  parent_style.width = Some(Length::px(100.0));
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_raw = Some("2 / 3".to_string());

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![abs_child],
  );
  let container = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let constraints = LayoutConstraints::definite(100.0, 100.0);
  let fc = GridFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("grid layout");

  let abs_fragment = fragment
    .iter_fragments()
    .find(|node| {
      matches!(
        node.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert!(
    (abs_fragment.bounds.x() - 40.0).abs() < 0.1,
    "static position should align with second inherited grid column start from raw placement (got x = {})",
    abs_fragment.bounds.x()
  );
}

#[test]
fn absolute_child_in_subgrid_uses_grid_track_static_position_with_vertical_writing_mode() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.position = Position::Relative;
  parent_style.writing_mode = WritingMode::VerticalRl;
  parent_style.width = Some(Length::px(110.0));
  parent_style.height = Some(Length::px(70.0));
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
  ];
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(50.0)),
    GridTrack::Length(Length::px(60.0)),
  ];

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  // writing-mode is inherited in real CSS. The test harness constructs computed styles manually,
  // so set it explicitly to avoid accidentally testing a mismatched writing mode.
  subgrid_style.writing_mode = WritingMode::VerticalRl;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 3;

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.writing_mode = WritingMode::VerticalRl;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_start = 2;
  abs_style.grid_column_end = 3;
  abs_style.grid_row_start = 2;
  abs_style.grid_row_end = 3;

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![abs_child],
  );
  let container = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fc = GridFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("grid layout");

  let abs_fragment = fragment
    .iter_fragments()
    .find(|node| {
      matches!(
        node.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert!(
    (abs_fragment.bounds.y() - 30.0).abs() < 0.1,
    "static position should align with second inherited grid column start on the physical y axis (got y = {})",
    abs_fragment.bounds.y()
  );
  assert!(
    (abs_fragment.bounds.x() - 50.0).abs() < 0.1,
    "static position should align with second inherited grid row start on the physical x axis (got x = {})",
    abs_fragment.bounds.x()
  );
}

#[test]
fn absolute_child_in_nested_subgrid_uses_grid_track_static_position_with_vertical_writing_mode() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.position = Position::Relative;
  parent_style.writing_mode = WritingMode::VerticalRl;
  parent_style.width = Some(Length::px(110.0));
  parent_style.height = Some(Length::px(70.0));
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
  ];
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(50.0)),
    GridTrack::Length(Length::px(60.0)),
  ];

  let mut outer_subgrid_style = ComputedStyle::default();
  outer_subgrid_style.display = Display::Grid;
  outer_subgrid_style.writing_mode = WritingMode::VerticalRl;
  outer_subgrid_style.grid_column_subgrid = true;
  outer_subgrid_style.grid_row_subgrid = true;
  outer_subgrid_style.grid_column_start = 1;
  outer_subgrid_style.grid_column_end = 3;
  outer_subgrid_style.grid_row_start = 1;
  outer_subgrid_style.grid_row_end = 3;

  let mut inner_subgrid_style = ComputedStyle::default();
  inner_subgrid_style.display = Display::Grid;
  inner_subgrid_style.writing_mode = WritingMode::VerticalRl;
  inner_subgrid_style.grid_column_subgrid = true;
  inner_subgrid_style.grid_row_subgrid = true;
  inner_subgrid_style.grid_column_start = 1;
  inner_subgrid_style.grid_column_end = 3;
  inner_subgrid_style.grid_row_start = 1;
  inner_subgrid_style.grid_row_end = 3;

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.writing_mode = WritingMode::VerticalRl;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_start = 2;
  abs_style.grid_column_end = 3;
  abs_style.grid_row_start = 2;
  abs_style.grid_row_end = 3;

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let inner_subgrid = BoxNode::new_block(
    Arc::new(inner_subgrid_style),
    FormattingContextType::Grid,
    vec![abs_child],
  );
  let outer_subgrid = BoxNode::new_block(
    Arc::new(outer_subgrid_style),
    FormattingContextType::Grid,
    vec![inner_subgrid],
  );
  let container = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![outer_subgrid],
  );

  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fc = GridFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("grid layout");

  let abs_fragment = fragment
    .iter_fragments()
    .find(|node| {
      matches!(
        node.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert!(
    (abs_fragment.bounds.y() - 30.0).abs() < 0.1,
    "static position should align with second inherited grid column start on the physical y axis (got y = {})",
    abs_fragment.bounds.y()
  );
  assert!(
    (abs_fragment.bounds.x() - 50.0).abs() < 0.1,
    "static position should align with second inherited grid row start on the physical x axis (got x = {})",
    abs_fragment.bounds.x()
  );
}

#[test]
fn absolute_child_in_subgrid_inherits_named_grid_lines_for_static_position() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.position = Position::Relative;
  parent_style.width = Some(Length::px(100.0));
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  parent_style.grid_column_line_names = vec![
    vec!["one".to_string()],
    vec!["two".to_string()],
    vec!["three".to_string()],
  ];

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;
  // Intentionally leave `grid_column_line_names` unset/empty to match how computed styles store
  // subgrid line-name overrides (they omit the inherited parent names).

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_raw = Some("two / three".to_string());

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![abs_child],
  );
  let container = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let constraints = LayoutConstraints::definite(100.0, 100.0);
  let fc = GridFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("grid layout");

  let abs_fragment = fragment
    .iter_fragments()
    .find(|node| {
      matches!(
        node.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert!(
    (abs_fragment.bounds.x() - 40.0).abs() < 0.1,
    "static position should align with second inherited grid column start from named line placement (got x = {})",
    abs_fragment.bounds.x()
  );
}

#[test]
fn absolute_child_in_nested_subgrid_inherits_named_grid_lines_for_static_position() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.position = Position::Relative;
  parent_style.width = Some(Length::px(100.0));
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  parent_style.grid_column_line_names = vec![
    vec!["one".to_string()],
    vec!["two".to_string()],
    vec!["three".to_string()],
  ];

  let mut outer_subgrid_style = ComputedStyle::default();
  outer_subgrid_style.display = Display::Grid;
  outer_subgrid_style.grid_column_subgrid = true;
  outer_subgrid_style.grid_column_start = 1;
  outer_subgrid_style.grid_column_end = 3;

  let mut inner_subgrid_style = ComputedStyle::default();
  inner_subgrid_style.display = Display::Grid;
  inner_subgrid_style.grid_column_subgrid = true;
  inner_subgrid_style.grid_column_start = 1;
  inner_subgrid_style.grid_column_end = 3;

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_raw = Some("two / three".to_string());

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let inner_subgrid = BoxNode::new_block(
    Arc::new(inner_subgrid_style),
    FormattingContextType::Grid,
    vec![abs_child],
  );
  let outer_subgrid = BoxNode::new_block(
    Arc::new(outer_subgrid_style),
    FormattingContextType::Grid,
    vec![inner_subgrid],
  );
  let container = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![outer_subgrid],
  );

  let constraints = LayoutConstraints::definite(100.0, 100.0);
  let fc = GridFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("grid layout");

  let abs_fragment = fragment
    .iter_fragments()
    .find(|node| {
      matches!(
        node.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert!(
    (abs_fragment.bounds.x() - 40.0).abs() < 0.1,
    "static position should align with second inherited grid column start through nested subgrids (got x = {})",
    abs_fragment.bounds.x()
  );
}
