use crate::abi::PromiseRef;
use crate::abi::RtCoroutineHeader;
use crate::abi::ShapeId;
use crate::abi::TaskId;
use crate::abi::ValueRef;
use crate::alloc;
use crate::async_rt;
use crate::gc::ObjHeader;
use crate::gc::YOUNG_SPACE;
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
  alloc::malloc_bytes(size, "rt_alloc")
}

/// Allocate a pinned (non-moving) object.
///
/// NOTE: The milestone runtime does not yet wire allocations into the GC. This entrypoint exists so
/// codegen/FFI can request a stable address today and so future GC-backed allocation can route
/// pinned objects to a non-moving space.
#[no_mangle]
pub extern "C" fn rt_alloc_pinned(size: usize, _shape: ShapeId) -> *mut u8 {
  alloc::malloc_bytes(size, "rt_alloc_pinned")
}

#[no_mangle]
pub extern "C" fn rt_alloc_array(len: usize, elem_size: usize) -> *mut u8 {
  alloc::calloc_array(len, elem_size, "rt_alloc_array")
}

/// GC safepoint.
#[no_mangle]
pub extern "C" fn rt_gc_safepoint() {
  crate::threading::safepoint::rt_gc_safepoint();
}

/// Update the active young-space address range used by the write barrier.
///
/// This must be called by the GC during initialization and after each nursery
/// flip/resize that changes the current young generation region.
#[no_mangle]
pub extern "C" fn rt_gc_set_young_range(start: *mut u8, end: *mut u8) {
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
  if obj.is_null() || slot.is_null() {
    return;
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

/// Trigger a GC cycle.
///
/// Milestone-1 runtime: no-op.
#[no_mangle]
pub extern "C" fn rt_gc_collect() {}

#[no_mangle]
pub extern "C" fn rt_parallel_spawn(task: extern "C" fn(*mut u8), data: *mut u8) -> TaskId {
  let rt = crate::rt_ensure_init();
  rt.parallel.spawn(task, data)
}

#[no_mangle]
pub extern "C" fn rt_parallel_join(tasks: *const TaskId, count: usize) {
  let rt = crate::rt_ensure_init();
  rt.parallel.join(tasks, count)
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
