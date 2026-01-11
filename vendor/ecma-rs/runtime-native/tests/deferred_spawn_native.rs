use core::ffi::c_void;
use core::ptr::null_mut;
use runtime_native::async_abi::{
  Coroutine, CoroutineRef, CoroutineStep, CoroutineStepTag, CoroutineVTable, PromiseHeader,
  RT_ASYNC_ABI_VERSION, CORO_FLAG_RUNTIME_OWNS_FRAME,
};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseRef as AbiPromiseRef;
use runtime_native::RtShapeId;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

#[repr(C)]
struct TestPromise {
  header: PromiseHeader,
  _padding: AtomicUsize,
}

impl TestPromise {
  fn new_pending() -> Self {
    Self {
      header: PromiseHeader {
        state: AtomicU8::new(PromiseHeader::PENDING),
        waiters: AtomicUsize::new(0),
        flags: AtomicU8::new(0),
      },
      _padding: AtomicUsize::new(0),
    }
  }
}

fn abi_promise_from_header(p: *mut PromiseHeader) -> AbiPromiseRef {
  AbiPromiseRef(p.cast::<c_void>())
}

#[repr(C)]
struct CounterCoro {
  header: Coroutine,
  counter: *const AtomicUsize,
  promise_ptr: *const AtomicUsize,
}

unsafe extern "C" fn counter_resume(coro: *mut Coroutine) -> CoroutineStep {
  // Safety: CounterCoro is #[repr(C)] and Coroutine is its first field.
  let coro = coro as *mut CounterCoro;
  assert!(!coro.is_null());
  if !(*coro).promise_ptr.is_null() {
    (&*(*coro).promise_ptr).store((*coro).header.promise as usize, Ordering::SeqCst);
  }
  (&*(*coro).counter).fetch_add(1, Ordering::SeqCst);
  runtime_native::rt_promise_fulfill(abi_promise_from_header((*coro).header.promise));
  CoroutineStep::complete()
}

unsafe extern "C" fn counter_destroy(coro: CoroutineRef) {
  if coro.is_null() {
    return;
  }
  if unsafe { (*coro).flags } & CORO_FLAG_RUNTIME_OWNS_FRAME == 0 {
    return;
  }
  // Safety: CounterCoro is #[repr(C)] and Coroutine is its first field. For runtime-owned
  // coroutines, the test passes a `Box::into_raw` pointer to the runtime, and the runtime
  // calls `destroy` exactly once on completion/cancellation.
  drop(Box::from_raw(coro as *mut CounterCoro));
}

