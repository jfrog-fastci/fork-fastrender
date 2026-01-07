use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::GridTrack;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn make_grid(column_raw: &str) -> BoxNode {
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(150.0));
  grid_style.height = Some(Length::px(50.0));
  grid_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(50.0)),
    GridTrack::Length(Length::px(50.0)),
    GridTrack::Length(Length::px(50.0)),
  ];
  grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(50.0))];
  let grid_style = Arc::new(grid_style);

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  // `grid-column-start: 2 span` (or `grid-column: 2 span`) is spec-valid and should behave like
  // `span 2`. This exercises the raw `<grid-line>` parsing in the grid context.
  second_style.grid_column_raw = Some(column_raw.to_string());
  let second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);

  BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![first, second])
}

#[test]
fn grid_column_span_token_order_matches_span_syntax() {
  let fc = GridFormattingContext::new();
  let constraints = LayoutConstraints::definite(150.0, 50.0);

  let span_fragment = fc
    .layout(&make_grid("span 2"), &constraints)
    .expect("layout succeeds");
  let reordered_fragment = fc
    .layout(&make_grid("2 span"), &constraints)
    .expect("layout succeeds");

  let span_item = &span_fragment.children[1];
  let reordered_item = &reordered_fragment.children[1];

  assert!(
    (span_item.bounds.x() - reordered_item.bounds.x()).abs() <= 0.5,
    "expected `2 span` x ({}) to match `span 2` x ({})",
    reordered_item.bounds.x(),
    span_item.bounds.x()
  );
  assert!(
    (span_item.bounds.width() - reordered_item.bounds.width()).abs() <= 0.5,
    "expected `2 span` width ({}) to match `span 2` width ({})",
    reordered_item.bounds.width(),
    span_item.bounds.width()
  );

  assert!(
    (reordered_item.bounds.x() - 50.0).abs() <= 0.5,
    "expected `2 span` to auto-place at column 2, got x={}",
    reordered_item.bounds.x()
  );
  assert!(
    (reordered_item.bounds.width() - 100.0).abs() <= 0.5,
    "expected `2 span` to span two columns (100px), got width={}",
    reordered_item.bounds.width()
  );
}

