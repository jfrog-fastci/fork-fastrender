use runtime_native::abi::PromiseRef;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseLayout;

#[repr(C, align(32))]
struct AlignedPayload {
  a: u64,
  b: u64,
}

extern "C" fn write_payload_and_fulfill(_data: *mut u8, promise: PromiseRef) {
  unsafe {
    let payload = runtime_native::rt_promise_payload_ptr(promise).cast::<AlignedPayload>();
    assert!(!payload.is_null());
    (*payload).a = 0x1111_2222_3333_4444;
    (*payload).b = 0xAAAA_BBBB_CCCC_DDDD;
    runtime_native::rt_promise_fulfill(promise);
  }
}

#[test]
fn parallel_spawn_promise_returns_gc_managed_promise() {
  let _rt = TestRuntimeGuard::new();

  let promise = runtime_native::rt_parallel_spawn_promise(
    write_payload_and_fulfill,
    core::ptr::null_mut(),
    PromiseLayout::of::<AlignedPayload>(),
  );
  assert!(!promise.is_null());

  // Ensure the payload pointer is properly aligned.
  let payload = runtime_native::rt_promise_payload_ptr(promise);
  assert!(!payload.is_null());
  assert_eq!(
    (payload as usize) % core::mem::align_of::<AlignedPayload>(),
    0,
    "payload pointer must respect PromiseLayout.align"
  );

  // Create a weak handle to the promise so we can verify it lives in the GC heap and is
  // reclaimable once unreachable.
  let weak = {
    let root = runtime_native::roots::Root::<u8>::new(promise.0.cast::<u8>());
    // Safety: `root.handle()` is a valid pointer to an addressable GC pointer slot.
    unsafe { runtime_native::rt_weak_add_h(root.handle()) }
  };

  // Wait for the worker to settle the promise and drain the runtime so any queued reaction jobs
  // (which root the promise) are dropped.
  unsafe {
    runtime_native::rt_async_block_on(promise);
  }
  while runtime_native::rt_async_poll() {}

  // Now the only remaining reference should be the weak handle.
  runtime_native::rt_gc_collect();
  assert!(
    runtime_native::rt_weak_get(weak).is_null(),
    "promise should be collectible (weak handle must clear after GC)"
  );
  runtime_native::rt_weak_remove(weak);
}

