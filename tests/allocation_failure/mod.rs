//! Shared allocator + helpers for allocation-failure tests.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

struct FailingAllocator;

static FAIL_SIZE: AtomicUsize = AtomicUsize::new(0);
static FAIL_ALIGN: AtomicUsize = AtomicUsize::new(0);
static FAIL_SKIP_MATCHES: AtomicUsize = AtomicUsize::new(0);
static FAILED_ALLOCS: AtomicUsize = AtomicUsize::new(0);
static RECORDED_ALLOC_SIZE: AtomicUsize = AtomicUsize::new(0);
static RECORDED_ALLOC_ALIGN: AtomicUsize = AtomicUsize::new(0);
static COUNT_SIZE: AtomicUsize = AtomicUsize::new(0);
static COUNT_ALIGN: AtomicUsize = AtomicUsize::new(0);
static COUNT_MATCHES: AtomicUsize = AtomicUsize::new(0);

static LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn lock_allocator() -> MutexGuard<'static, ()> {
  LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn failed_allocs() -> usize {
  FAILED_ALLOCS.load(Ordering::Relaxed)
}

pub(crate) fn reset_recorded_allocation_layout() {
  RECORDED_ALLOC_SIZE.store(0, Ordering::Relaxed);
  RECORDED_ALLOC_ALIGN.store(0, Ordering::Relaxed);
}

pub(crate) fn recorded_allocation_layout() -> Option<(usize, usize)> {
  let size = RECORDED_ALLOC_SIZE.load(Ordering::Relaxed);
  if size == 0 {
    return None;
  }
  Some((size, RECORDED_ALLOC_ALIGN.load(Ordering::Relaxed)))
}

pub(crate) fn fail_next_allocation(size: usize, align: usize) {
  FAIL_ALIGN.store(align, Ordering::Relaxed);
  FAIL_SIZE.store(size, Ordering::Relaxed);
  FAIL_SKIP_MATCHES.store(0, Ordering::Relaxed);
}

pub(crate) fn fail_nth_allocation(size: usize, align: usize, skip_matches: usize) {
  FAIL_ALIGN.store(align, Ordering::Relaxed);
  FAIL_SIZE.store(size, Ordering::Relaxed);
  FAIL_SKIP_MATCHES.store(skip_matches, Ordering::Relaxed);
}

pub(crate) fn start_counting(size: usize, align: usize) {
  COUNT_ALIGN.store(align, Ordering::Relaxed);
  COUNT_SIZE.store(size, Ordering::Relaxed);
  COUNT_MATCHES.store(0, Ordering::Relaxed);
}

pub(crate) fn stop_counting() -> usize {
  COUNT_SIZE.store(0, Ordering::Relaxed);
  COUNT_MATCHES.load(Ordering::Relaxed)
}

unsafe impl GlobalAlloc for FailingAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    if RECORDED_ALLOC_SIZE.load(Ordering::Relaxed) == 0 {
      RECORDED_ALLOC_ALIGN.store(layout.align(), Ordering::Relaxed);
      RECORDED_ALLOC_SIZE.store(layout.size(), Ordering::Relaxed);
    }

    let count_size = COUNT_SIZE.load(Ordering::Relaxed);
    if count_size != 0
      && layout.size() == count_size
      && layout.align() == COUNT_ALIGN.load(Ordering::Relaxed)
    {
      COUNT_MATCHES.fetch_add(1, Ordering::Relaxed);
    }

    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0
      && layout.size() == fail_size
      && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed)
    {
      let skip = FAIL_SKIP_MATCHES.load(Ordering::Relaxed);
      if skip > 0 {
        FAIL_SKIP_MATCHES.store(skip - 1, Ordering::Relaxed);
        return System.alloc(layout);
      }
      FAIL_SIZE.store(0, Ordering::Relaxed);
      FAILED_ALLOCS.fetch_add(1, Ordering::Relaxed);
      return std::ptr::null_mut();
    }
    System.alloc(layout)
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    if RECORDED_ALLOC_SIZE.load(Ordering::Relaxed) == 0 {
      RECORDED_ALLOC_ALIGN.store(layout.align(), Ordering::Relaxed);
      RECORDED_ALLOC_SIZE.store(layout.size(), Ordering::Relaxed);
    }

    let count_size = COUNT_SIZE.load(Ordering::Relaxed);
    if count_size != 0
      && layout.size() == count_size
      && layout.align() == COUNT_ALIGN.load(Ordering::Relaxed)
    {
      COUNT_MATCHES.fetch_add(1, Ordering::Relaxed);
    }

    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0
      && layout.size() == fail_size
      && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed)
    {
      let skip = FAIL_SKIP_MATCHES.load(Ordering::Relaxed);
      if skip > 0 {
        FAIL_SKIP_MATCHES.store(skip - 1, Ordering::Relaxed);
        return System.alloc_zeroed(layout);
      }
      FAIL_SIZE.store(0, Ordering::Relaxed);
      FAILED_ALLOCS.fetch_add(1, Ordering::Relaxed);
      return std::ptr::null_mut();
    }
    System.alloc_zeroed(layout)
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    if RECORDED_ALLOC_SIZE.load(Ordering::Relaxed) == 0 {
      RECORDED_ALLOC_ALIGN.store(layout.align(), Ordering::Relaxed);
      RECORDED_ALLOC_SIZE.store(new_size, Ordering::Relaxed);
    }

    let count_size = COUNT_SIZE.load(Ordering::Relaxed);
    if count_size != 0 && new_size == count_size && layout.align() == COUNT_ALIGN.load(Ordering::Relaxed)
    {
      COUNT_MATCHES.fetch_add(1, Ordering::Relaxed);
    }

    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0 && new_size == fail_size && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed)
    {
      let skip = FAIL_SKIP_MATCHES.load(Ordering::Relaxed);
      if skip > 0 {
        FAIL_SKIP_MATCHES.store(skip - 1, Ordering::Relaxed);
        return System.realloc(ptr, layout, new_size);
      }
      FAIL_SIZE.store(0, Ordering::Relaxed);
      FAILED_ALLOCS.fetch_add(1, Ordering::Relaxed);
      return std::ptr::null_mut();
    }
    System.realloc(ptr, layout, new_size)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    System.dealloc(ptr, layout)
  }
}

#[global_allocator]
static GLOBAL: FailingAllocator = FailingAllocator;

mod about_url_placeholder_pixmap_allocation_failure_test;
mod cascade_dom_maps_allocation_failure_test;
mod cascade_rule_index_allocation_failure_test;
mod cascade_selector_key_cache_allocation_failure_test;
mod cluster_map_allocation_failure_test;
mod colrv1_gradient_stop_allocation_failure_test;
mod cpal_palette_allocation_failure_test;
mod delta_set_index_map_allocation_failure_test;
mod item_variation_store_allocation_failure_test;
mod legacy_mask_luminance_allocation_failure_test;
mod pattern_fill_allocation_failure_test;
mod selector_bloom_allocation_failure_test;
mod shape_outside_allocation_failure_test;
