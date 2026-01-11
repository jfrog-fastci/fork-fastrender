#![cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]

use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::thread_stack;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;

#[test]
fn current_thread_stack_bounds_contains_local_address() {
  let _rt = TestRuntimeGuard::new();

  let local = 0u8;
  let local_addr = std::ptr::addr_of!(local) as usize;
  let bounds = thread_stack::current_thread_stack_bounds().unwrap();

  assert!(bounds.low < bounds.high);
  assert!(bounds.contains(local_addr));
}

#[test]
fn registered_thread_stack_bounds_contains_worker_local_address() {
  let _rt = TestRuntimeGuard::new();

  // Ensure the coordinator is registered so the registry is initialized in a normal way.
  threading::register_current_thread(ThreadKind::Main);

  let (tx, rx) = std::sync::mpsc::channel::<(threading::ThreadId, usize)>();
  let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();

  let handle = std::thread::spawn(move || {
    let tid = threading::register_current_thread(ThreadKind::Worker);
    let local = 0u8;
    let addr = std::ptr::addr_of!(local) as usize;
    tx.send((tid, addr)).unwrap();

    // Keep `local` in-scope until the main thread has validated the bounds.
    let _ = local;
    ack_rx.recv().unwrap();

    threading::unregister_current_thread();
  });

  let (tid, addr) = rx.recv().unwrap();

  let thread = threading::all_threads()
    .into_iter()
    .find(|t| t.id() == tid)
    .expect("worker thread should be present in registry");
  let bounds = thread
    .stack_bounds()
    .expect("registered worker should have stack bounds on this platform");

  assert!(bounds.lo < bounds.hi);
  assert!(
    addr >= bounds.lo && addr < bounds.hi,
    "worker local address {addr:#x} not within recorded bounds [{:#x}, {:#x})",
    bounds.lo,
    bounds.hi
  );

  ack_tx.send(()).unwrap();
  handle.join().unwrap();

  threading::unregister_current_thread();
}

