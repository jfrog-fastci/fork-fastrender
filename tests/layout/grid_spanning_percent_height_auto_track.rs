use std::sync::Arc;

use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::AlignItems;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::GridTrack;
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::ComputedStyle;

fn find_fragment_by_box_id<'a>(root: &'a FragmentNode, target: usize) -> Option<&'a FragmentNode> {
  if matches!(
    root.content,
    FragmentContent::Block { box_id: Some(id) } | FragmentContent::Replaced { box_id: Some(id), .. }
      if id == target
  ) {
    return Some(root);
  }

  for child in root.children.iter() {
    if let Some(found) = find_fragment_by_box_id(child, target) {
      return Some(found);
    }
  }
  None
}

#[test]
fn grid_spanning_item_percent_height_does_not_resolve_against_auto_tracks() {
  // Regression test for grid track sizing with spanning items that contain percentage-height
  // descendants.
  //
  // When the grid container uses content-sized (`auto`) tracks on the block axis, the grid area's
  // block size is not a definite percentage basis (CSS2.1 §10.5). Percentage heights inside items
  // should therefore compute to `auto` even if Taffy provides intermediate definite height probes
  // while sizing spanning tracks.

  let fixed_child_id = 300usize;
  let flex_id = 200usize;
  let spanning_id = 100usize;
  let row1_id = 101usize;
  let row2_id = 102usize;

  let mut fixed_child_style = ComputedStyle::default();
  fixed_child_style.display = Display::Block;
  fixed_child_style.height = Some(Length::px(10.0));
  let mut fixed_child = BoxNode::new_block(
    Arc::new(fixed_child_style),
    FormattingContextType::Block,
    vec![],
  );
  fixed_child.id = fixed_child_id;

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Column;
  flex_style.align_items = AlignItems::Stretch;
  flex_style.height = Some(Length::percent(100.0));
  let mut flex_box = BoxNode::new_block(
    Arc::new(flex_style),
    FormattingContextType::Flex,
    vec![fixed_child],
  );
  flex_box.id = flex_id;

  let mut spanning_style = ComputedStyle::default();
  spanning_style.display = Display::Block;
  spanning_style.grid_column_start = 1;
  spanning_style.grid_column_end = 2;
  spanning_style.grid_row_start = 1;
  spanning_style.grid_row_end = 3;
  let mut spanning = BoxNode::new_block(
    Arc::new(spanning_style),
    FormattingContextType::Block,
    vec![flex_box],
  );
  spanning.id = spanning_id;

  let mut row1_style = ComputedStyle::default();
  row1_style.display = Display::Block;
  row1_style.height = Some(Length::px(20.0));
  row1_style.grid_column_start = 2;
  row1_style.grid_column_end = 3;
  row1_style.grid_row_start = 1;
  row1_style.grid_row_end = 2;
  let mut row1 = BoxNode::new_block(Arc::new(row1_style), FormattingContextType::Block, vec![]);
  row1.id = row1_id;

  let mut row2_style = ComputedStyle::default();
  row2_style.display = Display::Block;
  row2_style.height = Some(Length::px(30.0));
  row2_style.grid_column_start = 2;
  row2_style.grid_column_end = 3;
  row2_style.grid_row_start = 2;
  row2_style.grid_row_end = 3;
  let mut row2 = BoxNode::new_block(Arc::new(row2_style), FormattingContextType::Block, vec![]);
  row2.id = row2_id;

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(100.0)),
    GridTrack::Length(Length::px(100.0)),
  ];
  grid_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  grid_style.justify_items = AlignItems::Stretch;
  grid_style.align_items = AlignItems::Stretch;
  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![spanning, row1, row2],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(
      &grid,
      // Provide a definite available height (like a viewport) even though the grid itself is
      // content-sized (`height:auto`). The spanning item must not treat this as a definite basis
      // for its `height: 100%` descendant.
      &LayoutConstraints::new(
        AvailableSpace::Definite(200.0),
        AvailableSpace::Definite(500.0),
      ),
    )
    .expect("layout succeeds");

  let spanning_fragment =
    find_fragment_by_box_id(&fragment, spanning_id).expect("spanning fragment");
  let row1_fragment = find_fragment_by_box_id(&fragment, row1_id).expect("row1 fragment");
  let row2_fragment = find_fragment_by_box_id(&fragment, row2_id).expect("row2 fragment");
  let flex_fragment = find_fragment_by_box_id(spanning_fragment, flex_id).expect("flex fragment");

  assert!(
    (row1_fragment.bounds.y() - 0.0).abs() < 0.5,
    "expected row1 item to start at y=0 (got {})",
    row1_fragment.bounds.y()
  );
  assert!(
    (row2_fragment.bounds.y() - 20.0).abs() < 0.5,
    "expected row2 item to start at y=20 (got {})",
    row2_fragment.bounds.y()
  );
  assert!(
    (spanning_fragment.bounds.height() - 50.0).abs() < 0.5,
    "expected spanning item to cover both auto tracks (got {})",
    spanning_fragment.bounds.height()
  );
  assert!(
    (flex_fragment.bounds.height() - 10.0).abs() < 0.5,
    "expected `height:100%` descendant to compute to auto in auto tracks (got {})",
    flex_fragment.bounds.height()
  );
}

