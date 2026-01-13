//! Promise algorithm scaffolding and Promise-internal-slot types.
//!
//! This module intentionally implements only the parts of the ECMA-262 Promise algorithms that are
//! needed to model *job scheduling* and the HTML integration points:
//!
//! - [`VmHostHooks::host_make_job_callback`] (HTML: `HostMakeJobCallback`, fallible)
//! - [`VmHostHooks::host_make_job_callback_fallible`] (deprecated alias)
//! - [`VmHostHooks::host_call_job_callback`] (HTML: `HostCallJobCallback`)
//!
//! In addition to job-scheduling scaffolding, it defines **GC-traceable, spec-shaped record types**
//! for Promise internal slots used by the heap's Promise object representation
//! (`HeapObject::Promise`).
//!
//! The spec requires Promise jobs (reaction jobs and thenable jobs) to call user-provided
//! callbacks via `HostCallJobCallback` so the embedding can re-establish incumbent/entry settings.
//!
//! Finally, this module exposes a minimal engine-internal [`Promise`] record and an [`await_value`]
//! helper that schedules async/`await` continuations as Promise jobs (microtasks) without creating
//! a derived promise.
//!
//! Spec references:
//! - `Await` abstract operation: <https://tc39.es/ecma262/#await>
//! - `PerformPromiseThen`: <https://tc39.es/ecma262/#sec-performpromisethen>
//! - `PromiseReactionJob`: <https://tc39.es/ecma262/#sec-promisereactionjob>
//! - `HostPromiseRejectionTracker`: <https://tc39.es/ecma262/#sec-host-promise-rejection-tracker>

use crate::heap::{Trace, Tracer};
use crate::promise_jobs::new_promise_reaction_job;
use crate::promise_jobs::new_promise_resolve_thenable_job;
use crate::{
  GcObject, Heap, Job, JobCallback, PromiseHandle, PromiseRejectionOperation, RootId, Value, VmError,
  VmHostHooks, VmJobContext,
};
use std::cell::RefCell;
use std::mem;
use std::rc::Rc;

struct EnqueueCtx<'a> {
  heap: &'a mut Heap,
}

impl VmJobContext for EnqueueCtx<'_> {
  fn call(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("EnqueueCtx::call"))
  }

  fn construct(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("EnqueueCtx::construct"))
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id);
  }
}

/// The value of a Promise object's `[[PromiseState]]` internal slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromiseState {
  Pending,
  Fulfilled,
  Rejected,
}

/// The `[[Type]]` of a Promise reaction record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromiseReactionType {
  Fulfill,
  Reject,
}

/// An ECMAScript PromiseCapability Record.
///
/// Spec reference: <https://tc39.es/ecma262/#sec-promisecapability-records>
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PromiseCapability {
  /// The `[[Promise]]` value.
  ///
  /// Per spec this is a Promise object, but `vm-js` stores it as a [`Value`] to simplify rooting
  /// and allow `Value::Undefined` as a sentinel during partially-initialized paths.
  pub promise: Value,
  pub resolve: Value,
  pub reject: Value,
}

impl Trace for PromiseCapability {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    tracer.trace_value(self.promise);
    tracer.trace_value(self.resolve);
    tracer.trace_value(self.reject);
  }
}

/// An ECMAScript PromiseReaction Record stored in a Promise's reaction lists.
///
/// Spec reference: <https://tc39.es/ecma262/#sec-promisereaction-records>
#[derive(Debug, Clone)]
pub struct PromiseReaction {
  /// `[[Capability]]` is either a PromiseCapability record or empty.
  pub capability: Option<PromiseCapability>,
  /// `[[Type]]` is either fulfill or reject.
  pub type_: PromiseReactionType,
  /// `[[Handler]]` is either a host-defined [`JobCallback`] record or empty.
  pub handler: Option<JobCallback>,
}

impl Trace for PromiseReaction {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    if let Some(cap) = &self.capability {
      cap.trace(tracer);
    }
    if let Some(handler) = &self.handler {
      handler.trace(tracer);
    }
  }
}

/// A spec-shaped Promise reaction record used by job scheduling scaffolding.
///
/// Mirrors ECMA-262's `PromiseReaction` record, but only includes the fields needed for job
/// creation at this scaffolding layer.
#[derive(Debug, Clone)]
pub struct PromiseReactionRecord {
  pub reaction_type: PromiseReactionType,
  /// The reaction handler stored as a host-defined [`JobCallback`] record (or empty).
  pub handler: Option<JobCallback>,
}

