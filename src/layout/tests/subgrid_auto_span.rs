use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::style::display::Display;
use crate::style::types::{AlignContent, AlignItems, GridAutoFlow, GridTrack, JustifyContent, WritingMode};
use crate::style::values::Length;
use crate::FormattingContext;
use crate::{BoxNode, ComputedStyle, FormattingContextType};
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
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(20.0)),
  ];
  parent_style.grid_column_gap = Length::px(10.0);
  parent_style.width = Some(Length::px(230.0));

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.subgrid_column_line_names =
    vec![vec!["a".into()], vec!["b".into()], vec!["c".into()]];
  let subgrid = BoxNode::new_block(Arc::new(subgrid_style), FormattingContextType::Grid, vec![]);

  let mut third_style = ComputedStyle::default();
  third_style.display = Display::Block;
  let third = BoxNode::new_block(Arc::new(third_style), FormattingContextType::Block, vec![]);

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
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(80.0)),
    GridTrack::Length(Length::px(80.0)),
  ];
  parent_style.grid_row_gap = Length::px(7.0);
  parent_style.grid_column_gap = Length::px(5.0);
  parent_style.width = Some(Length::px(165.0));

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.subgrid_row_line_names = vec![vec!["a".into()], vec!["b".into()], vec!["c".into()]];
  let subgrid = BoxNode::new_block(Arc::new(subgrid_style), FormattingContextType::Grid, vec![]);

  let mut third_style = ComputedStyle::default();
  third_style.display = Display::Block;
  let third = BoxNode::new_block(Arc::new(third_style), FormattingContextType::Block, vec![]);

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

#[test]
fn nested_subgrid_auto_span_defaults_to_all_parent_tracks_when_line_name_list_omitted() {
  // Mirrors WPT `css/subgrid/subgrid-nested-writing-mode-001`.
  //
  // A nested subgrid chain with `grid-template-columns/rows: subgrid` and *no explicit placement*
  // should default to spanning all parent tracks. Otherwise, grandchildren placed into column 2 get
  // clamped into column 1.
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(28.0)),
    GridTrack::Length(Length::px(42.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_column_gap = Length::px(5.0);
  parent_style.width = Some(Length::px(75.0));
  // Style parsing normally populates line-name vectors (tracks + 1). The auto-span synthesis relies
  // on this length.
  parent_style.grid_column_line_names = vec![Vec::new(), Vec::new(), Vec::new()];
  parent_style.grid_row_line_names = vec![Vec::new(), Vec::new()];

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Grid;
  outer_style.writing_mode = WritingMode::VerticalRl;
  outer_style.grid_column_subgrid = true;
  outer_style.grid_row_subgrid = true;
  outer_style.justify_content = JustifyContent::Start;
  outer_style.align_content = AlignContent::Start;

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Grid;
  inner_style.writing_mode = WritingMode::VerticalRl;
  inner_style.grid_column_subgrid = true;
  inner_style.grid_row_subgrid = true;
  inner_style.justify_content = JustifyContent::Start;
  inner_style.align_content = AlignContent::Start;

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  first_style.justify_self = Some(AlignItems::Start);
  first_style.align_self = Some(AlignItems::Start);
  first_style.grid_column_start = 1;
  first_style.grid_column_end = 2;
  first_style.grid_row_start = 1;
  first_style.grid_row_end = 2;
  first_style.height = Some(Length::px(12.0));

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  second_style.justify_self = Some(AlignItems::Start);
  second_style.align_self = Some(AlignItems::Start);
  second_style.grid_column_start = 2;
  second_style.grid_column_end = 3;
  second_style.grid_row_start = 1;
  second_style.grid_row_end = 2;
  second_style.height = Some(Length::px(12.0));

  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  let second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);

  let inner = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Grid,
    vec![first, second],
  );
  let outer = BoxNode::new_block(Arc::new(outer_style), FormattingContextType::Grid, vec![inner]);
  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![outer],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let outer_fragment = &fragment.children[0];
  let inner_fragment = &outer_fragment.children[0];
  assert_eq!(inner_fragment.children.len(), 2);
  let first = &inner_fragment.children[0];
  let second = &inner_fragment.children[1];

  // The nested subgrids use a vertical writing-mode, so grid columns map to the physical Y axis.
  // When auto-span is synthesized correctly, the second item should land in the second inherited
  // column track (y=28px + 5px gap).
  assert_approx(first.bounds.y(), 0.0, "first column y");
  assert_approx(second.bounds.y(), 33.0, "second column y (gap + first track)");
}
