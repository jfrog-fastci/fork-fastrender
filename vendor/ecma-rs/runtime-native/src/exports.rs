use crate::abi::PromiseRef;
use crate::abi::RtCoroutineHeader;
use crate::abi::ShapeId;
use crate::abi::TaskId;
use crate::abi::ValueRef;
use crate::alloc;
use crate::async_rt;

#[no_mangle]
pub extern "C" fn rt_alloc(size: usize, _shape: ShapeId) -> *mut u8 {
  alloc::malloc_bytes(size, "rt_alloc")
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

/// Write barrier for GC.
///
/// Milestone-1 runtime: no-op.
#[no_mangle]
pub extern "C" fn rt_write_barrier(_obj: *mut u8, _field: *mut u8) {}

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
  async_rt::coroutine::async_spawn(coro)
}

#[no_mangle]
pub extern "C" fn rt_async_poll() -> bool {
  let _ = crate::rt_ensure_init();
  async_rt::poll()
}

// -----------------------------------------------------------------------------
// Minimal promise ABI (used by async/await lowering)
// -----------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn rt_promise_new() -> PromiseRef {
  async_rt::promise::promise_new()
}

#[no_mangle]
pub extern "C" fn rt_promise_resolve(p: PromiseRef, value: ValueRef) {
  async_rt::promise::promise_resolve(p, value)
}

#[no_mangle]
pub extern "C" fn rt_promise_reject(p: PromiseRef, err: ValueRef) {
  async_rt::promise::promise_reject(p, err)
}

#[no_mangle]
pub extern "C" fn rt_promise_then(p: PromiseRef, on_settle: extern "C" fn(*mut u8), data: *mut u8) {
  async_rt::promise::promise_then(p, on_settle, data)
}

#[no_mangle]
pub extern "C" fn rt_coro_await(coro: *mut RtCoroutineHeader, awaited: PromiseRef, next_state: u32) {
  async_rt::coroutine::coro_await(coro, awaited, next_state)
}
