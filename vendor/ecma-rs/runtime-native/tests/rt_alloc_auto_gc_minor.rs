use std::sync::Once;

use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_alloc, rt_gc_get_young_range, rt_root_pop, rt_root_push, rt_thread_deinit, rt_thread_init, shape_table,
};

const BLOB_TOTAL_BYTES: usize = 8 * 1024;
const _: () = assert!(BLOB_TOTAL_BYTES % runtime_native::gc::OBJ_ALIGN == 0);

static SHAPE_TABLE_ONCE: Once = Once::new();
static PTR_OFFSETS: [u32; 0] = [];
static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  size: BLOB_TOTAL_BYTES as u32,
  align: runtime_native::gc::OBJ_ALIGN as u16,
  flags: 0,
  ptr_offsets: PTR_OFFSETS.as_ptr(),
  ptr_offsets_len: PTR_OFFSETS.len() as u32,
  reserved: 0,
}];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

fn nursery_contains(ptr: *mut u8, start: usize, end: usize) -> bool {
  let addr = ptr as usize;
  addr >= start && addr < end
}

#[test]
fn rt_alloc_triggers_minor_gc_without_explicit_rt_gc_collect() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();
  rt_thread_init(0);

  // Capture the nursery range. Tag the pointers so conservative stack scanning (debug fallback when
  // stackmaps are unavailable) does not treat the raw nursery start/end addresses as GC roots.
  let (young_start_tagged, young_end_tagged, nursery_bytes) = {
    let mut start: *mut u8 = core::ptr::null_mut();
    let mut end: *mut u8 = core::ptr::null_mut();
    unsafe {
      rt_gc_get_young_range(&mut start, &mut end);
    }
    assert!(!start.is_null());
    assert!(!end.is_null());
    let start_addr = start as usize;
    let end_addr = end as usize;
    assert!(start_addr < end_addr);
    ((start_addr | 1), (end_addr | 1), end_addr - start_addr)
  };
  let young_start = young_start_tagged & !1;
  let young_end = young_end_tagged & !1;
  assert!(nursery_bytes > 0);

  let mut root = rt_alloc(BLOB_TOTAL_BYTES, RtShapeId(1));
  assert!(!root.is_null());
  assert!(
    nursery_contains(root, young_start, young_end),
    "expected rt_alloc to initially allocate into the nursery"
  );

  unsafe {
    rt_root_push(&mut root as *mut *mut u8);
  }

  // Allocate enough to exceed the nursery capacity. Without allocation-triggered GC, the allocator
  // would permanently fall back to old-gen while leaving this rooted object in the nursery.
  let iters = nursery_bytes.div_ceil(BLOB_TOTAL_BYTES).saturating_add(16);
  let mut sink: usize = 0;
  for _ in 0..iters {
    // Keep only a tagged version of the returned pointer so conservative scanning doesn't retain
    // additional nursery objects as false roots.
    sink ^= (rt_alloc(BLOB_TOTAL_BYTES, RtShapeId(1)) as usize) | 1;
  }
  std::hint::black_box(sink);

  assert!(
    !nursery_contains(root, young_start, young_end),
    "expected rooted object to be evacuated out of nursery without calling rt_gc_collect"
  );

  unsafe {
    rt_root_pop(&mut root as *mut *mut u8);
  }
  rt_thread_deinit();
}
