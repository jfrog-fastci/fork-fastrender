mod pool;

use crate::abi::{LegacyPromiseRef, PromiseRef};
use crate::async_runtime::PromiseLayout;

pub(crate) fn spawn(task: extern "C" fn(*mut u8, LegacyPromiseRef), data: *mut u8) -> LegacyPromiseRef {
  pool::global().spawn(task, data)
}

pub(crate) fn spawn_promise(
  task: extern "C" fn(*mut u8, *mut u8) -> u8,
  data: *mut u8,
  layout: PromiseLayout,
) -> PromiseRef {
  pool::global().spawn_promise(task, data, layout)
}

pub(crate) fn spawn_promise_rooted(
  task: extern "C" fn(*mut u8, *mut u8) -> u8,
  data: *mut u8,
  layout: PromiseLayout,
) -> PromiseRef {
  pool::global().spawn_promise_rooted(task, data, layout)
}

pub(crate) unsafe fn spawn_promise_rooted_h(
  task: extern "C" fn(*mut u8, *mut u8) -> u8,
  data: crate::roots::GcHandle,
  layout: PromiseLayout,
) -> PromiseRef {
  pool::global().spawn_promise_rooted_h(task, data, layout)
}

pub(crate) fn cancel_all_pending() {
  pool::cancel_all_pending();
}
#[doc(hidden)]
pub(crate) fn debug_hold_queue_lock() -> impl Drop {
  pool::debug_hold_queue_lock()
}
