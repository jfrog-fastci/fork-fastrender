use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, ValueRef};
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

#[repr(C)]
struct TestCoroutine {
  header: RtCoroutineHeader,
  side_effect: *mut bool,
  completed: *mut bool,
  awaited: PromiseRef,
}

extern "C" fn test_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  // Safety: TestCoroutine is #[repr(C)] and RtCoroutineHeader is its first field.
  let coro = coro as *mut TestCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        *(*coro).side_effect = true;
        runtime_native::rt_coro_await(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        // The awaited promise settled and the runtime should have stored the result.
        assert_eq!((*coro).header.await_is_error, 0);
        assert_eq!((*coro).header.await_value as usize, 0xCAFE_BABE);

        *(*coro).completed = true;
        runtime_native::rt_promise_resolve((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn coroutine_spawn_runs_sync_until_first_await_and_resumes_as_microtask() {
  let _rt = TestRuntimeGuard::new();
  let awaited = runtime_native::rt_promise_new();
  let mut side_effect = false;
  let mut completed = false;

  let mut coro = Box::new(TestCoroutine {
    header: RtCoroutineHeader {
      resume: test_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    side_effect: &mut side_effect,
    completed: &mut completed,
    awaited,
  });

  let promise = runtime_native::rt_async_spawn(&mut coro.header);

  // JS semantics: the coroutine runs immediately until its first `await`.
  assert!(side_effect);
  assert!(!completed);
  assert_eq!(promise, coro.header.promise);

  // Settling the awaited promise should enqueue a microtask, not resume immediately.
  runtime_native::rt_promise_resolve(awaited, 0xCAFE_BABE as ValueRef);
  assert!(!completed);

  while runtime_native::rt_async_poll() {}

  assert!(completed);
}

#[repr(C)]
struct OrderCoroutine {
  header: RtCoroutineHeader,
  id: u32,
  log: *const Mutex<Vec<u32>>,
  awaited: PromiseRef,
}

extern "C" fn order_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut OrderCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        let log = &*(*coro).log;
        log.lock().unwrap().push((*coro).id);
        runtime_native::rt_promise_resolve((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn promise_waiters_resume_in_fifo_order() {
  let _rt = TestRuntimeGuard::new();

  let awaited = runtime_native::rt_promise_new();
  let log: &'static Mutex<Vec<u32>> = Box::leak(Box::new(Mutex::new(Vec::new())));

  let mut coro1 = Box::new(OrderCoroutine {
    header: RtCoroutineHeader {
      resume: order_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    id: 1,
    log,
    awaited,
  });
  let mut coro2 = Box::new(OrderCoroutine {
    header: RtCoroutineHeader {
      resume: order_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    id: 2,
    log,
    awaited,
  });

  runtime_native::rt_async_spawn(&mut coro1.header);
  runtime_native::rt_async_spawn(&mut coro2.header);

  runtime_native::rt_promise_resolve(awaited, 0x1234usize as ValueRef);
  while runtime_native::rt_async_poll() {}

  assert_eq!(&*log.lock().unwrap(), &[1, 2]);
}

#[repr(C)]
struct SettledAwaitCoroutine {
  header: RtCoroutineHeader,
  completed: *mut bool,
  awaited: PromiseRef,
}

extern "C" fn settled_await_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut SettledAwaitCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        *(*coro).completed = true;
        runtime_native::rt_promise_resolve((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn strict_mode_awaiting_settled_promise_yields_to_microtask() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::set_strict_await_yields(true);

  let awaited = runtime_native::rt_promise_new();
  runtime_native::rt_promise_resolve(awaited, 0xBEEFusize as ValueRef);

  let mut completed = false;
  let mut coro = Box::new(SettledAwaitCoroutine {
    header: RtCoroutineHeader {
      resume: settled_await_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    completed: &mut completed,
    awaited,
  });

  runtime_native::rt_async_spawn(&mut coro.header);
  assert!(!completed, "strict await should not resume synchronously inside rt_async_spawn");

  runtime_native::rt_async_poll();
  assert!(completed);
  assert!(!runtime_native::rt_async_poll());
}

#[test]
fn non_strict_mode_awaiting_settled_promise_resumes_synchronously() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::set_strict_await_yields(false);

  let awaited = runtime_native::rt_promise_new();
  runtime_native::rt_promise_resolve(awaited, 0xBEEFusize as ValueRef);

  let mut completed = false;
  let mut coro = Box::new(SettledAwaitCoroutine {
    header: RtCoroutineHeader {
      resume: settled_await_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    completed: &mut completed,
    awaited,
  });

  runtime_native::rt_async_spawn(&mut coro.header);
  assert!(completed, "non-strict await should resume synchronously inside rt_async_spawn");
  assert!(!runtime_native::rt_async_poll());
}

// -----------------------------------------------------------------------------
// spawn_blocking integration
// -----------------------------------------------------------------------------

extern "C" fn blocking_resolve_value(_data: *mut u8, promise: PromiseRef) {
  std::thread::sleep(Duration::from_millis(20));
  runtime_native::rt_promise_resolve(promise, 0xCAFE_BABEusize as ValueRef);
}

extern "C" fn blocking_reject_value(_data: *mut u8, promise: PromiseRef) {
  std::thread::sleep(Duration::from_millis(20));
  runtime_native::rt_promise_reject(promise, 0xDEAD_BEEFusize as ValueRef);
}

#[repr(C)]
struct SpawnBlockingCoroutine {
  header: RtCoroutineHeader,
  completed: *mut bool,
  awaited: PromiseRef,
}

extern "C" fn spawn_blocking_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut SpawnBlockingCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        (*coro).awaited = runtime_native::rt_spawn_blocking(blocking_resolve_value, core::ptr::null_mut());
        runtime_native::rt_coro_await(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        assert_eq!((*coro).header.await_is_error, 0);
        assert_eq!((*coro).header.await_value as usize, 0xCAFE_BABE);
        *(*coro).completed = true;
        runtime_native::rt_promise_resolve((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn coroutine_can_await_spawn_blocking_promise() {
  let _rt = TestRuntimeGuard::new();

  let mut completed = false;
  let mut coro = Box::new(SpawnBlockingCoroutine {
    header: RtCoroutineHeader {
      resume: spawn_blocking_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    completed: &mut completed,
    awaited: PromiseRef::null(),
  });

  runtime_native::rt_async_spawn(&mut coro.header);
  assert!(
    !completed,
    "spawn_blocking should not resume synchronously inside rt_async_spawn when the promise is pending"
  );

  let start = Instant::now();
  while !completed {
    runtime_native::rt_async_poll();
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for spawn_blocking promise to resume coroutine"
    );
    std::thread::yield_now();
  }
}

#[repr(C)]
struct SpawnBlockingRejectCoroutine {
  header: RtCoroutineHeader,
  completed: *mut bool,
  awaited: PromiseRef,
}

extern "C" fn spawn_blocking_reject_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut SpawnBlockingRejectCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        (*coro).awaited = runtime_native::rt_spawn_blocking(blocking_reject_value, core::ptr::null_mut());
        runtime_native::rt_coro_await(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        assert_eq!((*coro).header.await_is_error, 1);
        assert_eq!((*coro).header.await_error as usize, 0xDEAD_BEEF);
        *(*coro).completed = true;
        runtime_native::rt_promise_resolve((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn coroutine_can_await_spawn_blocking_rejection() {
  let _rt = TestRuntimeGuard::new();

  let mut completed = false;
  let mut coro = Box::new(SpawnBlockingRejectCoroutine {
    header: RtCoroutineHeader {
      resume: spawn_blocking_reject_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    completed: &mut completed,
    awaited: PromiseRef::null(),
  });

  runtime_native::rt_async_spawn(&mut coro.header);
  assert!(!completed);

  let start = Instant::now();
  while !completed {
    runtime_native::rt_async_poll();
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for spawn_blocking rejection to resume coroutine"
    );
    std::thread::yield_now();
  }
}
