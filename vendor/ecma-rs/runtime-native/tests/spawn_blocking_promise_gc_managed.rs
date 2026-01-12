use runtime_native::abi::PromiseRef;
use runtime_native::async_abi::PromiseHeader;
use runtime_native::roots::Root;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::PromiseLayout;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[repr(C)]
struct TaskCtx {
  started: AtomicBool,
  finished: AtomicBool,
}

extern "C" fn write_u64_sleep(data: *mut u8, out_payload: *mut u8) -> u8 {
  let ctx = unsafe { &*(data as *const TaskCtx) };
  ctx.started.store(true, Ordering::Release);
  unsafe {
    *(out_payload as *mut u64) = 0x1122_3344_5566_7788u64;
  }
  std::thread::sleep(Duration::from_millis(200));
  ctx.finished.store(true, Ordering::Release);
  0 // fulfill
}

#[repr(C)]
struct RejectCtx {
  done: AtomicBool,
}

extern "C" fn write_u32_and_reject(data: *mut u8, out_payload: *mut u8) -> u8 {
  let ctx = unsafe { &*(data as *const RejectCtx) };
  unsafe {
    *(out_payload as *mut u32) = 123;
  }
  ctx.done.store(true, Ordering::Release);
  1 // reject
}

static ROOTED_DATA_STARTED: AtomicBool = AtomicBool::new(false);
static ROOTED_DATA_RELEASE: AtomicBool = AtomicBool::new(false);
static ROOTED_DATA_FINISHED: AtomicBool = AtomicBool::new(false);

extern "C" fn rooted_data_task_block(_data: *mut u8, _out_payload: *mut u8) -> u8 {
  ROOTED_DATA_STARTED.store(true, Ordering::Release);
  while !ROOTED_DATA_RELEASE.load(Ordering::Acquire) {
    std::thread::sleep(Duration::from_millis(1));
  }
  ROOTED_DATA_FINISHED.store(true, Ordering::Release);
  0 // fulfill
}

#[test]
fn spawn_blocking_promise_is_gc_managed_and_gc_safe() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  // Stop-the-world handshakes can take much longer in debug builds (especially
  // under parallel test execution on multi-agent hosts). Keep release builds
  // strict, but give debug builds enough slack to avoid flaky timeouts.
  const TIMEOUT: Duration = if cfg!(debug_assertions) {
    Duration::from_secs(30)
  } else {
    Duration::from_secs(2)
  };

  let ctx = Box::new(TaskCtx {
    started: AtomicBool::new(false),
    finished: AtomicBool::new(false),
  });
  let ctx_ptr = Box::into_raw(ctx);

  let mut promise =
    runtime_native::rt_spawn_blocking_promise(write_u64_sleep, ctx_ptr.cast::<u8>(), PromiseLayout::of::<u64>());

  // Root the promise across `rt_gc_collect` calls: Rust code has no stackmaps, so we must ensure the
  // promise pointer is updated if the GC moves it.
  let promise_root = Root::<PromiseHeader>::new(promise.0.cast::<PromiseHeader>());

  // Wait for the blocking task to start and enter the sleep.
  let deadline = Instant::now() + TIMEOUT;
  while !unsafe { &*ctx_ptr }.started.load(Ordering::Acquire) {
    assert!(Instant::now() < deadline, "timeout waiting for spawn_blocking_promise task to start");
    std::thread::yield_now();
  }

  // GC-deadlock regression: `rt_gc_collect` must complete even while a blocking worker thread is
  // blocked in a syscall (sleep).
  let (gc_done_tx, gc_done_rx) = mpsc::channel::<()>();
  std::thread::spawn(move || {
    threading::register_current_thread(ThreadKind::Worker);
    runtime_native::rt_gc_collect();
    gc_done_tx.send(()).unwrap();
    threading::unregister_current_thread();
  });

  let deadline = Instant::now() + TIMEOUT;
  loop {
    if gc_done_rx.try_recv().is_ok() {
      break;
    }
    assert!(Instant::now() < deadline, "timeout waiting for rt_gc_collect to complete");
    // Cooperatively poll for a stop-the-world request while waiting.
    threading::safepoint_poll();
    std::thread::yield_now();
  }

  // Wait for the blocking task to finish without draining microtasks. The promise must still be
  // pending: settlement happens via a microtask hop back to the event-loop thread.
  let deadline = Instant::now() + TIMEOUT;
  while !unsafe { &*ctx_ptr }.finished.load(Ordering::Acquire) {
    assert!(Instant::now() < deadline, "timeout waiting for spawn_blocking_promise task to finish");
    std::thread::yield_now();
  }
  let deadline = Instant::now() + Duration::from_millis(50);
  while Instant::now() < deadline {
    let state = unsafe { &*promise_root.get() }.state.load(Ordering::Acquire);
    assert_eq!(
      state,
      PromiseHeader::PENDING,
      "promise must not settle before its settle microtask runs"
    );
    std::thread::yield_now();
  }

  // Drive microtasks until the promise settles.
  let deadline = Instant::now() + TIMEOUT;
  loop {
    let state = unsafe { &*promise_root.get() }.state.load(Ordering::Acquire);
    if state == PromiseHeader::FULFILLED || state == PromiseHeader::REJECTED {
      break;
    }
    runtime_native::rt_drain_microtasks();
    assert!(Instant::now() < deadline, "timeout waiting for spawn_blocking_promise promise to settle");
    std::thread::yield_now();
  }

  let state = unsafe { &*promise_root.get() }.state.load(Ordering::Acquire);
  assert_eq!(state, PromiseHeader::FULFILLED, "blocking task should fulfill");

  let mut current_promise = PromiseRef(promise_root.get().cast());
  let mut payload_ptr = runtime_native::rt_promise_payload_ptr(current_promise);
  assert!(!payload_ptr.is_null());
  let value = unsafe { *(payload_ptr as *const u64) };
  assert_eq!(value, 0x1122_3344_5566_7788u64);

  // The returned promise must be GC-managed (collectible). Verify via a weak handle.
  let weak = runtime_native::rt_weak_add(promise_root.get().cast::<u8>());
  drop(promise_root);

  // Ensure local stack slots don't accidentally retain the promise pointer under conservative stack
  // scanning fallbacks.
  unsafe {
    ptr::write_volatile(&mut promise, PromiseRef::null());
    ptr::write_volatile(&mut current_promise, PromiseRef::null());
    ptr::write_volatile(&mut payload_ptr, ptr::null_mut());
  }

  runtime_native::rt_gc_collect();
  assert_eq!(runtime_native::rt_weak_get(weak), ptr::null_mut());
  runtime_native::rt_weak_remove(weak);

  unsafe {
    drop(Box::from_raw(ctx_ptr));
  }

  threading::unregister_current_thread();
}

