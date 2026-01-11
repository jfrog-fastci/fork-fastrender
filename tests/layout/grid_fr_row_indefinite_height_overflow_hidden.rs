use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::GridTrack;
use fastrender::style::types::Overflow;
use fastrender::style::values::Length;
use fastrender::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn grid_fr_row_sizes_to_content_when_container_height_is_indefinite_even_if_item_overflow_hidden() {
  // Regression test for `grid-template-rows: 1fr` with an auto-sized grid container.
  //
  // When the grid container's block size is indefinite, `fr` tracks must behave like content-sized
  // tracks so the container's used height can grow to fit its in-flow content.
  //
  // This matters for real pages like wired.com, whose sticky nav rows use:
  //   grid-template-rows: 1fr;
  //   height: 100%;
  // with the percentage height computing to `auto` because the containing block height is not
  // definite (CSS2.1 §10.5). If the `1fr` row collapses to 0px, overflow-hidden descendants clip
  // the nav content and the header disappears.

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(100.0));
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
  grid_style.grid_template_rows = vec![GridTrack::Fr(1.0)];

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Block;
  inner_style.height = Some(Length::px(80.0));
  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.overflow_x = Overflow::Hidden;
  item_style.overflow_y = Overflow::Hidden;
  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![inner]);

  let mut grid = BoxNode::new_block(Arc::new(grid_style), FormattingContextType::Grid, vec![item]);
  grid.id = 1;

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(
      &grid,
      &LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite),
    )
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 1);
  let item_fragment = fragment.children.first().expect("grid item fragment");

  assert_approx(fragment.bounds.height(), 80.0, "grid height");
  assert_approx(item_fragment.bounds.height(), 80.0, "grid item height");
}

