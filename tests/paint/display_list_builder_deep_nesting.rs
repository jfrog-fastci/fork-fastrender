use fastrender::geometry::Rect;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::tree::fragment_tree::FragmentNode;
use std::sync::Arc;

#[test]
fn display_list_builder_deep_fragment_nesting_does_not_overflow_stack() {
  let depth = 20_000;
  let rect = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);

  // Build a deep fragment chain iteratively (avoid recursion in the test itself).
  let mut root = FragmentNode::new_block(rect, vec![]);
  for _ in 0..depth {
    root = FragmentNode::new_block(rect, vec![root]);
  }

  // Run paint on a small-stack thread; this would previously risk stack overflow due to recursive
  // descent in `DisplayListBuilder::build_fragment_internal`.
  let root = Arc::new(root);
  let root_for_thread = Arc::clone(&root);
  let handle = std::thread::Builder::new()
    .name("paint_deep_fragment_nesting".to_string())
    .stack_size(256 * 1024)
    .spawn(move || DisplayListBuilder::new().build_checked(&root_for_thread))
    .expect("spawn deep-nesting paint thread");

  let result = handle.join().expect("deep-nesting paint thread panicked");
  assert!(
    result.is_ok(),
    "expected deep-nesting build to succeed; got {result:?}"
  );

  // Drop the deeply nested fragment chain iteratively to avoid recursive drop overhead in the test
  // harness.
  let mut current = Arc::try_unwrap(root).expect("deep-nesting root unexpectedly shared");
  loop {
    let mut children = std::mem::take(&mut current.children).into_iter();
    if let Some(child) = children.next() {
      current = child;
    } else {
      break;
    }
  }
}

