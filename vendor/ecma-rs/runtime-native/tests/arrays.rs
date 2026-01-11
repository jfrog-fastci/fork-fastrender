use std::ptr;

use runtime_native::gc::heap::IMMIX_MAX_OBJECT_SIZE;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootStack;
use runtime_native::GcHeap;
use runtime_native::TypeDescriptor;

// A remembered set that does nothing. Arrays in these tests are only linked via
// roots and young-to-young pointers, so we don't need write-barrier coverage.
#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[test]
fn ptr_arrays_trace_elements_in_minor_and_major() {
  let mut heap = GcHeap::new();

  // Allocate elements that will be promoted into the large-object space so we
  // can observe liveness via `los_object_count()`.
  let big_len = IMMIX_MAX_OBJECT_SIZE;

  let arr = heap.alloc_array_young(
    2,
    runtime_native::array::RT_ARRAY_ELEM_PTR_FLAG | core::mem::size_of::<*mut u8>(),
  );
  let a = heap.alloc_array_young(big_len, 1);
  let b = heap.alloc_array_young(big_len, 1);

  unsafe {
    // Fill payload bytes so we can verify evacuation copies the whole object.
    ptr::write_bytes(runtime_native::rt_array_data(a), 0xA5, 4);
    ptr::write_bytes(runtime_native::rt_array_data(b), 0x5A, 4);

    let slots = runtime_native::rt_array_data(arr) as *mut *mut u8;
    slots.add(0).write(a);
    slots.add(1).write(b);
  }

  assert_eq!(runtime_native::rt_array_len(arr), 2);

  // Minor GC should evacuate the pointer array and both referenced objects.
  let mut root_arr = arr;
  let mut roots = RootStack::new();
  roots.push(&mut root_arr as *mut *mut u8);
  heap.collect_minor(&mut roots, &mut NullRememberedSet::default());

  assert!(!heap.is_in_nursery(root_arr));
  let a2 = unsafe { *(runtime_native::rt_array_data(root_arr) as *mut *mut u8).add(0) };
  let b2 = unsafe { *(runtime_native::rt_array_data(root_arr) as *mut *mut u8).add(1) };

  assert!(!heap.is_in_nursery(a2));
  assert!(!heap.is_in_nursery(b2));
  assert!(heap.is_in_los(a2));
  assert!(heap.is_in_los(b2));
  assert_eq!(runtime_native::rt_array_len(a2), big_len);
  assert_eq!(runtime_native::rt_array_len(b2), big_len);
  unsafe {
    assert_eq!(*runtime_native::rt_array_data(a2), 0xA5);
    assert_eq!(*runtime_native::rt_array_data(b2), 0x5A);
  }

  // Major GC marking must trace pointer-array elements.
  heap.collect_major(&mut roots, &mut NullRememberedSet::default());
  assert_eq!(heap.los_object_count(), 2);

  // Drop all roots: the large objects should be swept.
  let mut empty_roots = RootStack::new();
  heap.collect_major(&mut empty_roots, &mut NullRememberedSet::default());
  assert_eq!(heap.los_object_count(), 0);
}

#[test]
fn non_ptr_arrays_do_not_get_scanned_for_pointers() {
  let mut heap = GcHeap::new();

  let root_arr = heap.alloc_array_young(32, 1);

  // Allocate the victim as a LOS object so sweeping is directly observable.
  static NO_PTR_OFFSETS: [u32; 0] = [];
  static VICTIM_DESC: TypeDescriptor = TypeDescriptor::new(IMMIX_MAX_OBJECT_SIZE + 64, &NO_PTR_OFFSETS);
  let victim = heap.alloc_old(&VICTIM_DESC);
  assert!(heap.is_in_los(victim));
  assert_eq!(heap.los_object_count(), 1);

  // Store the victim pointer as raw bytes in a non-pointer array. The GC must
  // not treat these bytes as a reference.
  let bytes = (victim as usize).to_ne_bytes();
  unsafe {
    ptr::copy_nonoverlapping(
      bytes.as_ptr(),
      runtime_native::rt_array_data(root_arr),
      bytes.len(),
    );
  }

  let mut root = root_arr;
  let mut roots = RootStack::new();
  roots.push(&mut root as *mut *mut u8);
  heap.collect_major(&mut roots, &mut NullRememberedSet::default());

  assert_eq!(heap.los_object_count(), 0);
}
