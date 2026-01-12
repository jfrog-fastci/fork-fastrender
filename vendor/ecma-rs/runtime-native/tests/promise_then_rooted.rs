use runtime_native::abi::{RtShapeDescriptor, RtShapeId};
use runtime_native::gc::ObjHeader;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use std::mem;
use std::sync::mpsc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[repr(C)]
struct GcBox<T> {
  header: ObjHeader,
  payload: T,
}

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
      size: mem::size_of::<GcBox<u8>>() as u32,
      align: 16,
      flags: 0,
      ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
      ptr_offsets_len: 0,
      reserved: 0,
    }];
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

static FIRED: AtomicUsize = AtomicUsize::new(0);
static OBSERVED_DATA: AtomicUsize = AtomicUsize::new(0);
static PARALLEL_TASK_RELEASE: AtomicBool = AtomicBool::new(false);
static INTERVAL_TIMER_ID: AtomicU64 = AtomicU64::new(0);

extern "C" fn on_settle(data: *mut u8) {
  FIRED.fetch_add(1, Ordering::SeqCst);
  OBSERVED_DATA.store(data as usize, Ordering::SeqCst);
}

extern "C" fn on_parallel_task(data: *mut u8) {
  OBSERVED_DATA.store(data as usize, Ordering::SeqCst);
  while !PARALLEL_TASK_RELEASE.load(Ordering::Acquire) {
    std::thread::yield_now();
  }
  FIRED.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn on_interval(data: *mut u8) {
  OBSERVED_DATA.store(data as usize, Ordering::SeqCst);
  FIRED.fetch_add(1, Ordering::SeqCst);

  // Cancel the interval after the first tick so the test doesn't spin forever.
  let id = INTERVAL_TIMER_ID.load(Ordering::SeqCst);
  runtime_native::rt_clear_timer(id);
}

#[test]
fn promise_then_rooted_legacy_roots_data_until_invoked() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_thread_init(0);
  ensure_shape_table();

  FIRED.store(0, Ordering::SeqCst);
  OBSERVED_DATA.store(0, Ordering::SeqCst);

  // Allocate a pinned GC object so we can safely pass its base pointer into the rooted-then ABI.
  let shape = RtShapeId(1);
  let data = runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<u8>>(), shape);

  let base_roots = runtime_native::roots::global_persistent_handle_table().live_count();

  let promise = runtime_native::rt_promise_new_legacy();
  runtime_native::rt_promise_then_rooted_legacy(promise, on_settle, data);

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots + 1,
    "rooted promise then should allocate exactly one persistent handle for `data` while pending"
  );

  runtime_native::rt_promise_resolve_legacy(promise, core::ptr::null_mut());
  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(FIRED.load(Ordering::SeqCst), 1, "callback should fire exactly once");
  assert_eq!(
    OBSERVED_DATA.load(Ordering::SeqCst),
    data as usize,
    "callback should receive the rooted GC base pointer"
  );

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots,
    "rooted promise then should release its persistent handle after the callback runs"
  );

  runtime_native::rt_thread_deinit();
}

