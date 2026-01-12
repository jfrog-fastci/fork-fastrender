use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;

thread_local! {
  static CALL_UNREGISTER_CURRENT_THREAD_ON_DROP: CallUnregisterCurrentThreadOnDrop = CallUnregisterCurrentThreadOnDrop;
}

struct CallUnregisterCurrentThreadOnDrop;

impl Drop for CallUnregisterCurrentThreadOnDrop {
  fn drop(&mut self) {
    // Regression test: `threading::unregister_current_thread` calls into `rt_alloc::on_thread_unregistered`,
    // which touches thread-local allocator bookkeeping. If those TLS keys have already been destroyed,
    // using `LocalKey::with` would panic with `AccessError` and abort the process
    // (`abort_on_dtor_unwind`).
    threading::unregister_current_thread();
  }
}

#[test]
fn unregister_is_safe_after_allocator_tls_has_been_destroyed() {
  let _rt = TestRuntimeGuard::new();

  std::thread::spawn(|| {
    // Initialize the thread-registry TLS first *without* initializing the allocator TLS.
    // (`threading::register_current_thread` would also initialize allocator TLS via `rt_alloc::on_thread_registered`.)
    let _ = threading::registry::register_current_thread(ThreadKind::External);

    // Initialize our TLS destructor key after the registry TLS.
    CALL_UNREGISTER_CURRENT_THREAD_ON_DROP.with(|_| {});

    // Now initialize the allocator TLS after our key. During thread teardown, the allocator TLS will
    // be destroyed first, then our key's destructor will call `unregister_current_thread` while the
    // registry TLS is still accessible.
    let _ = threading::register_current_thread(ThreadKind::External);
  })
  .join()
  .unwrap();
}

