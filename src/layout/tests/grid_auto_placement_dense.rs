use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::GridAutoFlow;
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
fn grid_auto_flow_row_dense_backfills_gaps() {
  // 3 fixed columns. Two spanning items create a gap at the end of the first row:
  //
  // Row 1: [item1 spans col1-2] [gap col3]
  // Row 2: [item2 spans col1-2] [item3]
  //
  // With `grid-auto-flow: row dense`, item3 should be backfilled into the gap in row 1 col 3.
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(30.0));
  container_style.height = Some(Length::px(20.0));
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(10.0)),
    GridTrack::Length(Length::px(10.0)),
    GridTrack::Length(Length::px(10.0)),
  ];
  container_style.grid_auto_flow = GridAutoFlow::RowDense;
  let container_style = Arc::new(container_style);

  let mut item1_style = ComputedStyle::default();
  item1_style.display = Display::Block;
  item1_style.width = Some(Length::px(10.0));
  item1_style.height = Some(Length::px(10.0));
  item1_style.grid_column_raw = Some("auto / span 2".to_string());
  let mut item1 = BoxNode::new_block(Arc::new(item1_style), FormattingContextType::Block, vec![]);
  item1.id = 1;

  let mut item2_style = ComputedStyle::default();
  item2_style.display = Display::Block;
  item2_style.width = Some(Length::px(10.0));
  item2_style.height = Some(Length::px(10.0));
  item2_style.grid_column_raw = Some("auto / span 2".to_string());
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
    .layout(&container, &LayoutConstraints::definite(30.0, 20.0))
    .expect("grid layout");

  let item3_fragment = fragment
    .iter_fragments()
    .find(|node| node.box_id() == Some(3))
    .expect("item3 fragment present");

  assert_approx(
    item3_fragment.bounds.x(),
    20.0,
    "dense placement column start",
  );
  assert_approx(item3_fragment.bounds.y(), 0.0, "dense placement row start");
}