#[test]
fn promise_then_rooted_h_legacy_reads_slot_after_lock_acquired() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_thread_init(0);
  ensure_shape_table();

  FIRED.store(0, Ordering::SeqCst);
  OBSERVED_DATA.store(0, Ordering::SeqCst);

  // Allocate pinned GC objects so we can safely pass their base pointers through the rooted-h ABI.
  let shape = RtShapeId(1);
  let old_ptr = runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<u8>>(), shape);
  let new_ptr = runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<u8>>(), shape);

  let promise = runtime_native::rt_promise_new_legacy();

  let base_roots = runtime_native::roots::global_persistent_handle_table().live_count();

  // Slot is a GC-handle (pointer-to-slot) so a moving GC can update it in-place.
  let mut slot = old_ptr;
  // Raw pointers are `!Send` on newer Rust versions; pass as an integer across threads.
  let slot_ptr: usize = runtime_native::roots::handle_from_slot(&mut slot) as usize;

  const TIMEOUT: Duration = Duration::from_secs(2);

  std::thread::scope(|scope| {
    // Thread A holds the persistent handle table lock.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

    // Thread C attempts to register a rooted-h reaction while the lock is held.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<runtime_native::threading::ThreadId>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<()>();

    scope.spawn(move || {
      runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Worker);
      runtime_native::roots::global_persistent_handle_table().debug_with_read_lock_for_tests(|| {
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
      });
      runtime_native::threading::unregister_current_thread();
    });

    a_locked_rx
      .recv_timeout(TIMEOUT)
      .expect("thread A should acquire the persistent handle table lock");

    scope.spawn(move || {
      let id = runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Worker);
      c_registered_tx.send(id).unwrap();

      c_start_rx.recv().unwrap();

      let slot_ptr = slot_ptr as runtime_native::roots::GcHandle;
      // Safety: `slot_ptr` points at a writable `GcPtr` slot that outlives this call.
      unsafe {
        runtime_native::rt_promise_then_rooted_h_legacy(promise, on_settle, slot_ptr);
      }
      c_done_tx.send(()).unwrap();

      runtime_native::threading::unregister_current_thread();
    });

    let c_id = c_registered_rx
      .recv_timeout(TIMEOUT)
      .expect("thread C should register with the thread registry");

    // Start thread C's registration attempt (it should block on the handle table lock).
    c_start_tx.send(()).unwrap();

    // Wait until thread C is marked NativeSafe (meaning it's blocked on the GC-aware lock).
    let start = Instant::now();
    loop {
      let mut native_safe = false;
      runtime_native::threading::registry::for_each_thread(|t| {
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

    // Update the slot while thread C is blocked. If `rt_promise_then_rooted_h_legacy` (or its
    // internal plumbing) incorrectly reads the slot before acquiring the lock, it would still
    // observe `old_ptr`.
    slot = new_ptr;

    // Release the lock so `alloc_from_slot` can proceed and read the updated slot value.
    a_release_tx.send(()).unwrap();

    c_done_rx
      .recv_timeout(TIMEOUT)
      .expect("promise then registration should complete after lock is released");
  });

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots + 1,
    "rooted-h promise then should allocate exactly one persistent handle for the slot contents while pending"
  );

  runtime_native::rt_promise_resolve_legacy(promise, core::ptr::null_mut());
  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(FIRED.load(Ordering::SeqCst), 1, "callback should fire exactly once");
  assert_eq!(
    OBSERVED_DATA.load(Ordering::SeqCst),
    new_ptr as usize,
    "callback should observe the slot value that was read after lock acquisition"
  );

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots,
    "rooted-h promise then should release its persistent handle after the callback runs"
  );

  runtime_native::rt_thread_deinit();
}

#[test]
fn queue_microtask_rooted_h_reads_slot_after_lock_acquired() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_thread_init(0);
  ensure_shape_table();

  FIRED.store(0, Ordering::SeqCst);
  OBSERVED_DATA.store(0, Ordering::SeqCst);

  // Allocate pinned GC objects so we can safely pass their base pointers through the rooted-h ABI.
  let shape = RtShapeId(1);
  let old_ptr = runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<u8>>(), shape);
  let new_ptr = runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<u8>>(), shape);

  let base_roots = runtime_native::roots::global_persistent_handle_table().live_count();

  // Slot is a GC-handle (pointer-to-slot) so a moving GC can update it in-place.
  let mut slot = old_ptr;
  // Raw pointers are `!Send` on newer Rust versions; pass as an integer across threads.
  let slot_ptr: usize = runtime_native::roots::handle_from_slot(&mut slot) as usize;

  const TIMEOUT: Duration = Duration::from_secs(2);

  std::thread::scope(|scope| {
    // Thread A holds the persistent handle table lock.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

    // Thread C attempts to enqueue a rooted-h microtask while the lock is held.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<runtime_native::threading::ThreadId>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<()>();

    scope.spawn(move || {
      runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Worker);
      runtime_native::roots::global_persistent_handle_table().debug_with_read_lock_for_tests(|| {
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
      });
      runtime_native::threading::unregister_current_thread();
    });

    a_locked_rx
      .recv_timeout(TIMEOUT)
      .expect("thread A should acquire the persistent handle table lock");

    scope.spawn(move || {
      let id = runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Worker);
      c_registered_tx.send(id).unwrap();

      c_start_rx.recv().unwrap();

      let slot_ptr = slot_ptr as runtime_native::roots::GcHandle;
      // Safety: `slot_ptr` points at a writable `GcPtr` slot that outlives this call.
      unsafe {
        runtime_native::rt_queue_microtask_rooted_h(on_settle, slot_ptr);
      }
      c_done_tx.send(()).unwrap();

      runtime_native::threading::unregister_current_thread();
    });

    let c_id = c_registered_rx
      .recv_timeout(TIMEOUT)
      .expect("thread C should register with the thread registry");

    // Start thread C's enqueue attempt (it should block on the handle table lock).
    c_start_tx.send(()).unwrap();

    // Wait until thread C is marked NativeSafe (meaning it's blocked on the GC-aware lock).
    let start = Instant::now();
    loop {
      let mut native_safe = false;
      runtime_native::threading::registry::for_each_thread(|t| {
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

    // Update the slot while thread C is blocked. If `rt_queue_microtask_rooted_h` (or its internal
    // plumbing) incorrectly reads the slot before acquiring the lock, it would still observe
    // `old_ptr`.
    slot = new_ptr;

    // Release the lock so `alloc_from_slot` can proceed and read the updated slot value.
    a_release_tx.send(()).unwrap();

    c_done_rx
      .recv_timeout(TIMEOUT)
      .expect("microtask enqueue should complete after lock is released");
  });

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots + 1,
    "rooted-h microtask enqueue should allocate exactly one persistent handle for the slot contents while pending"
  );

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(FIRED.load(Ordering::SeqCst), 1, "callback should fire exactly once");
  assert_eq!(
    OBSERVED_DATA.load(Ordering::SeqCst),
    new_ptr as usize,
    "callback should observe the slot value that was read after lock acquisition"
  );

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots,
    "rooted-h microtask enqueue should release its persistent handle after the callback runs"
  );

  runtime_native::rt_thread_deinit();
}

