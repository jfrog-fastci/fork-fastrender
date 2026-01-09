use std::sync::Arc;

use fastrender::layout::fragmentation::{
  fragment_tree, resolve_fragmentation_boundaries_with_context, FragmentationContext,
  FragmentationOptions,
};
use fastrender::style::display::Display;
use fastrender::style::types::{BreakBetween, FlexDirection};
use fastrender::ComputedStyle;
use fastrender::{FragmentContent, FragmentNode, Rect};

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
fn row_flex_does_not_break_inside_a_line_that_fits() {
  let spacer = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 100.0, 10.0), 1, vec![]);

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Row;
  let flex_style = Arc::new(flex_style);

  let item_a = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 50.0, 5.0), 3, vec![]);
  let item_b = FragmentNode::new_block_with_id(Rect::from_xywh(50.0, 0.0, 50.0, 18.0), 4, vec![]);

  let mut flex_container = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 10.0, 100.0, 18.0),
    2,
    vec![item_a, item_b],
  );
  flex_container.style = Some(flex_style);

  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 100.0, 28.0),
    vec![spacer, flex_container],
  );

  let boundaries =
    resolve_fragmentation_boundaries_with_context(&root, 20.0, FragmentationContext::Page).unwrap();
  assert!(
    (boundaries.get(1).copied().unwrap_or(0.0) - 10.0).abs() < 0.01,
    "expected first break before the flex line at 10, got {boundaries:?}"
  );

  let fragments = fragment_tree(&root, &FragmentationOptions::new(20.0)).unwrap();
  assert_eq!(fragments.len(), 2);
  assert!(
    fragments_with_id(&fragments[0], 4).is_empty(),
    "first fragment must not contain a sliced flex item"
  );
  let item_b_frags = fragments_with_id(&fragments[1], 4);
  assert_eq!(item_b_frags.len(), 1);
  assert!(
    (item_b_frags[0].bounds.height() - 18.0).abs() < 0.01,
    "flex item in second fragment should not be clipped"
  );
}

#[test]
fn row_flex_item_forced_break_propagates_to_line_boundary() {
  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Row;
  let flex_style = Arc::new(flex_style);

  let item_1 = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 30.0, 10.0), 1, vec![]);
  let mut item_2_style = ComputedStyle::default();
  item_2_style.break_after = BreakBetween::Always;
  let item_2_style = Arc::new(item_2_style);
  let mut item_2 =
    FragmentNode::new_block_with_id(Rect::from_xywh(30.0, 0.0, 30.0, 5.0), 2, vec![]);
  item_2.style = Some(item_2_style);
  let item_3 = FragmentNode::new_block_with_id(Rect::from_xywh(60.0, 0.0, 30.0, 10.0), 3, vec![]);
  // Second line: main-axis start resets, cross-axis advances.
  let item_4 = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 10.0, 30.0, 10.0), 4, vec![]);

  let mut flex_container = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
    10,
    vec![item_1, item_2, item_3, item_4],
  );
  flex_container.style = Some(flex_style);

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 20.0), vec![flex_container]);

  // Content fits in a single fragmentainer, but a forced break on an item in the middle of a flex
  // line should still force a break between flex lines.
  let boundaries =
    resolve_fragmentation_boundaries_with_context(&root, 50.0, FragmentationContext::Page).unwrap();
  assert!(
    boundaries.iter().any(|b| (*b - 10.0).abs() < 0.01),
    "expected a forced boundary at the flex line end (10), got {boundaries:?}"
  );
}