fn is_callable(heap: &Heap, value: Value) -> Result<Option<GcObject>, VmError> {
  let Value::Object(obj) = value else {
    return Ok(None);
  };

  if heap.is_callable(value)? {
    Ok(Some(obj))
  } else {
    Ok(None)
  }
}

/// Implements the "handler normalization" part of ECMA-262's `PerformPromiseThen`.
///
/// When `on_fulfilled` / `on_rejected` are callable, this captures them into host-defined
/// [`JobCallback`] records using [`VmHostHooks::host_make_job_callback`], per the
/// ECMA-262 + HTML
/// integration requirements.
pub fn normalize_promise_then_handlers(
  host: &mut dyn VmHostHooks,
  heap: &Heap,
  on_fulfilled: Value,
  on_rejected: Value,
) -> Result<(PromiseReactionRecord, PromiseReactionRecord), VmError> {
  let on_fulfilled = match is_callable(heap, on_fulfilled)? {
    Some(cb) => Some(host.host_make_job_callback(cb)?),
    None => None,
  };
  let on_rejected = match is_callable(heap, on_rejected)? {
    Some(cb) => Some(host.host_make_job_callback(cb)?),
    None => None,
  };

  Ok((
    PromiseReactionRecord {
      reaction_type: PromiseReactionType::Fulfill,
      handler: on_fulfilled,
    },
    PromiseReactionRecord {
      reaction_type: PromiseReactionType::Reject,
      handler: on_rejected,
    },
  ))
}

/// Creates a `PromiseResolveThenableJob` for a thenable resolution, capturing `then_action` as a
/// host-defined [`JobCallback`] record.
///
/// This corresponds to the part of ECMA-262's `CreateResolvingFunctions` that, when `then_action`
/// is callable, creates a `PromiseResolveThenableJob` and enqueues it.
pub fn create_promise_resolve_thenable_job(
  host: &mut dyn VmHostHooks,
  heap: &mut Heap,
  thenable: Value,
  then_action: Value,
  resolve: Value,
  reject: Value,
) -> Result<Option<Job>, VmError> {
  let Some(then_action) = is_callable(heap, then_action)? else {
    return Ok(None);
  };

  let then_job_callback = host.host_make_job_callback(then_action)?;
  Ok(Some(new_promise_resolve_thenable_job(
    heap,
    thenable,
    then_job_callback,
    resolve,
    reject,
  )?))
}

/// Minimal state for the engine-internal [`Promise`] record.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PromiseRecordState {
  Pending,
  Fulfilled(Value),
  Rejected(Value),
}

struct PromiseInner {
  handle: Option<PromiseHandle>,
  state: PromiseRecordState,
  is_handled: bool,
  fulfill_reactions: Vec<PromiseReactionRecord>,
  reject_reactions: Vec<PromiseReactionRecord>,
}

/// An engine-internal Promise record.
///
/// This is **not** a user-facing `Promise` object implementation; it exists to model Promise job
/// scheduling and rejection tracking for early async/await machinery.
#[derive(Clone)]
pub struct Promise {
  inner: Rc<RefCell<PromiseInner>>,
}

impl Promise {
  /// Create a new pending promise, optionally associated with a host-visible [`PromiseHandle`].
  pub fn pending(handle: Option<PromiseHandle>) -> Self {
    Self {
      inner: Rc::new(RefCell::new(PromiseInner {
        handle,
        state: PromiseRecordState::Pending,
        is_handled: false,
        fulfill_reactions: Vec::new(),
        reject_reactions: Vec::new(),
      })),
    }
  }

  /// Create a new already-fulfilled promise (used for `PromiseResolve` on non-promise values).
  fn fulfilled(value: Value) -> Self {
    Self {
      inner: Rc::new(RefCell::new(PromiseInner {
        handle: None,
        state: PromiseRecordState::Fulfilled(value),
        is_handled: true,
        fulfill_reactions: Vec::new(),
        reject_reactions: Vec::new(),
      })),
    }
  }

