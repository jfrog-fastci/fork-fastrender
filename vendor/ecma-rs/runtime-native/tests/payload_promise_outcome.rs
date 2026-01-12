use runtime_native::abi::PromiseRef;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseLayout;
use std::time::{Duration, Instant};

extern "C" fn fulfill_write_u32(data: *mut u8, promise: PromiseRef) {
  unsafe {
    let payload = runtime_native::rt_promise_payload_ptr(promise) as *mut u32;
    assert!(!payload.is_null());
    *payload = data as usize as u32;
    runtime_native::rt_promise_fulfill(promise);
  }
}

extern "C" fn reject_write_u32(data: *mut u8, promise: PromiseRef) {
  unsafe {
    let payload = runtime_native::rt_promise_payload_ptr(promise) as *mut u32;
    assert!(!payload.is_null());
    *payload = data as usize as u32;
    runtime_native::rt_promise_reject(promise);
  }
}

#[test]
fn payload_promise_outcome_reports_payload_ptr_on_fulfill() {
  let _rt = TestRuntimeGuard::new();

  let promise = runtime_native::rt_parallel_spawn_promise(
    fulfill_write_u32,
    123u32 as usize as *mut u8,
    PromiseLayout::of::<u32>(),
  );
  let payload_ptr = runtime_native::rt_promise_payload_ptr(promise);
  assert!(!payload_ptr.is_null());

  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let (tag, value) = runtime_native::rt_debug_promise_outcome(promise);
    if tag == 1 {
      assert_eq!(value as *mut u8, payload_ptr);
      return;
    }
    assert!(
      Instant::now() < deadline,
      "timed out waiting for payload promise to fulfill"
    );
    runtime_native::rt_async_poll_legacy();
  }
}

#[test]
fn payload_promise_outcome_reports_payload_ptr_on_reject() {
  let _rt = TestRuntimeGuard::new();

  let promise = runtime_native::rt_parallel_spawn_promise(
    reject_write_u32,
    0xDEAD_BEEF_u32 as usize as *mut u8,
    PromiseLayout::of::<u32>(),
  );
  let payload_ptr = runtime_native::rt_promise_payload_ptr(promise);
  assert!(!payload_ptr.is_null());

  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    let (tag, value) = runtime_native::rt_debug_promise_outcome(promise);
    if tag == 2 {
      assert_eq!(value as *mut u8, payload_ptr);
      return;
    }
    assert!(
      Instant::now() < deadline,
      "timed out waiting for payload promise to reject"
    );
    runtime_native::rt_async_poll_legacy();
  }
}
