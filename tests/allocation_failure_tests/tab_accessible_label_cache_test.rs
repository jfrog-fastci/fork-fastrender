use super::{alloc_calls, lock_allocator, reset_alloc_calls};
use fastrender::ui::TabAccessibleLabelCache;

#[test]
fn tab_accessible_label_cache_is_allocation_free_on_hits() {
  let _guard = lock_allocator();

  let mut cache = TabAccessibleLabelCache::default();

  // Prime the cache (allocations here are fine).
  let _ = cache.get_or_update("Stable Title", false, false, false, false, false);

  reset_alloc_calls();
  let _ = cache.get_or_update("Stable Title", false, false, false, false, false);
  assert_eq!(
    alloc_calls(),
    0,
    "expected cache-hit tab accessible label generation to perform zero allocations"
  );
}
