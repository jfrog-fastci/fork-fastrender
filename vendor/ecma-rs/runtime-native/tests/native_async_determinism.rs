use core::ffi::c_void;
use core::ptr::null_mut;
use runtime_native::async_abi::{
  Coroutine, CoroutineRef, CoroutineStep, CoroutineVTable, PromiseHeader, PromiseRef,
  CORO_FLAG_RUNTIME_OWNS_FRAME, RT_ASYNC_ABI_VERSION,
};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseRef as AbiPromiseRef;
use runtime_native::RtShapeId;
use std::sync::atomic::{AtomicU8, AtomicUsize};
use std::sync::Mutex;

fn abi_promise_from_header(p: *mut PromiseHeader) -> AbiPromiseRef {
  AbiPromiseRef(p.cast::<c_void>())
}

#[repr(C)]
struct OrderCoro {
  header: Coroutine,
  state: u32,
  id: u32,
  log: *const Mutex<Vec<u32>>,
  awaited: PromiseRef,
}

unsafe extern "C" fn order_resume(coro: *mut Coroutine) -> CoroutineStep {
  let coro = coro as *mut OrderCoro;
  assert!(!coro.is_null());

  match (*coro).state {
    0 => {
      (*coro).state = 1;
      CoroutineStep::await_((*coro).awaited)
    }
    1 => {
      unsafe { &*(*coro).log }.lock().unwrap().push((*coro).id);
      runtime_native::rt_promise_fulfill(abi_promise_from_header((*coro).header.promise));
      CoroutineStep::complete()
    }
    other => panic!("unexpected coroutine state: {other}"),
  }
}

unsafe extern "C" fn order_destroy(coro: CoroutineRef) {
  drop(Box::from_raw(coro as *mut OrderCoro));
}

