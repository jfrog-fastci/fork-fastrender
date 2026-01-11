#![cfg(target_os = "linux")]

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

#[test]
fn safepoint_request_wakes_io_uring_wait() {
  let waiter = match runtime_native::io::uring::IoUringCqeWaiter::new() {
    Ok(waiter) => waiter,
    Err(err) => {
      if matches!(err.raw_os_error(), Some(libc::ENOSYS | libc::EPERM | libc::EACCES)) {
        return;
      }
      panic!("failed to initialize io_uring: {err}");
    }
  };

  let waker = waiter.waker();
  let stop = Arc::new(AtomicBool::new(false));

  let (started_tx, started_rx) = mpsc::channel();
  let stop_thread = stop.clone();
  let poll_thread = std::thread::spawn(move || {
    runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Io);
    started_tx.send(()).unwrap();

    let mut waiter = waiter;
    while !stop_thread.load(Ordering::Acquire) {
      waiter.wait().expect("io_uring wait failed");
    }
  });

  started_rx
    .recv_timeout(Duration::from_secs(1))
    .expect("wait thread did not start");

  let deadline = Instant::now() + Duration::from_secs(1);
  loop {
    if runtime_native::io::uring::debug_in_uring_wait() {
      std::thread::sleep(Duration::from_millis(10));
      if runtime_native::io::uring::debug_in_uring_wait() {
        break;
      }
    }
    if Instant::now() > deadline {
      stop.store(true, Ordering::Release);
      waker.wake();
      poll_thread.join().unwrap();
      panic!("wait thread did not enter io_uring wait");
    }
    std::thread::yield_now();
  }

  runtime_native::rt_gc_request_stop_the_world();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(100));

  runtime_native::rt_gc_resume_world();

  stop.store(true, Ordering::Release);
  waker.wake();
  poll_thread.join().unwrap();

  assert!(
    stopped,
    "GC stop-the-world did not complete in time; io_uring wait likely was not woken or quiescent"
  );
}
