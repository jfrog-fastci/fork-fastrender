use std::sync::atomic::{AtomicUsize, Ordering};

use runtime_native::abi::PromiseRef;
use runtime_native::abi::ValueRef;
use runtime_native::abi::{PromiseResolveInput, ThenableRef, ThenableVTable};
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

#[repr(C)]
struct RootedThenData {
  observed: AtomicUsize,
}

extern "C" fn on_settle_store_self_ptr(data: *mut u8) {
  let ctx = unsafe { &*(data as *const RootedThenData) };
  ctx.observed.store(data as usize, Ordering::Release);
}

#[repr(C)]
struct ThenableRecordPtr {
  observed: AtomicUsize,
}

unsafe extern "C" fn thenable_call_then_record_ptr(
  thenable: *mut u8,
  on_fulfilled: runtime_native::abi::ThenableResolveCallback,
  _on_rejected: runtime_native::abi::ThenableRejectCallback,
  data: *mut u8,
) -> ValueRef {
  let t = &*(thenable as *const ThenableRecordPtr);
  t.observed.store(thenable as usize, Ordering::Release);
  // Settle the destination promise so the resolver root is released.
  on_fulfilled(data, PromiseResolveInput::value(core::ptr::null_mut()));
  core::ptr::null_mut()
}

static THENABLE_RECORD_PTR_VTABLE: ThenableVTable = ThenableVTable {
  call_then: thenable_call_then_record_ptr,
};

unsafe extern "C" fn thenable_call_then_resolve_value(
  _thenable: *mut u8,
  on_fulfilled: runtime_native::abi::ThenableResolveCallback,
  _on_rejected: runtime_native::abi::ThenableRejectCallback,
  data: *mut u8,
) -> ValueRef {
  on_fulfilled(data, PromiseResolveInput::value(0x1234usize as ValueRef));
  core::ptr::null_mut()
}

static THENABLE_RESOLVE_VALUE_VTABLE: ThenableVTable = ThenableVTable {
  call_then: thenable_call_then_resolve_value,
};

#[repr(C)]
struct ThenableStoreCallbacks {
  resolve_cb: AtomicUsize,
  data: AtomicUsize,
}

unsafe extern "C" fn thenable_call_then_store_callbacks(
  thenable: *mut u8,
  on_fulfilled: runtime_native::abi::ThenableResolveCallback,
  _on_rejected: runtime_native::abi::ThenableRejectCallback,
  data: *mut u8,
) -> ValueRef {
  let t = &*(thenable as *const ThenableStoreCallbacks);
  t.resolve_cb.store(on_fulfilled as usize, Ordering::Release);
  t.data.store(data as usize, Ordering::Release);
  core::ptr::null_mut()
}

static THENABLE_STORE_CALLBACKS_VTABLE: ThenableVTable = ThenableVTable {
  call_then: thenable_call_then_store_callbacks,
};

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
fn promise_then_rooted_passes_relocated_data_ptr() {
  let _rt = TestRuntimeGuard::new();

  runtime_native::gc::roots::debug_clear_global_roots_for_tests();
  drain_async_runtime();

  let promise = runtime_native::rt_promise_new_legacy();

  let old_data = Box::into_raw(Box::new(RootedThenData {
    observed: AtomicUsize::new(0),
  }));
  let new_data = Box::into_raw(Box::new(RootedThenData {
    observed: AtomicUsize::new(0),
  }));

  runtime_native::rt_promise_then_rooted_legacy(promise, on_settle_store_self_ptr, old_data.cast());
  // `then_rooted` installs a persistent handle for the rooted callback `data`.
  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 1);

  // Simulate a moving GC updating the rooted callback data pointer in-place.
  assert_eq!(replace_global_root_ptr(old_data.cast(), new_data.cast()), 1);

  // Resolve the promise with null, so only the callback's data pointer is rooted (not the value).
  // The queued reaction job also contributes a temporary root while it waits in the microtask
  // queue.
  runtime_native::rt_promise_resolve_legacy(promise, core::ptr::null_mut());
  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 2);

  drain_async_runtime();
  assert_eq!(
    unsafe { &*new_data }.observed.load(Ordering::Acquire) as *mut u8,
    new_data.cast()
  );
  assert_eq!(unsafe { &*old_data }.observed.load(Ordering::Acquire), 0);

  runtime_native::rt_promise_drop_legacy(promise);
  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 0);

  unsafe {
    drop(Box::from_raw(old_data));
    drop(Box::from_raw(new_data));
  }
}

#[test]
fn promise_thenable_job_passes_relocated_thenable_ptr() {
  let _rt = TestRuntimeGuard::new();

  runtime_native::gc::roots::debug_clear_global_roots_for_tests();
  drain_async_runtime();

  let promise = runtime_native::rt_promise_new_legacy();

  let old_thenable = Box::into_raw(Box::new(ThenableRecordPtr {
    observed: AtomicUsize::new(0),
  }));
  let new_thenable = Box::into_raw(Box::new(ThenableRecordPtr {
    observed: AtomicUsize::new(0),
  }));

  let thenable_ref = ThenableRef {
    vtable: &THENABLE_RECORD_PTR_VTABLE,
    ptr: old_thenable.cast(),
  };

  runtime_native::rt_promise_resolve_thenable_legacy(promise, thenable_ref);
  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 2);

  // Simulate a moving GC updating the thenable pointer in-place.
  assert_eq!(
    replace_global_root_ptr(old_thenable.cast(), new_thenable.cast()),
    1
  );

  drain_async_runtime();
  assert_eq!(
    unsafe { &*new_thenable }.observed.load(Ordering::Acquire) as *mut u8,
    new_thenable.cast()
  );
  assert_eq!(unsafe { &*old_thenable }.observed.load(Ordering::Acquire), 0);

  // The thenable job root should be freed after the microtask runs.
  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 0);

  runtime_native::rt_promise_drop_legacy(promise);

  unsafe {
    drop(Box::from_raw(old_thenable));
    drop(Box::from_raw(new_thenable));
  }
}

