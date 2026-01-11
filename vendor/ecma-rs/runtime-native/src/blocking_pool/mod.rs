mod pool;

use crate::abi::PromiseRef;

pub(crate) fn spawn(task: extern "C" fn(*mut u8, PromiseRef), data: *mut u8) -> PromiseRef {
  pool::global().spawn(task, data)
}
