use runtime_native::{enumerate_root_slots, register_global_root_slot, unregister_global_root_slot};

#[test]
fn global_root_slots_are_visited_and_can_be_relocated() {
  let _rt = runtime_native::test_util::TestRuntimeGuard::new();
  let mut slot: usize = 0x1111;
  let slot_ptr: *mut usize = (&mut slot) as *mut usize;

  register_global_root_slot(slot_ptr);

  let mut saw_slot = false;
  enumerate_root_slots(|s| {
    if s == slot_ptr {
      saw_slot = true;
      unsafe {
        *s = 0x2222;
      }
    }
  })
  .unwrap();

  assert!(saw_slot, "expected global root slot to be visited");
  assert_eq!(slot, 0x2222);

  unregister_global_root_slot(slot_ptr);
  slot = 0x3333;

  let mut saw_slot_after_unreg = false;
  enumerate_root_slots(|s| {
    if s == slot_ptr {
      saw_slot_after_unreg = true;
      unsafe {
        *s = 0x4444;
      }
    }
  })
  .unwrap();

  assert!(
    !saw_slot_after_unreg,
    "unregistered global root slot should no longer be visited"
  );
  assert_eq!(slot, 0x3333);
}
