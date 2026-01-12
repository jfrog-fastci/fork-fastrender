use runtime_native::abi::{RtGcConfig, RtGcLimits};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_gc_set_config, rt_gc_set_limits, rt_string_new_utf8, rt_thread_deinit, rt_thread_init,
  rt_weak_get, rt_weak_remove,
};

#[inline(never)]
fn scrub_stack_words() {
  // In debug builds, the runtime may fall back to conservative scanning of the Rust stack when
  // stackmap coverage is incomplete. Overwrite a chunk of the stack to reduce the chance of stale
  // young pointer bits being treated as roots.
  let mut scratch = [0usize; 16 * 1024]; // 128 KiB on 64-bit
  for slot in &mut scratch {
    *slot = 0;
  }
  std::hint::black_box(&mut scratch);
}

#[test]
fn string_alloc_triggers_minor_gc() {
  let _rt = TestRuntimeGuard::new();

  let cfg = RtGcConfig {
    nursery_size_bytes: 64 * 1024,
    los_threshold_bytes: 8 * 1024,
    minor_gc_nursery_used_percent: 1,
    // Keep majors disabled for this test: we specifically want a minor evacuation to clear a dead
    // young object.
    major_gc_old_bytes_threshold: usize::MAX,
    major_gc_old_blocks_threshold: usize::MAX,
    major_gc_external_bytes_threshold: usize::MAX,
    promote_after_minor_survivals: 1,
  };
  let limits = RtGcLimits {
    max_heap_bytes: 8 * 1024 * 1024,
    max_total_bytes: 16 * 1024 * 1024,
  };
  assert!(rt_gc_set_config(&cfg));
  assert!(rt_gc_set_limits(&limits));

  rt_thread_init(0);

  let weak = unsafe {
    let bytes = b"hello";
    let mut obj = rt_string_new_utf8(bytes.as_ptr(), bytes.len());

    // Root while registering the weak handle: acquiring the weak-handle table lock may temporarily
    // enter a GC-safe region.
    let mut scope = runtime_native::roots::RootScope::new();
    scope.push(&mut obj as *mut *mut u8);
    let h = runtime_native::rt_weak_add_h(&mut obj as *mut *mut u8);
    drop(scope);

    // Ensure the young pointer value doesn't linger in an active stack slot under conservative
    // scanning.
    core::ptr::write_volatile(&mut obj, core::ptr::null_mut());
    h
  };
  scrub_stack_words();

  // Allocate enough strings to exhaust the nursery multiple times. We intentionally do *not* call
  // `rt_gc_collect`; the string allocator should trigger GC automatically.
  for _ in 0..10_000 {
    if rt_weak_get(weak).is_null() {
      break;
    }
    let bytes = b"hello";
    let _ = rt_string_new_utf8(bytes.as_ptr(), bytes.len());
  }

  assert!(
    rt_weak_get(weak).is_null(),
    "expected allocator-triggered minor GC to clear unreachable runtime string weak handle"
  );

  rt_weak_remove(weak);
  rt_thread_deinit();
}
