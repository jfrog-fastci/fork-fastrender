use std::sync::Arc;

use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::layout::formatting_context::IntrinsicSizingMode;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::AlignItems;
use crate::style::types::GridTrack;
use crate::style::types::IntrinsicSizeKeyword;
use crate::style::types::WordBreak;
use crate::style::types::WritingMode;
use crate::style::values::Length;
use crate::BoxNode;
use crate::ComputedStyle;
use crate::FragmentNode;

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
fn grid_item_fit_content_percent_resolves_against_physical_width_in_vertical_writing_mode() {
  let container_width = 200.0;
  let container_height = 80.0;
  let percent = 50.0;

  let expected = (container_width * percent) / 100.0;
  let wrong_axis = (container_height * percent) / 100.0;

  let make_item = |width_keyword: Option<IntrinsicSizeKeyword>, id: usize| {
    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    item_style.writing_mode = WritingMode::VerticalRl;
    item_style.font_size = 16.0;
    item_style.word_break = WordBreak::BreakAll;
    item_style.width_keyword = width_keyword;
    // Prevent the default grid item stretch behavior from overriding the fit-content width in the
    // physical X axis (block axis for vertical writing modes).
    item_style.align_self = Some(AlignItems::Start);
    item_style.justify_self = Some(AlignItems::Start);
    let item_style = Arc::new(item_style);

    let mut text_style = ComputedStyle::default();
    text_style.writing_mode = WritingMode::VerticalRl;
    text_style.font_size = 16.0;
    text_style.word_break = WordBreak::BreakAll;
    let text_style = Arc::new(text_style);

    let text = "a".repeat(64);
    let text_child = BoxNode::new_text(text_style, text);

    let mut item = BoxNode::new_block(item_style, FormattingContextType::Block, vec![text_child]);
    item.id = id;
    item
  };

  // Ensure the item has a meaningful intrinsic min/max range so `fit-content(<percentage>)`
  // resolves to the preferred percentage size rather than an intrinsic bound.
  let probe_item = make_item(None, 500);
  let bfc = BlockFormattingContext::new();
  let min_intrinsic = bfc
    .compute_intrinsic_block_size(&probe_item, IntrinsicSizingMode::MinContent)
    .expect("min-content intrinsic width");
  let max_intrinsic = bfc
    .compute_intrinsic_block_size(&probe_item, IntrinsicSizingMode::MaxContent)
    .expect("max-content intrinsic width");
  let lo = min_intrinsic.min(max_intrinsic);
  let hi = min_intrinsic.max(max_intrinsic);
  assert!(
    hi - lo > 1.0,
    "expected distinct intrinsic widths (min={min_intrinsic:.2}, max={max_intrinsic:.2})"
  );
  assert!(
    expected > lo && expected < hi,
    "expected {expected:.2}px to be within intrinsic clamp range [{lo:.2}, {hi:.2}]"
  );
  assert!(
    wrong_axis > lo && wrong_axis < hi,
    "expected {wrong_axis:.2}px (wrong-axis resolution) to also fall within [{lo:.2}, {hi:.2}] so the test is sensitive"
  );

  let grid_item_id = 20;
  let item = make_item(
    Some(IntrinsicSizeKeyword::FitContent {
      limit: Some(Length::percent(percent)),
    }),
    grid_item_id,
  );

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.writing_mode = WritingMode::VerticalRl;
  // When writing-mode is vertical, grid rows map to the physical X axis (width) in Taffy.
  container_style.grid_template_rows = vec![GridTrack::Length(Length::px(container_width))];
  container_style.grid_template_columns = vec![GridTrack::Length(Length::px(container_height))];
  let container_style = Arc::new(container_style);

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
  let actual = item_fragment.bounds.width();

  assert!(
    (actual - expected).abs() <= 0.5,
    "expected grid item fit-content({percent:.0}%) width≈{expected:.2}, got {actual:.2} (wrong-axis would yield {wrong_axis:.2})"
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
