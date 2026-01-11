use core::mem::MaybeUninit;

use runtime_native::{
  ArrayBuffer, BackingStoreAllocError, BackingStoreAllocator, GlobalBackingStoreAllocator, Uint8Array,
};
use runtime_native::{GcHeap, RootStack};
use runtime_native::gc::{HeapConfig, HeapLimits, SimpleRememberedSet};
use runtime_native::gc::OBJ_HEADER_SIZE;
use runtime_native::test_util::TestRuntimeGuard;

#[test]
fn backing_store_pointer_is_stable_across_header_relocation() {
  let _rt = TestRuntimeGuard::new();
  let alloc = GlobalBackingStoreAllocator::default();

  let buf = ArrayBuffer::new_zeroed_in(&alloc, 64).expect("alloc");
  assert_eq!(alloc.external_bytes(), 64);
  let ptr_before = buf.data_ptr().expect("ptr");

  // Simulate a moving GC relocating the *header object* with a raw `memcpy`.
  let relocated = unsafe {
    let mut dst = MaybeUninit::<ArrayBuffer>::uninit();
    core::ptr::copy_nonoverlapping(
      &buf as *const ArrayBuffer as *const u8,
      dst.as_mut_ptr() as *mut u8,
      core::mem::size_of::<ArrayBuffer>(),
    );
    dst.assume_init()
  };

  // The old header becomes unreachable after relocation; it won't be finalized.
  core::mem::forget(buf);

  let mut relocated = relocated;
  assert_eq!(relocated.data_ptr().expect("ptr"), ptr_before);

  relocated.finalize_in(&alloc);
  assert_eq!(alloc.external_bytes(), 0);
}

#[test]
fn pinned_backing_store_can_outlive_header_finalization() {
  let alloc = GlobalBackingStoreAllocator::default();

  let mut buf = Box::new(ArrayBuffer::new_zeroed_in(&alloc, 16).expect("alloc"));
  assert_eq!(alloc.external_bytes(), 16);

  let mut pinned = buf.pin().expect("pin should succeed");

  // Simulate a GC finalizer dropping the `ArrayBuffer` header while the OS still holds a pointer to
  // the backing store. The backing store must remain alive until the last pin is dropped.
  buf.finalize_in(&alloc);
  assert!(buf.is_detached());
  assert_eq!(alloc.external_bytes(), 16);

  // Release the header allocation entirely; the pinned view must still be usable.
  drop(buf);

  unsafe {
    assert_eq!(pinned.as_slice(), &[0u8; 16]);
    pinned.as_mut_slice()[0] = 1;
  }

  drop(pinned);
  assert_eq!(alloc.external_bytes(), 0);
}

#[test]
fn uint8array_view_is_bounds_checked() {
  let _rt = TestRuntimeGuard::new();
  let alloc = GlobalBackingStoreAllocator::default();
  let mut buf = ArrayBuffer::new_zeroed_in(&alloc, 8).expect("alloc");

  let view = Uint8Array::view(&buf, 2, 4).expect("in-bounds view");
  let (ptr, len) = view.as_ptr_range().expect("ptr range");
  assert_eq!(len, 4);
  assert_eq!(ptr, unsafe { buf.data_ptr().unwrap().add(2) });

  assert!(Uint8Array::view(&buf, 7, 2).is_err());
  assert!(Uint8Array::view(&buf, 9, 0).is_err());

  buf.finalize_in(&alloc);
  assert_eq!(alloc.external_bytes(), 0);
}

#[test]
fn external_backing_store_bytes_are_accounted_and_released() {
  let _rt = TestRuntimeGuard::new();
  let alloc = GlobalBackingStoreAllocator::default();
  assert_eq!(alloc.external_bytes(), 0);

  let mut a = ArrayBuffer::new_zeroed_in(&alloc, 10).expect("alloc");
  assert_eq!(alloc.external_bytes(), 10);

  let mut b = ArrayBuffer::new_zeroed_in(&alloc, 20).expect("alloc");
  assert_eq!(alloc.external_bytes(), 30);

  b.finalize_in(&alloc);
  assert_eq!(alloc.external_bytes(), 10);

  a.finalize_in(&alloc);
  assert_eq!(alloc.external_bytes(), 0);
}

