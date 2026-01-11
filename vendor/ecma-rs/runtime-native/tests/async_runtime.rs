use core::mem::{align_of, size_of, MaybeUninit};
use core::ptr::null_mut;
use core::sync::atomic::Ordering;

use runtime_native::async_abi::{
  Coroutine, CoroutineRef, CoroutineStep, CoroutineVTable, PromiseHeader, PromiseRef as PromisePtr,
  RT_ASYNC_ABI_VERSION, CORO_FLAG_RUNTIME_OWNS_FRAME,
};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{CoroutineId, PromiseRef as PromiseHandle, RtShapeId};

#[repr(C)]
struct TestPromise {
  header: PromiseHeader,
  value: u32,
}

#[repr(C)]
struct AwaitThenReturnCoro {
  header: Coroutine,
  state: u8,
  awaited: PromisePtr,
  return_value: u32,
}

unsafe extern "C" fn await_then_return_resume(coro: *mut Coroutine) -> CoroutineStep {
  // Safety: generated code guarantees `Coroutine` is the first field; the tests build frames with
  // the same layout.
  let coro = coro.cast::<AwaitThenReturnCoro>();

  // Safety: `coro` is a valid pointer to an `AwaitThenReturnCoro`.
  unsafe {
    match (*coro).state {
      0 => {
        (*coro).state = 1;
        CoroutineStep::await_((*coro).awaited)
      }
      1 => {
        let promise = (*coro).header.promise;
        assert!(!promise.is_null(), "runtime must set coro.promise before resuming");

        // Write payload, then fulfill.
        let out = promise.cast::<TestPromise>();
        (*out).value = (*coro).return_value;
        runtime_native::rt_promise_fulfill(promise_handle_from_ptr(promise));

        (*coro).state = 2;
        CoroutineStep::complete()
      }
      _ => CoroutineStep::complete(),
    }
  }
}

unsafe extern "C" fn await_then_return_destroy(coro: CoroutineRef) {
  // Safety: the tests allocate coroutine frames with `Box::into_raw` and mark them as
  // `CORO_FLAG_RUNTIME_OWNS_FRAME`, so the runtime owns and destroys them exactly once.
  unsafe { drop(Box::from_raw(coro as *mut AwaitThenReturnCoro)) };
}

