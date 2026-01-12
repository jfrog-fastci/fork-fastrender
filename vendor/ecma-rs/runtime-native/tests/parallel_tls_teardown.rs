use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;

thread_local! {
  static CALL_PARALLEL_SPAWN_ON_DROP: CallParallelSpawnOnDrop = CallParallelSpawnOnDrop;
}

struct CallParallelSpawnOnDrop;

extern "C" fn noop_task(_data: *mut u8) {}

impl Drop for CallParallelSpawnOnDrop {
  fn drop(&mut self) {
    // Regression test: the parallel runtime uses thread-local state (`LOCAL_WORKER`) to detect when
    // a call is originating from a worker thread. `rt_parallel_spawn` can be called from other TLS
    // destructors during thread teardown; if that TLS key has already been destroyed, using
    // `LocalKey::with` would panic with `AccessError` and abort the process (`abort_on_dtor_unwind`).
    //
    // We spawn and join a no-op task to avoid leaking TaskIds in the global runtime singleton.
    let task = runtime_native::rt_parallel_spawn(noop_task, core::ptr::null_mut());
    runtime_native::rt_parallel_join(&task as *const _, 1);
  }
}

#[test]
fn parallel_tls_is_safe_to_access_during_tls_teardown() {
  let _rt = TestRuntimeGuard::new();

  std::thread::spawn(|| {
    // Ensure the thread is registered first so the registry TLS outlives our TLS destructor key.
    threading::register_current_thread(ThreadKind::External);

    // Initialize our TLS key before calling into the parallel runtime so `LOCAL_WORKER` is created
    // after it and will be destroyed first during thread teardown.
    CALL_PARALLEL_SPAWN_ON_DROP.with(|_| {});

    // Create the `LOCAL_WORKER` TLS key after our key by calling into the parallel runtime.
    let task = runtime_native::rt_parallel_spawn(noop_task, core::ptr::null_mut());
    runtime_native::rt_parallel_join(&task as *const _, 1);
  })
  .join()
  .unwrap();
}

