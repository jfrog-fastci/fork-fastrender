use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::{GridAutoFlow, GridTrack};
use fastrender::style::values::Length;
use fastrender::{BoxNode, ComputedStyle, FormattingContextType};
use fastrender::FormattingContext;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{}: got {} expected {}",
    msg,
    val,
    expected
  );
}

#[test]
fn column_subgrid_auto_span_is_derived_from_line_names() {
  // Parent grid has three fixed columns. A column-subgrid child with `grid-column: auto` and a
  // `<line-name-list>` of length 3 should default to spanning 2 columns (tracks = lines - 1).
  //
  // Place an additional sibling afterwards so the span changes auto-placement:
  // - With correct span=2, the third item wraps to the next row.
  // - With span=1 (bug), the third item fits on the first row.
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(50.0)),
    GridTrack::Length(Length::px(70.0)),
    GridTrack::Length(Length::px(90.0)),
  ];
  parent_style.grid_template_rows =
    vec![GridTrack::Length(Length::px(20.0)), GridTrack::Length(Length::px(20.0))];
  parent_style.grid_column_gap = Length::px(10.0);
  parent_style.width = Some(Length::px(230.0));

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  let first = BoxNode::new_block(
    Arc::new(first_style),
    FormattingContextType::Block,
    vec![],
  );

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.subgrid_column_line_names = vec![
    vec!["a".into()],
    vec!["b".into()],
    vec!["c".into()],
  ];
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![],
  );

  let mut third_style = ComputedStyle::default();
  third_style.display = Display::Block;
  let third = BoxNode::new_block(
    Arc::new(third_style),
    FormattingContextType::Block,
    vec![],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![first, subgrid, third],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(300.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[1];
  let third_fragment = &fragment.children[2];

  assert_approx(
    subgrid_fragment.bounds.width(),
    70.0 + 10.0 + 90.0,
    "subgrid spans two parent columns (+ gap)",
  );
  assert_approx(
    third_fragment.bounds.y(),
    20.0,
    "third item wrapped to second row due to subgrid span",
  );
}

#[test]
fn row_subgrid_auto_span_is_derived_from_line_names() {
  // Same behaviour as `column_subgrid_auto_span_is_derived_from_line_names`, but for the row axis.
  // Use `grid-auto-flow: column` so the row span is on the primary axis and affects placement.
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_auto_flow = GridAutoFlow::Column;
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(50.0)),
  ];
  parent_style.grid_template_columns =
    vec![GridTrack::Length(Length::px(80.0)), GridTrack::Length(Length::px(80.0))];
  parent_style.grid_row_gap = Length::px(7.0);
  parent_style.grid_column_gap = Length::px(5.0);
  parent_style.width = Some(Length::px(165.0));

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  let first = BoxNode::new_block(
    Arc::new(first_style),
    FormattingContextType::Block,
    vec![],
  );

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.subgrid_row_line_names =
    vec![vec!["a".into()], vec!["b".into()], vec!["c".into()]];
  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![],
  );

  let mut third_style = ComputedStyle::default();
  third_style.display = Display::Block;
  let third = BoxNode::new_block(
    Arc::new(third_style),
    FormattingContextType::Block,
    vec![],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![first, subgrid, third],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(300.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[1];
  let third_fragment = &fragment.children[2];

  assert_approx(
    subgrid_fragment.bounds.height(),
    40.0 + 7.0 + 50.0,
    "subgrid spans two parent rows (+ gap)",
  );
  assert_approx(
    third_fragment.bounds.x(),
    80.0 + 5.0,
    "third item moved to second column due to subgrid row span",
  );
}
