use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::WritingMode;
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::ComputedStyle;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn vertical_writing_mode_simple_grid_stacks_along_physical_x() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.writing_mode = WritingMode::VerticalLr;
  // Set a definite size that matches the expected intrinsic track sizing so `align-content:
  // stretch` does not redistribute free space into the implicit rows.
  container_style.width = Some(Length::px(20.0));
  container_style.height = Some(Length::px(100.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  let child_style = Arc::new(child_style);

  let mut child_a = BoxNode::new_block(child_style.clone(), FormattingContextType::Block, vec![]);
  child_a.id = 1;
  let mut child_b = BoxNode::new_block(child_style, FormattingContextType::Block, vec![]);
  child_b.id = 2;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![child_a, child_b],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(20.0, 100.0))
    .expect("grid layout succeeds");

  let child_b_fragment = fragment
    .iter_fragments()
    .find(|node| node.box_id() == Some(2))
    .expect("second child fragment");

  assert_approx(
    child_b_fragment.bounds.x(),
    10.0,
    "second child should start after the first child along physical x",
  );
  assert_approx(
    child_b_fragment.bounds.y(),
    0.0,
    "second child should remain aligned to the top edge on physical y",
  );
}

#[test]
fn abspos_static_position_uses_implicit_row_offsets_in_simple_grids() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));

  let mut flow_style = ComputedStyle::default();
  flow_style.display = Display::Block;
  flow_style.height = Some(Length::px(20.0));
  let flow_child = BoxNode::new_block(Arc::new(flow_style), FormattingContextType::Block, vec![]);

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.grid_row_start = 2;
  abs_style.grid_row_end = 3;
  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![flow_child, abs_child],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(100.0))
    .expect("grid layout succeeds");

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
    abs_fragment.bounds.y(),
    20.0,
    "static position should align to the start of the second implicit row",
  );
}
