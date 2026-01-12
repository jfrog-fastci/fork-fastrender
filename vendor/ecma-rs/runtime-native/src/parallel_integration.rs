use crate::abi::{PromiseRef, RtShapeId};
use crate::async_abi::{PromiseHeader, PROMISE_FLAG_EXTERNAL_PENDING};
use crate::async_rt::gc::Root as PersistentRoot;
use crate::async_runtime::PromiseLayout;
use crate::roots::GcHandle;
use crate::roots::Root as StackRoot;
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

// --- GC-managed promise spawn --------------------------------------------------------------------

/// Heap-allocated wrapper for promise-returning parallel tasks where the promise itself is a
/// GC-managed movable object.
///
/// Unlike [`PromiseTask`], the `promise` field cannot store a raw pointer across async boundaries: a
/// moving GC may relocate the promise allocation before the worker runs. Store the promise as a
/// persistent handle (`PersistentRoot`) so the trampoline can re-load the current pointer.
#[repr(C)]
struct GcPromiseTask {
  func: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  data_root: Option<PersistentRoot>,
  promise_root: PersistentRoot,
}

// Raw pointers are not `Send` by default; the runtime ABI requires that `data` be safe to access
// from worker threads, and `PersistentRoot` is thread-safe.
unsafe impl Send for GcPromiseTask {}

extern "C" fn gc_promise_task_trampoline(ptr: *mut u8) {
  // Safety: allocated by `spawn_promise_with_shape_impl` as a `Box<GcPromiseTask>`.
  let task = unsafe { Box::from_raw(ptr as *mut GcPromiseTask) };

  let data_for_cb = task
    .data_root
    .as_ref()
    .map(|r| r.ptr())
    .unwrap_or(task.data);

  let promise_for_cb = PromiseRef(task.promise_root.ptr().cast());

  // `task.func` comes from generated code / the embedder and is typed as `extern "C"`. If it panics
  // we must not unwind across the `extern "C"` boundary (UB); instead, allow unwinding into Rust
  // (`extern "C-unwind"`) and abort deterministically.
  crate::ffi::invoke_cb2_promise(task.func, data_for_cb, promise_for_cb);
  // Box dropped here.
}

fn spawn_promise_with_shape_impl(
  func: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  promise_size: usize,
  promise_align: usize,
  promise_shape: RtShapeId,
  data_root: Option<PersistentRoot>,
) -> PromiseRef {
  // Ensure the async runtime is initialized so promise settlement can wake a
  // thread blocked in the platform reactor wait syscall (`epoll_wait`/`kevent`).
  let _ = crate::async_rt::global();

  // Ensure the caller thread participates in GC safepoints; this matches `ParallelRuntime::spawn`.
  crate::threading::register_current_thread(ThreadKind::External);

  if promise_size < core::mem::size_of::<PromiseHeader>() {
    crate::trap::rt_trap_invalid_arg("rt_parallel_spawn_promise_with_shape: promise_size too small");
  }
  if promise_align < core::mem::align_of::<PromiseHeader>() || !promise_align.is_power_of_two() {
    crate::trap::rt_trap_invalid_arg("rt_parallel_spawn_promise_with_shape: promise_align must be a power of two and >= alignof(PromiseHeader)");
  }

  // Validate the allocation request (size matches descriptor, shape id is in-bounds, shape table
  // registered). `rt_alloc` will validate this too, but we also want access to the descriptor's
  // alignment.
  let (rt_desc, _type_desc) = crate::shape_table::validate_alloc_request(promise_size, promise_shape);
  let desc_align = (rt_desc.align as usize).max(crate::gc::OBJ_ALIGN);
  if desc_align < promise_align {
    crate::trap::rt_trap_invalid_arg_fmt(format_args!(
      "rt_parallel_spawn_promise_with_shape: promise_align {promise_align} exceeds registered shape alignment {desc_align}"
    ));
  }

  // Allocate the promise as a GC-managed object so the payload can contain traceable GC pointers.
  let promise_ptr = crate::rt_alloc(promise_size, promise_shape);

  // Root the newly allocated promise in a stack slot so we can create a persistent handle
  // (`PersistentRoot`) in a moving-GC-safe way (`alloc_from_slot` reads after lock acquisition).
  let promise_slot = StackRoot::<u8>::new(promise_ptr);
  // Safety: `promise_slot.handle()` is a valid pointer to a `GcPtr` slot.
  let promise_root = unsafe { PersistentRoot::new_from_slot_unchecked(promise_slot.handle()) };

  // Initialize the header after rooting so any GC during initialization (or subsequent
  // bookkeeping) cannot orphan the promise.
  let promise = PromiseRef(promise_root.ptr().cast());
  unsafe {
    crate::native_async::promise_init(promise);
  }

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

  // Done with the stack root; the promise is now kept alive via `promise_root`.
  drop(promise_slot);

  let wrapper = Box::new(GcPromiseTask {
    func,
    data,
    data_root,
    promise_root: promise_root.clone(),
  });
  let wrapper_ptr = Box::into_raw(wrapper) as *mut u8;

  // Run the wrapper on the work-stealing pool without requiring a `TaskId` join.
  crate::rt_parallel().spawn_detached(gc_promise_task_trampoline, wrapper_ptr);

  // `spawn_detached` may block on scheduler locks (GC-aware) and temporarily enter a GC-safe region.
  // Re-load the current promise pointer from the persistent root in case a moving GC relocated it
  // while we were enqueueing the work item.
  PromiseRef(promise_root.ptr().cast())
}

pub(crate) fn spawn_promise_with_shape(
  func: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  promise_size: usize,
  promise_align: usize,
  promise_shape: RtShapeId,
) -> PromiseRef {
  spawn_promise_with_shape_impl(func, data, promise_size, promise_align, promise_shape, None)
}

pub(crate) fn spawn_promise_with_shape_rooted(
  func: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  promise_size: usize,
  promise_align: usize,
  promise_shape: RtShapeId,
) -> PromiseRef {
  // Safety: caller must uphold the rooted-task contract that `data` is the base pointer of a
  // GC-managed object.
  let root = unsafe { PersistentRoot::new_unchecked(data) };
  spawn_promise_with_shape_impl(
    func,
    data,
    promise_size,
    promise_align,
    promise_shape,
    Some(root),
  )
}

pub(crate) unsafe fn spawn_promise_with_shape_rooted_h(
  func: extern "C" fn(*mut u8, PromiseRef),
  data: GcHandle,
  promise_size: usize,
  promise_align: usize,
  promise_shape: RtShapeId,
) -> PromiseRef {
  // Safety: caller must uphold the rooted-task contract that `data` is a valid pointer to a
  // writable `GcPtr` slot containing the base pointer of a GC-managed object.
  let root = unsafe { PersistentRoot::new_from_slot_unchecked(data) };
  let ptr = root.ptr();
  spawn_promise_with_shape_impl(
    func,
    ptr,
    promise_size,
    promise_align,
    promise_shape,
    Some(root),
  )
}
