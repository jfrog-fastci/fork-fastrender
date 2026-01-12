use fastrender::layout::fragmentation::{
  fragment_tree, fragment_tree_for_writing_mode, FragmentationOptions,
};
use fastrender::style::types::{Direction, WritingMode};
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
fn fragmentation_axes_override_vertical_lr_fragments_horizontally() {
  let child1 = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 150.0, 100.0), 1, vec![]);
  let child2 =
    FragmentNode::new_block_with_id(Rect::from_xywh(150.0, 0.0, 150.0, 100.0), 2, vec![]);
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 300.0, 100.0),
    vec![child1, child2],
  );
  let options = FragmentationOptions::new(150.0);

  let default = fragment_tree(&root, &options).expect("fragment_tree");
  assert_eq!(
    default.len(),
    1,
    "default (Y-axis) fragmentation should fit in one fragmentainer"
  );

  let vertical =
    fragment_tree_for_writing_mode(&root, &options, WritingMode::VerticalLr, Direction::Ltr)
      .expect("fragment_tree_for_writing_mode");
  assert_eq!(
    vertical.len(),
    2,
    "vertical-lr uses a horizontal block axis so content should fragment across two fragmentainers"
  );

  assert_eq!(fragments_with_id(&vertical[0], 1).len(), 1);
  assert_eq!(fragments_with_id(&vertical[0], 2).len(), 0);
  assert_eq!(fragments_with_id(&vertical[1], 1).len(), 0);
  assert_eq!(fragments_with_id(&vertical[1], 2).len(), 1);
}

#[test]
fn fragmentation_axes_override_vertical_rl_respects_reversed_block_progression() {
  let child1 = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 150.0, 100.0), 1, vec![]);
  let child2 =
    FragmentNode::new_block_with_id(Rect::from_xywh(150.0, 0.0, 150.0, 100.0), 2, vec![]);
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 300.0, 100.0),
    vec![child1, child2],
  );
  let options = FragmentationOptions::new(150.0);

  let vertical =
    fragment_tree_for_writing_mode(&root, &options, WritingMode::VerticalRl, Direction::Ltr)
      .expect("fragment_tree_for_writing_mode");
  assert_eq!(vertical.len(), 2);

  // In vertical-rl the block axis progresses right-to-left, so the right-hand child should appear
  // in the first fragmentainer.
  assert_eq!(fragments_with_id(&vertical[0], 1).len(), 0);
  assert_eq!(fragments_with_id(&vertical[0], 2).len(), 1);
  assert_eq!(fragments_with_id(&vertical[1], 1).len(), 1);
  assert_eq!(fragments_with_id(&vertical[1], 2).len(), 0);
}
