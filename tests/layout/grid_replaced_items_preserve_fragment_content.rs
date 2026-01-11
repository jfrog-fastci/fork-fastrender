use fastrender::geometry::Size;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::GridTrack;
use fastrender::style::values::Length;
use fastrender::tree::box_tree::{ReplacedType, SvgContent};
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use std::sync::Arc;

#[test]
fn grid_keeps_replaced_fragment_content_for_leaf_items() {
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(100.0));
  grid_style.height = Some(Length::px(100.0));
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
  grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(100.0))];

  let mut replaced_style = ComputedStyle::default();
  replaced_style.display = Display::Block;
  replaced_style.width = Some(Length::px(40.0));
  replaced_style.height = Some(Length::px(20.0));
  replaced_style.grid_column_start = 1;
  replaced_style.grid_column_end = 2;
  replaced_style.grid_row_start = 1;
  replaced_style.grid_row_end = 2;

  let replaced_type = ReplacedType::Svg {
    content: SvgContent::raw(
      "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"40\" height=\"20\"><rect width=\"40\" height=\"20\" fill=\"black\"/></svg>",
    ),
  };
  let mut replaced = BoxNode::new_replaced(
    Arc::new(replaced_style),
    replaced_type,
    Some(Size::new(40.0, 20.0)),
    Some(2.0),
  );
  replaced.id = 10;

  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![replaced],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(100.0, 100.0))
    .expect("grid layout succeeds");

  assert_eq!(fragment.children.len(), 1);
  assert!(
    matches!(
      fragment.children[0].content,
      FragmentContent::Replaced { box_id: Some(10), .. }
    ),
    "expected grid leaf replaced element to preserve FragmentContent::Replaced"
  );
}

