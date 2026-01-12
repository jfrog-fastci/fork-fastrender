use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::AlignItems;
use fastrender::style::types::GridTrack;
use fastrender::style::values::Length;
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

fn find_fragment_with_id<'a>(
  fragment: &'a fastrender::FragmentNode,
  id: usize,
) -> Option<&'a fastrender::FragmentNode> {
  if fragment
    .box_id()
    .is_some_and(|fragment_id| fragment_id == id)
  {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment_with_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn grid_items_do_not_collapse_margins_with_children() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(100.0));
  container_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
  container_style.grid_template_rows = vec![GridTrack::Auto];
  container_style.align_items = AlignItems::Start;

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(20.0));
  child_style.margin_bottom = Some(Length::px(10.0));

  let mut grandchild =
    BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  grandchild.id = 3;

  let mut item = BoxNode::new_block(
    Arc::new(item_style),
    FormattingContextType::Block,
    vec![grandchild],
  );
  item.id = 2;

  let mut grid = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![item],
  );
  grid.id = 1;

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite_width(100.0))
    .expect("layout succeeds");

  let item_fragment = find_fragment_with_id(&fragment, 2).expect("grid item fragment");
  assert_approx(
    item_fragment.bounds.height(),
    30.0,
    "grid item height should include the child's bottom margin",
  );
}
