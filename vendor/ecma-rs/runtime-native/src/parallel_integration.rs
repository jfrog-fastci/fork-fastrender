use crate::abi::PromiseRef;
use crate::async_abi::{PromiseHeader, PROMISE_FLAG_EXTERNAL_PENDING};
use crate::async_runtime::PromiseLayout;
use crate::threading::ThreadKind;
use core::sync::atomic::Ordering;

/// Heap-allocated wrapper passed through the join-based parallel scheduler.
///
/// The scheduler's public `rt_parallel_spawn` API expects tasks of the form
/// `extern "C" fn(*mut u8)`. For promise-returning tasks we allocate a small
/// wrapper containing the real callback + promise handle.
#[repr(C)]
struct PromiseTask {
  func: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  promise: PromiseRef,
}

extern "C" fn promise_task_trampoline(ptr: *mut u8) {
  // Safety: allocated by `spawn_promise` as a `Box<PromiseTask>`.
  let task = unsafe { Box::from_raw(ptr as *mut PromiseTask) };
  (task.func)(task.data, task.promise);
  // Box dropped here.
}

pub(crate) fn spawn_promise(
  func: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  layout: PromiseLayout,
) -> PromiseRef {
  // Ensure the async runtime is initialized so promise settlement can wake a
  // thread blocked in the platform reactor wait syscall (`epoll_wait`/`kevent`).
  let _ = crate::async_rt::global();

  // Register the caller thread for GC safepoints; this matches `ParallelRuntime::spawn`.
  crate::threading::register_current_thread(ThreadKind::External);

  let promise = crate::async_rt::promise::promise_new_with_payload(layout);
  // While the worker task is outstanding, keep the async runtime from reporting itself as fully
  // idle. The flag is cleared (and the pending count decremented) when the promise settles.
  if !promise.is_null() {
    let header = promise.0.cast::<PromiseHeader>();
    if header.is_null() {
      std::process::abort();
    }
    unsafe {
      (*header)
        .flags
        .fetch_or(PROMISE_FLAG_EXTERNAL_PENDING, Ordering::Release);
    }
    crate::async_rt::external_pending_inc();
  }
  let wrapper = Box::new(PromiseTask {
    func,
    data,
    promise,
  });
  let wrapper_ptr = Box::into_raw(wrapper) as *mut u8;

  // Run the wrapper on the work-stealing pool without requiring a `TaskId` join.
  crate::rt_parallel().spawn_detached(promise_task_trampoline, wrapper_ptr);

  promise
}
