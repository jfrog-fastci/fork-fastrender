#![cfg(all(
  target_os = "linux",
  target_arch = "x86_64",
  runtime_native_has_stackmap_test_artifact
))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{rt_gc_collect, rt_thread_deinit, rt_thread_init};

struct CountingAlloc;

static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    System.alloc(layout)
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    System.alloc_zeroed(layout)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    System.dealloc(ptr, layout)
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    System.realloc(ptr, layout, new_size)
  }
}

#[test]
fn rt_gc_collect_does_not_allocate_after_thread_init() {
  let _rt = TestRuntimeGuard::new();

  // Registering a thread should eagerly parse and index stackmaps so stop-the-world GC doesn't do
  // any lazy allocation work while the world is stopped.
  rt_thread_init(3);

  // Ensure the safepoint coordinator singleton is initialized outside the measured section.
  let _ = runtime_native::threading::safepoint::threads_waiting_at_safepoint();

  ALLOC_CALLS.store(0, Ordering::SeqCst);
  rt_gc_collect();
  let allocs = ALLOC_CALLS.load(Ordering::SeqCst);
  assert_eq!(
    allocs, 0,
    "rt_gc_collect performed unexpected allocations after thread init (alloc calls={allocs})"
  );

  rt_thread_deinit();
}

