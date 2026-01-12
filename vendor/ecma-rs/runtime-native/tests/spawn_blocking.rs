use runtime_native::abi::PromiseRef;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::RootStack;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::gc::TypeDescriptor;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::{
  rt_async_poll_legacy as rt_async_poll,
  rt_async_sleep_legacy as rt_async_sleep,
  rt_promise_resolve_legacy as rt_promise_resolve,
  rt_promise_then_legacy as rt_promise_then,
  rt_spawn_blocking,
  rt_spawn_blocking_rooted,
  rt_spawn_blocking_rooted_h,
  GcHeap,
};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

extern "C" fn task_set_flag(data: *mut u8, promise: PromiseRef) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
  rt_promise_resolve(promise, core::ptr::null_mut());
}

extern "C" fn task_sleep_and_inc(data: *mut u8, promise: PromiseRef) {
  std::thread::sleep(Duration::from_millis(20));
  let counter = unsafe { &*(data as *const AtomicUsize) };
  counter.fetch_add(1, Ordering::SeqCst);
  rt_promise_resolve(promise, core::ptr::null_mut());
}

extern "C" fn task_long_sleep_and_inc(data: *mut u8, promise: PromiseRef) {
  std::thread::sleep(Duration::from_millis(300));
  let counter = unsafe { &*(data as *const AtomicUsize) };
  counter.fetch_add(1, Ordering::SeqCst);
  rt_promise_resolve(promise, core::ptr::null_mut());
}

extern "C" fn set_bool(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

extern "C" fn inc_usize(data: *mut u8) {
  let counter = unsafe { &*(data as *const AtomicUsize) };
  counter.fetch_add(1, Ordering::SeqCst);
}

#[repr(C)]
struct BlockerState {
  release: AtomicBool,
  finished: AtomicUsize,
}

extern "C" fn blocker_wait(data: *mut u8, promise: PromiseRef) {
  let st = unsafe { &*(data as *const BlockerState) };
  while !st.release.load(Ordering::Acquire) {
    std::thread::sleep(Duration::from_millis(1));
  }
  st.finished.fetch_add(1, Ordering::SeqCst);
  rt_promise_resolve(promise, core::ptr::null_mut());
}

static ROOTED_H_BLOCKING_STARTED: AtomicBool = AtomicBool::new(false);
static ROOTED_H_BLOCKING_RELEASE: AtomicBool = AtomicBool::new(false);
static ROOTED_H_BLOCKING_PTR: AtomicUsize = AtomicUsize::new(0);

extern "C" fn rooted_h_record_ptr_and_block(data: *mut u8, promise: PromiseRef) {
  ROOTED_H_BLOCKING_PTR.store(data as usize, Ordering::SeqCst);
  ROOTED_H_BLOCKING_STARTED.store(true, Ordering::SeqCst);
  while !ROOTED_H_BLOCKING_RELEASE.load(Ordering::Acquire) {
    std::thread::yield_now();
  }
  rt_promise_resolve(promise, core::ptr::null_mut());
}

#[repr(C)]
struct BoxedUsize {
  header: ObjHeader,
  value: usize,
}

static NO_PTR_OFFSETS: [u32; 0] = [];
static BOXED_USIZE_DESC: TypeDescriptor =
  TypeDescriptor::new(std::mem::size_of::<BoxedUsize>(), &NO_PTR_OFFSETS);

extern "C" fn rooted_set_value(data: *mut u8, promise: PromiseRef) {
  let obj = unsafe { &mut *(data as *mut BoxedUsize) };
  obj.value = 999;
  rt_promise_resolve(promise, core::ptr::null_mut());
}

#[test]
fn spawn_blocking_basic() {
  let _rt = TestRuntimeGuard::new();
  let flag = Box::new(AtomicBool::new(false));
  let settled = Box::new(AtomicBool::new(false));
  let flag_ptr = Box::into_raw(flag);
  let settled_ptr = Box::into_raw(settled);

  let promise = rt_spawn_blocking(task_set_flag, flag_ptr.cast::<u8>());
  rt_promise_then(promise, set_bool, settled_ptr.cast::<u8>());

  let start = Instant::now();
  while !unsafe { &*settled_ptr }.load(Ordering::SeqCst) {
    rt_async_poll();
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for spawn_blocking promise to settle"
    );
  }

  let flag = unsafe { &*flag_ptr };
  assert!(flag.load(Ordering::SeqCst));

  unsafe {
    drop(Box::from_raw(flag_ptr));
    drop(Box::from_raw(settled_ptr));
  }
}

