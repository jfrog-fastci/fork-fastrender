use crate::abi::{PromiseRef, RtShapeId};
use crate::async_abi::{PromiseHeader, PROMISE_FLAG_EXTERNAL_PENDING};
use crate::async_rt::gc::Root as PersistentRoot;
use crate::async_runtime::PromiseLayout;
use crate::gc::HandleId;
use crate::roots::GcHandle;
use crate::roots::Root as StackRoot;
use crate::sync::GcAwareMutex;
use crate::threading::ThreadKind;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use once_cell::sync::Lazy;
use std::sync::{Arc, Weak};

#[inline]
fn maybe_clear_external_pending(promise: PromiseRef) {
  if promise.is_null() {
    return;
  }
  // `PromiseRef` is an opaque handle, but by contract it must point to a `PromiseHeader` prefix at
  // offset 0 of the allocation.
  let header = promise.0.cast::<PromiseHeader>();
  if (header as usize) % core::mem::align_of::<PromiseHeader>() != 0 {
    std::process::abort();
  }
  let prev = unsafe { &(*header).flags }.fetch_and(!PROMISE_FLAG_EXTERNAL_PENDING, Ordering::AcqRel);
  if (prev & PROMISE_FLAG_EXTERNAL_PENDING) != 0 {
    crate::async_rt::external_pending_dec();
  }
}

/// Heap-allocated wrapper passed through the join-based parallel scheduler.
///
/// The scheduler's public `rt_parallel_spawn` API expects tasks of the form
/// `extern "C" fn(*mut u8)`. For promise-returning tasks we allocate a small
/// wrapper containing the real callback + promise handle.
#[repr(C)]
struct PromiseTask {
  func: extern "C" fn(*mut u8, PromiseRef),
  /// Data pointer for unrooted tasks (opaque, not GC-managed).
  ///
  /// Stored as an integer so the wrapper remains `Send + Sync` on newer Rust versions where raw
  /// pointers are `!Send`/`!Sync`.
  data: usize,
  /// Persistent handle rooting the promise object while the task is queued/running.
  promise_handle: AtomicU64,
  /// Optional persistent handle rooting GC-managed `data` for rooted task variants.
  data_handle: AtomicU64,
  state: AtomicU8,
}

const HANDLE_NONE: u64 = 0;

#[inline]
fn encode_handle(id: Option<HandleId>) -> u64 {
  id.map(|h| h.to_u64()).unwrap_or(HANDLE_NONE)
}

#[inline]
fn decode_handle(raw: u64) -> Option<HandleId> {
  if raw == HANDLE_NONE {
    None
  } else {
    Some(HandleId::from_u64(raw))
  }
}

const STATE_QUEUED: u8 = 0;
const STATE_RUNNING: u8 = 1;
const STATE_FINISHED: u8 = 2;
const STATE_CANCELED: u8 = 3;

static PROMISE_TASKS: Lazy<GcAwareMutex<Vec<Weak<PromiseTask>>>> =
  Lazy::new(|| GcAwareMutex::new(Vec::new()));

