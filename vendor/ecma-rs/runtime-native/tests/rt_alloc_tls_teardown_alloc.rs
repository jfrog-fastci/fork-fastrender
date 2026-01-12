use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;

thread_local! {
  static CALL_RT_ALLOC_ARRAY_ON_DROP: CallRtAllocArrayOnDrop = CallRtAllocArrayOnDrop;
}

struct CallRtAllocArrayOnDrop;

impl Drop for CallRtAllocArrayOnDrop {
  fn drop(&mut self) {
    // Regression test: `rt_alloc_array` uses thread-local allocator state (`TLS_ALLOC`). If that TLS
    // key has already been destroyed, using `LocalKey::with` would panic with `AccessError` and
    // abort the process (`abort_on_dtor_unwind`).
    //
    // This should fall back to a safe slow path (LOS allocation under the heap lock) instead of
    // aborting.
    let obj = runtime_native::rt_alloc_array(1, 1);
    if obj.is_null() {
      std::process::abort();
    }
  }
}

#[test]
fn rt_alloc_array_is_safe_after_allocator_tls_teardown() {
  let _rt = TestRuntimeGuard::new();

  std::thread::spawn(|| {
    // Register the thread in the GC/safepoint registry first *without* initializing the allocator
    // TLS. We do this by calling the registry API directly instead of the wrapper in
    // `threading/mod.rs`.
    let _ = threading::registry::register_current_thread(ThreadKind::External);

    // Initialize our TLS destructor key before allocating so allocator TLS is created after it and
    // destroyed before it.
    CALL_RT_ALLOC_ARRAY_ON_DROP.with(|_| {});

    // Create allocator TLS after our key.
    let obj = runtime_native::rt_alloc_array(1, 1);
    if obj.is_null() {
      std::process::abort();
    }
  })
  .join()
  .unwrap();
}