static VTABLE: CoroutineVTable = CoroutineVTable {
  resume: await_then_return_resume,
  destroy: await_then_return_destroy,
  promise_size: size_of::<TestPromise>() as u32,
  promise_align: align_of::<TestPromise>() as u32,
  promise_shape_id: RtShapeId::INVALID,
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

fn promise_header_ptr(p: PromiseHandle) -> PromisePtr {
  p.0.cast::<PromiseHeader>()
}

fn promise_handle_from_ptr(p: PromisePtr) -> PromiseHandle {
  PromiseHandle(p as *mut _)
}

fn promise_state(p: PromisePtr) -> u8 {
  assert!(!p.is_null());
  // Safety: `p` points to a valid promise allocation whose prefix is `PromiseHeader`.
  unsafe { (*p).state.load(Ordering::Acquire) }
}

unsafe fn alloc_pending_test_promise() -> *mut TestPromise {
  let raw = Box::into_raw(Box::new(MaybeUninit::<TestPromise>::uninit()));
  let p = raw.cast::<TestPromise>();

  // Safety: `p` points to writable memory for at least a `PromiseHeader` prefix.
  runtime_native::rt_promise_init(promise_handle_from_ptr(p.cast::<PromiseHeader>()));
  // Initialize the payload field so the allocation is a fully-initialized `TestPromise`.
  core::ptr::addr_of_mut!((*p).value).write(0);

  p
}

#[test]
fn async_spawn_then_wake_and_complete() {
  let _rt = TestRuntimeGuard::new();

  let awaited = unsafe { alloc_pending_test_promise() };
  // Ensure `value` is initialized before we ever treat `awaited` as a `TestPromise`.
  unsafe {
    (*awaited).value = 0;
  }
  let awaited_ref: PromisePtr = awaited.cast::<PromiseHeader>();

  let coro = Box::into_raw(Box::new(AwaitThenReturnCoro {
    header: Coroutine {
      vtable: &VTABLE,
      promise: null_mut(),
      next_waiter: null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    state: 0,
    awaited: awaited_ref,
    return_value: 42,
  }));

  let handle = runtime_native::rt_handle_alloc(coro.cast::<u8>());
  let result_promise = unsafe { runtime_native::rt_async_spawn(CoroutineId(handle)) };
  let result_hdr = promise_header_ptr(result_promise);
  assert_eq!(promise_state(result_hdr), PromiseHeader::PENDING);

  // No microtasks should be runnable until the awaited promise is fulfilled.
  assert!(!runtime_native::rt_async_poll());

  // Fulfill the awaited promise: should enqueue a coroutine resume microtask.
  unsafe {
    (*awaited).value = 1;
    runtime_native::rt_promise_fulfill(promise_handle_from_ptr(awaited_ref));
  }

  assert!(runtime_native::rt_async_poll());
  assert_eq!(promise_state(result_hdr), PromiseHeader::FULFILLED);
  assert_eq!(unsafe { (*result_hdr.cast::<TestPromise>()).value }, 42);
  assert!(
    runtime_native::rt_handle_load(handle).is_null(),
    "runtime must free the CoroutineId handle when the coroutine completes"
  );

  // Clean up allocations owned by this test.
  unsafe {
    drop(Box::from_raw(awaited));
  }
}

#[test]
fn multi_waiter_wakes_all() {
  let _rt = TestRuntimeGuard::new();

  let awaited = unsafe { alloc_pending_test_promise() };
  unsafe {
    (*awaited).value = 0;
  }
  let awaited_ref: PromisePtr = awaited.cast::<PromiseHeader>();

  let mk_coro = |return_value: u32| {
    Box::into_raw(Box::new(AwaitThenReturnCoro {
      header: Coroutine {
        vtable: &VTABLE,
        promise: null_mut(),
        next_waiter: null_mut(),
        flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
      },
      state: 0,
      awaited: awaited_ref,
      return_value,
    }))
  };

  let c1 = mk_coro(1);
  let c2 = mk_coro(2);

  let handle1 = runtime_native::rt_handle_alloc(c1.cast::<u8>());
  let handle2 = runtime_native::rt_handle_alloc(c2.cast::<u8>());
  let p1 = unsafe { runtime_native::rt_async_spawn(CoroutineId(handle1)) };
  let p2 = unsafe { runtime_native::rt_async_spawn(CoroutineId(handle2)) };
  let p1_hdr = promise_header_ptr(p1);
  let p2_hdr = promise_header_ptr(p2);

  assert_eq!(promise_state(p1_hdr), PromiseHeader::PENDING);
  assert_eq!(promise_state(p2_hdr), PromiseHeader::PENDING);

  unsafe {
    runtime_native::rt_promise_fulfill(promise_handle_from_ptr(awaited_ref));
  }

  while runtime_native::rt_async_poll() {}

  assert_eq!(promise_state(p1_hdr), PromiseHeader::FULFILLED);
  assert_eq!(unsafe { (*p1_hdr.cast::<TestPromise>()).value }, 1);

  assert_eq!(promise_state(p2_hdr), PromiseHeader::FULFILLED);
  assert_eq!(unsafe { (*p2_hdr.cast::<TestPromise>()).value }, 2);
  assert!(runtime_native::rt_handle_load(handle1).is_null());
  assert!(runtime_native::rt_handle_load(handle2).is_null());

  unsafe {
    drop(Box::from_raw(awaited));
  }
}

#[test]
fn fast_path_already_fulfilled_promise_completes_in_spawn() {
  let _rt = TestRuntimeGuard::new();

  let awaited = unsafe { alloc_pending_test_promise() };
  unsafe {
    (*awaited).value = 123;
  }
  let awaited_ref: PromisePtr = awaited.cast::<PromiseHeader>();

  // Settle awaited promise before spawning.
  unsafe {
    runtime_native::rt_promise_fulfill(promise_handle_from_ptr(awaited_ref));
  }

  let coro = Box::into_raw(Box::new(AwaitThenReturnCoro {
    header: Coroutine {
      vtable: &VTABLE,
      promise: null_mut(),
      next_waiter: null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    state: 0,
    awaited: awaited_ref,
    return_value: 7,
  }));

  let handle = runtime_native::rt_handle_alloc(coro.cast::<u8>());
  let result_promise = unsafe { runtime_native::rt_async_spawn(CoroutineId(handle)) };
  let result_hdr = promise_header_ptr(result_promise);
  assert_eq!(promise_state(result_hdr), PromiseHeader::FULFILLED);
  assert_eq!(unsafe { (*result_hdr.cast::<TestPromise>()).value }, 7);
  assert!(runtime_native::rt_handle_load(handle).is_null());

  // `await` on an already-settled promise should not require an external poll in the default
  // non-strict mode.
  assert!(!runtime_native::rt_async_poll());

  unsafe {
    drop(Box::from_raw(awaited));
  }
}
