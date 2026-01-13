//! ECMAScript jobs and host integration hooks.
//!
//! This module is intentionally **evaluator-independent**: it defines small, engine-owned types
//! that can exist before a full ECMAScript evaluator/interpreter is implemented.
//!
//! ## Spec background
//!
//! - **ECMA-262** defines *job abstract closures* (e.g. Promise jobs) and requires the host
//!   environment to schedule them via host-defined hooks:
//!   - [`HostEnqueuePromiseJob`](https://tc39.es/ecma262/#sec-hostenqueuepromisejob) (FIFO ordering)
//!   - [`HostPromiseRejectionTracker`](https://tc39.es/ecma262/#sec-host-promise-rejection-tracker)
//! - **HTML** defines how these hooks map onto the browser event loop:
//!   - [`HostEnqueuePromiseJob`](https://html.spec.whatwg.org/multipage/webappapis.html#hostenqueuepromisejob)
//!     queues a microtask which "prepares to run script", runs the job, cleans up, and reports
//!     exceptions.
//!   - Microtasks are processed at
//!     [microtask checkpoints](https://html.spec.whatwg.org/multipage/webappapis.html#perform-a-microtask-checkpoint).
//!   - HTML also defines
//!     [`HostMakeJobCallback`](https://html.spec.whatwg.org/multipage/webappapis.html#hostmakejobcallback) and
//!     [`HostCallJobCallback`](https://html.spec.whatwg.org/multipage/webappapis.html#hostcalljobcallback) for
//!     capturing and propagating the incumbent settings object / active script when scheduling and
//!     running callbacks.
//!
//! The main integration point is [`VmHostHooks::host_enqueue_promise_job`]. An embedding (e.g.
//! FastRender) can implement it by routing Promise jobs into the HTML microtask queue. The actual
//! queue is **host-owned**; this crate only provides the job representation.

use crate::heap::{Trace, Tracer};
use crate::fallible_alloc::{arc_try_new_vm, box_try_new_vm};
use crate::{
  GcObject, ImportMetaProperty, ModuleGraph, ModuleId, PropertyKey, RootId, Scope, Value, Vm, VmError,
};
use crate::{HostDefined, ModuleLoadPayload, ModuleReferrer, ModuleRequest};
use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;

/// Host-provided state passed to native call/construct handlers.
///
/// This is intentionally minimal and primarily exists so embeddings can thread arbitrary state
/// through VM-to-host call boundaries without using globals.
pub trait VmHost: Any {
  /// Returns `self` as [`Any`] for embedder-side downcasting.
  fn as_any(&self) -> &dyn Any;

  /// Returns `self` as [`Any`] for embedder-side downcasting.
  fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Any> VmHost for T {
  #[inline]
  fn as_any(&self) -> &dyn Any {
    self
  }

  #[inline]
  fn as_any_mut(&mut self) -> &mut dyn Any {
    self
  }
}

/// Opaque identifier for a Realm Record that a job should run in.
///
/// In ECMA-262, realms are described here:
/// <https://tc39.es/ecma262/#sec-code-realms>
///
/// In an HTML embedding, realms are typically associated with an
/// [environment settings object](https://html.spec.whatwg.org/multipage/webappapis.html#environment-settings-object).
///
/// This type is an *opaque token*: hosts should treat it as an identifier to store and pass back
/// to the VM/host hooks, not something to interpret.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct RealmId(u64);

impl RealmId {
  /// Create a new `RealmId` from an opaque numeric value.
  ///
  /// The numeric representation is intentionally unspecified; it may change.
  #[inline]
  pub const fn from_raw(raw: u64) -> Self {
    Self(raw)
  }

  /// Returns the underlying opaque numeric representation.
  #[inline]
  pub const fn to_raw(self) -> u64 {
    self.0
  }
}

impl fmt::Debug for RealmId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_tuple("RealmId").field(&self.0).finish()
  }
}

/// A coarse classification of host-scheduled work.
///
/// The host can use this to map work onto different event-loop queues (e.g. Promise jobs into the
/// microtask queue vs. timers into a task queue).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobKind {
  /// A Promise job (microtask in HTML).
  Promise,
  /// Generic work that does not have additional spec constraints.
  Generic,
  /// A timer callback (`setTimeout`/`setInterval`-like host tasks).
  Timeout,
  /// A cleanup job run for `FinalizationRegistry`.
  FinalizationRegistryCleanup,
}

/// The result of running an ECMAScript job.
///
/// If this returns an error, the embedding is expected to treat it similarly to an uncaught
/// exception during a microtask/task (e.g. report it).
pub type JobResult = Result<(), VmError>;

