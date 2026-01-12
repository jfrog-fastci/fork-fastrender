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

#[test]
fn remembered_entries_flush_when_thread_exits() {
  let _gc = TestGcGuard::new();
  runtime_native::clear_write_barrier_state_for_tests();

  // Use a 1-byte "young range" around an arbitrary address so `rt_write_barrier`
  // sees a young pointer without allocating a full GC heap.
  let mut young_byte = Box::new(0u8);
  let young_ptr = (&mut *young_byte) as *mut u8;
  unsafe {
    runtime_native::rt_gc_set_young_range(young_ptr, young_ptr.add(1));
  }

  #[repr(C)]
  struct DummyObject {
    header: ObjHeader,
    field: *mut u8,
  }

  let mut old = Box::new(DummyObject {
    // The write barrier only touches atomic metadata and doesn't require a valid
    // type descriptor.
    header: unsafe { std::mem::zeroed() },
    field: young_ptr,
  });

  let obj_ptr = (&mut old.header) as *mut ObjHeader as *mut u8;
  let slot_ptr = (&mut old.field) as *mut *mut u8 as *mut u8;
  let obj_addr = obj_ptr as usize;
  let slot_addr = slot_ptr as usize;

  // Record the old object from a registered thread so the write barrier uses the
  // per-thread remset buffer. When the thread exits, its TLS registration is
  // dropped and the buffer must be flushed to the global remset so GC doesn't
  // miss the edge.
  std::thread::spawn(move || {
    // Use the low-level thread registry API instead of
    // `threading::register_current_thread`: the higher-level wrapper also
    // initializes the global heap and resets the process-wide young range to the
    // nursery backing that heap.
    //
    // This test uses a synthetic 1-byte "young range" around a dummy pointer;
    // overwriting that range would make `rt_write_barrier` fast-path and skip
    // recording the edge.
    runtime_native::threading::registry::register_current_thread(ThreadKind::External);
    unsafe {
      runtime_native::rt_write_barrier(obj_addr as *mut u8, slot_addr as *mut u8);
    }
    // Intentionally do not call `unregister_current_thread`: dropping TLS on
    // thread exit must still flush the remset buffer.
  })
  .join()
  .unwrap();

  let mut remembered = SimpleRememberedSet::new();
  runtime_native::gc::global_remset::remset_drain_into(&mut remembered);
  assert!(remembered.contains(obj_ptr));

  // Clear while the dummy object is still alive so its header remembered bit is
  // reset.
  remembered.clear();
  assert!(!unsafe { &*(obj_ptr as *const ObjHeader) }.is_remembered());
}
