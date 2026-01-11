use crate::abi::PromiseRef;
use crate::async_runtime::PromiseLayout;
use crate::threading::ThreadKind;

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
  // thread blocked in `epoll_wait`.
  let _ = crate::async_rt::global();

  // Register the caller thread for GC safepoints; this matches `ParallelRuntime::spawn`.
  crate::threading::register_current_thread(ThreadKind::External);

  let promise = crate::async_rt::promise::promise_new_with_payload(layout);
  let wrapper = Box::new(PromiseTask {
    func,
    data,
    promise,
  });
  let wrapper_ptr = Box::into_raw(wrapper) as *mut u8;

  // Run the wrapper on the work-stealing pool without requiring a `TaskId` join.
  let rt = crate::rt_ensure_init();
  rt.parallel.spawn_detached(promise_task_trampoline, wrapper_ptr);

  promise
}

