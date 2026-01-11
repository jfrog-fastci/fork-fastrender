use runtime_native::current_thread;
use runtime_native::Runtime;
use std::collections::HashSet;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Barrier;
use std::thread;
use std::time::Duration;

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

#[test]
fn stop_the_world_blocks_attach() {
  let runtime = Arc::new(Runtime::new());

  let guard = runtime.attach_current_thread().expect("attach main");
  let main_id = guard.thread().id;

  // Hold an STW guard while another thread tries to attach.
  let stw = runtime.stop_the_world();

  let (tx_started, rx_started) = mpsc::channel();
  let (tx_attached, rx_attached) = mpsc::channel();

  let runtime2 = runtime.clone();
  let handle = thread::spawn(move || {
    tx_started.send(()).unwrap();
    let g = runtime2.attach_current_thread().expect("attach worker");
    let id = current_thread().expect("TLS should be set").id;
    tx_attached.send(id).unwrap();
    drop(g);
  });

  // Ensure the worker thread is running and attempting to attach.
  rx_started.recv().unwrap();

  // Attach should not complete while STW is held.
  assert!(rx_attached.recv_timeout(Duration::from_millis(100)).is_err());
  assert_eq!(runtime.thread_count(), 1);

  drop(stw);

  let worker_id = rx_attached
    .recv_timeout(Duration::from_secs(2))
    .expect("worker should attach after STW released");
  assert_ne!(worker_id, main_id);

  handle.join().unwrap();

  drop(guard);
  assert_eq!(runtime.thread_count(), 0);
}
