use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, RtShapeDescriptor, RtShapeId, ValueRef};
use runtime_native::async_abi::{
  Coroutine, CoroutineStep, CoroutineVTable, PromiseHeader, CORO_FLAG_RUNTIME_OWNS_FRAME, RT_ASYNC_ABI_VERSION,
};
use runtime_native::gc::ObjHeader;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::CoroutineId;
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, Once};

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
      size: mem::size_of::<GcBox<LogCoroutine>>() as u32,
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

#[repr(C)]
struct LogCoroutine {
  header: RtCoroutineHeader,
  id: u32,
  log: *const Mutex<Vec<u32>>,
  awaited: PromiseRef,
}

extern "C" fn log_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  // Safety: LogCoroutine is #[repr(C)] and RtCoroutineHeader is its first field.
  let coro = coro as *mut LogCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        let log = &*(*coro).log;
        log.lock().unwrap().push((*coro).id);
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[repr(C)]
struct LogCtx {
  log: *const Mutex<Vec<u32>>,
  id: u32,
}

extern "C" fn push_log(data: *mut u8) {
  let ctx = unsafe { &*(data as *const LogCtx) };
  let log = unsafe { &*ctx.log };
  log.lock().unwrap().push(ctx.id);
}

#[test]
fn await_and_then_share_single_reaction_list_with_fifo_ordering() {
  let _rt = TestRuntimeGuard::new();
  let awaited = runtime_native::rt_promise_new_legacy();
  let log: &'static Mutex<Vec<u32>> = Box::leak(Box::new(Mutex::new(Vec::new())));

  let coro_obj = unsafe { alloc_pinned::<LogCoroutine>(RtShapeId(1)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: log_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.id = 1;
  coro.log = log;
  coro.awaited = awaited;

  // Register the await reaction first (via spawning the coroutine).
  runtime_native::rt_async_spawn_legacy(&mut coro.header);

  // Then register an explicit `then` callback.
  let then_ctx: &'static LogCtx = Box::leak(Box::new(LogCtx { log, id: 2 }));
  runtime_native::rt_promise_then_legacy(awaited, push_log, then_ctx as *const LogCtx as *mut u8);

  runtime_native::rt_promise_resolve_legacy(awaited, 0x1234usize as ValueRef);
  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(&*log.lock().unwrap(), &[1, 2]);
}

#[test]
fn concurrent_registrations_do_not_lose_reactions() {
  let _rt = TestRuntimeGuard::new();

  let promise = runtime_native::rt_promise_new_legacy();
  let fired: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));

  extern "C" fn inc(data: *mut u8) {
    let c = unsafe { &*(data as *const AtomicUsize) };
    c.fetch_add(1, Ordering::SeqCst);
  }

  const THREADS: usize = 4;
  const PER_THREAD: usize = 200;
  const HALF: usize = PER_THREAD / 2;

  let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS + 1));
  let half_ready = std::sync::Arc::new(AtomicUsize::new(0));
  let settled = std::sync::Arc::new(AtomicBool::new(false));
  let mut joins = Vec::new();
  for _ in 0..THREADS {
    let b = barrier.clone();
    let half_ready = half_ready.clone();
    let settled = settled.clone();
    joins.push(std::thread::spawn(move || {
      b.wait();
      for i in 0..PER_THREAD {
        runtime_native::rt_promise_then_legacy(promise, inc, fired as *const AtomicUsize as *mut u8);
        if i + 1 == HALF {
          half_ready.fetch_add(1, Ordering::SeqCst);
          while !settled.load(Ordering::SeqCst) {
            std::thread::yield_now();
          }
        }
        if i % 17 == 0 {
          std::thread::yield_now();
        }
      }
    }));
  }

  // Start the registrars and resolve mid-flight to cover both pending + already-settled paths.
  barrier.wait();
  while half_ready.load(Ordering::SeqCst) < (THREADS / 2).max(1) {
    std::thread::yield_now();
  }
  runtime_native::rt_promise_resolve_legacy(promise, core::ptr::null_mut());
  settled.store(true, Ordering::SeqCst);

  for j in joins {
    j.join().unwrap();
  }

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(fired.load(Ordering::SeqCst), THREADS * PER_THREAD);
}

#[test]
fn reentrant_then_handlers_observe_microtask_checkpoint_ordering() {
  let _rt = TestRuntimeGuard::new();

  let promise = runtime_native::rt_promise_new_legacy();
  let log: &'static Mutex<Vec<u32>> = Box::leak(Box::new(Mutex::new(Vec::new())));

  #[repr(C)]
  struct ReentrantCtx {
    promise: PromiseRef,
    log: *const Mutex<Vec<u32>>,
  }

  extern "C" fn first(data: *mut u8) {
    let ctx = unsafe { &*(data as *const ReentrantCtx) };
    unsafe { &*ctx.log }.lock().unwrap().push(1);

    // Re-register a handler while processing reactions for an already-settled promise.
    let b_ctx: &'static LogCtx = Box::leak(Box::new(LogCtx {
      log: ctx.log,
      id: 3,
    }));
    runtime_native::rt_promise_then_legacy(ctx.promise, push_log, b_ctx as *const LogCtx as *mut u8);
  }

  let ctx: &'static ReentrantCtx = Box::leak(Box::new(ReentrantCtx { promise, log }));
  let c_ctx: &'static LogCtx = Box::leak(Box::new(LogCtx { log, id: 2 }));

  runtime_native::rt_promise_then_legacy(promise, first, ctx as *const ReentrantCtx as *mut u8);
  runtime_native::rt_promise_then_legacy(promise, push_log, c_ctx as *const LogCtx as *mut u8);

  runtime_native::rt_promise_resolve_legacy(promise, core::ptr::null_mut());
  while runtime_native::rt_async_poll_legacy() {}

  // `first` runs, queues a new microtask (id=3). The second handler (id=2) was already queued and
  // must run before the newly-queued handler.
  assert_eq!(&*log.lock().unwrap(), &[1, 2, 3]);
}

