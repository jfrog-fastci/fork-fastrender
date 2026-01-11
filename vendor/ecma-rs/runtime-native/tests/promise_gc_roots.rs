use std::sync::atomic::{AtomicUsize, Ordering};

use runtime_native::abi::PromiseRef;
use runtime_native::abi::ValueRef;
use runtime_native::test_util::TestRuntimeGuard;

fn drain_async_runtime() {
  while runtime_native::rt_async_poll() {}
}

fn replace_global_root_ptr(old: *mut u8, new: *mut u8) -> usize {
  let mut replaced = 0usize;
  runtime_native::gc::roots::debug_for_each_global_root_slot_mut(|slot| unsafe {
    if *slot == old {
      *slot = new;
      replaced += 1;
    }
  });
  replaced
}

#[repr(C)]
struct ThenData {
  promise: PromiseRef,
  observed: AtomicUsize,
}

extern "C" fn on_settle_observe_outcome(data: *mut u8) {
  let data = unsafe { &*(data as *const ThenData) };
  let (_state, value) = runtime_native::rt_debug_promise_outcome(data.promise);
  data.observed.store(value as usize, Ordering::Release);
}

#[test]
fn promise_outcome_is_relocatable_via_global_root_handle() {
  let _rt = TestRuntimeGuard::new();

  runtime_native::gc::roots::debug_clear_global_roots_for_tests();
  drain_async_runtime();

  let promise = runtime_native::rt_promise_new_legacy();

  let mut then_data = ThenData {
    promise,
    observed: AtomicUsize::new(0),
  };
  runtime_native::rt_promise_then_legacy(
    promise,
    on_settle_observe_outcome,
    (&mut then_data as *mut ThenData).cast::<u8>(),
  );

  let old_obj = Box::into_raw(Box::new(1u8));
  let new_obj = Box::into_raw(Box::new(2u8));

  runtime_native::rt_promise_resolve_legacy(promise, old_obj.cast::<core::ffi::c_void>() as ValueRef);
  // Resolving the promise installs a persistent-handle root for the fulfillment value, and also
  // queues a reaction job whose state holds a rooted `PromiseRef` while it sits in the microtask
  // queue.
  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 2);

  // Simulate a moving GC updating the stored root slot in-place.
  assert_eq!(replace_global_root_ptr(old_obj.cast(), new_obj.cast()), 1);

  let (state, value) = runtime_native::rt_debug_promise_outcome(promise);
  assert_eq!(state, 1, "promise should be fulfilled");
  assert_eq!(value as *mut u8, new_obj.cast());

  // Run the queued microtask and verify the continuation observes the relocated pointer.
  drain_async_runtime();
  assert_eq!(then_data.observed.load(Ordering::Acquire) as *mut u8, new_obj.cast());

  runtime_native::rt_promise_drop_legacy(promise);
  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 0);

  // Safety: the pointers came from `Box::into_raw` above and are no longer referenced by the
  // promise/root table.
  unsafe {
    drop(Box::from_raw(old_obj));
    drop(Box::from_raw(new_obj));
  }
}

#[test]
fn promise_drop_releases_persistent_roots() {
  let _rt = TestRuntimeGuard::new();

  runtime_native::gc::roots::debug_clear_global_roots_for_tests();
  drain_async_runtime();

  let promise = runtime_native::rt_promise_new_legacy();

  let obj = Box::into_raw(Box::new(123u8));
  runtime_native::rt_promise_resolve_legacy(promise, obj.cast::<core::ffi::c_void>() as ValueRef);
  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 1);

  runtime_native::rt_promise_drop_legacy(promise);
  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 0);

  // Safety: the pointer came from `Box::into_raw` above and is no longer referenced by the
  // promise/root table.
  unsafe {
    drop(Box::from_raw(obj));
  }
}