extern "C" fn promise_task_trampoline(ptr: *mut u8) {
  // Safety: allocated by `spawn_promise_impl` as `Arc<PromiseTask>` and passed through the scheduler
  // via `Arc::into_raw`.
  let task = unsafe { Arc::from_raw(ptr as *const PromiseTask) };

  // If the async runtime was torn down (`rt_async_cancel_all` / `TestRuntimeGuard`), this task may
  // have been cancelled while still queued. In that case, do not call into generated code, and do
  // not touch stale persistent-handle IDs.
  if task
    .state
    .compare_exchange(STATE_QUEUED, STATE_RUNNING, Ordering::AcqRel, Ordering::Acquire)
    .is_err()
  {
    return;
  }

  let table = crate::roots::global_persistent_handle_table();

  let promise_handle_raw = task.promise_handle.load(Ordering::Acquire);
  let promise_handle = decode_handle(promise_handle_raw).unwrap_or_else(|| std::process::abort());

  // Resolve the task context pointer first so we do not hold an unrooted raw promise pointer across
  // potential lock contention (GC-aware locks can enter a GC-safe region while blocked).
  let data = match decode_handle(task.data_handle.load(Ordering::Acquire)) {
    Some(h) => table.get(h).unwrap_or_else(|| std::process::abort()),
    None => task.data as *mut u8,
  };

  let promise_ptr = table.get(promise_handle).unwrap_or_else(|| std::process::abort());
  let promise = PromiseRef(promise_ptr.cast());

  // `task.func` comes from generated code / the embedder and is typed as `extern "C"`. If it
  // panics we must not unwind across the `extern "C"` boundary (UB); instead, allow unwinding into
  // Rust (`extern "C-unwind"`) and abort deterministically.
  crate::ffi::invoke_cb2_promise(task.func, data, promise);

  // If the callback forgot to settle the promise, ensure we don't leak the "external pending" count
  // and keep the event loop alive forever. Re-load the promise pointer from the handle table in case
  // a moving GC relocated it while the callback was executing.
  let promise_ptr = table.get(promise_handle).unwrap_or_else(|| std::process::abort());
  maybe_clear_external_pending(PromiseRef(promise_ptr.cast()));

  // Free roots exactly once after completion.
  if let Some(id) = decode_handle(task.promise_handle.swap(HANDLE_NONE, Ordering::AcqRel)) {
    let _ = table.free(id);
  }
  if let Some(id) = decode_handle(task.data_handle.swap(HANDLE_NONE, Ordering::AcqRel)) {
    let _ = table.free(id);
  }

  task.state.store(STATE_FINISHED, Ordering::Release);
}

fn spawn_promise_impl(
  func: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
  layout: PromiseLayout,
  data_handle: Option<HandleId>,
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
  // Store the promise in the persistent handle table so stop-the-world evacuation/compaction can
  // update it in-place.
  let table = crate::roots::global_persistent_handle_table();
  let promise_handle = if promise.is_null() {
    None
  } else {
    Some(table.alloc_movable(promise.0.cast()))
  };

  let wrapper = Arc::new(PromiseTask {
    func,
    data: data as usize,
    promise_handle: AtomicU64::new(encode_handle(promise_handle)),
    data_handle: AtomicU64::new(encode_handle(data_handle)),
    state: AtomicU8::new(STATE_QUEUED),
  });
  PROMISE_TASKS.lock().push(Arc::downgrade(&wrapper));
  let wrapper_ptr = Arc::into_raw(wrapper) as *mut u8;

  // Run the wrapper on the work-stealing pool without requiring a `TaskId` join.
  crate::rt_parallel().spawn_detached(promise_task_trampoline, wrapper_ptr);

  // Re-load the current promise pointer from the handle table in case a moving GC relocated it while
  // we were enqueueing the work item.
  let promise_ptr = promise_handle
    .and_then(|id| table.get(id))
    .unwrap_or(core::ptr::null_mut());
  PromiseRef(promise_ptr.cast())
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
  // Ensure `alloc_movable` is moving-GC safe by registering this thread before it may contend on the
  // persistent handle table lock.
  crate::threading::register_current_thread(ThreadKind::External);

  // Safety: caller must uphold the rooted-task contract that `data` is the base pointer of a
  // GC-managed object.
  let handle = crate::roots::global_persistent_handle_table().alloc_movable(data);
  spawn_promise_impl(func, core::ptr::null_mut(), layout, Some(handle))
}

pub(crate) unsafe fn spawn_promise_rooted_h(
  func: extern "C" fn(*mut u8, PromiseRef),
  data: GcHandle,
  layout: PromiseLayout,
) -> PromiseRef {
  // Ensure the current thread participates in GC safepoints before we potentially block on the
  // persistent handle table lock while allocating a handle.
  crate::threading::register_current_thread(ThreadKind::External);

  // Safety: caller must uphold the rooted-task contract that `data` is a valid pointer to a
  // writable `GcPtr` slot containing the base pointer of a GC-managed object.
  let handle = unsafe { crate::roots::global_persistent_handle_table().alloc_from_slot(data) };
  spawn_promise_impl(func, core::ptr::null_mut(), layout, Some(handle))
}