#[test]
fn set_timeout_rooted_h_reads_slot_after_lock_acquired() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_thread_init(0);
  ensure_shape_table();

  // Ensure the current thread claims the event-loop identity so the worker thread below registers
  // as `External` rather than becoming the event loop.
  let _ = runtime_native::rt_async_poll_legacy();

  FIRED.store(0, Ordering::SeqCst);
  OBSERVED_DATA.store(0, Ordering::SeqCst);

  // Allocate pinned GC objects so we can safely pass their base pointers through the rooted-h ABI.
  let shape = RtShapeId(1);
  let old_ptr = runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<u8>>(), shape);
  let new_ptr = runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<u8>>(), shape);

  let base_roots = runtime_native::roots::global_persistent_handle_table().live_count();

  // Slot is a GC-handle (pointer-to-slot) so a moving GC can update it in-place.
  let mut slot = old_ptr;
  // Raw pointers are `!Send` on newer Rust versions; pass as an integer across threads.
  let slot_ptr: usize = runtime_native::roots::handle_from_slot(&mut slot) as usize;

  const TIMEOUT: Duration = Duration::from_secs(2);

  std::thread::scope(|scope| {
    // Thread A holds the persistent handle table lock.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

    // Thread C attempts to schedule a rooted-h timeout while the lock is held.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<runtime_native::threading::ThreadId>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<runtime_native::abi::TimerId>();

    scope.spawn(move || {
      runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Worker);
      runtime_native::roots::global_persistent_handle_table().debug_with_read_lock_for_tests(|| {
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
      });
      runtime_native::threading::unregister_current_thread();
    });

    a_locked_rx
      .recv_timeout(TIMEOUT)
      .expect("thread A should acquire the persistent handle table lock");

    scope.spawn(move || {
      let id = runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Worker);
      c_registered_tx.send(id).unwrap();

      c_start_rx.recv().unwrap();

      let slot_ptr = slot_ptr as runtime_native::roots::GcHandle;
      // Safety: `slot_ptr` points at a writable `GcPtr` slot that outlives this call.
      let timer = unsafe { runtime_native::rt_set_timeout_rooted_h(on_settle, slot_ptr, 0) };
      c_done_tx.send(timer).unwrap();

      runtime_native::threading::unregister_current_thread();
    });

    let c_id = c_registered_rx
      .recv_timeout(TIMEOUT)
      .expect("thread C should register with the thread registry");

    // Start thread C's schedule attempt (it should block on the handle table lock).
    c_start_tx.send(()).unwrap();

    // Wait until thread C is marked NativeSafe (meaning it's blocked on the GC-aware lock).
    let start = Instant::now();
    loop {
      let mut native_safe = false;
      runtime_native::threading::registry::for_each_thread(|t| {
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

    // Update the slot while thread C is blocked. If `rt_set_timeout_rooted_h` (or its internal
    // plumbing) incorrectly reads the slot before acquiring the lock, it would still observe
    // `old_ptr`.
    slot = new_ptr;

    // Release the lock so `alloc_from_slot` can proceed and read the updated slot value.
    a_release_tx.send(()).unwrap();

    let timer = c_done_rx
      .recv_timeout(TIMEOUT)
      .expect("timer schedule should complete after lock is released");
    assert_ne!(timer, 0);
  });

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots + 1,
    "rooted-h timeout should allocate exactly one persistent handle for the slot contents while pending"
  );

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(FIRED.load(Ordering::SeqCst), 1, "callback should fire exactly once");
  assert_eq!(
    OBSERVED_DATA.load(Ordering::SeqCst),
    new_ptr as usize,
    "callback should observe the slot value that was read after lock acquisition"
  );

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots,
    "rooted-h timeout should release its persistent handle after the callback runs"
  );

  runtime_native::rt_thread_deinit();
}