  /// Reject this promise with `reason`, enqueueing any rejection reactions as Promise jobs.
  ///
  /// If this promise has no rejection handlers, this will call
  /// [`VmHostHooks::host_promise_rejection_tracker`] with
  /// [`PromiseRejectionOperation::Reject`].
  pub fn reject(&self, host: &mut dyn VmHostHooks, heap: &mut Heap, reason: Value) -> Result<(), VmError> {
    let (handle, should_track_reject, reactions) = {
      let mut inner = self.inner.borrow_mut();
      match inner.state {
        PromiseRecordState::Pending => {}
        PromiseRecordState::Fulfilled(_) | PromiseRecordState::Rejected(_) => return Ok(()),
      }

      inner.state = PromiseRecordState::Rejected(reason);
      let handle = inner.handle;
      let should_track_reject = !inner.is_handled;
      let reactions = mem::take(&mut inner.reject_reactions);
      (handle, should_track_reject, reactions)
    };

    if should_track_reject {
      if let Some(handle) = handle {
        host.host_promise_rejection_tracker(handle, PromiseRejectionOperation::Reject);
      }
    }

    for reaction in reactions {
      let job = new_promise_reaction_job(heap, reaction, reason)?;
      let mut ctx = EnqueueCtx { heap: &mut *heap };
      host.host_enqueue_promise_job_fallible(&mut ctx, job, None)?;
    }

    Ok(())
  }

  fn then_without_result(
    &self,
    host: &mut dyn VmHostHooks,
    heap: &mut Heap,
    on_fulfilled: Value,
    on_rejected: Value,
  ) -> Result<(), VmError> {
    let (fulfill_reaction, reject_reaction) =
      normalize_promise_then_handlers(host, heap, on_fulfilled, on_rejected)?;

    // `[[PromiseIsHandled]]` bookkeeping for unhandled rejection tracking.
    let has_reject_handler = reject_reaction.handler.is_some();
    let mut inner = self.inner.borrow_mut();
    match inner.state {
      PromiseRecordState::Pending => {
        // `Vec::push` will abort the process on allocator OOM. Reactions can be attacker-controlled
        // (many `.then` chains attached to a pending promise), so use fallible reservation and
        // surface a recoverable `VmError::OutOfMemory`.
        //
        // Reserve for both reaction lists before mutating any Promise state, so we never partially
        // attach reactions.
        inner
          .fulfill_reactions
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
        inner
          .reject_reactions
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;

        if has_reject_handler && !inner.is_handled {
          inner.is_handled = true;
        }

        inner.fulfill_reactions.push(fulfill_reaction);
        inner.reject_reactions.push(reject_reaction);
      }
      PromiseRecordState::Fulfilled(v) => {
        let job = new_promise_reaction_job(heap, fulfill_reaction, v)?;
        if has_reject_handler && !inner.is_handled {
          inner.is_handled = true;
        }
        drop(inner);
        let mut ctx = EnqueueCtx { heap: &mut *heap };
        host.host_enqueue_promise_job_fallible(&mut ctx, job, None)?;
      }
      PromiseRecordState::Rejected(r) => {
        let job = new_promise_reaction_job(heap, reject_reaction, r)?;
        if has_reject_handler && !inner.is_handled {
          inner.is_handled = true;
          if let Some(handle) = inner.handle {
            host.host_promise_rejection_tracker(handle, PromiseRejectionOperation::Handle);
          }
        }
        drop(inner);
        let mut ctx = EnqueueCtx { heap: &mut *heap };
        host.host_enqueue_promise_job_fallible(&mut ctx, job, None)?;
      }
    }

    Ok(())
  }
}

#[cfg(all(test, unix))]
mod tests {
  use super::*;
  use crate::{HeapLimits, Job, RealmId};
  use std::io;
  use std::os::unix::process::CommandExt;
  use std::process::Command;
  use std::sync::Mutex;

  static OOM_TEST_LOCK: Mutex<()> = Mutex::new(());

  // Keep the child process's address space comfortably above the vm-js runtime overhead, while
  // still low enough that reaction-list growth reliably hits `VmError::OutOfMemory` rather than
  // aborting the process.
  const LIMIT_AS_BYTES: libc::rlim_t = 192 * 1024 * 1024;

  const CHILD_ENV: &str = "VMJS_INTERNAL_PROMISE_OOM_CHILD";
  const CHILD_TEST_NAME: &str = "promise::tests::internal_promise_reaction_list_oom_child";

