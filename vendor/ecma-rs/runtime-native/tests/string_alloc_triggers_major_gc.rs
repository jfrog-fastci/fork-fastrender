use runtime_native::abi::{RtGcConfig, RtGcLimits};
use runtime_native::gc::heap::IMMIX_MAX_OBJECT_SIZE;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_gc_set_config, rt_gc_set_limits, rt_string_new_utf8, rt_thread_deinit, rt_thread_init,
  rt_weak_get, rt_weak_remove,
};

#[inline(never)]
fn scrub_stack_words() {
  // See `string_alloc_triggers_minor_gc`.
  let mut scratch = [0usize; 16 * 1024]; // 128 KiB on 64-bit
  for slot in &mut scratch {
    *slot = 0;
  }
  std::hint::black_box(&mut scratch);
}

#[test]
fn string_alloc_triggers_major_gc() {
  let _rt = TestRuntimeGuard::new();

  let cfg = RtGcConfig {
    nursery_size_bytes: 1 * 1024 * 1024,
    los_threshold_bytes: 8 * 1024,
    // Avoid standalone minors; this test is specifically about major triggering.
    minor_gc_nursery_used_percent: 100,
    major_gc_old_bytes_threshold: 16 * 1024,
    major_gc_old_blocks_threshold: usize::MAX,
    major_gc_external_bytes_threshold: usize::MAX,
    promote_after_minor_survivals: 1,
  };
  let limits = RtGcLimits {
    max_heap_bytes: 16 * 1024 * 1024,
    max_total_bytes: 32 * 1024 * 1024,
  };
  assert!(rt_gc_set_config(&cfg));
  assert!(rt_gc_set_limits(&limits));

  rt_thread_init(0);

  // Force a LOS allocation by exceeding the Immix max object size.
  let len = IMMIX_MAX_OBJECT_SIZE + 4096;
  let bytes = vec![b'a'; len];

  let weak = unsafe {
    let mut obj = rt_string_new_utf8(bytes.as_ptr(), bytes.len());
    let mut scope = runtime_native::roots::RootScope::new();
    scope.push(&mut obj as *mut *mut u8);
    let h = runtime_native::rt_weak_add_h(&mut obj as *mut *mut u8);
    drop(scope);
    core::ptr::write_volatile(&mut obj, core::ptr::null_mut());
    h
  };
  scrub_stack_words();

  // Drive old-gen pressure via repeated LOS allocations until the weak handle is cleared.
  for _ in 0..256 {
    if rt_weak_get(weak).is_null() {
      break;
    }
    let _ = rt_string_new_utf8(bytes.as_ptr(), bytes.len());
  }

  assert!(
    rt_weak_get(weak).is_null(),
    "expected allocator-triggered major GC to clear unreachable runtime string weak handle"
  );

  rt_weak_remove(weak);
  rt_thread_deinit();
}
