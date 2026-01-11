use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use runtime_native::abi::TaskId;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;

extern "C" fn bump_counter(data: *mut u8) {
  let slot = unsafe { &*(data as *const AtomicUsize) };
  slot.fetch_add(1, Ordering::Relaxed);
}

#[test]
fn spawn_join_runs_many_tasks_exactly_once() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  const TASKS: usize = 10_000;
  const PRODUCERS: usize = 4;

  let counters: Arc<Vec<AtomicUsize>> = Arc::new((0..TASKS).map(|_| AtomicUsize::new(0)).collect());

  let mut handles = Vec::with_capacity(PRODUCERS);
  for producer in 0..PRODUCERS {
    let counters = counters.clone();
    handles.push(std::thread::spawn(move || {
      // Ensure the producer thread participates in GC safepoint coordination
      // while spawning work.
      threading::register_current_thread(ThreadKind::External);

      let start = producer * TASKS / PRODUCERS;
      let end = (producer + 1) * TASKS / PRODUCERS;
      let mut out: Vec<TaskId> = Vec::with_capacity(end - start);
      for i in start..end {
        let slot: *const AtomicUsize = &counters[i];
        out.push(runtime_native::rt_parallel_spawn(
          bump_counter,
          slot as *mut u8,
        ));
      }

      // Leave the registry clean in case the test harness reuses this thread.
      threading::unregister_current_thread();
      out
    }));
  }

  let mut tasks: Vec<TaskId> = Vec::with_capacity(TASKS);
  for handle in handles {
    tasks.extend(handle.join().expect("producer thread panicked"));
  }

  runtime_native::rt_parallel_join(tasks.as_ptr(), tasks.len());

  for (idx, slot) in counters.iter().enumerate() {
    let ran = slot.load(Ordering::Relaxed);
    assert_eq!(ran, 1, "task {idx} ran {ran} times");
  }

  threading::unregister_current_thread();
}

