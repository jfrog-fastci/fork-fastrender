use std::sync::Arc;

use crate::layout::constraints::{AvailableSpace, LayoutConstraints};
use crate::layout::contexts::block::BlockFormattingContext;
use crate::style::display::{Display, FormattingContextType};
use crate::style::float::{Clear, Float};
use crate::style::types::GridTrack;
use crate::style::values::Length;
use crate::{BoxNode, BoxTree, ComputedStyle, FormattingContext, FragmentNode};

const EPS: f32 = 0.1;

fn block_style() -> ComputedStyle {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style
}

fn find_by_style<'a>(
  node: &'a FragmentNode,
  predicate: &impl Fn(&ComputedStyle) -> bool,
) -> Option<&'a FragmentNode> {
  if let Some(style) = node.style.as_ref() {
    if predicate(style) {
      return Some(node);
    }
  }
  node
    .children
    .iter()
    .find_map(|child| find_by_style(child, predicate))
}

#[test]
fn float_grid_container_lays_out_children_as_grid() {
  let root_style = Arc::new(block_style());

  let mut item1_style = block_style();
  item1_style.width = Some(Length::px(20.0));
  item1_style.width_keyword = None;
  item1_style.height = Some(Length::px(10.0));
  item1_style.height_keyword = None;
  let item1 = BoxNode::new_block(Arc::new(item1_style), FormattingContextType::Block, vec![]);

  let mut item2_style = block_style();
  item2_style.width = Some(Length::px(30.0));
  item2_style.width_keyword = None;
  item2_style.height = Some(Length::px(10.0));
  item2_style.height_keyword = None;
  let item2 = BoxNode::new_block(Arc::new(item2_style), FormattingContextType::Block, vec![]);

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.float = Float::Left;
  grid_style.clear = Clear::None;
  grid_style.width = Some(Length::px(55.0));
  grid_style.width_keyword = None;
  grid_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(30.0)),
  ];
  grid_style.grid_column_gap = Length::px(5.0);
  grid_style.grid_column_gap_is_normal = false;

  let float_grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![item1, item2],
  );

  let root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![float_grid]);
  let tree = BoxTree::new(root);

  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let float_fragment = find_by_style(&fragment, &|s| {
    s.float == Float::Left && s.display == Display::Grid
  })
  .expect("float:grid fragment");

  let item1_fragment =
    find_by_style(float_fragment, &|s| s.width == Some(Length::px(20.0))).expect("item1");
  let item2_fragment =
    find_by_style(float_fragment, &|s| s.width == Some(Length::px(30.0))).expect("item2");

  assert!(
    (item1_fragment.bounds.y() - item2_fragment.bounds.y()).abs() <= EPS,
    "expected grid items to share the same row inside a floated grid container (y1={:.2}, y2={:.2})",
    item1_fragment.bounds.y(),
    item2_fragment.bounds.y(),
  );
  assert!(
    (item2_fragment.bounds.x() - 25.0).abs() <= EPS,
    "expected second grid item to start at x≈25px (20px + 5px gap) inside floated grid container (x={:.2})",
    item2_fragment.bounds.x(),
  );
}
