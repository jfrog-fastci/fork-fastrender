use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Once};

use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  array, rt_alloc, rt_alloc_array, rt_alloc_pinned, rt_gc_collect, rt_gc_get_young_range, rt_gc_register_root_slot,
  rt_gc_root_get, rt_gc_safepoint, rt_gc_unregister_root_slot, rt_thread_deinit, rt_thread_init, shape_table,
};

#[repr(C)]
struct Leaf {
  _header: runtime_native::gc::ObjHeader,
  value: usize,
}

static SHAPE_TABLE_ONCE: Once = Once::new();
static LEAF_PTR_OFFSETS: [u32; 0] = [];
static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  size: core::mem::size_of::<Leaf>() as u32,
  align: 16,
  flags: 0,
  ptr_offsets: LEAF_PTR_OFFSETS.as_ptr(),
  ptr_offsets_len: LEAF_PTR_OFFSETS.len() as u32,
  reserved: 0,
}];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

fn nursery_contains(ptr: *mut u8) -> bool {
  let mut start: *mut u8 = core::ptr::null_mut();
  let mut end: *mut u8 = core::ptr::null_mut();
  // SAFETY: out pointers are valid.
  unsafe { rt_gc_get_young_range(&mut start, &mut end) };
  let addr = ptr as usize;
  addr >= start as usize && addr < end as usize
}

#[test]
fn rt_alloc_moves_young_object_and_updates_root_slot() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();
  rt_thread_init(0);

  let mut root: *mut u8 = core::ptr::null_mut();
  let h = rt_gc_register_root_slot(&mut root as *mut *mut u8);

  root = rt_alloc(core::mem::size_of::<Leaf>(), RtShapeId(1));
  assert!(!root.is_null());
  assert!(nursery_contains(root), "expected rt_alloc to allocate into the nursery");
  assert_eq!(rt_gc_root_get(h), root, "registered root handle should see the current slot value");

  unsafe {
    (*(root as *mut Leaf)).value = 123;
  }

  // The stack walker performs conservative scanning fallback in debug builds when stackmaps are
  // unavailable. This can relocate any stack word that looks like a young object pointer, including
  // locals we use for test bookkeeping. Tag the value so it is not a plausible object-start address.
  let before_tagged = (root as usize) | 1;
  rt_gc_collect();

  let after = rt_gc_root_get(h);
  assert!(!nursery_contains(after), "evacuated object must not remain in nursery");
  let before = (before_tagged & !1) as *mut u8;
  assert_ne!(after, before, "GC should evacuate nursery objects and update the root slot");
  unsafe {
    assert_eq!((*(after as *mut Leaf)).value, 123);
  }

  rt_gc_unregister_root_slot(h);
  rt_thread_deinit();
}

#[test]
fn rt_alloc_array_traces_pointer_elems_and_relocates() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();
  rt_thread_init(0);

  // Allocate two leaf objects and store them into a pointer array. Only the array itself is rooted;
  // the leaves must stay alive via the traced element slots.
  let a = rt_alloc(core::mem::size_of::<Leaf>(), RtShapeId(1));
  let b = rt_alloc(core::mem::size_of::<Leaf>(), RtShapeId(1));
  unsafe {
    (*(a as *mut Leaf)).value = 1;
    (*(b as *mut Leaf)).value = 2;
  }

  let arr = rt_alloc_array(2, core::mem::size_of::<*mut u8>() | array::RT_ARRAY_ELEM_PTR_FLAG);
  assert!(nursery_contains(arr));

  // SAFETY: array payload is `len * elem_size` bytes and we request pointer elements.
  let elems = unsafe { array::array_data_ptr(arr).cast::<*mut u8>() };
  unsafe {
    elems.add(0).write(a);
    elems.add(1).write(b);
  }

  let mut root_arr = arr;
  let h = rt_gc_register_root_slot(&mut root_arr as *mut *mut u8);

  rt_gc_collect();

  assert!(!nursery_contains(root_arr), "array header should be evacuated out of the nursery");

  let elems = unsafe { array::array_data_ptr(root_arr).cast::<*mut u8>() };
  let a2 = unsafe { elems.add(0).read() };
  let b2 = unsafe { elems.add(1).read() };
  assert!(!nursery_contains(a2), "array element should be promoted out of nursery");
  assert!(!nursery_contains(b2), "array element should be promoted out of nursery");

  unsafe {
    assert_eq!((*(a2 as *mut Leaf)).value, 1);
    assert_eq!((*(b2 as *mut Leaf)).value, 2);
  }

  rt_gc_unregister_root_slot(h);
  rt_thread_deinit();
}

#[test]
fn rt_alloc_pinned_is_non_moving() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();
  rt_thread_init(0);

  let mut root: *mut u8 = core::ptr::null_mut();
  let h = rt_gc_register_root_slot(&mut root as *mut *mut u8);

  root = rt_alloc_pinned(core::mem::size_of::<Leaf>(), RtShapeId(1));
  assert!(!root.is_null());
  assert!(!nursery_contains(root), "pinned objects must not be allocated into the nursery");

  unsafe {
    (*(root as *mut Leaf)).value = 7;
  }

  let before = root;
  rt_gc_collect();
  let after = root;

  assert_eq!(after, before, "pinned objects must have a stable address across GC");
  unsafe {
    assert_eq!((*(after as *mut Leaf)).value, 7);
  }

  rt_gc_unregister_root_slot(h);
  rt_thread_deinit();
}

#[test]
fn stw_gc_is_deadlock_free_across_threads() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();

  const WORKERS: usize = 4;
  let start = Arc::new(Barrier::new(WORKERS + 1));
  let stop = Arc::new(AtomicBool::new(false));

  // Register the coordinator (main test thread) as a mutator so stackmap-based enumeration is
  // exercised when available.
  rt_thread_init(0);

  let mut handles = Vec::with_capacity(WORKERS);
  for idx in 0..WORKERS {
    let start = start.clone();
    let stop = stop.clone();
    handles.push(std::thread::spawn(move || {
      rt_thread_init(1);

      let mut root: *mut u8 = core::ptr::null_mut();
      let h = rt_gc_register_root_slot(&mut root as *mut *mut u8);

      root = rt_alloc(core::mem::size_of::<Leaf>(), RtShapeId(1));
      unsafe {
        (*(root as *mut Leaf)).value = idx;
      }

      start.wait();

      while !stop.load(Ordering::Acquire) {
        let _garbage = rt_alloc(core::mem::size_of::<Leaf>(), RtShapeId(1));
        rt_gc_safepoint();
        std::thread::yield_now();
      }

      unsafe {
        assert_eq!((*(root as *mut Leaf)).value, idx);
      }

      rt_gc_unregister_root_slot(h);
      rt_thread_deinit();
    }));
  }

  start.wait();

  for _ in 0..20 {
    rt_gc_collect();
  }

  stop.store(true, Ordering::Release);
  for h in handles {
    h.join().unwrap();
  }

  rt_thread_deinit();
}
