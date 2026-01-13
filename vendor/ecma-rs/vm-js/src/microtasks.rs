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
  /// (fuel exhausted, deadline exceeded, interrupt). Unlike ordinary job failures (exceptions),
  /// termination is treated as a **hard stop**:
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
  use crate::GcObject;
  use crate::Heap;
  use crate::HeapLimits;
  use crate::Job;
  use crate::JobKind;
  use crate::Realm;
  use crate::RootId;
  use crate::Scope;
  use crate::TypedArrayKind;
  use crate::Value;
  use crate::Vm;
  use crate::VmError;
  use crate::VmHost;
  use crate::VmHostHooks;
  use crate::VmJobContext;
  use crate::VmOptions;
  use crate::WeakGcObject;

  struct TestContext {
    heap: Heap,
    vm: Vm,
  }

  impl TestContext {
    fn new() -> Self {
      Self {
        heap: Heap::new(HeapLimits::new(1024 * 1024, 512 * 1024)),
        vm: Vm::new(VmOptions::default()),
      }
    }
  }

  impl VmJobContext for TestContext {
    fn call(
      &mut self,
      host: &mut dyn VmHostHooks,
      callee: Value,
      this: Value,
      args: &[Value],
    ) -> Result<Value, VmError> {
      // Borrow-split `vm` + `heap` so we can hold a `Scope` while calling into the VM.
      let vm = &mut self.vm;
      let heap = &mut self.heap;
      let mut scope = heap.scope();
      vm.call_with_host(&mut scope, host, callee, this, args)
    }

    fn construct(
      &mut self,
      host: &mut dyn VmHostHooks,
      callee: Value,
      args: &[Value],
      new_target: Value,
    ) -> Result<Value, VmError> {
      let vm = &mut self.vm;
      let heap = &mut self.heap;
      let mut scope = heap.scope();
      vm.construct_with_host(&mut scope, host, callee, args, new_target)
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
      Job::new(JobKind::Promise, |_ctx, _host| Err(VmError::Unimplemented("job failed")))?,
      None,
    );

    // Also enqueue a job holding a persistent root so we can validate teardown still cleans up
    // roots even when error collection OOMs.
    let obj = {
      let mut scope = ctx.heap.scope();
      scope.alloc_object()?
    };
    let weak = WeakGcObject::from(obj);
    let mut rooted_job = Job::new(JobKind::Promise, |_ctx, _host| Ok(()))?;
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

  fn assert_typed_array_and_data_view_are_live(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    const EXPECTED_BYTES: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    let Some(Value::Object(ta)) = args.get(0).copied() else {
      return Err(VmError::TypeError("expected Uint8Array argument"));
    };
    let Some(Value::Object(dv)) = args.get(1).copied() else {
      return Err(VmError::TypeError("expected DataView argument"));
    };
    let Some(Value::Object(ab)) = args.get(2).copied() else {
      return Err(VmError::TypeError("expected ArrayBuffer argument"));
    };

    // Validate the ArrayBuffer handle and contents.
    let data = scope.heap().array_buffer_data(ab)?;
    if data != EXPECTED_BYTES {
      return Err(VmError::InvariantViolation("ArrayBuffer contents mismatch"));
    }

    // Validate the typed array view and its link to the backing buffer.
    if scope.heap().typed_array_kind(ta)? != TypedArrayKind::Uint8 {
      return Err(VmError::InvariantViolation("expected Uint8Array kind"));
    }
    let (ta_buf, ta_off, ta_len) = scope.heap().typed_array_view_bytes(ta)?;
    if ta_buf != ab || ta_off != 0 || ta_len != EXPECTED_BYTES.len() {
      return Err(VmError::InvariantViolation(
        "Uint8Array view does not match backing ArrayBuffer",
      ));
    }

    // Validate the DataView view and its link to the backing buffer.
    if scope.heap().data_view_byte_length(dv)? != EXPECTED_BYTES.len() {
      return Err(VmError::InvariantViolation("DataView byteLength mismatch"));
    }
    if scope.heap().data_view_byte_offset(dv)? != 0 {
      return Err(VmError::InvariantViolation("DataView byteOffset mismatch"));
    }
    let dv_buf = scope.heap().data_view_buffer(dv)?;
    if dv_buf != ab {
      return Err(VmError::InvariantViolation(
        "DataView buffer does not match expected ArrayBuffer",
      ));
    }

    Ok(Value::Undefined)
  }

  #[test]
  fn promise_jobs_keep_typed_array_and_data_view_alive_across_gc_until_run() -> Result<(), VmError> {
    let mut ctx = TestContext::new();
    let mut realm = Realm::new(&mut ctx.vm, &mut ctx.heap)?;
    let intr = *realm.intrinsics();

    // Allocate an ArrayBuffer + views that will only be kept alive by the queued job.
    let (checker, array_buffer, typed_array, data_view) = {
      let vm = &mut ctx.vm;
      let heap = &mut ctx.heap;
      let mut scope = heap.scope();

      let check_id = vm.register_native_call(assert_typed_array_and_data_view_are_live)?;
      let name = scope.alloc_string("")?;
      let checker = scope.alloc_native_function(check_id, None, name, 0)?;
      scope.push_root(Value::Object(checker))?;
      scope
        .heap_mut()
        .object_set_prototype(checker, Some(intr.function_prototype()))?;

      let ab = scope.alloc_array_buffer(8)?;
      scope.push_root(Value::Object(ab))?;
      scope
        .heap_mut()
        .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
      scope
        .heap_mut()
        .array_buffer_write(ab, 0, &[1, 2, 3, 4, 5, 6, 7, 8])?;

      let ta = scope.alloc_uint8_array(ab, 0, 8)?;
      scope.push_root(Value::Object(ta))?;
      scope
        .heap_mut()
        .object_set_prototype(ta, Some(intr.uint8_array_prototype()))?;

      let dv = scope.alloc_data_view(ab, 0, 8)?;
      scope.push_root(Value::Object(dv))?;
      scope
        .heap_mut()
        .object_set_prototype(dv, Some(intr.data_view_prototype()))?;

      (checker, ab, ta, dv)
    };

    let weak_ab = WeakGcObject::from(array_buffer);
    let weak_ta = WeakGcObject::from(typed_array);
    let weak_dv = WeakGcObject::from(data_view);

    let root_count_before = ctx.heap.persistent_root_count();

    let mut queue = MicrotaskQueue::new();
    let checker_val = Value::Object(checker);
    let typed_array_val = Value::Object(typed_array);
    let data_view_val = Value::Object(data_view);
    let array_buffer_val = Value::Object(array_buffer);

    let mut job = Job::new(JobKind::Promise, move |ctx, host| {
      ctx.call(
        host,
        checker_val,
        Value::Undefined,
        &[typed_array_val, data_view_val, array_buffer_val],
      )?;
      Ok(())
    })?;

    // Jobs are opaque closures; explicitly root captured handles until the job runs.
    //
    // Root all values on the stack while creating persistent roots so a GC triggered by one root
    // allocation cannot collect the other yet-to-be-rooted values.
    let values = [checker_val, typed_array_val, data_view_val, array_buffer_val];
    let stack_len = ctx.heap.stack_root_len();
    ctx.heap.push_stack_roots(&values)?;
    let root_result: Result<(), VmError> = (|| {
      job.add_root(&mut ctx, checker_val)?;
      job.add_root(&mut ctx, typed_array_val)?;
      job.add_root(&mut ctx, data_view_val)?;
      job.add_root(&mut ctx, array_buffer_val)?;
      Ok(())
    })();
    ctx.heap.truncate_stack_roots(stack_len);
    root_result?;

    queue.enqueue_promise_job(job, None);
    assert_eq!(ctx.heap.persistent_root_count(), root_count_before + 4);

    // A GC between enqueue and execution must not invalidate the captured handles.
    ctx.heap.collect_garbage();
    assert_eq!(weak_ab.upgrade(&ctx.heap), Some(array_buffer));
    assert_eq!(weak_ta.upgrade(&ctx.heap), Some(typed_array));
    assert_eq!(weak_dv.upgrade(&ctx.heap), Some(data_view));

    let errors = queue.perform_microtask_checkpoint(&mut ctx);
    assert!(errors.is_empty(), "unexpected microtask errors: {errors:?}");
    assert_eq!(ctx.heap.persistent_root_count(), root_count_before);

    // After the job runs and roots are removed, the views/buffer should be collectible.
    ctx.heap.collect_garbage();
    assert_eq!(weak_ab.upgrade(&ctx.heap), None);
    assert_eq!(weak_ta.upgrade(&ctx.heap), None);
    assert_eq!(weak_dv.upgrade(&ctx.heap), None);

    realm.teardown(&mut ctx.heap);
    Ok(())
  }

  #[test]
  fn queued_job_persistent_root_keeps_uint8array_and_buffer_alive_across_gc() -> Result<(), VmError> {
    let mut ctx = TestContext::new();
    let mut queue = MicrotaskQueue::new();

    let (weak_view, weak_buffer) = {
      let (buffer, view) = {
        let mut scope = ctx.heap.scope();
        let buffer = scope.alloc_array_buffer(8)?;
        let view = scope.alloc_uint8_array(buffer, 0, 8)?;
        (buffer, view)
      };

      let weak_view = WeakGcObject::from(view);
      let weak_buffer = WeakGcObject::from(buffer);

      assert_eq!(ctx.heap.stack_root_len(), 0);
      assert_eq!(ctx.heap.persistent_root_count(), 0);

      let mut job = Job::new(JobKind::Promise, |_ctx, _host| Ok(()))?;
      // Root only the view. The GC must trace the view -> ArrayBuffer edge to keep `buffer` alive.
      job.add_root(&mut ctx, Value::Object(view))?;
      queue.enqueue_promise_job(job, None);

      assert_eq!(ctx.heap.persistent_root_count(), 1);
      (weak_view, weak_buffer)
    };

    // While the job remains queued, its persistent root must keep both the view and its backing
    // buffer alive across GC.
    ctx.heap.collect_garbage();
    assert!(weak_view.upgrade(&ctx.heap).is_some(), "Uint8Array view should be kept alive by job root");
    assert!(
      weak_buffer.upgrade(&ctx.heap).is_some(),
      "ArrayBuffer should be kept alive via the rooted view"
    );

    // After teardown, the job's roots must be dropped, making both objects collectible.
    queue.teardown(&mut ctx);
    assert!(queue.is_empty());
    assert_eq!(ctx.heap.persistent_root_count(), 0);
    ctx.heap.collect_garbage();
    assert_eq!(weak_view.upgrade(&ctx.heap), None);
    assert_eq!(weak_buffer.upgrade(&ctx.heap), None);
    Ok(())
  }

  #[test]
  fn queued_job_persistent_root_keeps_data_view_and_buffer_alive_across_gc() -> Result<(), VmError> {
    let mut ctx = TestContext::new();
    let mut queue = MicrotaskQueue::new();

    let (weak_view, weak_buffer) = {
      let (buffer, view) = {
        let mut scope = ctx.heap.scope();
        let buffer = scope.alloc_array_buffer(16)?;
        let view = scope.alloc_data_view(buffer, 0, 16)?;
        (buffer, view)
      };

      let weak_view = WeakGcObject::from(view);
      let weak_buffer = WeakGcObject::from(buffer);

      assert_eq!(ctx.heap.stack_root_len(), 0);
      assert_eq!(ctx.heap.persistent_root_count(), 0);

      let mut job = Job::new(JobKind::Promise, |_ctx, _host| Ok(()))?;
      // Root only the view. The GC must trace the view -> ArrayBuffer edge to keep `buffer` alive.
      job.add_root(&mut ctx, Value::Object(view))?;
      queue.enqueue_promise_job(job, None);

      assert_eq!(ctx.heap.persistent_root_count(), 1);
      (weak_view, weak_buffer)
    };

    ctx.heap.collect_garbage();
    assert!(weak_view.upgrade(&ctx.heap).is_some(), "DataView should be kept alive by job root");
    assert!(
      weak_buffer.upgrade(&ctx.heap).is_some(),
      "ArrayBuffer should be kept alive via the rooted DataView"
    );

    queue.teardown(&mut ctx);
    assert!(queue.is_empty());
    assert_eq!(ctx.heap.persistent_root_count(), 0);
    ctx.heap.collect_garbage();
    assert_eq!(weak_view.upgrade(&ctx.heap), None);
    assert_eq!(weak_buffer.upgrade(&ctx.heap), None);
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
