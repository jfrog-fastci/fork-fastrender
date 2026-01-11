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

fn find_fragment<'a>(
  fragment: &'a fastrender::tree::fragment_tree::FragmentNode,
  id: usize,
) -> &'a fastrender::tree::fragment_tree::FragmentNode {
  fragment
    .iter_fragments()
    .find(|node| node.box_id() == Some(id))
    .unwrap_or_else(|| panic!("fragment with id {id} not found"))
}

#[test]
fn grid_abspos_negative_numeric_lines_count_from_explicit_grid_end() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(60.0));
  container_style.height = Some(Length::px(20.0));
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(20.0)),
  ];
  container_style.grid_template_rows = vec![GridTrack::Length(Length::px(20.0))];
  container_style.grid_auto_columns = vec![GridTrack::Length(Length::px(20.0))].into();
  let container_style = Arc::new(container_style);

  // Place an in-flow item into the first implicit column. This creates one trailing implicit track,
  // so the realized grid has 3 columns (2 explicit + 1 implicit).
  let mut flow_style = ComputedStyle::default();
  flow_style.display = Display::Block;
  flow_style.width = Some(Length::px(20.0));
  flow_style.height = Some(Length::px(20.0));
  flow_style.grid_column_start = 3;
  flow_style.grid_column_end = 4;
  let mut flow_child =
    BoxNode::new_block(Arc::new(flow_style), FormattingContextType::Block, vec![]);
  flow_child.id = 1;

  // Negative numeric lines should still count from the *explicit* grid end edge, not from the end
  // of the realized implicit grid.
  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_column_start = -2;
  abs_style.grid_column_end = -1;
  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 2;

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![flow_child, abs_child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(60.0, 20.0))
    .expect("grid layout");

  let abs_fragment = find_fragment(&fragment, 2);
  assert_approx(
    abs_fragment.bounds.x(),
    20.0,
    "abspos static position should resolve -2/-1 against the explicit grid",
  );
}
