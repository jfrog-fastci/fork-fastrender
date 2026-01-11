use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::io::IoRuntime;
use runtime_native::abi::PromiseRef;
use runtime_native::promise_api::{Promise, PromiseExt};
use std::future::Future;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, RawWaker, RawWakerVTable, Waker};
use std::time::Duration;

extern "C" fn noop_task(_data: *mut u8) {}

fn noop_waker() -> Waker {
  unsafe fn clone(_: *const ()) -> RawWaker {
    RawWaker::new(std::ptr::null(), &VTABLE)
  }
  unsafe fn wake(_: *const ()) {}
  unsafe fn wake_by_ref(_: *const ()) {}
  unsafe fn drop_waker(_: *const ()) {}

  static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);
  unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

fn wait_until_thread_native_safe(thread_id: u64, timeout: Duration) {
  let deadline = std::time::Instant::now() + timeout;
  loop {
    let thread = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == thread_id)
      .expect("thread state");

    if thread.is_native_safe() {
      return;
    }

    assert!(
      std::time::Instant::now() < deadline,
      "thread did not enter NativeSafe in time"
    );
    std::thread::yield_now();
  }
}

#[test]
fn stop_the_world_does_not_wait_for_thread_blocked_on_microtask_queue_mutex() {
  let _rt = TestRuntimeGuard::new();

  threading::register_current_thread(ThreadKind::Main);

  let handle = runtime_native::async_rt::debug_with_microtasks_lock(|| {
    // While holding the lock, spawn a worker that will contend and block trying to enqueue.
    let (tx_id, rx_id) = mpsc::channel();
    let started = Arc::new(Barrier::new(2));

    let started_worker = started.clone();
    let handle = std::thread::spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      tx_id.send(id.get()).unwrap();

      // Begin contended acquisition deterministically.
      started_worker.wait();

      runtime_native::async_rt::enqueue_microtask(noop_task, std::ptr::null_mut());

      threading::unregister_current_thread();
    });

    let worker_id = rx_id.recv().unwrap();
    started.wait();

    // Wait until the worker is blocked in the contended path (NativeSafe).
    wait_until_thread_native_safe(worker_id, Duration::from_secs(2));

    // Stop-the-world should *not* wait for a thread that's blocked on the async runtime mutex.
    runtime_native::rt_gc_request_stop_the_world();
    let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(200));
    runtime_native::rt_gc_resume_world();
    assert!(
      stopped,
      "world did not stop while worker thread was blocked on the microtask-queue mutex"
    );

    handle
  });

  handle.join().unwrap();
  threading::unregister_current_thread();
}

#[cfg(target_os = "linux")]
#[test]
fn stop_the_world_does_not_wait_for_thread_blocked_on_reactor_watcher_mutex() {
  let _rt = TestRuntimeGuard::new();

  threading::register_current_thread(ThreadKind::Main);

  extern "C" fn noop_io(_events: u32, _data: *mut u8) {}

  fn pipe() -> (i32, i32) {
    let mut fds = [0; 2];
    // The reactor enforces an edge-triggered contract which requires all registered fds to be
    // nonblocking.
    let res = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK) };
    assert_eq!(res, 0);
    (fds[0], fds[1])
  }

  fn close(fd: i32) {
    unsafe {
      libc::close(fd);
    }
  }

  let handle = runtime_native::async_rt::debug_with_reactor_watchers_lock(|| {
    let (tx_id, rx_id) = mpsc::channel();
    let started = Arc::new(Barrier::new(2));

    let started_worker = started.clone();
    let handle = std::thread::spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      tx_id.send(id.get()).unwrap();

      started_worker.wait();

      let (rfd, wfd) = pipe();
      let watcher = runtime_native::async_rt::global()
        .register_io(rfd, runtime_native::abi::RT_IO_READABLE, noop_io, std::ptr::null_mut())
        .expect("register_io should succeed");
      let _ = runtime_native::async_rt::global().deregister_fd(watcher);
      close(rfd);
      close(wfd);

      threading::unregister_current_thread();
    });

    let worker_id = rx_id.recv().unwrap();
    started.wait();

    wait_until_thread_native_safe(worker_id, Duration::from_secs(2));

    runtime_native::rt_gc_request_stop_the_world();
    let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(200));
    runtime_native::rt_gc_resume_world();
    assert!(
      stopped,
      "world did not stop while worker thread was blocked on the reactor watcher mutex"
    );

    handle
  });

  handle.join().unwrap();
  threading::unregister_current_thread();
}

