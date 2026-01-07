use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::GridTrack;
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
fn row_gap_percentage_resolves_against_inline_size_and_inherits_to_row_subgrid() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.grid_template_columns = vec![GridTrack::Auto];
  parent_style.grid_row_gap = Length::percent(10.0);
  parent_style.width = Some(Length::px(200.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_row_subgrid = true;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 3;

  let mut child1_style = ComputedStyle::default();
  child1_style.display = Display::Block;
  child1_style.height = Some(Length::px(15.0));

  let mut child2_style = ComputedStyle::default();
  child2_style.display = Display::Block;
  child2_style.height = Some(Length::px(25.0));

  let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_eq!(subgrid_fragment.children.len(), 2);
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];

  // 10% row-gap resolves against the container inline-size (200px) => 20px.
  assert_approx(first.bounds.y(), 0.0, "first row origin");
  assert_approx(first.bounds.height(), 15.0, "first row size");
  assert_approx(second.bounds.y(), 35.0, "second row offset includes gap");
  assert_approx(second.bounds.height(), 25.0, "second row size");
}

#[test]
fn column_gap_percentage_inherits_to_column_subgrid() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_column_gap = Length::percent(10.0);
  parent_style.width = Some(Length::px(200.0));

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 3;

  let mut child1_style = ComputedStyle::default();
  child1_style.display = Display::Block;
  child1_style.height = Some(Length::px(10.0));

  let mut child2_style = ComputedStyle::default();
  child2_style.display = Display::Block;
  child2_style.height = Some(Length::px(10.0));

  let child1 = BoxNode::new_block(Arc::new(child1_style), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child2_style), FormattingContextType::Block, vec![]);

  let subgrid = BoxNode::new_block(
    Arc::new(subgrid_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![subgrid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  let subgrid_fragment = &fragment.children[0];
  assert_eq!(subgrid_fragment.children.len(), 2);
  let first = &subgrid_fragment.children[0];
  let second = &subgrid_fragment.children[1];

  // 10% column-gap resolves against the container inline-size (200px) => 20px.
  assert_approx(first.bounds.x(), 0.0, "first column origin");
  assert_approx(first.bounds.width(), 30.0, "first column width");
  assert_approx(second.bounds.x(), 50.0, "second column offset includes gap");
  assert_approx(second.bounds.width(), 40.0, "second column width");
}

