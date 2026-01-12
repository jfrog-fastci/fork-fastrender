use fastrender::render_control::{
  record_stage, GlobalStageListenerGuard, StageHeartbeat, StageListener,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[test]
fn stage_listener_guard_restores_previous_listener() {
  let _lock = crate::common::global_test_lock();
  let render_thread = std::thread::current().id();
  let a_hits = Arc::new(AtomicUsize::new(0));
  let a_hits_for_listener = Arc::clone(&a_hits);
  let render_thread_a = render_thread;
  let listener_a: StageListener = Arc::new(move |stage: StageHeartbeat| {
    if std::thread::current().id() != render_thread_a {
      return;
    }
    if stage == StageHeartbeat::DomParse {
      a_hits_for_listener.fetch_add(1, Ordering::SeqCst);
    }
  });

  let b_hits = Arc::new(AtomicUsize::new(0));
  let b_hits_for_listener = Arc::clone(&b_hits);
  let render_thread_b = render_thread;
  let listener_b: StageListener = Arc::new(move |stage: StageHeartbeat| {
    if std::thread::current().id() != render_thread_b {
      return;
    }
    if stage == StageHeartbeat::DomParse {
      b_hits_for_listener.fetch_add(1, Ordering::SeqCst);
    }
  });

  // Install A, then temporarily override it with B.
  let guard_a = GlobalStageListenerGuard::new(Arc::clone(&listener_a));
  record_stage(StageHeartbeat::DomParse);
  assert_eq!(a_hits.load(Ordering::SeqCst), 1);
  assert_eq!(b_hits.load(Ordering::SeqCst), 0);

  {
    let _guard_b = GlobalStageListenerGuard::new(Arc::clone(&listener_b));
    record_stage(StageHeartbeat::DomParse);
    assert_eq!(a_hits.load(Ordering::SeqCst), 1);
    assert_eq!(b_hits.load(Ordering::SeqCst), 1);
  }

  // Dropping B should restore A.
  record_stage(StageHeartbeat::DomParse);
  assert_eq!(a_hits.load(Ordering::SeqCst), 2);
  assert_eq!(b_hits.load(Ordering::SeqCst), 1);

  // Dropping A should restore the previous listener (usually `None` in tests).
  drop(guard_a);
  record_stage(StageHeartbeat::DomParse);
  assert_eq!(a_hits.load(Ordering::SeqCst), 2);
  assert_eq!(b_hits.load(Ordering::SeqCst), 1);
}
