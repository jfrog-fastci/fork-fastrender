use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::GridTrack;
use fastrender::style::types::IntrinsicSizeKeyword;
use fastrender::style::types::WritingMode;
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::ComputedStyle;
use std::sync::Arc;

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

