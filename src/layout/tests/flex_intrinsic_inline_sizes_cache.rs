use crate::layout::contexts::flex::FlexFormattingContext;
use crate::layout::formatting_context::{
  intrinsic_cache_stats, intrinsic_cache_test_lock, intrinsic_cache_use_epoch, FormattingContext,
  IntrinsicSizingMode,
};
use crate::style::display::Display;
use crate::BoxNode;
use crate::ComputedStyle;
use crate::FormattingContextType;
use std::sync::Arc;

#[test]
fn flex_intrinsic_inline_size_caches_both_modes_on_single_probe() {
  let _guard = intrinsic_cache_test_lock();
  intrinsic_cache_use_epoch(1, true);

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  // Ensure both min/max-content differ for the container.
  flex_style.flex_wrap = crate::style::types::FlexWrap::Wrap;
  let flex_style = Arc::new(flex_style);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  let item_style = Arc::new(item_style);

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::Block;
  let inline_style = Arc::new(inline_style);

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  let text_style = Arc::new(text_style);

  let mut children = Vec::new();
  for idx in 0..6 {
    let text = BoxNode::new_text(
      text_style.clone(),
      format!("child-{idx} lorem ipsum dolor sit amet consectetur adipiscing elit"),
    );
    let inline = BoxNode::new_block(
      inline_style.clone(),
      FormattingContextType::Inline,
      vec![text],
    );
    let mut item =
      BoxNode::new_block(item_style.clone(), FormattingContextType::Block, vec![inline]);
    item.id = 100 + idx;
    children.push(item);
  }

  let mut container = BoxNode::new_block(flex_style, FormattingContextType::Flex, children);
  container.id = 42;

  let fc = FlexFormattingContext::new();

  let min = fc
    .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MinContent)
    .expect("min-content sizing should succeed");
  let (lookups_before, hits_before, stores_before, _, _, _) = intrinsic_cache_stats();

  let max = fc
    .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent)
    .expect("max-content sizing should succeed");
  assert!(
    max > min + 0.5,
    "expected max-content ({max:.2}) to exceed min-content ({min:.2})"
  );

  let (lookups_after, hits_after, stores_after, _, _, _) = intrinsic_cache_stats();

  // The max-content probe should be a cache hit at the flex container level (single lookup/hit, no
  // additional stores), demonstrating that the first min-content probe stored both modes.
  assert_eq!(
    lookups_after.saturating_sub(lookups_before),
    1,
    "expected exactly one additional cache lookup for max-content probe"
  );
  assert_eq!(
    hits_after.saturating_sub(hits_before),
    1,
    "expected max-content probe to hit the intrinsic cache"
  );
  assert_eq!(
    stores_after, stores_before,
    "expected max-content probe to avoid recomputation/stores thanks to combined caching"
  );
}

