use runtime_native::abi::{
  LegacyPromiseRef, PromiseResolveInput, RtCoroStatus, RtCoroutineHeader, RtShapeDescriptor, RtShapeId, ThenableRef,
  ThenableRejectCallback, ThenableResolveCallback, ThenableVTable, ValueRef,
};
use runtime_native::gc::ObjHeader;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use std::mem;
use std::sync::Once;

#[repr(C)]
struct GcBox<T> {
  header: ObjHeader,
  payload: T,
}

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    static SHAPES: [RtShapeDescriptor; 2] = [
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<AwaitPromiseCoroutine>>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<AwaitValueCoroutine>>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
    ];
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

unsafe fn alloc_pinned<T>(shape: RtShapeId) -> *mut GcBox<T> {
  ensure_shape_table();
  runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<T>>(), shape).cast::<GcBox<T>>()
}

fn drain_event_loop() {
  while runtime_native::rt_async_poll_legacy() {}
}

#[repr(C)]
struct AwaitPromiseCoroutine {
  header: RtCoroutineHeader,
  awaited: LegacyPromiseRef,
  out_is_error: *mut u32,
  out_value: *mut ValueRef,
  out_error: *mut ValueRef,
  done: *mut bool,
}

extern "C" fn await_promise_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut AwaitPromiseCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::RT_CORO_PENDING
      }
      1 => {
        *(*coro).out_is_error = (*coro).header.await_is_error;
        *(*coro).out_value = (*coro).header.await_value;
        *(*coro).out_error = (*coro).header.await_error;
        *(*coro).done = true;
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut());
        RtCoroStatus::RT_CORO_DONE
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[repr(C)]
struct AwaitValueCoroutine {
  header: RtCoroutineHeader,
  awaited: PromiseResolveInput,
  out_is_error: *mut u32,
  out_value: *mut ValueRef,
  out_error: *mut ValueRef,
  done: *mut bool,
}

