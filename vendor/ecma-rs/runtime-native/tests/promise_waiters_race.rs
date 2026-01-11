use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, ValueRef};
use runtime_native::test_util::{promise_waiters_is_empty, PromiseWaiterRaceGuard, TestRuntimeGuard};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[repr(C)]
struct TestCoroutine {
  header: RtCoroutineHeader,
  completed: *const AtomicBool,
  awaited: PromiseRef,
}

extern "C" fn test_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  // Safety: `TestCoroutine` is #[repr(C)] and `RtCoroutineHeader` is its first field.
  let coro = coro as *mut TestCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        // The awaited promise settled and the runtime should have stored the result.
        assert_eq!((*coro).header.await_is_error, 0);
        assert_eq!((*coro).header.await_value as usize, 0xDEAD_BEEF);

        let completed = &*(*coro).completed;
        completed.store(true, Ordering::SeqCst);

        // Resolve the coroutine's own promise to mimic JS async function semantics.
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn promise_waiter_race_does_not_lose_wakeup_or_retain_waiters() {
  let _rt = TestRuntimeGuard::new();

  // Deterministically force the "resolve while waiter is registering" interleaving.
  let hook = PromiseWaiterRaceGuard::enable();

  let awaited = runtime_native::rt_promise_new_legacy();
  let completed: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));

  let coro: &'static mut TestCoroutine = Box::leak(Box::new(TestCoroutine {
    header: RtCoroutineHeader {
      resume: test_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    completed,
    awaited,
  }));

  // Avoid relying on `Send` impls for raw pointers/ABI handles in this regression test.
  let coro_ptr = (&mut coro.header as *mut RtCoroutineHeader) as usize;
  let awaited_raw = awaited.0 as usize;

  let spawn_thread = std::thread::spawn(move || {
    let coro_ptr = coro_ptr as *mut RtCoroutineHeader;
    let _promise = runtime_native::rt_async_spawn_legacy(coro_ptr);
  });
  let resolve_thread = std::thread::spawn(move || {
    let awaited = PromiseRef(awaited_raw as *mut core::ffi::c_void);
    runtime_native::rt_promise_resolve_legacy(awaited, 0xDEAD_BEEF as ValueRef);
  });

  spawn_thread.join().unwrap();
  resolve_thread.join().unwrap();

  drop(hook);

  // The promise should not retain stale waiters: the waiter list must be empty even before the
  // coroutine runs its microtask.
  assert!(promise_waiters_is_empty(awaited));

  let deadline = Instant::now() + Duration::from_secs(1);
  while !completed.load(Ordering::SeqCst) {
    assert!(Instant::now() < deadline, "timed out waiting for coroutine to resume");
    runtime_native::rt_async_poll_legacy();
  }

  assert!(promise_waiters_is_empty(awaited));
  assert!(!runtime_native::rt_async_poll_legacy());
}
