use runtime_native::test_util::TestRuntimeGuard;

thread_local! {
  static CALL_RESUME_WORLD_ON_DROP: CallResumeWorldOnDrop = CallResumeWorldOnDrop;
}

struct CallResumeWorldOnDrop;

impl Drop for CallResumeWorldOnDrop {
  fn drop(&mut self) {
    // Regression test: `rt_gc_resume_world` touches safepoint TLS (`IN_STOP_THE_WORLD`).
    // If it uses `LocalKey::with` internally, calling it from a different TLS destructor after
    // `IN_STOP_THE_WORLD` has already been destroyed aborts the process (`abort_on_dtor_unwind`).
    runtime_native::rt_gc_resume_world();
  }
}

#[test]
fn safepoint_tls_is_safe_to_access_during_tls_teardown() {
  let _rt = TestRuntimeGuard::new();

  std::thread::spawn(|| {
    // Ensure this TLS key is initialized *before* `IN_STOP_THE_WORLD` so it is destroyed after it.
    CALL_RESUME_WORLD_ON_DROP.with(|_| {});

    // Initialize the runtime's safepoint TLS after our key. At thread exit, `IN_STOP_THE_WORLD`
    // will be dropped first and marked "destroyed", so the `Drop` impl above will attempt to
    // access it again after destruction.
    runtime_native::rt_gc_resume_world();
  })
  .join()
  .unwrap();
}

