use runtime_native::abi::PromiseRef;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseLayout;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

extern "C" fn task_returns_without_settling(data: *mut u8, _promise: PromiseRef) {
  // Safety: caller passes `Arc::into_raw(done.clone()) as *mut u8`.
  let done = unsafe { Arc::from_raw(data as *const AtomicBool) };
  done.store(true, Ordering::Release);
  // `done` dropped here.
}

extern "C" fn noop(_data: *mut u8) {}

#[test]
fn parallel_spawn_promise_does_not_leak_external_pending_if_task_returns_without_settling() {
  let _rt = TestRuntimeGuard::new();

  // Ensure the persistent handle table starts clean so we can wait for the worker wrapper to drop
  // its internal promise root.
  let baseline_roots = runtime_native::roots::global_persistent_handle_table().live_count();
  assert_eq!(
    baseline_roots, 0,
    "expected clean persistent handle table at start of test"
  );

  let done = Arc::new(AtomicBool::new(false));

  let promise = runtime_native::rt_parallel_spawn_promise(
    task_returns_without_settling,
    Arc::into_raw(done.clone()) as *mut u8,
    PromiseLayout::of::<()>(),
  );
  assert!(!promise.is_null());

  // Wait until the task has returned and the runtime has released its persistent root for the
  // promise. The task intentionally violates the "must settle" contract, so the promise remains
  // pending, but the async runtime must not stay externally pending forever once the worker task is
  // complete.
  let deadline = Instant::now() + Duration::from_secs(10);
  while !done.load(Ordering::Acquire)
    || runtime_native::roots::global_persistent_handle_table().live_count() != baseline_roots
  {
    assert!(
      Instant::now() < deadline,
      "timeout waiting for detached parallel promise task to complete"
    );
    std::thread::yield_now();
  }

  // Schedule an immediate timeout so `rt_async_poll` makes progress even if the external-pending
  // count was leaked (otherwise it could block indefinitely waiting for "external work" that can
  // never complete).
  runtime_native::rt_set_timeout(noop, core::ptr::null_mut(), 0);

  let pending = runtime_native::rt_async_poll();
  assert!(
    !pending,
    "expected runtime to report idle after the worker task returned; leaked EXTERNAL_PENDING would keep rt_async_poll pending"
  );
}

