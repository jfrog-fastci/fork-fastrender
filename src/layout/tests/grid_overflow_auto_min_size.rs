use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::style::display::Display;
use crate::style::types::GridTrack;
use crate::style::types::Overflow;
use crate::style::types::WhiteSpace;
use crate::style::values::Length;
use crate::BoxNode;
use crate::ComputedStyle;
use crate::FormattingContext;
use crate::FormattingContextType;
use std::sync::Arc;

fn layout_grid_child_width(overflow_x: Overflow) -> f32 {
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(200.0));
  grid_style.height = Some(Length::px(50.0));
  // 1fr is `minmax(auto, 1fr)`; we rely on the `auto` minimum to demonstrate the difference
  // between content-based min sizing (overflow: visible) and scroll container min sizing
  // (overflow: auto).
  grid_style.grid_template_columns = vec![GridTrack::Fr(1.0)];

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.overflow_x = overflow_x;
  item_style.white_space = WhiteSpace::Nowrap;

  let mut text_style = ComputedStyle::default();
  text_style.white_space = WhiteSpace::Nowrap;
  let long_word = "W".repeat(200);
  let text = BoxNode::new_text(Arc::new(text_style), long_word);

  let item = BoxNode::new_block(
    Arc::new(item_style),
    FormattingContextType::Block,
    vec![text],
  );
  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![item],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 50.0))
    .expect("layout succeeds");
  assert_eq!(fragment.children.len(), 1);
  fragment.children[0].bounds.width()
}

#[test]
fn grid_overflow_auto_allows_fr_track_to_shrink() {
  let visible_width = layout_grid_child_width(Overflow::Visible);
  let auto_width = layout_grid_child_width(Overflow::Auto);

  assert!(
    visible_width > 200.5,
    "overflow: visible should expand the 1fr track due to content-based automatic min size (got {:.2})",
    visible_width
  );
  assert!(
    (auto_width - 200.0).abs() < 0.5,
    "overflow: auto should make the grid item a scroll container so the 1fr track can shrink to the container width (got {:.2})",
    auto_width
  );
}
