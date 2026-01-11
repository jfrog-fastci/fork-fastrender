use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, RtShapeDescriptor, RtShapeId, ValueRef};
use runtime_native::async_abi::PromiseHeader;
use runtime_native::gc::ObjHeader;
use runtime_native::shape_table;
use runtime_native::test_util::{PromiseRejectionEvent, TestRuntimeGuard};
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
    static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
      size: mem::size_of::<GcBox<PropagatingAwaitCoroutine>>() as u32,
      align: 16,
      flags: 0,
      ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
      ptr_offsets_len: 0,
      reserved: 0,
    }];
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

unsafe fn alloc_pinned<T>(shape: RtShapeId) -> *mut GcBox<T> {
  ensure_shape_table();
  runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<T>>(), shape).cast::<GcBox<T>>()
}

extern "C" fn noop(_data: *mut u8) {}

#[repr(C)]
struct PropagatingAwaitCoroutine {
  header: RtCoroutineHeader,
  awaited: PromiseRef,
}

extern "C" fn propagating_await_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut PropagatingAwaitCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        assert_eq!((*coro).header.await_is_error, 1);
        runtime_native::rt_promise_reject_legacy((*coro).header.promise, (*coro).header.await_error);
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn awaited_rejection_is_not_reported_as_unhandled() {
  let _rt = TestRuntimeGuard::new();

  let p = runtime_native::rt_promise_new_legacy();
  let err = 0xDEAD_BEEFu64 as usize as ValueRef;
  runtime_native::rt_promise_reject_legacy(p, err);

  let coro_obj = unsafe { alloc_pinned::<PropagatingAwaitCoroutine>(RtShapeId(1)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: propagating_await_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.awaited = p;

  let coro_promise = runtime_native::rt_async_spawn_legacy(&mut coro.header);
  // Avoid spurious unhandled rejections for the coroutine promise; we only care about `p`.
  runtime_native::rt_promise_then_legacy(coro_promise, noop, core::ptr::null_mut());

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    Vec::<PromiseRejectionEvent>::new()
  );
}

#[test]
fn awaiting_after_unhandled_rejection_reports_rejectionhandled() {
  let _rt = TestRuntimeGuard::new();

  let p = runtime_native::rt_promise_new_legacy();
  let err = 0xBAD0_C0DEu64 as usize as ValueRef;
  runtime_native::rt_promise_reject_legacy(p, err);

  // Microtask checkpoint: should report as unhandled.
  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    vec![PromiseRejectionEvent::UnhandledRejection { promise: p }]
  );

  let coro_obj = unsafe { alloc_pinned::<PropagatingAwaitCoroutine>(RtShapeId(1)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: propagating_await_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.awaited = p;

  let coro_promise = runtime_native::rt_async_spawn_legacy(&mut coro.header);
  runtime_native::rt_promise_then_legacy(coro_promise, noop, core::ptr::null_mut());

  // Next microtask checkpoint: should report `rejectionhandled` for `p`.
  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    vec![PromiseRejectionEvent::RejectionHandled { promise: p }]
  );
}

#[test]
fn native_promise_rejection_is_reported_as_unhandled() {
  let _rt = TestRuntimeGuard::new();

  let mut promise_header = Box::new(PromiseHeader {
    state: core::sync::atomic::AtomicU8::new(0),
    waiters: core::sync::atomic::AtomicUsize::new(0),
    flags: core::sync::atomic::AtomicU8::new(0),
  });
  let p = PromiseRef((&mut *promise_header as *mut PromiseHeader).cast());

  unsafe {
    runtime_native::rt_promise_init(p);
    runtime_native::rt_promise_reject(p);
  }

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    vec![PromiseRejectionEvent::UnhandledRejection { promise: p }]
  );
}

#[test]
fn native_promise_mark_handled_before_checkpoint_suppresses_unhandled() {
  let _rt = TestRuntimeGuard::new();

  let mut promise_header = Box::new(PromiseHeader {
    state: core::sync::atomic::AtomicU8::new(0),
    waiters: core::sync::atomic::AtomicUsize::new(0),
    flags: core::sync::atomic::AtomicU8::new(0),
  });
  let p = PromiseRef((&mut *promise_header as *mut PromiseHeader).cast());

  unsafe {
    runtime_native::rt_promise_init(p);
    runtime_native::rt_promise_reject(p);
    runtime_native::rt_promise_mark_handled(p);
  }

  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    Vec::<PromiseRejectionEvent>::new()
  );
}