#[test]
fn grid_spanning_flex_item_percent_height_does_not_resolve_against_auto_tracks() {
  // Like `grid_spanning_item_percent_height_does_not_resolve_against_auto_tracks`, but the grid
  // item itself establishes a flex formatting context. This exercises the `used_border_box_size_*`
  // override path in flex layout.

  let fixed_child_id = 400usize;
  let percent_child_id = 300usize;
  let spanning_id = 200usize;
  let row1_id = 101usize;
  let row2_id = 102usize;

  let mut fixed_child_style = ComputedStyle::default();
  fixed_child_style.display = Display::Block;
  fixed_child_style.height = Some(Length::px(10.0));
  let mut fixed_child = BoxNode::new_block(
    Arc::new(fixed_child_style),
    FormattingContextType::Block,
    vec![],
  );
  fixed_child.id = fixed_child_id;

  let mut percent_child_style = ComputedStyle::default();
  percent_child_style.display = Display::Block;
  percent_child_style.height = Some(Length::percent(100.0));
  let mut percent_child = BoxNode::new_block(
    Arc::new(percent_child_style),
    FormattingContextType::Block,
    vec![fixed_child],
  );
  percent_child.id = percent_child_id;

  let mut spanning_style = ComputedStyle::default();
  spanning_style.display = Display::Flex;
  spanning_style.flex_direction = FlexDirection::Column;
  spanning_style.align_items = AlignItems::Stretch;
  spanning_style.grid_column_start = 1;
  spanning_style.grid_column_end = 2;
  spanning_style.grid_row_start = 1;
  spanning_style.grid_row_end = 3;
  let mut spanning = BoxNode::new_block(
    Arc::new(spanning_style),
    FormattingContextType::Flex,
    vec![percent_child],
  );
  spanning.id = spanning_id;

  let mut row1_style = ComputedStyle::default();
  row1_style.display = Display::Block;
  row1_style.height = Some(Length::px(20.0));
  row1_style.grid_column_start = 2;
  row1_style.grid_column_end = 3;
  row1_style.grid_row_start = 1;
  row1_style.grid_row_end = 2;
  let mut row1 = BoxNode::new_block(Arc::new(row1_style), FormattingContextType::Block, vec![]);
  row1.id = row1_id;

  let mut row2_style = ComputedStyle::default();
  row2_style.display = Display::Block;
  row2_style.height = Some(Length::px(30.0));
  row2_style.grid_column_start = 2;
  row2_style.grid_column_end = 3;
  row2_style.grid_row_start = 2;
  row2_style.grid_row_end = 3;
  let mut row2 = BoxNode::new_block(Arc::new(row2_style), FormattingContextType::Block, vec![]);
  row2.id = row2_id;

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(100.0)),
    GridTrack::Length(Length::px(100.0)),
  ];
  grid_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  grid_style.justify_items = AlignItems::Stretch;
  grid_style.align_items = AlignItems::Stretch;
  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![spanning, row1, row2],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(
      &grid,
      &LayoutConstraints::new(
        AvailableSpace::Definite(200.0),
        AvailableSpace::Definite(500.0),
      ),
    )
    .expect("layout succeeds");

  let spanning_fragment =
    find_fragment_by_box_id(&fragment, spanning_id).expect("spanning fragment");
  let percent_child_fragment =
    find_fragment_by_box_id(spanning_fragment, percent_child_id).expect("percent child fragment");
  let row1_fragment = find_fragment_by_box_id(&fragment, row1_id).expect("row1 fragment");
  let row2_fragment = find_fragment_by_box_id(&fragment, row2_id).expect("row2 fragment");

  assert!(
    (row1_fragment.bounds.y() - 0.0).abs() < 0.5,
    "expected row1 item to start at y=0 (got {})",
    row1_fragment.bounds.y()
  );
  assert!(
    (row2_fragment.bounds.y() - 20.0).abs() < 0.5,
    "expected row2 item to start at y=20 (got {})",
    row2_fragment.bounds.y()
  );
  assert!(
    (spanning_fragment.bounds.height() - 50.0).abs() < 0.5,
    "expected spanning item to cover both auto tracks (got {})",
    spanning_fragment.bounds.height()
  );
  assert!(
    (percent_child_fragment.bounds.height() - 10.0).abs() < 0.5,
    "expected `height:100%` child to compute to auto in auto tracks (got {})",
    percent_child_fragment.bounds.height()
  );
}
