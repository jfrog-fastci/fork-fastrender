use std::ptr;
use std::sync::Once;

use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{rt_alloc, rt_gc_collect, rt_root_pop, rt_root_push, rt_thread_deinit, rt_thread_init, rt_weak_add, rt_weak_get, rt_weak_remove};

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];
static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  // `ObjHeader` is 16 bytes on 64-bit (type_desc pointer + meta word).
  size: 16,
  align: 16,
  flags: 0,
  ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
  ptr_offsets_len: 0,
  reserved: 0,
}];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    runtime_native::shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

#[inline(never)]
fn alloc_unrooted_weak_handle() -> u64 {
  // Keep the object pointer out of the caller's stack frame so conservative stack scanning (debug
  // fallback) doesn't accidentally keep it alive.
  let mut obj = rt_alloc(16, RtShapeId(1));
  let handle = rt_weak_add(obj);
  // Ensure the stack slot does not retain the young pointer.
  unsafe {
    ptr::write_volatile(&mut obj, ptr::null_mut());
  }
  handle
}

#[test]
fn rt_gc_collect_clears_weak_handles_for_unreachable_objects() {
  ensure_shape_table();
  let _rt = TestRuntimeGuard::new();
  rt_thread_init(3);

  // Unrooted object should be collected and clear its weak handle.
  let weak = alloc_unrooted_weak_handle();
  rt_gc_collect();
  assert_eq!(rt_weak_get(weak), ptr::null_mut());
  rt_weak_remove(weak);

  // Rooted via the per-thread handle stack should keep the object alive across GC.
  let mut slot = rt_alloc(16, RtShapeId(1));
  let weak = rt_weak_add(slot);
  unsafe {
    rt_root_push(&mut slot as *mut *mut u8);
  }
  rt_gc_collect();
  assert_eq!(rt_weak_get(weak), slot);

  unsafe {
    rt_root_pop(&mut slot as *mut *mut u8);
  }
  rt_gc_collect();
  assert_eq!(rt_weak_get(weak), ptr::null_mut());
  rt_weak_remove(weak);

  rt_thread_deinit();
}

