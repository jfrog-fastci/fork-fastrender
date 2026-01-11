use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, ValueRef};
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[repr(C)]
struct LogCoroutine {
  header: RtCoroutineHeader,
  id: u32,
  log: *const Mutex<Vec<u32>>,
  awaited: PromiseRef,
}

extern "C" fn log_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut LogCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        let log = &*(*coro).log;
        log.lock().unwrap().push((*coro).id);
        runtime_native::rt_promise_resolve((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
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

  let awaited = runtime_native::rt_promise_new();
  let log: &'static Mutex<Vec<u32>> = Box::leak(Box::new(Mutex::new(Vec::new())));

  let mut coro = Box::new(LogCoroutine {
    header: RtCoroutineHeader {
      resume: log_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    id: 1,
    log,
    awaited,
  });

  // Register the await reaction first (via spawning the coroutine).
  runtime_native::rt_async_spawn(&mut coro.header);

  // Then register an explicit `then` callback.
  let then_ctx: &'static LogCtx = Box::leak(Box::new(LogCtx { log, id: 2 }));
  runtime_native::rt_promise_then(awaited, push_log, then_ctx as *const LogCtx as *mut u8);

  runtime_native::rt_promise_resolve(awaited, 0x1234usize as ValueRef);
  while runtime_native::rt_async_poll() {}

  assert_eq!(&*log.lock().unwrap(), &[1, 2]);
}

#[test]
fn concurrent_registrations_do_not_lose_reactions() {
  let _rt = TestRuntimeGuard::new();

  let promise = runtime_native::rt_promise_new();
  let fired: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));

  extern "C" fn inc(data: *mut u8) {
    let c = unsafe { &*(data as *const AtomicUsize) };
    c.fetch_add(1, Ordering::SeqCst);
  }

  const THREADS: usize = 4;
  const PER_THREAD: usize = 200;

  let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS + 1));
  let mut joins = Vec::new();
  for _ in 0..THREADS {
    let b = barrier.clone();
    joins.push(std::thread::spawn(move || {
      b.wait();
      for i in 0..PER_THREAD {
        runtime_native::rt_promise_then(promise, inc, fired as *const AtomicUsize as *mut u8);
        if i % 17 == 0 {
          std::thread::yield_now();
        }
      }
    }));
  }

  // Start the registrars and resolve mid-flight to cover both pending + already-settled paths.
  barrier.wait();
  std::thread::sleep(std::time::Duration::from_millis(5));
  runtime_native::rt_promise_resolve(promise, core::ptr::null_mut());

  for j in joins {
    j.join().unwrap();
  }

  while runtime_native::rt_async_poll() {}

  assert_eq!(fired.load(Ordering::SeqCst), THREADS * PER_THREAD);
}

#[test]
fn reentrant_then_handlers_observe_microtask_checkpoint_ordering() {
  let _rt = TestRuntimeGuard::new();

  let promise = runtime_native::rt_promise_new();
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
    runtime_native::rt_promise_then(ctx.promise, push_log, b_ctx as *const LogCtx as *mut u8);
  }

  let ctx: &'static ReentrantCtx = Box::leak(Box::new(ReentrantCtx { promise, log }));
  let c_ctx: &'static LogCtx = Box::leak(Box::new(LogCtx { log, id: 2 }));

  runtime_native::rt_promise_then(promise, first, ctx as *const ReentrantCtx as *mut u8);
  runtime_native::rt_promise_then(promise, push_log, c_ctx as *const LogCtx as *mut u8);

  runtime_native::rt_promise_resolve(promise, core::ptr::null_mut());
  while runtime_native::rt_async_poll() {}

  // `first` runs, queues a new microtask (id=3). The second handler (id=2) was already queued and
  // must run before the newly-queued handler.
  assert_eq!(&*log.lock().unwrap(), &[1, 2, 3]);
}

