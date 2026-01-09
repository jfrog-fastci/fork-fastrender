use std::sync::Arc;

use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{AlignContent, AlignItems, FlexDirection, FlexWrap, JustifyContent, WritingMode};
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{BoxNode, BoxTree, ComputedStyle, LayoutConfig, LayoutEngine, Size};

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

fn fixed_block(width: f32, height: f32) -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = Some(Length::px(width));
  style.height = Some(Length::px(height));
  style.width_keyword = None;
  style.height_keyword = None;
  // Avoid flexing so line breaks are driven by the authored sizes.
  style.flex_shrink = 0.0;
  Arc::new(style)
}

#[test]
fn flex_pagination_vertical_writing_mode_breaks_between_wrap_lines() {
  // In `writing-mode: vertical-rl`, the block axis is horizontal and progresses right-to-left.
  // `flex-direction: row` makes the cross axis the block axis, so wrapped flex lines are stacked
  // horizontally and should fragment between lines (not within a line).
  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.writing_mode = WritingMode::VerticalRl;
  flex_style.flex_direction = FlexDirection::Row;
  flex_style.flex_wrap = FlexWrap::Wrap;
  flex_style.align_content = AlignContent::FlexStart;
  flex_style.align_items = AlignItems::FlexStart;
  flex_style.justify_content = JustifyContent::FlexStart;
  // Two 10px-wide lines; use a 15px fragmentainer so the naive boundary would land inside the
  // second line, exercising the flex-line atomicity logic.
  flex_style.width = Some(Length::px(20.0));
  // Enough inline size (vertical) for two items per line.
  flex_style.height = Some(Length::px(60.0));
  flex_style.width_keyword = None;
  flex_style.height_keyword = None;

  let child1 = BoxNode::new_block(fixed_block(10.0, 30.0), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(fixed_block(10.0, 30.0), FormattingContextType::Block, vec![]);
  // Second line has a short block-axis span child first so a naive break opportunity exists inside
  // the line when the fragmentainer boundary slices through it.
  let child3 = BoxNode::new_block(fixed_block(4.0, 30.0), FormattingContextType::Block, vec![]);
  let child4 = BoxNode::new_block(fixed_block(10.0, 30.0), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(
    Arc::new(flex_style),
    FormattingContextType::Flex,
    vec![child1, child2, child3, child4],
  );
  let box_tree = BoxTree::new(root);
  let child_ids: Vec<usize> = box_tree.root.children.iter().map(|c| c.id).collect();
  assert_eq!(child_ids.len(), 4);
  let item1_id = child_ids[0];
  let item2_id = child_ids[1];
  let item3_id = child_ids[2];
  let item4_id = child_ids[3];

  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(15.0, 15.0), 0.0));
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert_eq!(
    tree.additional_fragments.len(),
    1,
    "expected two pages (root + 1 additional) when fragmenting the two flex lines"
  );
  let first_page = &tree.root;
  let second_page = &tree.additional_fragments[0];

  // First page should contain the first flex line only.
  assert_eq!(fragments_with_id(first_page, item1_id).len(), 1);
  assert_eq!(fragments_with_id(first_page, item2_id).len(), 1);
  assert!(
    fragments_with_id(first_page, item3_id).is_empty(),
    "expected item 3 to be pushed to the next page; root slice offsets: page1={:?} page2={:?}; found fragments on page 1: {:?}",
    first_page.slice_info,
    second_page.slice_info,
    fragments_with_id(first_page, item3_id)
      .iter()
      .map(|frag| frag.bounds)
      .collect::<Vec<_>>()
  );
  assert!(
    fragments_with_id(first_page, item4_id).is_empty(),
    "expected item 4 to be pushed to the next page; found {} fragments on page 1",
    fragments_with_id(first_page, item4_id).len()
  );

  // Second page should restart at the block-start edge with the second flex line intact.
  assert!(fragments_with_id(second_page, item1_id).is_empty());
  assert!(fragments_with_id(second_page, item2_id).is_empty());
  let line2_item3 = fragments_with_id(second_page, item3_id);
  let line2_item4 = fragments_with_id(second_page, item4_id);
  assert_eq!(line2_item3.len(), 1);
  assert_eq!(line2_item4.len(), 1);

  let item3 = line2_item3[0];
  let item4 = line2_item4[0];

  // The sliced fragmentainer is 10px wide (the flex line width). The narrower 4px child should
  // remain aligned to the block-start (right) edge, leaving 6px of slack on the left.
  assert!(
    (item4.bounds.x() - 0.0).abs() < 0.1,
    "wide item should sit at the slice origin"
  );
  assert!(
    (item3.bounds.x() - 6.0).abs() < 0.1,
    "narrow item should keep its block-start alignment within the new page, got x={}",
    item3.bounds.x()
  );
}

