use runtime_native::async_abi::{
  Coroutine, CoroutineRef, CoroutineStep, CoroutineVTable, PromiseHeader, PromiseRef, CORO_FLAG_RUNTIME_OWNS_FRAME,
  RT_ASYNC_ABI_VERSION,
};
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::{AtomicUsize, Ordering};

#[repr(C)]
struct TestCoro {
  header: Coroutine,
  destroyed: *const AtomicUsize,
  await_promise: PromiseRef,
}

unsafe extern "C" fn complete_resume(_coro: *mut Coroutine) -> CoroutineStep {
  CoroutineStep::complete()
}

unsafe extern "C" fn await_resume(coro: *mut Coroutine) -> CoroutineStep {
  let coro = coro as *mut TestCoro;
  CoroutineStep::await_(unsafe { (*coro).await_promise })
}

unsafe extern "C" fn heap_destroy(coro: CoroutineRef) {
  let coro = coro as *mut TestCoro;
  let counter = unsafe { &*(*coro).destroyed };
  counter.fetch_add(1, Ordering::SeqCst);
  unsafe { drop(Box::from_raw(coro)) };
}

unsafe extern "C" fn count_only_destroy(coro: CoroutineRef) {
  let coro = coro as *mut TestCoro;
  let counter = unsafe { &*(*coro).destroyed };
  counter.fetch_add(1, Ordering::SeqCst);
}

static COMPLETE_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: complete_resume,
  destroy: heap_destroy,
  promise_size: core::mem::size_of::<PromiseHeader>() as u32,
  promise_align: core::mem::align_of::<PromiseHeader>() as u32,
  promise_shape_id: runtime_native::RtShapeId::INVALID,
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

static AWAIT_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: await_resume,
  destroy: heap_destroy,
  promise_size: core::mem::size_of::<PromiseHeader>() as u32,
  promise_align: core::mem::align_of::<PromiseHeader>() as u32,
  promise_shape_id: runtime_native::RtShapeId::INVALID,
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

static STACK_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: complete_resume,
  destroy: count_only_destroy,
  promise_size: core::mem::size_of::<PromiseHeader>() as u32,
  promise_align: core::mem::align_of::<PromiseHeader>() as u32,
  promise_shape_id: runtime_native::RtShapeId::INVALID,
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[test]
fn heap_owned_coroutine_is_destroyed_exactly_once_on_completion() {
  let _rt = TestRuntimeGuard::new();
  let destroyed = AtomicUsize::new(0);

  let coro = Box::new(TestCoro {
    header: Coroutine {
      vtable: &COMPLETE_VTABLE,
      promise: core::ptr::null_mut(),
      next_waiter: core::ptr::null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    destroyed: &destroyed,
    await_promise: core::ptr::null_mut(),
  });

  let coro_ref = Box::into_raw(coro) as CoroutineRef;
  unsafe {
    let _promise = runtime_native::rt_async_spawn(coro_ref);
  }

  assert_eq!(destroyed.load(Ordering::SeqCst), 1);

  // Cancellation after completion should not double-destroy.
  runtime_native::rt_async_cancel_all();
  assert_eq!(destroyed.load(Ordering::SeqCst), 1);
}

#[test]
fn stack_owned_coroutine_is_not_destroyed_and_must_complete_synchronously() {
  let _rt = TestRuntimeGuard::new();
  let destroyed = AtomicUsize::new(0);

  let mut coro = TestCoro {
    header: Coroutine {
      vtable: &STACK_VTABLE,
      promise: core::ptr::null_mut(),
      next_waiter: core::ptr::null_mut(),
      flags: 0,
    },
    destroyed: &destroyed,
    await_promise: core::ptr::null_mut(),
  };

  unsafe {
    let _promise = runtime_native::rt_async_spawn(&mut coro.header as *mut Coroutine);
  }

  assert_eq!(destroyed.load(Ordering::SeqCst), 0);

  // Cancelling the runtime must not attempt to destroy stack-owned frames.
  runtime_native::rt_async_cancel_all();
  assert_eq!(destroyed.load(Ordering::SeqCst), 0);
}

#[test]
fn cancel_all_destroys_deferred_heap_owned_coroutines_once() {
  let _rt = TestRuntimeGuard::new();
  let destroyed = AtomicUsize::new(0);

  let coro = Box::new(TestCoro {
    header: Coroutine {
      vtable: &COMPLETE_VTABLE,
      promise: core::ptr::null_mut(),
      next_waiter: core::ptr::null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    destroyed: &destroyed,
    await_promise: core::ptr::null_mut(),
  });

  let coro_ref = Box::into_raw(coro) as CoroutineRef;
  unsafe {
    let _promise = runtime_native::rt_async_spawn_deferred(coro_ref);
  }
  assert_eq!(destroyed.load(Ordering::SeqCst), 0);

  runtime_native::rt_async_cancel_all();
  assert_eq!(destroyed.load(Ordering::SeqCst), 1);

  // Drain microtasks to ensure stale scheduled resumes are harmless.
  let _ = runtime_native::rt_drain_microtasks();

  // Idempotent.
  runtime_native::rt_async_cancel_all();
  assert_eq!(destroyed.load(Ordering::SeqCst), 1);
}

#[test]
fn cancel_all_prevents_stale_resume_after_awaited_promise_settles() {
  let _rt = TestRuntimeGuard::new();
  let destroyed = AtomicUsize::new(0);

  // Allocate a standalone awaited promise header.
  let awaited = Box::new(PromiseHeader {
    state: core::sync::atomic::AtomicU8::new(PromiseHeader::PENDING),
    waiters: core::sync::atomic::AtomicUsize::new(0),
    flags: core::sync::atomic::AtomicU8::new(0),
  });
  let awaited_hdr: PromiseRef = Box::into_raw(awaited);

  let coro = Box::new(TestCoro {
    header: Coroutine {
      vtable: &AWAIT_VTABLE,
      promise: core::ptr::null_mut(),
      next_waiter: core::ptr::null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    destroyed: &destroyed,
    await_promise: awaited_hdr,
  });

  let coro_ref = Box::into_raw(coro) as CoroutineRef;
  unsafe {
    let _promise = runtime_native::rt_async_spawn(coro_ref);
  }
  assert_eq!(destroyed.load(Ordering::SeqCst), 0);

  runtime_native::rt_async_cancel_all();
  assert_eq!(destroyed.load(Ordering::SeqCst), 1);

  // Fulfill the awaited promise: this will schedule a reaction job that would normally resume the
  // coroutine. It must be a no-op (and not crash) after cancellation.
  unsafe {
    runtime_native::rt_promise_fulfill(runtime_native::PromiseRef(awaited_hdr.cast()));
  }
  let _ = runtime_native::rt_drain_microtasks();
  assert_eq!(destroyed.load(Ordering::SeqCst), 1);

  unsafe {
    drop(Box::from_raw(awaited_hdr));
  }
}
