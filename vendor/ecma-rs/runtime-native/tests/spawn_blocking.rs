use runtime_native::abi::PromiseRef;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::RootStack;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::gc::TypeDescriptor;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_async_poll_legacy as rt_async_poll,
  rt_async_sleep_legacy as rt_async_sleep,
  rt_promise_resolve_legacy as rt_promise_resolve,
  rt_promise_then_legacy as rt_promise_then,
  rt_spawn_blocking,
  rt_spawn_blocking_rooted,
  GcHeap,
};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
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
