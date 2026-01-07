use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::GridTrack;
use fastrender::style::types::IntrinsicSizeKeyword;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn grid_percent_tracks_act_as_auto_when_width_is_indefinite_then_resolve() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::percent(25.0)),
    GridTrack::Length(Length::percent(75.0)),
  ];
  container_style.grid_template_rows = vec![GridTrack::Auto];
  let container_style = Arc::new(container_style);

  let mut item1_style = ComputedStyle::default();
  item1_style.display = Display::Block;
  item1_style.width = Some(Length::px(40.0));
  item1_style.height = Some(Length::px(10.0));
  item1_style.grid_column_start = 1;
  item1_style.grid_column_end = 2;
  let item1 = BoxNode::new_block(Arc::new(item1_style), FormattingContextType::Block, vec![]);

  let mut item2_style = ComputedStyle::default();
  item2_style.display = Display::Block;
  item2_style.width = Some(Length::px(40.0));
  item2_style.height = Some(Length::px(10.0));
  item2_style.grid_column_start = 2;
  item2_style.grid_column_end = 3;
  let item2 = BoxNode::new_block(Arc::new(item2_style), FormattingContextType::Block, vec![]);

  let mut grid = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![item1, item2],
  );
  grid.id = 1;

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(500.0, 200.0))
    .expect("layout succeeds");

  // With `width: max-content`, the container size is initially indefinite. Per CSS Grid,
  // percentage tracks behave as `auto` until the container size resolves, and then must be rerun.
  //
  // Here both items are 40px wide -> container shrinkwrap width is 80px, and the percentages then
  // resolve against that (25% = 20px, 75% = 60px).
  assert_approx(fragment.bounds.width(), 80.0, "grid width");

  assert_eq!(fragment.children.len(), 2);
  assert_approx(fragment.children[0].bounds.x(), 0.0, "first column start");
  assert_approx(
    fragment.children[1].bounds.x(),
    20.0,
    "second column start (25% of resolved width)",
  );
}

