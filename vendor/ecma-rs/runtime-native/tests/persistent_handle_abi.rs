use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[test]
fn persistent_handle_abi_roundtrip() {
  let _rt = TestRuntimeGuard::new();

  let a = Box::into_raw(Box::new(1u8)) as *mut u8;
  let b = Box::into_raw(Box::new(2u8)) as *mut u8;

  let h = runtime_native::rt_handle_alloc(a);
  assert_eq!(runtime_native::rt_handle_load(h), a);

  runtime_native::rt_handle_store(h, b);
  assert_eq!(runtime_native::rt_handle_load(h), b);

  runtime_native::rt_handle_free(h);
  assert_eq!(runtime_native::rt_handle_load(h), std::ptr::null_mut());
  // Double-free should be a no-op.
  runtime_native::rt_handle_free(h);

  unsafe {
    drop(Box::from_raw(a));
    drop(Box::from_raw(b));
  }
}

#[test]
fn persistent_handle_alloc_h_reads_slot_after_lock_acquired() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let base_roots = runtime_native::roots::global_persistent_handle_table().live_count();

  // Pointers are treated as opaque addresses; they do not need to be dereferenceable in this test.
  let mut slot_value: *mut u8 = 0x1111usize as *mut u8;
  let new_value: *mut u8 = 0x2222usize as *mut u8;
  // Raw pointers are `!Send` on newer Rust versions; pass as an integer across threads.
  let slot_ptr: usize = (&mut slot_value as *mut *mut u8) as usize;

  const TIMEOUT: Duration = Duration::from_secs(2);

  let handle = std::thread::scope(|scope| {
    // Thread A holds the persistent handle table lock.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

    // Thread C attempts to allocate from a slot while the lock is held.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<u64>();

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
      .expect("thread A should acquire the persistent handle table lock");

    scope.spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      c_registered_tx.send(id).unwrap();

      c_start_rx.recv().unwrap();

      let slot_ptr = slot_ptr as runtime_native::roots::GcHandle;
      // Safety: `slot_ptr` is a valid slot pointer.
      let handle = unsafe { runtime_native::rt_handle_alloc_h(slot_ptr) };
      c_done_tx.send(handle).unwrap();

      threading::unregister_current_thread();
    });

    let c_id = c_registered_rx
      .recv_timeout(TIMEOUT)
      .expect("thread C should register with the thread registry");

    // Start thread C's allocation attempt (it should block on the handle table lock).
    c_start_tx.send(()).unwrap();

    // Wait until thread C is marked NativeSafe (meaning it's blocked on the GC-aware lock).
    let start = Instant::now();
    loop {
      let mut native_safe = false;
      threading::registry::for_each_thread(|t| {
        if t.id() == c_id {
          native_safe = t.is_native_safe();
        }
      });

      if native_safe {
        break;
      }
      if start.elapsed() > TIMEOUT {
        panic!("thread C did not enter a GC-safe region while blocked on the persistent handle table lock");
      }
      std::thread::yield_now();
    }

    // Update the slot while thread C is blocked. If `rt_handle_alloc_h` incorrectly read the slot
    // before acquiring the lock, it would still observe the old value.
    slot_value = new_value;

    // Release the lock so `alloc_from_slot` can proceed and read the updated slot value.
    a_release_tx.send(()).unwrap();

    c_done_rx
      .recv_timeout(TIMEOUT)
      .expect("handle allocation should complete after lock is released")
  });

  assert_ne!(handle, 0);
  assert_eq!(runtime_native::rt_handle_load(handle), new_value);
  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots + 1,
    "rt_handle_alloc_h should allocate exactly one persistent handle"
  );

  runtime_native::rt_handle_free(handle);
  assert_eq!(runtime_native::rt_handle_load(handle), std::ptr::null_mut());
  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots,
    "rt_handle_free should release the persistent handle"
  );

  threading::unregister_current_thread();
}

#[test]
fn persistent_handle_store_h_reads_slot_after_lock_acquired() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let base_roots = runtime_native::roots::global_persistent_handle_table().live_count();

  // Pointers are treated as opaque addresses; they do not need to be dereferenceable in this test.
  let old_value: *mut u8 = 0x1111usize as *mut u8;
  let mut slot_value: *mut u8 = old_value;
  let new_value: *mut u8 = 0x2222usize as *mut u8;
  // Raw pointers are `!Send` on newer Rust versions; pass as an integer across threads.
  let slot_ptr: usize = (&mut slot_value as *mut *mut u8) as usize;

  let handle = runtime_native::rt_handle_alloc(old_value);
  assert_ne!(handle, 0);

  const TIMEOUT: Duration = Duration::from_secs(2);

  std::thread::scope(|scope| {
    // Thread A holds the persistent handle table lock.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

    // Thread C attempts to update from a slot while the lock is held.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<()>();

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
      .expect("thread A should acquire the persistent handle table lock");

    scope.spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      c_registered_tx.send(id).unwrap();

      c_start_rx.recv().unwrap();

      let slot_ptr = slot_ptr as runtime_native::roots::GcHandle;
      // Safety: `slot_ptr` is a valid slot pointer.
      unsafe {
        runtime_native::rt_handle_store_h(handle, slot_ptr);
      }
      c_done_tx.send(()).unwrap();

      threading::unregister_current_thread();
    });

    let c_id = c_registered_rx
      .recv_timeout(TIMEOUT)
      .expect("thread C should register with the thread registry");

    // Start thread C's update attempt (it should block on the handle table lock).
    c_start_tx.send(()).unwrap();

    // Wait until thread C is marked NativeSafe (meaning it's blocked on the GC-aware lock).
    let start = Instant::now();
    loop {
      let mut native_safe = false;
      threading::registry::for_each_thread(|t| {
        if t.id() == c_id {
          native_safe = t.is_native_safe();
        }
      });

      if native_safe {
        break;
      }
      if start.elapsed() > TIMEOUT {
        panic!("thread C did not enter a GC-safe region while blocked on the persistent handle table lock");
      }
      std::thread::yield_now();
    }

    // Update the slot while thread C is blocked. If `rt_handle_store_h` incorrectly read the slot
    // before acquiring the lock, it would still observe the old value.
    slot_value = new_value;

    // Release the lock so `set_from_slot` can proceed and read the updated slot value.
    a_release_tx.send(()).unwrap();

    c_done_rx
      .recv_timeout(TIMEOUT)
      .expect("handle store should complete after lock is released");
  });

  assert_eq!(runtime_native::rt_handle_load(handle), new_value);
  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots + 1,
    "rt_handle_store_h should not allocate or free persistent handles"
  );

  runtime_native::rt_handle_free(handle);
  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots,
    "rt_handle_free should release the persistent handle"
  );

  threading::unregister_current_thread();
}
