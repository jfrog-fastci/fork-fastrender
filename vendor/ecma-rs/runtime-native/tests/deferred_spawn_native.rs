use core::ptr::null_mut;
use runtime_native::async_abi::{
  Coroutine, CoroutineRef, CoroutineStep, CoroutineStepTag, CoroutineVTable, PromiseHeader,
  CORO_FLAG_RUNTIME_OWNS_FRAME, RT_ASYNC_ABI_VERSION,
};
use runtime_native::test_util::{new_promise_header_pending, TestRuntimeGuard};
use runtime_native::CoroutineId;
use runtime_native::PromiseRef as AbiPromiseRef;
use runtime_native::RtShapeId;
use std::sync::atomic::{AtomicUsize, Ordering};

#[repr(C)]
struct TestPromise {
  header: PromiseHeader,
  _padding: AtomicUsize,
}

impl TestPromise {
  fn new_pending() -> Self {
    Self {
      header: new_promise_header_pending(),
      _padding: AtomicUsize::new(0),
    }
  }
}

fn abi_promise_from_header(p: *mut PromiseHeader) -> AbiPromiseRef {
  AbiPromiseRef(p.cast())
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

  let coro_ptr: *mut Coroutine = &mut coro.header;
  let handle = runtime_native::rt_handle_alloc((coro_ptr as *mut u8).cast());
  let coro_id = CoroutineId(handle);
  let promise = unsafe { runtime_native::rt_async_spawn(coro_id) };
  assert_eq!(counter.load(Ordering::SeqCst), 1);
  assert_eq!(promise.0, coro.header.promise.cast());
  assert_eq!(promise_ptr.load(Ordering::SeqCst), coro.header.promise as usize);
  // Coroutine completed synchronously; runtime must have freed the handle.
  assert!(runtime_native::rt_handle_load(handle).is_null());

  // `rt_async_spawn_deferred` only enqueues; no resume until `rt_async_poll`.
  let counter = AtomicUsize::new(0);
  let promise_ptr = AtomicUsize::new(0);
  let coro = Box::new(CounterCoro {
    header: Coroutine {
      vtable: &COUNTER_VTABLE,
      promise: null_mut(),
      next_waiter: null_mut(),
      // Deferred spawn always stores the coroutine across turns; treat it as runtime-owned.
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    counter: &counter,
    promise_ptr: &promise_ptr,
  });
  let coro = Box::into_raw(coro);

  let handle = runtime_native::rt_handle_alloc(coro as *mut u8);
  let coro_id = CoroutineId(handle);
  let promise = unsafe { runtime_native::rt_async_spawn_deferred(coro_id) };
  assert_eq!(counter.load(Ordering::SeqCst), 0);
  assert_eq!(promise.0, unsafe { (*coro).header.promise.cast() });
  assert_eq!(promise_ptr.load(Ordering::SeqCst), 0);

  while runtime_native::rt_async_poll() {}
  assert_eq!(counter.load(Ordering::SeqCst), 1);
  assert_eq!(promise_ptr.load(Ordering::SeqCst), promise.0 as usize);
  // Coroutine completed in a later microtask; runtime must have freed the handle.
  assert!(runtime_native::rt_handle_load(handle).is_null());
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
  let coro = Box::into_raw(coro);

  let handle = runtime_native::rt_handle_alloc(coro as *mut u8);
  let coro_id = CoroutineId(handle);
  let promise = unsafe { runtime_native::rt_async_spawn_deferred(coro_id) };
  assert_eq!(promise.0, unsafe { (*coro).header.promise.cast() });
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
  // Coroutine completed after being resumed through a promise reaction; handle must be freed.
  assert!(runtime_native::rt_handle_load(handle).is_null());
}
