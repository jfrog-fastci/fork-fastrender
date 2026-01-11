use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{rt_gc_collect, rt_gc_safepoint, rt_register_current_thread, rt_unregister_current_thread};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

#[test]
fn rt_gc_collect_stops_and_resumes_registered_threads() {
  let _rt = TestRuntimeGuard::new();
  const WORKERS: usize = 4;

  let start = Arc::new(Barrier::new(WORKERS + 1));
  let stop = Arc::new(AtomicBool::new(false));
  let progress: Arc<Vec<AtomicUsize>> =
    Arc::new((0..WORKERS).map(|_| AtomicUsize::new(0)).collect());

  let mut handles = Vec::new();
  for i in 0..WORKERS {
    let start = start.clone();
    let stop = stop.clone();
    let progress = progress.clone();
    handles.push(std::thread::spawn(move || {
      rt_register_current_thread();
      start.wait();

      while !stop.load(Ordering::Acquire) {
        progress[i].fetch_add(1, Ordering::Relaxed);
        rt_gc_safepoint();
        std::thread::yield_now();
      }

      rt_unregister_current_thread();
    }));
  }

  start.wait();
  std::thread::sleep(Duration::from_millis(50));
  let before: Vec<usize> = progress.iter().map(|c| c.load(Ordering::Relaxed)).collect();

  // Trigger a cooperative stop-the-world handshake.
  rt_gc_collect();

  std::thread::sleep(Duration::from_millis(50));
  let after: Vec<usize> = progress.iter().map(|c| c.load(Ordering::Relaxed)).collect();

  stop.store(true, Ordering::Release);
  for h in handles {
    h.join().unwrap();
  }

  for (i, (b, a)) in before.into_iter().zip(after).enumerate() {
    assert!(
      a > b,
      "worker {i} did not make progress after rt_gc_collect (before={b}, after={a})"
    );
  }
}

