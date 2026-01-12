use runtime_native::abi::PromiseRef;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseLayout;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[repr(C)]
struct BlockCtx {
  started: AtomicUsize,
  release_lock: Mutex<bool>,
  release_cv: Condvar,
}

extern "C" fn blocking_task(data: *mut u8) {
  let ctx = unsafe { &*(data as *const BlockCtx) };
  ctx.started.fetch_add(1, Ordering::Release);

  // Allow stop-the-world GC to proceed while this task is blocked.
  let gc_safe = runtime_native::threading::enter_gc_safe_region();

  let mut guard = ctx.release_lock.lock().unwrap();
  while !*guard {
    guard = ctx.release_cv.wait(guard).unwrap();
  }
  drop(guard);
  drop(gc_safe);
}

extern "C" fn fulfill_and_signal(data: *mut u8, promise: PromiseRef) {
  // Safety: caller passes `Arc::into_raw(done.clone()) as *mut u8`.
  let done = unsafe { Arc::from_raw(data as *const AtomicBool) };
  unsafe {
    let payload = runtime_native::rt_promise_payload_ptr(promise);
    if !payload.is_null() {
      payload.write_volatile(0x5A);
    }
    runtime_native::rt_promise_fulfill(promise);
  }
  done.store(true, Ordering::Release);
  // `done` dropped here.
}

#[test]
fn parallel_spawn_promise_is_safe_if_user_drops_promise_ref_and_gc_runs() {
  let _rt = TestRuntimeGuard::new();

  // Ensure the global worker pool is initialized.
  extern "C" fn noop(_data: *mut u8) {}
  let warmup = runtime_native::rt_parallel_spawn(noop, core::ptr::null_mut());
  runtime_native::rt_parallel_join(&warmup as *const runtime_native::abi::TaskId, 1);

  // Match the runtime's worker-count selection logic so we can saturate the pool.
  let workers = std::env::var("ECMA_RS_RUNTIME_NATIVE_THREADS")
    .ok()
    .or_else(|| std::env::var("RT_NUM_THREADS").ok())
    .and_then(|v| v.parse::<usize>().ok())
    .filter(|&n| n > 0)
    .unwrap_or_else(|| {
      let default = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
      if cfg!(debug_assertions) {
        default.min(32)
      } else {
        default
      }
    });

  // Ensure worker threads are registered before we try to saturate them with blocking tasks.
  let deadline = Instant::now() + Duration::from_secs(10);
  while runtime_native::threading::thread_counts().worker < workers {
    assert!(Instant::now() < deadline, "worker threads did not register in time");
    std::thread::yield_now();
  }

  let ctx: &'static BlockCtx = Box::leak(Box::new(BlockCtx {
    started: AtomicUsize::new(0),
    release_lock: Mutex::new(false),
    release_cv: Condvar::new(),
  }));

  let mut tasks: Vec<runtime_native::abi::TaskId> = Vec::with_capacity(workers);
  for _ in 0..workers {
    tasks.push(runtime_native::rt_parallel_spawn(
      blocking_task,
      ctx as *const BlockCtx as *mut u8,
    ));
  }

  // Wait for all workers to start and block, ensuring the promise task remains queued.
  let deadline = Instant::now() + Duration::from_secs(10);
  while ctx.started.load(Ordering::Acquire) < workers {
    assert!(Instant::now() < deadline, "worker threads did not start blocking tasks in time");
    std::thread::yield_now();
  }

  let done = Arc::new(AtomicBool::new(false));

  // Drop the returned `PromiseRef` immediately: the runtime must keep the promise alive until the
  // worker settles it.
  let _ = runtime_native::rt_parallel_spawn_promise(
    fulfill_and_signal,
    Arc::into_raw(done.clone()) as *mut u8,
    PromiseLayout { size: 1, align: 1 },
  );

  // Force a stop-the-world GC while the promise is only reachable via the worker's persistent root.
  runtime_native::rt_gc_collect();

  // Release the worker pool so the detached promise task can execute and settle.
  {
    let mut guard = ctx.release_lock.lock().unwrap();
    *guard = true;
    ctx.release_cv.notify_all();
  }

  // Join the blocking tasks so the worker pool is not left saturated after this test.
  runtime_native::rt_parallel_join(tasks.as_ptr(), tasks.len());

  let deadline = Instant::now() + Duration::from_secs(10);
  while !done.load(Ordering::Acquire) {
    assert!(
      Instant::now() < deadline,
      "timeout waiting for detached payload promise task to run"
    );
    std::thread::yield_now();
  }

  // The worker wrapper allocates and must later free exactly one persistent handle for the promise.
  let deadline = Instant::now() + Duration::from_secs(10);
  while runtime_native::roots::global_persistent_handle_table().live_count() != 0 {
    assert!(
      Instant::now() < deadline,
      "persistent handle leak after payload promise task completed"
    );
    std::thread::yield_now();
  }
}

