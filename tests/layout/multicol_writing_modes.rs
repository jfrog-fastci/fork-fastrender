use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{ColumnFill, WritingMode};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::FormattingContext;
use std::sync::Arc;

fn find_fragment<'a>(fragment: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
  if let FragmentContent::Block { box_id: Some(b) } = fragment.content {
    if b == id {
      return Some(fragment);
    }
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment(child, id) {
      return Some(found);
    }
  }
  None
}

fn fragments_with_id<'a>(fragment: &'a FragmentNode, id: usize) -> Vec<&'a FragmentNode> {
  fn walk<'a>(fragment: &'a FragmentNode, id: usize, out: &mut Vec<&'a FragmentNode>) {
    if let FragmentContent::Block { box_id: Some(b) } = fragment.content {
      if b == id {
        out.push(fragment);
      }
    }
    for child in fragment.children.iter() {
      walk(child, id, out);
    }
  }

  let mut out = Vec::new();
  walk(fragment, id, &mut out);
  out
}

#[test]
fn multicol_vertical_rl_with_horizontal_child_writing_mode_advances_in_block_axis() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Block;
  parent_style.writing_mode = WritingMode::VerticalRl;
  parent_style.width = Some(Length::px(200.0));
  parent_style.height = Some(Length::px(200.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(0.0);
  parent_style.column_fill = ColumnFill::Auto;
  let parent_style = Arc::new(parent_style);

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Flex;
  first_style.writing_mode = WritingMode::HorizontalTb;
  first_style.width = Some(Length::px(200.0));
  first_style.height = Some(Length::px(20.0));
  let mut first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Flex, vec![]);
  first.id = 1;

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  second_style.writing_mode = WritingMode::VerticalRl;
  second_style.width = Some(Length::px(200.0));
  second_style.height = Some(Length::px(20.0));
  let mut second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  second.id = 2;

  let mut root = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![first, second],
  );
  root.id = 100;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout");

  let container = find_fragment(&fragment, root.id).expect("multicol container fragment");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info for multicol container");
  assert_eq!(info.column_count, 2);
  let stride = info.column_width + info.column_gap;

  let first_frags = fragments_with_id(&fragment, 1);
  let second_frags = fragments_with_id(&fragment, 2);
  assert_eq!(
    first_frags.len(),
    1,
    "expected exactly one fragment for first child"
  );
  assert_eq!(
    second_frags.len(),
    1,
    "expected exactly one fragment for second child"
  );
  let first_frag = first_frags[0];
  let second_frag = second_frags[0];

  assert_eq!(
    first_frag.fragment_index, 0,
    "first child should be in column 0"
  );
  assert_eq!(
    second_frag.fragment_index, 1,
    "second child should be in column 1"
  );
  assert!(
    (first_frag.bounds.y() - 0.0).abs() < 0.1,
    "first column should start at y=0 (got y={})",
    first_frag.bounds.y()
  );
  assert!(
    (second_frag.bounds.y() - stride).abs() < 0.2,
    "second column should be offset by the column stride (expected y≈{}, got y={})",
    stride,
    second_frag.bounds.y()
  );
  assert!(
    !first_frag.bounds.intersects(second_frag.bounds),
    "children should not overlap across columns: first={:?} second={:?}",
    first_frag.bounds,
    second_frag.bounds
  );
}

#[test]
fn multicol_horizontal_with_vertical_child_writing_mode_advances_in_block_axis() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Block;
  parent_style.writing_mode = WritingMode::HorizontalTb;
  parent_style.width = Some(Length::px(200.0));
  parent_style.height = Some(Length::px(200.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(0.0);
  parent_style.column_fill = ColumnFill::Auto;
  let parent_style = Arc::new(parent_style);

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Flex;
  first_style.writing_mode = WritingMode::VerticalRl;
  first_style.width = Some(Length::px(20.0));
  first_style.height = Some(Length::px(200.0));
  let mut first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Flex, vec![]);
  first.id = 1;

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  second_style.writing_mode = WritingMode::HorizontalTb;
  second_style.height = Some(Length::px(20.0));
  let mut second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  second.id = 2;

  let mut root = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![first, second],
  );
  root.id = 200;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout");

  let container = find_fragment(&fragment, root.id).expect("multicol container fragment");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info for multicol container");
  assert_eq!(info.column_count, 2);
  let stride = info.column_width + info.column_gap;

  let first_frags = fragments_with_id(&fragment, 1);
  let second_frags = fragments_with_id(&fragment, 2);
  assert_eq!(
    first_frags.len(),
    1,
    "expected exactly one fragment for first child"
  );
  assert_eq!(
    second_frags.len(),
    1,
    "expected exactly one fragment for second child"
  );
  let first_frag = first_frags[0];
  let second_frag = second_frags[0];

  assert_eq!(
    first_frag.fragment_index, 0,
    "first child should be in column 0"
  );
  assert_eq!(
    second_frag.fragment_index, 1,
    "second child should be in column 1"
  );
  assert!(
    (first_frag.bounds.x() - 0.0).abs() < 0.1,
    "first column should start at x=0 (got x={})",
    first_frag.bounds.x()
  );
  assert!(
    (second_frag.bounds.x() - stride).abs() < 0.2,
    "second column should be offset by the column stride (expected x≈{}, got x={})",
    stride,
    second_frag.bounds.x()
  );
  assert!(
    !first_frag.bounds.intersects(second_frag.bounds),
    "children should not overlap across columns: first={:?} second={:?}",
    first_frag.bounds,
    second_frag.bounds
  );
}
