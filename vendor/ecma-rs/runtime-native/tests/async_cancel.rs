use runtime_native::abi::Microtask;
use runtime_native::async_abi::{
  Coroutine, CoroutineRef, CoroutineStep, CoroutineVTable, PromiseHeader, PromiseRef, CORO_FLAG_RUNTIME_OWNS_FRAME,
  RT_ASYNC_ABI_VERSION,
};
use runtime_native::test_util::{new_promise_header_pending, TestRuntimeGuard};
use runtime_native::{rt_async_cancel_all, rt_drain_microtasks, rt_queue_microtask, CoroutineId};
use std::sync::atomic::{AtomicUsize, Ordering};

#[repr(C)]
struct DropPayload {
  ran: *const AtomicUsize,
  dropped: *const AtomicUsize,
}

extern "C" fn microtask_run(data: *mut u8) {
  // SAFETY: owned by this microtask invocation.
  let payload: Box<DropPayload> = unsafe { Box::from_raw(data.cast()) };
  let ran = unsafe { &*payload.ran };
  ran.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn microtask_drop(data: *mut u8) {
  // SAFETY: owned by this microtask drop hook invocation.
  let payload: Box<DropPayload> = unsafe { Box::from_raw(data.cast()) };
  let dropped = unsafe { &*payload.dropped };
  dropped.fetch_add(1, Ordering::SeqCst);
}

#[repr(C)]
struct AwaitCoro {
  header: Coroutine,
  destroyed: *const AtomicUsize,
  await_promise: PromiseRef,
}

unsafe extern "C" fn await_resume(coro: *mut Coroutine) -> CoroutineStep {
  let coro = coro as *mut AwaitCoro;
  CoroutineStep::await_(unsafe { (*coro).await_promise })
}

unsafe extern "C" fn heap_destroy(coro: CoroutineRef) {
  let coro = coro as *mut AwaitCoro;
  let counter = unsafe { &*(*coro).destroyed };
  counter.fetch_add(1, Ordering::SeqCst);
  unsafe {
    drop(Box::from_raw(coro));
  }
}

static AWAIT_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: await_resume,
  destroy: heap_destroy,
  promise_size: core::mem::size_of::<PromiseHeader>() as u32,
  promise_align: core::mem::align_of::<PromiseHeader>() as u32,
  promise_shape_id: runtime_native::RtShapeId::INVALID,
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[test]
fn cancel_runs_microtask_drop_hook_without_executing() {
  let _rt = TestRuntimeGuard::new();

  let ran = Box::into_raw(Box::new(AtomicUsize::new(0)));
  let dropped = Box::into_raw(Box::new(AtomicUsize::new(0)));

  let payload = Box::new(DropPayload { ran, dropped });
  unsafe {
    rt_queue_microtask(Microtask {
      func: microtask_run,
      data: Box::into_raw(payload).cast(),
      drop: Some(microtask_drop),
    });
  }

  rt_async_cancel_all();

  // The queue should be empty and the microtask must not run.
  assert!(!rt_drain_microtasks());
  assert_eq!(unsafe { &*ran }.load(Ordering::SeqCst), 0);
  assert_eq!(unsafe { &*dropped }.load(Ordering::SeqCst), 1);

  // Idempotent.
  rt_async_cancel_all();

  unsafe {
    drop(Box::from_raw(ran));
    drop(Box::from_raw(dropped));
  }
}

#[test]
fn cancel_drops_pending_native_promise_reactions() {
  let _rt = TestRuntimeGuard::new();
  let destroyed = AtomicUsize::new(0);

  let awaited = Box::new(new_promise_header_pending());
  let awaited_hdr: PromiseRef = Box::into_raw(awaited);

  let coro = Box::new(AwaitCoro {
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
  let handle = runtime_native::rt_handle_alloc(coro_ref.cast());
  unsafe {
    let _promise = runtime_native::rt_async_spawn(CoroutineId(handle));
  }

  assert_ne!(
    unsafe { &(*awaited_hdr).waiters }.load(Ordering::Acquire),
    0,
    "awaiting a pending promise must register a reaction node"
  );

  rt_async_cancel_all();
  assert_eq!(destroyed.load(Ordering::SeqCst), 1);
  assert!(runtime_native::rt_handle_load(handle).is_null());

  assert_eq!(
    unsafe { &(*awaited_hdr).waiters }.load(Ordering::Acquire),
    0,
    "rt_async_cancel_all must detach and drop pending promise reactions"
  );

  unsafe {
    drop(Box::from_raw(awaited_hdr));
  }
}