extern "C" fn await_value_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut AwaitValueCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await_value_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::RT_CORO_PENDING
      }
      1 => {
        *(*coro).out_is_error = (*coro).header.await_is_error;
        *(*coro).out_value = (*coro).header.await_value;
        *(*coro).out_error = (*coro).header.await_error;
        *(*coro).done = true;
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut());
        RtCoroStatus::RT_CORO_DONE
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn self_resolution_rejects() {
  let _rt = TestRuntimeGuard::new();

  let p = runtime_native::rt_promise_new_legacy();
  runtime_native::rt_promise_resolve_promise_legacy(p, p);

  let mut is_error = 0u32;
  let mut value: ValueRef = core::ptr::null_mut();
  let mut error: ValueRef = core::ptr::null_mut();
  let mut done = false;

  let coro_obj = unsafe { alloc_pinned::<AwaitPromiseCoroutine>(RtShapeId(1)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: await_promise_resume,
    promise: LegacyPromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.awaited = p;
  coro.out_is_error = &mut is_error;
  coro.out_value = &mut value;
  coro.out_error = &mut error;
  coro.done = &mut done;

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  drain_event_loop();

  assert!(done);
  assert_eq!(is_error, 1);
  assert_ne!(error, core::ptr::null_mut());
  assert_eq!(value, core::ptr::null_mut());
}

#[test]
fn resolving_with_pending_promise_adopts_fulfillment() {
  let _rt = TestRuntimeGuard::new();

  let src = runtime_native::rt_promise_new_legacy();
  let dst = runtime_native::rt_promise_new_legacy();
  runtime_native::rt_promise_resolve_promise_legacy(dst, src);

  let mut is_error = 0u32;
  let mut value: ValueRef = core::ptr::null_mut();
  let mut error: ValueRef = core::ptr::null_mut();
  let mut done = false;

  let coro_obj = unsafe { alloc_pinned::<AwaitPromiseCoroutine>(RtShapeId(1)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: await_promise_resume,
    promise: LegacyPromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.awaited = dst;
  coro.out_is_error = &mut is_error;
  coro.out_value = &mut value;
  coro.out_error = &mut error;
  coro.done = &mut done;

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  runtime_native::rt_promise_resolve_legacy(src, 0xCAFE_BABEusize as ValueRef);
  drain_event_loop();

  assert!(done);
  assert_eq!(is_error, 0);
  assert_eq!(value as usize, 0xCAFE_BABEusize);
  assert_eq!(error, core::ptr::null_mut());
}

#[test]
fn resolving_with_pending_promise_adopts_rejection() {
  let _rt = TestRuntimeGuard::new();

  let src = runtime_native::rt_promise_new_legacy();
  let dst = runtime_native::rt_promise_new_legacy();
  runtime_native::rt_promise_resolve_promise_legacy(dst, src);

  let mut is_error = 0u32;
  let mut value: ValueRef = core::ptr::null_mut();
  let mut error: ValueRef = core::ptr::null_mut();
  let mut done = false;

  let coro_obj = unsafe { alloc_pinned::<AwaitPromiseCoroutine>(RtShapeId(1)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: await_promise_resume,
    promise: LegacyPromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.awaited = dst;
  coro.out_is_error = &mut is_error;
  coro.out_value = &mut value;
  coro.out_error = &mut error;
  coro.done = &mut done;

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  runtime_native::rt_promise_reject_legacy(src, 0xDEAD_BEEFusize as ValueRef);
  drain_event_loop();

  assert!(done);
  assert_eq!(is_error, 1);
  assert_eq!(error as usize, 0xDEAD_BEEFusize);
  assert_eq!(value, core::ptr::null_mut());
}

#[repr(C)]
struct ResolveTwiceThenable {
  src: LegacyPromiseRef,
}

unsafe extern "C" fn call_then_resolve_twice(
  thenable: *mut u8,
  on_fulfilled: ThenableResolveCallback,
  _on_rejected: ThenableRejectCallback,
  data: *mut u8,
) -> ValueRef {
  let t = &*(thenable as *mut ResolveTwiceThenable);
  on_fulfilled(data, PromiseResolveInput::promise(t.src));
  on_fulfilled(data, PromiseResolveInput::value(0x1111usize as ValueRef));
  core::ptr::null_mut()
}

static RESOLVE_TWICE_VTABLE: ThenableVTable = ThenableVTable {
  call_then: call_then_resolve_twice,
};

#[test]
fn thenable_calling_resolve_twice_only_resolves_once() {
  let _rt = TestRuntimeGuard::new();

  let src = runtime_native::rt_promise_new_legacy();
  let dst = runtime_native::rt_promise_new_legacy();

  let thenable = Box::new(ResolveTwiceThenable { src });
  let thenable_ref = ThenableRef {
    vtable: &RESOLVE_TWICE_VTABLE,
    ptr: Box::into_raw(thenable) as *mut u8,
  };

  runtime_native::rt_promise_resolve_thenable_legacy(dst, thenable_ref);

  let mut is_error = 0u32;
  let mut value: ValueRef = core::ptr::null_mut();
  let mut error: ValueRef = core::ptr::null_mut();
  let mut done = false;

  let coro_obj = unsafe { alloc_pinned::<AwaitPromiseCoroutine>(RtShapeId(1)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: await_promise_resume,
    promise: LegacyPromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.awaited = dst;
  coro.out_is_error = &mut is_error;
  coro.out_value = &mut value;
  coro.out_error = &mut error;
  coro.done = &mut done;

  runtime_native::rt_async_spawn_legacy(&mut coro.header);

  // First, allow the thenable job to run and register adoption on `src`.
  drain_event_loop();

  runtime_native::rt_promise_resolve_legacy(src, 0x2222usize as ValueRef);
  drain_event_loop();

  assert!(done);
  assert_eq!(is_error, 0);
  assert_eq!(value as usize, 0x2222usize);
  assert_eq!(error, core::ptr::null_mut());
}

#[repr(C)]
struct ResolveThenRejectThenable {
  src: LegacyPromiseRef,
  reject_reason: ValueRef,
}

unsafe extern "C" fn call_then_resolve_then_reject(
  thenable: *mut u8,
  on_fulfilled: ThenableResolveCallback,
  on_rejected: ThenableRejectCallback,
  data: *mut u8,
) -> ValueRef {
  let t = &*(thenable as *mut ResolveThenRejectThenable);
  on_fulfilled(data, PromiseResolveInput::promise(t.src));
  on_rejected(data, t.reject_reason);
  core::ptr::null_mut()
}

static RESOLVE_THEN_REJECT_VTABLE: ThenableVTable = ThenableVTable {
  call_then: call_then_resolve_then_reject,
};

#[test]
fn thenable_calling_resolve_then_reject_only_resolves() {
  let _rt = TestRuntimeGuard::new();

  let src = runtime_native::rt_promise_new_legacy();
  let dst = runtime_native::rt_promise_new_legacy();

  let thenable = Box::new(ResolveThenRejectThenable {
    src,
    reject_reason: 0x9999usize as ValueRef,
  });
  let thenable_ref = ThenableRef {
    vtable: &RESOLVE_THEN_REJECT_VTABLE,
    ptr: Box::into_raw(thenable) as *mut u8,
  };

  runtime_native::rt_promise_resolve_thenable_legacy(dst, thenable_ref);

  let mut is_error = 0u32;
  let mut value: ValueRef = core::ptr::null_mut();
  let mut error: ValueRef = core::ptr::null_mut();
  let mut done = false;

  let coro_obj = unsafe { alloc_pinned::<AwaitPromiseCoroutine>(RtShapeId(1)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: await_promise_resume,
    promise: LegacyPromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.awaited = dst;
  coro.out_is_error = &mut is_error;
  coro.out_value = &mut value;
  coro.out_error = &mut error;
  coro.done = &mut done;

  runtime_native::rt_async_spawn_legacy(&mut coro.header);

  // Run the thenable job so it registers adoption on `src`.
  drain_event_loop();

  runtime_native::rt_promise_resolve_legacy(src, 0xABCDusize as ValueRef);
  drain_event_loop();

  assert!(done);
  assert_eq!(is_error, 0);
  assert_eq!(value as usize, 0xABCDusize);
  assert_eq!(error, core::ptr::null_mut());
}

#[repr(C)]
struct ThrowingThenable {
  error: ValueRef,
}

unsafe extern "C" fn call_then_throw(
  thenable: *mut u8,
  _on_fulfilled: ThenableResolveCallback,
  _on_rejected: ThenableRejectCallback,
  _data: *mut u8,
) -> ValueRef {
  let t = &*(thenable as *mut ThrowingThenable);
  t.error
}

static THROWING_VTABLE: ThenableVTable = ThenableVTable { call_then: call_then_throw };

#[test]
fn thenable_throwing_during_then_call_rejects() {
  let _rt = TestRuntimeGuard::new();

  let dst = runtime_native::rt_promise_new_legacy();

  let thenable = Box::new(ThrowingThenable {
    error: 0xBADC0DEusize as ValueRef,
  });
  let thenable_ref = ThenableRef {
    vtable: &THROWING_VTABLE,
    ptr: Box::into_raw(thenable) as *mut u8,
  };

  runtime_native::rt_promise_resolve_thenable_legacy(dst, thenable_ref);

  let mut is_error = 0u32;
  let mut value: ValueRef = core::ptr::null_mut();
  let mut error: ValueRef = core::ptr::null_mut();
  let mut done = false;

  let coro_obj = unsafe { alloc_pinned::<AwaitPromiseCoroutine>(RtShapeId(1)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: await_promise_resume,
    promise: LegacyPromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.awaited = dst;
  coro.out_is_error = &mut is_error;
  coro.out_value = &mut value;
  coro.out_error = &mut error;
  coro.done = &mut done;

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  drain_event_loop();

  assert!(done);
  assert_eq!(is_error, 1);
  assert_eq!(error as usize, 0xBADC0DEusize);
  assert_eq!(value, core::ptr::null_mut());
}

#[repr(C)]
struct RejectingThenable {
  reason: ValueRef,
}

unsafe extern "C" fn call_then_reject(
  thenable: *mut u8,
  _on_fulfilled: ThenableResolveCallback,
  on_rejected: ThenableRejectCallback,
  data: *mut u8,
) -> ValueRef {
  let t = &*(thenable as *mut RejectingThenable);
  on_rejected(data, t.reason);
  core::ptr::null_mut()
}

static REJECTING_VTABLE: ThenableVTable = ThenableVTable { call_then: call_then_reject };

#[test]
fn await_thenable_uses_promise_resolve_and_marks_handled() {
  let _rt = TestRuntimeGuard::new();

  let thenable = Box::new(RejectingThenable {
    reason: 0x1234usize as ValueRef,
  });
  let thenable_ref = ThenableRef {
    vtable: &REJECTING_VTABLE,
    ptr: Box::into_raw(thenable) as *mut u8,
  };

  let awaited = PromiseResolveInput::thenable(thenable_ref);

  let mut is_error = 0u32;
  let mut value: ValueRef = core::ptr::null_mut();
  let mut error: ValueRef = core::ptr::null_mut();
  let mut done = false;

  let coro_obj = unsafe { alloc_pinned::<AwaitValueCoroutine>(RtShapeId(2)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: await_value_resume,
    promise: LegacyPromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.awaited = awaited;
  coro.out_is_error = &mut is_error;
  coro.out_value = &mut value;
  coro.out_error = &mut error;
  coro.done = &mut done;

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  drain_event_loop();

  assert!(done);
  assert_eq!(is_error, 1);
  assert_eq!(error as usize, 0x1234usize);
  assert_eq!(value, core::ptr::null_mut());

  // The intermediate promise created by `await` should have been marked as handled, so no unhandled
  // rejection is recorded.
  assert_eq!(runtime_native::test_util::unhandled_rejection_count(), 0);
}