#[test]
fn set_interval_rooted_h_reads_slot_after_lock_acquired() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_thread_init(0);
  ensure_shape_table();

  // Ensure the current thread claims the event-loop identity so the worker thread below registers
  // as `External` rather than becoming the event loop.
  let _ = runtime_native::rt_async_poll_legacy();

  FIRED.store(0, Ordering::SeqCst);
  OBSERVED_DATA.store(0, Ordering::SeqCst);
  INTERVAL_TIMER_ID.store(0, Ordering::SeqCst);

  // Allocate pinned GC objects so we can safely pass their base pointers through the rooted-h ABI.
  let shape = RtShapeId(1);
  let old_ptr = runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<u8>>(), shape);
  let new_ptr = runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<u8>>(), shape);

  let base_roots = runtime_native::roots::global_persistent_handle_table().live_count();

  // Slot is a GC-handle (pointer-to-slot) so a moving GC can update it in-place.
  let mut slot = old_ptr;
  // Raw pointers are `!Send` on newer Rust versions; pass as an integer across threads.
  let slot_ptr: usize = runtime_native::roots::handle_from_slot(&mut slot) as usize;

  const TIMEOUT: Duration = Duration::from_secs(2);

  let timer_id = std::thread::scope(|scope| {
    // Thread A holds the persistent handle table lock.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

    // Thread C attempts to schedule a rooted-h interval while the lock is held.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<runtime_native::threading::ThreadId>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<runtime_native::abi::TimerId>();

    scope.spawn(move || {
      runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Worker);
      runtime_native::roots::global_persistent_handle_table().debug_with_read_lock_for_tests(|| {
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
      });
      runtime_native::threading::unregister_current_thread();
    });

    a_locked_rx
      .recv_timeout(TIMEOUT)
      .expect("thread A should acquire the persistent handle table lock");

    scope.spawn(move || {
      let id = runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Worker);
      c_registered_tx.send(id).unwrap();

      c_start_rx.recv().unwrap();

      let slot_ptr = slot_ptr as runtime_native::roots::GcHandle;
      // Safety: `slot_ptr` points at a writable `GcPtr` slot that outlives this call.
      let timer = unsafe { runtime_native::rt_set_interval_rooted_h(on_interval, slot_ptr, 0) };
      c_done_tx.send(timer).unwrap();

      runtime_native::threading::unregister_current_thread();
    });

    let c_id = c_registered_rx
      .recv_timeout(TIMEOUT)
      .expect("thread C should register with the thread registry");

    // Start thread C's schedule attempt (it should block on the handle table lock).
    c_start_tx.send(()).unwrap();

    // Wait until thread C is marked NativeSafe (meaning it's blocked on the GC-aware lock).
    let start = Instant::now();
    loop {
      let mut native_safe = false;
      runtime_native::threading::registry::for_each_thread(|t| {
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

    // Update the slot while thread C is blocked. If `rt_set_interval_rooted_h` (or its internal
    // plumbing) incorrectly reads the slot before acquiring the lock, it would still observe
    // `old_ptr`.
    slot = new_ptr;

    // Release the lock so `alloc_from_slot` can proceed and read the updated slot value.
    a_release_tx.send(()).unwrap();

    let timer = c_done_rx
      .recv_timeout(TIMEOUT)
      .expect("interval schedule should complete after lock is released");
    assert_ne!(timer, 0);
    timer
  });

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots + 1,
    "rooted-h interval should allocate exactly one persistent handle for the slot contents while pending"
  );

  INTERVAL_TIMER_ID.store(timer_id, Ordering::SeqCst);

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(FIRED.load(Ordering::SeqCst), 1, "callback should fire exactly once");
  assert_eq!(
    OBSERVED_DATA.load(Ordering::SeqCst),
    new_ptr as usize,
    "callback should observe the slot value that was read after lock acquisition"
  );

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots,
    "rooted-h interval should release its persistent handle after the callback runs"
  );

  runtime_native::rt_thread_deinit();
}

