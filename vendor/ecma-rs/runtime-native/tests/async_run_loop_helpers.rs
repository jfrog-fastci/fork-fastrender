use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, ValueRef};
use runtime_native::async_abi::PromiseHeader;
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[repr(C)]
struct YieldTwiceCoroutine {
  header: RtCoroutineHeader,
  done: *const AtomicBool,
}

extern "C" fn yield_twice_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut YieldTwiceCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        (*coro).header.state = 1;
        RtCoroStatus::Yield
      }
      1 => {
        (*coro).header.state = 2;
        RtCoroStatus::Yield
      }
      2 => {
        (*( (*coro).done)).store(true, Ordering::SeqCst);
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

extern "C" fn set_bool(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

extern "C" fn noop(_data: *mut u8) {}

#[test]
fn run_until_idle_drains_deferred_coroutines() {
  let _rt = TestRuntimeGuard::new();

  let done = Box::new(AtomicBool::new(false));
  let on_settle = Box::new(AtomicBool::new(false));

  let mut coro = Box::new(YieldTwiceCoroutine {
    header: RtCoroutineHeader {
      resume: yield_twice_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    done: done.as_ref(),
  });

  let promise = runtime_native::rt_async_spawn_legacy(&mut coro.header);
  runtime_native::rt_promise_then_legacy(promise, set_bool, on_settle.as_ref() as *const AtomicBool as *mut u8);

  assert!(!done.load(Ordering::SeqCst));
  assert!(!on_settle.load(Ordering::SeqCst));

  // Safety: ABI call.
  assert!(unsafe { runtime_native::rt_async_run_until_idle_abi() });

  assert!(done.load(Ordering::SeqCst));
  assert!(on_settle.load(Ordering::SeqCst));

  // Safety: ABI call.
  assert!(!unsafe { runtime_native::rt_async_run_until_idle_abi() });
}

#[repr(C)]
struct AwaitCoroutine {
  header: RtCoroutineHeader,
  done: *const AtomicBool,
  awaited: PromiseRef,
}

extern "C" fn await_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut AwaitCoroutine;
  assert!(!coro.is_null());

  unsafe {
      match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        // The awaited promise should have fulfilled.
        assert_eq!((*coro).header.await_is_error, 0);
        assert_eq!((*coro).header.await_value as usize, 0xCAFE_BABE);
        (*( (*coro).done)).store(true, Ordering::SeqCst);
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn block_on_waits_for_promise_settlement() {
  let _rt = TestRuntimeGuard::new();

  let awaited = runtime_native::rt_promise_new_legacy();
  let done = Box::new(AtomicBool::new(false));

  let mut coro = Box::new(AwaitCoroutine {
    header: RtCoroutineHeader {
      resume: await_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    done: done.as_ref(),
    awaited,
  });

  let promise = runtime_native::rt_async_spawn_legacy(&mut coro.header);

  let t = std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(20));
    runtime_native::rt_promise_resolve_legacy(awaited, 0xCAFE_BABE as ValueRef);
  });

  let start = Instant::now();
  // Safety: ABI call.
  unsafe {
    runtime_native::rt_async_block_on(promise);
  }
  let elapsed = start.elapsed();

  // Should have waited for the resolver thread to run.
  assert!(
    elapsed >= Duration::from_millis(5),
    "block_on returned too quickly (elapsed={elapsed:?})"
  );
  assert!(done.load(Ordering::SeqCst));
  t.join().unwrap();
}

#[test]
fn block_on_returns_immediately_when_promise_already_settled() {
  let _rt = TestRuntimeGuard::new();

  // Warm up the runtime so this test doesn't include one-time initialization
  // (thread pool startup, etc) in the timing assertion.
  //
  // Safety: ABI call.
  unsafe {
    let _ = runtime_native::rt_async_run_until_idle_abi();
  }

  let p = runtime_native::rt_promise_new_legacy();
  runtime_native::rt_promise_resolve_legacy(p, core::ptr::null_mut());

  // If `rt_async_block_on` mistakenly calls `rt_async_wait` even though the
  // promise is already settled, it would block indefinitely unless something
  // wakes the event loop. Use a watchdog wake to keep this test bounded.
  let (tx, rx) = mpsc::channel::<()>();
  let t = std::thread::spawn(move || {
    if rx.recv_timeout(Duration::from_millis(250)).is_err() {
      runtime_native::rt_queue_microtask(noop, core::ptr::null_mut());
    }
  });

  let start = Instant::now();
  // Safety: ABI call.
  unsafe {
    runtime_native::rt_async_block_on(p);
  }
  let elapsed = start.elapsed();

  let _ = tx.send(());
  t.join().unwrap();

  assert!(
    elapsed < Duration::from_millis(100),
    "block_on should return immediately when promise is settled (elapsed={elapsed:?})"
  );
}

#[test]
fn block_on_wakes_on_native_promise_settlement_without_payload() {
  let _rt = TestRuntimeGuard::new();

  // Allocate a promise that is *only* a `PromiseHeader` (no extra payload). This
  // models the native async ABI contract: promise payload begins immediately
  // after the header and may be empty.
  let header = Box::new(PromiseHeader {
    state: AtomicU8::new(PromiseHeader::PENDING),
    waiters: AtomicUsize::new(0),
    flags: AtomicU8::new(0),
  });
  let p = PromiseRef(Box::into_raw(header).cast());

  // Initialize to a clean pending state.
  unsafe {
    runtime_native::rt_promise_init(p);
  }

  // Fulfill from another thread after a short delay.
  let fulfiller = std::thread::spawn(move || unsafe {
    std::thread::sleep(Duration::from_millis(20));
    runtime_native::rt_promise_fulfill(p);
  });

  // Watchdog: if `rt_async_block_on` fails to wake on settlement, it will sleep
  // until some other event wakes the runtime. Use a bounded wake to prevent a
  // hung test; also assert it returns *before* this fires.
  let (tx, rx) = mpsc::channel::<()>();
  let watchdog = std::thread::spawn(move || {
    if rx.recv_timeout(Duration::from_millis(200)).is_err() {
      runtime_native::rt_queue_microtask(noop, core::ptr::null_mut());
    }
  });

  let start = Instant::now();
  unsafe {
    runtime_native::rt_async_block_on(p);
  }
  let elapsed = start.elapsed();

  let _ = tx.send(());
  watchdog.join().unwrap();
  fulfiller.join().unwrap();

  assert!(
    elapsed >= Duration::from_millis(5),
    "block_on returned too quickly (elapsed={elapsed:?})"
  );
  assert!(
    elapsed < Duration::from_millis(150),
    "block_on did not wake promptly on promise settlement (elapsed={elapsed:?})"
  );

  // Safety: the promise is settled and `rt_async_block_on` has drained its reaction jobs.
  unsafe {
    drop(Box::from_raw(p.0.cast::<PromiseHeader>()));
  }
}

#[test]
fn block_on_returns_when_executor_is_in_error_state() {
  let _rt = TestRuntimeGuard::new();

  // Warm up the runtime so this test doesn't include one-time initialization in timing assertions.
  unsafe {
    let _ = runtime_native::rt_async_run_until_idle_abi();
  }

  // Put the async runtime into a known error state: restrict the ready queue to 1 entry, then
  // attempt to enqueue a second microtask.
  runtime_native::rt_async_set_limits(1, 1);
  runtime_native::rt_queue_microtask(noop, core::ptr::null_mut());
  runtime_native::rt_queue_microtask(noop, core::ptr::null_mut());

  // `rt_async_run_until_idle` should observe the error and return without spinning/aborting.
  unsafe {
    assert!(!runtime_native::rt_async_run_until_idle_abi());
  }

  // Allocate a promise that is only a header. `rt_async_block_on` should return immediately when
  // the executor has an error, rather than spinning or blocking forever.
  let header = Box::new(PromiseHeader {
    state: AtomicU8::new(PromiseHeader::PENDING),
    reactions: AtomicUsize::new(0),
    flags: AtomicU8::new(0),
  });
  let p = PromiseRef(Box::into_raw(header).cast());
  unsafe {
    runtime_native::rt_promise_init(p);
  }

  let start = Instant::now();
  unsafe {
    runtime_native::rt_async_block_on(p);
  }
  let elapsed = start.elapsed();
  assert!(
    elapsed < Duration::from_millis(100),
    "block_on should return promptly when async runtime has an error (elapsed={elapsed:?})"
  );

  // Settle so any registered waiter nodes are drained (even though the runtime is in an error
  // state).
  unsafe {
    runtime_native::rt_promise_fulfill(p);
  }

  let err = runtime_native::rt_async_take_last_error();
  assert!(
    !err.is_null(),
    "expected async runtime error after overflowing ready queue"
  );
  unsafe {
    runtime_native::rt_async_free_c_string(err);
  }

  unsafe {
    drop(Box::from_raw(p.0.cast::<PromiseHeader>()));
  }
}