/// Dynamic context passed to jobs at execution time.
///
/// Promise jobs need to:
/// - call/construct JS values,
/// - keep captured GC handles alive while queued (persistent roots).
///
/// This trait is intentionally object-safe so hosts can store job runners behind trait objects.
pub trait VmJobContext {
  /// Calls `callee` with the provided `this` value and arguments.
  fn call(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError>;

  /// Constructs `callee` with the provided arguments and `new_target`.
  fn construct(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError>;

  /// Adds a persistent root, keeping `value` live until the returned [`RootId`] is removed.
  fn add_root(&mut self, value: Value) -> Result<RootId, VmError>;

  /// Removes a persistent root previously created by [`VmJobContext::add_root`].
  fn remove_root(&mut self, id: RootId);

  /// Coerces internal helper errors that represent spec throw completions (TypeError, NotCallable,
  /// etc.) into a thrown value with an attached stack trace when intrinsics are available.
  ///
  /// This is used by [`Job::run`] to ensure **host-visible job failures** surface as
  /// [`VmError::ThrowWithStack`] rather than leaking helper variants to the embedding.
  ///
  /// The default implementation is a no-op; job contexts that have access to a [`Vm`] + [`Heap`]
  /// should override it.
  #[inline]
  fn coerce_error_to_throw_with_stack(&mut self, err: VmError) -> VmError {
    err
  }
}

/// A spec-shaped representation of an ECMAScript *Job Abstract Closure*.
///
/// In ECMA-262, a "job" is a parameterless abstract closure that can be enqueued and later run by
/// the host (usually as part of a microtask checkpoint).
///
/// This representation is Rust-idiomatic: a job is a boxed `FnOnce` that receives a dynamic
/// [`VmJobContext`] and [`VmHostHooks`] so it can call back into the evaluator/embedding at run
/// time.
///
/// # GC safety
///
/// Promise jobs can be queued across allocations/GC. Any GC-managed [`Value`] captured by a job
/// MUST be kept alive until the job runs.
///
/// The engine-supported pattern is:
/// - create persistent roots at enqueue time (via [`VmJobContext::add_root`]),
/// - record the returned [`RootId`]s on the job,
/// - and let [`Job::run`] / [`Job::discard`] automatically remove them.
pub struct Job {
  kind: JobKind,
  roots: Vec<RootId>,
  run: Option<Box<dyn FnOnce(&mut dyn VmJobContext, &mut dyn VmHostHooks) -> JobResult + Send + 'static>>,
}

impl Job {
  /// Create a new job of `kind` backed by `run`.
  pub fn new(
    kind: JobKind,
    run: impl FnOnce(&mut dyn VmJobContext, &mut dyn VmHostHooks) -> JobResult + Send + 'static,
  ) -> Result<Self, VmError> {
    // `Box::new` aborts the process on allocator OOM; use a fallible allocator so this error can
    // propagate as `VmError::OutOfMemory`.
    let run = box_try_new_vm(run)?;
    // Coerce `Box<F>` into a trait object without allocating.
    let run: Box<
      dyn FnOnce(&mut dyn VmJobContext, &mut dyn VmHostHooks) -> JobResult + Send + 'static,
    > = run;
    Ok(Self {
      kind,
      roots: Vec::new(),
      run: Some(run),
    })
  }

  /// Adds a persistent root that will be automatically removed when the job is run or discarded.
  pub fn add_root(&mut self, ctx: &mut dyn VmJobContext, value: Value) -> Result<RootId, VmError> {
    let id = ctx.add_root(value)?;
    if self.roots.try_reserve_exact(1).is_err() {
      ctx.remove_root(id);
      return Err(VmError::OutOfMemory);
    }
    self.roots.push(id);
    Ok(id)
  }

  /// Adds multiple persistent roots, keeping all `values` live until the job is run or discarded.
  ///
  /// If root registration fails, any roots created by this call are removed and the job is left
  /// unchanged.
  ///
  /// ## GC safety note
  ///
  /// Like [`Job::add_root`], this only roots each value *once it is registered*. If allocating one
  /// root triggers a GC cycle, values that have not yet been registered must still be reachable via
  /// other roots (for example: stack roots, heap reachability, or embedder-owned persistent roots).
  pub fn add_roots(
    &mut self,
    ctx: &mut dyn VmJobContext,
    values: &[Value],
  ) -> Result<(), VmError> {
    if values.is_empty() {
      return Ok(());
    }

    // Pre-reserve to avoid partial updates under OOM while pushing into `self.roots`.
    self
      .roots
      .try_reserve_exact(values.len())
      .map_err(|_| VmError::OutOfMemory)?;

    let start_len = self.roots.len();
    for &value in values {
      match ctx.add_root(value) {
        Ok(id) => self.roots.push(id),
        Err(err) => {
          // Roll back roots created by this call so callers can safely drop the job on error.
          for id in self.roots.drain(start_len..) {
            ctx.remove_root(id);
          }
          return Err(err);
        }
      }
    }
    Ok(())
  }

  /// Records an existing persistent root so it will be automatically removed when the job is run
  /// or discarded.
  pub fn try_push_root(&mut self, id: RootId) -> Result<(), VmError> {
    self
      .roots
      .try_reserve_exact(1)
      .map_err(|_| VmError::OutOfMemory)?;
    // `try_reserve_exact(1)` guarantees `push` won't grow/reallocate the buffer.
    self.roots.push(id);
    Ok(())
  }

  /// Adds multiple existing persistent roots.
  pub fn try_extend_roots(&mut self, ids: impl IntoIterator<Item = RootId>) -> Result<(), VmError> {
    let iter = ids.into_iter();
    let (lower, upper) = iter.size_hint();
    // Prefer reserving the exact number of elements up-front when the iterator provides an upper
    // bound. This avoids partial updates under OOM and reduces the chance of failing due to
    // over-reserving (e.g. `try_reserve`'s exponential growth strategy).
    if let Some(upper) = upper {
      self
        .roots
        .try_reserve_exact(upper)
        .map_err(|_| VmError::OutOfMemory)?;
      // `try_reserve_exact(upper)` guarantees `extend` won't grow/reallocate the buffer because the
      // iterator must yield no more than `upper` items.
      self.roots.extend(iter);
      return Ok(());
    }

    // Fallback for iterators with an unknown upper bound: extend one element at a time using a
    // fallible reserve so allocator OOM does not abort the process.
    //
    // We maintain an all-or-nothing guarantee: on allocation failure, `roots` is truncated back to
    // its original length so callers can safely clean up the provided roots if desired.
    let original_len = self.roots.len();
    if lower != 0 {
      // Pre-reserve the lower bound as a best-effort optimisation; failure here indicates OOM.
      self
        .roots
        .try_reserve_exact(lower)
        .map_err(|_| VmError::OutOfMemory)?;
    }
    for id in iter {
      if let Err(e) = self.try_push_root(id) {
        self.roots.truncate(original_len);
        return Err(e);
      }
    }
    Ok(())
  }

  /// Replaces the job's root list (useful when capturing roots at enqueue time).
  pub fn with_roots(mut self, roots: Vec<RootId>) -> Self {
    self.roots = roots;
    self
  }

  /// Returns this job's kind.
  #[inline]
  pub fn kind(&self) -> JobKind {
    self.kind
  }

  fn cleanup_roots(&mut self, ctx: &mut dyn VmJobContext) {
    for root in self.roots.drain(..) {
      ctx.remove_root(root);
    }
  }

  /// Run the job, consuming it.
  #[inline]
  pub fn run(mut self, ctx: &mut dyn VmJobContext, host: &mut dyn VmHostHooks) -> JobResult {
    let Some(run) = self.run.take() else {
      return Err(VmError::Unimplemented("job already consumed"));
    };

    // Ensure roots are cleaned up even if the job panics.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(ctx, host)));
    self.cleanup_roots(ctx);

    match result {
      Ok(result) => match result {
        Err(err) if err.is_throw_completion() => Err(ctx.coerce_error_to_throw_with_stack(err)),
        other => other,
      },
      Err(_) => Err(VmError::InvariantViolation("job closure panicked")),
    }
  }

  /// Discards the job without running it, cleaning up any persistent roots it owns.
  pub fn discard(mut self, ctx: &mut dyn VmJobContext) {
    self.run = None;
    self.cleanup_roots(ctx);
  }
}

impl fmt::Debug for Job {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Job")
      .field("kind", &self.kind)
      .field("roots", &self.roots.len())
      .finish()
  }
}

impl Drop for Job {
  fn drop(&mut self) {
    // Avoid panicking from a destructor while unwinding (that would abort).
    if std::thread::panicking() {
      return;
    }
    debug_assert!(
      self.roots.is_empty(),
      "Job dropped with {} leaked persistent roots; call Job::run(..) or Job::discard(..)",
      self.roots.len()
    );
  }
}

/// A host-defined *JobCallback* record.
///
/// HTML uses JobCallback records to capture the "incumbent settings object" / active script state
/// at the moment a callback is created, and later re-establish that state when calling it.
///
/// In this crate the record is mostly opaque: the host may associate arbitrary data with the
/// callback, but the callback object itself is stored explicitly so engine code can keep it alive
/// across GC cycles.
///
/// # GC safety / rooting (important!)
///
/// A [`JobCallback`] is **host-owned data**. Like any host-owned structure, it is not
/// automatically visited by the GC: the GC only traces objects that are reachable from the heap
/// graph and from explicit roots.
///
/// `JobCallback` implements [`Trace`] by tracing the callback object, so an embedding can keep the
/// callback alive by storing `JobCallback` inside a traced structure. However, simply holding a
/// `JobCallback` record in host state (queued tasks/microtasks, timers, etc.) does **not** keep the
/// callback alive.
///
/// If a callback must stay alive until some future host work runs, the embedding MUST keep
/// [`JobCallback::callback`] alive by registering it as a persistent root (for example via
/// [`VmJobContext::add_root`] / [`VmJobContext::remove_root`], typically attached to the queued
/// [`Job`], or via [`crate::Heap::add_root`] / [`crate::Heap::remove_root`]).
///
/// The host-defined payload is **opaque** to the GC. Hosts MUST NOT store GC handles inside the
/// payload unless they keep them alive independently.
///
/// ## Recommended pattern
///
/// When enqueuing a job that will later observe/call a callback, register the callback object as a
/// persistent root for the lifetime of the queued job:
///
/// ```no_run
/// # use vm_js::{GcObject, Job, JobKind, JobResult, JobCallback, RealmId, RootId, Value, VmError, VmHostHooks, VmJobContext};
/// # fn main() -> Result<(), VmError> {
/// # struct Host;
/// # impl VmHostHooks for Host {
/// #   fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
/// # }
/// # struct Ctx;
/// # impl VmJobContext for Ctx {
/// #   fn call(&mut self, _host: &mut dyn VmHostHooks, _callee: Value, _this: Value, _args: &[Value]) -> Result<Value, VmError> { unimplemented!() }
/// #   fn construct(&mut self, _host: &mut dyn VmHostHooks, _callee: Value, _args: &[Value], _new_target: Value) -> Result<Value, VmError> { unimplemented!() }
/// #   fn add_root(&mut self, _value: Value) -> Result<RootId, VmError> { unimplemented!() }
/// #   fn remove_root(&mut self, _id: RootId) { unimplemented!() }
/// # }
/// # let mut host = Host;
/// # let mut ctx = Ctx;
/// # let callback_obj: GcObject = todo!();
/// let job_callback: JobCallback = host.host_make_job_callback(callback_obj)?;
///
/// let mut job = Job::new(JobKind::Generic, move |_ctx, _host| -> JobResult {
///   // Later: _host.host_call_job_callback(_ctx, &job_callback, ...)?;
///   let _ = job_callback.callback();
///   Ok(())
/// })?;
///
/// // IMPORTANT: keep `callback_obj` alive until the queued job runs.
/// job.add_root(&mut ctx, Value::Object(callback_obj))?;
/// host.host_enqueue_promise_job(job, None);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct JobCallback(Arc<JobCallbackInner>);

struct JobCallbackInner {
  callback: GcObject,
  realm: Option<RealmId>,
  host_defined: Option<Arc<dyn Any + Send + Sync>>,
  rooted: Mutex<Option<RootId>>,
}

impl JobCallback {
  /// Fallible constructor for a `JobCallback` with no extra host-defined metadata.
  pub fn try_new(callback: GcObject) -> Result<Self, VmError> {
    Self::try_new_in_realm(callback, None)
  }

