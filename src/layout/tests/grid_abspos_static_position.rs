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

#[test]
fn absolute_child_uses_named_grid_lines_for_static_position() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  container_style.grid_column_line_names = vec![
    vec!["start".to_string()],
    vec!["mid".to_string()],
    vec!["end".to_string()],
  ];

  let mut flow_style = ComputedStyle::default();
  flow_style.display = Display::Block;
  flow_style.width = Some(Length::px(10.0));
  flow_style.height = Some(Length::px(10.0));
  let flow_child = BoxNode::new_block(Arc::new(flow_style), FormattingContextType::Block, vec![]);

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_raw = Some("mid / end".to_string());
  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![flow_child, abs_child],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 100.0))
    .expect("grid layout");

  let abs_fragment = fragment
    .iter_fragments()
    .find(|node| {
      node
        .style
        .as_ref()
        .is_some_and(|style| style.position == Position::Absolute)
    })
    .expect("absolute fragment present");

  assert_approx(
    abs_fragment.bounds.x(),
    40.0,
    "static position should align to the 'mid' line",
  );
}

#[test]
fn absolute_child_uses_named_grid_lines_with_occurrence_index_for_static_position() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(30.0));
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(10.0)),
    GridTrack::Length(Length::px(10.0)),
    GridTrack::Length(Length::px(10.0)),
  ];
  // Name every line "foo" so `foo 2` maps to the second line (after the first track).
  container_style.grid_column_line_names = vec![
    vec!["foo".to_string()],
    vec!["foo".to_string()],
    vec!["foo".to_string()],
    vec!["foo".to_string()],
  ];

  let mut flow_style = ComputedStyle::default();
  flow_style.display = Display::Block;
  flow_style.width = Some(Length::px(1.0));
  flow_style.height = Some(Length::px(1.0));
  let flow_child = BoxNode::new_block(Arc::new(flow_style), FormattingContextType::Block, vec![]);

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(5.0));
  abs_style.height = Some(Length::px(5.0));
  abs_style.grid_column_raw = Some("foo 2 / foo 4".to_string());
  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![flow_child, abs_child],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(30.0, 50.0))
    .expect("grid layout");

  let abs_fragment = fragment
    .iter_fragments()
    .find(|node| {
      node
        .style
        .as_ref()
        .is_some_and(|style| style.position == Position::Absolute)
    })
    .expect("absolute fragment present");

  assert_approx(
    abs_fragment.bounds.x(),
    10.0,
    "static position should align to the 2nd 'foo' line",
  );
}
