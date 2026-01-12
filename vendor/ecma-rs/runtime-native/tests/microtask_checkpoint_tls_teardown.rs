use runtime_native::test_util::TestRuntimeGuard;

thread_local! {
  static CALL_DRAIN_MICROTASKS_ON_DROP: CallDrainMicrotasksOnDrop = CallDrainMicrotasksOnDrop;
}

struct CallDrainMicrotasksOnDrop;

impl Drop for CallDrainMicrotasksOnDrop {
  fn drop(&mut self) {
    // Regression test: `rt_drain_microtasks` uses the `PERFORMING_MICROTASK_CHECKPOINT` TLS flag to
    // enforce HTML-style non-reentrancy. If it uses `LocalKey::with`, calling it from another TLS
    // destructor after that flag has already been destroyed aborts the process (`abort_on_dtor_unwind`).
    let _ = runtime_native::rt_drain_microtasks();
  }
}

#[test]
fn microtask_checkpoint_tls_is_safe_to_access_during_tls_teardown() {
  let _rt = TestRuntimeGuard::new();

  std::thread::spawn(|| {
    // Ensure this TLS key is initialized *before* `PERFORMING_MICROTASK_CHECKPOINT` so it is
    // destroyed after it.
    CALL_DRAIN_MICROTASKS_ON_DROP.with(|_| {});

    // Initialize the microtask checkpoint TLS after our key. At thread exit,
    // `PERFORMING_MICROTASK_CHECKPOINT` will be dropped first and marked "destroyed", so the `Drop`
    // impl above will attempt to access it again after destruction.
    let _ = runtime_native::rt_drain_microtasks();
  })
  .join()
  .unwrap();
}

