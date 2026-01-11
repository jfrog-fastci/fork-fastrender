use crate::abi::PromiseRef;
use crate::abi::RtCoroutineHeader;
use crate::abi::ShapeId;
use crate::abi::TaskId;
use crate::abi::ValueRef;
use crate::abi::IoWatcherId;
use crate::alloc;
use crate::async_rt;
use crate::async_rt::WatcherId;
use crate::gc::ObjHeader;
use crate::gc::WeakHandle;
use crate::gc::YOUNG_SPACE;
use crate::threading;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;

#[inline(always)]
fn ensure_event_loop_thread_registered() {
  // The async runtime is driven by the main thread/event loop. Register it on
  // first use so GC can coordinate stop-the-world safepoints across all
  // mutator threads.
  crate::threading::register_current_thread(crate::threading::ThreadKind::Main);
}

#[no_mangle]
pub extern "C" fn rt_alloc(size: usize, _shape: ShapeId) -> *mut u8 {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_alloc(size);
  alloc::alloc_bytes(size, 16, "rt_alloc")
}

/// Allocate a pinned (non-moving) object.
///
/// NOTE: The milestone runtime does not yet wire allocations into the GC. This entrypoint exists so
/// codegen/FFI can request a stable address today and so future GC-backed allocation can route
/// pinned objects to a non-moving space.
#[no_mangle]
pub extern "C" fn rt_alloc_pinned(size: usize, _shape: ShapeId) -> *mut u8 {
  alloc::alloc_bytes(size, 16, "rt_alloc_pinned")
}

#[no_mangle]
pub extern "C" fn rt_alloc_array(len: usize, elem_size: usize) -> *mut u8 {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_alloc_array(len, elem_size);
  alloc::calloc_array(len, elem_size, "rt_alloc_array")
}

/// Register the current OS thread with the runtime.
#[no_mangle]
pub extern "C" fn rt_thread_init(kind: u32) {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_thread_init();

  let kind = match kind {
    0 => threading::ThreadKind::Main,
    1 => threading::ThreadKind::Worker,
    2 => threading::ThreadKind::Io,
    3 => threading::ThreadKind::External,
    _ => threading::ThreadKind::External,
  };
  threading::register_current_thread(kind);
}

/// Unregister the current OS thread from the runtime.
#[no_mangle]
pub extern "C" fn rt_thread_deinit() {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_thread_deinit();
  threading::unregister_current_thread();
}

/// GC safepoint.
#[no_mangle]
pub extern "C" fn rt_gc_safepoint() {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_safepoint();
  crate::threading::safepoint::rt_gc_safepoint();
}

/// Update the active young-space address range used by the write barrier.
///
/// This must be called by the GC during initialization and after each nursery
/// flip/resize that changes the current young generation region.
#[no_mangle]
pub extern "C" fn rt_gc_set_young_range(start: *mut u8, end: *mut u8) {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_set_young_range();
  YOUNG_SPACE.start.store(start as usize, Ordering::Release);
  YOUNG_SPACE.end.store(end as usize, Ordering::Release);
}

/// Debug/test helper: return the current young-space range.
///
/// # Safety
/// If `out_start`/`out_end` are non-null, they must be valid writable pointers.
#[no_mangle]
pub unsafe extern "C" fn rt_gc_get_young_range(out_start: *mut *mut u8, out_end: *mut *mut u8) {
  if !out_start.is_null() {
    *out_start = YOUNG_SPACE.start.load(Ordering::Acquire) as *mut u8;
  }
  if !out_end.is_null() {
    *out_end = YOUNG_SPACE.end.load(Ordering::Acquire) as *mut u8;
  }
}

