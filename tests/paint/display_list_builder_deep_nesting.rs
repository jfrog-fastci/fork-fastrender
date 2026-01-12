use fastrender::geometry::Rect;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::style::types::{BackgroundBox, BackgroundLayer};
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::ComputedStyle;
use fastrender::Rgba;
use std::collections::HashSet;
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

#[test]
fn display_list_builder_deep_inline_fragment_nesting_does_not_overflow_stack() {
  // This test targets stack safety for line decoration helper routines (e.g. style hinting) which
  // historically walked fragment children recursively.
  let depth = 20_000;
  let rect = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);

  let style = Arc::new(ComputedStyle::default());

  // Deep inline chain inside a single line fragment.
  let mut leaf = FragmentNode::new_text(rect, "x", 0.0);
  leaf.style = Some(style);
  let mut node = leaf;
  for _ in 0..depth {
    node = FragmentNode::new_inline(rect, 0, vec![node]);
  }
  let line = FragmentNode::new_line(rect, 0.0, vec![node]);
  let root = FragmentNode::new_block(rect, vec![line]);

  let root = Arc::new(root);
  let root_for_thread = Arc::clone(&root);
  let handle = std::thread::Builder::new()
    .name("paint_deep_inline_fragment_nesting".to_string())
    .stack_size(256 * 1024)
    .spawn(move || DisplayListBuilder::new().build_checked(&root_for_thread))
    .expect("spawn deep-nesting paint thread");

  let result = handle
    .join()
    .expect("deep-nesting paint thread panicked");
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

#[test]
fn display_list_builder_deep_fragment_nesting_with_clips_does_not_overflow_stack() {
  let depth = 20_000;
  let rect = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);

  // Build a deep fragment chain iteratively (avoid recursion in the test itself).
  let mut root = FragmentNode::new_block(rect, vec![]);
  for _ in 0..depth {
    root = FragmentNode::new_block(rect, vec![root]);
  }

  let root = Arc::new(root);
  let root_for_thread = Arc::clone(&root);
  let handle = std::thread::Builder::new()
    .name("paint_deep_fragment_nesting_with_clips".to_string())
    .stack_size(256 * 1024)
    .spawn(move || {
      let clips: HashSet<Option<usize>> = HashSet::new();
      DisplayListBuilder::new().build_with_clips_checked(&root_for_thread, &clips)
    })
    .expect("spawn deep-nesting paint thread");

  let result = handle.join().expect("deep-nesting paint thread panicked");
  assert!(
    result.is_ok(),
    "expected deep-nesting build_with_clips to succeed; got {result:?}"
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

#[test]
fn display_list_builder_deep_background_clip_text_nesting_does_not_overflow_stack() {
  // `background-clip: text` triggers a text-run collection pass over the fragment subtree.
  // Historically that pass recursed directly and could stack overflow on hostile depth.
  let depth = 20_000;
  let rect = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);

  let leaf = FragmentNode::new_text(rect, "x", 0.0);
  let mut node = leaf;
  for _ in 0..depth {
    node = FragmentNode::new_block(rect, vec![node]);
  }

  let mut bg_style = ComputedStyle::default();
  bg_style.background_color = Rgba::rgb(255, 0, 0);
  let mut layer = BackgroundLayer::default();
  layer.clip = BackgroundBox::Text;
  bg_style.background_layers = vec![layer].into();

  let root = FragmentNode::new_block_styled(rect, vec![node], Arc::new(bg_style));

  let root = Arc::new(root);
  let root_for_thread = Arc::clone(&root);
  let handle = std::thread::Builder::new()
    .name("paint_deep_background_clip_text_nesting".to_string())
    .stack_size(256 * 1024)
    .spawn(move || DisplayListBuilder::new().build_checked(&root_for_thread))
    .expect("spawn deep-nesting paint thread");

  let result = handle.join().expect("deep-nesting paint thread panicked");
  assert!(
    result.is_ok(),
    "expected deep-nesting background-clip:text build to succeed; got {result:?}"
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
