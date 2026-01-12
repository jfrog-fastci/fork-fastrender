mod pool;

use crate::abi::LegacyPromiseRef;

pub(crate) fn spawn(task: extern "C" fn(*mut u8, LegacyPromiseRef), data: *mut u8) -> LegacyPromiseRef {
  pool::global().spawn(task, data)
}

#[doc(hidden)]
pub(crate) fn debug_hold_queue_lock() -> impl Drop {
  pool::debug_hold_queue_lock()
}
