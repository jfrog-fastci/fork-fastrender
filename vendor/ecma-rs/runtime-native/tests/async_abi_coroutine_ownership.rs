use runtime_native::async_abi::{
  Coroutine, CoroutineRef, CoroutineStep, CoroutineVTable, PromiseHeader, PromiseRef,
  CORO_FLAG_RUNTIME_OWNS_FRAME, RT_ASYNC_ABI_VERSION,
};
use runtime_native::shape_table;
use runtime_native::test_util::{new_promise_header_pending, TestRuntimeGuard};
use runtime_native::CoroutineId;
use runtime_native::RtShapeDescriptor;
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
      size: core::mem::size_of::<PromiseHeader>() as u32,
      align: core::mem::align_of::<PromiseHeader>() as u16,
      flags: 0,
      ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
      ptr_offsets_len: 0,
      reserved: 0,
    }];
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

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
  promise_shape_id: runtime_native::RtShapeId(1),
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

static AWAIT_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: await_resume,
  destroy: heap_destroy,
  promise_size: core::mem::size_of::<PromiseHeader>() as u32,
  promise_align: core::mem::align_of::<PromiseHeader>() as u32,
  promise_shape_id: runtime_native::RtShapeId(1),
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

static STACK_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: complete_resume,
  destroy: count_only_destroy,
  promise_size: core::mem::size_of::<PromiseHeader>() as u32,
  promise_align: core::mem::align_of::<PromiseHeader>() as u32,
  promise_shape_id: runtime_native::RtShapeId(1),
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[test]
fn heap_owned_coroutine_is_destroyed_exactly_once_on_completion() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();
  let destroyed = AtomicUsize::new(0);

  let mut coro = Box::new(TestCoro {
    header: unsafe { core::mem::zeroed() },
    destroyed: &destroyed,
    await_promise: core::ptr::null_mut(),
  });
  coro.header.vtable = &COMPLETE_VTABLE;
  coro.header.promise = core::ptr::null_mut();
  coro.header.next_waiter = core::ptr::null_mut();
  coro.header.flags = CORO_FLAG_RUNTIME_OWNS_FRAME;

  let coro_ref = Box::into_raw(coro) as CoroutineRef;
  let handle = runtime_native::rt_handle_alloc(coro_ref.cast());
  let _promise = unsafe { runtime_native::rt_async_spawn(CoroutineId(handle)) };
  assert!(runtime_native::rt_handle_load(handle).is_null());

  assert_eq!(destroyed.load(Ordering::SeqCst), 1);
  assert!(runtime_native::rt_handle_load(handle).is_null());

  // Cancellation after completion should not double-destroy.
  runtime_native::rt_async_cancel_all();
  assert_eq!(destroyed.load(Ordering::SeqCst), 1);
}

#[test]
fn stack_owned_coroutine_is_not_destroyed_and_must_complete_synchronously() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();
  let destroyed = AtomicUsize::new(0);

  let mut coro = TestCoro {
    header: unsafe { core::mem::zeroed() },
    destroyed: &destroyed,
    await_promise: core::ptr::null_mut(),
  };
  coro.header.vtable = &STACK_VTABLE;
  coro.header.promise = core::ptr::null_mut();
  coro.header.next_waiter = core::ptr::null_mut();
  coro.header.flags = 0;

  let coro_ptr = &mut coro.header as *mut Coroutine;
  let handle = runtime_native::rt_handle_alloc(coro_ptr.cast());
  let _promise = unsafe { runtime_native::rt_async_spawn(CoroutineId(handle)) };
  assert!(
    runtime_native::rt_handle_load(handle).is_null(),
    "stack-owned coroutines must complete synchronously so the runtime can free the handle"
  );

  assert_eq!(destroyed.load(Ordering::SeqCst), 0);
  assert!(runtime_native::rt_handle_load(handle).is_null());

  // Cancelling the runtime must not attempt to destroy stack-owned frames.
  runtime_native::rt_async_cancel_all();
  assert_eq!(destroyed.load(Ordering::SeqCst), 0);
}

#[test]
fn cancel_all_destroys_deferred_heap_owned_coroutines_once() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();
  let destroyed = AtomicUsize::new(0);

  let mut coro = Box::new(TestCoro {
    header: unsafe { core::mem::zeroed() },
    destroyed: &destroyed,
    await_promise: core::ptr::null_mut(),
  });
  coro.header.vtable = &COMPLETE_VTABLE;
  coro.header.promise = core::ptr::null_mut();
  coro.header.next_waiter = core::ptr::null_mut();
  coro.header.flags = CORO_FLAG_RUNTIME_OWNS_FRAME;

  let coro_ref = Box::into_raw(coro) as CoroutineRef;
  let handle = runtime_native::rt_handle_alloc(coro_ref.cast());
  let _promise = unsafe { runtime_native::rt_async_spawn_deferred(CoroutineId(handle)) };
  assert_eq!(destroyed.load(Ordering::SeqCst), 0);

  runtime_native::rt_async_cancel_all();
  assert_eq!(destroyed.load(Ordering::SeqCst), 1);
  assert!(runtime_native::rt_handle_load(handle).is_null());

  // Drain microtasks to ensure stale scheduled resumes are harmless.
  let _ = runtime_native::rt_drain_microtasks();

  // Idempotent.
  runtime_native::rt_async_cancel_all();
  assert_eq!(destroyed.load(Ordering::SeqCst), 1);
}

#[test]
fn cancel_all_prevents_stale_resume_after_awaited_promise_settles() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();
  let destroyed = AtomicUsize::new(0);

  // Allocate a standalone awaited promise header.
  let awaited = Box::new(new_promise_header_pending());
  let awaited_hdr: PromiseRef = Box::into_raw(awaited);

  let mut coro = Box::new(TestCoro {
    header: unsafe { core::mem::zeroed() },
    destroyed: &destroyed,
    await_promise: awaited_hdr,
  });
  coro.header.vtable = &AWAIT_VTABLE;
  coro.header.promise = core::ptr::null_mut();
  coro.header.next_waiter = core::ptr::null_mut();
  coro.header.flags = CORO_FLAG_RUNTIME_OWNS_FRAME;

  let coro_ref = Box::into_raw(coro) as CoroutineRef;
  let handle = runtime_native::rt_handle_alloc(coro_ref.cast());
  let _promise = unsafe { runtime_native::rt_async_spawn(CoroutineId(handle)) };
  assert_eq!(destroyed.load(Ordering::SeqCst), 0);

  runtime_native::rt_async_cancel_all();
  assert_eq!(destroyed.load(Ordering::SeqCst), 1);
  assert!(runtime_native::rt_handle_load(handle).is_null());

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
