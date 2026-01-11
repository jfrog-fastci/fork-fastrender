use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading::{self, ThreadKind};

fn count_roots_world_stopped() -> usize {
  threading::safepoint::with_world_stopped(|stop_epoch| {
    let mut count = 0usize;
    threading::safepoint::for_each_root_slot_world_stopped(stop_epoch, |_| {
      count += 1;
    })
    .unwrap();
    count
  })
}

struct UnregisterOnDrop;
impl Drop for UnregisterOnDrop {
  fn drop(&mut self) {
    threading::unregister_current_thread();
  }
}

#[test]
fn root_get_sees_relocation_updates() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);
  let _unreg = UnregisterOnDrop;

  let before = 0xdead_beefusize as *mut u8;
  let after = 0xcafe_babeusize as *mut u8;

  let mut root = runtime_native::roots::Root::<u8>::new(before);
  assert_eq!(root.get(), before);
  assert_eq!(count_roots_world_stopped(), 1);

  // Fake GC relocator: scan root slots and update matching pointers in-place.
  threading::safepoint::with_world_stopped(|stop_epoch| {
    let mut updated = 0usize;
    threading::safepoint::for_each_root_slot_world_stopped(stop_epoch, |slot| unsafe {
      if slot.read() == before {
        slot.write(after);
        updated += 1;
      }
    })
    .unwrap();
    assert_eq!(updated, 1);
  });

  assert_eq!(root.get(), after);

  root.set(before);
  assert_eq!(root.get(), before);
}

#[test]
fn multi_root_nesting_is_lifo() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);
  let _unreg = UnregisterOnDrop;

  assert_eq!(count_roots_world_stopped(), 0);

  {
    let _r1 = runtime_native::roots::Root::<u8>::new(0x1111usize as *mut u8);
    assert_eq!(count_roots_world_stopped(), 1);

    {
      let _r2 = runtime_native::roots::Root::<u8>::new(0x2222usize as *mut u8);
      assert_eq!(count_roots_world_stopped(), 2);
    }

    assert_eq!(count_roots_world_stopped(), 1);
  }

  assert_eq!(count_roots_world_stopped(), 0);
}

#[test]
fn root_scope_batch_pops_slots() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);
  let _unreg = UnregisterOnDrop;

  assert_eq!(count_roots_world_stopped(), 0);

  {
    let mut scope = runtime_native::roots::RootScope::new();
    let mut slot1 = 0x1111usize as *mut u8;
    let mut slot2 = 0x2222usize as *mut u8;
    scope.push(&mut slot1 as *mut *mut u8);
    scope.push(&mut slot2 as *mut *mut u8);

    assert_eq!(count_roots_world_stopped(), 2);
  }

  assert_eq!(count_roots_world_stopped(), 0);
}

#[test]
fn c_abi_shadow_stack_push_pop_works() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);
  let _unreg = UnregisterOnDrop;

  assert_eq!(count_roots_world_stopped(), 0);

  let mut slot1 = 0x1111usize as *mut u8;
  let mut slot2 = 0x2222usize as *mut u8;
  unsafe {
    runtime_native::rt_root_push(&mut slot1 as *mut *mut u8);
    runtime_native::rt_root_push(&mut slot2 as *mut *mut u8);
  }
  assert_eq!(count_roots_world_stopped(), 2);

  unsafe {
    runtime_native::rt_root_pop(&mut slot2 as *mut *mut u8);
    runtime_native::rt_root_pop(&mut slot1 as *mut *mut u8);
  }
  assert_eq!(count_roots_world_stopped(), 0);
}