#[test]
fn promise_thenable_resolver_uses_relocated_promise_ptr_after_job_runs() {
  let _rt = TestRuntimeGuard::new();

  runtime_native::gc::roots::debug_clear_global_roots_for_tests();
  drain_async_runtime();

  let dst_old = runtime_native::rt_promise_new_legacy();
  let dst_new = runtime_native::rt_promise_new_legacy();

  let thenable = Box::into_raw(Box::new(ThenableStoreCallbacks {
    resolve_cb: AtomicUsize::new(0),
    data: AtomicUsize::new(0),
  }));
  let thenable_ref = ThenableRef {
    vtable: &THENABLE_STORE_CALLBACKS_VTABLE,
    ptr: thenable.cast(),
  };

  runtime_native::rt_promise_resolve_thenable_legacy(dst_old, thenable_ref);
  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 2);

  // Run the thenable job; it should install a resolver root and then return without settling.
  drain_async_runtime();
  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 1);

  assert_eq!(
    replace_global_root_ptr(dst_old.0.cast::<u8>(), dst_new.0.cast::<u8>()),
    1
  );

  let resolve_cb = unsafe { &*thenable }.resolve_cb.load(Ordering::Acquire);
  let data = unsafe { &*thenable }.data.load(Ordering::Acquire);
  assert_ne!(resolve_cb, 0);

  let resolve_cb: runtime_native::abi::ThenableResolveCallback = unsafe { core::mem::transmute(resolve_cb) };
  resolve_cb(data as *mut u8, PromiseResolveInput::value(core::ptr::null_mut()));

  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 0);

  let (state_old, _) = runtime_native::rt_debug_promise_outcome(dst_old);
  assert_eq!(state_old, 0, "old promise should remain pending");

  let (state_new, value_new) = runtime_native::rt_debug_promise_outcome(dst_new);
  assert_eq!(state_new, 1, "relocated promise should be fulfilled");
  assert_eq!(value_new, core::ptr::null_mut());

  runtime_native::rt_promise_drop_legacy(dst_old);
  runtime_native::rt_promise_drop_legacy(dst_new);

  unsafe {
    drop(Box::from_raw(thenable));
  }
}

#[test]
fn promise_thenable_job_uses_relocated_promise_ptr() {
  let _rt = TestRuntimeGuard::new();

  runtime_native::gc::roots::debug_clear_global_roots_for_tests();
  drain_async_runtime();

  let dst_old = runtime_native::rt_promise_new_legacy();
  let dst_new = runtime_native::rt_promise_new_legacy();

  let thenable_ref = ThenableRef {
    vtable: &THENABLE_RESOLVE_VALUE_VTABLE,
    ptr: core::ptr::null_mut(),
  };

  runtime_native::rt_promise_resolve_thenable_legacy(dst_old, thenable_ref);
  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 1);

  assert_eq!(
    replace_global_root_ptr(dst_old.0.cast::<u8>(), dst_new.0.cast::<u8>()),
    1
  );

  drain_async_runtime();

  let (state_old, _) = runtime_native::rt_debug_promise_outcome(dst_old);
  assert_eq!(state_old, 0, "old promise should remain pending");

  let (state_new, value_new) = runtime_native::rt_debug_promise_outcome(dst_new);
  assert_eq!(state_new, 1, "relocated promise should be fulfilled");
  assert_eq!(value_new as usize, 0x1234usize);

  runtime_native::rt_promise_drop_legacy(dst_old);
  runtime_native::rt_promise_drop_legacy(dst_new);

  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 0);
}

#[test]
fn promise_adopt_continuation_uses_relocated_dst_ptr() {
  let _rt = TestRuntimeGuard::new();

  runtime_native::gc::roots::debug_clear_global_roots_for_tests();
  drain_async_runtime();

  let src = runtime_native::rt_promise_new_legacy();
  let dst_old = runtime_native::rt_promise_new_legacy();
  let dst_new = runtime_native::rt_promise_new_legacy();

  runtime_native::rt_promise_resolve_promise_legacy(dst_old, src);
  // Promise adoption stores roots for `dst` and `src` pointers inside the adopt continuation.
  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 2);

  assert_eq!(
    replace_global_root_ptr(dst_old.0.cast::<u8>(), dst_new.0.cast::<u8>()),
    1
  );

  runtime_native::rt_promise_resolve_legacy(src, core::ptr::null_mut());
  drain_async_runtime();

  let (state_old, _) = runtime_native::rt_debug_promise_outcome(dst_old);
  assert_eq!(state_old, 0, "old destination should remain pending");

  let (state_new, value_new) = runtime_native::rt_debug_promise_outcome(dst_new);
  assert_eq!(state_new, 1, "relocated destination should be fulfilled");
  assert_eq!(value_new, core::ptr::null_mut());

  assert_eq!(runtime_native::gc::roots::debug_global_root_count(), 0);

  runtime_native::rt_promise_drop_legacy(src);
  runtime_native::rt_promise_drop_legacy(dst_old);
  runtime_native::rt_promise_drop_legacy(dst_new);
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
