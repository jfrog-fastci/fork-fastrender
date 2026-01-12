use crate::geometry::Size;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::GridTrack;
use crate::style::values::Length;
use crate::tree::box_tree::ReplacedType;
use crate::BoxNode;
use crate::ComputedStyle;
use std::sync::Arc;

#[test]
fn grid_fr_tracks_allow_replaced_percentage_width_to_compress_min_content() {
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(200.0));
  grid_style.height = Some(Length::px(50.0));
  grid_style.grid_template_columns = vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0)];
  grid_style.grid_template_rows = vec![GridTrack::Auto];

  let mut image_style = ComputedStyle::default();
  image_style.display = Display::Block;
  image_style.width = Some(Length::percent(100.0));

  let make_item = |col_start: i32| {
    let mut item_style = ComputedStyle::default();
    item_style.display = Display::Block;
    item_style.grid_column_start = col_start;
    item_style.grid_column_end = col_start + 1;

    let image = BoxNode::new_replaced(
      Arc::new(image_style.clone()),
      ReplacedType::Canvas,
      Some(Size::new(1000.0, 100.0)),
      None,
    );

    BoxNode::new_block(
      Arc::new(item_style),
      FormattingContextType::Block,
      vec![image],
    )
  };

  let item1 = make_item(1);
  let item2 = make_item(2);

  // Sanity-check the primitive this test depends on: the grid items should have a *min-content*
  // intrinsic width of ~0 when their replaced children use cyclic percentage widths, but still
  // report a large max-content intrinsic width based on their natural size.
  let bfc = BlockFormattingContext::new();
  let image_node = item1
    .children
    .get(0)
    .expect("grid item contains replaced child");
  let (image_min, image_max) = bfc
    .compute_intrinsic_inline_sizes(image_node)
    .expect("intrinsic sizing succeeds");
  assert!(
    image_min.abs() < 0.5,
    "expected replaced min-content intrinsic width to be ~0 (got {image_min:.2})"
  );
  assert!(
    (image_max - 1000.0).abs() < 0.5,
    "expected replaced max-content intrinsic width to match natural size (got {image_max:.2})"
  );
  let (min_intrinsic, max_intrinsic) = bfc
    .compute_intrinsic_inline_sizes(&item1)
    .expect("intrinsic sizing succeeds");
  assert!(
    min_intrinsic.abs() < 0.5,
    "expected min-content intrinsic width to be ~0 (got {min_intrinsic:.2})"
  );
  assert!(
    (max_intrinsic - 1000.0).abs() < 0.5,
    "expected max-content intrinsic width to match natural size (got {max_intrinsic:.2})"
  );

  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![item1, item2],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 50.0))
    .expect("grid layout succeeds");
  assert_eq!(fragment.children.len(), 2);

  let rightmost = fragment
    .children
    .iter()
    .max_by(|a, b| a.bounds.x().partial_cmp(&b.bounds.x()).unwrap())
    .expect("child fragments");
  let x = rightmost.bounds.x();
  let eps = 0.5;

  assert!(
    (x - 100.0).abs() < eps,
    "expected second grid item to start at ~100px with 1fr tracks (got {x:.2})"
  );
}
