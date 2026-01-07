use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::Display;
use fastrender::style::types::GridTrack;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
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
fn nested_subgrid_item_contributions_flow_to_ancestor_tracks() {
  // Outer grid: auto auto columns with a gap. The nested subgrid descendant items should size the
  // ancestor tracks individually (10px for col 1 and 100px for col 2).
  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Grid;
  outer_style.grid_template_columns = vec![GridTrack::Auto, GridTrack::Auto];
  outer_style.grid_template_rows = vec![GridTrack::Auto];
  outer_style.grid_column_gap = Length::px(10.0);

  let mut a_style = ComputedStyle::default();
  a_style.display = Display::Grid;
  a_style.grid_column_subgrid = true;
  a_style.grid_column_start = 1;
  a_style.grid_column_end = 3;

  let mut b_style = ComputedStyle::default();
  b_style.display = Display::Grid;
  b_style.grid_column_subgrid = true;
  b_style.grid_column_start = 1;
  b_style.grid_column_end = 3;

  let mut col1_style = ComputedStyle::default();
  col1_style.display = Display::Block;
  col1_style.width = Some(Length::px(10.0));
  col1_style.grid_column_start = 1;
  col1_style.grid_column_end = 2;

  let mut col2_style = ComputedStyle::default();
  col2_style.display = Display::Block;
  col2_style.width = Some(Length::px(100.0));
  col2_style.grid_column_start = 2;
  col2_style.grid_column_end = 3;

  let leaf1 = BoxNode::new_block(Arc::new(col1_style), FormattingContextType::Block, vec![]);
  let leaf2 = BoxNode::new_block(Arc::new(col2_style), FormattingContextType::Block, vec![]);
  let b = BoxNode::new_block(
    Arc::new(b_style),
    FormattingContextType::Grid,
    vec![leaf1, leaf2],
  );
  let a = BoxNode::new_block(Arc::new(a_style), FormattingContextType::Grid, vec![b]);
  let outer = BoxNode::new_block(Arc::new(outer_style), FormattingContextType::Grid, vec![a]);

  let fc = GridFormattingContext::new();
  let max_content = fc
    .compute_intrinsic_inline_size(&outer, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic size");
  assert_approx(
    max_content,
    120.0,
    "nested subgrid descendant sizes apply to ancestor columns",
  );
}

#[test]
fn nested_subgrid_contributions_respect_inherited_track_offsets() {
  // Variant: the outer grid has three columns, and the nested subgrid spans columns 2-3. The
  // descendant item in the second inherited track must contribute to the outer third column, not be
  // smeared across both inherited tracks.
  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Grid;
  outer_style.grid_template_columns = vec![GridTrack::Auto, GridTrack::Auto, GridTrack::Auto];
  outer_style.grid_template_rows = vec![GridTrack::Auto];
  outer_style.grid_column_gap = Length::px(10.0);

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  first_style.width = Some(Length::px(15.0));
  first_style.grid_column_start = 1;
  first_style.grid_column_end = 2;
  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);

  let mut a_style = ComputedStyle::default();
  a_style.display = Display::Grid;
  a_style.grid_column_subgrid = true;
  a_style.grid_column_start = 2;
  a_style.grid_column_end = 4;

  let mut b_style = ComputedStyle::default();
  b_style.display = Display::Grid;
  b_style.grid_column_subgrid = true;
  b_style.grid_column_start = 1;
  b_style.grid_column_end = 3;

  let mut col1_style = ComputedStyle::default();
  col1_style.display = Display::Block;
  col1_style.width = Some(Length::px(10.0));
  col1_style.grid_column_start = 1;
  col1_style.grid_column_end = 2;

  let mut col2_style = ComputedStyle::default();
  col2_style.display = Display::Block;
  col2_style.width = Some(Length::px(100.0));
  col2_style.grid_column_start = 2;
  col2_style.grid_column_end = 3;

  let leaf1 = BoxNode::new_block(Arc::new(col1_style), FormattingContextType::Block, vec![]);
  let leaf2 = BoxNode::new_block(Arc::new(col2_style), FormattingContextType::Block, vec![]);
  let b = BoxNode::new_block(
    Arc::new(b_style),
    FormattingContextType::Grid,
    vec![leaf1, leaf2],
  );
  let a = BoxNode::new_block(Arc::new(a_style), FormattingContextType::Grid, vec![b]);
  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Grid,
    vec![first, a],
  );

  let fc = GridFormattingContext::new();
  let max_content = fc
    .compute_intrinsic_inline_size(&outer, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic size");
  assert_approx(
    max_content,
    145.0,
    "nested subgrid contribution maps into ancestor grid with offset",
  );
}

