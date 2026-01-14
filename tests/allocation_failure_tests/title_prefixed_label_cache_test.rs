use super::{alloc_calls, lock_allocator, reset_alloc_calls};
use fastrender::ui::TitlePrefixedLabelCache;

#[test]
fn title_prefixed_label_cache_is_allocation_free_on_hits() {
  let _guard = lock_allocator();

  let mut cache = TitlePrefixedLabelCache::default();

  // Prime the cache (allocations here are fine).
  let _ = cache.get_or_update("Close tab", "Stable Title");

  reset_alloc_calls();
  let _ = cache.get_or_update("Close tab", "Stable Title");
  assert_eq!(
    alloc_calls(),
    0,
    "expected cache-hit title prefixed label generation to perform zero allocations"
  );
}

