use std::sync::{Arc, Barrier};
use std::thread;

use types_ts_interned::TypeStore;

#[test]
fn parallel_hot_name_lookups_do_not_require_write_lock() {
  let store = TypeStore::new();

  // Pre-intern a set of names so that all subsequent calls hit the read-fast
  // path. In real TypeScript programs, repeated property keys and intrinsic
  // names make this a common hot path under parallel type checking.
  let names: Arc<Vec<String>> = Arc::new((0..128).map(|i| format!("name_{i:04}")).collect());
  let expected: Arc<Vec<_>> = Arc::new(names.iter().map(|s| store.intern_name_ref(s)).collect());

  let thread_count = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(4)
    .min(8)
    .max(2);
  // Keep this test reasonably fast while still exercising the hot lookup path
  // under contention.
  let iters = 2_000usize;

  let barrier = Arc::new(Barrier::new(thread_count));
  let handles: Vec<_> = (0..thread_count)
    .map(|_| {
      let store = Arc::clone(&store);
      let names = Arc::clone(&names);
      let expected = Arc::clone(&expected);
      let barrier = Arc::clone(&barrier);
      thread::spawn(move || {
        barrier.wait();
        for _ in 0..iters {
          for (idx, name) in names.iter().enumerate() {
            let id = store.intern_name_ref(name);
            assert_eq!(id, expected[idx]);
          }
        }
      })
    })
    .collect();

  for handle in handles {
    handle.join().expect("thread panicked");
  }
}
