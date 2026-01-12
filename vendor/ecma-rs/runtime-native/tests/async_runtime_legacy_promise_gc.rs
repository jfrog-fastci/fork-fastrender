use core::ffi::c_void;

use runtime_native::test_util::{drop_legacy_promise, legacy_promise_outcome, LegacyPromiseOutcome, TestRuntimeGuard};
use runtime_native::threading;
use runtime_native::threading::ThreadKind;

struct ThreadGuard;

impl ThreadGuard {
  fn new(kind: ThreadKind) -> Self {
    threading::register_current_thread(kind);
    Self
  }
}

impl Drop for ThreadGuard {
  fn drop(&mut self) {
    threading::unregister_current_thread();
  }
}

#[test]
fn legacy_promise_fulfillment_value_is_rooted_and_relocatable() {
  let _rt = TestRuntimeGuard::new();
  let _thread = ThreadGuard::new(ThreadKind::Main);

  let obj1 = Box::into_raw(Box::new(1u8)) as *mut u8;
  let obj2 = Box::into_raw(Box::new(2u8)) as *mut u8;

  let p = runtime_native::rt_promise_new_legacy();
  runtime_native::rt_promise_resolve_legacy(p, obj1.cast::<c_void>());

  assert_eq!(
    legacy_promise_outcome(runtime_native::abi::PromiseRef(p.cast())),
    LegacyPromiseOutcome::Fulfilled(obj1.cast::<c_void>())
  );

  // Reachability: the settled value should be reachable via the global root set (persistent handles).
  let mut found = 0usize;
  threading::safepoint::with_world_stopped(|epoch| {
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |slot| unsafe {
      if *slot == obj1 {
        found += 1;
      }
    })
    .expect("root enumeration should succeed");
  });
  assert_eq!(found, 1, "expected exactly one rooted fulfillment value");

  // Simulate relocation by rewriting the rooted pointer slot under STW.
  let mut updated = 0usize;
  threading::safepoint::with_world_stopped(|epoch| {
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |slot| unsafe {
      if *slot == obj1 {
        *slot = obj2;
        updated += 1;
      }
    })
    .expect("root enumeration should succeed");
  });
  assert_eq!(updated, 1, "expected exactly one rooted fulfillment slot to update");

  // Observers must see the relocated pointer.
  assert_eq!(
    legacy_promise_outcome(runtime_native::abi::PromiseRef(p.cast())),
    LegacyPromiseOutcome::Fulfilled(obj2.cast::<c_void>())
  );

  // Teardown should free the persistent handle so it no longer appears in the root set.
  unsafe { drop_legacy_promise(runtime_native::abi::PromiseRef(p.cast())) };

  let mut still_rooted = 0usize;
  threading::safepoint::with_world_stopped(|epoch| {
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |slot| unsafe {
      if *slot == obj2 {
        still_rooted += 1;
      }
    })
    .expect("root enumeration should succeed");
  });
  assert_eq!(still_rooted, 0, "fulfillment value handle should be freed on promise drop");

  unsafe {
    drop(Box::from_raw(obj1));
    drop(Box::from_raw(obj2));
  }
}

#[test]
fn legacy_promise_rejection_reason_is_rooted_and_relocatable() {
  let _rt = TestRuntimeGuard::new();
  let _thread = ThreadGuard::new(ThreadKind::Main);

  let err1 = Box::into_raw(Box::new(3u8)) as *mut u8;
  let err2 = Box::into_raw(Box::new(4u8)) as *mut u8;

  let p = runtime_native::rt_promise_new_legacy();
  // Mark handled so rejection tracking doesn't retain the promise handle after we drop it.
  unsafe {
    runtime_native::rt_promise_mark_handled(runtime_native::abi::PromiseRef(p.cast()));
  }
  runtime_native::rt_promise_reject_legacy(p, err1.cast::<c_void>());

  assert_eq!(
    legacy_promise_outcome(runtime_native::abi::PromiseRef(p.cast())),
    LegacyPromiseOutcome::Rejected(err1.cast::<c_void>())
  );

  let mut found = 0usize;
  threading::safepoint::with_world_stopped(|epoch| {
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |slot| unsafe {
      if *slot == err1 {
        found += 1;
      }
    })
    .expect("root enumeration should succeed");
  });
  assert_eq!(found, 1, "expected exactly one rooted rejection reason");

  let mut updated = 0usize;
  threading::safepoint::with_world_stopped(|epoch| {
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |slot| unsafe {
      if *slot == err1 {
        *slot = err2;
        updated += 1;
      }
    })
    .expect("root enumeration should succeed");
  });
  assert_eq!(updated, 1, "expected exactly one rooted rejection slot to update");

  assert_eq!(
    legacy_promise_outcome(runtime_native::abi::PromiseRef(p.cast())),
    LegacyPromiseOutcome::Rejected(err2.cast::<c_void>())
  );

  unsafe { drop_legacy_promise(runtime_native::abi::PromiseRef(p.cast())) };

  let mut still_rooted = 0usize;
  threading::safepoint::with_world_stopped(|epoch| {
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |slot| unsafe {
      if *slot == err2 {
        still_rooted += 1;
      }
    })
    .expect("root enumeration should succeed");
  });
  assert_eq!(still_rooted, 0, "rejection handle should be freed on promise drop");

  unsafe {
    drop(Box::from_raw(err1));
    drop(Box::from_raw(err2));
  }
}
