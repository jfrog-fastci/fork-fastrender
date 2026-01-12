use runtime_native::mutator;
use runtime_native::test_util::TestRuntimeGuard;

thread_local! {
  static CALL_READ_MUTATOR_PTR_ON_DROP: CallReadMutatorPtrOnDrop = CallReadMutatorPtrOnDrop;
}

struct CallReadMutatorPtrOnDrop;

impl Drop for CallReadMutatorPtrOnDrop {
  fn drop(&mut self) {
    // Regression test: some runtime entrypoints (write barrier, remembered-set flushing) read the
    // current mutator-thread pointer from TLS. If that TLS key is accessed with `LocalKey::with`,
    // calling such helpers from other TLS destructors after the key has been destroyed aborts the
    // process (`abort_on_dtor_unwind`).
    let _ = mutator::current_mutator_thread_ptr();
  }
}

#[test]
fn mutator_tls_is_safe_to_access_during_tls_teardown() {
  let _rt = TestRuntimeGuard::new();

  std::thread::spawn(|| {
    // Initialize our TLS key first so the mutator TLS key is created after it.
    CALL_READ_MUTATOR_PTR_ON_DROP.with(|_| {});

    // Create the mutator TLS key after our key.
    let _ = mutator::current_mutator_thread_ptr();
  })
  .join()
  .unwrap();
}