static COUNTER_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: counter_resume,
  destroy: counter_destroy,
  promise_size: core::mem::size_of::<TestPromise>() as u32,
  promise_align: core::mem::align_of::<TestPromise>() as u32,
  promise_shape_id: RtShapeId::INVALID,
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[test]
fn spawn_vs_deferred_spawn_immediacy_native() {
  let _rt = TestRuntimeGuard::new();

  // `rt_async_spawn` resumes the coroutine during the call.
  let counter = AtomicUsize::new(0);
  let promise_ptr = AtomicUsize::new(0);
  let mut coro = Box::new(CounterCoro {
    header: Coroutine {
      vtable: &COUNTER_VTABLE,
      promise: null_mut(),
      next_waiter: null_mut(),
      flags: 0,
    },
    counter: &counter,
    promise_ptr: &promise_ptr,
  });

  let promise = unsafe { runtime_native::rt_async_spawn(&mut coro.header) };
  assert_eq!(counter.load(Ordering::SeqCst), 1);
  assert_eq!(promise.0, coro.header.promise.cast::<c_void>());
  assert_eq!(promise_ptr.load(Ordering::SeqCst), coro.header.promise as usize);

  // `rt_async_spawn_deferred` only enqueues; no resume until `rt_async_poll`.
  let counter = AtomicUsize::new(0);
  let promise_ptr = AtomicUsize::new(0);
  let coro = Box::new(CounterCoro {
    header: Coroutine {
      vtable: &COUNTER_VTABLE,
      promise: null_mut(),
      next_waiter: null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    counter: &counter,
    promise_ptr: &promise_ptr,
  });
  let coro_ptr = Box::into_raw(coro);

  let promise = unsafe { runtime_native::rt_async_spawn_deferred(&mut (*coro_ptr).header) };
  assert_eq!(counter.load(Ordering::SeqCst), 0);
  assert_eq!(promise_ptr.load(Ordering::SeqCst), 0);
  assert_eq!(promise.0, unsafe { (*coro_ptr).header.promise.cast::<c_void>() });

  while runtime_native::rt_async_poll() {}
  assert_eq!(counter.load(Ordering::SeqCst), 1);
  assert_eq!(promise_ptr.load(Ordering::SeqCst), promise.0 as usize);
}

#[repr(C)]
struct YieldOnceCoro {
  header: Coroutine,
  state: u32,
  promise_ptr: *const AtomicUsize,
  started: *mut bool,
  completed: *mut bool,
  awaited: *mut PromiseHeader,
}

unsafe extern "C" fn yield_once_resume(coro: *mut Coroutine) -> CoroutineStep {
  let coro = coro as *mut YieldOnceCoro;
  assert!(!coro.is_null());
  if !(*coro).promise_ptr.is_null() {
    (&*(*coro).promise_ptr).store((*coro).header.promise as usize, Ordering::SeqCst);
  }

  match (*coro).state {
    0 => {
      *(*coro).started = true;
      (*coro).state = 1;
      CoroutineStep {
        tag: CoroutineStepTag::Await,
        await_promise: (*coro).awaited,
      }
    }
    1 => {
      *(*coro).completed = true;
      runtime_native::rt_promise_fulfill(abi_promise_from_header((*coro).header.promise));
      CoroutineStep::complete()
    }
    other => panic!("unexpected coroutine state: {other}"),
  }
}

unsafe extern "C" fn yield_once_destroy(coro: CoroutineRef) {
  if coro.is_null() {
    return;
  }
  if unsafe { (*coro).flags } & CORO_FLAG_RUNTIME_OWNS_FRAME == 0 {
    return;
  }
  // Safety: YieldOnceCoro is #[repr(C)] and Coroutine is its first field. For runtime-owned
  // coroutines, the test passes a `Box::into_raw` pointer to the runtime, and the runtime
  // calls `destroy` exactly once on completion/cancellation.
  drop(Box::from_raw(coro as *mut YieldOnceCoro));
}

static YIELD_ONCE_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: yield_once_resume,
  destroy: yield_once_destroy,
  promise_size: core::mem::size_of::<TestPromise>() as u32,
  promise_align: core::mem::align_of::<TestPromise>() as u32,
  promise_shape_id: RtShapeId::INVALID,
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[test]
fn deferred_spawn_registers_waiter_when_polled_native() {
  let _rt = TestRuntimeGuard::new();

  let mut awaited = Box::new(TestPromise::new_pending());
  let awaited_ptr: *mut PromiseHeader = &mut awaited.header;
  unsafe {
    runtime_native::rt_promise_init(abi_promise_from_header(awaited_ptr));
  }

  let promise_ptr = AtomicUsize::new(0);
  let mut started = false;
  let mut completed = false;
  let coro = Box::new(YieldOnceCoro {
    header: Coroutine {
      vtable: &YIELD_ONCE_VTABLE,
      promise: null_mut(),
      next_waiter: null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    state: 0,
    promise_ptr: &promise_ptr,
    started: &mut started,
    completed: &mut completed,
    awaited: awaited_ptr,
  });
  let coro_ptr = Box::into_raw(coro);

  let promise = unsafe { runtime_native::rt_async_spawn_deferred(&mut (*coro_ptr).header) };
  assert_eq!(promise.0, unsafe { (*coro_ptr).header.promise.cast::<c_void>() });
  assert!(!started);
  assert!(!completed);

  // First poll: coroutine runs and awaits `awaited`, registering a continuation.
  while runtime_native::rt_async_poll() {}
  assert!(started);
  assert!(!completed);
  assert_eq!(promise_ptr.load(Ordering::SeqCst), promise.0 as usize);

  // Settling the awaited promise should enqueue a microtask (not resume immediately).
  unsafe {
    runtime_native::rt_promise_fulfill(abi_promise_from_header(awaited_ptr));
  }
  assert!(!completed);

  while runtime_native::rt_async_poll() {}
  assert!(completed);
}
