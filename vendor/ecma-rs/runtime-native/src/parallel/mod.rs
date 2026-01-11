use std::sync::atomic::{AtomicU64, Ordering};

use crate::abi::TaskId;

pub(crate) struct ParallelRuntime {
  next_task_id: AtomicU64,
}

impl ParallelRuntime {
  pub(crate) fn new() -> Self {
    Self {
      next_task_id: AtomicU64::new(1),
    }
  }

  pub(crate) fn spawn(&self, _task: extern "C" fn(*mut u8), _data: *mut u8) -> TaskId {
    TaskId(self.next_task_id.fetch_add(1, Ordering::Relaxed))
  }

  pub(crate) fn join(&self, _tasks: *const TaskId, _count: usize) {}
}
