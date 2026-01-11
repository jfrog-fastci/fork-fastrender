use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, ValueRef};
use runtime_native::async_abi::PromiseHeader;
use runtime_native::test_util::{PromiseRejectionEvent, TestRuntimeGuard};

extern "C" fn noop(_data: *mut u8) {}

#[repr(C)]
struct PropagatingAwaitCoroutine {
  header: RtCoroutineHeader,
  awaited: PromiseRef,
}

extern "C" fn propagating_await_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut PropagatingAwaitCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        assert_eq!((*coro).header.await_is_error, 1);
        runtime_native::rt_promise_reject_legacy((*coro).header.promise, (*coro).header.await_error);
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn awaited_rejection_is_not_reported_as_unhandled() {
  let _rt = TestRuntimeGuard::new();

  let p = runtime_native::rt_promise_new_legacy();
  let err = 0xDEAD_BEEFu64 as usize as ValueRef;
  runtime_native::rt_promise_reject_legacy(p, err);

  let mut coro = Box::new(PropagatingAwaitCoroutine {
    header: RtCoroutineHeader {
      resume: propagating_await_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    awaited: p,
  });

  let coro_promise = runtime_native::rt_async_spawn_legacy(&mut coro.header);
  // Avoid spurious unhandled rejections for the coroutine promise; we only care about `p`.
  runtime_native::rt_promise_then_legacy(coro_promise, noop, core::ptr::null_mut());

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    Vec::<PromiseRejectionEvent>::new()
  );
}

#[test]
fn awaiting_after_unhandled_rejection_reports_rejectionhandled() {
  let _rt = TestRuntimeGuard::new();

  let p = runtime_native::rt_promise_new_legacy();
  let err = 0xBAD0_C0DEu64 as usize as ValueRef;
  runtime_native::rt_promise_reject_legacy(p, err);

  // Microtask checkpoint: should report as unhandled.
  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    vec![PromiseRejectionEvent::UnhandledRejection { promise: p }]
  );

  let mut coro = Box::new(PropagatingAwaitCoroutine {
    header: RtCoroutineHeader {
      resume: propagating_await_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    awaited: p,
  });

  let coro_promise = runtime_native::rt_async_spawn_legacy(&mut coro.header);
  runtime_native::rt_promise_then_legacy(coro_promise, noop, core::ptr::null_mut());

  // Next microtask checkpoint: should report `rejectionhandled` for `p`.
  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    vec![PromiseRejectionEvent::RejectionHandled { promise: p }]
  );
}

#[test]
fn native_promise_rejection_is_reported_as_unhandled() {
  let _rt = TestRuntimeGuard::new();

  let mut promise_header = Box::new(PromiseHeader {
    state: core::sync::atomic::AtomicU8::new(0),
    waiters: core::sync::atomic::AtomicUsize::new(0),
    flags: core::sync::atomic::AtomicU8::new(0),
  });
  let p = PromiseRef((&mut *promise_header as *mut PromiseHeader).cast());

  unsafe {
    runtime_native::rt_promise_init(p);
    runtime_native::rt_promise_reject(p);
  }

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    vec![PromiseRejectionEvent::UnhandledRejection { promise: p }]
  );
}

#[test]
fn native_promise_mark_handled_before_checkpoint_suppresses_unhandled() {
  let _rt = TestRuntimeGuard::new();

  let mut promise_header = Box::new(PromiseHeader {
    state: core::sync::atomic::AtomicU8::new(0),
    waiters: core::sync::atomic::AtomicUsize::new(0),
    flags: core::sync::atomic::AtomicU8::new(0),
  });
  let p = PromiseRef((&mut *promise_header as *mut PromiseHeader).cast());

  unsafe {
    runtime_native::rt_promise_init(p);
    runtime_native::rt_promise_reject(p);
    runtime_native::rt_promise_mark_handled(p);
  }

  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    Vec::<PromiseRejectionEvent>::new()
  );
}

#[test]
fn native_promise_mark_handled_after_unhandled_reports_rejectionhandled() {
  let _rt = TestRuntimeGuard::new();

  let mut promise_header = Box::new(PromiseHeader {
    state: core::sync::atomic::AtomicU8::new(0),
    waiters: core::sync::atomic::AtomicUsize::new(0),
    flags: core::sync::atomic::AtomicU8::new(0),
  });
  let p = PromiseRef((&mut *promise_header as *mut PromiseHeader).cast());

  unsafe {
    runtime_native::rt_promise_init(p);
    runtime_native::rt_promise_reject(p);
  }

  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    vec![PromiseRejectionEvent::UnhandledRejection { promise: p }]
  );

  unsafe {
    runtime_native::rt_promise_mark_handled(p);
  }
  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    vec![PromiseRejectionEvent::RejectionHandled { promise: p }]
  );
}
