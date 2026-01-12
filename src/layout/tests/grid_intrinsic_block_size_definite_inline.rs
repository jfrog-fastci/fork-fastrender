use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::FlexDirection;
use crate::style::types::FlexWrap;
use crate::style::types::GridTrack;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn grid_intrinsic_block_size_probes_use_definite_inline_size() {
  let fc = GridFormattingContext::new();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(200.0));
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
  grid_style.grid_template_rows = vec![GridTrack::Auto];
  let grid_style = Arc::new(grid_style);

  // A wrapping flex container whose block size depends heavily on the available inline size.
  // At 200px wide, 60px children wrap into 4 lines (height 40px). When measured at the min-content
  // inline size (~60px), they wrap into 10 lines (height 100px). Grid track sizing probes must use
  // the definite grid area width, otherwise `auto` rows become far too tall (NatGeo pageset).
  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Row;
  flex_style.flex_wrap = FlexWrap::Wrap;
  flex_style.grid_column_start = 1;
  flex_style.grid_column_end = 2;
  flex_style.grid_row_start = 1;
  flex_style.grid_row_end = 2;
  let mut flex_children = Vec::new();
  for _ in 0..10 {
    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.width = Some(Length::px(60.0));
    child_style.height = Some(Length::px(10.0));
    child_style.flex_shrink = 0.0;
    child_style.flex_grow = 0.0;
    flex_children.push(BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![],
    ));
  }
  let flex_node = BoxNode::new_block(
    Arc::new(flex_style),
    FormattingContextType::Flex,
    flex_children,
  );

  let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![flex_node]);

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("grid layout");

  assert_eq!(fragment.children.len(), 1);
  assert_approx(
    fragment.children[0].bounds.height(),
    40.0,
    "grid item height",
  );
  assert_approx(fragment.bounds.height(), 40.0, "grid height");
}
