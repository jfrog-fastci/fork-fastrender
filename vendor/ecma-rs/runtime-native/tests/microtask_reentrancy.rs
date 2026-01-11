use runtime_native::test_util::{enqueue_macrotask, enqueue_microtask, set_microtask_checkpoint_end_hook, TestRuntimeGuard};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

struct ReentrancyState {
  inside_a: AtomicBool,
  inner_call_result: AtomicBool,
  b_ran_during_a: AtomicBool,
  b_runs: AtomicUsize,
}

impl ReentrancyState {
  fn new() -> Self {
    Self {
      inside_a: AtomicBool::new(false),
      // Default to `true` so the test fails if the callback never ran.
      inner_call_result: AtomicBool::new(true),
      b_ran_during_a: AtomicBool::new(false),
      b_runs: AtomicUsize::new(0),
    }
  }
}

extern "C" fn microtask_b(data: *mut u8) {
  let st = unsafe { &*(data as *const ReentrancyState) };
  if st.inside_a.load(Ordering::SeqCst) {
    st.b_ran_during_a.store(true, Ordering::SeqCst);
  }
  st.b_runs.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn microtask_a_calls_drain(data: *mut u8) {
  let st = unsafe { &*(data as *const ReentrancyState) };
  st.inside_a.store(true, Ordering::SeqCst);
  enqueue_microtask(microtask_b, data);
  let did_work = runtime_native::rt_drain_microtasks();
  st.inner_call_result.store(did_work, Ordering::SeqCst);
  st.inside_a.store(false, Ordering::SeqCst);
}

extern "C" fn macrotask_a_calls_run_until_idle(data: *mut u8) {
  let st = unsafe { &*(data as *const ReentrancyState) };
  st.inside_a.store(true, Ordering::SeqCst);
  enqueue_microtask(microtask_b, data);
  let did_work = runtime_native::rt_async_run_until_idle();
  st.inner_call_result.store(did_work, Ordering::SeqCst);
  st.inside_a.store(false, Ordering::SeqCst);
}

#[test]
fn drain_microtasks_is_non_reentrant() {
  let _rt = TestRuntimeGuard::new();

  let hook_calls = Arc::new(AtomicUsize::new(0));
  set_microtask_checkpoint_end_hook(Some(Box::new({
    let hook_calls = hook_calls.clone();
    move || {
      hook_calls.fetch_add(1, Ordering::SeqCst);
    }
  })));

  let st: &'static ReentrancyState = Box::leak(Box::new(ReentrancyState::new()));
  enqueue_microtask(microtask_a_calls_drain, st as *const ReentrancyState as *mut u8);

  assert!(runtime_native::rt_drain_microtasks());
  assert!(!st.inner_call_result.load(Ordering::SeqCst));
  assert_eq!(st.b_runs.load(Ordering::SeqCst), 1);
  assert!(!st.b_ran_during_a.load(Ordering::SeqCst));
  assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn run_until_idle_is_non_reentrant() {
  let _rt = TestRuntimeGuard::new();

  let hook_calls = Arc::new(AtomicUsize::new(0));
  set_microtask_checkpoint_end_hook(Some(Box::new({
    let hook_calls = hook_calls.clone();
    move || {
      hook_calls.fetch_add(1, Ordering::SeqCst);
    }
  })));

  let st: &'static ReentrancyState = Box::leak(Box::new(ReentrancyState::new()));
  enqueue_macrotask(macrotask_a_calls_run_until_idle, st as *const ReentrancyState as *mut u8);

  assert!(runtime_native::rt_async_run_until_idle());
  assert!(!st.inner_call_result.load(Ordering::SeqCst));
  assert_eq!(st.b_runs.load(Ordering::SeqCst), 1);
  assert!(!st.b_ran_during_a.load(Ordering::SeqCst));
  assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn drain_microtasks_is_non_reentrant_inside_rt_async_poll() {
  let _rt = TestRuntimeGuard::new();

  let st: &'static ReentrancyState = Box::leak(Box::new(ReentrancyState::new()));
  enqueue_microtask(microtask_a_calls_drain, st as *const ReentrancyState as *mut u8);

  // Drive the runtime using the event loop polling API. The inner call to `rt_drain_microtasks`
  // must not deadlock on the poll lock.
  while runtime_native::rt_async_poll_legacy() {}

  assert!(!st.inner_call_result.load(Ordering::SeqCst));
  assert_eq!(st.b_runs.load(Ordering::SeqCst), 1);
  assert!(!st.b_ran_during_a.load(Ordering::SeqCst));
}

#[test]
fn run_until_idle_is_non_reentrant_inside_rt_async_poll() {
  let _rt = TestRuntimeGuard::new();

  let st: &'static ReentrancyState = Box::leak(Box::new(ReentrancyState::new()));
  enqueue_macrotask(macrotask_a_calls_run_until_idle, st as *const ReentrancyState as *mut u8);

  // Drive the runtime using the event loop polling API. The inner call to `rt_async_run_until_idle`
  // must not deadlock on the poll lock.
  while runtime_native::rt_async_poll_legacy() {}

  assert!(!st.inner_call_result.load(Ordering::SeqCst));
  assert_eq!(st.b_runs.load(Ordering::SeqCst), 1);
  assert!(!st.b_ran_during_a.load(Ordering::SeqCst));
}
