use std::mem;
use std::ptr;

use runtime_native::gc::{ObjHeader, SimpleRememberedSet, TypeDescriptor};
use runtime_native::test_util::TestGcGuard;
use runtime_native::{MutatorThread, ThreadContextGuard};

#[repr(C)]
struct TestObject {
  header: ObjHeader,
  field: *mut u8,
}

static TEST_OBJ_PTR_OFFSETS: [u32; 1] = [mem::offset_of!(TestObject, field) as u32];
static TEST_OBJ_DESC: TypeDescriptor =
  TypeDescriptor::new(mem::size_of::<TestObject>(), &TEST_OBJ_PTR_OFFSETS);

impl TestObject {
  fn new() -> Self {
    Self {
      header: ObjHeader::new(&TEST_OBJ_DESC),
      field: ptr::null_mut(),
    }
  }
}

fn set_young_range_for<T>(b: &Box<T>) -> (*mut u8, *mut u8) {
  let start = (&**b as *const T) as *mut u8;
  let end = unsafe { start.add(mem::size_of::<T>()) };
  runtime_native::rt_gc_set_young_range(start, end);
  (start, end)
}

#[test]
fn barrier_old_to_young_store_adds_object_once() {
  let _gc = TestGcGuard::new();
  runtime_native::clear_write_barrier_state_for_tests();

  let mut thread = MutatorThread::new();
  let _guard = ThreadContextGuard::install(&mut thread);

  let mut old_obj = Box::new(TestObject::new());
  let young_value = Box::new(0u8);

  set_young_range_for(&young_value);

  old_obj.field = (&*young_value as *const u8) as *mut u8;
  let obj_ptr = (&mut *old_obj as *mut TestObject) as *mut u8;
  let slot_ptr = (&mut old_obj.field as *mut *mut u8) as *mut u8;

  unsafe {
    runtime_native::rt_write_barrier(obj_ptr, slot_ptr);
    runtime_native::rt_write_barrier(obj_ptr, slot_ptr);
  }

  assert_eq!(thread.new_remembered, vec![obj_ptr]);
  assert!(old_obj.header.is_remembered());

  // Merge buffers as a minor GC would.
  let mut remset = SimpleRememberedSet::new();
  remset.prepare_for_minor_gc(std::iter::once(&mut thread));
  assert!(remset.contains(obj_ptr));
}

#[test]
fn barrier_old_to_old_store_does_nothing() {
  let _gc = TestGcGuard::new();
  runtime_native::clear_write_barrier_state_for_tests();

  let mut thread = MutatorThread::new();
  let _guard = ThreadContextGuard::install(&mut thread);

  let mut old_obj = Box::new(TestObject::new());
  let old_value = Box::new(123u8);
  let young_dummy = Box::new(0u8);

  // Ensure the young range does not include `old_value`.
  set_young_range_for(&young_dummy);

  old_obj.field = (&*old_value as *const u8) as *mut u8;
  let obj_ptr = (&mut *old_obj as *mut TestObject) as *mut u8;
  let slot_ptr = (&mut old_obj.field as *mut *mut u8) as *mut u8;

  unsafe { runtime_native::rt_write_barrier(obj_ptr, slot_ptr) };

  assert!(thread.new_remembered.is_empty());
  assert!(!old_obj.header.is_remembered());
}

#[test]
fn barrier_young_object_store_does_nothing() {
  let _gc = TestGcGuard::new();
  runtime_native::clear_write_barrier_state_for_tests();

  let mut thread = MutatorThread::new();
  let _guard = ThreadContextGuard::install(&mut thread);

  #[repr(C)]
  struct Pair {
    a: TestObject,
    b: TestObject,
  }

  let mut pair = Box::new(Pair {
    a: TestObject::new(),
    b: TestObject::new(),
  });

  let start = (&mut *pair as *mut Pair) as *mut u8;
  let end = unsafe { start.add(mem::size_of::<Pair>()) };
  runtime_native::rt_gc_set_young_range(start, end);

  pair.a.field = (&mut pair.b as *mut TestObject) as *mut u8;
  let obj_ptr = (&mut pair.a as *mut TestObject) as *mut u8;
  let slot_ptr = (&mut pair.a.field as *mut *mut u8) as *mut u8;

  unsafe { runtime_native::rt_write_barrier(obj_ptr, slot_ptr) };

  assert!(thread.new_remembered.is_empty());
  assert!(!pair.a.header.is_remembered());
}

#[test]
fn barrier_null_store_does_nothing() {
  let _gc = TestGcGuard::new();
  runtime_native::clear_write_barrier_state_for_tests();

  let mut thread = MutatorThread::new();
  let _guard = ThreadContextGuard::install(&mut thread);

  let mut old_obj = Box::new(TestObject::new());
  let young_dummy = Box::new(0u8);
  set_young_range_for(&young_dummy);

  old_obj.field = ptr::null_mut();
  let obj_ptr = (&mut *old_obj as *mut TestObject) as *mut u8;
  let slot_ptr = (&mut old_obj.field as *mut *mut u8) as *mut u8;

  unsafe { runtime_native::rt_write_barrier(obj_ptr, slot_ptr) };

  assert!(thread.new_remembered.is_empty());
  assert!(!old_obj.header.is_remembered());
}

#[test]
fn remembered_set_is_sticky_and_clears_bit_when_object_drops_young_ptrs() {
  let _gc = TestGcGuard::new();
  runtime_native::clear_write_barrier_state_for_tests();

  let mut thread = MutatorThread::new();
  let _guard = ThreadContextGuard::install(&mut thread);

  let mut old_obj = Box::new(TestObject::new());
  let young_value = Box::new(0u8);
  let old_value = Box::new(1u8);

  let (young_start, young_end) = set_young_range_for(&young_value);

  // Create old→young edge and trigger barrier.
  old_obj.field = (&*young_value as *const u8) as *mut u8;
  let obj_ptr = (&mut *old_obj as *mut TestObject) as *mut u8;
  let slot_ptr = (&mut old_obj.field as *mut *mut u8) as *mut u8;
  unsafe { runtime_native::rt_write_barrier(obj_ptr, slot_ptr) };

  let mut remset = SimpleRememberedSet::new();
  remset.prepare_for_minor_gc(std::iter::once(&mut thread));
  assert!(remset.contains(obj_ptr));
  assert!(old_obj.header.is_remembered());

  // Remove the young pointer (old→old store); the sticky remembered bit stays
  // set until the next minor GC rescan.
  old_obj.field = (&*old_value as *const u8) as *mut u8;
  assert!(old_obj.header.is_remembered());

  remset.scan_and_rebuild(|obj| unsafe {
    let obj = &*(obj as *const TestObject);
    let value = obj.field;
    let addr = value as usize;
    !value.is_null() && addr >= young_start as usize && addr < young_end as usize
  });

  assert!(!remset.contains(obj_ptr));
  assert!(!old_obj.header.is_remembered());
}
