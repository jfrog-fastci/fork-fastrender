use crate::abi::PromiseRef;
use crate::async_runtime::PromiseLayout;
use crate::async_rt::gc::Root as PersistentRoot;
use crate::roots::Root as StackRoot;
use crate::roots::GcHandle;
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
  promise_root: PersistentRoot,
  data_root: Option<PersistentRoot>,
}

extern "C" fn promise_task_trampoline(ptr: *mut u8) {
  // Safety: allocated by `spawn_promise` as a `Box<PromiseTask>`.
  let task = unsafe { Box::from_raw(ptr as *mut PromiseTask) };
  let data_for_cb = task
    .data_root
    .as_ref()
    .map(|r| r.ptr())
    .unwrap_or(task.data);
  let promise = PromiseRef(task.promise_root.ptr().cast());
  // `task.func` comes from generated code / the embedder and is typed as `extern "C"`. If it
  // panics we must not unwind across the `extern "C"` boundary (UB); instead, allow unwinding into
  // Rust (`extern "C-unwind"`) and abort deterministically.
  crate::ffi::invoke_cb2_promise(task.func, data_for_cb, promise);
  // Box dropped here.
}

fn spawn_promise_impl(
  func: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  layout: PromiseLayout,
  data_root: Option<PersistentRoot>,
) -> PromiseRef {
  // Ensure the async runtime is initialized so promise settlement can wake a
  // thread blocked in the platform reactor wait syscall (`epoll_wait`/`kevent`).
  let _ = crate::async_rt::global();

  // Register the caller thread for GC safepoints; this matches `ParallelRuntime::spawn`.
  crate::threading::register_current_thread(ThreadKind::External);

  // Allocate a GC-managed payload promise. The external-pending flag is cleared and the counter
  // decremented by `rt_promise_{fulfill,reject}`.
  let promise = crate::payload_promise::alloc_payload_promise(layout, true);

  // Keep the promise object alive (and relocatable) while the worker is outstanding. Even if the
  // caller drops the returned `PromiseRef` immediately, the worker callback still needs to write and
  // settle the promise.
  //
  // Use a temporary shadow-stack root to avoid a TOCTOU race where the promise could be relocated by
  // a moving GC while contending on the persistent-handle table lock.
  let promise_root = {
    let tmp = StackRoot::new(promise.0.cast::<u8>());
    // Safety: `tmp.handle()` is a valid pointer-to-slot (`GcHandle`) containing a GC object base
    // pointer.
    let rooted = unsafe { PersistentRoot::new_from_slot_unchecked(tmp.handle()) };
    drop(tmp);
    rooted
  };
  let wrapper = Box::new(PromiseTask {
    func,
    data,
    promise_root,
    data_root,
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
  let root = unsafe { PersistentRoot::new_unchecked(data) };
  spawn_promise_impl(func, data, layout, Some(root))
}

pub(crate) unsafe fn spawn_promise_rooted_h(
  func: extern "C" fn(*mut u8, PromiseRef),
  data: GcHandle,
  layout: PromiseLayout,
) -> PromiseRef {
  // Safety: caller must uphold the rooted-task contract that `data` is a valid pointer to a
  // writable `GcPtr` slot containing the base pointer of a GC-managed object.
  let root = unsafe { PersistentRoot::new_from_slot_unchecked(data) };
  // Provide the current pointer value for consistency/debugging; the trampoline will always reload
  // via `root.ptr()` before invoking the callback.
  let ptr = root.ptr();
  spawn_promise_impl(func, ptr, layout, Some(root))
}