#[test]
fn native_promise_mark_handled_after_unhandled_reports_rejectionhandled() {
  let _rt = TestRuntimeGuard::new();

  let mut promise_header = Box::new(PromiseHeader {
    state: core::sync::atomic::AtomicU8::new(0),
    waiters: core::sync::atomic::AtomicUsize::new(0),
    flags: core::sync::atomic::AtomicU8::new(0),
  });
  let p = PromiseRef((&mut *promise_header as *mut PromiseHeader).cast());

  unsafe {
    runtime_native::rt_promise_init(p);
    runtime_native::rt_promise_reject(p);
  }

  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    vec![PromiseRejectionEvent::UnhandledRejection { promise: p }]
  );

  unsafe {
    runtime_native::rt_promise_mark_handled(p);
  }
  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    vec![PromiseRejectionEvent::RejectionHandled { promise: p }]
  );
}

#[test]
fn native_promise_rejection_reports_unhandled_and_rejectionhandled_when_awaited_later() {
  use runtime_native::abi::RtShapeId;
  use runtime_native::async_abi::{
    Coroutine, CoroutineRef, CoroutineStep, CoroutineStepTag, CoroutineVTable, CORO_FLAG_RUNTIME_OWNS_FRAME,
    RT_ASYNC_ABI_VERSION,
  };
  use std::sync::atomic::{AtomicU8, AtomicUsize};

  #[repr(C)]
  struct AwaitOnceCoro {
    header: Coroutine,
    state: u32,
    awaited: *mut PromiseHeader,
  }

  #[inline]
  fn abi_promise_from_header(p: *mut PromiseHeader) -> PromiseRef {
    PromiseRef(p.cast())
  }

  unsafe extern "C" fn await_once_resume(coro: *mut Coroutine) -> CoroutineStep {
    let coro = coro as *mut AwaitOnceCoro;
    assert!(!coro.is_null());
    match (*coro).state {
      0 => {
        (*coro).state = 1;
        CoroutineStep {
          tag: CoroutineStepTag::Await,
          await_promise: (*coro).awaited,
        }
      }
      1 => {
        runtime_native::rt_promise_fulfill(abi_promise_from_header((*coro).header.promise));
        CoroutineStep::complete()
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }

  unsafe extern "C" fn await_once_destroy(coro: CoroutineRef) {
    drop(Box::from_raw(coro as *mut AwaitOnceCoro));
  }

  static AWAIT_ONCE_VTABLE: CoroutineVTable = CoroutineVTable {
    resume: await_once_resume,
    destroy: await_once_destroy,
    promise_size: core::mem::size_of::<PromiseHeader>() as u32,
    promise_align: core::mem::align_of::<PromiseHeader>() as u32,
    promise_shape_id: RtShapeId::INVALID,
    abi_version: RT_ASYNC_ABI_VERSION,
    reserved: [0; 4],
  };

  let _rt = TestRuntimeGuard::new();

  // Allocate a native PromiseHeader directly and reject it with no handlers.
  let mut p = Box::new(PromiseHeader {
    state: AtomicU8::new(PromiseHeader::PENDING),
    waiters: AtomicUsize::new(0),
    flags: AtomicU8::new(0),
  });
  let p_ref = PromiseRef((&mut *p as *mut PromiseHeader).cast());
  unsafe {
    runtime_native::rt_promise_init(p_ref);
    runtime_native::rt_promise_reject(p_ref);
  }

  // Next microtask checkpoint: should report as unhandled.
  while runtime_native::rt_async_poll() {}
  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    vec![PromiseRejectionEvent::UnhandledRejection { promise: p_ref }]
  );

  // Attach an `await` handler later; this must trigger `rejectionhandled` even if the native
  // runtime takes the settled fast path (sync resumption) instead of registering a waiter.
  let coro = Box::new(AwaitOnceCoro {
    header: Coroutine {
      vtable: &AWAIT_ONCE_VTABLE,
      promise: core::ptr::null_mut(),
      next_waiter: core::ptr::null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    state: 0,
    awaited: (&mut *p as *mut PromiseHeader),
  });
  let coro_ref = Box::into_raw(coro) as *mut Coroutine;
  let _ = unsafe { runtime_native::rt_async_spawn(coro_ref) };

  while runtime_native::rt_async_poll() {}
  assert_eq!(
    runtime_native::test_util::drain_promise_rejection_events(),
    vec![PromiseRejectionEvent::RejectionHandled { promise: p_ref }]
  );
}
