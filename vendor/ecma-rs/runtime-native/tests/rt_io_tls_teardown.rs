use runtime_native::test_util::TestRuntimeGuard;

thread_local! {
  static CALL_RT_IO_TAKE_LAST_ERROR_ON_DROP: CallRtIoTakeLastErrorOnDrop = CallRtIoTakeLastErrorOnDrop;
}

struct CallRtIoTakeLastErrorOnDrop;

impl Drop for CallRtIoTakeLastErrorOnDrop {
  fn drop(&mut self) {
    // Regression test: `rt_io_debug_take_last_error` uses `RT_IO_LAST_ERROR` TLS. If it uses
    // `LocalKey::with`, calling it from another TLS destructor after `RT_IO_LAST_ERROR` has already
    // been destroyed aborts the process (`abort_on_dtor_unwind`).
    let _ = runtime_native::rt_io_debug_take_last_error();
  }
}

#[test]
fn rt_io_tls_is_safe_to_access_during_tls_teardown() {
  let _rt = TestRuntimeGuard::new();

  std::thread::spawn(|| {
    // Ensure this TLS key is initialized *before* `RT_IO_LAST_ERROR` so it is destroyed after it.
    CALL_RT_IO_TAKE_LAST_ERROR_ON_DROP.with(|_| {});

    // Initialize `RT_IO_LAST_ERROR` after our key. At thread exit, `RT_IO_LAST_ERROR` will be
    // dropped first and marked "destroyed", so the `Drop` impl above will attempt to access it
    // again after destruction.
    let _ = runtime_native::rt_io_debug_take_last_error();
  })
  .join()
  .unwrap();
}
