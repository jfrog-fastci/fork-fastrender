use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
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
fn absolute_child_inherits_flex_static_position() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));

  let mut first_style = ComputedStyle::default();
  first_style.width = Some(Length::px(50.0));
  first_style.height = Some(Length::px(10.0));

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(20.0));
  abs_style.height = Some(Length::px(10.0));

  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![first, abs_child],
  );

  let constraints = LayoutConstraints::definite(200.0, 100.0);
  let fc = FlexFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("flex layout");

  let abs_fragment = fragment
    .children
    .iter()
    .find(|child| {
      matches!(
        child.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert!(
    abs_fragment.bounds.x().abs() < 0.1,
    "static position should be computed as-if the abspos child were the sole flex item (got x = {})",
    abs_fragment.bounds.x()
  );
}

#[test]
fn absolute_child_uses_grid_track_static_position() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];

  let mut flow_style = ComputedStyle::default();
  flow_style.width = Some(Length::px(20.0));
  flow_style.height = Some(Length::px(10.0));

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_start = 2;
  abs_style.grid_column_end = 3;

  let flow_child = BoxNode::new_block(Arc::new(flow_style), FormattingContextType::Block, vec![]);
  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![flow_child, abs_child],
  );

  let constraints = LayoutConstraints::definite(100.0, 100.0);
  let fc = GridFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("grid layout");

  let abs_fragment = fragment
    .children
    .iter()
    .find(|child| {
      matches!(
        child.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert!(
    (abs_fragment.bounds.x() - 40.0).abs() < 0.1,
    "static position should align with second grid column start (got x = {})",
    abs_fragment.bounds.x()
  );
}

#[test]
fn absolute_child_uses_grid_track_static_position_from_raw_numeric_placement() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];

  let mut flow_style = ComputedStyle::default();
  flow_style.width = Some(Length::px(20.0));
  flow_style.height = Some(Length::px(10.0));

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_raw = Some("2 / 3".to_string());

  let flow_child = BoxNode::new_block(Arc::new(flow_style), FormattingContextType::Block, vec![]);
  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![flow_child, abs_child],
  );

  let constraints = LayoutConstraints::definite(100.0, 100.0);
  let fc = GridFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("grid layout");

  let abs_fragment = fragment
    .children
    .iter()
    .find(|child| {
      matches!(
        child.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert!(
    (abs_fragment.bounds.x() - 40.0).abs() < 0.1,
    "static position should align with second grid column start from raw placement (got x = {})",
    abs_fragment.bounds.x()
  );
}

#[test]
fn absolute_child_uses_grid_track_static_position_from_raw_span_placement() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(90.0));
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
  ];

  let mut flow_style = ComputedStyle::default();
  flow_style.width = Some(Length::px(10.0));
  flow_style.height = Some(Length::px(10.0));

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_raw = Some("2 / span 2".to_string());

  let flow_child = BoxNode::new_block(Arc::new(flow_style), FormattingContextType::Block, vec![]);
  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![flow_child, abs_child],
  );

  let constraints = LayoutConstraints::definite(90.0, 100.0);
  let fc = GridFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("grid layout");

  let abs_fragment = fragment
    .children
    .iter()
    .find(|child| {
      matches!(
        child.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert!(
    (abs_fragment.bounds.x() - 20.0).abs() < 0.1,
    "static position should align with second grid column start from raw span placement (got x = {})",
    abs_fragment.bounds.x()
  );
}

#[test]
fn absolute_child_uses_grid_track_static_position_with_multi_track_span() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(90.0));
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
  ];

  let mut flow_style = ComputedStyle::default();
  flow_style.width = Some(Length::px(10.0));
  flow_style.height = Some(Length::px(10.0));

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_start = 2;
  abs_style.grid_column_end = 4;

  let flow_child = BoxNode::new_block(Arc::new(flow_style), FormattingContextType::Block, vec![]);
  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![flow_child, abs_child],
  );

  let constraints = LayoutConstraints::definite(90.0, 100.0);
  let fc = GridFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("grid layout");

  let abs_fragment = fragment
    .children
    .iter()
    .find(|child| {
      matches!(
        child.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert!(
    (abs_fragment.bounds.x() - 20.0).abs() < 0.1,
    "static position should align with multi-track grid area start (got x = {})",
    abs_fragment.bounds.x()
  );
}

#[test]
fn absolute_child_uses_grid_track_static_position_with_vertical_writing_mode() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.width = Some(Length::px(110.0));
  container_style.height = Some(Length::px(70.0));
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
  ];
  container_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(50.0)),
    GridTrack::Length(Length::px(60.0)),
  ];

  let mut flow_style = ComputedStyle::default();
  flow_style.width = Some(Length::px(10.0));
  flow_style.height = Some(Length::px(10.0));

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_start = 2;
  abs_style.grid_column_end = 3;
  abs_style.grid_row_start = 2;
  abs_style.grid_row_end = 3;

  let flow_child = BoxNode::new_block(Arc::new(flow_style), FormattingContextType::Block, vec![]);
  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![flow_child, abs_child],
  );

  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fc = GridFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("grid layout");

  let abs_fragment = fragment
    .children
    .iter()
    .find(|child| {
      matches!(
        child.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert!(
    (abs_fragment.bounds.y() - 30.0).abs() < 0.1,
    "static position should align with second grid column start on the physical y axis (got y = {})",
    abs_fragment.bounds.y()
  );
  assert!(
    (abs_fragment.bounds.x() - 50.0).abs() < 0.1,
    "static position should align with second grid row start on the physical x axis (got x = {})",
    abs_fragment.bounds.x()
  );
}