  struct TestHost;

  impl VmHostHooks for TestHost {
    fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {
      // Not needed for this test: we only attach reactions to a pending internal Promise.
    }
  }

  #[test]
  fn internal_promise_reaction_lists_do_not_abort_on_oom() {
    // Don't recursively spawn when we're already in the constrained child process.
    if std::env::var_os(CHILD_ENV).is_some() {
      return;
    }

    // Avoid running multiple memory-pressure subprocesses in parallel (tests run in multiple
    // threads by default).
    let _guard = OOM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let exe = std::env::current_exe().expect("current_exe");
    let output = unsafe {
      let mut cmd = Command::new(exe);
      cmd.arg("--exact");
      cmd.arg(CHILD_TEST_NAME);
      cmd.env(CHILD_ENV, "1");

      cmd.pre_exec(|| {
        let lim = libc::rlimit {
          rlim_cur: LIMIT_AS_BYTES,
          rlim_max: LIMIT_AS_BYTES,
        };
        if libc::setrlimit(libc::RLIMIT_AS, &lim) != 0 {
          return Err(io::Error::last_os_error());
        }
        Ok(())
      });

      cmd.output().expect("spawn child test")
    };

    assert!(
      output.status.success(),
      "child OOM test failed: status={status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
      status = output.status,
      stdout = String::from_utf8_lossy(&output.stdout),
      stderr = String::from_utf8_lossy(&output.stderr),
    );
  }

  #[test]
  fn internal_promise_reaction_list_oom_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
      return;
    }

    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut host = TestHost;
    let promise = Promise::pending(None);

    let mut iters: usize = 0;
    loop {
      if iters > 50_000_000 {
        panic!("expected VmError::OutOfMemory from reaction list growth under RLIMIT_AS");
      }

      // Record current list lengths so we can assert we never partially attach reactions on OOM.
      let (len_fulfill, len_reject) = {
        let inner = promise.inner.borrow();
        (inner.fulfill_reactions.len(), inner.reject_reactions.len())
      };

      let result = promise.then_without_result(
        &mut host,
        &mut heap,
        Value::Undefined,
        Value::Undefined,
      );

      match result {
        Ok(()) => {
          iters += 1;
          continue;
        }
        Err(VmError::OutOfMemory) => {
          let inner = promise.inner.borrow();
          assert_eq!(
            inner.fulfill_reactions.len(),
            len_fulfill,
            "fulfill reactions should not be partially appended on OOM"
          );
          assert_eq!(
            inner.reject_reactions.len(),
            len_reject,
            "reject reactions should not be partially appended on OOM"
          );
          // The promise stays pending and can be safely observed after the failed `.then`.
          assert!(matches!(inner.state, PromiseRecordState::Pending));
          break;
        }
        Err(other) => panic!("unexpected error from then_without_result: {other:?}"),
      }
    }

    // Sanity check: we actually exercised some growth before failing.
    assert!(iters > 0, "expected to attach at least one reaction before OOM");
  }
}

/// A value that can be awaited.
#[derive(Clone)]
pub enum Awaitable {
  /// A non-Promise ECMAScript value.
  Value(Value),
  /// A Promise record.
  Promise(Promise),
}

impl From<Value> for Awaitable {
  fn from(value: Value) -> Self {
    Self::Value(value)
  }
}

impl From<Promise> for Awaitable {
  fn from(value: Promise) -> Self {
    Self::Promise(value)
  }
}

fn promise_resolve(value: Awaitable) -> Promise {
  match value {
    Awaitable::Promise(p) => p,
    Awaitable::Value(v) => Promise::fulfilled(v),
  }
}

/// Spec-shaped helper for async/await continuation scheduling.
///
/// Equivalent to `Await(value)` steps 2–4 + step 9:
/// 1. `promise = PromiseResolve(%Promise%, value)`
/// 2. `PerformPromiseThen(promise, on_fulfilled, on_rejected)` (no derived promise)
pub fn await_value(
  host: &mut dyn VmHostHooks,
  heap: &mut Heap,
  value: Awaitable,
  on_fulfilled: Value,
  on_rejected: Value,
) -> Result<(), VmError> {
  let promise = promise_resolve(value);
  promise.then_without_result(host, heap, on_fulfilled, on_rejected)
}
