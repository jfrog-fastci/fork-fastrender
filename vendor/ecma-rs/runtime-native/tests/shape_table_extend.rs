use std::ptr;
use std::sync::Once;

use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::gc::ObjHeader;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_alloc, rt_gc_collect, rt_gc_register_root_slot, rt_gc_unregister_root_slot, rt_thread_deinit,
  rt_thread_init, rt_weak_add, rt_weak_get, rt_weak_remove,
};

#[repr(C)]
struct Pair {
  _header: ObjHeader,
  left: *mut u8,
  right: *mut u8,
}

static SHAPE_TABLE_ONCE: Once = Once::new();
static LEAF_PTR_OFFSETS: [u32; 0] = [];
static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  size: core::mem::size_of::<ObjHeader>() as u32,
  align: 16,
  flags: 0,
  ptr_offsets: LEAF_PTR_OFFSETS.as_ptr(),
  ptr_offsets_len: LEAF_PTR_OFFSETS.len() as u32,
  reserved: 0,
}];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    runtime_native::shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

#[test]
fn shape_table_extend_copies_offsets_and_traces() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();
  rt_thread_init(3);

  // Build a shape descriptor at runtime whose `ptr_offsets` are heap-allocated.
  let mut ptr_offsets = vec![core::mem::offset_of!(Pair, left) as u32];
  let pair_desc = RtShapeDescriptor {
    size: core::mem::size_of::<Pair>() as u32,
    align: 16,
    flags: 0,
    ptr_offsets: ptr_offsets.as_ptr(),
    ptr_offsets_len: ptr_offsets.len() as u32,
    reserved: 0,
  };

  let pair_shape = unsafe {
    runtime_native::shape_table::rt_register_shape_table_extend(ptr::from_ref(&pair_desc), 1)
  };
  assert_eq!(pair_shape, RtShapeId(2));

  // Mutate + drop the original offsets array to prove the runtime copied it.
  //
  // If the runtime incorrectly retains `pair_desc.ptr_offsets`, it would observe this mutated offset
  // and fail to trace `Pair.left`.
  ptr_offsets[0] = core::mem::offset_of!(Pair, right) as u32;
  drop(ptr_offsets);

  let mut leaf = rt_alloc(core::mem::size_of::<ObjHeader>(), RtShapeId(1));
  assert!(!leaf.is_null());

  let pair = rt_alloc(core::mem::size_of::<Pair>(), pair_shape);
  assert!(!pair.is_null());

  unsafe {
    let pair = &mut *(pair as *mut Pair);
    pair.left = leaf;
    pair.right = ptr::null_mut();
  }

  let mut root_pair = pair;
  let root = rt_gc_register_root_slot(&mut root_pair as *mut *mut u8);
  let weak = rt_weak_add(leaf);

  // Ensure conservative stack scanning (debug fallback) doesn't keep the leaf alive.
  unsafe {
    ptr::write_volatile(&mut leaf, ptr::null_mut());
  }

  rt_gc_collect();

  let leaf2 = rt_weak_get(weak);
  assert!(!leaf2.is_null(), "leaf must remain alive via Pair.left tracing");

  let pair2 = root_pair as *mut Pair;
  let traced = unsafe { (*pair2).left };
  assert_eq!(traced, leaf2, "GC should update traced fields in-place");

  rt_weak_remove(weak);
  rt_gc_unregister_root_slot(root);
  rt_thread_deinit();
}

