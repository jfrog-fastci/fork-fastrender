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

  #[test]
  fn queued_job_persistent_root_keeps_uint8array_and_buffer_alive_across_gc() -> Result<(), VmError> {
    let mut ctx = TestContext::new();
    let mut queue = JobQueue::new();

    let (weak_view, weak_buffer, baseline_roots) = {
      let (buffer, view) = {
        let mut scope = ctx.heap.scope();
        let buffer = scope.alloc_array_buffer(8)?;
        let view = scope.alloc_uint8_array(buffer, 0, 8)?;
        (buffer, view)
      };

      let weak_view = WeakGcObject::from(view);
      let weak_buffer = WeakGcObject::from(buffer);
      let baseline_roots = ctx.heap.persistent_root_count();

      let mut job = Job::new(JobKind::Promise, |_ctx, _host| Ok(()))?;
      // Root only the view; the GC must trace the view -> ArrayBuffer edge to keep `buffer` alive.
      job.add_root(&mut ctx, Value::Object(view))?;
      queue.push(&mut ctx, job)?;

      (weak_view, weak_buffer, baseline_roots)
    };

    // While the job remains queued, its persistent root must keep both the view and its backing
    // buffer alive across GC.
    ctx.heap.collect_garbage();
    assert!(
      weak_view.upgrade(&ctx.heap).is_some(),
      "Uint8Array view should be kept alive by queued job root"
    );
    assert!(
      weak_buffer.upgrade(&ctx.heap).is_some(),
      "ArrayBuffer should be kept alive via the rooted Uint8Array view"
    );

    // Once the job is discarded, its roots must be removed, making both objects collectible.
    let job = queue.pop().expect("job should be queued");
    job.discard(&mut ctx);
    assert!(queue.is_empty());
    assert_eq!(ctx.heap.persistent_root_count(), baseline_roots);
    ctx.heap.collect_garbage();
    assert_eq!(weak_view.upgrade(&ctx.heap), None);
    assert_eq!(weak_buffer.upgrade(&ctx.heap), None);
    Ok(())
  }

  #[test]
  fn queued_job_persistent_root_keeps_data_view_and_buffer_alive_across_gc() -> Result<(), VmError> {
    let mut ctx = TestContext::new();
    let mut queue = JobQueue::new();

    let (weak_view, weak_buffer, baseline_roots) = {
      let (buffer, view) = {
        let mut scope = ctx.heap.scope();
        let buffer = scope.alloc_array_buffer(16)?;
        let view = scope.alloc_data_view(buffer, 0, 16)?;
        (buffer, view)
      };

      let weak_view = WeakGcObject::from(view);
      let weak_buffer = WeakGcObject::from(buffer);
      let baseline_roots = ctx.heap.persistent_root_count();

      let mut job = Job::new(JobKind::Promise, |_ctx, _host| Ok(()))?;
      // Root only the view; the GC must trace the view -> ArrayBuffer edge to keep `buffer` alive.
      job.add_root(&mut ctx, Value::Object(view))?;
      queue.push(&mut ctx, job)?;

      (weak_view, weak_buffer, baseline_roots)
    };

    ctx.heap.collect_garbage();
    assert!(
      weak_view.upgrade(&ctx.heap).is_some(),
      "DataView should be kept alive by queued job root"
    );
    assert!(
      weak_buffer.upgrade(&ctx.heap).is_some(),
      "ArrayBuffer should be kept alive via the rooted DataView"
    );

    let job = queue.pop().expect("job should be queued");
    job.discard(&mut ctx);
    assert!(queue.is_empty());
    assert_eq!(ctx.heap.persistent_root_count(), baseline_roots);
    ctx.heap.collect_garbage();
    assert_eq!(weak_view.upgrade(&ctx.heap), None);
    assert_eq!(weak_buffer.upgrade(&ctx.heap), None);
    Ok(())
  }
}

// These types are used by the rejection tracker API and are re-exported through `vm_js::...`.
pub use crate::jobs::PromiseHandle;
pub use crate::jobs::PromiseRejectionOperation;