#[cfg(not(target_os = "linux"))]
#[test]
fn reactor_watcher_mutex_contention_not_supported_on_this_platform() {}

#[test]
fn stop_the_world_does_not_wait_for_thread_blocked_on_time_registry_mutex() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let handle = runtime_native::time::debug_with_registry_lock(|| {
    let (tx_id, rx_id) = mpsc::channel();
    let started = Arc::new(Barrier::new(2));

    let started_worker = started.clone();
    let handle = std::thread::spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      tx_id.send(id.get()).unwrap();
      started_worker.wait();

      // Contend on the registry lock deterministically.
      let _ = runtime_native::time::debug_registration_count();

      threading::unregister_current_thread();
    });

    let worker_id = rx_id.recv().unwrap();
    started.wait();

    wait_until_thread_native_safe(worker_id, Duration::from_secs(2));

    runtime_native::rt_gc_request_stop_the_world();
    let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(200));
    runtime_native::rt_gc_resume_world();
    assert!(
      stopped,
      "world did not stop while worker thread was blocked on the time registry mutex"
    );

    handle
  });

  handle.join().unwrap();
  threading::unregister_current_thread();
}

#[test]
fn stop_the_world_does_not_wait_for_thread_blocked_on_io_registry_mutex() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let io_rt = Arc::new(IoRuntime::new());

  let handle = io_rt.debug_with_registry_lock(|| {
    let (tx_id, rx_id) = mpsc::channel();
    let started = Arc::new(Barrier::new(2));
    let io_rt_worker = io_rt.clone();

    let started_worker = started.clone();
    let handle = std::thread::spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      tx_id.send(id.get()).unwrap();
      started_worker.wait();

      // Contend on the op registry lock deterministically.
      let _ = io_rt_worker.debug_registry_len();

      threading::unregister_current_thread();
    });

    let worker_id = rx_id.recv().unwrap();
    started.wait();

    wait_until_thread_native_safe(worker_id, Duration::from_secs(2));

    runtime_native::rt_gc_request_stop_the_world();
    let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(200));
    runtime_native::rt_gc_resume_world();
    assert!(
      stopped,
      "world did not stop while worker thread was blocked on the I/O op registry mutex"
    );

    handle
  });

  handle.join().unwrap();
  drop(io_rt);
  threading::unregister_current_thread();
}

extern "C" fn spawn_blocking_noop(data: *mut u8, promise: PromiseRef) {
  let done = unsafe { &*(data as *const AtomicBool) };
  done.store(true, Ordering::Release);
  runtime_native::rt_promise_resolve_legacy(promise, core::ptr::null_mut());
}

#[test]
fn stop_the_world_does_not_wait_for_thread_blocked_on_spawn_blocking_queue_mutex() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let done = Box::new(AtomicBool::new(false));
  let done_ptr = Box::into_raw(done);
  let done_bits = done_ptr as usize;

  let hold = runtime_native::test_util::debug_hold_blocking_pool_queue_lock();

  // While holding the lock, spawn a worker that will contend and block trying to enqueue.
  let (tx_id, rx_id) = mpsc::channel();
  let started = Arc::new(Barrier::new(2));

  let started_worker = started.clone();
  let handle = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Worker);
    tx_id.send(id.get()).unwrap();

    // Begin contended acquisition deterministically.
    started_worker.wait();

    let _ = runtime_native::rt_spawn_blocking(spawn_blocking_noop, done_bits as *mut u8);

    threading::unregister_current_thread();
  });

  let worker_id = rx_id.recv().unwrap();
  started.wait();

  // Wait until the worker is blocked in the contended path (NativeSafe).
  wait_until_thread_native_safe(worker_id, Duration::from_secs(2));

  // Stop-the-world should *not* wait for a thread that's blocked on the blocking pool mutex.
  runtime_native::rt_gc_request_stop_the_world();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(200));
  runtime_native::rt_gc_resume_world();
  assert!(
    stopped,
    "world did not stop while worker thread was blocked on the spawn_blocking queue mutex"
  );

  // Allow the worker to enqueue the task and finish.
  drop(hold);
  handle.join().unwrap();

  let deadline = std::time::Instant::now() + Duration::from_secs(2);
  while !unsafe { &*done_ptr }.load(Ordering::Acquire) {
    assert!(std::time::Instant::now() < deadline, "spawn_blocking task did not run in time");
    std::thread::yield_now();
  }

  unsafe {
    drop(Box::from_raw(done_ptr));
  }

  threading::unregister_current_thread();
}

