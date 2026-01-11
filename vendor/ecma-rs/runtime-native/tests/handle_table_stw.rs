use core::ptr::NonNull;
use runtime_native::gc::HandleTable;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

fn wait_until_native_safe(thread_id: u64, timeout: Duration) {
  let deadline = std::time::Instant::now() + timeout;
  loop {
    let thread = threading::all_threads()
      .into_iter()
      .find(|t| t.id().get() == thread_id)
      .expect("worker thread state");
    if thread.is_native_safe() {
      return;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "thread did not enter NativeSafe while blocked on HandleTable lock"
    );
    std::thread::yield_now();
  }
}

#[test]
fn stop_the_world_completes_while_threads_contend_on_handle_table_lock() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  const TIMEOUT: Duration = Duration::from_secs(2);

  let table = Arc::new(HandleTable::<u8>::new());
  let ptr = NonNull::new(Box::into_raw(Box::new(123u8))).unwrap();
  let handle = table.alloc(ptr);

  std::thread::scope(|scope| {
    // Thread A holds the table's write lock inside `with_stw_update`.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

    // Thread B blocks in `get()` while thread A is holding the write lock.
    let (b_id_tx, b_id_rx) = mpsc::channel::<u64>();
    let (b_start_tx, b_start_rx) = mpsc::channel::<()>();
    let (b_done_tx, b_done_rx) = mpsc::channel::<usize>();

    let table_a = Arc::clone(&table);
    scope.spawn(move || {
      threading::register_current_thread(ThreadKind::Worker);
      table_a.with_stw_update(|_| {
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
      });

      // Cooperatively stop at the safepoint request (after releasing the lock).
      runtime_native::rt_gc_safepoint();
      threading::unregister_current_thread();
    });

    a_locked_rx
      .recv_timeout(TIMEOUT)
      .expect("thread A should acquire the handle table lock");

    let table_b = Arc::clone(&table);
    scope.spawn(move || {
      let id = threading::register_current_thread(ThreadKind::Worker);
      b_id_tx.send(id.get()).unwrap();

      b_start_rx.recv().unwrap();

      let got = table_b.get(handle).expect("handle must stay live");
      b_done_tx.send(got.as_ptr() as usize).unwrap();

      runtime_native::rt_gc_safepoint();
      threading::unregister_current_thread();
    });

    let b_id = b_id_rx
      .recv_timeout(TIMEOUT)
      .expect("thread B should register with the thread registry");

    // Ensure thread B is actively contending before requesting STW.
    b_start_tx.send(()).unwrap();
    wait_until_native_safe(b_id, TIMEOUT);

    let stop_epoch = runtime_native::rt_gc_request_stop_the_world();
    assert_eq!(stop_epoch & 1, 1, "stop-the-world epoch must be odd");
    struct ResumeOnDrop;
    impl Drop for ResumeOnDrop {
      fn drop(&mut self) {
        runtime_native::rt_gc_resume_world();
      }
    }
    let _resume = ResumeOnDrop;

    // Allow thread A to release the lock and reach the safepoint.
    a_release_tx.send(()).unwrap();

    assert!(
      runtime_native::rt_gc_wait_for_world_stopped_timeout(TIMEOUT),
      "world failed to stop within timeout; HandleTable lock contention must not block STW"
    );

    // Root enumeration/relocation must be able to take the HandleTable write lock while the world
    // is stopped, even if threads were previously contending on it.
    let (enum_done_tx, enum_done_rx) = mpsc::channel::<()>();
    let watchdog = scope.spawn(move || {
      if enum_done_rx.recv_timeout(TIMEOUT).is_err() {
        runtime_native::rt_gc_resume_world();
        panic!("HandleTable::with_stw_update deadlocked under stop-the-world");
      }
    });
    table.with_stw_update(|stw| {
      let mut saw_handle = false;
      for (id, slot) in stw.iter_live_mut() {
        if id == handle {
          assert_eq!(*slot as usize, ptr.as_ptr() as usize);
          saw_handle = true;
        }
      }
      assert!(saw_handle, "expected to observe the live handle during STW enumeration");
    });
    let _ = enum_done_tx.send(());
    watchdog.join().unwrap();

    // Resume so the blocked `get()` can complete.
    runtime_native::rt_gc_resume_world();

    assert_eq!(
      b_done_rx.recv_timeout(TIMEOUT).unwrap(),
      ptr.as_ptr() as usize,
      "blocked `get` should return the expected pointer after the world is resumed"
    );
  });

  // Cleanup.
  let freed = table.free(handle).unwrap();
  assert_eq!(freed.as_ptr(), ptr.as_ptr());
  unsafe {
    drop(Box::from_raw(ptr.as_ptr()));
  }

  threading::unregister_current_thread();
}