#[repr(C)]
struct AbiPromise {
  header: PromiseHeader,
  payload: u64,
}

#[test]
fn promise_fulfill_abi_drains_then_reactions() {
  let _rt = TestRuntimeGuard::new();

  let promise = Box::new(AbiPromise {
    header: PromiseHeader {
      state: AtomicU8::new(123),
      waiters: AtomicUsize::new(456),
      flags: AtomicU8::new(7),
    },
    payload: 0,
  });
  let promise = PromiseRef(Box::into_raw(promise).cast());

  unsafe {
    runtime_native::rt_promise_init(promise);
  }

  let fired: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
  extern "C" fn set_fired(data: *mut u8) {
    let flag = unsafe { &*(data as *const AtomicBool) };
    flag.store(true, Ordering::SeqCst);
  }

  runtime_native::rt_promise_then_legacy(promise, set_fired, fired as *const AtomicBool as *mut u8);
  unsafe {
    runtime_native::rt_promise_fulfill(promise);
  }

  while runtime_native::rt_async_poll() {}

  assert!(fired.load(Ordering::SeqCst));
  let state = unsafe { &*(promise.0 as *const PromiseHeader) }
    .state
    .load(Ordering::Acquire);
  assert_eq!(state, PromiseHeader::FULFILLED);
}

#[repr(C)]
struct AbiResultPromise {
  header: PromiseHeader,
  payload: u32,
}

#[repr(C)]
struct AbiCoroutineFrame {
  header: Coroutine,
  state: u32,
  awaited: *mut PromiseHeader,
  completed: *const AtomicBool,
}

unsafe extern "C" fn abi_coro_resume(coro: *mut Coroutine) -> CoroutineStep {
  let coro = coro as *mut AbiCoroutineFrame;
  match unsafe { (*coro).state } {
    0 => {
      unsafe {
        (*coro).state = 1;
      }
      CoroutineStep::await_(unsafe { (*coro).awaited })
    }
    1 => {
      let completed = unsafe { &*(*coro).completed };
      completed.store(true, Ordering::SeqCst);
      unsafe {
        runtime_native::rt_promise_fulfill(PromiseRef((*coro).header.promise.cast()));
      }
      unsafe {
        (*coro).state = 2;
      }
      CoroutineStep::complete()
    }
    _ => std::process::abort(),
  }
}

unsafe extern "C" fn abi_coro_destroy(coro: *mut Coroutine) {
  if coro.is_null() {
    return;
  }
  unsafe {
    drop(Box::from_raw(coro as *mut AbiCoroutineFrame));
  }
}

static ABI_CORO_VTABLE: CoroutineVTable = CoroutineVTable {
  resume: abi_coro_resume,
  destroy: abi_coro_destroy,
  promise_size: core::mem::size_of::<AbiResultPromise>() as u32,
  promise_align: core::mem::align_of::<AbiResultPromise>() as u32,
  promise_shape_id: runtime_native::RtShapeId::INVALID,
  abi_version: RT_ASYNC_ABI_VERSION,
  reserved: [0; 4],
};

#[test]
fn async_spawn_abi_resumes_on_awaited_promise_settlement() {
  let _rt = TestRuntimeGuard::new();

  let awaited = runtime_native::rt_promise_new_legacy();
  let awaited_header = awaited.0.cast::<PromiseHeader>();

  let completed: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
  let then_ran: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));

  let coro = Box::new(AbiCoroutineFrame {
    header: Coroutine {
      vtable: &ABI_CORO_VTABLE,
      promise: core::ptr::null_mut(),
      next_waiter: core::ptr::null_mut(),
      flags: CORO_FLAG_RUNTIME_OWNS_FRAME,
    },
    state: 0,
    awaited: awaited_header,
    completed,
  });
  let coro_ptr = Box::into_raw(coro);

  let handle = runtime_native::rt_handle_alloc(coro_ptr.cast::<u8>());
  let result_promise = unsafe { runtime_native::rt_async_spawn(CoroutineId(handle)) };
  assert!(!completed.load(Ordering::SeqCst));

  extern "C" fn set_then(data: *mut u8) {
    let flag = unsafe { &*(data as *const AtomicBool) };
    flag.store(true, Ordering::SeqCst);
  }
  runtime_native::rt_promise_then_legacy(result_promise, set_then, then_ran as *const AtomicBool as *mut u8);

  runtime_native::rt_promise_resolve_legacy(awaited, core::ptr::null_mut());
  while runtime_native::rt_async_poll() {}

  assert!(completed.load(Ordering::SeqCst));
  assert!(then_ran.load(Ordering::SeqCst));
  assert!(runtime_native::rt_handle_load(handle).is_null());

  let state = unsafe { &*(result_promise.0 as *const PromiseHeader) }
    .state
    .load(Ordering::Acquire);
  assert_eq!(state, PromiseHeader::FULFILLED);
}
