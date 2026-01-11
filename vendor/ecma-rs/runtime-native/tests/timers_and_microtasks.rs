use runtime_native::{rt_async_poll_legacy as rt_async_poll, rt_clear_timer, rt_queue_microtask, rt_set_interval, rt_set_timeout};
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn microtask_ordering() {
  let _rt = TestRuntimeGuard::new();

  #[derive(Clone)]
  struct Shared(Arc<Mutex<Vec<&'static str>>>);

  extern "C" fn microtask_b(data: *mut u8) {
    // SAFETY: owned by this microtask invocation.
    let shared: Box<Shared> = unsafe { Box::from_raw(data.cast()) };
    shared.0.lock().unwrap().push("B");
  }

  extern "C" fn microtask_a(data: *mut u8) {
    // SAFETY: owned by this microtask invocation.
    let shared: Box<Shared> = unsafe { Box::from_raw(data.cast()) };
    shared.0.lock().unwrap().push("A");

    let b_data = Box::new(shared.as_ref().clone());
    rt_queue_microtask(microtask_b, Box::into_raw(b_data).cast());
  }

  let shared = Shared(Arc::new(Mutex::new(Vec::new())));
  rt_queue_microtask(microtask_a, Box::into_raw(Box::new(shared.clone())).cast());

  rt_async_poll();
  assert_eq!(&*shared.0.lock().unwrap(), &["A", "B"]);
}

#[test]
fn timeout_runs_microtask_checkpoint_after_macrotask() {
  let _rt = TestRuntimeGuard::new();

  #[derive(Clone)]
  struct Shared(Arc<Mutex<Vec<&'static str>>>);

  extern "C" fn microtask(data: *mut u8) {
    // SAFETY: owned by this microtask invocation.
    let shared: Box<Shared> = unsafe { Box::from_raw(data.cast()) };
    shared.0.lock().unwrap().push("microtask");
  }

  extern "C" fn timeout_cb(data: *mut u8) {
    // SAFETY: owned by this timer invocation.
    let shared: Box<Shared> = unsafe { Box::from_raw(data.cast()) };
    shared.0.lock().unwrap().push("timeout");

    let mt_data = Box::new(shared.as_ref().clone());
    rt_queue_microtask(microtask, Box::into_raw(mt_data).cast());
  }

  let shared = Shared(Arc::new(Mutex::new(Vec::new())));
  let timeout_data = Box::new(shared.clone());
  let _id = rt_set_timeout(timeout_cb, Box::into_raw(timeout_data).cast(), 0);

  rt_async_poll();
  assert_eq!(&*shared.0.lock().unwrap(), &["timeout", "microtask"]);
}

#[test]
fn interval_fires_and_can_be_cleared() {
  let _rt = TestRuntimeGuard::new();

  const N: usize = 3;
  let counter: *mut AtomicUsize = Box::into_raw(Box::new(AtomicUsize::new(0)));

  extern "C" fn interval_cb(data: *mut u8) {
    // SAFETY: `data` is a stable pointer for the lifetime of the interval.
    let counter: &AtomicUsize = unsafe { &*data.cast::<AtomicUsize>() };
    counter.fetch_add(1, Ordering::SeqCst);
  }

  let id = rt_set_interval(interval_cb, counter.cast::<u8>(), 1);

  while unsafe { &*counter }.load(Ordering::SeqCst) < N {
    rt_async_poll();
  }

  rt_clear_timer(id);

  // If the interval was not cleared correctly, calling `rt_async_poll` again should eventually
  // run another timer callback and increment the counter.
  rt_async_poll();

  assert_eq!(unsafe { &*counter }.load(Ordering::SeqCst), N);

  // SAFETY: interval has been cleared and we've verified it didn't fire again.
  unsafe {
    drop(Box::from_raw(counter));
  }
}

#[test]
fn thread_safe_microtask_wake_from_epoll() {
  let _rt = TestRuntimeGuard::new();

  let ran = Arc::new(AtomicBool::new(false));
  let timeout_ran = Arc::new(AtomicBool::new(false));

  extern "C" fn mark_ran(data: *mut u8) {
    // SAFETY: owned by this microtask invocation.
    let ran: Box<Arc<AtomicBool>> = unsafe { Box::from_raw(data.cast()) };
    ran.store(true, Ordering::SeqCst);
  }

  extern "C" fn mark_timeout(data: *mut u8) {
    // SAFETY: points to an `AtomicBool` that outlives this test.
    let ran: &AtomicBool = unsafe { &*data.cast::<AtomicBool>() };
    ran.store(true, Ordering::SeqCst);
  }

  // Ensure `rt_async_poll` blocks in `epoll_wait` by registering a far-future timer.
  let _timer_id = rt_set_timeout(
    mark_timeout,
    Arc::as_ptr(&timeout_ran).cast_mut().cast(),
    500,
  );

  let started = Arc::new(AtomicBool::new(false));
  let returned = Arc::new(AtomicBool::new(false));

  let poll_started = started.clone();
  let poll_returned = returned.clone();
  let poll_thread = std::thread::spawn(move || {
    poll_started.store(true, Ordering::SeqCst);
    let _ = rt_async_poll();
    poll_returned.store(true, Ordering::SeqCst);
  });

  while !started.load(Ordering::SeqCst) {
    std::thread::yield_now();
  }

  // Give the poll thread time to enter `epoll_wait`.
  std::thread::sleep(Duration::from_millis(20));
  assert!(
    !returned.load(Ordering::SeqCst),
    "rt_async_poll returned early; expected it to block in epoll_wait"
  );

  rt_queue_microtask(mark_ran, Box::into_raw(Box::new(ran.clone())).cast());

  // Wait for the poll thread to finish.
  while !returned.load(Ordering::SeqCst) {
    std::thread::yield_now();
  }
  poll_thread.join().unwrap();
  assert!(ran.load(Ordering::SeqCst));
  assert!(!timeout_ran.load(Ordering::SeqCst));
}
