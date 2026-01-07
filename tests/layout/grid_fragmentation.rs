use std::sync::Arc;

use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{AlignItems, GridTrack};
use fastrender::style::values::Length;
use fastrender::{
  BoxNode, BoxTree, ComputedStyle, FragmentContent, FragmentNode, FragmentTree, LayoutConfig,
  LayoutEngine, Size,
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

fn paginated_pages<'a>(tree: &'a FragmentTree) -> Vec<&'a FragmentNode> {
  std::iter::once(&tree.root)
    .chain(tree.additional_fragments.iter())
    .collect()
}

#[test]
fn grid_pagination_pushes_next_row_to_next_page() {
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(100.0));
  grid_style.height = Some(Length::px(60.0));
  grid_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(30.0)),
    GridTrack::Length(Length::px(30.0)),
  ];
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
  grid_style.align_items = AlignItems::Start;
  let grid_style = Arc::new(grid_style);

  let mut item_a_style = ComputedStyle::default();
  item_a_style.display = Display::Block;
  item_a_style.height = Some(Length::px(10.0));
  item_a_style.grid_row_start = 1;
  item_a_style.grid_row_end = 2;
  let item_a_style = Arc::new(item_a_style);

  let mut item_b_style = ComputedStyle::default();
  item_b_style.display = Display::Block;
  item_b_style.height = Some(Length::px(20.0));
  item_b_style.grid_row_start = 2;
  item_b_style.grid_row_end = 3;
  let item_b_style = Arc::new(item_b_style);

  let item_a = BoxNode::new_block(item_a_style, FormattingContextType::Block, vec![]);
  let item_b = BoxNode::new_block(item_b_style, FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(
    grid_style,
    FormattingContextType::Grid,
    vec![item_a, item_b],
  );

  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![grid],
  );
  let box_tree = BoxTree::new(root);

  let item_a_id = box_tree.root.children[0].children[0].id;
  let item_b_id = box_tree.root.children[0].children[1].id;

  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(200.0, 50.0), 0.0));
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert!(
    tree.additional_fragments.len() >= 1,
    "grid container should span at least two pages"
  );
  let first_page = &tree.root;
  let second_page = &tree.additional_fragments[0];

  assert_eq!(
    fragments_with_id(first_page, item_a_id).len(),
    1,
    "expected item A to live entirely on the first page"
  );
  assert!(
    fragments_with_id(first_page, item_b_id).is_empty(),
    "expected the second row to be pushed to the next page instead of slicing within the row"
  );
  assert_eq!(
    fragments_with_id(second_page, item_b_id).len(),
    1,
    "expected item B to live entirely on the second page"
  );

  let pages = paginated_pages(&tree);
  for id in [item_a_id, item_b_id] {
    let count: usize = pages
      .iter()
      .map(|page| fragments_with_id(page, id).len())
      .sum();
    assert_eq!(
      count, 1,
      "expected box id {id} to appear exactly once total across all pages"
    );
  }
}

#[test]
fn grid_pagination_splits_spanning_item_on_row_boundaries() {
  const EPSILON: f32 = 0.1;

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(100.0));
  grid_style.height = Some(Length::px(60.0));
  grid_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(20.0)),
    GridTrack::Length(Length::px(20.0)),
  ];
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
  let grid_style = Arc::new(grid_style);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.grid_row_start = 1;
  item_style.grid_row_end = 4;
  item_style.height = Some(Length::px(60.0));
  let item_style = Arc::new(item_style);

  let item = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);
  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![grid],
  );
  let box_tree = BoxTree::new(root);
  let item_id = box_tree.root.children[0].children[0].id;

  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(200.0, 35.0), 0.0));
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert_eq!(
    tree.additional_fragments.len(),
    2,
    "expected three pages (root + 2 additional fragments) when breaks align to 20px grid rows"
  );

  let pages = paginated_pages(&tree);
  assert_eq!(pages.len(), 3);

  let mut slices = Vec::new();
  for (page_idx, page) in pages.iter().enumerate() {
    let fragments = fragments_with_id(page, item_id);
    assert_eq!(
      fragments.len(),
      1,
      "expected exactly one fragment for the spanning grid item on page {page_idx}"
    );
    let slice = fragments[0];
    assert!(
      (slice.bounds.height() - 20.0).abs() < EPSILON,
      "expected page {page_idx} slice height to be ~20px, got {}",
      slice.bounds.height()
    );
    slices.push(slice);
  }

  let first = slices[0];
  let middle = slices[1];
  let last = slices[2];

  assert!(first.slice_info.is_first);
  assert!(!first.slice_info.is_last);
  assert!(first.slice_info.slice_offset.abs() < EPSILON);
  assert!((first.slice_info.original_block_size - 60.0).abs() < EPSILON);

  assert!(!middle.slice_info.is_first);
  assert!(!middle.slice_info.is_last);
  assert!((middle.slice_info.slice_offset - 20.0).abs() < EPSILON);
  assert!((middle.slice_info.original_block_size - 60.0).abs() < EPSILON);

  assert!(!last.slice_info.is_first);
  assert!(last.slice_info.is_last);
  assert!((last.slice_info.slice_offset - 40.0).abs() < EPSILON);
  assert!((last.slice_info.original_block_size - 60.0).abs() < EPSILON);
}
