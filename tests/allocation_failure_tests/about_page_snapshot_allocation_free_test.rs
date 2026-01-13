use super::{alloc_calls, lock_allocator, reset_alloc_calls};

#[test]
fn about_page_snapshot_is_allocation_free_when_unchanged() {
  let _guard = lock_allocator();

  // Prime the global snapshot (allocations here are fine).
  let _ = fastrender::ui::about_pages::about_page_snapshot();

  reset_alloc_calls();
  let _a = fastrender::ui::about_pages::about_page_snapshot();
  let _b = fastrender::ui::about_pages::about_page_snapshot();
  assert_eq!(
    alloc_calls(),
    0,
    "expected about_page_snapshot() to perform zero allocations when unchanged"
  );
}