  /// Fallible constructor for a `JobCallback` associated with an opaque realm identifier.
  pub fn try_new_in_realm(callback: GcObject, realm: Option<RealmId>) -> Result<Self, VmError> {
    Ok(Self(arc_try_new_vm(JobCallbackInner {
      callback,
      realm,
      host_defined: None,
      rooted: Mutex::new(None),
    })?))
  }

  /// Fallible constructor for a `JobCallback` with host-defined metadata.
  pub fn try_new_with_data<T: Any + Send + Sync>(callback: GcObject, data: T) -> Result<Self, VmError> {
    Self::try_new_with_data_in_realm(callback, data, None)
  }

  /// Fallible constructor for a `JobCallback` with host-defined metadata, associated with an
  /// opaque realm identifier.
  pub fn try_new_with_data_in_realm<T: Any + Send + Sync>(
    callback: GcObject,
    data: T,
    realm: Option<RealmId>,
  ) -> Result<Self, VmError> {
    let host_defined: Arc<dyn Any + Send + Sync> = arc_try_new_vm(data)?;
    Ok(Self(arc_try_new_vm(JobCallbackInner {
      callback,
      realm,
      host_defined: Some(host_defined),
      rooted: Mutex::new(None),
    })?))
  }

  /// Create a new `JobCallback` with no extra host-defined metadata.
  ///
  /// This constructor is fallible: it returns [`VmError::OutOfMemory`] instead of panicking if the
  /// underlying `Arc` allocation fails.
  pub fn new(callback: GcObject) -> Result<Self, VmError> {
    Self::try_new_in_realm(callback, None)
  }

  /// Create a new `JobCallback` associated with an opaque realm identifier.
  ///
  /// This constructor is fallible: it returns [`VmError::OutOfMemory`] instead of panicking if the
  /// underlying `Arc` allocation fails.
  pub fn new_in_realm(callback: GcObject, realm: Option<RealmId>) -> Result<Self, VmError> {
    Self::try_new_in_realm(callback, realm)
  }

  /// Create a new `JobCallback` with host-defined metadata.
  ///
  /// This constructor is fallible: it returns [`VmError::OutOfMemory`] instead of panicking if the
  /// underlying `Arc` allocation fails.
  pub fn new_with_data<T: Any + Send + Sync>(callback: GcObject, data: T) -> Result<Self, VmError> {
    Self::try_new_with_data_in_realm(callback, data, None)
  }

