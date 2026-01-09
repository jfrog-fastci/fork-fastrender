use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::GridTrack;
use fastrender::style::types::IntrinsicSizeKeyword;
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::ComputedStyle;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn grid_item_definite_height_contributes_to_max_content_row_size() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(100.0));
  container_style.height_keyword = Some(IntrinsicSizeKeyword::MaxContent);
  container_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
  container_style.grid_template_rows = vec![GridTrack::MaxContent, GridTrack::MaxContent];
  let container_style = Arc::new(container_style);

  let mut item1_style = ComputedStyle::default();
  item1_style.display = Display::Block;
  item1_style.width = Some(Length::px(10.0));
  item1_style.height = Some(Length::px(40.0));
  item1_style.grid_row_start = 1;
  item1_style.grid_row_end = 2;
  let item1 = BoxNode::new_block(Arc::new(item1_style), FormattingContextType::Block, vec![]);

  let mut item2_style = ComputedStyle::default();
  item2_style.display = Display::Block;
  item2_style.width = Some(Length::px(10.0));
  item2_style.height = Some(Length::px(10.0));
  item2_style.grid_row_start = 2;
  item2_style.grid_row_end = 3;
  let item2 = BoxNode::new_block(Arc::new(item2_style), FormattingContextType::Block, vec![]);

  let mut grid = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![item1, item2],
  );
  grid.id = 1;

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 2);
  assert_approx(fragment.bounds.height(), 50.0, "grid height");
  assert_approx(fragment.children[0].bounds.y(), 0.0, "first row start");
  assert_approx(fragment.children[1].bounds.y(), 40.0, "second row start");
}

