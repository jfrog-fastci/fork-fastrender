use std::sync::Arc;

use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::GridTrack;
use fastrender::style::types::IntrinsicSizeKeyword;
use fastrender::style::types::WritingMode;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FragmentNode;

fn find_first_fragment_with_id<'a>(
  fragment: &'a FragmentNode,
  id: usize,
) -> Option<&'a FragmentNode> {
  if fragment
    .box_id()
    .is_some_and(|fragment_id| fragment_id == id)
  {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_first_fragment_with_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn fill_available_block(id: usize) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width_keyword = Some(IntrinsicSizeKeyword::FillAvailable);

  let mut node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
  node.id = id;
  node
}

#[test]
fn grid_fill_available_keyword_resolves_against_physical_width_in_vertical_writing_mode() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.width_keyword = Some(IntrinsicSizeKeyword::FillAvailable);
  container_style.grid_template_columns = vec![GridTrack::Length(Length::px(10.0))];
  container_style.grid_template_rows = vec![GridTrack::Length(Length::px(10.0))];
  let container_style = Arc::new(container_style);

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  let child_style = Arc::new(child_style);

  let mut child = BoxNode::new_block(child_style, FormattingContextType::Block, vec![]);
  child.id = 2;

  let mut grid = BoxNode::new_block(container_style, FormattingContextType::Grid, vec![child]);
  grid.id = 1;

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 400.0))
    .expect("grid layout succeeds");

  let actual = fragment.bounds.width();
  let expected = 200.0;
  assert!(
    (actual - expected).abs() < 0.5,
    "expected fill-available to resolve against physical width ({expected}), got {actual}",
  );
}

#[test]
fn grid_item_fill_available_uses_physical_width_in_vertical_writing_mode() {
  // Regression: In vertical writing modes, grid layout receives LayoutConstraints in physical
  // width/height axes. Intrinsic keyword resolution must therefore use constraints.available_width
  // for resolving width keywords, not constraints.available_height.
  let container_width = 200.0;
  let container_height = 80.0;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.writing_mode = WritingMode::VerticalRl;
  // When writing-mode is vertical, grid rows map to the physical X axis (width) in Taffy.
  container_style.grid_template_rows = vec![GridTrack::Length(Length::px(container_width))];
  container_style.grid_template_columns = vec![GridTrack::Length(Length::px(container_height))];
  let container_style = Arc::new(container_style);

  let grid_item_id = 20;
  let item = fill_available_block(grid_item_id);

  let mut grid = BoxNode::new_block(container_style, FormattingContextType::Grid, vec![item]);
  grid.id = 1;

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(
      &grid,
      &LayoutConstraints::definite(container_width, container_height),
    )
    .expect("grid layout succeeds");

  let item_fragment =
    find_first_fragment_with_id(&fragment, grid_item_id).expect("grid item fragment");

  let actual_width = item_fragment.bounds.width();
  assert!(
    (actual_width - container_width).abs() <= 0.5,
    "expected grid item width≈{container_width:.2}, got {actual_width:.2}"
  );
}