  /// Create a new `JobCallback` with host-defined metadata, associated with an opaque realm
  /// identifier.
  ///
  /// This constructor is fallible: it returns [`VmError::OutOfMemory`] instead of panicking if the
  /// underlying `Arc` allocation fails.
  pub fn new_with_data_in_realm<T: Any + Send + Sync>(
    callback: GcObject,
    data: T,
    realm: Option<RealmId>,
  ) -> Result<Self, VmError> {
    Self::try_new_with_data_in_realm(callback, data, realm)
  }

  /// Returns the callback object captured by this record.
  #[inline]
  pub fn callback(&self) -> GcObject {
    self.0.callback
  }

  /// Alias for [`JobCallback::callback`].
  #[inline]
  pub fn callback_object(&self) -> GcObject {
    self.0.callback
  }

  /// Opaque realm identifier captured at creation time, if any.
  #[inline]
  pub fn realm(&self) -> Option<RealmId> {
    self.0.realm
  }

  /// Ensures the callback object is kept alive by registering it as a persistent root.
  ///
  /// This is intended for embeddings that store [`JobCallback`] records in host-owned task queues
  /// (timers, tasks, microtasks). The GC does not trace host memory; without an explicit root, the
  /// callback object can be collected and the record will hold a stale handle.
  ///
  /// This method is **idempotent**: if the callback is already rooted, it returns the existing
  /// root id.
  ///
  /// Call [`JobCallback::teardown`] to unregister the root when the callback is no longer needed.
  pub fn ensure_rooted(&self, ctx: &mut dyn VmJobContext) -> Result<RootId, VmError> {
    let mut guard = match self.0.rooted.lock() {
      Ok(guard) => guard,
      // If a panic occurred while holding the lock, treat it as poisoned but still recover the
      // internal state. This avoids cascading panics from JS/host-triggered failures.
      Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(id) = *guard {
      return Ok(id);
    }
    let id = ctx.add_root(Value::Object(self.callback()))?;
    *guard = Some(id);
    Ok(id)
  }

  /// Returns the persistent-root id for this callback, if it has been rooted via
  /// [`JobCallback::ensure_rooted`].
  pub fn root_id(&self) -> Option<RootId> {
    match self.0.rooted.lock() {
      Ok(guard) => *guard,
      Err(poisoned) => *poisoned.into_inner(),
    }
  }

  /// Unregisters the persistent root created by [`JobCallback::ensure_rooted`], if any.
  ///
  /// This method is **idempotent**.
  pub fn teardown(&self, ctx: &mut dyn VmJobContext) {
    let id = match self.0.rooted.lock() {
      Ok(mut guard) => guard.take(),
      Err(mut poisoned) => poisoned.get_mut().take(),
    };
    if let Some(id) = id {
      ctx.remove_root(id);
    }
  }

  /// Alias for [`JobCallback::teardown`].
  #[inline]
  pub fn remove_roots(&self, ctx: &mut dyn VmJobContext) {
    self.teardown(ctx);
  }

  /// Attempts to downcast the host-defined metadata payload by reference.
  pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
    self.0.host_defined.as_ref()?.downcast_ref::<T>()
  }
}

impl Drop for JobCallbackInner {
  fn drop(&mut self) {
    // Avoid panicking from a destructor while unwinding (that would abort).
    if std::thread::panicking() {
      return;
    }
    // We cannot automatically remove the root without access to the heap/context; require explicit
    // teardown in debug builds.
    if let Ok(rooted) = self.rooted.get_mut() {
      debug_assert!(
        rooted.is_none(),
        "JobCallback dropped with a leaked persistent root; call JobCallback::teardown(..)"
      );
    }
  }
}

impl fmt::Debug for JobCallback {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("JobCallback")
      .field("callback", &self.0.callback)
      .field("realm", &self.0.realm)
      .field(
        "host_defined_type_id",
        &self.0.host_defined.as_ref().map(|v| v.type_id()),
      )
      .field("rooted", &self.root_id().is_some())
      .finish()
  }
}

impl Trace for JobCallback {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    tracer.trace_value(Value::Object(self.0.callback));
  }
}

/// Opaque handle to a promise object passed to [`VmHostHooks::host_promise_rejection_tracker`].
///
/// At this layer, promises are represented as ordinary JavaScript objects (in HTML, they are
/// surfaced as an `object` on `PromiseRejectionEvent`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct PromiseHandle(pub GcObject);

impl From<GcObject> for PromiseHandle {
  fn from(value: GcObject) -> Self {
    Self(value)
  }
}

impl From<PromiseHandle> for GcObject {
  fn from(value: PromiseHandle) -> Self {
    value.0
  }
}

/// The operation passed to [`VmHostHooks::host_promise_rejection_tracker`].
///
/// Mirrors the `operation` string argument in the ECMAScript spec.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PromiseRejectionOperation {
  /// A promise became rejected while having no rejection handlers.
  Reject,
  /// A rejection handler was added to a previously-unhandled rejected promise.
  Handle,
}

