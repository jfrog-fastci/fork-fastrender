use runtime_native::abi::LegacyPromiseRef;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading::ThreadKind;
use runtime_native::{
  rt_async_poll_legacy as rt_async_poll,
  rt_gc_collect,
  rt_promise_resolve_legacy as rt_promise_resolve,
  rt_promise_then_legacy as rt_promise_then,
  rt_spawn_blocking,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[repr(C)]
struct BlockedTaskState {
  started: AtomicBool,
  release: AtomicBool,
  finished: AtomicBool,
}

extern "C" fn blocking_task_wait(data: *mut u8, promise: LegacyPromiseRef) {
  let st = unsafe { &*(data as *const BlockedTaskState) };
  st.started.store(true, Ordering::Release);

  while !st.release.load(Ordering::Acquire) {
    // Use an actual blocking primitive so the regression test catches worker threads that are not
    // in a GC-safe region (stop-the-world must not wait on this thread).
    std::thread::sleep(Duration::from_millis(1));
  }

  st.finished.store(true, Ordering::Release);
  rt_promise_resolve(promise, core::ptr::null_mut());
}

extern "C" fn set_bool(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

#[test]
fn stw_gc_does_not_deadlock_with_blocking_pool_tasks() {
  let _rt = TestRuntimeGuard::new();

  // Stop-the-world handshakes can take much longer in debug builds (especially under parallel test
  // execution on multi-agent hosts). Keep release builds strict, but give debug builds enough slack
  // to avoid flaky timeouts.
  const TIMEOUT: Duration = if cfg!(debug_assertions) {
    Duration::from_secs(30)
  } else {
    Duration::from_secs(2)
  };

  let state = Box::new(BlockedTaskState {
    started: AtomicBool::new(false),
    release: AtomicBool::new(false),
    finished: AtomicBool::new(false),
  });
  let state_ptr = (&*state as *const BlockedTaskState).cast_mut().cast::<u8>();

  let settled = Box::new(AtomicBool::new(false));
  let settled_ptr = (&*settled as *const AtomicBool).cast_mut().cast::<u8>();

  let promise = rt_spawn_blocking(blocking_task_wait, state_ptr);
  rt_promise_then(promise, set_bool, settled_ptr);

  // Wait until the blocking worker thread is definitely inside `blocking_task_wait`.
  let start = Instant::now();
  while !state.started.load(Ordering::Acquire) {
    assert!(
      start.elapsed() < TIMEOUT,
      "timeout waiting for blocking task to start (pool may not be running tasks)"
    );
    std::thread::yield_now();
  }

  // Trigger a stop-the-world GC from another thread while the blocking task remains blocked.
  let (done_tx, done_rx) = mpsc::channel::<()>();
  let gc_thread = std::thread::spawn(move || {
    runtime_native::threading::register_current_thread(ThreadKind::Worker);
    rt_gc_collect();
    done_tx.send(()).unwrap();
    runtime_native::threading::unregister_current_thread();
  });

  // Mark this (event-loop) thread NativeSafe while we block waiting for GC completion. If the
  // blocking worker thread is GC-unsafe, `rt_gc_collect` will deadlock waiting for it; staying
  // NativeSafe here ensures we can still time out and release the worker.
  let gc_safe_guard = runtime_native::threading::enter_gc_safe_region();

  // In the expected/fixed behavior, GC completes even though the blocking worker thread is still
  // asleep in `blocking_task_wait`.
  match done_rx.recv_timeout(TIMEOUT) {
    Ok(()) => {
      assert!(
        !state.finished.load(Ordering::Acquire),
        "blocking task finished before it was released; test must keep the worker blocked during GC"
      );
    }
    Err(_) => {
      // Defensive cleanup: unblock the worker so the GC coordinator can make progress even if the
      // blocking pool regresses and forgets to enter a GC-safe region.
      state.release.store(true, Ordering::Release);

      // Wait again: after unblocking, GC should be able to complete quickly.
      done_rx
        .recv_timeout(TIMEOUT)
        .expect("stop-the-world GC did not complete after unblocking the worker (deadlock)");

      // Keep the guard alive until after STW completes; dropping it while STW is active would block.
      drop(gc_safe_guard);
      let _ = gc_thread.join();
      panic!("stop-the-world GC timed out while a blocking pool task was blocked (deadlock regression)");
    }
  }

  drop(gc_safe_guard);
  gc_thread.join().unwrap();

  // Release the blocking task and ensure its promise settles.
  state.release.store(true, Ordering::Release);

  let start = Instant::now();
  while !settled.load(Ordering::SeqCst) {
    rt_async_poll();
    assert!(
      start.elapsed() < TIMEOUT,
      "timeout waiting for spawn_blocking promise to settle after releasing the task"
    );
  }

  assert!(state.finished.load(Ordering::Acquire));
}
