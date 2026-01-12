use crate::abi::PromiseRef;
use crate::async_abi::{PromiseHeader, PROMISE_FLAG_EXTERNAL_PENDING};
use crate::async_runtime::PromiseLayout;
use crate::async_rt::gc::Root;
use crate::roots::GcHandle;
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
  root: Option<Root>,
}

extern "C" fn promise_task_trampoline(ptr: *mut u8) {
  // Safety: allocated by `spawn_promise` as a `Box<PromiseTask>`.
  let task = unsafe { Box::from_raw(ptr as *mut PromiseTask) };
  let data_for_cb = task
    .root
    .as_ref()
    .map(|r| r.ptr())
    .unwrap_or(task.data);
  // `task.func` comes from generated code / the embedder and is typed as `extern "C"`. If it
  // panics we must not unwind across the `extern "C"` boundary (UB); instead, allow unwinding into
  // Rust (`extern "C-unwind"`) and abort deterministically.
  crate::ffi::invoke_cb2_promise(task.func, data_for_cb, task.promise);
  // Box dropped here.
}

fn spawn_promise_impl(
  func: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  layout: PromiseLayout,
  root: Option<Root>,
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
    root,
  });
  let wrapper_ptr = Box::into_raw(wrapper) as *mut u8;

  // Run the wrapper on the work-stealing pool without requiring a `TaskId` join.
  crate::rt_parallel().spawn_detached(promise_task_trampoline, wrapper_ptr);

  promise
}

pub(crate) fn spawn_promise(
  func: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  layout: PromiseLayout,
) -> PromiseRef {
  spawn_promise_impl(func, data, layout, None)
}

pub(crate) fn spawn_promise_rooted(
  func: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  layout: PromiseLayout,
) -> PromiseRef {
  // Safety: caller must uphold the rooted-task contract that `data` is the base pointer of a
  // GC-managed object.
  let root = unsafe { Root::new_unchecked(data) };
  spawn_promise_impl(func, data, layout, Some(root))
}

pub(crate) unsafe fn spawn_promise_rooted_h(
  func: extern "C" fn(*mut u8, PromiseRef),
  data: GcHandle,
  layout: PromiseLayout,
) -> PromiseRef {
  // Safety: caller must uphold the rooted-task contract that `data` is a valid pointer to a
  // writable `GcPtr` slot containing the base pointer of a GC-managed object.
  let root = unsafe { Root::new_from_slot_unchecked(data) };
  // Provide the current pointer value for consistency/debugging; the trampoline will always reload
  // via `root.ptr()` before invoking the callback.
  let ptr = root.ptr();
  spawn_promise_impl(func, ptr, layout, Some(root))
}
