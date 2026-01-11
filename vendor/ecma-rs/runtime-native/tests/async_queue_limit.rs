use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{rt_async_free_c_string, rt_async_set_limits, rt_async_take_last_error, rt_queue_microtask};
use std::ffi::CStr;

extern "C" fn noop(_data: *mut u8) {}

#[test]
fn ready_queue_len_is_capped() {
  let _rt = TestRuntimeGuard::new();
  rt_async_set_limits(10_000, 2);

  rt_queue_microtask(noop, std::ptr::null_mut());
  rt_queue_microtask(noop, std::ptr::null_mut());
  rt_queue_microtask(noop, std::ptr::null_mut());

  let err_ptr = rt_async_take_last_error();
  assert!(!err_ptr.is_null());
  let err = unsafe { CStr::from_ptr(err_ptr) }.to_string_lossy().into_owned();
  unsafe { rt_async_free_c_string(err_ptr) };

  assert!(err.contains("max_ready_queue_len"));
}
