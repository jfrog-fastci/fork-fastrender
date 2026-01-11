use core::ffi::c_void;

use crate::abi::PromiseRef;
use crate::abi::ShapeId;
use crate::abi::TaskId;
use crate::alloc;
use crate::trap;

#[no_mangle]
pub extern "C" fn rt_alloc(size: usize, _shape: ShapeId) -> *mut u8 {
  alloc::malloc_bytes(size, "rt_alloc")
}

#[no_mangle]
pub extern "C" fn rt_alloc_array(len: usize, elem_size: usize) -> *mut u8 {
  alloc::calloc_array(len, elem_size, "rt_alloc_array")
}

/// GC safepoint.
///
/// Milestone-1 runtime: no-op.
#[no_mangle]
pub extern "C" fn rt_gc_safepoint() {}

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
pub extern "C" fn rt_parallel_spawn(_task: extern "C" fn(*mut u8), _data: *mut u8) -> TaskId {
  trap::rt_trap_unimplemented("rt_parallel_spawn")
}

#[no_mangle]
pub extern "C" fn rt_parallel_join(_tasks: *const TaskId, _count: usize) {
  trap::rt_trap_unimplemented("rt_parallel_join")
}

#[no_mangle]
pub extern "C" fn rt_async_spawn(_coro: *mut c_void) -> PromiseRef {
  trap::rt_trap_unimplemented("rt_async_spawn")
}

#[no_mangle]
pub extern "C" fn rt_async_poll() -> bool {
  trap::rt_trap_unimplemented("rt_async_poll")
}

