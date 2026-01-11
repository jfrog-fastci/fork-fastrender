use runtime_native::threading;
use std::sync::Arc;
use std::sync::Barrier;
use std::time::Duration;

extern "C" {
  fn rt_thread_register(kind: u32) -> u64;
  fn rt_thread_unregister();
}

#[test]
fn rt_thread_register_and_unregister_update_registry_counts() {
  let baseline = threading::thread_counts();

  // Register the test harness thread as "Main" via the stable C ABI.
  let main_id = unsafe { rt_thread_register(0) };
  assert_ne!(main_id, 0);
  assert_eq!(unsafe { rt_thread_register(0) }, main_id, "registration should be idempotent");

  const THREADS: usize = 12;

  let registered = Arc::new(Barrier::new(THREADS + 1));
  let can_unregister = Arc::new(Barrier::new(THREADS + 1));

  let mut handles = Vec::new();
  for i in 0..THREADS {
    let registered = registered.clone();
    let can_unregister = can_unregister.clone();

    handles.push(std::thread::spawn(move || {
      // Cycle through Worker/Io/External kinds to exercise the ABI mapping.
      let kind = match i % 3 {
        0 => 1, // Worker
        1 => 2, // Io
        _ => 3, // External
      };

      let id = unsafe { rt_thread_register(kind) };
      assert_ne!(id, 0);
      assert_eq!(unsafe { rt_thread_register(kind) }, id, "registration should be idempotent");

      registered.wait();
      can_unregister.wait();

      unsafe { rt_thread_unregister() };
    }));
  }

  // Wait until all threads are registered so we can inspect the registry counts.
  registered.wait();

  let counts = threading::thread_counts();
  assert_eq!(counts.total, baseline.total + THREADS + 1);
  assert_eq!(counts.main, baseline.main + 1);

  // THREADS is divisible by 3, so the distribution is even.
  assert_eq!(counts.worker, baseline.worker + THREADS / 3);
  assert_eq!(counts.io, baseline.io + THREADS / 3);
  assert_eq!(counts.external, baseline.external + THREADS / 3);

  // Let the threads unregister and exit.
  can_unregister.wait();
  for h in handles {
    h.join().unwrap();
  }

  unsafe { rt_thread_unregister() };

  // The registry should return to the baseline counts.
  let deadline = std::time::Instant::now() + Duration::from_secs(2);
  loop {
    let counts = threading::thread_counts();
    if counts.total == baseline.total {
      assert_eq!(counts.main, baseline.main);
      assert_eq!(counts.worker, baseline.worker);
      assert_eq!(counts.io, baseline.io);
      assert_eq!(counts.external, baseline.external);
      break;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "thread registry did not return to baseline after unregister"
    );
    std::thread::yield_now();
  }
}

