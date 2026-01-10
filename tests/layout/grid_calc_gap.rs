use std::sync::Arc;

use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::GridTrack;
use fastrender::style::values::CalcLength;
use fastrender::style::values::Length;
use fastrender::style::values::LengthUnit;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextType;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

fn calc_percent_plus_px(percent: f32, px: f32) -> Length {
  let calc = CalcLength::single(LengthUnit::Percent, percent)
    .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
    .expect("calc expression should be representable");
  Length::calc(calc)
}

#[test]
fn grid_column_gap_calc_percentage_resolves_against_container_inline_size() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_column_gap_is_normal = false;
  parent_style.grid_column_gap = calc_percent_plus_px(10.0, -5.0);
  parent_style.width = Some(Length::px(200.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(10.0));

  let child1 = BoxNode::new_block(Arc::new(child_style.clone()), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 50.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 2);
  let first = &fragment.children[0];
  let second = &fragment.children[1];

  // 10% of 200px = 20px; calc(20px - 5px) = 15px.
  assert_approx(first.bounds.x(), 0.0, "first column origin");
  assert_approx(first.bounds.width(), 30.0, "first column width");
  assert_approx(second.bounds.x(), 45.0, "second column offset includes resolved gap");
  assert_approx(second.bounds.width(), 40.0, "second column width");
}

#[test]
fn grid_row_gap_calc_percentage_resolves_against_container_inline_size() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![GridTrack::Length(Length::px(50.0))];
  parent_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(10.0)),
    GridTrack::Length(Length::px(10.0)),
  ];
  parent_style.grid_row_gap_is_normal = false;
  parent_style.grid_row_gap = calc_percent_plus_px(10.0, -5.0);
  // Make the inline size (width) differ from the block size so the percentage base is observable.
  parent_style.width = Some(Length::px(200.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(10.0));

  let child1 = BoxNode::new_block(Arc::new(child_style.clone()), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Grid,
    vec![child1, child2],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 300.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 2);
  let first = &fragment.children[0];
  let second = &fragment.children[1];

  // Row-gap percentage resolution uses the grid container's inline size (200px):
  // 10% of 200px = 20px; calc(20px - 5px) = 15px.
  assert_approx(first.bounds.y(), 0.0, "first row origin");
  assert_approx(first.bounds.height(), 10.0, "first row height");
  assert_approx(second.bounds.y(), 25.0, "second row offset includes resolved gap");
  assert_approx(second.bounds.height(), 10.0, "second row height");
}

#[test]
fn grid_row_gap_calc_percentage_inherits_to_row_subgrid() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  parent_style.grid_template_columns = vec![GridTrack::Auto];
  parent_style.grid_row_gap_is_normal = false;
  parent_style.grid_row_gap = calc_percent_plus_px(10.0, -5.0);
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

  // 10% of 200px = 20px; calc(20px - 5px) = 15px.
  assert_approx(first.bounds.y(), 0.0, "first row origin");
  assert_approx(first.bounds.height(), 15.0, "first row size");
  assert_approx(second.bounds.y(), 30.0, "second row offset includes resolved gap");
  assert_approx(second.bounds.height(), 25.0, "second row size");
}

#[test]
fn grid_column_gap_calc_percentage_inherits_to_column_subgrid() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(40.0)),
  ];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  parent_style.grid_column_gap_is_normal = false;
  parent_style.grid_column_gap = calc_percent_plus_px(10.0, -5.0);
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

  // 10% of 200px = 20px; calc(20px - 5px) = 15px.
  assert_approx(first.bounds.x(), 0.0, "first column origin");
  assert_approx(first.bounds.width(), 30.0, "first column width");
  assert_approx(second.bounds.x(), 45.0, "second column offset includes resolved gap");
  assert_approx(second.bounds.width(), 40.0, "second column width");
}