#[test]
fn flex_pagination_wrap_reverse_breaks_between_physical_lines() {
  // `flex-wrap: wrap-reverse` reverses the cross axis so later flex lines can have decreasing
  // block-start coordinates. Fragmentation must still treat each physical line as atomic and break
  // between lines rather than inside them.
  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Row;
  flex_style.flex_wrap = FlexWrap::WrapReverse;
  flex_style.align_content = AlignContent::FlexStart;
  flex_style.align_items = AlignItems::FlexStart;
  flex_style.justify_content = JustifyContent::FlexStart;
  flex_style.width = Some(Length::px(40.0));
  flex_style.height = Some(Length::px(40.0));
  flex_style.width_keyword = None;
  flex_style.height_keyword = None;

  let child1 = BoxNode::new_block(fixed_block(20.0, 20.0), FormattingContextType::Block, vec![]);
  let child2 = BoxNode::new_block(fixed_block(20.0, 5.0), FormattingContextType::Block, vec![]);
  let child3 = BoxNode::new_block(fixed_block(20.0, 20.0), FormattingContextType::Block, vec![]);
  let child4 = BoxNode::new_block(fixed_block(20.0, 20.0), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(
    Arc::new(flex_style),
    FormattingContextType::Flex,
    vec![child1, child2, child3, child4],
  );
  let box_tree = BoxTree::new(root);
  let child_ids: Vec<usize> = box_tree.root.children.iter().map(|c| c.id).collect();
  assert_eq!(child_ids.len(), 4);
  let item1_id = child_ids[0];
  let item2_id = child_ids[1];
  let item3_id = child_ids[2];
  let item4_id = child_ids[3];

  // Use a fragmentainer height that slices through the first flex line (which is placed at the
  // bottom due to wrap-reverse) if we incorrectly treat intra-line boundaries as candidates.
  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(100.0, 25.0), 0.0));
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert_eq!(
    tree.additional_fragments.len(),
    1,
    "expected two pages when fragmenting the wrap-reverse container"
  );
  let first_page = &tree.root;
  let second_page = &tree.additional_fragments[0];

  // The second flex line (children 3+4) should live wholly on the first page.
  assert!(fragments_with_id(first_page, item1_id).is_empty());
  assert!(fragments_with_id(first_page, item2_id).is_empty());
  assert_eq!(fragments_with_id(first_page, item3_id).len(), 1);
  assert_eq!(fragments_with_id(first_page, item4_id).len(), 1);

  // The first flex line (children 1+2) should move intact to the next page and restart at y=0.
  assert!(fragments_with_id(second_page, item3_id).is_empty());
  assert!(fragments_with_id(second_page, item4_id).is_empty());
  let line1_item1 = fragments_with_id(second_page, item1_id);
  let line1_item2 = fragments_with_id(second_page, item2_id);
  assert_eq!(line1_item1.len(), 1);
  assert_eq!(line1_item2.len(), 1);

  let item1 = line1_item1[0];
  let item2 = line1_item2[0];
  assert!(
    item1.bounds.y().abs() < 0.1,
    "wrap-reverse continuation should place the next flex line at the block-start of the new page; got item1={:?} item2={:?} (page2 root={:?})",
    item1.bounds,
    item2.bounds,
    second_page.bounds
  );
  assert!(
    (item1.bounds.height() - 20.0).abs() < 0.1 && (item2.bounds.height() - 5.0).abs() < 0.1,
    "continuation should not clip the moved line's items; got item1={:?} item2={:?} (page2 root={:?})",
    item1.bounds,
    item2.bounds,
    second_page.bounds
  );
}
