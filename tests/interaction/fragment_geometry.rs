use fastrender::interaction::absolute_bounds_for_box_id;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::{FragmentContent, FragmentNode, FragmentTree, Rect};

#[test]
fn absolute_bounds_for_box_id_accumulates_ancestor_offsets() {
  let target_box_id = 1;

  let target_fragment = FragmentNode::new(
    Rect::from_xywh(5.0, 6.0, 7.0, 8.0),
    FragmentContent::Replaced {
      replaced_type: ReplacedType::Canvas,
      box_id: Some(target_box_id),
    },
    vec![],
  );

  let parent = FragmentNode::new_block(
    Rect::from_xywh(10.0, 20.0, 100.0, 50.0),
    vec![target_fragment],
  );
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![parent]);
  let tree = FragmentTree::new(root);

  let bounds =
    absolute_bounds_for_box_id(&tree, target_box_id).expect("expected box id to resolve");
  assert_eq!(bounds, Rect::from_xywh(15.0, 26.0, 7.0, 8.0));
}

#[test]
fn absolute_bounds_for_box_id_searches_additional_fragments() {
  let target_box_id = 2;

  let target_fragment =
    FragmentNode::new_block_with_id(Rect::from_xywh(3.0, 4.0, 5.0, 6.0), target_box_id, vec![]);
  let additional_root = FragmentNode::new_block(
    Rect::from_xywh(100.0, 200.0, 300.0, 400.0),
    vec![target_fragment],
  );

  let mut tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    vec![],
  ));
  tree.additional_fragments.push(additional_root);

  let bounds =
    absolute_bounds_for_box_id(&tree, target_box_id).expect("expected box id to resolve");
  assert_eq!(bounds, Rect::from_xywh(103.0, 204.0, 5.0, 6.0));
}
