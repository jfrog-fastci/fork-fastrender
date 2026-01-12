use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::PromiseLayout;
use runtime_native::PromiseRef;
use runtime_native::abi::LegacyPromiseRef;

thread_local! {
  static CALL_SPAWN_PAYLOAD_PROMISE_ON_DROP: CallSpawnPayloadPromiseOnDrop = CallSpawnPayloadPromiseOnDrop;
}

extern "C" fn fulfill_task(_data: *mut u8, promise: PromiseRef) {
  unsafe {
    runtime_native::rt_promise_fulfill(promise);
  }
}

struct CallSpawnPayloadPromiseOnDrop;

impl Drop for CallSpawnPayloadPromiseOnDrop {
  fn drop(&mut self) {
    // Regression test: calling `rt_parallel_spawn_promise` from *other* TLS destructors during
    // thread teardown must not abort due to inaccessible TLS keys (`abort_on_dtor_unwind`).
    unsafe {
      let promise = runtime_native::rt_parallel_spawn_promise(
        fulfill_task,
        core::ptr::null_mut(),
        PromiseLayout { size: 1, align: 1 },
      );
      runtime_native::rt_async_block_on(promise);
      runtime_native::rt_promise_drop_legacy(LegacyPromiseRef(promise.0.cast()));
    }
  }
}

#[test]
fn alloc_bump_tls_is_safe_to_access_during_tls_teardown() {
  let _rt = TestRuntimeGuard::new();

  std::thread::spawn(|| {
    // Ensure the thread registry TLS outlives our TLS destructor key.
    threading::register_current_thread(ThreadKind::External);

    // Initialize our TLS key first so the bump allocator TLS key is created after it.
    CALL_SPAWN_PAYLOAD_PROMISE_ON_DROP.with(|_| {});

    // Create the bump allocator TLS key after our key by allocating a payload promise.
    unsafe {
      let promise = runtime_native::rt_parallel_spawn_promise(
        fulfill_task,
        core::ptr::null_mut(),
        PromiseLayout { size: 1, align: 1 },
      );
      runtime_native::rt_async_block_on(promise);
      runtime_native::rt_promise_drop_legacy(LegacyPromiseRef(promise.0.cast()));
    }
  })
  .join()
  .unwrap();
}