/// Host hooks required by the ECMAScript specification (and refined by HTML for browsers).
///
/// The VM/evaluator calls into this trait; the embedding provides the implementation.
///
/// ## FIFO requirement
///
/// ECMA-262 requires Promise jobs to be processed in FIFO order for an agent:
/// <https://tc39.es/ecma262/#sec-hostenqueuepromisejob>.
///
/// The VM will call [`VmHostHooks::host_enqueue_promise_job`] in the spec-required order; hosts
/// MUST preserve this ordering when running the queued jobs.
pub trait VmHostHooks {
  /// Enqueue a Promise job.
  ///
  /// ## ECMA-262
  ///
  /// This corresponds to
  /// [`HostEnqueuePromiseJob(job, realm)`](https://tc39.es/ecma262/#sec-hostenqueuepromisejob).
  ///
  /// ## HTML embedding
  ///
  /// HTML defines this hook by
  /// [queueing a microtask](https://html.spec.whatwg.org/multipage/webappapis.html#queue-a-microtask)
  /// that:
  ///
  /// 1. (If `realm` is not `None`) [prepares to run script](https://html.spec.whatwg.org/multipage/webappapis.html#prepare-to-run-script),
  /// 2. runs `job`,
  /// 3. [cleans up after running script](https://html.spec.whatwg.org/multipage/webappapis.html#clean-up-after-running-script),
  /// 4. and [reports exceptions](https://html.spec.whatwg.org/multipage/webappapis.html#report-the-exception).
  ///
  /// Microtasks are processed at
  /// [microtask checkpoints](https://html.spec.whatwg.org/multipage/webappapis.html#perform-a-microtask-checkpoint).
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>);

  /// Fallible variant of [`VmHostHooks::host_enqueue_promise_job`].
  ///
  /// # OOM safety / rooting
  ///
  /// Enqueueing a Promise job is allowed to allocate (e.g. growing a host-side queue). Under
  /// allocator OOM, hosts should return [`VmError::OutOfMemory`] rather than aborting the process.
  ///
  /// The provided [`VmJobContext`] allows the host to discard `job` on enqueue failure via
  /// [`Job::discard`], ensuring any persistent roots owned by the job are unregistered.
  ///
  /// The default implementation delegates to the infallible legacy hook and returns `Ok(())`.
  #[inline]
  fn host_enqueue_promise_job_fallible(
    &mut self,
    _ctx: &mut dyn VmJobContext,
    job: Job,
    realm: Option<RealmId>,
  ) -> Result<(), VmError> {
    self.host_enqueue_promise_job(job, realm);
    Ok(())
  }

  /// Optional host hook for `Math.random()`.
  ///
  /// If provided, this hook supplies raw pseudorandom bits that will be converted into a
  /// `Number` in the range `[0, 1)`, using the same 53-bit conversion as the engine's default PRNG.
  ///
  /// Returning `None` falls back to the VM's deterministic per-VM PRNG (seeded by
  /// [`crate::VmOptions::math_random_seed`]).
  #[inline]
  fn host_math_random_u64(&mut self) -> Option<u64> {
    None
  }

  /// Returns the current time in milliseconds since the Unix epoch.
  ///
  /// This is used by the `Date` built-in (`new Date()`, `Date.now()`).
  ///
  /// The default implementation uses [`std::time::SystemTime`]. Hosts that need deterministic
  /// execution (e.g. test runners) can override this to return a controlled time source.
  #[inline]
  fn host_current_time_millis(&mut self) -> f64 {
    let now = std::time::SystemTime::now();
    match now.duration_since(std::time::UNIX_EPOCH) {
      Ok(dur) => dur.as_secs_f64() * 1000.0,
      Err(err) => -(err.duration().as_secs_f64() * 1000.0),
    }
  }

  /// Host hook for "exotic" `[[Get]]` behavior.
  ///
  /// This allows embeddings to model lightweight host objects (e.g. DOMStringMap-style named
  /// properties) without implementing full ECMAScript Proxy semantics.
  ///
  /// The VM calls this hook **after** an own-property lookup miss and **before** walking the
  /// prototype chain.
  ///
  /// - Return `Ok(Some(value))` to treat the property as resolved to `value`.
  /// - Return `Ok(None)` to fall back to ordinary property lookup semantics.
  #[inline]
  fn host_exotic_get(
    &mut self,
    _scope: &mut Scope<'_>,
    _obj: GcObject,
    _key: PropertyKey,
    _receiver: Value,
  ) -> Result<Option<Value>, VmError> {
    Ok(None)
  }

  /// Host hook for "exotic" `[[Set]]` behavior.
  ///
  /// The VM calls this hook **before** ordinary `[[Set]]` processing so the host can override
  /// prototype-chain properties (mirroring WebIDL's legacy platform object named property setter
  /// behaviour).
  ///
  /// - Return `Ok(Some(true/false))` to treat the set as handled and return that boolean result.
  /// - Return `Ok(None)` to fall back to ordinary `[[Set]]` semantics.
  #[inline]
  fn host_exotic_set(
    &mut self,
    _scope: &mut Scope<'_>,
    _obj: GcObject,
    _key: PropertyKey,
    _value: Value,
    _receiver: Value,
  ) -> Result<Option<bool>, VmError> {
    Ok(None)
  }

  /// Host hook for "exotic" `[[Delete]]` behavior.
  ///
  /// - Return `Ok(Some(true/false))` to treat the deletion as handled and return that boolean
  ///   result.
  /// - Return `Ok(None)` to fall back to ordinary `[[Delete]]` semantics.
  #[inline]
  fn host_exotic_delete(
    &mut self,
    _scope: &mut Scope<'_>,
    _obj: GcObject,
    _key: PropertyKey,
  ) -> Result<Option<bool>, VmError> {
    Ok(None)
  }

  /// Returns embedder-defined state for downcasting.
  ///
  /// ## Why this exists
  ///
  /// Native call/construct handlers receive both:
  /// - `host: &mut dyn VmHost`, and
  /// - `hooks: &mut dyn VmHostHooks`.
  ///
  /// Some `vm-js` entrypoints pass a dummy [`VmHost`] (commonly `()`) when no per-call host context
  /// is needed. In those cases, embeddings (and generated bindings) that need access to embedder
  /// state should downcast via `hooks.as_any_mut()` instead of relying on `host`.
  ///
  /// The default implementation returns `None`. Embeddings that require downcasting must override
  /// this and return a `&mut dyn Any` that is valid for the duration of the native call.
  #[inline]
  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    None
  }

  /// Creates a host-defined [`JobCallback`] record.
  ///
  /// Stub hook for HTML's `HostMakeJobCallback`:
  /// <https://html.spec.whatwg.org/multipage/webappapis.html#hostmakejobcallback>.
  ///
  /// Embeddings that do not need incumbent/active-script propagation can use the default
  /// implementation, which stores the callback object with no extra host-defined metadata.
  ///
  /// ## GC safety (important!)
  ///
  /// The default implementation stores `callback` as a raw [`GcObject`] inside a [`JobCallback`]
  /// record, but does not register it as a persistent root.
  ///
  /// If the callback object must stay alive until some future task/microtask runs, the embedding
  /// MUST keep it alive itself (for example by rooting it as part of the queued [`Job`] via
  /// [`Job::add_root`], or by using [`crate::Heap::add_root`]).
  ///
  /// ## Fallibility
  ///
  /// This hook is fallible so OOM during callback wrapping can propagate as
  /// [`VmError::OutOfMemory`] instead of panicking or aborting the process.
  fn host_make_job_callback(&mut self, callback: GcObject) -> Result<JobCallback, VmError> {
    JobCallback::try_new(callback)
  }

  /// Deprecated alias for [`VmHostHooks::host_make_job_callback`].
  #[deprecated(note = "use VmHostHooks::host_make_job_callback")]
  fn host_make_job_callback_fallible(&mut self, callback: GcObject) -> Result<JobCallback, VmError> {
    self.host_make_job_callback(callback)
  }

  /// Calls a host-defined [`JobCallback`] record.
  ///
  /// Stub hook for HTML's `HostCallJobCallback`:
  /// <https://html.spec.whatwg.org/multipage/webappapis.html#hostcalljobcallback>.
  ///
  /// The default implementation delegates to [`VmJobContext::call`], passing:
  /// - `callee`: `callback.[[Callback]]`
  /// - `this`: `this_argument`
  /// - `args`: `arguments`
  fn host_call_job_callback(
    &mut self,
    ctx: &mut dyn VmJobContext,
    callback: &JobCallback,
    this_argument: Value,
    arguments: &[Value],
  ) -> Result<Value, VmError> {
    // `VmJobContext::call` expects a `&mut dyn VmHostHooks`. In a trait default method, `Self` may
    // be `dyn VmHostHooks` (unsized), so we can't directly pass `self` as a trait object without a
    // `Self: Sized` bound (which would break object safety).
    //
    // Wrap `self` in a sized proxy that forwards all host hooks.
    struct HostProxy<'a, H: VmHostHooks + ?Sized>(&'a mut H);

    impl<H: VmHostHooks + ?Sized> VmHostHooks for HostProxy<'_, H> {
      fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
        self.0.host_enqueue_promise_job(job, realm);
      }

      fn host_enqueue_promise_job_fallible(
        &mut self,
        ctx: &mut dyn VmJobContext,
        job: Job,
        realm: Option<RealmId>,
      ) -> Result<(), VmError> {
        self.0.host_enqueue_promise_job_fallible(ctx, job, realm)
      }

      fn host_math_random_u64(&mut self) -> Option<u64> {
        self.0.host_math_random_u64()
      }

      fn host_current_time_millis(&mut self) -> f64 {
        self.0.host_current_time_millis()
      }

      fn host_exotic_get(
        &mut self,
        scope: &mut Scope<'_>,
        obj: GcObject,
        key: PropertyKey,
        receiver: Value,
      ) -> Result<Option<Value>, VmError> {
        self.0.host_exotic_get(scope, obj, key, receiver)
      }

      fn host_exotic_set(
        &mut self,
        scope: &mut Scope<'_>,
        obj: GcObject,
        key: PropertyKey,
        value: Value,
        receiver: Value,
      ) -> Result<Option<bool>, VmError> {
        self.0.host_exotic_set(scope, obj, key, value, receiver)
      }

      fn host_exotic_delete(
        &mut self,
        scope: &mut Scope<'_>,
        obj: GcObject,
        key: PropertyKey,
      ) -> Result<Option<bool>, VmError> {
        self.0.host_exotic_delete(scope, obj, key)
      }

      fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        self.0.as_any_mut()
      }

      fn host_make_job_callback(&mut self, callback: GcObject) -> Result<JobCallback, VmError> {
        self.0.host_make_job_callback(callback)
      }

      #[allow(deprecated)]
      fn host_make_job_callback_fallible(&mut self, callback: GcObject) -> Result<JobCallback, VmError> {
        self.0.host_make_job_callback_fallible(callback)
      }

      fn host_call_job_callback(
        &mut self,
        ctx: &mut dyn VmJobContext,
        callback: &JobCallback,
        this_argument: Value,
        arguments: &[Value],
      ) -> Result<Value, VmError> {
        self
          .0
          .host_call_job_callback(ctx, callback, this_argument, arguments)
      }

      fn host_promise_rejection_tracker(
        &mut self,
        promise: PromiseHandle,
        operation: PromiseRejectionOperation,
      ) {
        self.0.host_promise_rejection_tracker(promise, operation);
      }

      fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
        self.0.host_get_supported_import_attributes()
      }

      fn host_get_import_meta_properties(
        &mut self,
        vm: &mut Vm,
        scope: &mut Scope<'_>,
        module: ModuleId,
      ) -> Result<Vec<ImportMetaProperty>, VmError> {
        self.0.host_get_import_meta_properties(vm, scope, module)
      }

      fn host_finalize_import_meta(
        &mut self,
        vm: &mut Vm,
        scope: &mut Scope<'_>,
        import_meta: GcObject,
        module: ModuleId,
      ) -> Result<(), VmError> {
        self.0.host_finalize_import_meta(vm, scope, import_meta, module)
      }

      fn host_load_imported_module(
        &mut self,
        vm: &mut Vm,
        scope: &mut Scope<'_>,
        modules: &mut ModuleGraph,
        referrer: ModuleReferrer,
        module_request: ModuleRequest,
        host_defined: HostDefined,
        payload: ModuleLoadPayload,
      ) -> Result<(), VmError> {
        self
          .0
          .host_load_imported_module(vm, scope, modules, referrer, module_request, host_defined, payload)
      }
    }

    let mut proxy = HostProxy(self);
    ctx.call(
      &mut proxy,
      Value::Object(callback.callback_object()),
      this_argument,
      arguments,
    )
  }

  /// Promise rejection tracker hook (unhandled rejection reporting).
  ///
  /// Stub hook for ECMA-262's `HostPromiseRejectionTracker`:
  /// <https://tc39.es/ecma262/#sec-host-promise-rejection-tracker>.
  ///
  /// HTML's host implementation uses:
  /// - an "about-to-be-notified rejected promises list", and
  /// - an "outstanding rejected promises weak set"
  ///
  /// to later report `unhandledrejection`/`rejectionhandled` events at microtask checkpoints. See:
  /// <https://html.spec.whatwg.org/multipage/webappapis.html#the-hostpromiserejectiontracker-implementation>.
  ///
  /// This default implementation does nothing.
  fn host_promise_rejection_tracker(
    &mut self,
    _promise: PromiseHandle,
    _operation: PromiseRejectionOperation,
  ) {
  }

  /// Returns the list of import attribute keys supported by this host.
  ///
  /// This corresponds to ECMA-262's `HostGetSupportedImportAttributes()`:
  /// <https://tc39.es/ecma262/#sec-hostgetsupportedimportattributes>.
  ///
  /// The default implementation returns an empty list (no attributes supported).
  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    &[]
  }

  /// Returns the list of initial properties to define on the `import.meta` object for `module`.
  ///
  /// Spec reference: `HostGetImportMetaProperties`:
  /// <https://tc39.es/ecma262/#sec-hostgetimportmetaproperties>.
  fn host_get_import_meta_properties(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _module: ModuleId,
  ) -> Result<Vec<ImportMetaProperty>, VmError> {
    Ok(Vec::new())
  }

  /// Gives the host a chance to finalize the `import.meta` object after initial properties have
  /// been defined.
  ///
  /// Spec reference: `HostFinalizeImportMeta`:
  /// <https://tc39.es/ecma262/#sec-hostfinalizeimportmeta>.
  fn host_finalize_import_meta(
    &mut self,
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _import_meta: GcObject,
    _module: ModuleId,
  ) -> Result<(), VmError> {
    Ok(())
  }

  /// Load an imported module (host hook).
  ///
  /// This corresponds to ECMA-262's
  /// [`HostLoadImportedModule(referrer, moduleRequest, hostDefined, payload)`](https://tc39.es/ecma262/#sec-HostLoadImportedModule).
  ///
  /// The host environment must perform
  /// `FinishLoadingImportedModule(referrer, moduleRequest, payload, result)` by calling
  /// [`Vm::finish_loading_imported_module`], either synchronously (re-entrantly) or asynchronously.
  ///
  /// ## Re-entrancy
  ///
  /// The host may call `FinishLoadingImportedModule` synchronously from inside this hook. That
  /// re-enters module graph loading (spec `ContinueModuleLoading`) and may cause nested
  /// `host_load_imported_module` calls.
  ///
  /// ## Caching requirement (ECMA-262)
  ///
  /// If this operation is called multiple times with the same `(referrer, moduleRequest)` pair (as
  /// determined by `ModuleRequestsEqual` / [`crate::module_requests_equal`]) and it completes
  /// normally (i.e. `FinishLoadingImportedModule` is called with `Ok(module)`), then it must
  /// complete with the **same Module Record** each time.
  ///
  /// The `payload` argument is an opaque token owned by the engine; the host must not inspect it.
  ///
  /// For an end-to-end embedder guide (static graph loading, dynamic `import()`, top-level `await`,
  /// and `vm.module_graph_ptr` lifetime), see [`crate::docs::modules`].
  fn host_load_imported_module(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    referrer: ModuleReferrer,
    module_request: ModuleRequest,
    host_defined: HostDefined,
    payload: ModuleLoadPayload,
  ) -> Result<(), VmError> {
    let _ = host_defined;

    // `finish_loading_imported_module` expects a `&mut dyn VmHostHooks`. In a trait default method,
    // `Self` may be `dyn VmHostHooks` (unsized), so we can't directly pass `self` as a trait object
    // without a `Self: Sized` bound (which would break object safety).
    //
    // Wrap `self` in a sized proxy that forwards all host hooks.
    struct HostProxy<'a, H: VmHostHooks + ?Sized>(&'a mut H);
    impl<H: VmHostHooks + ?Sized> VmHostHooks for HostProxy<'_, H> {
      fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
        self.0.host_enqueue_promise_job(job, realm);
      }

      fn host_enqueue_promise_job_fallible(
        &mut self,
        ctx: &mut dyn VmJobContext,
        job: Job,
        realm: Option<RealmId>,
      ) -> Result<(), VmError> {
        self.0.host_enqueue_promise_job_fallible(ctx, job, realm)
      }

      fn host_math_random_u64(&mut self) -> Option<u64> {
        self.0.host_math_random_u64()
      }

      fn host_current_time_millis(&mut self) -> f64 {
        self.0.host_current_time_millis()
      }

      fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        self.0.as_any_mut()
      }

      fn host_make_job_callback(&mut self, callback: GcObject) -> Result<JobCallback, VmError> {
        self.0.host_make_job_callback(callback)
      }

      #[allow(deprecated)]
      fn host_make_job_callback_fallible(&mut self, callback: GcObject) -> Result<JobCallback, VmError> {
        self.0.host_make_job_callback_fallible(callback)
      }

      fn host_call_job_callback(
        &mut self,
        ctx: &mut dyn VmJobContext,
        callback: &JobCallback,
        this_argument: Value,
        arguments: &[Value],
      ) -> Result<Value, VmError> {
        self
          .0
          .host_call_job_callback(ctx, callback, this_argument, arguments)
      }

      fn host_promise_rejection_tracker(
        &mut self,
        promise: PromiseHandle,
        operation: PromiseRejectionOperation,
      ) {
        self.0.host_promise_rejection_tracker(promise, operation);
      }

      fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
        self.0.host_get_supported_import_attributes()
      }

      fn host_load_imported_module(
        &mut self,
        vm: &mut Vm,
        scope: &mut Scope<'_>,
        modules: &mut ModuleGraph,
        referrer: ModuleReferrer,
        module_request: ModuleRequest,
        host_defined: HostDefined,
        payload: ModuleLoadPayload,
      ) -> Result<(), VmError> {
        self
          .0
          .host_load_imported_module(vm, scope, modules, referrer, module_request, host_defined, payload)
      }
    }

    let mut proxy = HostProxy(self);
    crate::module_loading::finish_loading_imported_module(
      vm,
      scope,
      modules,
      &mut proxy,
      referrer,
      module_request,
      payload,
      Err(VmError::Unimplemented("HostLoadImportedModule")),
    )?;
    Ok(())
  }
}

