use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;

thread_local! {
  static CALL_ASYNC_CANCEL_ALL_ON_DROP: CallAsyncCancelAllOnDrop = CallAsyncCancelAllOnDrop;
}

struct CallAsyncCancelAllOnDrop;

impl Drop for CallAsyncCancelAllOnDrop {
  fn drop(&mut self) {
    // Regression test: on non-Linux platforms the async driver guard identifies "this thread" via a
    // thread-local token. If that token is accessed with `LocalKey::with`, calling driving
    // entrypoints from other TLS destructors after the token has already been destroyed aborts the
    // process (`abort_on_dtor_unwind`).
    runtime_native::rt_async_cancel_all();
  }
}

#[test]
fn async_driver_guard_tls_is_safe_to_access_during_tls_teardown() {
  let _rt = TestRuntimeGuard::new();

  std::thread::spawn(|| {
    // Ensure the thread registry TLS is initialized before our TLS destructor key so it outlives it
    // (rt_async_cancel_all registers the calling thread as the event-loop thread).
    threading::register_current_thread(ThreadKind::External);

    // Initialize our TLS key first so the driver-token TLS key is created after it.
    CALL_ASYNC_CANCEL_ALL_ON_DROP.with(|_| {});

    // Create the async driver guard TLS token after our key by calling a driving entrypoint.
    runtime_native::rt_async_cancel_all();
  })
  .join()
  .unwrap();
}

