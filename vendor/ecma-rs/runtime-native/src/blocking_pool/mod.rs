mod pool;

use crate::abi::PromiseRef;

pub(crate) fn spawn(task: extern "C" fn(*mut u8, PromiseRef), data: *mut u8) -> PromiseRef {
  pool::global().spawn(task, data)
}

pub(crate) fn spawn_rooted(task: extern "C" fn(*mut u8, PromiseRef), data: *mut u8) -> PromiseRef {
  pool::global().spawn_rooted(task, data)
}

pub(crate) unsafe fn spawn_rooted_h(
  task: extern "C" fn(*mut u8, PromiseRef),
  data: crate::roots::GcHandle,
) -> PromiseRef {
  pool::global().spawn_rooted_h(task, data)
}

#[doc(hidden)]
pub(crate) fn debug_hold_queue_lock() -> impl Drop {
  pool::debug_hold_queue_lock()
}
