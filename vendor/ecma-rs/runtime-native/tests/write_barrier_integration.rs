use std::mem;

use runtime_native::gc::ObjHeader;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootStack;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::gc::TypeDescriptor;
use runtime_native::test_util::TestGcGuard;
use runtime_native::threading::ThreadKind;
use runtime_native::GcHeap;

#[repr(C)]
struct Node {
  header: ObjHeader,
  next: *mut u8,
}

static NODE_PTR_OFFSETS: [u32; 1] = [mem::offset_of!(Node, next) as u32];
static NODE_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<Node>(), &NODE_PTR_OFFSETS);

#[test]
fn write_barrier_feeds_remembered_set_for_minor_gc() {
  let _gc = TestGcGuard::new();
  runtime_native::clear_write_barrier_state_for_tests();
  runtime_native::threading::register_current_thread(ThreadKind::External);

  let mut heap = GcHeap::new();
  let (start, end) = heap.nursery_range();
  runtime_native::rt_gc_set_young_range(start, end);

  let old = heap.alloc_old(&NODE_DESC);
  let young = heap.alloc_young(&NODE_DESC);

  unsafe {
    (*(old as *mut Node)).next = young;
  }

  // Call the exported write barrier as codegen would: pass the base object and
  // the address of the slot that now contains the pointer.
  let slot = unsafe { &mut (*(old as *mut Node)).next as *mut *mut u8 };
  unsafe {
    runtime_native::rt_write_barrier(old, slot.cast::<u8>());
  }

  // Drain the global remembered-set buffers into the set used by the GC.
  let mut remembered = SimpleRememberedSet::new();
  runtime_native::gc::global_remset::remset_drain_into(&mut remembered);
  assert!(remembered.contains(old));
  let mut count = 0usize;
  remembered.for_each_remembered_obj(&mut |obj| {
    assert_eq!(obj, old);
    count += 1;
  });
  assert_eq!(count, 1);

  let mut root_old = old;
  let mut roots = RootStack::new();
  roots.push(&mut root_old as *mut *mut u8);

  heap.collect_minor(&mut roots, &mut remembered).unwrap();

  // The young object should have been evacuated and the interior pointer in the
  // old object updated.
  let updated = unsafe { (*(old as *mut Node)).next };
  assert!(!updated.is_null());
  assert!(!heap.is_in_nursery(updated));
  assert!(heap.is_in_immix(updated));
  assert_ne!(updated, young);

  // `collect_minor` clears the remembered set at the end.
  let mut count = 0usize;
  remembered.for_each_remembered_obj(&mut |_| {
    count += 1;
  });
  assert_eq!(count, 0);
  assert!(!unsafe { &*(old as *const ObjHeader) }.is_remembered());

  // The global buffers should also be empty after draining.
  let mut after = SimpleRememberedSet::new();
  runtime_native::gc::global_remset::remset_drain_into(&mut after);
  let mut count = 0usize;
  after.for_each_remembered_obj(&mut |_| {
    count += 1;
  });
  assert_eq!(count, 0);

  runtime_native::threading::unregister_current_thread();
}