#[test]
fn stop_the_world_does_not_wait_for_thread_blocked_on_promise_rejection_tracker_mutex() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let handle = runtime_native::promise_api::debug_with_rejection_tracker_lock(|| {
    let (tx_id, rx_id) = mpsc::channel();
    let started = Arc::new(Barrier::new(2));
    let started_worker = started.clone();

    let handle = std::thread::spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      tx_id.send(id.get()).unwrap();

      started_worker.wait();

      // Contend on the global rejection tracker lock deterministically.
      let _ = runtime_native::promise_api::rt_take_unhandled_rejections();

      threading::unregister_current_thread();
    });

    let worker_id = rx_id.recv().unwrap();
    started.wait();

    wait_until_thread_native_safe(worker_id, Duration::from_secs(2));

    runtime_native::rt_gc_request_stop_the_world();
    let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(200));
    runtime_native::rt_gc_resume_world();
    assert!(
      stopped,
      "world did not stop while worker thread was blocked on the promise rejection tracker mutex"
    );

    handle
  });

  handle.join().unwrap();
  threading::unregister_current_thread();
}

#[test]
fn stop_the_world_does_not_wait_for_thread_blocked_on_promise_wakers_mutex() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let (promise, _resolve, _reject) = Promise::<u8>::new();

  let handle = runtime_native::promise_api::debug_with_promise_wakers_lock(&promise, || {
    let (tx_id, rx_id) = mpsc::channel();
    let started = Arc::new(Barrier::new(2));

    let promise_worker = promise.clone();
    let started_worker = started.clone();
    let handle = std::thread::spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      tx_id.send(id.get()).unwrap();

      started_worker.wait();

      // Contend on the per-promise wakers lock by polling the pending future.
      let waker = noop_waker();
      let mut cx = Context::from_waker(&waker);
      let mut fut = Box::pin(promise_worker.into_future());
      let _ = fut.as_mut().poll(&mut cx);

      threading::unregister_current_thread();
    });

    let worker_id = rx_id.recv().unwrap();
    started.wait();

    wait_until_thread_native_safe(worker_id, Duration::from_secs(2));

    runtime_native::rt_gc_request_stop_the_world();
    let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(200));
    runtime_native::rt_gc_resume_world();
    assert!(
      stopped,
      "world did not stop while worker thread was blocked on the promise wakers mutex"
    );

    handle
  });

  handle.join().unwrap();
  threading::unregister_current_thread();
}

#[test]
fn stop_the_world_does_not_wait_for_thread_blocked_on_stackmap_registry_rwlock() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  // Hold the registry's write lock so other threads will deterministically contend.
  let hold = runtime_native::global_stackmap_registry().write();

  let (tx_id, rx_id) = mpsc::channel();
  let started = Arc::new(Barrier::new(2));

  let started_worker = started.clone();
  let handle = std::thread::spawn(move || {
    let id = threading::register_current_thread(ThreadKind::Worker);
    tx_id.send(id.get()).unwrap();

    started_worker.wait();

    // Contend on the global stackmap registry lock.
    let _ = runtime_native::global_stackmap_registry().read();

    threading::unregister_current_thread();
  });

  let worker_id = rx_id.recv().unwrap();
  started.wait();

  wait_until_thread_native_safe(worker_id, Duration::from_secs(2));

  runtime_native::rt_gc_request_stop_the_world();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(200));
  runtime_native::rt_gc_resume_world();
  assert!(
    stopped,
    "world did not stop while worker thread was blocked on the stackmap registry rwlock"
  );

  drop(hold);
  handle.join().unwrap();
  threading::unregister_current_thread();
}
