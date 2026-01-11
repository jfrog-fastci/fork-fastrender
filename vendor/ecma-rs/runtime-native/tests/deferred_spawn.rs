use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, ValueRef};
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::{AtomicUsize, Ordering};

#[repr(C)]
struct CounterCoro {
  header: RtCoroutineHeader,
  counter: *const AtomicUsize,
}

extern "C" fn counter_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  // Safety: CounterCoro is #[repr(C)] and RtCoroutineHeader is its first field.
  let coro = coro as *mut CounterCoro;
  assert!(!coro.is_null());
  unsafe {
    (&*(*coro).counter).fetch_add(1, Ordering::SeqCst);
    runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
  }
  RtCoroStatus::Done
}

#[test]
fn spawn_vs_deferred_spawn_immediacy() {
  let _rt = TestRuntimeGuard::new();

  // `rt_async_spawn_legacy` resumes the coroutine during the call.
  let counter = AtomicUsize::new(0);
  let mut coro = Box::new(CounterCoro {
    header: RtCoroutineHeader {
      resume: counter_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    counter: &counter,
  });

  let promise = runtime_native::rt_async_spawn_legacy(&mut coro.header);
  assert_eq!(counter.load(Ordering::SeqCst), 1);
  assert_eq!(promise, coro.header.promise);

  // `rt_async_spawn_deferred_legacy` only enqueues; no resume until `rt_async_poll_legacy`.
  let counter = AtomicUsize::new(0);
  let mut coro = Box::new(CounterCoro {
    header: RtCoroutineHeader {
      resume: counter_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    counter: &counter,
  });

  let promise = runtime_native::rt_async_spawn_deferred_legacy(&mut coro.header);
  assert_eq!(counter.load(Ordering::SeqCst), 0);
  assert_eq!(promise, coro.header.promise);

  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[repr(C)]
struct YieldOnceCoro {
  header: RtCoroutineHeader,
  started: *mut bool,
  completed: *mut bool,
  awaited: PromiseRef,
}

extern "C" fn yield_once_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut YieldOnceCoro;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        *(*coro).started = true;
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        assert_eq!((*coro).header.await_is_error, 0);
        assert_eq!((*coro).header.await_value as usize, 0xCAFE_BABE);

        *(*coro).completed = true;
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn deferred_spawn_registers_waiter_when_polled() {
  let _rt = TestRuntimeGuard::new();

  let awaited = runtime_native::rt_promise_new_legacy();
  let mut started = false;
  let mut completed = false;
  let mut coro = Box::new(YieldOnceCoro {
    header: RtCoroutineHeader {
      resume: yield_once_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    started: &mut started,
    completed: &mut completed,
    awaited,
  });

  let promise = runtime_native::rt_async_spawn_deferred_legacy(&mut coro.header);
  assert_eq!(promise, coro.header.promise);
  assert!(!started);
  assert!(!completed);

  // First poll: coroutine runs and awaits `awaited`, registering a continuation.
  while runtime_native::rt_async_poll_legacy() {}
  assert!(started);
  assert!(!completed);

  // Settling the awaited promise should enqueue a microtask (not resume immediately).
  runtime_native::rt_promise_resolve_legacy(awaited, 0xCAFE_BABEusize as ValueRef);
  assert!(!completed);

  while runtime_native::rt_async_poll_legacy() {}
  assert!(completed);
}
