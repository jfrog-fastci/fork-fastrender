use super::{alloc_calls, lock_allocator, reset_alloc_calls};
use fastrender::ui::{TabId, WindowTitleCache};

#[test]
fn sync_window_title_is_allocation_free_when_unchanged() {
  let _guard = lock_allocator();

  let tab = TabId::new();
  let mut cache = WindowTitleCache::default();

  // Prime internal buffers (allocations here are fine).
  assert_eq!(
    cache.sync(Some(tab), Some("Stable Title")),
    Some("Stable Title — FastRender")
  );

  reset_alloc_calls();
  assert_eq!(cache.sync(Some(tab), Some("Stable Title")), None);
  assert_eq!(
    alloc_calls(),
    0,
    "expected sync to perform zero allocations when the title is unchanged"
  );
}

