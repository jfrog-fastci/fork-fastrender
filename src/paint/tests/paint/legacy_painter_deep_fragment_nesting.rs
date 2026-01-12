use crate::geometry::{Point, Rect};
use crate::image_loader::ImageCache;
use crate::paint::display_list_renderer::PaintParallelism;
use crate::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use crate::scroll::ScrollState;
use crate::text::font_loader::FontContext;
use crate::tree::fragment_tree::{FragmentNode, FragmentTree};
use crate::Rgba;

#[test]
fn legacy_painter_deep_fragment_nesting_does_not_stack_overflow() {
  // Use a hostile depth that would overflow the stack with the legacy recursive collector.
  let depth = 20_000usize;

  let bounds = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);
  let mut node = FragmentNode::new_block(bounds, vec![]);
  for _ in 0..depth {
    node = FragmentNode::new_block(bounds, vec![node]);
  }
  let tree = FragmentTree::new(node);

  // Build shared resources outside the small-stack thread.
  let font_ctx = FontContext::new();
  let image_cache = ImageCache::new();

  let background = Rgba::TRANSPARENT;

  let result = std::thread::Builder::new()
    .name("legacy_painter_deep_fragment_nesting".to_string())
    .stack_size(256 * 1024)
    .spawn(move || {
      let scroll_state = ScrollState::default();
      paint_tree_with_resources_scaled_offset_backend(
        &tree,
        1,
        1,
        background,
        font_ctx,
        image_cache,
        1.0,
        Point::ZERO,
        PaintParallelism::default(),
        &scroll_state,
        PaintBackend::Legacy,
      )
    })
    .expect("spawned paint thread")
    .join()
    .expect("paint thread panicked");

  assert!(result.is_ok(), "legacy painter should not overflow on deep nesting");
}
