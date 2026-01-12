use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;

thread_local! {
  static CALL_REGISTER_THREAD_ON_DROP: CallRegisterThreadOnDrop = CallRegisterThreadOnDrop;
}

struct CallRegisterThreadOnDrop;

impl Drop for CallRegisterThreadOnDrop {
  fn drop(&mut self) {
    // Regression test: `threading::register_current_thread` installs a `ThreadRegistration` in TLS.
    // If it uses `LocalKey::with`, calling it from another TLS destructor after the registry TLS
    // has already been destroyed aborts the process (`abort_on_dtor_unwind`).
    let _ = threading::register_current_thread(ThreadKind::External);
  }
}

#[test]
fn register_current_thread_is_safe_after_registry_tls_teardown() {
  let _rt = TestRuntimeGuard::new();

  std::thread::spawn(|| {
    // Initialize our TLS destructor key first so the registry TLS keys are created after it and
    // destroyed before it.
    CALL_REGISTER_THREAD_ON_DROP.with(|_| {});

    // Create the thread-registry TLS keys after our key.
    let _ = threading::register_current_thread(ThreadKind::External);
  })
  .join()
  .unwrap();
}

