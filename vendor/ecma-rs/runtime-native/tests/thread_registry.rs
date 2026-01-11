use runtime_native::current_thread;
use runtime_native::Runtime;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Barrier;
use std::thread;

#[test]
fn thread_attach_detach_registry_and_tls() {
  let n = 8usize;
  let runtime = Arc::new(Runtime::new());

  assert_eq!(runtime.thread_count(), 0);

  let attached = Arc::new(Barrier::new(n + 1));
  let detach = Arc::new(Barrier::new(n + 1));

  let mut handles = Vec::with_capacity(n);
  for _ in 0..n {
    let runtime = runtime.clone();
    let attached = attached.clone();
    let detach = detach.clone();
    handles.push(thread::spawn(move || {
      let guard = runtime.attach_current_thread().expect("attach failed");

      let thread = current_thread().expect("TLS should be set after attach");
      assert_eq!(thread.id, guard.thread().id);

      // Double attach is rejected.
      assert!(runtime.attach_current_thread().is_err());

      attached.wait();
      detach.wait();

      drop(guard);
      assert!(current_thread().is_none());

      thread.id
    }));
  }

  // Wait until all threads have attached.
  attached.wait();
  assert_eq!(runtime.thread_count(), n);

  // Allow threads to detach.
  detach.wait();

  let mut ids = Vec::with_capacity(n);
  for handle in handles {
    ids.push(handle.join().expect("thread panicked"));
  }

  assert_eq!(runtime.thread_count(), 0);

  let uniq: HashSet<u32> = ids.into_iter().collect();
  assert_eq!(uniq.len(), n);
}

