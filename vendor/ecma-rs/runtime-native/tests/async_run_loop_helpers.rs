use runtime_native::abi::{Microtask, PromiseRef, RtCoroStatus, RtCoroutineHeader, RtShapeDescriptor, RtShapeId, ValueRef};
use runtime_native::async_abi::PromiseHeader;
use runtime_native::gc::ObjHeader;
use runtime_native::shape_table;
use runtime_native::test_util::{new_promise_header_pending, TestRuntimeGuard};
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Once;
use std::time::{Duration, Instant};

#[repr(C)]
struct GcBox<T> {
  header: ObjHeader,
  payload: T,
}

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    static SHAPES: [RtShapeDescriptor; 2] = [
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<YieldTwiceCoroutine>>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<AwaitCoroutine>>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
    ];
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

unsafe fn alloc_pinned<T>(shape: RtShapeId) -> *mut GcBox<T> {
  ensure_shape_table();
  runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<T>>(), shape).cast::<GcBox<T>>()
}

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

  let coro_obj = unsafe { alloc_pinned::<YieldTwiceCoroutine>(RtShapeId(1)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: yield_twice_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.done = done.as_ref();

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

  let coro_obj = unsafe { alloc_pinned::<AwaitCoroutine>(RtShapeId(2)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: await_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.done = done.as_ref();
  coro.awaited = awaited;

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
  let watchdog_fired = Arc::new(AtomicBool::new(false));
  let watchdog_fired2 = watchdog_fired.clone();
  let (tx, rx) = mpsc::channel::<()>();
  let t = std::thread::spawn(move || {
    if rx.recv_timeout(Duration::from_secs(1)).is_err() {
      watchdog_fired2.store(true, Ordering::SeqCst);
      unsafe {
        runtime_native::rt_queue_microtask(Microtask {
          func: noop,
          data: core::ptr::null_mut(),
          drop: None,
        });
      }
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
    !watchdog_fired.load(Ordering::SeqCst),
    "block_on appeared to wait even though the promise was already settled (elapsed={elapsed:?})"
  );
}

#[test]
fn block_on_wakes_on_native_promise_settlement_without_payload() {
  let _rt = TestRuntimeGuard::new();

  // Warm up the runtime so this test doesn't include one-time initialization in the wakeup timing.
  //
  // Safety: ABI call.
  unsafe {
    let _ = runtime_native::rt_async_run_until_idle_abi();
  }
  let this_thread_id = runtime_native::threading::registry::current_thread_id()
    .expect("rt_async_run_until_idle_abi should register the current thread");

  // Allocate a promise that is *only* a `PromiseHeader` (no extra payload). This
  // models the native async ABI contract: promise payload begins immediately
  // after the header and may be empty.
  let header = Box::new(new_promise_header_pending());
  let p = PromiseRef(Box::into_raw(header).cast());

  // Initialize to a clean pending state.
  unsafe {
    runtime_native::rt_promise_init(p);
  }

  // Fulfill from another thread once the event-loop thread is parked inside the runtime.
  let (fulfill_tx, fulfill_rx) = mpsc::channel::<(Instant, bool)>();
  let fulfiller = std::thread::spawn(move || unsafe {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut saw_parked = false;
    while Instant::now() < deadline {
      if runtime_native::threading::all_threads()
        .iter()
        .any(|t| t.id() == this_thread_id && t.is_parked())
      {
        saw_parked = true;
        break;
      }
      std::thread::yield_now();
    }

    let fulfilled_at = Instant::now();
    runtime_native::rt_promise_fulfill(p);
    let _ = fulfill_tx.send((fulfilled_at, saw_parked));
  });

  // Watchdog: if `rt_async_block_on` fails to wake on settlement, it will sleep
  // until some other event wakes the runtime. Use a bounded wake to prevent a hung test.
  let watchdog_fired = Arc::new(AtomicBool::new(false));
  let watchdog_fired2 = watchdog_fired.clone();
  let (tx, rx) = mpsc::channel::<()>();
  let watchdog = std::thread::spawn(move || {
    if rx.recv_timeout(Duration::from_secs(2)).is_err() {
      watchdog_fired2.store(true, Ordering::SeqCst);
      unsafe {
        runtime_native::rt_queue_microtask(Microtask {
          func: noop,
          data: core::ptr::null_mut(),
          drop: None,
        });
      }
    }
  });

  let start = Instant::now();
  unsafe {
    runtime_native::rt_async_block_on(p);
  }
  let end = Instant::now();
  let elapsed = end.duration_since(start);

  let _ = tx.send(());
  watchdog.join().unwrap();
  fulfiller.join().unwrap();

  let (fulfilled_at, saw_parked) = fulfill_rx
    .recv_timeout(Duration::from_secs(1))
    .expect("fulfiller thread did not report fulfillment time");

  assert!(
    saw_parked,
    "fulfiller never observed the event-loop thread parked inside the runtime (elapsed={elapsed:?})"
  );
  assert!(
    !watchdog_fired.load(Ordering::SeqCst),
    "block_on did not wake on promise settlement without external payload (elapsed={elapsed:?})"
  );

  let after_fulfill = end.duration_since(fulfilled_at);
  assert!(
    after_fulfill < Duration::from_millis(250),
    "block_on returned too long after promise fulfillment (after_fulfill={after_fulfill:?}, elapsed={elapsed:?})"
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
  unsafe {
    runtime_native::rt_queue_microtask(Microtask {
      func: noop,
      data: core::ptr::null_mut(),
      drop: None,
    });
    runtime_native::rt_queue_microtask(Microtask {
      func: noop,
      data: core::ptr::null_mut(),
      drop: None,
    });
  }

  // `rt_async_run_until_idle` should observe the error and return without spinning/aborting.
  unsafe {
    assert!(!runtime_native::rt_async_run_until_idle_abi());
  }

  // Allocate a promise that is only a header. `rt_async_block_on` should return immediately when
  // the executor has an error, rather than spinning or blocking forever.
  let header = Box::new(new_promise_header_pending());
  let p = PromiseRef(Box::into_raw(header).cast());
  unsafe {
    runtime_native::rt_promise_init(p);
  }

  // If `rt_async_block_on` mistakenly parks even though the executor is already in an error state,
  // it could block indefinitely. Use a watchdog wake to keep the test bounded.
  let watchdog_fired = Arc::new(AtomicBool::new(false));
  let watchdog_fired2 = watchdog_fired.clone();
  let (tx, rx) = mpsc::channel::<()>();
  let watchdog = std::thread::spawn(move || {
    if rx.recv_timeout(Duration::from_secs(2)).is_err() {
      watchdog_fired2.store(true, Ordering::SeqCst);
      unsafe {
        runtime_native::rt_queue_microtask(Microtask {
          func: noop,
          data: core::ptr::null_mut(),
          drop: None,
        });
      }
    }
  });

  let start = Instant::now();
  unsafe {
    runtime_native::rt_async_block_on(p);
  }
  let elapsed = start.elapsed();

  let _ = tx.send(());
  watchdog.join().unwrap();

  assert!(
    !watchdog_fired.load(Ordering::SeqCst),
    "block_on appeared to wait even though the executor is in an error state (elapsed={elapsed:?})"
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

  // Teardown: `rt_async_block_on` may have registered waiter nodes and/or enqueued reaction jobs
  // before the executor entered an error state. Ensure all pending work is discarded before freeing
  // this test-owned promise allocation.
  runtime_native::rt_async_cancel_all();

  unsafe {
    drop(Box::from_raw(p.0.cast::<PromiseHeader>()));
  }
}
