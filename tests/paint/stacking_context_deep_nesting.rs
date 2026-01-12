use fastrender::geometry::Rect;
use fastrender::paint::stacking::build_stacking_tree_from_fragment_tree_checked;
use fastrender::ComputedStyle;
use fastrender::FragmentNode;
use std::sync::Arc;

#[test]
fn stacking_context_deep_nesting_stack_safe() {
  const DEPTH: usize = 20_000;

  let handle = std::thread::Builder::new()
    .stack_size(256 * 1024)
    .spawn(|| {
      let mut style = ComputedStyle::default();
      style.opacity = 0.99;
      let style = Arc::new(style);

      let mut root =
        FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![], style.clone());
      for _ in 0..DEPTH {
        root = FragmentNode::new_block_styled(
          Rect::from_xywh(0.0, 0.0, 1.0, 1.0),
          vec![root],
          style.clone(),
        );
      }

      // Stacking tree construction is still recursive, so build it on a large-stack helper thread
      // before validating that the post-processing passes (`sort_children` and `compute_bounds`) are
      // stack-safe on this tiny stack.
      let build_handle = std::thread::Builder::new()
        .stack_size(fastrender::system::DEFAULT_RENDER_STACK_SIZE)
        .spawn(move || {
          let result = build_stacking_tree_from_fragment_tree_checked(&root);
          // Avoid recursive drop of the deep fragment tree.
          std::mem::forget(root);
          result
        })
        .expect("spawn stacking-tree build thread");

      let mut context = build_handle
        .join()
        .expect("join stacking-tree build thread")
        .expect("build stacking tree");

      context.sort_children();
      context.compute_bounds(None, None).unwrap();

      // Avoid recursive drops on the small-stack thread.
      std::mem::forget(context);
    })
    .expect("spawn deep-nesting thread");

  handle.join().expect("join deep-nesting thread");
}
