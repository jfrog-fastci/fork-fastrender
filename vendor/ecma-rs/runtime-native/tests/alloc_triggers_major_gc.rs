use runtime_native::gc::heap::IMMIX_MAX_OBJECT_SIZE;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_alloc_array, rt_gc_set_config, rt_gc_set_limits, rt_thread_deinit, rt_thread_init, rt_weak_get,
  rt_weak_remove,
};

#[inline(never)]
fn scrub_stack_words() {
  // See `alloc_triggers_minor_gc`: in debug builds the runtime may fall back to conservative stack
  // scanning for roots. Overwrite a chunk of the stack to reduce the chance of stale pointer bits
  // keeping the unreachable old object alive.
  let mut scratch = [0usize; 16 * 1024]; // 128 KiB on 64-bit
  for slot in &mut scratch {
    *slot = 0;
  }
  std::hint::black_box(&mut scratch);
}

#[test]
fn alloc_triggers_major_gc() {
  let _rt = TestRuntimeGuard::new();

  let cfg = runtime_native::abi::RtGcConfig {
    nursery_size_bytes: 1 * 1024 * 1024,
    los_threshold_bytes: 8 * 1024,
    // Avoid standalone minor collections; this test is specifically about major triggering.
    minor_gc_nursery_used_percent: 100,
    major_gc_old_bytes_threshold: 16 * 1024,
    major_gc_old_blocks_threshold: usize::MAX,
    major_gc_external_bytes_threshold: usize::MAX,
    promote_after_minor_survivals: 1,
  };
  let limits = runtime_native::abi::RtGcLimits {
    max_heap_bytes: 8 * 1024 * 1024,
    max_total_bytes: 16 * 1024 * 1024,
  };
  assert!(rt_gc_set_config(&cfg));
  assert!(rt_gc_set_limits(&limits));

  rt_thread_init(0);

  // Force LOS allocation by exceeding the Immix max object size.
  let len = IMMIX_MAX_OBJECT_SIZE + 4096;
  let elem_size = 1usize;

  // Allocate one old/LOS object, create a weak handle for it, then drop all strong roots. A major
  // GC should clear the weak handle once old-gen pressure crosses the configured threshold.
  let weak = unsafe {
    let mut obj = rt_alloc_array(len, elem_size);
    let mut scope = runtime_native::roots::RootScope::new();
    scope.push(&mut obj as *mut *mut u8);
    let h = runtime_native::rt_weak_add_h(&mut obj as *mut *mut u8);
    drop(scope);
    // Ensure the array pointer value doesn't linger in an active stack slot, which could keep the
    // referent alive under conservative scanning.
    core::ptr::write_volatile(&mut obj, core::ptr::null_mut());
    h
  };
  scrub_stack_words();

  // Drive old-gen pressure via repeated LOS allocations until the weak handle is cleared.
  for _ in 0..256 {
    if rt_weak_get(weak).is_null() {
      break;
    }
    let _ = rt_alloc_array(len, elem_size);
  }

  assert!(
    rt_weak_get(weak).is_null(),
    "expected allocator-triggered major GC to clear unreachable old/LOS weak handle"
  );

  rt_weak_remove(weak);
  rt_thread_deinit();
}
