use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

// Stop-the-world handshakes can take longer in debug builds under parallel test execution.
const TIMEOUT: Duration = if cfg!(debug_assertions) {
  Duration::from_secs(30)
} else {
  Duration::from_secs(2)
};

static OBSERVED: AtomicUsize = AtomicUsize::new(0);

extern "C" fn record_ptr(_i: usize, data: *mut u8) {
  OBSERVED.store(data as usize, Ordering::SeqCst);
}

fn wait_until_native_safe(thread_id: u64) {
  let deadline = Instant::now() + TIMEOUT;
  loop {
    let native_safe = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == thread_id)
      .map(|t| t.is_native_safe())
      .unwrap_or(false);
    if native_safe {
      return;
    }
    assert!(
      Instant::now() < deadline,
      "thread {thread_id} did not enter NativeSafe while blocked on persistent handle table lock"
    );
    std::thread::yield_now();
  }
}

#[test]
fn parallel_for_rooted_h_reads_userdata_slot_after_lock_acquired() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  // Treat pointers as opaque addresses; they do not need to be dereferenceable for this test.
  let mut slot_value: *mut u8 = 0x1111usize as *mut u8;
  let new_value: *mut u8 = 0x2222usize as *mut u8;
  // Raw pointers are `!Send` on newer Rust versions; pass them across threads as addresses.
  let slot_ptr: usize = (&mut slot_value as *mut *mut u8) as usize;

  // Ensure the global persistent handle table starts empty so the test is deterministic.
  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    0
  );

  std::thread::scope(|scope| {
    // Thread A holds the persistent handle table read lock.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

    // Thread B attempts to allocate from the slot while the lock is held.
    let (b_id_tx, b_id_rx) = mpsc::channel::<u64>();
    let (b_start_tx, b_start_rx) = mpsc::channel::<()>();

    scope.spawn(move || {
      threading::register_current_thread(ThreadKind::Worker);
      runtime_native::roots::global_persistent_handle_table().debug_with_read_lock_for_tests(|| {
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
      });
      threading::unregister_current_thread();
    });

    a_locked_rx
      .recv_timeout(TIMEOUT)
      .expect("thread A should acquire the persistent handle table read lock");

    let worker = scope.spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      b_id_tx.send(id.get()).unwrap();
      b_start_rx.recv().unwrap();

      let slot_ptr = slot_ptr as runtime_native::roots::GcHandle;
      unsafe { runtime_native::rt_parallel_for_rooted_h(0, 1, record_ptr, slot_ptr) };

      threading::unregister_current_thread();
    });

    let b_id = b_id_rx
      .recv_timeout(TIMEOUT)
      .expect("thread B should register");

    OBSERVED.store(0, Ordering::SeqCst);
    b_start_tx.send(()).unwrap();

    // Ensure thread B is actively blocked on the persistent handle table lock before mutating the
    // slot. If `rt_parallel_for_rooted_h` incorrectly reads the pointer before acquiring the lock,
    // it would observe the old value even though we update the slot below.
    wait_until_native_safe(b_id);

    slot_value = new_value;
    a_release_tx.send(()).unwrap();

    worker.join().unwrap();
  });

  assert_eq!(OBSERVED.load(Ordering::SeqCst), new_value as usize);
  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    0,
    "rt_parallel_for_rooted_h must release its persistent handle after returning"
  );

  threading::unregister_current_thread();
}

