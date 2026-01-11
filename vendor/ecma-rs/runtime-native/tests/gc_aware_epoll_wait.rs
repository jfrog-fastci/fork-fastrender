#![cfg(target_os = "linux")]

use runtime_native::async_rt::AsyncRuntime;
use runtime_native::async_rt::Task;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

extern "C" fn noop_task(_: *mut u8) {}

#[test]
fn stw_completes_while_event_loop_blocked_in_epoll_wait() {
  threading::register_current_thread(ThreadKind::Main);

  let rt = Arc::new(AsyncRuntime::new().expect("AsyncRuntime::new failed"));

  // Keep the event loop non-idle so `poll()` will block in `epoll_wait`.
  let timer = rt.schedule_timer(
    Instant::now() + Duration::from_secs(5),
    Task::new(noop_task, std::ptr::null_mut()),
  );

  let (tx_id, rx_id) = mpsc::channel();

  let handle = std::thread::spawn({
    let rt = rt.clone();
    move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      tx_id.send(id.get()).unwrap();

      // With only a far-future timer scheduled, this call blocks in epoll_wait.
      let _pending = rt.poll();

      threading::unregister_current_thread();
    }
  });

  let worker_id = rx_id.recv().unwrap();

  // Wait until the event-loop thread is GC-quiescent while blocked in `epoll_wait`.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let quiescent = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == worker_id)
      .is_some_and(|t| t.is_parked() || t.is_native_safe());
    if quiescent {
      break;
    }
    assert!(Instant::now() < deadline, "event loop did not enter a GC-quiescent state");
    std::thread::yield_now();
  }

  runtime_native::rt_gc_request_stop_the_world();
  let stopped = runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(2));
  runtime_native::rt_gc_resume_world();
  assert!(stopped, "world did not stop while event loop was blocked in epoll_wait");

  // Wake the event loop and let it exit cleanly.
  assert!(rt.cancel_timer(timer), "expected timer to exist");

  handle.join().unwrap();
  threading::unregister_current_thread();
}