/// Write barrier for GC.
///
/// Records old→young pointer stores in the remembered set.
#[no_mangle]
pub unsafe extern "C" fn rt_write_barrier(obj: *mut u8, slot: *mut u8) {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_write_barrier();

  if obj.is_null() || slot.is_null() {
    return;
  }

  // Avoid UB on misaligned pointers: the barrier is specified to read a pointer-sized value from
  // `slot` and to treat `obj` as an `ObjHeader` base pointer.
  if (slot as usize) % std::mem::align_of::<*mut u8>() != 0 {
    std::process::abort();
  }
  if (obj as usize) % std::mem::align_of::<ObjHeader>() != 0 {
    std::process::abort();
  }

  // SAFETY: The write barrier contract requires `slot` be aligned and contain a
  // valid GC pointer or null.
  let value = (slot as *const *mut u8).read();
  if value.is_null() {
    return;
  }

  if !YOUNG_SPACE.contains(value as usize) {
    return;
  }

  // Writes into young objects don't need a barrier: nursery tracing will find
  // the edge.
  if YOUNG_SPACE.contains(obj as usize) {
    return;
  }

  // Old → young store. Mark the object as remembered.
  //
  // TODO: Replace this with the real remembered-set / card-table update once
  // runtime-native's GC is wired up to native codegen.
  let header = &mut *(obj as *mut ObjHeader);
  if !header.is_remembered() {
    header.set_remembered(true);
  }
}

/// Range write barrier for GC.
///
/// Like [`rt_write_barrier`], but for a contiguous run of pointer slots.
///
/// - `start_slot` is the address of the first pointer slot.
/// - `len` is the number of bytes to scan (must cover a whole number of pointer slots).
#[no_mangle]
pub unsafe extern "C" fn rt_write_barrier_range(obj: *mut u8, start_slot: *mut u8, len: usize) {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_write_barrier_range();

  if obj.is_null() || start_slot.is_null() || len == 0 {
    return;
  }

  // Writes into young objects don't need a barrier: nursery tracing will find the edge.
  if YOUNG_SPACE.contains(obj as usize) {
    return;
  }

  let header = &mut *(obj as *mut ObjHeader);
  if header.is_remembered() {
    return;
  }

  // Scan slots for any young pointer. If we see one, remember the object.
  let slot_count = len / core::mem::size_of::<*mut u8>();
  let slots = start_slot as *const *mut u8;
  for i in 0..slot_count {
    let value = slots.add(i).read();
    if value.is_null() {
      continue;
    }
    if YOUNG_SPACE.contains(value as usize) {
      header.set_remembered(true);
      return;
    }
  }
}

/// Trigger a GC cycle.
///
/// Milestone-1 runtime: no-op.
#[no_mangle]
pub extern "C" fn rt_gc_collect() {
  #[cfg(feature = "gc_stats")]
  crate::gc_stats::record_gc_collect();
}

#[cfg(feature = "gc_stats")]
#[no_mangle]
pub unsafe extern "C" fn rt_gc_stats_snapshot(out: *mut crate::abi::RtGcStatsSnapshot) {
  if out.is_null() {
    return;
  }
  *out = crate::gc_stats::snapshot();
}

#[cfg(feature = "gc_stats")]
#[no_mangle]
pub extern "C" fn rt_gc_stats_reset() {
  crate::gc_stats::reset();
}

// -----------------------------------------------------------------------------
// Weak handles (non-owning references)
// -----------------------------------------------------------------------------

/// Create a new weak handle for `value`.
///
/// Weak handles do not keep the referent alive. If the referent is collected, `rt_weak_get`
/// returns null.
#[no_mangle]
pub extern "C" fn rt_weak_add(value: *mut u8) -> u64 {
  crate::gc::weak::global_weak_add(value).as_u64()
}

/// Resolve a weak handle back to a pointer, or null if the referent is dead/cleared.
#[no_mangle]
pub extern "C" fn rt_weak_get(handle: u64) -> *mut u8 {
  crate::gc::weak::global_weak_get(WeakHandle::from_u64(handle)).unwrap_or(std::ptr::null_mut())
}

/// Remove a weak handle.
#[no_mangle]
pub extern "C" fn rt_weak_remove(handle: u64) {
  crate::gc::weak::global_weak_remove(WeakHandle::from_u64(handle));
}

#[no_mangle]
pub extern "C" fn rt_parallel_spawn(task: extern "C" fn(*mut u8), data: *mut u8) -> TaskId {
  let res = catch_unwind(AssertUnwindSafe(|| {
    let rt = crate::rt_ensure_init();
    rt.parallel.spawn(task, data)
  }));
  match res {
    Ok(id) => id,
    Err(_) => std::process::abort(),
  }
}

#[no_mangle]
pub extern "C" fn rt_parallel_join(tasks: *const TaskId, count: usize) {
  let res = catch_unwind(AssertUnwindSafe(|| {
    let rt = crate::rt_ensure_init();
    rt.parallel.join(tasks, count)
  }));
  if res.is_err() {
    std::process::abort();
  }
}

