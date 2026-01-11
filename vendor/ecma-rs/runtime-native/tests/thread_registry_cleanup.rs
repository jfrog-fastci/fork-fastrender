use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use std::time::Duration;

struct ResumeWorldOnDrop;

impl Drop for ResumeWorldOnDrop {
  fn drop(&mut self) {
    runtime_native::rt_gc_resume_world();
  }
}

#[test]
fn dead_threads_are_pruned_and_do_not_block_stop_the_world() {
  let _rt = TestRuntimeGuard::new();

  // Spawn short-lived threads that register with the runtime (via `rt_async_poll`) but never
  // explicitly call `threading::unregister_current_thread()`.
  //
  // The thread registry must not keep these dead threads alive forever: STW GC must not wait on
  // them, and `thread_counts().total` must not monotonically grow.
  const ITERS: usize = 32;

  let mut prev_total = None;
  let mut saw_non_increase = false;

  for _ in 0..ITERS {
    std::thread::spawn(|| {
      let _ = runtime_native::rt_async_poll();
    })
    .join()
    .unwrap();

    let total = threading::thread_counts().total;
    if let Some(prev) = prev_total {
      if total <= prev {
        saw_non_increase = true;
      }
    }
    prev_total = Some(total);

    // A stale thread entry with an unobserved epoch would deadlock/timeout here.
    runtime_native::rt_gc_request_stop_the_world();
    let _resume = ResumeWorldOnDrop;
    assert!(
      runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(200)),
      "stop-the-world should not wait on threads that have already exited"
    );
  }

  assert!(
    saw_non_increase,
    "thread_counts().total appears to monotonically increase across iterations (last={prev_total:?})"
  );
}

#[test]
fn register_current_thread_can_upgrade_kind() {
  let _rt = TestRuntimeGuard::new();

  let id1 = threading::register_current_thread(threading::ThreadKind::External);
  assert_eq!(
    threading::registry::current_thread_state().unwrap().kind(),
    threading::ThreadKind::External
  );

  // Re-registering the same OS thread should be able to upgrade its kind. This is important for
  // threads that first enter via generic parallel APIs (`External`) but later become the main
  // event-loop thread.
  let id2 = threading::register_current_thread(threading::ThreadKind::Main);
  assert_eq!(id1.get(), id2.get());
  assert_eq!(
    threading::registry::current_thread_state().unwrap().kind(),
    threading::ThreadKind::Main
  );
}