#[test]
fn stw_root_enumeration_can_iterate_handle_table_slots() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  const TIMEOUT: Duration = Duration::from_secs(2);

  let table = Arc::new(HandleTable::<u8>::new());
  let p1 = NonNull::new(Box::into_raw(Box::new(1u8))).unwrap();
  let p2 = NonNull::new(Box::into_raw(Box::new(2u8))).unwrap();
  let id1 = table.alloc(p1);
  let id2 = table.alloc(p2);

  let stop_epoch = runtime_native::rt_gc_request_stop_the_world();
  assert_eq!(stop_epoch & 1, 1);
  struct ResumeOnDrop;
  impl Drop for ResumeOnDrop {
    fn drop(&mut self) {
      runtime_native::rt_gc_resume_world();
    }
  }
  let _resume = ResumeOnDrop;

  assert!(
    runtime_native::rt_gc_wait_for_world_stopped_timeout(TIMEOUT),
    "world failed to stop within timeout"
  );

  // Guard against a deadlock inside `with_stw_update` by running enumeration under a watchdog.
  let (done_tx, done_rx) = mpsc::channel::<()>();
  std::thread::scope(|scope| {
    let watchdog = scope.spawn(move || {
      if done_rx.recv_timeout(TIMEOUT).is_err() {
        runtime_native::rt_gc_resume_world();
        panic!("HandleTable iteration deadlocked under stop-the-world");
      }
    });

    let mut seen_ids = Vec::new();
    let mut seen_ptrs = Vec::new();
    table.with_stw_update(|stw| {
      for (id, slot) in stw.iter_live_mut() {
        seen_ids.push(id.to_u64());
        seen_ptrs.push(*slot as usize);
      }
    });

    assert!(seen_ids.contains(&id1.to_u64()));
    assert!(seen_ids.contains(&id2.to_u64()));
    assert!(seen_ptrs.contains(&(p1.as_ptr() as usize)));
    assert!(seen_ptrs.contains(&(p2.as_ptr() as usize)));

    let _ = done_tx.send(());
    watchdog.join().unwrap();
  });

  runtime_native::rt_gc_resume_world();

  // Cleanup.
  let _ = table.free(id1);
  let _ = table.free(id2);
  unsafe {
    drop(Box::from_raw(p1.as_ptr()));
    drop(Box::from_raw(p2.as_ptr()));
  }

  threading::unregister_current_thread();
}

#[test]
fn stale_handle_ids_do_not_resolve_after_free_and_reuse() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let table = HandleTable::<u8>::new();

  let p1 = NonNull::new(Box::into_raw(Box::new(1u8))).unwrap();
  let id1 = table.alloc(p1);
  assert_eq!(table.get(id1), Some(p1));
  assert!(table.free(id1).is_some());
  unsafe {
    drop(Box::from_raw(p1.as_ptr()));
  }

  let p2 = NonNull::new(Box::into_raw(Box::new(2u8))).unwrap();
  let id2 = table.alloc(p2);

  assert_eq!(
    id1.index(),
    id2.index(),
    "HandleTable should reuse the freed slot first"
  );
  assert_ne!(id1.generation(), id2.generation());

  assert_eq!(table.get(id1), None, "stale handle ID must not resolve");
  assert_eq!(table.get(id2), Some(p2));

  assert!(table.free(id2).is_some());
  unsafe {
    drop(Box::from_raw(p2.as_ptr()));
  }

  threading::unregister_current_thread();
}