#[no_mangle]
pub extern "C" fn rt_parallel_for(
  start: usize,
  end: usize,
  body: extern "C" fn(usize, *mut u8),
  data: *mut u8,
) {
  let res = catch_unwind(AssertUnwindSafe(|| {
    let rt = crate::rt_ensure_init();
    rt.parallel.parallel_for(start, end, body, data)
  }));
  if res.is_err() {
    std::process::abort();
  }
}

#[no_mangle]
pub extern "C" fn rt_spawn_blocking(
  task: extern "C" fn(*mut u8, PromiseRef),
  data: *mut u8,
) -> PromiseRef {
  let _ = crate::rt_ensure_init();
  ensure_event_loop_thread_registered();
  crate::blocking_pool::spawn(task, data)
}

#[no_mangle]
pub extern "C" fn rt_async_spawn(coro: *mut RtCoroutineHeader) -> PromiseRef {
  let _ = crate::rt_ensure_init();
  ensure_event_loop_thread_registered();
  async_rt::coroutine::async_spawn(coro)
}

/// Drive the runtime's async/event-loop queues.
///
/// This runtime maintains process-global singleton state. `rt_async_poll` may be called from
/// multiple threads, but calls are **globally serialized** (only one thread executes the poll loop
/// at a time).
#[no_mangle]
pub extern "C" fn rt_async_poll() -> bool {
  let _ = crate::rt_ensure_init();
  ensure_event_loop_thread_registered();
  async_rt::poll()
}

#[no_mangle]
pub extern "C" fn rt_async_sleep(delay_ms: u64) -> PromiseRef {
  let _ = crate::rt_ensure_init();
  ensure_event_loop_thread_registered();

  extern "C" fn resolve_sleep(data: *mut u8) {
    let promise = PromiseRef(data.cast());
    async_rt::promise::promise_resolve(promise, core::ptr::null_mut());
  }

  let promise = async_rt::promise::promise_new();
  let _timer_id = async_rt::global().schedule_timer_in(
    std::time::Duration::from_millis(delay_ms),
    async_rt::Task::new(resolve_sleep, promise.0 as *mut u8),
  );
  promise
}

// -----------------------------------------------------------------------------
// I/O readiness watchers (epoll-backed)
// -----------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn rt_io_register(
  fd: i32,
  interests: u32,
  cb: extern "C" fn(u32, *mut u8),
  data: *mut u8,
) -> IoWatcherId {
  let _ = crate::rt_ensure_init();
  let Ok(id) = async_rt::global().register_io(fd, interests, cb, data) else {
    return 0;
  };
  id.as_raw()
}

#[no_mangle]
pub extern "C" fn rt_io_update(id: IoWatcherId, interests: u32) {
  let _ = crate::rt_ensure_init();
  let _ = async_rt::global().update_io(WatcherId::from_raw(id), interests);
}

#[no_mangle]
pub extern "C" fn rt_io_unregister(id: IoWatcherId) {
  let _ = crate::rt_ensure_init();
  let _ = async_rt::global().deregister_fd(WatcherId::from_raw(id));
}

// -----------------------------------------------------------------------------
// Minimal promise ABI (used by async/await lowering)
// -----------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn rt_promise_new() -> PromiseRef {
  ensure_event_loop_thread_registered();
  async_rt::promise::promise_new()
}

#[no_mangle]
pub extern "C" fn rt_promise_resolve(p: PromiseRef, value: ValueRef) {
  ensure_event_loop_thread_registered();
  async_rt::promise::promise_resolve(p, value)
}

#[no_mangle]
pub extern "C" fn rt_promise_reject(p: PromiseRef, err: ValueRef) {
  ensure_event_loop_thread_registered();
  async_rt::promise::promise_reject(p, err)
}

#[no_mangle]
pub extern "C" fn rt_promise_then(p: PromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8) {
  ensure_event_loop_thread_registered();
  async_rt::promise::promise_then(p, on_settle, data)
}

#[no_mangle]
pub extern "C" fn rt_coro_await(coro: *mut RtCoroutineHeader, awaited: PromiseRef, next_state: u32) {
  ensure_event_loop_thread_registered();
  async_rt::coroutine::coro_await(coro, awaited, next_state)
}