#[test]
fn parallel_spawn_rooted_h_reads_slot_after_lock_acquired() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_thread_init(0);
  ensure_shape_table();

  FIRED.store(0, Ordering::SeqCst);
  OBSERVED_DATA.store(0, Ordering::SeqCst);

  struct ReleaseOnDrop;
  impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
      PARALLEL_TASK_RELEASE.store(true, Ordering::Release);
    }
  }
  let _release_on_drop = ReleaseOnDrop;
  PARALLEL_TASK_RELEASE.store(false, Ordering::Release);

  // Allocate pinned GC objects so we can safely pass their base pointers through the rooted-h ABI.
  let shape = RtShapeId(1);
  let old_ptr = runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<u8>>(), shape);
  let new_ptr = runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<u8>>(), shape);

  let base_roots = runtime_native::roots::global_persistent_handle_table().live_count();

  // Slot is a GC-handle (pointer-to-slot) so a moving GC can update it in-place.
  let mut slot = old_ptr;
  // Raw pointers are `!Send` on newer Rust versions; pass as an integer across threads.
  let slot_ptr: usize = runtime_native::roots::handle_from_slot(&mut slot) as usize;

  const TIMEOUT: Duration = Duration::from_secs(2);

  let task_id = std::thread::scope(|scope| {
    // Thread A holds the persistent handle table lock.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

    // Thread C attempts to spawn a rooted-h parallel task while the lock is held.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<runtime_native::threading::ThreadId>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<runtime_native::TaskId>();

    scope.spawn(move || {
      runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Worker);
      runtime_native::roots::global_persistent_handle_table().debug_with_read_lock_for_tests(|| {
        a_locked_tx.send(()).unwrap();
        a_release_rx.recv().unwrap();
      });
      runtime_native::threading::unregister_current_thread();
    });

    a_locked_rx
      .recv_timeout(TIMEOUT)
      .expect("thread A should acquire the persistent handle table lock");

    scope.spawn(move || {
      let id = runtime_native::threading::register_current_thread(runtime_native::threading::ThreadKind::Worker);
      c_registered_tx.send(id).unwrap();

      c_start_rx.recv().unwrap();

      let slot_ptr = slot_ptr as runtime_native::roots::GcHandle;
      // Safety: `slot_ptr` points at a writable `GcPtr` slot that outlives this call.
      let id = unsafe { runtime_native::rt_parallel_spawn_rooted_h(on_parallel_task, slot_ptr) };
      c_done_tx.send(id).unwrap();

      runtime_native::threading::unregister_current_thread();
    });

    let c_id = c_registered_rx
      .recv_timeout(TIMEOUT)
      .expect("thread C should register with the thread registry");

    // Start thread C's spawn attempt (it should block on the handle table lock).
    c_start_tx.send(()).unwrap();

    // Wait until thread C is marked NativeSafe (meaning it's blocked on the GC-aware lock).
    let start = Instant::now();
    loop {
      let mut native_safe = false;
      runtime_native::threading::registry::for_each_thread(|t| {
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

    // Update the slot while thread C is blocked. If `rt_parallel_spawn_rooted_h` (or its internal
    // plumbing) incorrectly reads the slot before acquiring the lock, it would still observe
    // `old_ptr`.
    slot = new_ptr;

    // Release the lock so `alloc_from_slot` can proceed and read the updated slot value.
    a_release_tx.send(()).unwrap();

    c_done_rx
      .recv_timeout(TIMEOUT)
      .expect("task spawn should complete after lock is released")
  });

  assert_ne!(task_id.0, 0);

  let pending_roots = runtime_native::roots::global_persistent_handle_table().live_count();

  // Release the task callback so the worker can complete and free its persistent handle.
  PARALLEL_TASK_RELEASE.store(true, Ordering::Release);
  runtime_native::rt_parallel_join(&task_id as *const runtime_native::TaskId, 1);

  assert_eq!(
    pending_roots,
    base_roots + 1,
    "rooted-h parallel spawn should allocate exactly one persistent handle for the slot contents while pending"
  );

  assert_eq!(FIRED.load(Ordering::SeqCst), 1, "task should run exactly once");
  assert_eq!(
    OBSERVED_DATA.load(Ordering::SeqCst),
    new_ptr as usize,
    "task should observe the slot value that was read after lock acquisition"
  );

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots,
    "rooted-h parallel spawn should release its persistent handle after the task completes"
  );

  runtime_native::rt_thread_deinit();
}
