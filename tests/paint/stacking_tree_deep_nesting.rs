use fastrender::geometry::Rect;
use fastrender::paint::stacking::build_stacking_tree_from_fragment_tree_checked;
use fastrender::ComputedStyle;
use fastrender::FragmentNode;
use fastrender::Position;
use std::sync::Arc;

#[test]
fn stacking_tree_build_handles_deep_nesting_without_stack_overflow() {
  // This test targets stack safety: historically the stacking tree builder used deep recursion
  // (proportional to fragment nesting depth) and could stack overflow on hostile input.
  const DEPTH: usize = 20_000;

  // Only the leaf is positioned so we (a) force a traversal of the full chain and (b) keep the
  // stacking output small enough for this to run quickly in debug builds.
  let mut leaf_style = ComputedStyle::default();
  leaf_style.position = Position::Relative;
  let leaf_style = Arc::new(leaf_style);

  let mut node = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![]);
  node.style = Some(leaf_style);

  for _ in 1..DEPTH {
    let mut parent =
      FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![node]);
    node = parent;
  }

  let root = node;
  let handle = std::thread::Builder::new()
    .name("stacking_tree_deep_nesting".into())
    .stack_size(256 * 1024)
    .spawn(move || {
      let tree = build_stacking_tree_from_fragment_tree_checked(&root)
        .expect("stacking tree build should succeed");

      // Ensure we actually traversed the full chain by observing the leaf positioned element.
      assert_eq!(tree.layer6_positioned.len(), 1);
    })
    .expect("thread spawn should succeed");

  handle.join().expect("thread should complete successfully");
}
