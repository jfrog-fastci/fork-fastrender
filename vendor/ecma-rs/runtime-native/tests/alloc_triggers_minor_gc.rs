use std::sync::Once;

use runtime_native::abi::{RtGcConfig, RtGcLimits, RtShapeDescriptor, RtShapeId};
use runtime_native::gc::OBJ_HEADER_SIZE;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_alloc, rt_gc_set_config, rt_gc_set_limits, rt_thread_deinit, rt_thread_init, rt_weak_get,
  rt_weak_remove,
};

const SHAPE_MARKER: RtShapeId = RtShapeId(1);
const MARKER_PAYLOAD_BYTES: usize = 16;
const MARKER_OBJ_SIZE: usize = OBJ_HEADER_SIZE + MARKER_PAYLOAD_BYTES;

static EMPTY_PTR_OFFSETS: [u32; 0] = [];
static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  size: MARKER_OBJ_SIZE as u32,
  align: 16,
  flags: 0,
  ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
  ptr_offsets_len: 0,
  reserved: 0,
}];

fn ensure_shape_table() {
  static ONCE: Once = Once::new();
  ONCE.call_once(|| unsafe {
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

#[inline(never)]
fn alloc_unrooted_young_marker_weak_handle() -> u64 {
  unsafe {
    let mut obj = rt_alloc(MARKER_OBJ_SIZE, SHAPE_MARKER);

    // Root the object while registering the weak handle: lock contention while
    // acquiring the weak-handle table may temporarily enter a GC-safe region.
    let mut scope = runtime_native::roots::RootScope::new();
    scope.push(&mut obj as *mut *mut u8);
    let h = runtime_native::rt_weak_add_h(&mut obj as *mut *mut u8);
    drop(scope);

    // In debug builds, `runtime-native` may fall back to conservative scanning of
    // the Rust stack when stackmap coverage is incomplete. Ensure this function
    // does not leave the young pointer value in an active stack slot so the
    // subsequent allocator-triggered minor GC can observe the referent as dead.
    core::ptr::write_volatile(&mut obj, core::ptr::null_mut());

    h
  }
}

#[inline(never)]
fn scrub_stack_words() {
  // In debug builds, conservative scanning can treat any stale young pointer bits
  // in the current thread's stack as a GC root. Overwrite a chunk of the stack to
  // reduce the chance of those stale values keeping the young referent alive.
  //
  // This is a test-only helper; production code should rely on precise stackmaps
  // and explicit rooting (handle stack / shadow stack).
  let mut scratch = [0usize; 16 * 1024]; // 128 KiB on 64-bit
  for slot in &mut scratch {
    *slot = 0;
  }
  std::hint::black_box(&mut scratch);
}

#[test]
fn alloc_triggers_minor_gc() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();

  let cfg = RtGcConfig {
    nursery_size_bytes: 64 * 1024,
    los_threshold_bytes: 8 * 1024,
    // Trigger a minor collection quickly once the nursery has any reserved bytes.
    minor_gc_nursery_used_percent: 1,
    // Keep majors disabled for this test: we specifically want a minor evacuation to clear a
    // dead young object.
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

  // Allocate one young object, register a weak handle for it, then drop all strong roots. A minor
  // GC should clear the weak handle once the nursery is collected.
  let weak = alloc_unrooted_young_marker_weak_handle();
  scrub_stack_words();

  // Allocate enough to exhaust the tiny nursery multiple times. We intentionally do *not* call
  // `rt_gc_collect`; the allocator should trigger GC automatically.
  for _ in 0..10_000 {
    let _ = rt_alloc(MARKER_OBJ_SIZE, SHAPE_MARKER);
    if rt_weak_get(weak).is_null() {
      break;
    }
  }

  assert!(
    rt_weak_get(weak).is_null(),
    "expected allocator-triggered minor GC to clear unreachable nursery weak handle"
  );

  rt_weak_remove(weak);
  rt_thread_deinit();
}
