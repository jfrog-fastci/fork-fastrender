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

#[cfg(test)]
mod tests {
  use super::*;

  use crate::test_alloc::FailAllocsGuard;
  use crate::{Heap, HeapLimits, JobKind, RootId, Value, VmError, VmHostHooks, VmJobContext, WeakGcObject};

  struct TestContext {
    heap: Heap,
  }

  impl TestContext {
    fn new() -> Self {
      Self {
        heap: Heap::new(HeapLimits::new(1024 * 1024, 512 * 1024)),
      }
    }
  }

  impl VmJobContext for TestContext {
    fn call(
      &mut self,
      _host: &mut dyn VmHostHooks,
      _callee: Value,
      _this: Value,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      Err(VmError::Unimplemented("TestContext::call"))
    }

    fn construct(
      &mut self,
      _host: &mut dyn VmHostHooks,
      _callee: Value,
      _args: &[Value],
      _new_target: Value,
    ) -> Result<Value, VmError> {
      Err(VmError::Unimplemented("TestContext::construct"))
    }

    fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
      self.heap.add_root(value)
    }

    fn remove_root(&mut self, id: RootId) {
      self.heap.remove_root(id)
    }
  }

  #[test]
  fn try_push_discards_job_on_oom_to_avoid_root_leaks() -> Result<(), VmError> {
    let mut ctx = TestContext::new();
    let mut queue = JobQueue::new();

    // Allocate an object and keep only a weak handle so the only strong reference is the job's
    // persistent root.
    let obj = {
      let mut scope = ctx.heap.scope();
      scope.alloc_object()?
    };
    let weak = WeakGcObject::from(obj);

    let baseline_roots = ctx.heap.persistent_root_count();

    let mut job = Job::new(JobKind::Promise, |_ctx, _host| Ok(()))?;
    job.add_root(&mut ctx, Value::Object(obj))?;
    assert_eq!(ctx.heap.persistent_root_count(), baseline_roots + 1);

    // The job's root should keep the object alive across GC.
    ctx.heap.collect_garbage();
    assert_eq!(weak.upgrade(&ctx.heap), Some(obj));

    // Simulate allocator OOM when the queue tries to grow.
    let _guard = FailAllocsGuard::new();
    let res = queue.try_push(&mut ctx, job);
    drop(_guard);

    assert!(matches!(res, Err(VmError::OutOfMemory)));

    // `try_push` must discard the job so its persistent root is unregistered.
    assert_eq!(ctx.heap.persistent_root_count(), baseline_roots);
    ctx.heap.collect_garbage();
    assert_eq!(weak.upgrade(&ctx.heap), None);
    Ok(())
  }
}

// These types are used by the rejection tracker API and are re-exported through `vm_js::...`.
pub use crate::jobs::PromiseHandle;
pub use crate::jobs::PromiseRejectionOperation;
