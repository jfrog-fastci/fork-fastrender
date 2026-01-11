use runtime_native::{rt_gc_collect, rt_gc_safepoint, rt_thread_deinit, rt_thread_init};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn wait_until(timeout: Duration, f: impl Fn() -> bool) {
  let start = Instant::now();
  while start.elapsed() < timeout {
    if f() {
      return;
    }
    thread::sleep(Duration::from_millis(1));
  }
  panic!("timeout");
}

fn spawn_workers(
  count: usize,
  stop: Arc<AtomicBool>,
  progress: Arc<Vec<AtomicUsize>>,
) -> Vec<thread::JoinHandle<()>> {
  (0..count)
    .map(|idx| {
      let stop = stop.clone();
      let progress = progress.clone();
      thread::spawn(move || {
        rt_thread_init(1);
        while !stop.load(Ordering::Relaxed) {
          progress[idx].fetch_add(1, Ordering::Relaxed);
          rt_gc_safepoint();
        }
        rt_thread_deinit();
      })
    })
    .collect()
}

#[test]
fn attached_initiator() {
  let _test_guard = TEST_LOCK.lock().unwrap();

  let stop = Arc::new(AtomicBool::new(false));
  let workers = 4;
  let progress = Arc::new(
    (0..workers)
      .map(|_| AtomicUsize::new(0))
      .collect::<Vec<_>>(),
  );
  let handles = spawn_workers(workers, stop.clone(), progress.clone());

  wait_until(Duration::from_secs(2), || {
    progress.iter().all(|c| c.load(Ordering::Relaxed) > 50)
  });

  let start_epoch = runtime_native::threading::safepoint::current_epoch();
  assert_eq!(start_epoch & 1, 0, "GC epoch should start even");

  let initiator = thread::spawn(move || {
    rt_thread_init(1);
    rt_gc_collect();
    rt_thread_deinit();
  });
  initiator.join().unwrap();

  let end_epoch = runtime_native::threading::safepoint::current_epoch();
  assert_eq!(end_epoch, start_epoch + 2);

  let after = progress
    .iter()
    .map(|c| c.load(Ordering::Relaxed))
    .collect::<Vec<_>>();
  wait_until(Duration::from_secs(2), || {
    progress
      .iter()
      .zip(after.iter())
      .all(|(c, before)| c.load(Ordering::Relaxed) > *before)
  });

  stop.store(true, Ordering::Relaxed);
  for h in handles {
    h.join().unwrap();
  }
}

#[test]
fn concurrent_initiators() {
  let _test_guard = TEST_LOCK.lock().unwrap();

  let stop = Arc::new(AtomicBool::new(false));
  let workers = 4;
  let progress = Arc::new(
    (0..workers)
      .map(|_| AtomicUsize::new(0))
      .collect::<Vec<_>>(),
  );
  let handles = spawn_workers(workers, stop.clone(), progress.clone());

  wait_until(Duration::from_secs(2), || {
    progress.iter().all(|c| c.load(Ordering::Relaxed) > 50)
  });

  let start_epoch = runtime_native::threading::safepoint::current_epoch();
  assert_eq!(start_epoch & 1, 0, "GC epoch should start even");

  let barrier = Arc::new(std::sync::Barrier::new(2));
  let mut initiators = Vec::new();
  for _ in 0..2 {
    let barrier = barrier.clone();
    initiators.push(thread::spawn(move || {
      rt_thread_init(1);
      barrier.wait();
      rt_gc_collect();
      rt_thread_deinit();
    }));
  }
  for t in initiators {
    t.join().unwrap();
  }

  let end_epoch = runtime_native::threading::safepoint::current_epoch();
  assert_eq!(
    end_epoch,
    start_epoch + 2,
    "concurrent callers should not start multiple GC cycles"
  );

  let after = progress
    .iter()
    .map(|c| c.load(Ordering::Relaxed))
    .collect::<Vec<_>>();
  wait_until(Duration::from_secs(2), || {
    progress
      .iter()
      .zip(after.iter())
      .all(|(c, before)| c.load(Ordering::Relaxed) > *before)
  });

  stop.store(true, Ordering::Relaxed);
  for h in handles {
    h.join().unwrap();
  }
}

#[test]
fn non_attached_initiator() {
  let _test_guard = TEST_LOCK.lock().unwrap();

  let stop = Arc::new(AtomicBool::new(false));
  let workers = 4;
  let progress = Arc::new(
    (0..workers)
      .map(|_| AtomicUsize::new(0))
      .collect::<Vec<_>>(),
  );
  let handles = spawn_workers(workers, stop.clone(), progress.clone());

  wait_until(Duration::from_secs(2), || {
    progress.iter().all(|c| c.load(Ordering::Relaxed) > 50)
  });

  let start_epoch = runtime_native::threading::safepoint::current_epoch();
  assert_eq!(start_epoch & 1, 0, "GC epoch should start even");

  // Main test thread is intentionally *not* attached.
  rt_gc_collect();

  let end_epoch = runtime_native::threading::safepoint::current_epoch();
  assert_eq!(end_epoch, start_epoch + 2);

  let after = progress
    .iter()
    .map(|c| c.load(Ordering::Relaxed))
    .collect::<Vec<_>>();
  wait_until(Duration::from_secs(2), || {
    progress
      .iter()
      .zip(after.iter())
      .all(|(c, before)| c.load(Ordering::Relaxed) > *before)
  });

  stop.store(true, Ordering::Relaxed);
  for h in handles {
    h.join().unwrap();
  }
}