fn cancel_promise_task(task: &PromiseTask) {
  // Attempt to cancel a task that is still queued. If it's already running (or finished), do not
  // touch its handles: the worker thread will free them on completion.
  if task
    .state
    .compare_exchange(STATE_QUEUED, STATE_CANCELED, Ordering::AcqRel, Ordering::Acquire)
    .is_err()
  {
    return;
  }

  let table = crate::roots::global_persistent_handle_table();

  // Clear the external-pending flag and decrement the global pending count so the event loop can
  // report idle after teardown.
  //
  // This is best-effort: if the promise handle was already taken/freed, skip.
  let promise_handle_raw = task.promise_handle.swap(HANDLE_NONE, Ordering::AcqRel);
  if let Some(id) = decode_handle(promise_handle_raw) {
    if let Some(promise_ptr) = table.get(id) {
      let header = promise_ptr.cast::<PromiseHeader>();
      if header.is_null() {
        std::process::abort();
      }
      let prev = unsafe {
        (*header)
          .flags
          .fetch_and(!PROMISE_FLAG_EXTERNAL_PENDING, Ordering::AcqRel)
      };
      if (prev & PROMISE_FLAG_EXTERNAL_PENDING) != 0 {
        crate::async_rt::external_pending_dec();
      }
    }
    let _ = table.free(id);
  }

  let data_handle_raw = task.data_handle.swap(HANDLE_NONE, Ordering::AcqRel);
  if let Some(id) = decode_handle(data_handle_raw) {
    let _ = table.free(id);
  }
}

/// Cancel all outstanding `rt_parallel_spawn_promise*` tasks without running them.
///
/// This is used by teardown paths (`rt_async_cancel_all`, `TestRuntimeGuard`) to ensure:
/// - queued tasks do not run after the embedding is torn down,
/// - persistent handle-table roots for promise/data pointers are released,
/// - and the async runtime's external-pending count is not left stuck.
pub(crate) fn cancel_all_promise_tasks() {
  // Avoid lock-order inversion with `spawn_promise_impl`:
  //
  // - spawn path: handle table lock (alloc_movable) → PROMISE_TASKS lock (register weak)
  // - cancel path: PROMISE_TASKS lock (enumerate) → handle table lock (free)
  //
  // Snapshot the live tasks first, then cancel without holding the registry lock.
  let live: Vec<Arc<PromiseTask>> = PROMISE_TASKS
    .lock()
    .iter()
    .filter_map(|w| w.upgrade())
    .collect();
  for task in &live {
    cancel_promise_task(task);
  }

  // Prune dead tasks from the registry.
  PROMISE_TASKS.lock().retain(|w| w.upgrade().is_some());
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

  // Resolve pointers under possible handle-table lock contention without holding unrooted raw GC
  // pointers across potential GC-safe regions.
  {
    let mut promise_ptr: *mut u8 = task.promise_root.ptr();
    let mut scope = crate::roots::RootScope::new();
    scope.push(&mut promise_ptr as *mut *mut u8);

    let data_for_cb = task
      .data_root
      .as_ref()
      .map(|r| r.ptr())
      .unwrap_or(task.data);
    let promise_for_cb = PromiseRef(promise_ptr.cast());

    drop(scope);

    // `task.func` comes from generated code / the embedder and is typed as `extern "C"`. If it panics
    // we must not unwind across the `extern "C"` boundary (UB); instead, allow unwinding into Rust
    // (`extern "C-unwind"`) and abort deterministically.
    crate::ffi::invoke_cb2_promise(task.func, data_for_cb, promise_for_cb);
  }

  let promise = PromiseRef(task.promise_root.ptr().cast());
  maybe_clear_external_pending(promise);
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
  // Ensure `PersistentRoot::new_unchecked` is moving-GC safe by registering this thread before it
  // may contend on the persistent handle table lock.
  crate::threading::register_current_thread(ThreadKind::External);

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
  // Ensure the current thread participates in GC safepoints before we potentially block on the
  // persistent handle table lock while creating `root`.
  crate::threading::register_current_thread(ThreadKind::External);

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
