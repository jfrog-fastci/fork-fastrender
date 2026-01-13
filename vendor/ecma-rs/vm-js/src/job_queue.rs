//! Minimal microtask/job queue scaffolding.
//!
//! This module exists to provide a small, spec-shaped substrate for embeddings that want an
//! internal queue for Promise jobs / microtasks. The VM currently delegates actual scheduling to
//! the host via [`VmHostHooks::host_enqueue_promise_job`](crate::VmHostHooks::host_enqueue_promise_job),
//! but having a concrete queue type is useful for tests and simple embeddings.

use crate::jobs::Job;
use crate::VmError;
use crate::VmJobContext;
use std::collections::VecDeque;

/// A queued microtask job.
///
/// For now, microtasks are represented directly as [`Job`] records.
pub type MicrotaskJob = Job;

/// A FIFO microtask queue.
#[derive(Default, Debug)]
pub struct JobQueue {
  queue: VecDeque<MicrotaskJob>,
}

impl JobQueue {
  pub fn new() -> Self {
    Self::default()
  }

  /// Attempts to enqueue `job` at the back of the queue.
  ///
  /// This uses a fallible reservation (`try_reserve`) so allocator OOM is surfaced as
  /// [`VmError::OutOfMemory`] rather than aborting the process.
  ///
  /// ## GC rooting
  ///
  /// [`Job`]s can own persistent roots. If enqueuing fails (OOM), the job must be discarded so those
  /// roots are unregistered; otherwise dropping the job would leak roots and trigger debug
  /// assertions.
  pub fn try_push(
    &mut self,
    ctx: &mut dyn VmJobContext,
    job: MicrotaskJob,
  ) -> Result<(), VmError> {
    if self.queue.try_reserve(1).is_err() {
      // Ensure we do not drop a job that still owns persistent roots.
      job.discard(ctx);
      return Err(VmError::OutOfMemory);
    }
    // `try_reserve(1)` guarantees `push_back` won't grow/reallocate the buffer.
    self.queue.push_back(job);
    Ok(())
  }

  /// Enqueues `job` at the back of the queue.
  ///
  /// This is a convenience wrapper around [`JobQueue::try_push`].
  pub fn push(&mut self, ctx: &mut dyn VmJobContext, job: MicrotaskJob) -> Result<(), VmError> {
    self.try_push(ctx, job)
  }

  pub fn pop(&mut self) -> Option<MicrotaskJob> {
    self.queue.pop_front()
  }

  pub fn is_empty(&self) -> bool {
    self.queue.is_empty()
  }

  pub fn len(&self) -> usize {
    self.queue.len()
  }
}

// These types are used by the rejection tracker API and are re-exported through `vm_js::...`.
pub use crate::jobs::PromiseHandle;
pub use crate::jobs::PromiseRejectionOperation;
