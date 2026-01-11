use runtime_native::abi::Microtask;
use runtime_native::async_rt::Task;
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

extern "C" fn inc_atomic_usize(data: *mut u8) {
  let counter = unsafe { &*(data as *const AtomicUsize) };
  counter.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn microtask_does_not_run_until_drained() {
  let _rt = TestRuntimeGuard::new();

  let counter: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));

  unsafe {
    runtime_native::rt_queue_microtask(Microtask {
      func: inc_atomic_usize,
      data: counter as *const AtomicUsize as *mut u8,
    });
  }

  assert_eq!(counter.load(Ordering::SeqCst), 0, "microtask should not run synchronously");

  assert!(runtime_native::rt_drain_microtasks());

  assert_eq!(counter.load(Ordering::SeqCst), 1, "microtask should run when drained");
}

struct ChainCtx {
  counter: AtomicUsize,
}

extern "C" fn chain_first(data: *mut u8) {
  let ctx = unsafe { &*(data as *const ChainCtx) };
  ctx.counter.fetch_add(1, Ordering::SeqCst);

  unsafe {
    runtime_native::rt_queue_microtask(Microtask {
      func: chain_second,
      data,
    });
  }
}

extern "C" fn chain_second(data: *mut u8) {
  let ctx = unsafe { &*(data as *const ChainCtx) };
  ctx.counter.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn microtasks_enqueued_during_execution_run_in_same_drain_call() {
  let _rt = TestRuntimeGuard::new();

  let ctx: &'static ChainCtx = Box::leak(Box::new(ChainCtx {
    counter: AtomicUsize::new(0),
  }));

  unsafe {
    runtime_native::rt_queue_microtask(Microtask {
      func: chain_first,
      data: ctx as *const ChainCtx as *mut u8,
    });
  }
  assert!(runtime_native::rt_drain_microtasks());

  assert_eq!(
    ctx.counter.load(Ordering::SeqCst),
    2,
    "expected both microtasks to run in one drain call"
  );
}

extern "C" fn set_atomic_bool(data: *mut u8) {
  let flag = unsafe { &*(data as *const AtomicBool) };
  flag.store(true, Ordering::SeqCst);
}

#[test]
fn drain_microtasks_does_not_run_macrotasks() {
  let _rt = TestRuntimeGuard::new();

  let microtask_ran: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
  let macrotask_ran: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));

  unsafe {
    runtime_native::rt_queue_microtask(Microtask {
      func: set_atomic_bool,
      data: microtask_ran as *const AtomicBool as *mut u8,
    });
  }
  runtime_native::async_rt::global().enqueue_macrotask(Task::new(
    set_atomic_bool,
    macrotask_ran as *const AtomicBool as *mut u8,
  ));

  assert!(runtime_native::rt_drain_microtasks());

  assert!(microtask_ran.load(Ordering::SeqCst));
  assert!(
    !macrotask_ran.load(Ordering::SeqCst),
    "rt_drain_microtasks should not run macrotasks"
  );

  runtime_native::rt_async_poll_legacy();
  assert!(macrotask_ran.load(Ordering::SeqCst));
}
