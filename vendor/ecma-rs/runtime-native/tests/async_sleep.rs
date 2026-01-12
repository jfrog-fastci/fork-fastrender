use runtime_native::abi::{LegacyPromiseRef, PromiseRef};
use runtime_native::async_abi::PromiseHeader;
use runtime_native::test_util::TestRuntimeGuard;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

extern "C" {
  fn rt_async_sleep(delay_ms: u64) -> PromiseRef;
}

extern "C" fn set_bool(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

struct ThreadInitGuard;

impl ThreadInitGuard {
  fn new() -> Self {
    runtime_native::rt_thread_init(0);
    Self
  }
}

impl Drop for ThreadInitGuard {
  fn drop(&mut self) {
    runtime_native::rt_thread_deinit();
  }
}

#[test]
fn async_sleep_fulfills_promise() {
  let _rt = TestRuntimeGuard::new();
  let _thread = ThreadInitGuard::new();

  let p = unsafe { rt_async_sleep(10) };
  assert!(!p.is_null(), "rt_async_sleep returned a null promise");
  let p_legacy: LegacyPromiseRef = p.0.cast();

  let settled = AtomicBool::new(false);
  runtime_native::rt_promise_then_legacy(
    p_legacy,
    set_bool,
    (&settled as *const AtomicBool).cast::<u8>().cast_mut(),
  );

  // Drive the runtime until the timer callback settles the promise.
  let start = Instant::now();
  loop {
    unsafe {
      runtime_native::rt_async_run_until_idle_abi();
    }

    let header = p.0.cast::<PromiseHeader>();
    let state = unsafe { &(*header).state }.load(Ordering::Acquire);
    if state == PromiseHeader::FULFILLED && settled.load(Ordering::SeqCst) {
      break;
    }
    assert_ne!(
      state,
      PromiseHeader::REJECTED,
      "rt_async_sleep promise unexpectedly rejected"
    );
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for rt_async_sleep promise to fulfill"
    );
    std::thread::sleep(Duration::from_millis(1));
  }
}

#[inline(never)]
fn sleep_promise_weak_handle_after_settle() -> u64 {
  let p = unsafe { rt_async_sleep(10) };
  assert!(!p.is_null(), "rt_async_sleep returned a null promise");

  // Track the promise via a weak handle so we can assert it becomes unreachable.
  let weak = runtime_native::rt_weak_add(p.0.cast::<u8>());

  let settled = AtomicBool::new(false);
  let p_legacy: LegacyPromiseRef = p.0.cast();
  runtime_native::rt_promise_then_legacy(
    p_legacy,
    set_bool,
    (&settled as *const AtomicBool).cast::<u8>().cast_mut(),
  );

  // Avoid accidentally keeping the promise alive via conservative stack scanning.
  let mut p = p;
  unsafe {
    ptr::write_volatile(&mut p, PromiseRef::null());
  }

  let start = Instant::now();
  while !settled.load(Ordering::SeqCst) {
    unsafe {
      runtime_native::rt_async_run_until_idle_abi();
    }
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for rt_async_sleep promise to settle"
    );
    std::thread::sleep(Duration::from_millis(1));
  }

  // Drain any remaining microtasks/reaction jobs so the promise is no longer rooted by queued work.
  unsafe {
    runtime_native::rt_async_run_until_idle_abi();
  }

  weak
}

#[test]
fn async_sleep_promise_is_collectable_after_settle() {
  let _rt = TestRuntimeGuard::new();
  let _thread = ThreadInitGuard::new();

  let weak = sleep_promise_weak_handle_after_settle();

  runtime_native::rt_gc_collect();
  assert!(
    runtime_native::rt_weak_get(weak).is_null(),
    "expected sleep promise to be collectable after settling"
  );

  runtime_native::rt_weak_remove(weak);
}

#[test]
fn async_sleep_promise_relocates_while_rooted_in_timer_queue() {
  let _rt = TestRuntimeGuard::new();
  let _thread = ThreadInitGuard::new();

  // Ensure the nursery is in a clean state before we allocate the sleep promise so it is likely to
  // land in the young generation and be evacuated by minor GC.
  runtime_native::rt_gc_collect_minor();

  let p = unsafe { rt_async_sleep(50) };
  assert!(!p.is_null(), "rt_async_sleep returned a null promise");

  // Root the promise via the persistent handle table so we can observe relocation.
  let mut promise_ptr: *mut u8 = p.0.cast::<u8>();
  let handle = unsafe { runtime_native::rt_handle_alloc_h(&mut promise_ptr as *mut *mut u8) };

  // Snapshot the original nursery address in non-GC memory so GC can't scribble it.
  let original = Box::new(p.0 as usize);

  // Confirm the allocation is in the young range so minor GC will evacuate it.
  let mut young_start: *mut u8 = ptr::null_mut();
  let mut young_end: *mut u8 = ptr::null_mut();
  unsafe {
    runtime_native::rt_gc_get_young_range(&mut young_start, &mut young_end);
  }
  assert!(!young_start.is_null());
  assert!(!young_end.is_null());
  assert!(
    (young_start as usize..young_end as usize).contains(original.as_ref()),
    "expected sleep promise to be allocated in the nursery"
  );

  runtime_native::rt_gc_collect_minor();

  // Re-read the young range after the collection; conservative scanning may have scribbled locals.
  unsafe {
    runtime_native::rt_gc_get_young_range(&mut young_start, &mut young_end);
  }

  let relocated = runtime_native::rt_handle_load(handle);
  assert!(!relocated.is_null(), "expected rooted handle to remain valid after minor GC");
  assert_ne!(
    relocated as usize,
    *original,
    "expected sleep promise pointer to relocate after minor GC"
  );
  assert!(
    !(young_start as usize..young_end as usize).contains(&(relocated as usize)),
    "expected sleep promise to be evacuated out of the nursery after minor GC"
  );

  let settled = AtomicBool::new(false);
  let promise_relocated = PromiseRef(relocated.cast());
  let promise_relocated_legacy: LegacyPromiseRef = promise_relocated.0.cast();
  runtime_native::rt_promise_then_legacy(
    promise_relocated_legacy,
    set_bool,
    (&settled as *const AtomicBool).cast::<u8>().cast_mut(),
  );

  // Drive the runtime until the timer callback settles the promise.
  let start = Instant::now();
  loop {
    unsafe {
      runtime_native::rt_async_run_until_idle_abi();
    }

    let header = promise_relocated.0.cast::<PromiseHeader>();
    let state = unsafe { &(*header).state }.load(Ordering::Acquire);
    if state == PromiseHeader::FULFILLED && settled.load(Ordering::SeqCst) {
      break;
    }
    assert_ne!(
      state,
      PromiseHeader::REJECTED,
      "rt_async_sleep promise unexpectedly rejected"
    );
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for rt_async_sleep promise to fulfill after relocation"
    );
    std::thread::sleep(Duration::from_millis(1));
  }

  runtime_native::rt_handle_free(handle);
}
