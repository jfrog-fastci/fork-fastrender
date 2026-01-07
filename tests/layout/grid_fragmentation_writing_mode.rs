use std::sync::Arc;

use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::fragmentation::{fragment_tree, FragmentationOptions};
use fastrender::style::display::Display;
use fastrender::style::types::{AlignItems, GridTrack, WritingMode};
use fastrender::style::values::Length;
use fastrender::{
  BoxNode, ComputedStyle, FormattingContext, FormattingContextType, FragmentContent, FragmentNode,
};

fn fragments_with_id<'a>(fragment: &'a FragmentNode, id: usize) -> Vec<&'a FragmentNode> {
  let mut out = Vec::new();
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    if let FragmentContent::Block { box_id: Some(b) } = node.content {
      if b == id {
        out.push(node);
      }
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  out
}

#[test]
fn grid_vertical_writing_mode_breaks_between_rows_not_inside_row() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.width = Some(Length::px(60.0));
  container_style.height = Some(Length::px(80.0));
  container_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(30.0)),
  ];
  container_style.grid_template_columns = vec![GridTrack::Length(Length::px(80.0))];
  container_style.align_items = AlignItems::Start;

  let mut item_a_style = ComputedStyle::default();
  item_a_style.display = Display::Block;
  item_a_style.width = Some(Length::px(20.0));
  item_a_style.grid_row_start = 1;
  item_a_style.grid_row_end = 2;
  let mut item_a = BoxNode::new_block(Arc::new(item_a_style), FormattingContextType::Block, vec![]);
  item_a.id = 1;

  let mut item_b_style = ComputedStyle::default();
  item_b_style.display = Display::Block;
  item_b_style.width = Some(Length::px(20.0));
  item_b_style.grid_row_start = 2;
  item_b_style.grid_row_end = 3;
  let mut item_b = BoxNode::new_block(Arc::new(item_b_style), FormattingContextType::Block, vec![]);
  item_b.id = 2;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![item_a, item_b],
  );

  let grid_fc = GridFormattingContext::new();
  let constraints = LayoutConstraints::new(
    AvailableSpace::Definite(60.0),
    AvailableSpace::Definite(80.0),
  );
  let grid_fragment = grid_fc.layout(&container, &constraints).expect("layout succeeds");

  let fragments = fragment_tree(&grid_fragment, &FragmentationOptions::new(50.0))
    .expect("fragmentation succeeds");

  assert_eq!(fragments.len(), 2, "expected one fragment per grid row band");
  assert!(
    !fragments_with_id(&fragments[0], 1).is_empty(),
    "first fragment should contain the first row item"
  );
  assert!(
    fragments_with_id(&fragments[0], 2).is_empty(),
    "first fragment should not contain the second row item"
  );
  assert!(
    !fragments_with_id(&fragments[1], 2).is_empty(),
    "second fragment should contain the second row item"
  );
}