#[cfg(test)]
mod oom_tests {
  use super::*;
  use crate::test_alloc::FailAllocsGuard;
  use crate::test_alloc::FailNextMatchingAllocGuard;
  use crate::{Heap, HeapLimits, VmOptions};
  use std::sync::atomic::AtomicUsize;

  // Keep this local so we can precisely fail `arc_try_new_vm`'s allocation without also failing
  // allocations performed by the panic runtime (important for regression tests that used to
  // `panic!` on OOM).
  #[repr(C)]
  struct ArcInner<T> {
    strong: AtomicUsize,
    weak: AtomicUsize,
    data: T,
  }

  const JOB_CALLBACK_ARC_INNER_SIZE: usize = std::mem::size_of::<ArcInner<JobCallbackInner>>();
  const JOB_CALLBACK_ARC_INNER_ALIGN: usize = std::mem::align_of::<ArcInner<JobCallbackInner>>();
  const U32_ARC_INNER_SIZE: usize = std::mem::size_of::<ArcInner<u32>>();
  const U32_ARC_INNER_ALIGN: usize = std::mem::align_of::<ArcInner<u32>>();

  trait IntoJobCallbackResult {
    fn into_job_callback_result(self) -> Result<JobCallback, VmError>;
  }

  impl IntoJobCallbackResult for JobCallback {
    fn into_job_callback_result(self) -> Result<JobCallback, VmError> {
      Ok(self)
    }
  }

