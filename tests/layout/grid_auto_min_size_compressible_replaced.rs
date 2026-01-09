use fastrender::geometry::Size;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::GridTrack;
use fastrender::style::types::Overflow;
use fastrender::style::values::Length;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use std::sync::Arc;

#[test]
fn grid_auto_min_size_compressible_replaced_caps_content_based_min() {
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(50.0));
  grid_style.grid_template_columns =
    vec![GridTrack::Fr(1.0), GridTrack::Length(Length::px(0.0))];
  grid_style.grid_template_rows = vec![GridTrack::Auto];

  let mut replaced_style = ComputedStyle::default();
  replaced_style.display = Display::Block;
  replaced_style.overflow_x = Overflow::Visible;
  replaced_style.overflow_y = Overflow::Visible;
  replaced_style.max_width = Some(Length::px(50.0));
  replaced_style.grid_column_start = 1;
  replaced_style.grid_column_end = 2;

  let replaced = BoxNode::new_replaced(
    Arc::new(replaced_style),
    ReplacedType::Canvas,
    Some(Size::new(1000.0, 10.0)),
    None,
  );

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.width = Some(Length::px(1.0));
  item_style.height = Some(Length::px(1.0));
  item_style.grid_column_start = 2;
  item_style.grid_column_end = 3;

  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![replaced, item],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(50.0, 20.0))
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
    (x - 50.0).abs() < eps,
    "expected second grid item to start at ~50px due to compressible replaced min-size capping, got {x:.2}"
  );
}

