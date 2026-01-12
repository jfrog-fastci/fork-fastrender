use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::GridAutoFlow;
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

#[test]
fn grid_auto_flow_column_dense_backfills_gaps() {
  // 3 fixed rows. Two spanning items create a gap at the end of the first column:
  //
  // Col 1: [item1 spans row1-2]
  //        [gap row3]
  // Col 2: [item2 spans row1-2]
  //        [item3]
  //
  // With `grid-auto-flow: column dense`, item3 should be backfilled into the gap in col 1 row 3.
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(20.0));
  container_style.height = Some(Length::px(30.0));
  container_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(10.0)),
    GridTrack::Length(Length::px(10.0)),
    GridTrack::Length(Length::px(10.0)),
  ];
  container_style.grid_auto_columns = vec![GridTrack::Length(Length::px(10.0))].into();
  container_style.grid_auto_flow = GridAutoFlow::ColumnDense;
  let container_style = Arc::new(container_style);

  let mut item1_style = ComputedStyle::default();
  item1_style.display = Display::Block;
  item1_style.width = Some(Length::px(10.0));
  item1_style.height = Some(Length::px(10.0));
  item1_style.grid_row_raw = Some("auto / span 2".to_string());
  let mut item1 = BoxNode::new_block(Arc::new(item1_style), FormattingContextType::Block, vec![]);
  item1.id = 1;

  let mut item2_style = ComputedStyle::default();
  item2_style.display = Display::Block;
  item2_style.width = Some(Length::px(10.0));
  item2_style.height = Some(Length::px(10.0));
  item2_style.grid_row_raw = Some("auto / span 2".to_string());
  let mut item2 = BoxNode::new_block(Arc::new(item2_style), FormattingContextType::Block, vec![]);
  item2.id = 2;

  let mut item3_style = ComputedStyle::default();
  item3_style.display = Display::Block;
  item3_style.width = Some(Length::px(10.0));
  item3_style.height = Some(Length::px(10.0));
  let mut item3 = BoxNode::new_block(Arc::new(item3_style), FormattingContextType::Block, vec![]);
  item3.id = 3;

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![item1, item2, item3],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(20.0, 30.0))
    .expect("grid layout");

  let item3_fragment = fragment
    .iter_fragments()
    .find(|node| node.box_id() == Some(3))
    .expect("item3 fragment present");

  assert_approx(
    item3_fragment.bounds.x(),
    0.0,
    "dense placement column start",
  );
  assert_approx(item3_fragment.bounds.y(), 20.0, "dense placement row start");
}

#[test]
fn grid_auto_flow_column_dense_backfills_row1_after_spanning_item() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(20.0));
  container_style.height = Some(Length::px(30.0));
  container_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(10.0)),
    GridTrack::Length(Length::px(10.0)),
    GridTrack::Length(Length::px(10.0)),
  ];
  container_style.grid_auto_columns = vec![GridTrack::Length(Length::px(10.0))].into();
  container_style.grid_auto_flow = GridAutoFlow::ColumnDense;
  let container_style = Arc::new(container_style);

  // Place an item spanning rows 2-3 in the first column, leaving row 1 open.
  let mut item1_style = ComputedStyle::default();
  item1_style.display = Display::Block;
  item1_style.width = Some(Length::px(10.0));
  item1_style.height = Some(Length::px(20.0));
  item1_style.grid_row_start = 2;
  item1_style.grid_row_end = 4;
  let mut item1 = BoxNode::new_block(Arc::new(item1_style), FormattingContextType::Block, vec![]);
  item1.id = 4;

  let mut item2_style = ComputedStyle::default();
  item2_style.display = Display::Block;
  item2_style.width = Some(Length::px(10.0));
  item2_style.height = Some(Length::px(10.0));
  let mut item2 = BoxNode::new_block(Arc::new(item2_style), FormattingContextType::Block, vec![]);
  item2.id = 5;

  let container = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![item1, item2],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(20.0, 30.0))
    .expect("grid layout");

  let item2_fragment = fragment
    .iter_fragments()
    .find(|node| node.box_id() == Some(5))
    .expect("item2 fragment present");

  assert_approx(
    item2_fragment.bounds.x(),
    0.0,
    "dense placement column start",
  );
  assert_approx(item2_fragment.bounds.y(), 0.0, "dense placement row start");
}
