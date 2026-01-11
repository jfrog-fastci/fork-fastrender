use std::mem;
use std::ptr;

use runtime_native::gc::ObjHeader;
use runtime_native::test_util::TestGcGuard;

#[repr(C)]
struct FakeObj {
  header: ObjHeader,
  slot: *mut u8,
}

fn new_fake_obj() -> Box<FakeObj> {
  Box::new(FakeObj {
    // `ObjHeader` is plain data (pointers/usize). Zero-init is fine for this test; we only use the
    // remembered flag bit.
    header: unsafe { mem::zeroed() },
    slot: ptr::null_mut(),
  })
}

fn ptr_range_for_obj(obj: &mut FakeObj) -> (*mut u8, *mut u8) {
  let start = obj as *mut FakeObj as *mut u8;
  let end = unsafe { start.add(mem::size_of::<FakeObj>()) };
  (start, end)
}

#[test]
fn write_barrier_uses_updateable_young_range() {
  let _gc = TestGcGuard::new();
  let mut young_a = new_fake_obj();
  let (start_a, end_a) = ptr_range_for_obj(&mut young_a);
  runtime_native::rt_gc_set_young_range(start_a, end_a);

  let mut old_obj_a = new_fake_obj();
  let old_a_ptr = old_obj_a.as_mut() as *mut FakeObj as *mut u8;

  // Under range A, a store from old_obj -> young_a should trigger the barrier.
  old_obj_a.slot = young_a.as_mut() as *mut FakeObj as *mut u8;
  let old_a_slot_ptr = (&mut old_obj_a.slot as *mut *mut u8).cast::<u8>();
  unsafe {
    runtime_native::rt_write_barrier(old_a_ptr, old_a_slot_ptr);
  }
  assert!(old_obj_a.header.is_remembered());

  // Stores into young objects should not trigger (even if the stored value is young).
  young_a.slot = young_a.as_mut() as *mut FakeObj as *mut u8;
  let young_a_ptr = young_a.as_mut() as *mut FakeObj as *mut u8;
  let young_slot_ptr = (&mut young_a.slot as *mut *mut u8).cast::<u8>();
  unsafe {
    runtime_native::rt_write_barrier(young_a_ptr, young_slot_ptr);
  }
  assert!(!young_a.header.is_remembered());

  // Flip to a disjoint range B: young_a is no longer considered young, but young_b is.
  let mut young_b = new_fake_obj();
  let (start_b, end_b) = ptr_range_for_obj(&mut young_b);
  assert!(
    end_a <= start_b || end_b <= start_a,
    "test requires disjoint young ranges"
  );
  runtime_native::rt_gc_set_young_range(start_b, end_b);

  // Under range B, storing young_b into an old object should still trigger.
  let mut old_obj_b = new_fake_obj();
  let old_b_ptr = old_obj_b.as_mut() as *mut FakeObj as *mut u8;
  old_obj_b.slot = young_b.as_mut() as *mut FakeObj as *mut u8;
  let old_b_slot_ptr = (&mut old_obj_b.slot as *mut *mut u8).cast::<u8>();
  unsafe {
    runtime_native::rt_write_barrier(old_b_ptr, old_b_slot_ptr);
  }
  assert!(old_obj_b.header.is_remembered());

  // Under range B, storing young_a (now "old") should not trigger.
  let mut old_obj2 = new_fake_obj();
  let old2_ptr = old_obj2.as_mut() as *mut FakeObj as *mut u8;
  old_obj2.slot = young_a.as_mut() as *mut FakeObj as *mut u8;
  let slot_ptr2 = (&mut old_obj2.slot as *mut *mut u8).cast::<u8>();
  unsafe {
    runtime_native::rt_write_barrier(old2_ptr, slot_ptr2);
  }
  assert!(!old_obj2.header.is_remembered());

  // Also validate the base-object classification changes: young_a is treated as old now, so an
  // old (young_a) -> young (young_b) store triggers.
  young_a.slot = young_b.as_mut() as *mut FakeObj as *mut u8;
  unsafe {
    runtime_native::rt_write_barrier(young_a_ptr, young_slot_ptr);
  }
  assert!(young_a.header.is_remembered());

  // IMPORTANT: clear global remset before we drop the fake objects (it contains raw pointers).
  runtime_native::clear_write_barrier_state_for_tests();
}
