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

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

fn find_fragment<'a>(
  fragment: &'a fastrender::tree::fragment_tree::FragmentNode,
  id: usize,
) -> &'a fastrender::tree::fragment_tree::FragmentNode {
  fragment
    .iter_fragments()
    .find(|node| node.box_id() == Some(id))
    .unwrap_or_else(|| panic!("fragment with id {id} not found"))
}

fn run_axis_mirroring_test(writing_mode: WritingMode) {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.writing_mode = writing_mode;
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
  let container_style = Arc::new(container_style);

  let mut flow_style = ComputedStyle::default();
  flow_style.display = Display::Block;
  flow_style.writing_mode = writing_mode;
  flow_style.width = Some(Length::px(10.0));
  flow_style.height = Some(Length::px(10.0));
  flow_style.grid_column_start = 2;
  flow_style.grid_column_end = 3;
  flow_style.grid_row_start = 2;
  flow_style.grid_row_end = 3;
  let mut flow_child =
    BoxNode::new_block(Arc::new(flow_style), FormattingContextType::Block, vec![]);
  flow_child.id = 1;

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.writing_mode = writing_mode;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_start = 2;
  abs_style.grid_column_end = 3;
  abs_style.grid_row_start = 2;
  abs_style.grid_row_end = 3;
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 2;

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![flow_child, abs_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(200.0, 200.0))
    .expect("grid layout");

  let flow_fragment = find_fragment(&fragment, 1);
  let abs_fragment = find_fragment(&fragment, 2);

  assert_approx(
    abs_fragment.bounds.x(),
    flow_fragment.bounds.x(),
    "abspos x should match equivalent in-flow placement",
  );
  assert_approx(
    abs_fragment.bounds.y(),
    flow_fragment.bounds.y(),
    "abspos y should match equivalent in-flow placement",
  );
}

#[test]
fn grid_abspos_static_position_matches_in_flow_under_vertical_rl() {
  run_axis_mirroring_test(WritingMode::VerticalRl);
}

#[test]
fn grid_abspos_static_position_matches_in_flow_under_sideways_rl() {
  run_axis_mirroring_test(WritingMode::SidewaysRl);
}
