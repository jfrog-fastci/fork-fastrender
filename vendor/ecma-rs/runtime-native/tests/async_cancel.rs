use runtime_native::abi::Microtask;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{rt_async_cancel_all, rt_drain_microtasks, rt_queue_microtask};
use std::sync::atomic::{AtomicUsize, Ordering};

#[repr(C)]
struct DropPayload {
  ran: *const AtomicUsize,
  dropped: *const AtomicUsize,
}

extern "C" fn microtask_run(data: *mut u8) {
  // SAFETY: owned by this microtask invocation.
  let payload: Box<DropPayload> = unsafe { Box::from_raw(data.cast()) };
  let ran = unsafe { &*payload.ran };
  ran.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn microtask_drop(data: *mut u8) {
  // SAFETY: owned by this microtask drop hook invocation.
  let payload: Box<DropPayload> = unsafe { Box::from_raw(data.cast()) };
  let dropped = unsafe { &*payload.dropped };
  dropped.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn cancel_runs_microtask_drop_hook_without_executing() {
  let _rt = TestRuntimeGuard::new();

  let ran = Box::into_raw(Box::new(AtomicUsize::new(0)));
  let dropped = Box::into_raw(Box::new(AtomicUsize::new(0)));

  let payload = Box::new(DropPayload { ran, dropped });
  unsafe {
    rt_queue_microtask(Microtask {
      func: microtask_run,
      data: Box::into_raw(payload).cast(),
      drop: Some(microtask_drop),
    });
  }

  rt_async_cancel_all();

  // The queue should be empty and the microtask must not run.
  assert!(!rt_drain_microtasks());
  assert_eq!(unsafe { &*ran }.load(Ordering::SeqCst), 0);
  assert_eq!(unsafe { &*dropped }.load(Ordering::SeqCst), 1);

  // Idempotent.
  rt_async_cancel_all();

  unsafe {
    drop(Box::from_raw(ran));
    drop(Box::from_raw(dropped));
  }
}