#[test]
fn spawn_blocking_concurrency() {
  let _rt = TestRuntimeGuard::new();
  const N: usize = 64;

  let counter = Box::new(AtomicUsize::new(0));
  let settled = Box::new(AtomicUsize::new(0));
  let counter_ptr = Box::into_raw(counter);
  let settled_ptr = Box::into_raw(settled);

  for _ in 0..N {
    let p = rt_spawn_blocking(task_sleep_and_inc, counter_ptr.cast::<u8>());
    rt_promise_then(p, inc_usize, settled_ptr.cast::<u8>());
  }

  let start = Instant::now();
  while unsafe { &*settled_ptr }.load(Ordering::SeqCst) != N {
    rt_async_poll();
    assert!(
      start.elapsed() < Duration::from_secs(10),
      "timeout waiting for {N} spawn_blocking tasks to settle"
    );
  }

  let counter = unsafe { &*counter_ptr };
  assert_eq!(counter.load(Ordering::SeqCst), N);

  unsafe {
    drop(Box::from_raw(counter_ptr));
    drop(Box::from_raw(settled_ptr));
  }
}

#[test]
fn spawn_blocking_does_not_block_event_loop() {
  let _rt = TestRuntimeGuard::new();
  const N: usize = 16;

  let start = Instant::now();
  let timer_promise = rt_async_sleep(50);
  let timer_settled = Box::new(AtomicBool::new(false));
  let timer_settled_ptr = Box::into_raw(timer_settled);
  rt_promise_then(timer_promise, set_bool, timer_settled_ptr.cast::<u8>());

  let counter = Box::new(AtomicUsize::new(0));
  let settled = Box::new(AtomicUsize::new(0));
  let counter_ptr = Box::into_raw(counter);
  let settled_ptr = Box::into_raw(settled);

  for _ in 0..N {
    let p = rt_spawn_blocking(task_long_sleep_and_inc, counter_ptr.cast::<u8>());
    rt_promise_then(p, inc_usize, settled_ptr.cast::<u8>());
  }

  let mut timer_fired_at = None;
  let mut counter_at_timer = 0;

  while timer_fired_at.is_none() || unsafe { &*settled_ptr }.load(Ordering::SeqCst) != N {
    rt_async_poll();
    if timer_fired_at.is_none() && unsafe { &*timer_settled_ptr }.load(Ordering::SeqCst) {
      timer_fired_at = Some(start.elapsed());
      counter_at_timer = unsafe { &*settled_ptr }.load(Ordering::SeqCst);
    }
    assert!(
      start.elapsed() < Duration::from_secs(15),
      "timeout waiting for timer + spawn_blocking tasks to complete"
    );
  }

  let fired = timer_fired_at.expect("timer should have fired");

  // The timer should not be delayed until *after* all blocking tasks complete.
  assert!(counter_at_timer < N, "timer fired only after all blocking tasks completed");

  // Also keep a loose upper bound to catch regressions where the event loop is actually blocked.
  assert!(fired < Duration::from_secs(2), "timer fired too late: {fired:?}");

  let counter = unsafe { &*counter_ptr };
  assert_eq!(counter.load(Ordering::SeqCst), N);

  unsafe {
    drop(Box::from_raw(counter_ptr));
    drop(Box::from_raw(settled_ptr));
    drop(Box::from_raw(timer_settled_ptr));
  }
}

#[test]
fn spawn_blocking_rooted_keeps_gc_data_alive_across_minor_gc() {
  let _rt = TestRuntimeGuard::new();

  // Ensure the rooted work item stays queued while we trigger GC: saturate the pool with blocking
  // tasks that wait on an atomic flag.
  const BLOCKERS: usize = 64;
  let blocker_state = Box::new(BlockerState {
    release: AtomicBool::new(false),
    finished: AtomicUsize::new(0),
  });
  let blocker_ptr = (&*blocker_state as *const BlockerState).cast_mut().cast::<u8>();
  for _ in 0..BLOCKERS {
    let _ = rt_spawn_blocking(blocker_wait, blocker_ptr);
  }

  // Allocate a nursery object that is only kept alive by the rooted blocking work item.
  let mut heap = GcHeap::new();
  let obj = heap.alloc_young(&BOXED_USIZE_DESC);
  unsafe {
    (*(obj as *mut BoxedUsize)).value = 41;
  }
  let handle = heap.weak_add(obj);

  let p = rt_spawn_blocking_rooted(rooted_set_value, obj);

  // Trigger a minor GC while the rooted work item is still queued. The runtime root registry must
  // keep `obj` alive and update the root slot to the evacuated address.
  let mut roots = RootStack::new();
  let mut remembered = SimpleRememberedSet::new();
  heap
    .collect_minor(&mut roots, &mut remembered)
    .expect("minor GC should succeed");

  let moved = heap
    .weak_get(handle)
    .expect("rooted spawn_blocking work item should keep data alive across GC");
  assert_ne!(moved, obj);
  assert!(!heap.is_in_nursery(moved));

  let value_before = unsafe { (*(moved as *const BoxedUsize)).value };
  assert_eq!(value_before, 41);

  // Release the pool so it can run the rooted task and settle the promise.
  let settled = Box::new(AtomicBool::new(false));
  let settled_ptr = (&*settled as *const AtomicBool).cast_mut().cast::<u8>();
  rt_promise_then(p, set_bool, settled_ptr);

  blocker_state.release.store(true, Ordering::Release);

  let start = Instant::now();
  while !settled.load(Ordering::SeqCst) {
    rt_async_poll();
    assert!(
      start.elapsed() < Duration::from_secs(10),
      "timeout waiting for rooted spawn_blocking promise to settle"
    );
  }

  let after = heap
    .weak_get(handle)
    .expect("object should remain alive after rooted blocking task completes");
  let value_after = unsafe { (*(after as *const BoxedUsize)).value };
  assert_eq!(value_after, 999);

  // Ensure all blocker tasks have finished before dropping the shared state.
  let start = Instant::now();
  while blocker_state.finished.load(Ordering::SeqCst) != BLOCKERS {
    std::thread::yield_now();
    assert!(
      start.elapsed() < Duration::from_secs(10),
      "timeout waiting for blocker tasks to finish"
    );
  }
}