static ORDER_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: order_resume,
  destroy: order_destroy,
  promise_size: core::mem::size_of::<PromiseHeader>() as u32,
  promise_align: core::mem::align_of::<PromiseHeader>() as u32,
  promise_shape_id: RtShapeId::INVALID,
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[test]
fn native_async_promise_waiters_resume_in_fifo_order() {
  let _rt = TestRuntimeGuard::new();

  let log: &'static Mutex<Vec<u32>> = Box::leak(Box::new(Mutex::new(Vec::new())));

  // Standalone awaited promise header (pending initially).
  let awaited = Box::new(PromiseHeader {
    state: AtomicU8::new(PromiseHeader::PENDING),
    reactions: AtomicUsize::new(0),
    flags: AtomicU8::new(0),
  });
  let awaited_ptr: PromiseRef = Box::into_raw(awaited);
  unsafe {
    runtime_native::rt_promise_init(abi_promise_from_header(awaited_ptr));
  }

  let coro1 = Box::new(OrderCoro {
    header: Coroutine {
      vtable: &ORDER_VTABLE,
      promise: null_mut(),
      next_waiter: null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    state: 0,
    id: 1,
    log,
    awaited: awaited_ptr,
  });
  let coro2 = Box::new(OrderCoro {
    header: Coroutine {
      vtable: &ORDER_VTABLE,
      promise: null_mut(),
      next_waiter: null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    state: 0,
    id: 2,
    log,
    awaited: awaited_ptr,
  });

  // Await registration order is defined by program order: coro1 then coro2.
  unsafe {
    let _p1 = runtime_native::rt_async_spawn(Box::into_raw(coro1) as CoroutineRef);
    let _p2 = runtime_native::rt_async_spawn(Box::into_raw(coro2) as CoroutineRef);
  }

  unsafe {
    runtime_native::rt_promise_fulfill(abi_promise_from_header(awaited_ptr));
  }

  while runtime_native::rt_async_poll() {}

  assert_eq!(&*log.lock().unwrap(), &[1, 2]);

  unsafe {
    drop(Box::from_raw(awaited_ptr));
  }
}

#[repr(C)]
struct SettledAwaitCoro {
  header: Coroutine,
  state: u32,
  completed: *mut bool,
  awaited: PromiseRef,
}

unsafe extern "C" fn settled_await_resume(coro: *mut Coroutine) -> CoroutineStep {
  let coro = coro as *mut SettledAwaitCoro;
  assert!(!coro.is_null());

  match (*coro).state {
    0 => {
      (*coro).state = 1;
      CoroutineStep::await_((*coro).awaited)
    }
    1 => {
      *(*coro).completed = true;
      runtime_native::rt_promise_fulfill(abi_promise_from_header((*coro).header.promise));
      CoroutineStep::complete()
    }
    other => panic!("unexpected coroutine state: {other}"),
  }
}

unsafe extern "C" fn settled_await_destroy(coro: CoroutineRef) {
  drop(Box::from_raw(coro as *mut SettledAwaitCoro));
}

static SETTLED_AWAIT_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: settled_await_resume,
  destroy: settled_await_destroy,
  promise_size: core::mem::size_of::<PromiseHeader>() as u32,
  promise_align: core::mem::align_of::<PromiseHeader>() as u32,
  promise_shape_id: RtShapeId::INVALID,
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[test]
fn native_async_strict_await_yields_on_already_settled_promise() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_async_set_strict_await_yields(true);

  let awaited = Box::new(PromiseHeader {
    state: AtomicU8::new(PromiseHeader::PENDING),
    reactions: AtomicUsize::new(0),
    flags: AtomicU8::new(0),
  });
  let awaited_ptr: PromiseRef = Box::into_raw(awaited);
  unsafe {
    runtime_native::rt_promise_init(abi_promise_from_header(awaited_ptr));
    runtime_native::rt_promise_fulfill(abi_promise_from_header(awaited_ptr));
  }

  let mut completed = false;
  let coro = Box::new(SettledAwaitCoro {
    header: Coroutine {
      vtable: &SETTLED_AWAIT_VTABLE,
      promise: null_mut(),
      next_waiter: null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    state: 0,
    completed: &mut completed,
    awaited: awaited_ptr,
  });

  unsafe {
    let _promise = runtime_native::rt_async_spawn(Box::into_raw(coro) as CoroutineRef);
  }

  assert!(
    !completed,
    "strict await should not resume synchronously inside rt_async_spawn"
  );

  runtime_native::rt_async_poll();
  assert!(completed);

  // Restore default for any other tests that don't use `TestRuntimeGuard`.
  runtime_native::rt_async_set_strict_await_yields(false);

  unsafe {
    drop(Box::from_raw(awaited_ptr));
  }
}

#[test]
fn native_async_non_strict_await_resumes_synchronously_on_already_settled_promise() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_async_set_strict_await_yields(false);

  let awaited = Box::new(PromiseHeader {
    state: AtomicU8::new(PromiseHeader::PENDING),
    reactions: AtomicUsize::new(0),
    flags: AtomicU8::new(0),
  });
  let awaited_ptr: PromiseRef = Box::into_raw(awaited);
  unsafe {
    runtime_native::rt_promise_init(abi_promise_from_header(awaited_ptr));
    runtime_native::rt_promise_fulfill(abi_promise_from_header(awaited_ptr));
  }

  let mut completed = false;
  let coro = Box::new(SettledAwaitCoro {
    header: Coroutine {
      vtable: &SETTLED_AWAIT_VTABLE,
      promise: null_mut(),
      next_waiter: null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    state: 0,
    completed: &mut completed,
    awaited: awaited_ptr,
  });

  unsafe {
    let _promise = runtime_native::rt_async_spawn(Box::into_raw(coro) as CoroutineRef);
  }

  assert!(
    completed,
    "non-strict await should resume synchronously inside rt_async_spawn"
  );
  assert!(!runtime_native::rt_async_poll());

  unsafe {
    drop(Box::from_raw(awaited_ptr));
  }
}
