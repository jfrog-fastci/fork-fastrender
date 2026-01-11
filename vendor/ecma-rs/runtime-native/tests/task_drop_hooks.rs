use runtime_native::test_util::{reset_runtime_state, TestRuntimeGuard};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[repr(C)]
struct DropCounter {
  drops: Arc<AtomicUsize>,
}

impl Drop for DropCounter {
  fn drop(&mut self) {
    self.drops.fetch_add(1, Ordering::AcqRel);
  }
}

extern "C" fn noop(_data: *mut u8) {}

extern "C" fn drop_counter(data: *mut u8) {
  // Safety: allocated as `Box<DropCounter>` in the test setup.
  unsafe {
    drop(Box::from_raw(data as *mut DropCounter));
  }
}

#[test]
fn microtask_with_drop_runs_drop_on_discard() {
  let _rt = TestRuntimeGuard::new();

  let drops = Arc::new(AtomicUsize::new(0));
  let data = Box::new(DropCounter { drops: drops.clone() });
  let data_ptr = Box::into_raw(data) as *mut u8;

  runtime_native::rt_queue_microtask_with_drop(noop, data_ptr, drop_counter);

  // Discard queued microtasks without executing them.
  reset_runtime_state();

  assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[test]
fn microtask_with_drop_does_not_run_drop_after_execution() {
  let _rt = TestRuntimeGuard::new();

  #[repr(C)]
  struct Ctx {
    ran: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
  }

  impl Drop for Ctx {
    fn drop(&mut self) {
      self.drops.fetch_add(1, Ordering::AcqRel);
    }
  }

  extern "C" fn mark_ran(data: *mut u8) {
    let ctx = unsafe { &*(data as *const Ctx) };
    ctx.ran.store(1, Ordering::Release);
  }

  extern "C" fn drop_ctx(data: *mut u8) {
    // Safety: allocated as `Box<Ctx>` in the test setup.
    unsafe {
      drop(Box::from_raw(data as *mut Ctx));
    }
  }

  let ran = Arc::new(AtomicUsize::new(0));
  let drops = Arc::new(AtomicUsize::new(0));
  let data = Box::new(Ctx {
    ran: ran.clone(),
    drops: drops.clone(),
  });
  let data_ptr = Box::into_raw(data) as *mut u8;

  runtime_native::rt_queue_microtask_with_drop(mark_ran, data_ptr, drop_ctx);
  assert_eq!(ran.load(Ordering::Acquire), 0);
  assert_eq!(drops.load(Ordering::Acquire), 0);

  assert!(runtime_native::rt_drain_microtasks());
  assert_eq!(ran.load(Ordering::Acquire), 1);
  assert_eq!(drops.load(Ordering::Acquire), 0);

  // The microtask drop hook is discard-only: after normal execution the caller still owns `data`.
  unsafe {
    drop(Box::from_raw(data_ptr as *mut Ctx));
  }
  assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[test]
fn timeout_with_drop_runs_drop_on_clear() {
  let _rt = TestRuntimeGuard::new();

  let drops = Arc::new(AtomicUsize::new(0));
  let data = Box::new(DropCounter { drops: drops.clone() });
  let data_ptr = Box::into_raw(data) as *mut u8;

  // Schedule a long timeout and clear it immediately; the callback must never run, but the callback
  // state must still be freed.
  let id = runtime_native::rt_set_timeout_with_drop(noop, data_ptr, drop_counter, 60_000);
  runtime_native::rt_clear_timer(id);

  assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[test]
fn interval_with_drop_can_clear_itself_without_dropping_while_running() {
  let _rt = TestRuntimeGuard::new();

  #[repr(C)]
  struct IntervalCtx {
    id: runtime_native::abi::TimerId,
    fired: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
  }

  impl Drop for IntervalCtx {
    fn drop(&mut self) {
      self.drops.fetch_add(1, Ordering::AcqRel);
    }
  }

  extern "C" fn on_interval(data: *mut u8) {
    // Safety: allocated as `Box<IntervalCtx>` in the test setup and freed by `drop_interval_ctx`.
    let ctx = unsafe { &*(data as *const IntervalCtx) };
    ctx.fired.fetch_add(1, Ordering::AcqRel);
    runtime_native::rt_clear_timer(ctx.id);
  }

  extern "C" fn drop_interval_ctx(data: *mut u8) {
    unsafe {
      drop(Box::from_raw(data as *mut IntervalCtx));
    }
  }

  let fired = Arc::new(AtomicUsize::new(0));
  let drops = Arc::new(AtomicUsize::new(0));

  let ctx = Box::new(IntervalCtx {
    id: 0,
    fired: fired.clone(),
    drops: drops.clone(),
  });
  let ctx_ptr = Box::into_raw(ctx) as *mut u8;

  let id = runtime_native::rt_set_interval_with_drop(on_interval, ctx_ptr, drop_interval_ctx, 1);
  unsafe {
    (*(ctx_ptr as *mut IntervalCtx)).id = id;
  }

  let start = Instant::now();
  while drops.load(Ordering::Acquire) == 0 {
    runtime_native::rt_async_poll_legacy();
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for interval to fire + drop"
    );
  }

  assert_eq!(fired.load(Ordering::Acquire), 1);
  assert_eq!(drops.load(Ordering::Acquire), 1);
}