  impl IntoJobCallbackResult for Result<JobCallback, VmError> {
    fn into_job_callback_result(self) -> Result<JobCallback, VmError> {
      self
    }
  }

  #[test]
  fn host_make_job_callback_returns_out_of_memory_on_arc_alloc_failure() {
    struct Host;

    impl VmHostHooks for Host {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {
      }
    }

    let mut host = Host;
    let callback = GcObject(crate::HeapId(0));
    let _guard = FailAllocsGuard::new();

    let err = host
      .host_make_job_callback(callback)
      .expect_err("expected OOM error");
    assert!(matches!(err, VmError::OutOfMemory));
  }

  #[test]
  fn job_callback_new_returns_out_of_memory_on_arc_alloc_failure_without_panicking() {
    let callback = GcObject(crate::HeapId(0));
    let _guard =
      FailNextMatchingAllocGuard::new(JOB_CALLBACK_ARC_INNER_SIZE, JOB_CALLBACK_ARC_INNER_ALIGN);

    let result = std::panic::catch_unwind(|| JobCallback::new(callback).into_job_callback_result())
      .expect("JobCallback::new should not panic on OOM");
    assert!(matches!(result, Err(VmError::OutOfMemory)));
  }

  #[test]
  fn job_callback_new_in_realm_returns_out_of_memory_on_arc_alloc_failure_without_panicking() {
    let callback = GcObject(crate::HeapId(0));
    let _guard =
      FailNextMatchingAllocGuard::new(JOB_CALLBACK_ARC_INNER_SIZE, JOB_CALLBACK_ARC_INNER_ALIGN);

    let result = std::panic::catch_unwind(|| {
      JobCallback::new_in_realm(callback, Some(RealmId::from_raw(123))).into_job_callback_result()
    })
    .expect("JobCallback::new_in_realm should not panic on OOM");
    assert!(matches!(result, Err(VmError::OutOfMemory)));
  }