#[test]
fn spawn_blocking_promise_rejects_when_tag_is_nonzero() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  const TIMEOUT: Duration = if cfg!(debug_assertions) {
    Duration::from_secs(10)
  } else {
    Duration::from_secs(2)
  };

  let ctx = Box::new(RejectCtx {
    done: AtomicBool::new(false),
  });
  let ctx_ptr = Box::into_raw(ctx);

  let promise =
    runtime_native::rt_spawn_blocking_promise(write_u32_and_reject, ctx_ptr.cast::<u8>(), PromiseLayout::of::<u32>());
  let promise_root = Root::<PromiseHeader>::new(promise.0.cast::<PromiseHeader>());

  // Ensure the worker ran the callback.
  let deadline = Instant::now() + TIMEOUT;
  while !unsafe { &*ctx_ptr }.done.load(Ordering::Acquire) {
    assert!(Instant::now() < deadline, "timeout waiting for blocking task to run");
    std::thread::yield_now();
  }

  // Drain microtasks until the settle job runs.
  let deadline = Instant::now() + TIMEOUT;
  loop {
    let state = unsafe { &*promise_root.get() }.state.load(Ordering::Acquire);
    if state == PromiseHeader::REJECTED {
      break;
    }
    runtime_native::rt_drain_microtasks();
    assert!(Instant::now() < deadline, "timeout waiting for reject microtask to run");
    std::thread::yield_now();
  }

  let payload_ptr = runtime_native::rt_promise_payload_ptr(PromiseRef(promise_root.get().cast()));
  assert!(!payload_ptr.is_null());
  assert_eq!(unsafe { *(payload_ptr as *const u32) }, 123);

  unsafe {
    drop(Box::from_raw(ctx_ptr));
  }
  threading::unregister_current_thread();
}

#[test]
fn spawn_blocking_promise_rooted_keeps_data_alive_until_task_finishes() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  const TIMEOUT: Duration = if cfg!(debug_assertions) {
    Duration::from_secs(30)
  } else {
    Duration::from_secs(2)
  };

  ROOTED_DATA_STARTED.store(false, Ordering::Release);
  ROOTED_DATA_RELEASE.store(false, Ordering::Release);
  ROOTED_DATA_FINISHED.store(false, Ordering::Release);

  // Use a GC-managed allocation that does not depend on registering a shape table.
  let mut data_obj = runtime_native::rt_alloc_array(16, 1);
  assert!(!data_obj.is_null());

  let weak = runtime_native::rt_weak_add(data_obj);

  let _promise = runtime_native::rt_spawn_blocking_promise_rooted(
    rooted_data_task_block,
    data_obj,
    PromiseLayout::of::<()>(),
  );

  // Wait until the blocking task has started (meaning it's running under `enter_gc_safe_region`).
  let deadline = Instant::now() + TIMEOUT;
  while !ROOTED_DATA_STARTED.load(Ordering::Acquire) {
    assert!(Instant::now() < deadline, "timeout waiting for rooted blocking task to start");
    std::thread::yield_now();
  }

  // Ensure the stack doesn't accidentally keep a strong reference to `data_obj`.
  unsafe {
    ptr::write_volatile(&mut data_obj, ptr::null_mut());
  }

  // While the blocking task is still running, the rooted variant must keep the GC object alive.
  runtime_native::rt_gc_collect();
  assert_ne!(
    runtime_native::rt_weak_get(weak),
    ptr::null_mut(),
    "rooted spawn_blocking_promise must keep data alive across GC while the task is pending"
  );

  // Allow the task to complete, then ensure the object becomes collectible again.
  ROOTED_DATA_RELEASE.store(true, Ordering::Release);
  let deadline = Instant::now() + TIMEOUT;
  while !ROOTED_DATA_FINISHED.load(Ordering::Acquire) {
    assert!(Instant::now() < deadline, "timeout waiting for rooted blocking task to finish");
    std::thread::yield_now();
  }

  // Drain the settle microtask so the runtime can release its internal promise handle.
  let deadline = Instant::now() + TIMEOUT;
  while runtime_native::roots::global_persistent_handle_table().live_count() != 0 {
    let _ = runtime_native::rt_drain_microtasks();
    assert!(
      Instant::now() < deadline,
      "timeout waiting for spawn_blocking_promise_rooted to release persistent handles"
    );
    std::thread::yield_now();
  }

  runtime_native::rt_gc_collect();
  assert_eq!(
    runtime_native::rt_weak_get(weak),
    ptr::null_mut(),
    "data should be collectible after the rooted blocking task finishes"
  );
  runtime_native::rt_weak_remove(weak);

  threading::unregister_current_thread();
}