#[test]
fn gc_finalizer_releases_arraybuffer_backing_store_on_minor_gc() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let count = 8usize;
  let size = 1024 * 1024;
  let mut roots = RootStack::new();
  let mut remembered = SimpleRememberedSet::new();
  for _ in 0..count {
    heap
      .alloc_array_buffer_young_gc_aware(&mut roots, &mut remembered, size)
      .expect("alloc array buffer");
  }
  assert_eq!(heap.external_bytes(), count * size);

  heap.collect_minor(&mut roots, &mut remembered).unwrap();
  assert_eq!(heap.external_bytes(), 0);
}

#[test]
fn gc_finalizer_not_called_on_promotion_and_runs_once() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let mut roots = RootStack::new();
  let mut remembered = SimpleRememberedSet::new();

  let size = 256 * 1024;
  let mut root = heap
    .alloc_array_buffer_young_gc_aware(&mut roots, &mut remembered, size)
    .expect("alloc array buffer");
  assert!(heap.is_in_nursery(root));
  assert_eq!(heap.external_bytes(), size);

  roots.push(&mut root as *mut *mut u8);

  heap.collect_minor(&mut roots, &mut remembered).unwrap();
  assert!(
    !heap.is_in_nursery(root),
    "expected ArrayBuffer header to be promoted out of nursery"
  );
  assert_eq!(heap.external_bytes(), size);

  // Still reachable: major GC must not run the finalizer.
  heap.collect_major(&mut roots, &mut remembered).unwrap();
  assert_eq!(heap.external_bytes(), size);

  // Unreachable: major GC must run the finalizer once.
  let mut empty_roots = RootStack::new();
  heap.collect_major(&mut empty_roots, &mut remembered).unwrap();
  assert_eq!(heap.external_bytes(), 0);

  // Subsequent collections must not run it again.
  heap.collect_major(&mut empty_roots, &mut remembered).unwrap();
  assert_eq!(heap.external_bytes(), 0);
}

#[test]
fn gc_finalizer_delays_backing_store_free_until_unpinned() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let mut roots = RootStack::new();
  let mut remembered = SimpleRememberedSet::new();

  let size = 1024 * 1024;
  let buf_obj = heap
    .alloc_array_buffer_young_gc_aware(&mut roots, &mut remembered, size)
    .expect("alloc array buffer");

  let pinned = {
    // Pin the backing store pointer without rooting the ArrayBuffer header.
    let buf = unsafe { &*(buf_obj.add(OBJ_HEADER_SIZE) as *const ArrayBuffer) };
    buf.pin().expect("pin")
  };
 
  heap.collect_minor(&mut roots, &mut remembered).unwrap();

  // The header is unreachable so the GC finalizer should have run, but the backing store is still
  // pinned and must remain allocated until the pin guard is dropped.
  assert_eq!(heap.external_bytes(), size);

  drop(pinned);
  assert_eq!(heap.external_bytes(), 0);
}

#[test]
fn gc_finalizer_survives_major_compaction_relocation() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  // Promote an ArrayBuffer header to old-gen first so the major-GC compactor can move it.
  let size = 128 * 1024;
  let mut roots = RootStack::new();
  let mut remembered = SimpleRememberedSet::new();

  let mut root = heap
    .alloc_array_buffer_young_gc_aware(&mut roots, &mut remembered, size)
    .expect("alloc array buffer");
  roots.push(&mut root as *mut *mut u8);
  
  heap.collect_minor(&mut roots, &mut remembered).unwrap();
  assert!(!heap.is_in_nursery(root));
  let promoted = root;

  // Force compaction candidates to include sparsely-occupied blocks.
  {
    let cfg = heap.major_compaction_config_mut();
    cfg.enabled = true;
    cfg.max_live_ratio_percent = 100;
    cfg.min_live_lines = 1;
  }

  heap.collect_major(&mut roots, &mut remembered).unwrap();
  assert_ne!(
    root, promoted,
    "expected major compaction to relocate the ArrayBuffer header"
  );
  assert_eq!(heap.external_bytes(), size);

  // Once unreachable, the finalizer should run and free the backing store exactly once.
  let mut empty_roots = RootStack::new();
  heap.collect_major(&mut empty_roots, &mut remembered).unwrap();
  assert_eq!(heap.external_bytes(), 0);
  heap.collect_major(&mut empty_roots, &mut remembered).unwrap();
  assert_eq!(heap.external_bytes(), 0);
}