  #[test]
  fn job_callback_new_with_data_returns_out_of_memory_on_host_data_arc_alloc_failure_without_panicking() {
    let callback = GcObject(crate::HeapId(0));
    let _guard = FailNextMatchingAllocGuard::new(U32_ARC_INNER_SIZE, U32_ARC_INNER_ALIGN);

    let result = std::panic::catch_unwind(|| {
      JobCallback::new_with_data(callback, 42u32).into_job_callback_result()
    })
    .expect("JobCallback::new_with_data should not panic on OOM");
    assert!(matches!(result, Err(VmError::OutOfMemory)));
  }

  #[test]
  fn job_callback_new_with_data_in_realm_returns_out_of_memory_on_callback_record_arc_alloc_failure_without_panicking() {
    let callback = GcObject(crate::HeapId(0));
    // First allocation (for the host data) should succeed; fail the allocation for the callback
    // record `Arc`.
    let _guard =
      FailNextMatchingAllocGuard::new(JOB_CALLBACK_ARC_INNER_SIZE, JOB_CALLBACK_ARC_INNER_ALIGN);

    let result = std::panic::catch_unwind(|| {
      JobCallback::new_with_data_in_realm(callback, 42u32, Some(RealmId::from_raw(123)))
        .into_job_callback_result()
    })
    .expect("JobCallback::new_with_data_in_realm should not panic on OOM");
    assert!(matches!(result, Err(VmError::OutOfMemory)));
  }

  #[test]
  fn job_root_helpers_return_out_of_memory_instead_of_aborting_on_allocator_oom() -> Result<(), VmError> {
    // Construct jobs before enabling allocation failure so `Job::new` can allocate its closure box.
    let mut job_push = Job::new(JobKind::Generic, |_ctx, _host| Ok(()))?;
    let mut job_extend = Job::new(JobKind::Generic, |_ctx, _host| Ok(()))?;

    // Fail all allocations on this thread. `try_push_root` / `try_extend_roots` must surface this
    // as `VmError::OutOfMemory` rather than aborting via `Vec` growth.
    let _guard = FailAllocsGuard::new();
    let push_result = job_push.try_push_root(RootId(0));
    let extend_result = job_extend.try_extend_roots([RootId(0), RootId(1)]);
    drop(_guard);

    assert!(matches!(push_result, Err(VmError::OutOfMemory)));
    assert!(matches!(extend_result, Err(VmError::OutOfMemory)));
    assert!(job_push.roots.is_empty());
    assert!(job_extend.roots.is_empty());
    Ok(())
  }

  #[test]
  fn promise_job_callback_wrap_is_oom_safe() -> Result<(), VmError> {
    fn noop(
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _host: &mut dyn VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: GcObject,
      _this: Value,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      Ok(Value::Undefined)
    }

    struct Host;
    impl VmHostHooks for Host {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    // Allocate all VM/heap state and the callable `then` function *before* forcing allocator
    // failures.
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let then_action = {
      let mut scope = heap.scope();
      let call_id = vm.register_native_call(noop)?;
      let name = scope.alloc_string("then")?;
      scope.alloc_native_function(call_id, None, name, 1)?
    };

    let mut host = Host;

    // When allocations fail, creating a Promise job that requires `HostMakeJobCallback` should not
    // panic; it must surface `VmError::OutOfMemory`.
    let _guard = FailAllocsGuard::new();
    let err = crate::create_promise_resolve_thenable_job(
      &mut host,
      &mut heap,
      Value::Undefined,
      Value::Object(then_action),
      Value::Undefined,
      Value::Undefined,
    )
    .expect_err("expected OOM error");

    assert!(matches!(err, VmError::OutOfMemory));
    Ok(())
  }

  #[test]
  fn job_try_extend_roots_rolls_back_on_oom_with_unknown_upper_bound_iterator() -> Result<(), VmError> {
    // Construct the job before enabling allocation failure so `Job::new` can allocate its closure box.
    let mut job = Job::new(JobKind::Generic, |_ctx, _host| Ok(()))?;

    // Ensure we can push the first element without allocating, so we can simulate OOM mid-extend.
    job
      .roots
      .try_reserve_exact(1)
      .map_err(|_| VmError::OutOfMemory)?;

    struct NoUpperBoundIter {
      items: [RootId; 2],
      idx: usize,
    }

    impl Iterator for NoUpperBoundIter {
      type Item = RootId;

      fn next(&mut self) -> Option<Self::Item> {
        let out = self.items.get(self.idx).copied();
        self.idx += 1;
        out
      }

      fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
      }
    }

    let iter = NoUpperBoundIter {
      items: [RootId(0), RootId(1)],
      idx: 0,
    };

    let _guard = FailAllocsGuard::new();
    let result = job.try_extend_roots(iter);
    drop(_guard);

    assert!(matches!(result, Err(VmError::OutOfMemory)));
    assert!(
      job.roots.is_empty(),
      "expected try_extend_roots to roll back on error"
    );
    Ok(())
  }
}