#[test]
fn spawn_blocking_rooted_h_reads_slot_after_lock_acquired() {
  let _rt = TestRuntimeGuard::new();

  // Ensure the current thread claims the event-loop identity so the worker thread below registers
  // as `External` rather than becoming the event loop.
  let _ = rt_async_poll();

  ROOTED_H_BLOCKING_STARTED.store(false, Ordering::SeqCst);
  ROOTED_H_BLOCKING_PTR.store(0, Ordering::SeqCst);
  ROOTED_H_BLOCKING_RELEASE.store(false, Ordering::Release);

  struct ReleaseOnDrop;
  impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
      ROOTED_H_BLOCKING_RELEASE.store(true, Ordering::Release);
    }
  }
  let _release_on_drop = ReleaseOnDrop;

  let base_roots = runtime_native::roots::global_persistent_handle_table().live_count();

  // Pointers are treated as opaque addresses; they do not need to be dereferenceable in this test.
  let mut slot_value: *mut u8 = 0x1111usize as *mut u8;
  let new_value: *mut u8 = 0x2222usize as *mut u8;
  // Raw pointers are `!Send` on newer Rust versions; pass as an integer across threads.
  let slot_ptr: usize = (&mut slot_value as *mut *mut u8) as usize;

  const TIMEOUT: Duration = Duration::from_secs(2);

  let promise = std::thread::scope(|scope| {
    // Thread A holds the persistent handle table lock.
    let (a_locked_tx, a_locked_rx) = mpsc::channel::<()>();
    let (a_release_tx, a_release_rx) = mpsc::channel::<()>();

    // Thread C attempts to spawn rooted-h work while the lock is held.
    let (c_registered_tx, c_registered_rx) = mpsc::channel::<threading::ThreadId>();
    let (c_start_tx, c_start_rx) = mpsc::channel::<()>();
    let (c_done_tx, c_done_rx) = mpsc::channel::<PromiseRef>();

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
      let promise = unsafe { rt_spawn_blocking_rooted_h(rooted_h_record_ptr_and_block, slot_ptr) };
      c_done_tx.send(promise).unwrap();

      threading::unregister_current_thread();
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

    // Update the slot while thread C is blocked. If `rt_spawn_blocking_rooted_h` (or its internal
    // plumbing) incorrectly reads the slot before acquiring the lock, it would still observe the
    // old value.
    slot_value = new_value;

    // Release the lock so `alloc_from_slot` can proceed and read the updated slot value.
    a_release_tx.send(()).unwrap();

    c_done_rx
      .recv_timeout(TIMEOUT)
      .expect("spawn_blocking rooted_h should complete after lock is released")
  });

  let settled = Box::new(AtomicBool::new(false));
  let settled_ptr = (&*settled as *const AtomicBool).cast_mut().cast::<u8>();
  rt_promise_then(promise, set_bool, settled_ptr);

  // Wait for the worker task to start so we know it has observed the rooted pointer.
  let deadline = Instant::now() + Duration::from_secs(2);
  while !ROOTED_H_BLOCKING_STARTED.load(Ordering::SeqCst) {
    assert!(Instant::now() < deadline, "timeout waiting for rooted_h spawn_blocking task to start");
    std::thread::yield_now();
  }

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots + 1,
    "rooted-h spawn_blocking should allocate exactly one persistent handle while task is pending"
  );

  // Release the blocking task so it can settle the promise and free its persistent handle.
  ROOTED_H_BLOCKING_RELEASE.store(true, Ordering::Release);

  let start = Instant::now();
  while !settled.load(Ordering::SeqCst) {
    rt_async_poll();
    assert!(
      start.elapsed() < Duration::from_secs(5),
      "timeout waiting for rooted_h spawn_blocking promise to settle"
    );
  }

  assert_eq!(
    ROOTED_H_BLOCKING_PTR.load(Ordering::SeqCst),
    new_value as usize,
    "spawn_blocking rooted_h task must observe the slot value read after lock acquisition"
  );

  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().live_count(),
    base_roots,
    "rooted-h spawn_blocking should release its persistent handle after the task completes"
  );
}
