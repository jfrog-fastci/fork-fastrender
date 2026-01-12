use runtime_native::abi::LegacyPromiseRef;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_async_poll_legacy as rt_async_poll,
  rt_async_sleep_legacy as rt_async_sleep,
  rt_promise_resolve_legacy as rt_promise_resolve,
  rt_promise_then_legacy as rt_promise_then,
  rt_spawn_blocking,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

extern "C" fn task_set_flag(data: *mut u8, promise: LegacyPromiseRef) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
  rt_promise_resolve(promise, core::ptr::null_mut());
}

extern "C" fn task_sleep_and_inc(data: *mut u8, promise: LegacyPromiseRef) {
  std::thread::sleep(Duration::from_millis(20));
  let counter = unsafe { &*(data as *const AtomicUsize) };
  counter.fetch_add(1, Ordering::SeqCst);
  rt_promise_resolve(promise, core::ptr::null_mut());
}

extern "C" fn task_long_sleep_and_inc(data: *mut u8, promise: LegacyPromiseRef) {
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
