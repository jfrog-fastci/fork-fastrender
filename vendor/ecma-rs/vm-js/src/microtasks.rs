//! A minimal, GC-safe microtask queue for Promise jobs.
//!
//! This is a small host-side container suitable for unit tests and lightweight embeddings that do
//! not yet have a full event loop. It preserves FIFO ordering and implements an HTML-style
//! microtask checkpoint reentrancy guard.

use crate::Job;
use crate::JobCallback;
use crate::RealmId;
use crate::Value;
use crate::VmError;
use crate::VmHostHooks;
use crate::VmJobContext;
use std::collections::VecDeque;

/// A simple, VM-owned microtask queue.
#[derive(Debug, Default)]
pub struct MicrotaskQueue {
  queue: VecDeque<(Option<RealmId>, Job)>,
  performing_microtask_checkpoint: bool,
}

impl MicrotaskQueue {
  /// Creates an empty microtask queue.
  #[inline]
  pub fn new() -> Self {
    Self::default()
  }

  /// Enqueues a Promise job in FIFO order.
  #[inline]
  pub fn enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    self.queue.push_back((realm, job));
  }

  /// Returns the number of queued jobs.
  #[inline]
  pub fn len(&self) -> usize {
    self.queue.len()
  }

  /// Returns whether the queue is empty.
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.queue.is_empty()
  }

  /// Begin a microtask checkpoint.
  ///
  /// This is a low-level API for embeddings that need to run queued jobs with a host hook
  /// implementation *other than* `MicrotaskQueue` itself (for example, a host that also implements
  /// module loading for dynamic `import()`).
  ///
  /// Returns `false` if a checkpoint is already in progress (reentrancy guard).
  pub fn begin_checkpoint(&mut self) -> bool {
    if self.performing_microtask_checkpoint {
      return false;
    }
    self.performing_microtask_checkpoint = true;
    true
  }

  /// Ends a microtask checkpoint started by [`MicrotaskQueue::begin_checkpoint`].
  pub fn end_checkpoint(&mut self) {
    self.performing_microtask_checkpoint = false;
  }

  /// Pops the next queued job in FIFO order.
  ///
  /// This is intended for embeddings that are implementing their own microtask checkpoint loop.
  pub fn pop_front(&mut self) -> Option<(Option<RealmId>, Job)> {
    self.queue.pop_front()
  }

  /// Performs a microtask checkpoint (HTML terminology).
  ///
  /// - If a checkpoint is already in progress, this is a no-op (reentrancy guard).
  /// - Otherwise, drains the queue until it becomes empty.
  ///
  /// Any errors returned by jobs are collected and returned; the checkpoint continues to run later
  /// jobs even if earlier ones fail (HTML's "report the exception and keep draining microtasks"
  /// behavior).
  ///
  /// ## Termination errors
  ///
  /// [`VmError::Termination`] represents a non-catchable, host-enforced termination condition
  /// (fuel exhausted, deadline exceeded, interrupt, stack overflow). Unlike ordinary job failures
  /// (exceptions), termination is treated as a **hard stop**:
  ///
  /// - Once a job returns `Err(VmError::Termination(..))`, the checkpoint stops executing any
  ///   further jobs.
  /// - [`VmError::OutOfMemory`] is also treated as a hard stop: it represents a fatal VM/resource
  ///   condition that must not be suppressed by continuing to run additional jobs.
  /// - Any remaining queued jobs (including jobs enqueued by the failing job) are discarded via
  ///   [`MicrotaskQueue::teardown`] so persistent roots are cleaned up.
  pub fn perform_microtask_checkpoint(&mut self, ctx: &mut dyn VmJobContext) -> Vec<VmError> {
    if self.performing_microtask_checkpoint {
      return Vec::new();
    }

    self.performing_microtask_checkpoint = true;
    let mut errors = Vec::new();
    while let Some((_realm, job)) = self.queue.pop_front() {
      if let Err(err) = job.run(ctx, self) {
        let is_hard_stop = matches!(err, VmError::Termination(_) | VmError::OutOfMemory);

        // `Vec::push` can abort the process on allocator OOM. Reserve fallibly and treat allocation
        // failure as a best-effort stop: tear down remaining jobs so persistent roots are cleaned
        // up, and return the errors collected so far.
        if errors.try_reserve(1).is_err() {
          self.teardown(ctx);
          return errors;
        }
        errors.push(err);
        if is_hard_stop {
          // Hard stop: discard any remaining queued jobs (and any jobs enqueued by the failing job)
          // so we don't leak persistent roots.
          self.teardown(ctx);
          break;
        }
      }
    }
    self.performing_microtask_checkpoint = false;
    errors
  }

  /// Tears down all queued jobs without running them.
  ///
  /// This unregisters any persistent roots held by queued jobs. Use this when an embedding needs
  /// to abandon the queue but still intends to reuse the heap.
  pub fn teardown(&mut self, ctx: &mut dyn VmJobContext) {
    while let Some((_realm, job)) = self.queue.pop_front() {
      job.discard(ctx);
    }
    // Teardown implies abandoning any in-progress checkpoint; reset the reentrancy guard so the
    // queue can be reused even if the embedding aborts mid-checkpoint.
    self.performing_microtask_checkpoint = false;
  }

  /// Alias for [`MicrotaskQueue::teardown`].
  pub fn cancel_all(&mut self, ctx: &mut dyn VmJobContext) {
    self.teardown(ctx);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use crate::test_alloc::FailAllocsGuard;
  use crate::Heap;
  use crate::HeapLimits;
  use crate::Job;
  use crate::JobKind;
  use crate::RootId;
  use crate::Value;
  use crate::VmError;
  use crate::VmHostHooks;
  use crate::VmJobContext;
  use crate::WeakGcObject;

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
      self.heap.remove_root(id);
    }
  }

  #[test]
  fn microtask_checkpoint_error_collection_does_not_abort_on_allocator_oom() -> Result<(), VmError> {
    let mut ctx = TestContext::new();
    let mut queue = MicrotaskQueue::new();

    // Ensure we have at least one queued job that will return an error during the checkpoint.
    queue.enqueue_promise_job(
      Job::new(JobKind::Promise, |_ctx, _host| Err(VmError::Unimplemented("job failed"))),
      None,
    );

    // Also enqueue a job holding a persistent root so we can validate teardown still cleans up
    // roots even when error collection OOMs.
    let obj = {
      let mut scope = ctx.heap.scope();
      scope.alloc_object()?
    };
    let weak = WeakGcObject::from(obj);
    let mut rooted_job = Job::new(JobKind::Promise, |_ctx, _host| Ok(()));
    rooted_job.add_root(&mut ctx, Value::Object(obj))?;
    queue.enqueue_promise_job(rooted_job, None);

    // The queued job holds a persistent root, so a GC cycle should not collect the object yet.
    ctx.heap.collect_garbage();
    assert_eq!(weak.upgrade(&ctx.heap), Some(obj));

    // Simulate allocator OOM right before the checkpoint attempts to collect errors.
    let _guard = FailAllocsGuard::new();
    let errors = queue.perform_microtask_checkpoint(&mut ctx);
    drop(_guard);

    // The checkpoint must not abort. Error collection is best-effort under OOM, so it may return an
    // empty vector.
    assert!(errors.is_empty());
    assert!(queue.is_empty(), "expected queue to be torn down on OOM");

    // After teardown, the rooted job's persistent root should be removed, making the object
    // collectible.
    ctx.heap.collect_garbage();
    assert_eq!(weak.upgrade(&ctx.heap), None);
    Ok(())
  }
}

impl VmHostHooks for MicrotaskQueue {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    self.enqueue_promise_job(job, realm);
  }

  fn host_call_job_callback(
    &mut self,
    ctx: &mut dyn VmJobContext,
    callback: &JobCallback,
    this_argument: Value,
    arguments: &[Value],
  ) -> Result<Value, VmError> {
    ctx.call(
      self,
      Value::Object(callback.callback_object()),
      this_argument,
      arguments,
    )
  }

  fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
    Some(self)
  }
}
