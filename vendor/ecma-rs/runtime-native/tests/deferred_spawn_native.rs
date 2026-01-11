use core::ffi::c_void;
use core::ptr::null_mut;
use runtime_native::async_abi::{Coroutine, CoroutineStep, CoroutineStepTag, CoroutineVTable, PromiseHeader};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseRef as AbiPromiseRef;
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
        reactions: AtomicUsize::new(0),
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
}

extern "C" fn counter_resume(coro: *mut Coroutine) -> CoroutineStep {
  // Safety: CounterCoro is #[repr(C)] and Coroutine is its first field.
  let coro = coro as *mut CounterCoro;
  assert!(!coro.is_null());
  unsafe {
    (&*(*coro).counter).fetch_add(1, Ordering::SeqCst);
    runtime_native::rt_promise_fulfill(abi_promise_from_header((*coro).header.promise));
  }
  CoroutineStep::complete()
}

static COUNTER_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: counter_resume,
  promise_size: core::mem::size_of::<TestPromise>() as u32,
  promise_align: core::mem::align_of::<TestPromise>() as u32,
  promise_shape_id: 0,
  abi_version: 0,
  reserved: [0; 4],
};

#[test]
fn spawn_vs_deferred_spawn_immediacy_native() {
  let _rt = TestRuntimeGuard::new();

  // `rt_async_spawn` resumes the coroutine during the call.
  let counter = AtomicUsize::new(0);
  let mut coro = Box::new(CounterCoro {
    header: Coroutine {
      vtable: &COUNTER_VTABLE,
      promise: null_mut(),
      flags: 0,
    },
    counter: &counter,
  });

  let promise = unsafe { runtime_native::rt_async_spawn(&mut coro.header) };
  assert_eq!(counter.load(Ordering::SeqCst), 1);
  assert_eq!(promise.0, coro.header.promise.cast::<c_void>());

  // `rt_async_spawn_deferred` only enqueues; no resume until `rt_async_poll`.
  let counter = AtomicUsize::new(0);
  let mut coro = Box::new(CounterCoro {
    header: Coroutine {
      vtable: &COUNTER_VTABLE,
      promise: null_mut(),
      flags: 0,
    },
    counter: &counter,
  });

  let promise = unsafe { runtime_native::rt_async_spawn_deferred(&mut coro.header) };
  assert_eq!(counter.load(Ordering::SeqCst), 0);
  assert_eq!(promise.0, coro.header.promise.cast::<c_void>());

  while runtime_native::rt_async_poll() {}
  assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[repr(C)]
struct YieldOnceCoro {
  header: Coroutine,
  state: u32,
  started: *mut bool,
  completed: *mut bool,
  awaited: *mut PromiseHeader,
}

extern "C" fn yield_once_resume(coro: *mut Coroutine) -> CoroutineStep {
  let coro = coro as *mut YieldOnceCoro;
  assert!(!coro.is_null());

  unsafe {
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
}

static YIELD_ONCE_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: yield_once_resume,
  promise_size: core::mem::size_of::<TestPromise>() as u32,
  promise_align: core::mem::align_of::<TestPromise>() as u32,
  promise_shape_id: 0,
  abi_version: 0,
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

  let mut started = false;
  let mut completed = false;
  let mut coro = Box::new(YieldOnceCoro {
    header: Coroutine {
      vtable: &YIELD_ONCE_VTABLE,
      promise: null_mut(),
      flags: 0,
    },
    state: 0,
    started: &mut started,
    completed: &mut completed,
    awaited: awaited_ptr,
  });

  let promise = unsafe { runtime_native::rt_async_spawn_deferred(&mut coro.header) };
  assert_eq!(promise.0, coro.header.promise.cast::<c_void>());
  assert!(!started);
  assert!(!completed);

  // First poll: coroutine runs and awaits `awaited`, registering a continuation.
  while runtime_native::rt_async_poll() {}
  assert!(started);
  assert!(!completed);

  // Settling the awaited promise should enqueue a microtask (not resume immediately).
  unsafe {
    runtime_native::rt_promise_fulfill(abi_promise_from_header(awaited_ptr));
  }
  assert!(!completed);

  while runtime_native::rt_async_poll() {}
  assert!(completed);
}

