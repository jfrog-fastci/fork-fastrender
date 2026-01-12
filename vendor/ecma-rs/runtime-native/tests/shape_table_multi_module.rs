use std::ptr;

use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_alloc, rt_gc_collect, rt_root_pop, rt_root_push, rt_thread_deinit, rt_thread_init, rt_weak_add,
  rt_weak_get, rt_weak_remove,
};

// Table 1: leaf object (header only).
static LEAF_PTR_OFFSETS: [u32; 0] = [];
static TABLE1: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  size: runtime_native::gc::OBJ_HEADER_SIZE as u32,
  align: runtime_native::gc::OBJ_ALIGN as u16,
  flags: 0,
  ptr_offsets: LEAF_PTR_OFFSETS.as_ptr(),
  ptr_offsets_len: LEAF_PTR_OFFSETS.len() as u32,
  reserved: 0,
}];

// Table 2: wrapper with two pointer-sized fields, but only the *second* one is traced.
static WRAPPER_PTR_OFFSETS: [u32; 1] = [(
  runtime_native::gc::OBJ_HEADER_SIZE + core::mem::size_of::<*mut u8>()
) as u32];
static TABLE2: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  size: (runtime_native::gc::OBJ_HEADER_SIZE + 2 * core::mem::size_of::<*mut u8>()) as u32,
  align: runtime_native::gc::OBJ_ALIGN as u16,
  flags: 0,
  ptr_offsets: WRAPPER_PTR_OFFSETS.as_ptr(),
  ptr_offsets_len: WRAPPER_PTR_OFFSETS.len() as u32,
  reserved: 0,
}];

#[test]
fn shape_table_multi_module_append() {
  let _rt = TestRuntimeGuard::new();
  rt_thread_init(3);

  // Register two synthetic "modules" worth of shapes.
  let base1 = unsafe { runtime_native::shape_table::rt_register_shape_table_append(TABLE1.as_ptr(), TABLE1.len()) };
  let base2 = unsafe { runtime_native::shape_table::rt_register_shape_table_append(TABLE2.as_ptr(), TABLE2.len()) };

  assert_eq!(base1, RtShapeId(1));
  assert_eq!(base2, RtShapeId(2));

  let leaf_shape = RtShapeId(base1.0 + 0);
  let wrapper_shape = RtShapeId(base2.0 + 0);

  // Allocate two leaf objects. Only `live` is referenced by a traced slot.
  let mut live = rt_alloc(TABLE1[0].size as usize, leaf_shape);
  let weak_live = rt_weak_add(live);

  let mut die = rt_alloc(TABLE1[0].size as usize, leaf_shape);
  let weak_die = rt_weak_add(die);

  // Allocate a wrapper object whose descriptor traces only the second pointer slot.
  let mut wrapper = rt_alloc(TABLE2[0].size as usize, wrapper_shape);
  assert!(!wrapper.is_null());

  unsafe {
    let base = wrapper as *mut u8;
    let header_size = runtime_native::gc::OBJ_HEADER_SIZE;
    let ptr_size = core::mem::size_of::<*mut u8>();

    // Untraced slot: should not keep `die` alive.
    *base.add(header_size).cast::<*mut u8>() = die;
    // Traced slot: keeps `live` alive.
    *base.add(header_size + ptr_size).cast::<*mut u8>() = live;

    // Keep the leaf pointers out of the Rust frame so conservative stack scanning doesn't keep
    // them alive: after this, the wrapper's traced slot is the only root for `live`.
    ptr::write_volatile(&mut live, ptr::null_mut());
    ptr::write_volatile(&mut die, ptr::null_mut());
  }

  unsafe {
    rt_root_push(&mut wrapper as *mut *mut u8);
  }

  rt_gc_collect();

  // `live` survives because it is referenced by a traced pointer slot in `wrapper`.
  let live_after = rt_weak_get(weak_live);
  assert_ne!(live_after, ptr::null_mut(), "expected traced leaf object to survive");

  // `die` is referenced only through an *untraced* slot and should be reclaimed.
  assert_eq!(
    rt_weak_get(weak_die),
    ptr::null_mut(),
    "expected untraced leaf object to be collected"
  );

  // The traced slot in `wrapper` should be updated to the relocated `live` pointer.
  unsafe {
    let base = wrapper as *mut u8;
    let header_size = runtime_native::gc::OBJ_HEADER_SIZE;
    let ptr_size = core::mem::size_of::<*mut u8>();
    let slot = base.add(header_size + ptr_size).cast::<*mut u8>();
    assert_eq!(*slot, live_after);
  }

  unsafe {
    rt_root_pop(&mut wrapper as *mut *mut u8);
  }

  rt_weak_remove(weak_live);
  rt_weak_remove(weak_die);
  rt_thread_deinit();
}

