use fastrender::layout::contexts::inline::InlineFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::{BoxNode, ComputedStyle, IntrinsicSizingMode, LayoutParallelism};
use std::sync::Arc;

fn make_inline_container_with_text(
  text: &str,
  font_size: f32,
  base_id: usize,
) -> (BoxNode, usize, Arc<ComputedStyle>) {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Inline;
  container_style.font_size = font_size;
  let container_style = Arc::new(container_style);

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  text_style.font_size = font_size;
  let text_style = Arc::new(text_style);

  let mut text_node = BoxNode::new_text(text_style.clone(), text.to_string());
  let text_id = base_id + 1;
  text_node.id = text_id;

  let mut root = BoxNode::new_inline(container_style, vec![text_node]);
  root.id = base_id;

  (root, text_id, text_style)
}

#[test]
fn text_item_cache_hits_on_repeated_intrinsic_sizing() {
  let _lock = super::layout_profile_lock();

  InlineFormattingContext::debug_enable_text_item_cache_diagnostics();
  InlineFormattingContext::debug_clear_text_item_cache_current_thread();

  let ifc = InlineFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
  let (root, _text_id, _text_style) =
    make_inline_container_with_text("Cache me if you can", 16.0, 100_000_001);

  let (_, hits_before, misses_before, _) = InlineFormattingContext::debug_text_item_cache_stats();
  let w1 = ifc
    .compute_intrinsic_inline_size(&root, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic width");
  let (_, hits_after_first, misses_after_first, _) =
    InlineFormattingContext::debug_text_item_cache_stats();
  let w2 = ifc
    .compute_intrinsic_inline_size(&root, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic width (repeat)");
  let (_, hits_after_second, misses_after_second, _) =
    InlineFormattingContext::debug_text_item_cache_stats();

  assert!(w1.is_finite() && w1 > 0.0, "expected non-zero intrinsic width");
  assert!((w2 - w1).abs() < 0.01, "expected stable intrinsic width on repeat");

  assert!(
    hits_after_second > hits_after_first,
    "expected cache hits on second intrinsic sizing call; hits_before={hits_before} hits_after_first={hits_after_first} hits_after_second={hits_after_second} misses_before={misses_before} misses_after_first={misses_after_first} misses_after_second={misses_after_second}"
  );
  assert_eq!(
    misses_after_second, misses_after_first,
    "expected no additional cache misses on second call"
  );
}

#[test]
fn text_item_cache_style_override_produces_distinct_entries() {
  let _lock = super::layout_profile_lock();

  InlineFormattingContext::debug_enable_text_item_cache_diagnostics();
  InlineFormattingContext::debug_clear_text_item_cache_current_thread();

  let ifc = InlineFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
  let (root, text_id, base_text_style) =
    make_inline_container_with_text("Override", 16.0, 100_000_101);

  let w_base = ifc
    .compute_intrinsic_inline_size(&root, IntrinsicSizingMode::MaxContent)
    .expect("base intrinsic width");
  let (_, hits_after_base, misses_after_base, _) =
    InlineFormattingContext::debug_text_item_cache_stats();

  let w_base_repeat = ifc
    .compute_intrinsic_inline_size(&root, IntrinsicSizingMode::MaxContent)
    .expect("base intrinsic width (repeat)");
  let (_, hits_after_base_repeat, misses_after_base_repeat, _) =
    InlineFormattingContext::debug_text_item_cache_stats();

  assert!(
    (w_base_repeat - w_base).abs() < 0.01,
    "expected stable intrinsic width on repeated base call; base={w_base:.2} repeat={w_base_repeat:.2}"
  );
  assert!(
    hits_after_base_repeat > hits_after_base,
    "expected cache hits on repeated base call"
  );
  assert_eq!(
    misses_after_base_repeat, misses_after_base,
    "expected no additional cache misses on repeated base call"
  );

  let mut overridden = (*base_text_style).clone();
  overridden.font_size = 32.0;
  let overridden = Arc::new(overridden);

  let w_override = InlineFormattingContext::debug_with_style_override(text_id, overridden, || {
    ifc
      .compute_intrinsic_inline_size(&root, IntrinsicSizingMode::MaxContent)
      .expect("override intrinsic width")
  });
  let (_, hits_after_override, misses_after_override, _) =
    InlineFormattingContext::debug_text_item_cache_stats();

  assert!(
    w_override > w_base + 0.5,
    "expected font-size override to increase intrinsic width; base={w_base:.2} override={w_override:.2}"
  );
  assert!(
    misses_after_override > misses_after_base_repeat,
    "expected style override to produce at least one additional cache miss"
  );

  let w_base_after = ifc
    .compute_intrinsic_inline_size(&root, IntrinsicSizingMode::MaxContent)
    .expect("base intrinsic width (after override)");
  let (_, hits_after_base_after, misses_after_base_after, _) =
    InlineFormattingContext::debug_text_item_cache_stats();

  assert!(
    (w_base_after - w_base).abs() < 0.01,
    "expected base intrinsic width to remain stable after override; base={w_base:.2} after={w_base_after:.2}"
  );
  assert!(
    hits_after_base_after > hits_after_override,
    "expected base call after override to use cached base entry"
  );
  assert_eq!(
    misses_after_base_after, misses_after_override,
    "expected override to store into distinct cache entry without evicting base entry"
  );
}