#[test]
fn backing_store_oom_triggers_gc_and_retries_allocation() {
  let _rt = TestRuntimeGuard::new();

  let budget = 64 * 1024;
  let buf_size = 32 * 1024;

  let alloc = GlobalBackingStoreAllocator::with_max_external_bytes(budget);
  let config = HeapConfig {
    // Ensure the collection is triggered by backing-store OOM rather than a threshold.
    major_gc_external_bytes_threshold: usize::MAX,
    ..HeapConfig::default()
  };
  let limits = HeapLimits::default();
  let mut heap = GcHeap::with_config_and_backing_store_allocator(config, limits, alloc);

  let mut roots = RootStack::new();
  let mut remembered = SimpleRememberedSet::new();

  // Allocate ArrayBuffers up to the allocator's external-bytes budget.
  let mut a = heap
    .alloc_array_buffer_young_gc_aware(&mut roots, &mut remembered, buf_size)
    .expect("alloc a");
  roots.push(&mut a as *mut *mut u8);

  let mut b = heap
    .alloc_array_buffer_young_gc_aware(&mut roots, &mut remembered, buf_size)
    .expect("alloc b");
  roots.push(&mut b as *mut *mut u8);

  assert_eq!(heap.external_bytes(), budget);

  // Drop roots so the existing backing stores become reclaimable.
  a = core::ptr::null_mut();
  b = core::ptr::null_mut();
  // `RootStack` will read these slots via raw pointers during GC; keep an explicit read here so
  // the assignments are not treated as dead stores by the Rust compiler.
  let _ = (a, b);
  assert_eq!(heap.external_bytes(), budget);

  // The next allocation would exceed the external budget; we should see a major GC run and the
  // allocation succeed on retry after finalizers release backing stores.
  let major_before = heap.stats().major_collections;
  let _c = heap
    .alloc_array_buffer_young_gc_aware(&mut roots, &mut remembered, buf_size)
    .expect("alloc should succeed after GC+retry");
  let major_after = heap.stats().major_collections;
  assert!(
    major_after > major_before,
    "expected a major GC when backing store allocator reports OOM"
  );

  // Previous two buffers were unreachable and should have been finalized; only the new one remains.
  assert_eq!(heap.external_bytes(), buf_size);

  // Clean up: avoid leaking external memory across tests.
  heap.collect_minor(&mut roots, &mut remembered).unwrap();
  assert_eq!(heap.external_bytes(), 0);
}

#[test]
fn pinned_backing_store_is_not_reclaimed_by_gc() {
  let _rt = TestRuntimeGuard::new();

  let budget = 32 * 1024;

  let alloc = GlobalBackingStoreAllocator::with_max_external_bytes(budget);
  let config = HeapConfig {
    major_gc_external_bytes_threshold: usize::MAX,
    ..HeapConfig::default()
  };
  let limits = HeapLimits::default();
  let mut heap = GcHeap::with_config_and_backing_store_allocator(config, limits, alloc);

  let mut roots = RootStack::new();
  let mut remembered = SimpleRememberedSet::new();

  // Fill the budget with a single backing store, then pin it so it can't be freed.
  let buf_obj = heap
    .alloc_array_buffer_young_gc_aware(&mut roots, &mut remembered, budget)
    .expect("alloc pinned buffer");
  assert_eq!(heap.external_bytes(), budget);

  let pinned = {
    // Pin the backing store pointer without rooting the ArrayBuffer header.
    let buf = unsafe { &*(buf_obj.add(OBJ_HEADER_SIZE) as *const ArrayBuffer) };
    buf.pin().expect("pin")
  };

  // Attempting another allocation must still fail even after GC runs, because the backing store is
  // pinned and therefore not reclaimable.
  let major_before = heap.stats().major_collections;
  let err = heap
    .alloc_array_buffer_young_gc_aware(&mut roots, &mut remembered, 1)
    .unwrap_err();
  assert_eq!(err, BackingStoreAllocError::OutOfMemory);
  assert!(
    heap.stats().major_collections > major_before,
    "expected a major GC attempt on backing store OOM"
  );
  assert_eq!(heap.external_bytes(), budget);

  drop(pinned);
  assert_eq!(heap.external_bytes(), 0);
}
