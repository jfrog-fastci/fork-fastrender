use runtime_native::abi::{
  PromiseRef, PromiseResolveInput, RtCoroStatus, RtCoroutineHeader, ThenableRef, ThenableRejectCallback,
  ThenableResolveCallback, ThenableVTable, ValueRef,
};
use runtime_native::test_util::TestRuntimeGuard;

fn drain_event_loop() {
  while runtime_native::rt_async_poll_legacy() {}
}

#[repr(C)]
struct AwaitPromiseCoroutine {
  header: RtCoroutineHeader,
  awaited: PromiseRef,
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
        RtCoroStatus::Pending
      }
      1 => {
        *(*coro).out_is_error = (*coro).header.await_is_error;
        *(*coro).out_value = (*coro).header.await_value;
        *(*coro).out_error = (*coro).header.await_error;
        *(*coro).done = true;
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut());
        RtCoroStatus::Done
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
        RtCoroStatus::Pending
      }
      1 => {
        *(*coro).out_is_error = (*coro).header.await_is_error;
        *(*coro).out_value = (*coro).header.await_value;
        *(*coro).out_error = (*coro).header.await_error;
        *(*coro).done = true;
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut());
        RtCoroStatus::Done
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

  let mut coro = Box::new(AwaitPromiseCoroutine {
    header: RtCoroutineHeader {
      resume: await_promise_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    awaited: p,
    out_is_error: &mut is_error,
    out_value: &mut value,
    out_error: &mut error,
    done: &mut done,
  });

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

  let mut coro = Box::new(AwaitPromiseCoroutine {
    header: RtCoroutineHeader {
      resume: await_promise_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    awaited: dst,
    out_is_error: &mut is_error,
    out_value: &mut value,
    out_error: &mut error,
    done: &mut done,
  });

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

  let mut coro = Box::new(AwaitPromiseCoroutine {
    header: RtCoroutineHeader {
      resume: await_promise_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    awaited: dst,
    out_is_error: &mut is_error,
    out_value: &mut value,
    out_error: &mut error,
    done: &mut done,
  });

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
  src: PromiseRef,
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

  let mut coro = Box::new(AwaitPromiseCoroutine {
    header: RtCoroutineHeader {
      resume: await_promise_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    awaited: dst,
    out_is_error: &mut is_error,
    out_value: &mut value,
    out_error: &mut error,
    done: &mut done,
  });

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
  src: PromiseRef,
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

  let mut coro = Box::new(AwaitPromiseCoroutine {
    header: RtCoroutineHeader {
      resume: await_promise_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    awaited: dst,
    out_is_error: &mut is_error,
    out_value: &mut value,
    out_error: &mut error,
    done: &mut done,
  });

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

  let mut coro = Box::new(AwaitPromiseCoroutine {
    header: RtCoroutineHeader {
      resume: await_promise_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    awaited: dst,
    out_is_error: &mut is_error,
    out_value: &mut value,
    out_error: &mut error,
    done: &mut done,
  });

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

  let mut coro = Box::new(AwaitValueCoroutine {
    header: RtCoroutineHeader {
      resume: await_value_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    awaited,
    out_is_error: &mut is_error,
    out_value: &mut value,
    out_error: &mut error,
    done: &mut done,
  });

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
