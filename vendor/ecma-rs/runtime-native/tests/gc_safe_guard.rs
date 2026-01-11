use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[test]
fn gc_safe_guard_is_nestable_and_blocks_exit_during_stop_the_world() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let state = threading::registry::current_thread_state().expect("registered thread state");
  assert!(!state.is_native_safe());

  // Nesting is tracked via `native_safe_depth`, and guards may be dropped in any
  // order without leaving the GC-safe region early.
  let g_outer = threading::enter_gc_safe_region();
  assert!(state.is_native_safe());
  let g_inner = threading::enter_gc_safe_region();
  assert!(state.is_native_safe());

  // Drop outermost first: should still be native-safe due to the inner guard.
  drop(g_outer);
  assert!(state.is_native_safe());

  let (tx_stopped, rx_stopped) = mpsc::channel();
  let (tx_drop_started, rx_drop_started) = mpsc::channel();

  let coordinator = std::thread::spawn(move || {
    runtime_native::rt_gc_request_stop_the_world();
    assert!(
      runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(1)),
      "world did not reach safepoint in time"
    );

    tx_stopped.send(()).unwrap();

    // Ensure the main thread has begun dropping the outermost guard before we
    // resume the world; the drop should block until resume.
    rx_drop_started
      .recv_timeout(Duration::from_secs(1))
      .expect("main thread did not start dropping guard in time");

    std::thread::sleep(Duration::from_millis(200));
    runtime_native::rt_gc_resume_world();
  });

  rx_stopped
    .recv_timeout(Duration::from_secs(1))
    .expect("coordinator did not stop the world in time");

  tx_drop_started.send(()).unwrap();

  let start = Instant::now();
  drop(g_inner);
  assert!(
    start.elapsed() >= Duration::from_millis(150),
    "dropping outermost GcSafeGuard should block until the world is resumed"
  );

  coordinator.join().unwrap();
  assert!(!state.is_native_safe());

  threading::unregister_current_thread();
}

