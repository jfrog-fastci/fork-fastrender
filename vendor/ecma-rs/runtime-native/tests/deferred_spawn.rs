use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, RtShapeDescriptor, RtShapeId, ValueRef};
use runtime_native::gc::ObjHeader;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
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
        size: mem::size_of::<GcBox<CounterCoro>>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<YieldOnceCoro>>() as u32,
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

#[repr(C)]
struct CounterCoro {
  header: RtCoroutineHeader,
  counter: *const AtomicUsize,
}

extern "C" fn counter_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  // Safety: CounterCoro is #[repr(C)] and RtCoroutineHeader is its first field.
  let coro = coro as *mut CounterCoro;
  assert!(!coro.is_null());
  unsafe {
    (&*(*coro).counter).fetch_add(1, Ordering::SeqCst);
    runtime_native::rt_promise_resolve_legacy(
      PromiseRef((*coro).header.promise.cast()),
      core::ptr::null_mut::<core::ffi::c_void>(),
    );
  }
  RtCoroStatus::Done
}

#[test]
fn spawn_vs_deferred_spawn_immediacy() {
  let _rt = TestRuntimeGuard::new();

  // `rt_async_spawn_legacy` resumes the coroutine during the call.
  let counter = AtomicUsize::new(0);
  let coro_obj = unsafe { alloc_pinned::<CounterCoro>(RtShapeId(1)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: counter_resume,
    promise: core::ptr::null_mut(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.counter = &counter;

  let promise = runtime_native::rt_async_spawn_legacy(&mut coro.header);
  assert_eq!(counter.load(Ordering::SeqCst), 1);
  assert_eq!(promise.0, coro.header.promise.cast());

  // `rt_async_spawn_deferred_legacy` only enqueues; no resume until `rt_async_poll_legacy`.
  let counter = AtomicUsize::new(0);
  let coro_obj = unsafe { alloc_pinned::<CounterCoro>(RtShapeId(1)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: counter_resume,
    promise: core::ptr::null_mut(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.counter = &counter;

  let promise = runtime_native::rt_async_spawn_deferred_legacy(&mut coro.header);
  assert_eq!(counter.load(Ordering::SeqCst), 0);
  assert_eq!(promise.0, coro.header.promise.cast());

  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[repr(C)]
struct YieldOnceCoro {
  header: RtCoroutineHeader,
  started: *mut bool,
  completed: *mut bool,
  awaited: PromiseRef,
}

extern "C" fn yield_once_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut YieldOnceCoro;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        *(*coro).started = true;
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        assert_eq!((*coro).header.await_is_error, 0);
        assert_eq!((*coro).header.await_value as usize, 0xCAFE_BABE);

        *(*coro).completed = true;
        runtime_native::rt_promise_resolve_legacy(
          PromiseRef((*coro).header.promise.cast()),
          core::ptr::null_mut::<core::ffi::c_void>(),
        );
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn deferred_spawn_registers_waiter_when_polled() {
  let _rt = TestRuntimeGuard::new();

  let awaited = runtime_native::rt_promise_new_legacy();
  let mut started = false;
  let mut completed = false;
  let coro_obj = unsafe { alloc_pinned::<YieldOnceCoro>(RtShapeId(2)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: yield_once_resume,
    promise: core::ptr::null_mut(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.started = &mut started;
  coro.completed = &mut completed;
  coro.awaited = awaited;

  let promise = runtime_native::rt_async_spawn_deferred_legacy(&mut coro.header);
  assert_eq!(promise.0, coro.header.promise.cast());
  assert!(!started);
  assert!(!completed);

  // First poll: coroutine runs and awaits `awaited`, registering a continuation.
  while runtime_native::rt_async_poll_legacy() {}
  assert!(started);
  assert!(!completed);

  // Settling the awaited promise should enqueue a microtask (not resume immediately).
  runtime_native::rt_promise_resolve_legacy(awaited, 0xCAFE_BABEusize as ValueRef);
  assert!(!completed);

  while runtime_native::rt_async_poll_legacy() {}
  assert!(completed);
}
