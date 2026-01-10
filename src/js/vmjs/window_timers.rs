//! `Window` timers (`setTimeout`/`setInterval`/`queueMicrotask`) backed by FastRender's [`EventLoop`]
//! and `vm-js` values.
//!
//! This replaces the old placeholder Rust-level timer API (fake `JsValue` + Rust closures) with
//! real JS-visible global functions.
//!
//! ## Safety / determinism
//! String handlers are intentionally rejected with a `TypeError` for now to avoid string-eval and
//! keep behavior deterministic.

use crate::js::event_loop::{EventLoop, TaskSource, TimerId};
use crate::js::runtime::{current_event_loop_mut, with_event_loop};
use crate::js::vm_error_format;
use crate::js::window_realm::{
  dataset_exotic_delete, dataset_exotic_get, dataset_exotic_set, WindowRealmHost,
};
use std::time::Duration;
use vm_js::{
  ExecutionContext, Heap, Job, JobCallback, PromiseHandle, PromiseRejectionOperation, PromiseState,
  PropertyDescriptor, PropertyKey, PropertyKind, RealmId, RootId, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks, VmJobContext,
};
use webidl_vm_js::VmJsHostHooksPayload;
pub(crate) const SET_TIMEOUT_STRING_HANDLER_ERROR: &str =
  "setTimeout does not currently support string handlers";
pub(crate) const SET_TIMEOUT_NOT_CALLABLE_ERROR: &str = "setTimeout callback is not callable";
pub(crate) const SET_INTERVAL_STRING_HANDLER_ERROR: &str =
  "setInterval does not currently support string handlers";
pub(crate) const SET_INTERVAL_NOT_CALLABLE_ERROR: &str = "setInterval callback is not callable";
pub(crate) const QUEUE_MICROTASK_STRING_HANDLER_ERROR: &str =
  "queueMicrotask does not currently support string callbacks";
pub(crate) const QUEUE_MICROTASK_NOT_CALLABLE_ERROR: &str =
  "queueMicrotask callback is not callable";

const TIMER_REGISTRY_KEY: &str = "__fastrender_timer_registry";
// Internal copy of `queueMicrotask` used by host shims that need to schedule microtasks without
// being affected by user scripts overwriting `globalThis.queueMicrotask`.
pub(crate) const INTERNAL_QUEUE_MICROTASK_KEY: &str = "__fastrender_queue_microtask";
const TIMER_RECORD_CALLBACK_KEY: &str = "__callback";
const TIMER_RECORD_ARG_PREFIX: &str = "__arg";

// Native slot index on timer host functions that stores the owning global object.
const TIMER_GLOBAL_SLOT: usize = 0;
#[cfg(test)]
const SYMBOL_TO_NUMBER_ERROR: &str = "Cannot convert a Symbol value to a number";
fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn read_only_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: false,
    },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn throw_type_error(message: &'static str) -> VmError {
  // Use `VmError::TypeError` so the evaluator can construct a real `TypeError` object in the
  // current realm (mirrors how internal VM operations report type errors).
  VmError::TypeError(message)
}

fn throw_error(scope: &mut Scope<'_>, message: &str) -> VmError {
  match scope.alloc_string(message) {
    Ok(s) => VmError::Throw(Value::String(s)),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

fn value_to_number(heap: &mut Heap, value: Value) -> Result<f64, VmError> {
  heap.to_number(value)
}

fn normalize_delay_ms(heap: &mut Heap, value: Value) -> Result<u64, VmError> {
  let mut n = value_to_number(heap, value)?;
  if !n.is_finite() || n.is_nan() {
    n = 0.0;
  }
  if n < 0.0 {
    n = 0.0;
  }
  // `ToIntegerOrInfinity` rounds toward zero.
  let n = n.trunc();
  if n >= u64::MAX as f64 {
    Ok(u64::MAX)
  } else {
    Ok(n as u64)
  }
}

fn normalize_timer_id(heap: &mut Heap, value: Value) -> Result<TimerId, VmError> {
  let mut n = value_to_number(heap, value)?;
  if !n.is_finite() || n.is_nan() {
    n = 0.0;
  }
  let n = n.trunc();
  if n >= i32::MAX as f64 {
    Ok(i32::MAX)
  } else if n <= i32::MIN as f64 {
    Ok(i32::MIN)
  } else {
    Ok(n as i32)
  }
}

fn is_callable(scope: &Scope<'_>, value: Value) -> bool {
  // Prefer the engine's brand check: only `HeapObject::Function` values are callable.
  scope.heap().is_callable(value).unwrap_or(false)
}

fn get_timer_registry(
  scope: &mut Scope<'_>,
  global: vm_js::GcObject,
) -> Result<vm_js::GcObject, VmError> {
  let key_s = scope.alloc_string(TIMER_REGISTRY_KEY)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  match scope
    .heap()
    .object_get_own_data_property_value(global, &key)?
  {
    Some(Value::Object(obj)) => Ok(obj),
    _ => Err(VmError::Unimplemented(
      "timer registry missing on global object",
    )),
  }
}

fn timer_global_from_callee(
  scope: &Scope<'_>,
  callee: vm_js::GcObject,
) -> Result<vm_js::GcObject, VmError> {
  let slot = scope
    .heap()
    .get_function_native_slots(callee)?
    .get(TIMER_GLOBAL_SLOT)
    .copied()
    .unwrap_or(Value::Undefined);
  match slot {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::Unimplemented(
      "timer function missing global binding",
    )),
  }
}

fn timer_global_from_this(
  scope: &Scope<'_>,
  callee: vm_js::GcObject,
  this: Value,
  invalid_this_msg: &'static str,
) -> Result<vm_js::GcObject, VmError> {
  let global = timer_global_from_callee(scope, callee)?;
  match this {
    Value::Undefined | Value::Null => Ok(global),
    Value::Object(obj) if obj == global => Ok(global),
    _ => Err(throw_type_error(invalid_this_msg)),
  }
}

fn clear_registry_entry(
  scope: &mut Scope<'_>,
  registry: vm_js::GcObject,
  id: TimerId,
) -> Result<(), VmError> {
  let key = alloc_key(scope, &id.to_string())?;
  scope.define_property(registry, key, data_desc(Value::Undefined))?;
  Ok(())
}

fn store_timer_record(
  scope: &mut Scope<'_>,
  registry: vm_js::GcObject,
  id: TimerId,
  callback: Value,
  extra_args: &[Value],
) -> Result<(), VmError> {
  let record = scope.alloc_object()?;
  scope.push_root(Value::Object(record))?;

  let callback_key = alloc_key(scope, TIMER_RECORD_CALLBACK_KEY)?;
  scope.define_property(record, callback_key, data_desc(callback))?;

  for (idx, arg) in extra_args.iter().copied().enumerate() {
    let key = alloc_key(scope, &format!("{TIMER_RECORD_ARG_PREFIX}{idx}"))?;
    scope.define_property(record, key, data_desc(arg))?;
  }

  let id_key = alloc_key(scope, &id.to_string())?;
  scope.define_property(registry, id_key, data_desc(Value::Object(record)))?;

  Ok(())
}

pub(crate) fn vm_error_to_event_loop_error(heap: &mut Heap, err: VmError) -> crate::error::Error {
  vm_error_format::vm_error_to_error(heap, err)
}

struct HeapRootContext<'a> {
  heap: &'a mut Heap,
}

impl VmJobContext for HeapRootContext<'_> {
  fn call(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("HeapRootContext::call"))
  }

  fn construct(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("HeapRootContext::construct"))
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id);
  }
}

struct WindowRealmJobContext<'a> {
  vm: &'a mut Vm,
  heap: &'a mut Heap,
  host: &'a mut dyn VmHost,
  realm: Option<RealmId>,
}

impl<'a> WindowRealmJobContext<'a> {
  fn new(
    vm: &'a mut Vm,
    heap: &'a mut Heap,
    host: &'a mut dyn VmHost,
    realm: Option<RealmId>,
  ) -> Self {
    Self { vm, heap, host, realm }
  }
}

impl VmJobContext for WindowRealmJobContext<'_> {
  fn call(
    &mut self,
    host_hooks: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let host = &mut *self.host;
    let mut scope = self.heap.scope();
    if let Some(realm) = self.realm {
      let mut vm = self.vm.execution_context_guard(ExecutionContext {
        realm,
        script_or_module: None,
      });
      vm.call_with_host_and_hooks(host, &mut scope, host_hooks, callee, this, args)
    } else {
      self
        .vm
        .call_with_host_and_hooks(host, &mut scope, host_hooks, callee, this, args)
    }
  }

  fn construct(
    &mut self,
    host_hooks: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let host = &mut *self.host;
    let mut scope = self.heap.scope();
    if let Some(realm) = self.realm {
      let mut vm = self.vm.execution_context_guard(ExecutionContext {
        realm,
        script_or_module: None,
      });
      vm.construct_with_host_and_hooks(host, &mut scope, host_hooks, callee, args, new_target)
    } else {
      self.vm.construct_with_host_and_hooks(
        host,
        &mut scope,
        host_hooks,
        callee,
        args,
        new_target,
      )
    }
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id);
  }
}

pub struct VmJsEventLoopHooks<Host: WindowRealmHost + 'static> {
  any: VmJsHostHooksPayload,
  pending_discard: Vec<Job>,
  enqueue_error: Option<crate::error::Error>,
  _marker: std::marker::PhantomData<fn() -> Host>,
}

impl<Host: WindowRealmHost + 'static> VmJsEventLoopHooks<Host> {
  /// Create host hooks for `vm-js` execution.
  ///
  /// Note: `vm-js` entry points like [`vm_js::Vm::call_with_host`] and
  /// [`vm_js::JsRuntime::exec_script_with_hooks`] pass a dummy [`vm_js::VmHost`] (`()`) to native
  /// handlers, so bindings that need embedder state must downcast via
  /// [`vm_js::VmHostHooks::as_any_mut`]. This hook implementation wires that up by returning a
  /// [`webidl_vm_js::VmJsHostHooksPayload`] containing:
  /// - the active [`vm_js::VmHost`] context (for downcasting), and
  /// - a [`webidl_vm_js::WebIdlBindingsHostSlot`] for WebIDL host dispatch.
  pub fn new(host_ctx: &mut dyn VmHost) -> Self {
    let mut any = VmJsHostHooksPayload::default();
    any.set_vm_host(host_ctx);
    Self {
      any,
      pending_discard: Vec::new(),
      enqueue_error: None,
      _marker: std::marker::PhantomData,
    }
  }

  pub fn new_with_host(host: &mut Host) -> Self {
    // Initialize the payload with the active `VmHost` context.
    let mut hooks = {
      let (host_ctx, _) = host.vm_host_and_window_realm();
      Self::new(host_ctx)
    };
    // Populate the WebIDL bindings host slot if the embedding provides one.
    if let Some(bindings_host) = host.webidl_bindings_host() {
      hooks.any.webidl_bindings_host_slot_mut().set(bindings_host);
    }
    hooks
  }

  pub fn finish(mut self, heap: &mut Heap) -> Option<crate::error::Error> {
    if !self.pending_discard.is_empty() {
      let mut ctx = HeapRootContext { heap };
      for job in self.pending_discard.drain(..) {
        job.discard(&mut ctx);
      }
    }
    self.enqueue_error.take()
  }
}

impl<Host: WindowRealmHost + 'static> VmHostHooks for VmJsEventLoopHooks<Host> {
  fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
    Some(&mut self.any)
  }

  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    if self.enqueue_error.is_some() {
      self.pending_discard.push(job);
      return;
    }

    let job_cell: std::rc::Rc<std::cell::RefCell<Option<Job>>> =
      std::rc::Rc::new(std::cell::RefCell::new(Some(job)));
    let job_cell_for_closure = std::rc::Rc::clone(&job_cell);

    let enqueue_result: crate::error::Result<()> = (|| {
      let Some(event_loop) = current_event_loop_mut::<Host>() else {
        return Err(crate::error::Error::Other(
          "vm-js Promise job enqueued without an active EventLoop".to_string(),
        ));
      };

      event_loop.queue_microtask(move |host, event_loop| {
        let Some(job) = job_cell_for_closure.borrow_mut().take() else {
          return Ok(());
        };

        let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
        // Borrow-split the host so we can pass both:
        // - a real `VmHost` context to native calls, and
        // - a mutable `WindowRealm` for executing the job.
        let (host_ctx, window_realm) = host.vm_host_and_window_realm();
        window_realm.reset_interrupt();

        let result: crate::error::Result<()> = with_event_loop(event_loop, || {
          let budget = window_realm.vm_budget_now();
          let (vm, heap) = window_realm.vm_and_heap_mut();
          let mut vm = vm.push_budget(budget);
          let tick_result = vm.tick();

          let job_result = match tick_result {
            Ok(()) => {
              let mut ctx = WindowRealmJobContext::new(&mut vm, heap, host_ctx, realm);
              job.run(&mut ctx, &mut hooks)
            }
            Err(err) => {
              // If the VM is already out of budget (deadline exceeded, interrupted, out of fuel),
              // we must still discard the job so any persistent roots it owns are cleaned up.
              let mut ctx = WindowRealmJobContext::new(&mut vm, heap, host_ctx, realm);
              job.discard(&mut ctx);
              Err(err)
            }
          };

          job_result
            .map_err(|err| vm_error_to_event_loop_error(heap, err))
            .map(|_| ())
        });

        if let Some(err) = hooks.finish(window_realm.heap_mut()) {
          return Err(err);
        }
        result
      })
    })();

    if let Err(err) = enqueue_result {
      if let Some(job) = job_cell.borrow_mut().take() {
        self.pending_discard.push(job);
      }
      self.enqueue_error = Some(err);
    }
  }

  fn host_exotic_get(
    &mut self,
    scope: &mut Scope<'_>,
    obj: vm_js::GcObject,
    key: vm_js::PropertyKey,
    receiver: vm_js::Value,
  ) -> Result<Option<vm_js::Value>, VmError> {
    let _ = receiver;
    dataset_exotic_get(scope, obj, key)
  }

  fn host_exotic_set(
    &mut self,
    scope: &mut Scope<'_>,
    obj: vm_js::GcObject,
    key: vm_js::PropertyKey,
    value: vm_js::Value,
    receiver: vm_js::Value,
  ) -> Result<Option<bool>, VmError> {
    let _ = receiver;
    dataset_exotic_set(scope, obj, key, value)
  }

  fn host_exotic_delete(
    &mut self,
    scope: &mut Scope<'_>,
    obj: vm_js::GcObject,
    key: vm_js::PropertyKey,
  ) -> Result<Option<bool>, VmError> {
    dataset_exotic_delete(scope, obj, key)
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

  fn host_promise_rejection_tracker(
    &mut self,
    promise: PromiseHandle,
    operation: PromiseRejectionOperation,
  ) {
    let Some(event_loop) = current_event_loop_mut::<Host>() else {
      // Not executing inside a FastRender `EventLoop` turn; ignore.
      return;
    };

    // Ensure we have a microtask checkpoint hook installed so we can dispatch events after the
    // microtask queue is drained (HTML "notify about rejected promises").
    event_loop.set_microtask_checkpoint_hook(Some(promise_rejection_microtask_checkpoint_hook::<Host>));

    let cap = event_loop.queue_limits().max_pending_tasks;
    let tracker = &mut event_loop.promise_rejection_tracker;

    match operation {
      PromiseRejectionOperation::Reject => {
        if tracker.about_to_be_notified.len() >= cap {
          return;
        }
        // Avoid duplicate tracking if the engine calls `Reject` more than once for the same
        // promise (defensive).
        if tracker.about_to_be_notified.iter().any(|p| *p == promise) {
          return;
        }
        tracker.about_to_be_notified.push(promise);
      }
      PromiseRejectionOperation::Handle => {
        // If the promise is still in the about-to-be-notified list, a handler was added before the
        // end-of-checkpoint notification step, so no `unhandledrejection` should be queued.
        if let Some(idx) = tracker
          .about_to_be_notified
          .iter()
          .position(|p| *p == promise)
        {
          tracker.about_to_be_notified.swap_remove(idx);
          return;
        }

        // If the promise was previously notified as unhandled and is now handled, queue a
        // `rejectionhandled` notification.
        if tracker.outstanding_rejected.remove(&promise) {
          if tracker.maybe_handled.len() >= cap {
            return;
          }
          tracker.maybe_handled.push(promise);
        }
      }
    }
  }
}

fn queue_promise_rejection_event_task<Host: WindowRealmHost + 'static>(
  event_loop: &mut EventLoop<Host>,
  heap: &mut Heap,
  promise: PromiseHandle,
  event_type: &'static str,
) -> crate::error::Result<()> {
  let promise_obj: vm_js::GcObject = promise.into();
  if !heap.is_valid_object(promise_obj) {
    return Ok(());
  }

  // Keep the promise (and thus its `[[PromiseResult]]`) alive until the event task runs.
  let root = heap
    .add_root(Value::Object(promise_obj))
    .map_err(|e| crate::error::Error::Other(e.to_string()))?;

  // `event_loop.queue_task` is fallible (queue limits); ensure the root is removed on failure.
  let queue_result = event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
    let (vm_host, window_realm) = host.vm_host_and_window_realm();
    window_realm.reset_interrupt();
    let global_obj = window_realm.global_object();
    let budget = window_realm.vm_budget_now();
    let (vm, heap) = window_realm.vm_and_heap_mut();

    let result: crate::error::Result<bool> = with_event_loop(event_loop, || {
      let mut vm = vm.push_budget(budget);
      vm.tick()
        .map_err(|err| vm_error_to_event_loop_error(heap, err))?;
      let handled_after_dispatch = (|| -> Result<bool, VmError> {
        let promise_value = heap.get_root(root).unwrap_or(Value::Undefined);
        let Value::Object(promise_obj) = promise_value else {
          // Root slot should always contain the promise object, but be defensive in release builds.
          return Ok(true);
        };

        let reason = heap
          .promise_result(promise_obj)?
          .unwrap_or(Value::Undefined);

        let mut scope = heap.scope();

        scope.push_root(Value::Object(global_obj))?;
        scope.push_root(Value::Object(promise_obj))?;
        scope.push_root(reason)?;

        // Use the realm's `PromiseRejectionEvent` constructor (if available) so promise rejection
        // events look like the web platform:
        // - cancelable `unhandledrejection` events support `preventDefault()`
        // - `promise`/`reason` are exposed as read-only properties.
        let type_s = scope.alloc_string(event_type)?;
        scope.push_root(Value::String(type_s))?;

        let cancelable = event_type == "unhandledrejection";

        let init_obj = scope.alloc_object()?;
        scope.push_root(Value::Object(init_obj))?;
        if cancelable {
          let cancelable_key = alloc_key(&mut scope, "cancelable")?;
          scope.define_property(init_obj, cancelable_key, data_desc(Value::Bool(true)))?;
        }
        let promise_key = alloc_key(&mut scope, "promise")?;
        scope.define_property(
          init_obj,
          promise_key,
          data_desc(Value::Object(promise_obj)),
        )?;
        let reason_key = alloc_key(&mut scope, "reason")?;
        scope.define_property(init_obj, reason_key, data_desc(reason))?;

        let promise_rejection_ctor_key = alloc_key(&mut scope, "PromiseRejectionEvent")?;
        let promise_rejection_ctor = vm.get(&mut scope, global_obj, promise_rejection_ctor_key)?;
        scope.push_root(promise_rejection_ctor)?;

        let (event_value, needs_payload_define) =
          if scope.heap().is_callable(promise_rejection_ctor).unwrap_or(false) {
            (
              vm.call_with_host_and_hooks(
                vm_host,
                &mut scope,
                &mut hooks,
                promise_rejection_ctor,
                Value::Undefined,
                &[Value::String(type_s), Value::Object(init_obj)],
              )?,
              false,
            )
          } else {
            let event_ctor_key = alloc_key(&mut scope, "Event")?;
            let event_ctor = vm.get(&mut scope, global_obj, event_ctor_key)?;
            scope.push_root(event_ctor)?;
            (
              vm.call_with_host_and_hooks(
                vm_host,
                &mut scope,
                &mut hooks,
                event_ctor,
                Value::Undefined,
                &[Value::String(type_s), Value::Object(init_obj)],
              )?,
              true,
            )
          };

        let Value::Object(event_obj) = event_value else {
          return Err(VmError::Unimplemented(
            "PromiseRejectionEvent/Event constructor returned non-object",
          ));
        };
        scope.push_root(Value::Object(event_obj))?;

        if needs_payload_define {
          scope.define_property(
            event_obj,
            reason_key,
            read_only_data_desc(reason),
          )?;
          scope.define_property(
            event_obj,
            promise_key,
            read_only_data_desc(Value::Object(promise_obj)),
          )?;
        }

        let dispatch_key = alloc_key(&mut scope, "dispatchEvent")?;
        let dispatch = vm.get(&mut scope, global_obj, dispatch_key)?;
        let _ = vm.call_with_host_and_hooks(
          vm_host,
          &mut scope,
          &mut hooks,
          dispatch,
          Value::Object(global_obj),
          &[Value::Object(event_obj)],
        )?;

        Ok(scope.heap().promise_is_handled(promise_obj)?)
      })();

      handled_after_dispatch
        .map_err(|err| vm_error_to_event_loop_error(heap, err))
    });

    let finish_err = hooks.finish(heap);
    // Always remove the persistent root, even if dispatch failed.
    heap.remove_root(root);

    if let Some(err) = finish_err {
      return Err(err);
    }

    let handled_after_dispatch = result?;

    // Only promises that remain unhandled after `unhandledrejection` dispatch should be eligible
    // for `rejectionhandled` later.
    if event_type == "unhandledrejection" && !handled_after_dispatch {
      let cap = event_loop.queue_limits().max_pending_tasks;
      let tracker = &mut event_loop.promise_rejection_tracker;
      if tracker.outstanding_rejected.len() < cap {
        tracker.outstanding_rejected.insert(promise);
      }
    }

    Ok(())
  });

  if let Err(err) = queue_result {
    heap.remove_root(root);
    return Err(err);
  }

  Ok(())
}

fn promise_rejection_microtask_checkpoint_hook<Host: WindowRealmHost + 'static>(
  host: &mut Host,
  event_loop: &mut EventLoop<Host>,
) -> crate::error::Result<()> {
  let (to_notify, to_handle) = {
    let tracker = &mut event_loop.promise_rejection_tracker;
    (
      std::mem::take(&mut tracker.about_to_be_notified),
      std::mem::take(&mut tracker.maybe_handled),
    )
  };

  let window_realm = host.window_realm();
  let heap = window_realm.heap_mut();

  // Keep the outstanding set bounded even though it is not a real "weak set" like HTML's: if
  // promises have been collected, drop their stale handles so new rejections can be tracked.
  event_loop
    .promise_rejection_tracker
    .outstanding_rejected
    .retain(|promise| heap.is_valid_object((*promise).into()));

  for promise in to_notify {
    let promise_obj: vm_js::GcObject = promise.into();
    if !heap.is_valid_object(promise_obj) {
      continue;
    }
    let Ok(PromiseState::Rejected) = heap.promise_state(promise_obj) else {
      continue;
    };
    let Ok(false) = heap.promise_is_handled(promise_obj) else {
      continue;
    };

    queue_promise_rejection_event_task::<Host>(event_loop, heap, promise, "unhandledrejection")?;
  }

  for promise in to_handle {
    queue_promise_rejection_event_task::<Host>(event_loop, heap, promise, "rejectionhandled")?;
  }

  Ok(())
}

// --- Compile-time regression guard (vm-js Promise-job GC safety) ---
//
// FastRender's host microtask queue is not traced by `vm-js`'s GC. Promise jobs can outlive the
// stack/rooting scope that created them, so queued jobs must be able to own persistent roots for
// any captured `vm_js::Value`s. If `vendor/ecma-rs` is updated to a `vm-js` version that regresses
// this API, we want compilation to fail immediately instead of silently reintroducing stale-handle
// bugs.
#[allow(dead_code)]
 mod vm_js_gc_safety_guard {
  // Keep this guard signature-based so it fails at compile time if the `vm-js` job API regresses.
  #[allow(clippy::type_complexity)]
  const _: () = {
    // `Job` must support owning persistent roots for captured Values.
    let _add_root: fn(
      &mut vm_js::Job,
      &mut dyn vm_js::VmJobContext,
      vm_js::Value,
    ) -> Result<vm_js::RootId, vm_js::VmError> = vm_js::Job::add_root;

    // `Job` must be executable/discardable with access to a `VmJobContext` so it can clean up roots.
    let _run: fn(
      vm_js::Job,
      &mut dyn vm_js::VmJobContext,
      &mut dyn vm_js::VmHostHooks,
    ) -> vm_js::JobResult = vm_js::Job::run;
     let _discard: fn(vm_js::Job, &mut dyn vm_js::VmJobContext) = vm_js::Job::discard;
   };
 }
 
fn set_timeout_native<Host: WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let handler = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(handler, Value::String(_)) {
    return Err(throw_type_error(SET_TIMEOUT_STRING_HANDLER_ERROR));
  }
  if !is_callable(scope, handler) {
    return Err(throw_type_error(SET_TIMEOUT_NOT_CALLABLE_ERROR));
  }

  let delay_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let delay_ms = normalize_delay_ms(scope.heap_mut(), delay_value)?;
  let delay = Duration::from_millis(delay_ms);
  let extra_args: Vec<Value> = if args.len() > 2 {
    args[2..].to_vec()
  } else {
    Vec::new()
  };

  let global_obj = timer_global_from_this(
    scope,
    callee,
    this,
    "setTimeout called with invalid this value",
  )?;
  let registry = get_timer_registry(scope, global_obj)?;

  let Some(event_loop) = current_event_loop_mut::<Host>() else {
    return Err(throw_type_error(
      "setTimeout called without an active EventLoop",
    ));
  };

  let id_cell = std::rc::Rc::new(std::cell::Cell::new(0));
  let id_cell_for_cb = id_cell.clone();

  let callback = handler;
  let extra_args_for_cb = extra_args.clone();

  let id = event_loop
    .set_timeout(delay, move |host, event_loop| {
      let id = id_cell_for_cb.get();
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
      let (host_ctx, window_realm) = host.vm_host_and_window_realm();
      window_realm.reset_interrupt();
      let budget = window_realm.vm_budget_now();
      let (vm, heap) = window_realm.vm_and_heap_mut();

      let result: crate::error::Result<()> = with_event_loop(event_loop, || {
        let mut vm = vm.push_budget(budget);
        let tick_result = vm.tick();

        let call_result = tick_result.and_then(|_| {
          let mut scope = heap.scope();
          vm.call_with_host_and_hooks(
            host_ctx,
            &mut scope,
            &mut hooks,
            callback,
            Value::Object(global_obj),
            &extra_args_for_cb,
          )
          .map(|_| ())
        });
        call_result
          .map_err(|err| vm_error_to_event_loop_error(heap, err))
          .map(|_| ())
      });
      let finish_err = hooks.finish(&mut *heap);

      {
        // Always clear the registry entry for one-shot timeouts, even if the callback throws.
        let mut scope = heap.scope();
        let _ = clear_registry_entry(&mut scope, registry, id);
      }
      if let Some(err) = finish_err {
        event_loop.clear_timeout(id);
        return Err(err);
      }
      if let Err(err) = result {
        event_loop.clear_timeout(id);
        return Err(err);
      }

      Ok(())
    })
    .map_err(|e| throw_error(scope, &format!("{e}")))?;

  id_cell.set(id);
  if let Err(err) = store_timer_record(scope, registry, id, callback, &extra_args) {
    // If we cannot store the record, the callback/args may be GC'd (Rust closures are not traced),
    // so we must cancel the timer to avoid UAF.
    event_loop.clear_timeout(id);
    return Err(err);
  }

  Ok(Value::Number(id as f64))
}

fn clear_timeout_native<Host: WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let id_value = args.get(0).copied().unwrap_or(Value::Number(0.0));
  let id = normalize_timer_id(scope.heap_mut(), id_value)?;

  let global_obj = timer_global_from_this(
    scope,
    callee,
    this,
    "clearTimeout called with invalid this value",
  )?;
  let registry = get_timer_registry(scope, global_obj)?;

  let Some(event_loop) = current_event_loop_mut::<Host>() else {
    return Err(throw_type_error(
      "clearTimeout called without an active EventLoop",
    ));
  };
  event_loop.clear_timeout(id);

  // Best-effort: clear the registry entry so callbacks/args can be collected.
  let _ = clear_registry_entry(scope, registry, id);

  Ok(Value::Undefined)
}

fn set_interval_native<Host: WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let handler = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(handler, Value::String(_)) {
    return Err(throw_type_error(SET_INTERVAL_STRING_HANDLER_ERROR));
  }
  if !is_callable(scope, handler) {
    return Err(throw_type_error(SET_INTERVAL_NOT_CALLABLE_ERROR));
  }

  let delay_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let interval_ms = normalize_delay_ms(scope.heap_mut(), delay_value)?;
  let interval = Duration::from_millis(interval_ms);
  let extra_args: Vec<Value> = if args.len() > 2 {
    args[2..].to_vec()
  } else {
    Vec::new()
  };

  let global_obj = timer_global_from_this(
    scope,
    callee,
    this,
    "setInterval called with invalid this value",
  )?;
  let registry = get_timer_registry(scope, global_obj)?;

  let Some(event_loop) = current_event_loop_mut::<Host>() else {
    return Err(throw_type_error(
      "setInterval called without an active EventLoop",
    ));
  };

  let id_cell = std::rc::Rc::new(std::cell::Cell::new(0));
  let id_cell_for_cb = id_cell.clone();

  let callback = handler;
  let extra_args_for_cb = extra_args.clone();

  let id = event_loop
    .set_interval(interval, move |host, event_loop| {
      let id = id_cell_for_cb.get();
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
      let (host_ctx, window_realm) = host.vm_host_and_window_realm();
      window_realm.reset_interrupt();
      let budget = window_realm.vm_budget_now();
      let (vm, heap) = window_realm.vm_and_heap_mut();

      let result: crate::error::Result<()> = with_event_loop(event_loop, || {
        let mut vm = vm.push_budget(budget);
        let tick_result = vm.tick();

        let call_result = tick_result.and_then(|_| {
          let mut scope = heap.scope();
          vm.call_with_host_and_hooks(
            host_ctx,
            &mut scope,
            &mut hooks,
            callback,
            Value::Object(global_obj),
            &extra_args_for_cb,
          )
          .map(|_| ())
        });

        call_result
          .map_err(|err| vm_error_to_event_loop_error(heap, err))
          .map(|_| ())
      });
      let finish_err = hooks.finish(&mut *heap);

      if let Some(err) = finish_err {
        // On error, cancel the interval and drop JS references to avoid repeated errors/leaks.
        event_loop.clear_interval(id);
        {
          let mut scope = heap.scope();
          let _ = clear_registry_entry(&mut scope, registry, id);
        }
        return Err(err);
      }
      if let Err(err) = result {
        // On error, cancel the interval and drop JS references to avoid repeated errors/leaks.
        event_loop.clear_interval(id);
        {
          let mut scope = heap.scope();
          let _ = clear_registry_entry(&mut scope, registry, id);
        }
        return Err(err);
      }

      Ok(())
    })
    .map_err(|e| throw_error(scope, &format!("{e}")))?;

  id_cell.set(id);
  if let Err(err) = store_timer_record(scope, registry, id, callback, &extra_args) {
    event_loop.clear_interval(id);
    return Err(err);
  }

  Ok(Value::Number(id as f64))
}

fn clear_interval_native<Host: WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let id_value = args.get(0).copied().unwrap_or(Value::Number(0.0));
  let id = normalize_timer_id(scope.heap_mut(), id_value)?;

  let global_obj = timer_global_from_this(
    scope,
    callee,
    this,
    "clearInterval called with invalid this value",
  )?;
  let registry = get_timer_registry(scope, global_obj)?;

  let Some(event_loop) = current_event_loop_mut::<Host>() else {
    return Err(throw_type_error(
      "clearInterval called without an active EventLoop",
    ));
  };
  event_loop.clear_interval(id);

  let _ = clear_registry_entry(scope, registry, id);

  Ok(Value::Undefined)
}

fn queue_microtask_native<Host: WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(callback, Value::String(_)) {
    return Err(throw_type_error(QUEUE_MICROTASK_STRING_HANDLER_ERROR));
  }
  if !is_callable(scope, callback) {
    return Err(throw_type_error(QUEUE_MICROTASK_NOT_CALLABLE_ERROR));
  }

  let _global_obj = timer_global_from_this(
    scope,
    callee,
    this,
    "queueMicrotask called with invalid this value",
  )?;

  let Some(event_loop) = current_event_loop_mut::<Host>() else {
    return Err(throw_type_error(
      "queueMicrotask called without an active EventLoop",
    ));
  };

  // Keep the callback alive until the microtask runs.
  let root = scope.heap_mut().add_root(callback)?;
  event_loop
    .queue_microtask(move |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
      let (host_ctx, window_realm) = host.vm_host_and_window_realm();
      window_realm.reset_interrupt();
      let budget = window_realm.vm_budget_now();
      let (vm, heap) = window_realm.vm_and_heap_mut();
      let callback = heap.get_root(root).unwrap_or(Value::Undefined);

      let result: crate::error::Result<()> = with_event_loop(event_loop, || {
        let mut vm = vm.push_budget(budget);
        let tick_result = vm.tick();

        let call_result = tick_result.and_then(|_| {
          let call_result: Result<(), VmError> = (|| {
            let mut scope = heap.scope();
            // HTML `queueMicrotask` invokes callbacks with an `undefined` callback-this value.
            vm
              .call_with_host_and_hooks(
                host_ctx,
                &mut scope,
                &mut hooks,
                callback,
                Value::Undefined,
                &[],
              )
              .map(|_| ())
          })();
          call_result
        });

        call_result
          .map_err(|err| vm_error_to_event_loop_error(heap, err))
          .map(|_| ())
      });

      let finish_err = hooks.finish(&mut *heap);
      heap.remove_root(root);

      if let Some(err) = finish_err {
        return Err(err);
      }

      result
    })
    .map_err(|e| {
      // If queueing fails, ensure we don't leak the persistent root.
      scope.heap_mut().remove_root(root);
      throw_error(scope, &format!("{e}"))
    })?;

  Ok(Value::Undefined)
}

/// Install `setTimeout`/`setInterval`/`clearTimeout`/`clearInterval`/`queueMicrotask` on the JS global.
///
/// This should be installed on a `Window`-like realm. The native implementations capture the
/// global object via native slots so identifier calls (`setTimeout(cb, 0)`) work even though
/// `vm-js` supplies `this = undefined` in that case.
pub fn install_window_timers_bindings<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &vm_js::Realm,
  heap: &mut Heap,
) -> Result<(), VmError> {
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // Internal registry that keeps timer callback/argument values alive until they are fired or
  // canceled.
  let registry = scope.alloc_object()?;
  scope.push_root(Value::Object(registry))?;
  let registry_key = alloc_key(&mut scope, TIMER_REGISTRY_KEY)?;
  scope.define_property(global, registry_key, data_desc(Value::Object(registry)))?;

  let global_slots = [Value::Object(global)];

  let set_timeout_id = vm.register_native_call(set_timeout_native::<Host>)?;
  let set_timeout_name = scope.alloc_string("setTimeout")?;
  scope.push_root(Value::String(set_timeout_name))?;
  let set_timeout = scope.alloc_native_function_with_slots(
    set_timeout_id,
    None,
    set_timeout_name,
    1,
    &global_slots,
  )?;
  scope
    .heap_mut()
    .object_set_prototype(set_timeout, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(set_timeout))?;

  let clear_timeout_id = vm.register_native_call(clear_timeout_native::<Host>)?;
  let clear_timeout_name = scope.alloc_string("clearTimeout")?;
  scope.push_root(Value::String(clear_timeout_name))?;
  let clear_timeout = scope.alloc_native_function_with_slots(
    clear_timeout_id,
    None,
    clear_timeout_name,
    1,
    &global_slots,
  )?;
  scope
    .heap_mut()
    .object_set_prototype(clear_timeout, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(clear_timeout))?;

  let set_interval_id = vm.register_native_call(set_interval_native::<Host>)?;
  let set_interval_name = scope.alloc_string("setInterval")?;
  scope.push_root(Value::String(set_interval_name))?;
  let set_interval = scope.alloc_native_function_with_slots(
    set_interval_id,
    None,
    set_interval_name,
    1,
    &global_slots,
  )?;
  scope
    .heap_mut()
    .object_set_prototype(set_interval, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(set_interval))?;

  let clear_interval_id = vm.register_native_call(clear_interval_native::<Host>)?;
  let clear_interval_name = scope.alloc_string("clearInterval")?;
  scope.push_root(Value::String(clear_interval_name))?;
  let clear_interval = scope.alloc_native_function_with_slots(
    clear_interval_id,
    None,
    clear_interval_name,
    1,
    &global_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    clear_interval,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(clear_interval))?;

  let queue_microtask_id = vm.register_native_call(queue_microtask_native::<Host>)?;
  let queue_microtask_name = scope.alloc_string("queueMicrotask")?;
  scope.push_root(Value::String(queue_microtask_name))?;
  let queue_microtask = scope.alloc_native_function_with_slots(
    queue_microtask_id,
    None,
    queue_microtask_name,
    1,
    &global_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    queue_microtask,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(queue_microtask))?;

  let set_timeout_key = alloc_key(&mut scope, "setTimeout")?;
  let clear_timeout_key = alloc_key(&mut scope, "clearTimeout")?;
  let set_interval_key = alloc_key(&mut scope, "setInterval")?;
  let clear_interval_key = alloc_key(&mut scope, "clearInterval")?;
  let queue_microtask_key = alloc_key(&mut scope, "queueMicrotask")?;

  scope.define_property(
    global,
    set_timeout_key,
    data_desc(Value::Object(set_timeout)),
  )?;
  scope.define_property(
    global,
    clear_timeout_key,
    data_desc(Value::Object(clear_timeout)),
  )?;
  scope.define_property(
    global,
    set_interval_key,
    data_desc(Value::Object(set_interval)),
  )?;
  scope.define_property(
    global,
    clear_interval_key,
    data_desc(Value::Object(clear_interval)),
  )?;
  scope.define_property(
    global,
    queue_microtask_key,
    data_desc(Value::Object(queue_microtask)),
  )?;

  // Keep an internal, non-configurable reference so other host-side shims can safely schedule
  // microtasks even if the page overwrites `queueMicrotask`.
  let internal_queue_microtask_key = alloc_key(&mut scope, INTERNAL_QUEUE_MICROTASK_KEY)?;
  scope.define_property(
    global,
    internal_queue_microtask_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Object(queue_microtask),
        writable: false,
      },
    },
  )?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::clock::VirtualClock;
  use crate::js::event_loop::{EventLoop, QueueLimits, RunLimits, RunUntilIdleOutcome, TaskSource};
  use crate::js::JsExecutionOptions;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use std::collections::HashMap;
  use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
  use std::sync::{Arc, Mutex, OnceLock};
  use std::time::Duration;
  use vm_js::Realm;
  use webidl_vm_js::{host_from_hooks, WebIdlBindingsHost};

  const CALLBACK_GLOBAL_KEY: &str = "__test_global";

  fn assert_type_error_contains(heap: &mut Heap, err: VmError, expected: &str) {
    match err {
      VmError::TypeError(msg) => {
        assert!(msg.contains(expected), "msg={msg:?} expected={expected:?}");
      }
      other => {
        let rendered = vm_error_format::vm_error_to_string(heap, other);
        let first_line = rendered.lines().next().unwrap_or("");
        assert!(
          first_line.starts_with("TypeError"),
          "expected TypeError, got {rendered:?}"
        );
        assert!(
          first_line.contains(expected),
          "expected TypeError message containing {expected:?}, got {rendered:?}"
        );
      }
    }
  }

  static PROMISE_JOB_LOGS: OnceLock<Mutex<HashMap<usize, Arc<Mutex<Vec<&'static str>>>>>> =
    OnceLock::new();

  fn promise_job_logs() -> &'static Mutex<HashMap<usize, Arc<Mutex<Vec<&'static str>>>>> {
    PROMISE_JOB_LOGS.get_or_init(|| Mutex::new(HashMap::new()))
  }

  static JOB_CALLBACK_CALLS: AtomicUsize = AtomicUsize::new(0);

  type CurrentRealmLog = Arc<Mutex<Option<Option<vm_js::RealmId>>>>;

  static CURRENT_REALM_LOGS: OnceLock<Mutex<HashMap<usize, CurrentRealmLog>>> = OnceLock::new();

  fn current_realm_logs() -> &'static Mutex<HashMap<usize, CurrentRealmLog>> {
    CURRENT_REALM_LOGS.get_or_init(|| Mutex::new(HashMap::new()))
  }

  struct HeapPromiseJobLogGuard {
    heap_ptr: usize,
  }

  impl Drop for HeapPromiseJobLogGuard {
    fn drop(&mut self) {
      promise_job_logs().lock().unwrap().remove(&self.heap_ptr);
    }
  }

  fn install_promise_job_log(
    heap: &Heap,
    log: Arc<Mutex<Vec<&'static str>>>,
  ) -> HeapPromiseJobLogGuard {
    let heap_ptr = heap as *const Heap as usize;
    promise_job_logs().lock().unwrap().insert(heap_ptr, log);
    HeapPromiseJobLogGuard { heap_ptr }
  }

  fn record_promise_job_log(heap_ptr: usize, label: &'static str) {
    let log = promise_job_logs().lock().unwrap().get(&heap_ptr).cloned();
    if let Some(log) = log {
      log.lock().unwrap().push(label);
    }
  }

  struct HostCtx {
    hook_downcast_count: usize,
  }

  struct HeapCurrentRealmLogGuard {
    heap_ptr: usize,
  }

  impl Drop for HeapCurrentRealmLogGuard {
    fn drop(&mut self) {
      current_realm_logs().lock().unwrap().remove(&self.heap_ptr);
    }
  }

  fn install_current_realm_log(heap: &Heap, log: CurrentRealmLog) -> HeapCurrentRealmLogGuard {
    let heap_ptr = heap as *const Heap as usize;
    current_realm_logs().lock().unwrap().insert(heap_ptr, log);
    HeapCurrentRealmLogGuard { heap_ptr }
  }

  struct BindingsHost {
    webidl_dispatch_count: usize,
  }

  impl BindingsHost {
    fn new() -> Self {
      Self {
        webidl_dispatch_count: 0,
      }
    }
  }

  impl WebIdlBindingsHost for BindingsHost {
    fn call_operation(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _receiver: Option<Value>,
      _interface: &'static str,
      _operation: &'static str,
      _overload: usize,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      self.webidl_dispatch_count += 1;
      Ok(Value::Undefined)
    }

    fn call_constructor(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _interface: &'static str,
      _overload: usize,
      _args: &[Value],
      _new_target: Value,
    ) -> Result<Value, VmError> {
      Err(VmError::Unimplemented(
        "constructor dispatch not implemented for BindingsHost",
      ))
    }
  }

  struct Host {
    host_ctx: HostCtx,
    bindings_host: BindingsHost,
    window: WindowRealm,
  }

  impl Host {
    fn new() -> Self {
      let window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).unwrap();
      Self {
        host_ctx: HostCtx {
          hook_downcast_count: 0,
        },
        bindings_host: BindingsHost::new(),
        window,
      }
    }

    fn new_with_js_execution_options(js_execution_options: JsExecutionOptions) -> Self {
      let window = WindowRealm::new_with_js_execution_options(
        WindowRealmConfig::new("https://example.invalid/"),
        js_execution_options,
      )
      .unwrap();
      Self {
        host_ctx: HostCtx {
          hook_downcast_count: 0,
        },
        bindings_host: BindingsHost::new(),
        window,
      }
    }
  }

  impl WindowRealmHost for Host {
    fn vm_host_and_window_realm(&mut self) -> (&mut dyn VmHost, &mut WindowRealm) {
      let Host { host_ctx, window, .. } = self;
      (host_ctx, window)
    }

    fn webidl_bindings_host(&mut self) -> Option<&mut dyn WebIdlBindingsHost> {
      Some(&mut self.bindings_host)
    }
  }

  fn cb_webidl_dispatch(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let host = host_from_hooks(hooks)?;
    let _ = host.call_operation(vm, scope, None, "TestInterface", "testOp", 0, &[])?;
    Ok(Value::Undefined)
  }

  fn cb_timeout_calls_webidl_and_enqueues_jobs(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let Value::Object(global) = get_prop(scope, callee, CALLBACK_GLOBAL_KEY) else {
      return Ok(Value::Undefined);
    };
    let binding = get_prop(scope, global, "__webidl_dispatch");
    scope.push_root(binding)?;
    let Value::Object(binding_obj) = binding else {
      return Err(VmError::Unimplemented(
        "__webidl_dispatch was not a callable object",
      ));
    };
    vm.call_with_host_and_hooks(host, scope, hooks, binding, Value::Undefined, &[])?;

    // Enqueue a Promise job that calls the generated binding wrapper, then enqueues another job
    // that does the same. This regression test ensures hooks created for nested Promise jobs still
    // expose the WebIDL bindings host slot via `VmHostHooks::as_any_mut`.
    let binding_value = Value::Object(binding_obj);
    hooks.host_enqueue_promise_job(
      vm_js::Job::new(vm_js::JobKind::Promise, move |ctx, hooks| {
        ctx.call(hooks, binding_value, Value::Undefined, &[])?;
        hooks.host_enqueue_promise_job(
          vm_js::Job::new(vm_js::JobKind::Promise, move |ctx, hooks| {
            ctx.call(hooks, binding_value, Value::Undefined, &[])?;
            Ok(())
          }),
          None,
        );
        Ok(())
      }),
      None,
    );

    Ok(Value::Undefined)
  }

  fn get_prop(scope: &mut Scope<'_>, obj: vm_js::GcObject, name: &str) -> Value {
    let key_s = scope.alloc_string(name).unwrap();
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)
      .unwrap()
      .unwrap_or(Value::Undefined)
  }

  fn set_prop(scope: &mut Scope<'_>, obj: vm_js::GcObject, name: &str, value: Value) {
    let key_s = scope.alloc_string(name).unwrap();
    scope
      .push_root(Value::String(key_s))
      .expect("push root key string");
    let key = PropertyKey::from_string(key_s);
    scope.define_property(obj, key, data_desc(value)).unwrap();
  }

  fn init_log(scope: &mut Scope<'_>, global: vm_js::GcObject) {
    let log_obj = scope.alloc_object().unwrap();
    scope
      .push_root(Value::Object(log_obj))
      .expect("push root log object");
    set_prop(scope, global, "__log_obj", Value::Object(log_obj));
    set_prop(scope, global, "__log_len", Value::Number(0.0));
  }

  fn push_log(scope: &mut Scope<'_>, global: vm_js::GcObject, label: &str) {
    let log_obj = match get_prop(scope, global, "__log_obj") {
      Value::Object(o) => o,
      _ => panic!("missing __log_obj"),
    };
    let len = match get_prop(scope, global, "__log_len") {
      Value::Number(n) => n as u32,
      _ => panic!("missing __log_len"),
    };
    let key_s = scope.alloc_string(&len.to_string()).unwrap();
    scope
      .push_root(Value::String(key_s))
      .expect("push root log key");
    let key = PropertyKey::from_string(key_s);
    let label_s = scope.alloc_string(label).unwrap();
    scope
      .push_root(Value::String(label_s))
      .expect("push root log label");
    scope
      .define_property(log_obj, key, data_desc(Value::String(label_s)))
      .unwrap();
    set_prop(scope, global, "__log_len", Value::Number((len + 1) as f64));
  }

  fn read_log(heap: &mut Heap, realm: &Realm) -> Vec<String> {
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope
      .push_root(Value::Object(global))
      .expect("push root global");
    let log_obj = match get_prop(&mut scope, global, "__log_obj") {
      Value::Object(o) => o,
      _ => panic!("missing __log_obj"),
    };
    let len = match get_prop(&mut scope, global, "__log_len") {
      Value::Number(n) => n as u32,
      _ => panic!("missing __log_len"),
    };
    let mut out = Vec::new();
    for i in 0..len {
      let key_s = scope.alloc_string(&i.to_string()).unwrap();
      let key = PropertyKey::from_string(key_s);
      let v = scope
        .heap()
        .object_get_own_data_property_value(log_obj, &key)
        .unwrap()
        .unwrap_or(Value::Undefined);
      let Value::String(s) = v else {
        panic!("log entry {i} not a string: {v:?}");
      };
      out.push(scope.heap().get_string(s).unwrap().to_utf8_lossy());
    }
    out
  }

  fn make_callback(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    global: vm_js::GcObject,
    name: &str,
    native: vm_js::NativeCall,
  ) -> vm_js::GcObject {
    let id = vm
      .register_native_call(native)
      .expect("register_native_call");
    let name_s = scope.alloc_string(name).unwrap();
    scope.push_root(Value::String(name_s)).unwrap();
    scope.push_root(Value::Object(global)).unwrap();
    let func = scope.alloc_native_function(id, None, name_s, 0).unwrap();
    scope.push_root(Value::Object(func)).unwrap();
    set_prop(scope, func, CALLBACK_GLOBAL_KEY, Value::Object(global));
    func
  }

  fn record_callback_call(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    JOB_CALLBACK_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(Value::Undefined)
  }

  fn record_current_realm_native(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let heap_ptr = scope.heap() as *const Heap as usize;
    let log = current_realm_logs().lock().unwrap().get(&heap_ptr).cloned();
    if let Some(log) = log {
      *log.lock().unwrap() = Some(vm.current_realm());
    }
    Ok(Value::Undefined)
  }

  fn cb_enqueue_promise_job(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let heap_ptr = scope.heap() as *const Heap as usize;
    record_promise_job_log(heap_ptr, "timeout");

    let heap_ptr_for_job = heap_ptr;
    hooks.host_enqueue_promise_job(
      vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
        record_promise_job_log(heap_ptr_for_job, "job");
        Ok(())
      }),
      None,
    );

    record_promise_job_log(heap_ptr, "timeout_end");
    Ok(Value::Undefined)
  }

  fn cb_record_next(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let heap_ptr = scope.heap() as *const Heap as usize;
    record_promise_job_log(heap_ptr, "next");
    Ok(Value::Undefined)
  }

  fn cb_push_t(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let Value::Object(global) = get_prop(scope, callee, CALLBACK_GLOBAL_KEY) else {
      return Ok(Value::Undefined);
    };
    push_log(scope, global, "t");
    Ok(Value::Undefined)
  }

  fn cb_push_m(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    callee: vm_js::GcObject,
    this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let Value::Object(global) = get_prop(scope, callee, CALLBACK_GLOBAL_KEY) else {
      return Ok(Value::Undefined);
    };
    set_prop(
      scope,
      global,
      "__microtask_this_is_undefined",
      Value::Bool(matches!(this, Value::Undefined)),
    );
    push_log(scope, global, "m");
    Ok(Value::Undefined)
  }

  fn cb_capture_args(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    callee: vm_js::GcObject,
    _this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let Value::Object(global) = get_prop(scope, callee, CALLBACK_GLOBAL_KEY) else {
      return Ok(Value::Undefined);
    };
    if let Some(v) = args.get(0).copied() {
      set_prop(scope, global, "__arg0", v);
    }
    if let Some(v) = args.get(1).copied() {
      set_prop(scope, global, "__arg1", v);
    }
    Ok(Value::Undefined)
  }

  fn cb_record_vm_budget(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let Value::Object(global) = get_prop(scope, callee, CALLBACK_GLOBAL_KEY) else {
      return Ok(Value::Undefined);
    };

    let budget = vm.budget();
    set_prop(
      scope,
      global,
      "__budget_fuel_is_some",
      Value::Bool(budget.fuel.is_some()),
    );
    set_prop(
      scope,
      global,
      "__budget_deadline_is_some",
      Value::Bool(budget.deadline.is_some()),
    );
    let fuel_value = budget
      .fuel
      .map(|fuel| Value::Number(fuel as f64))
      .unwrap_or(Value::Number(-1.0));
    set_prop(scope, global, "__budget_fuel_value", fuel_value);

    Ok(Value::Undefined)
  }

  fn cb_noop(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Ok(Value::Undefined)
  }

  fn cb_check_hooks_downcast(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    // Prefer the explicit `VmHost` context when it is the right type (e.g. calls routed through
    // `call_with_host_and_hooks`). Some VM entry points still pass a dummy `VmHost` (`()`), so we
    // fall back to downcasting through `VmHostHooks::as_any_mut`.
    if let Some(host_ctx) = host.as_any_mut().downcast_mut::<HostCtx>() {
      host_ctx.hook_downcast_count += 1;
      return Ok(Value::Undefined);
    }

    let Some(any) = hooks.as_any_mut() else {
      return Err(VmError::Unimplemented(
        "VmHostHooks::as_any_mut returned None",
      ));
    };
    let Some(payload) = any.downcast_mut::<VmJsHostHooksPayload>() else {
      return Err(VmError::Unimplemented(
        "VmHostHooks::as_any_mut did not downcast to VmJsHostHooksPayload",
      ));
    };
    let Some(vm_host) = payload.vm_host_mut() else {
      return Err(VmError::Unimplemented(
        "VmJsHostHooksPayload did not contain a VmHost pointer",
      ));
    };
    let Some(host_ctx) = vm_host.as_any_mut().downcast_mut::<HostCtx>() else {
      return Err(VmError::Unimplemented(
        "VmJsHostHooksPayload VmHost did not downcast to HostCtx",
      ));
    };
    host_ctx.hook_downcast_count += 1;
    Ok(Value::Undefined)
  }

  fn cb_dispatch_via_webidl_host(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let host = webidl_vm_js::host_from_hooks(hooks)?;
    host.call_operation(
      vm,
      scope,
      None,
      "Test",
      "cb_dispatch_via_webidl_host",
      0,
      args,
    )
  }

  fn cb_interval_tick(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let Value::Object(global) = get_prop(scope, callee, CALLBACK_GLOBAL_KEY) else {
      return Ok(Value::Undefined);
    };

    let count = match get_prop(scope, global, "__interval_count") {
      Value::Number(n) => n as u32,
      _ => 0,
    };
    let new_count = count + 1;
    set_prop(
      scope,
      global,
      "__interval_count",
      Value::Number(new_count as f64),
    );

    if new_count == 3 {
      // Call clearInterval(id).
      let id = match get_prop(scope, global, "__interval_id") {
        Value::Number(n) => Value::Number(n),
        _ => Value::Number(0.0),
      };
      let clear_interval = get_prop(scope, global, "clearInterval");
      let _ = vm.call_with_host_and_hooks(host, scope, hooks, clear_interval, Value::Undefined, &[id])?;
    }

    Ok(Value::Undefined)
  }

  #[test]
  fn set_timeout_rejects_non_callable_callback() -> Result<(), VmError> {
    let mut host = Host::new();

    let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
    install_window_timers_bindings::<Host>(vm, realm, heap)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    let set_timeout = get_prop(&mut scope, global, "setTimeout");
    let Value::Object(set_timeout_func) = set_timeout else {
      panic!("expected setTimeout to be a function object");
    };
    let not_a_function = scope.alloc_object()?;
    scope.push_root(Value::Object(not_a_function))?;

    // `setTimeout` is a host-created native function; verify it inherits from `Function.prototype`
    // by invoking it through `Function.prototype.call`.
    let call_key_s = scope.alloc_string("call")?;
    scope.push_root(Value::String(call_key_s))?;
    let call_key = PropertyKey::from_string(call_key_s);
    let call = vm.get(&mut scope, set_timeout_func, call_key)?;
    let err = vm.call_without_host(
      &mut scope,
      call,
      Value::Object(set_timeout_func),
      &[
        Value::Undefined,
        Value::Object(not_a_function),
        Value::Number(0.0),
      ],
    );

    let err = err.expect_err("expected TypeError for non-callable callback");
    assert_type_error_contains(scope.heap_mut(), err, SET_TIMEOUT_NOT_CALLABLE_ERROR);

    Ok(())
  }

  #[test]
  fn set_timeout_rejects_invalid_this() -> Result<(), VmError> {
    fn cb_noop(
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _host: &mut dyn VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: vm_js::GcObject,
      _this: Value,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      Ok(Value::Undefined)
    }

    let mut host = Host::new();

    let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
    install_window_timers_bindings::<Host>(vm, realm, heap)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    let set_timeout = get_prop(&mut scope, global, "setTimeout");
    let Value::Object(set_timeout_func) = set_timeout else {
      panic!("expected setTimeout to be a function object");
    };

    let bad_this = scope.alloc_object()?;
    scope.push_root(Value::Object(bad_this))?;
    let timeout_cb = make_callback(vm, &mut scope, global, "timeout_cb", cb_noop);

    // Invoke through `Function.prototype.call` so we can control `this`.
    let call_key_s = scope.alloc_string("call")?;
    scope.push_root(Value::String(call_key_s))?;
    let call_key = PropertyKey::from_string(call_key_s);
    let call = vm.get(&mut scope, set_timeout_func, call_key)?;
    let err = vm.call_without_host(
      &mut scope,
      call,
      Value::Object(set_timeout_func),
      &[
        Value::Object(bad_this),
        Value::Object(timeout_cb),
        Value::Number(0.0),
      ],
    );

    let err = err.expect_err("expected TypeError for invalid this");
    assert_type_error_contains(
      scope.heap_mut(),
      err,
      "setTimeout called with invalid this value",
    );
    Ok(())
  }

  #[test]
  fn set_timeout_rejects_symbol_delay() -> Result<(), VmError> {
    let mut host = Host::new();

    let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
    install_window_timers_bindings::<Host>(vm, realm, heap)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    let set_timeout = get_prop(&mut scope, global, "setTimeout");
    let timeout_cb = make_callback(vm, &mut scope, global, "timeout_cb", cb_push_t);

    let sym = scope.alloc_symbol(Some("delay"))?;
    scope.push_root(Value::Symbol(sym))?;

    let err = vm.call_without_host(
      &mut scope,
      set_timeout,
      Value::Undefined,
      &[Value::Object(timeout_cb), Value::Symbol(sym)],
    );

    let err = err.expect_err("expected TypeError for Symbol delay");
    assert_type_error_contains(scope.heap_mut(), err, SYMBOL_TO_NUMBER_ERROR);
    Ok(())
  }

  #[test]
  fn clear_timeout_rejects_symbol_handle() -> Result<(), VmError> {
    let mut host = Host::new();

    let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
    install_window_timers_bindings::<Host>(vm, realm, heap)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    let clear_timeout = get_prop(&mut scope, global, "clearTimeout");
    let sym = scope.alloc_symbol(Some("id"))?;
    scope.push_root(Value::Symbol(sym))?;

    let err = vm.call_without_host(
      &mut scope,
      clear_timeout,
      Value::Undefined,
      &[Value::Symbol(sym)],
    );

    let err = err.expect_err("expected TypeError for Symbol handle");
    assert_type_error_contains(scope.heap_mut(), err, SYMBOL_TO_NUMBER_ERROR);
    Ok(())
  }

  #[test]
  fn timer_callbacks_apply_vm_budget_from_js_execution_options() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut js_options = JsExecutionOptions::default();
    // Use a small fuel budget so we can detect whether the timer callback picked up the configured
    // value (vm-js decrements fuel on ticks, so we only assert it stays <= this maximum).
    js_options.max_instruction_count = Some(1_000);
    // Disable wall-time budgeting so the timer callback budget has no deadline unless a render
    // deadline is active.
    js_options.event_loop_run_limits.max_wall_time = None;

    let window = WindowRealm::new_with_js_execution_options(
      WindowRealmConfig::new("https://example.invalid/"),
      js_options,
    )
    .unwrap();
    let mut host = Host {
      host_ctx: HostCtx {
        hook_downcast_count: 0,
      },
      bindings_host: BindingsHost::new(),
      window,
    };

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
      let mut scope = heap.scope();
      let global = realm.global_object();
      set_prop(&mut scope, global, "__budget_fuel_is_some", Value::Bool(false));
      set_prop(&mut scope, global, "__budget_deadline_is_some", Value::Bool(false));
      set_prop(&mut scope, global, "__budget_fuel_value", Value::Number(-1.0));
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");
        let timeout_cb =
          make_callback(vm, &mut scope, global, "timeout_cb", cb_record_vm_budget);
        vm.call_without_host(
          &mut scope,
          set_timeout,
          Value::Undefined,
          &[Value::Object(timeout_cb), Value::Number(0.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    let (fuel_is_some, deadline_is_some, fuel_value) = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      (
        get_prop(&mut scope, global, "__budget_fuel_is_some"),
        get_prop(&mut scope, global, "__budget_deadline_is_some"),
        get_prop(&mut scope, global, "__budget_fuel_value"),
      )
    };

    assert_eq!(fuel_is_some, Value::Bool(true));
    assert_eq!(deadline_is_some, Value::Bool(false));

    let Value::Number(n) = fuel_value else {
      panic!("expected number fuel value, got {fuel_value:?}");
    };
    assert!(n >= 0.0 && n <= 1_000.0, "fuel={n}");

    Ok(())
  }

  #[test]
  fn normalize_delay_ms_parses_ecmascript_string_numeric_literals() -> Result<(), VmError> {
    let mut host = Host::new();
    let (_vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();

    for (input, expected) in [
      ("0x10", 16u64),
      ("0b101", 5),
      ("0o10", 8),
      ("\u{FEFF}1\u{FEFF}", 1),
    ] {
      let s = scope.alloc_string(input)?;
      scope.push_root(Value::String(s))?;
      let ms = normalize_delay_ms(scope.heap_mut(), Value::String(s))?;
      assert_eq!(ms, expected, "input={input:?}");
    }

    Ok(())
  }

  #[test]
  fn ordering_timeout_after_microtask() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();

      let mut scope = heap.scope();
      let global = realm.global_object();
      init_log(&mut scope, global);
      set_prop(
        &mut scope,
        global,
        "__microtask_this_is_undefined",
        Value::Bool(false),
      );
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();

      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        let mut scope = heap.scope();

        let set_timeout = get_prop(&mut scope, global, "setTimeout");
        let queue_microtask = get_prop(&mut scope, global, "queueMicrotask");

        let timeout_cb = make_callback(vm, &mut scope, global, "timeout_cb", cb_push_t);
        let micro_cb = make_callback(vm, &mut scope, global, "micro_cb", cb_push_m);

        vm.call_without_host(
          &mut scope,
          set_timeout,
          Value::Undefined,
          &[Value::Object(timeout_cb), Value::Number(0.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        vm.call_without_host(
          &mut scope,
          queue_microtask,
          Value::Undefined,
          &[Value::Object(micro_cb)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;

        push_log(&mut scope, global, "sync");
        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    let log = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_log(heap, realm)
    };
    assert_eq!(log, vec!["sync", "m", "t"]);

    let microtask_this_is_undefined = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      match get_prop(&mut scope, global, "__microtask_this_is_undefined") {
        Value::Bool(b) => b,
        other => panic!("expected bool, got {other:?}"),
      }
    };
    assert!(microtask_this_is_undefined);
    Ok(())
  }

  #[test]
  fn scheduled_microtask_respects_max_instruction_count() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut opts = JsExecutionOptions::default();
    // Give the scheduling task enough fuel to enqueue the callback, while still ensuring the
    // callback itself will terminate once it enters the infinite loop.
    //
    // Keep this fairly small so the test runs quickly (the callback is an infinite loop).
    opts.max_instruction_count = Some(500);
    // Keep wall-time generous so we deterministically hit OutOfFuel first.
    opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));
    let mut host = Host::new_with_js_execution_options(opts);

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();

      let mut scope = heap.scope();
      let global = realm.global_object();
      set_prop(&mut scope, global, "__ran", Value::Bool(false));
    }

    // Schedule a microtask that would set `__ran = true` if it were executed.
    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        host
          .window
          .exec_script(
            "queueMicrotask(() => {\n\
               while (true) {}\n\
               globalThis.__ran = true;\n\
             });",
          )
          .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        Ok(())
      })
    })?;

    let err = event_loop
      .run_until_idle(&mut host, RunLimits::unbounded())
      .expect_err("expected microtask to terminate due to instruction budget");
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
      msg.contains("out of fuel"),
      "expected OutOfFuel termination, got: {msg}"
    );

    let ran = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      match get_prop(&mut scope, global, "__ran") {
        Value::Bool(b) => b,
        other => panic!("expected bool, got {other:?}"),
      }
    };
    assert!(!ran, "microtask callback ran despite fuel=0");

    Ok(())
  }

  #[test]
  fn scheduled_timeout_respects_max_instruction_count() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut opts = JsExecutionOptions::default();
    // Keep this small so the test runs quickly (the timeout callback is an infinite loop).
    opts.max_instruction_count = Some(500);
    // Keep wall-time generous so we deterministically hit OutOfFuel first.
    opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));
    let mut host = Host::new_with_js_execution_options(opts);

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();

      let mut scope = heap.scope();
      let global = realm.global_object();
      set_prop(&mut scope, global, "__ran", Value::Bool(false));
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        host
          .window
          .exec_script(
            "setTimeout(() => {\n\
               while (true) {}\n\
               globalThis.__ran = true;\n\
             }, 0);",
          )
          .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        Ok(())
      })
    })?;

    let err = event_loop
      .run_until_idle(&mut host, RunLimits::unbounded())
      .expect_err("expected timeout callback to terminate due to instruction budget");
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
      msg.contains("out of fuel"),
      "expected OutOfFuel termination, got: {msg}"
    );

    let ran = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      match get_prop(&mut scope, global, "__ran") {
        Value::Bool(b) => b,
        other => panic!("expected bool, got {other:?}"),
      }
    };
    assert!(!ran, "timeout callback ran despite fuel budget");

    Ok(())
  }

  #[test]
  fn scheduled_promise_job_respects_max_instruction_count() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut opts = JsExecutionOptions::default();
    // Keep this small so the test runs quickly (the promise job callback is an infinite loop).
    opts.max_instruction_count = Some(500);
    // Keep wall-time generous so we deterministically hit OutOfFuel first.
    opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));
    let mut host = Host::new_with_js_execution_options(opts);

    // Pre-set the marker so the scheduling script doesn't need to do any extra work under the fuel
    // budget.
    {
      let (_vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      set_prop(&mut scope, global, "__ran", Value::Bool(false));
    }

    // Enqueue a Promise job whose callback is an infinite loop.
    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        let (host_ctx, window_realm) = host.vm_host_and_window_realm();
        let mut hooks = VmJsEventLoopHooks::<Host>::new(&mut *host_ctx);
        window_realm.reset_interrupt();

        let result = window_realm.exec_script_with_hooks(
          &mut hooks,
          "Promise.resolve().then(() => {\n\
             while (true) {}\n\
             globalThis.__ran = true;\n\
           });",
        );

        if let Some(err) = hooks.finish(window_realm.heap_mut()) {
          return Err(err);
        }

        result
          .map(|_| ())
          .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))
      })
    })?;

    let err = event_loop
      .run_until_idle(&mut host, RunLimits::unbounded())
      .expect_err("expected Promise job to terminate due to instruction budget");
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
      msg.contains("out of fuel"),
      "expected OutOfFuel termination, got: {msg}"
    );

    let ran = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      match get_prop(&mut scope, global, "__ran") {
        Value::Bool(b) => b,
        other => panic!("expected bool, got {other:?}"),
      }
    };
    assert!(!ran, "Promise job callback ran despite fuel budget");

    Ok(())
  }

  #[test]
  fn hooks_as_any_mut_downcasts_to_host_for_script_and_tasks() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();

      let mut scope = heap.scope();
      let global = realm.global_object();
      scope
        .push_root(Value::Object(global))
        .expect("push root global");

      let check = make_callback(
        vm,
        &mut scope,
        global,
        "__check_host_hooks",
        cb_check_hooks_downcast,
      );
      scope
        .push_root(Value::Object(check))
        .expect("push root __check_host_hooks");
      set_prop(&mut scope, global, "__check_host_hooks", Value::Object(check));
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
        let (_, window_realm) = host.vm_host_and_window_realm();
        window_realm.reset_interrupt();

        let result = window_realm.exec_script_with_hooks(
          &mut hooks,
          "__check_host_hooks(); queueMicrotask(__check_host_hooks); setTimeout(__check_host_hooks, 0);",
        );

        if let Some(err) = hooks.finish(window_realm.heap_mut()) {
          return Err(err);
        }

        result
          .map(|_| ())
          .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.host_ctx.hook_downcast_count, 3);
    Ok(())
  }

  #[test]
  fn webidl_bindings_host_is_available_via_hooks_slot_for_script_and_tasks() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();

      let mut scope = heap.scope();
      let global = realm.global_object();
      scope
        .push_root(Value::Object(global))
        .expect("push root global");

      let check = make_callback(
        vm,
        &mut scope,
        global,
        "__check_host_hooks",
        cb_dispatch_via_webidl_host,
      );
      scope
        .push_root(Value::Object(check))
        .expect("push root __check_host_hooks");
      set_prop(&mut scope, global, "__check_host_hooks", Value::Object(check));
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
        let (_, window_realm) = host.vm_host_and_window_realm();
        window_realm.reset_interrupt();

        let result = window_realm.exec_script_with_hooks(
          &mut hooks,
          "__check_host_hooks(); queueMicrotask(__check_host_hooks); setTimeout(__check_host_hooks, 0);",
        );

        if let Some(err) = hooks.finish(window_realm.heap_mut()) {
          return Err(err);
        }

        result
          .map(|_| ())
          .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.bindings_host.webidl_dispatch_count, 3);
    Ok(())
  }

  #[test]
  fn set_timeout_can_be_called_as_identifier_in_scripts() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        host
          .window
          .exec_script(
            "globalThis.__timeout_fired = false;\n\
             setTimeout(function(){ globalThis.__timeout_fired = true; }, 0);\n",
          )
          .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    let fired = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      get_prop(&mut scope, global, "__timeout_fired")
    };
    assert_eq!(fired, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn timer_and_promise_jobs_invoke_callbacks_with_embedder_vm_host(
  ) -> crate::error::Result<()> {
    #[derive(Default)]
    struct CounterVmHost {
      count: usize,
    }

    struct HostWithVmHost {
      window: WindowRealm,
      vm_host: CounterVmHost,
    }

    impl HostWithVmHost {
      fn new() -> Self {
        let window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).unwrap();
        Self {
          window,
          vm_host: CounterVmHost::default(),
        }
      }
    }

    impl WindowRealmHost for HostWithVmHost {
      fn vm_host_and_window_realm(&mut self) -> (&mut dyn VmHost, &mut WindowRealm) {
        (&mut self.vm_host, &mut self.window)
      }
    }

    impl WebIdlBindingsHost for HostWithVmHost {
      fn call_operation(
        &mut self,
        _vm: &mut Vm,
        _scope: &mut Scope<'_>,
        _receiver: Option<Value>,
        _interface: &'static str,
        _operation: &'static str,
        _overload: usize,
        _args: &[Value],
      ) -> Result<Value, VmError> {
        Ok(Value::Undefined)
      }

      fn call_constructor(
        &mut self,
        _vm: &mut Vm,
        _scope: &mut Scope<'_>,
        _interface: &'static str,
        _overload: usize,
        _args: &[Value],
        _new_target: Value,
      ) -> Result<Value, VmError> {
        Ok(Value::Undefined)
      }
    }

    fn bump_counter_native(
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      host: &mut dyn VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: vm_js::GcObject,
      _this: Value,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      let Some(counter) = host.as_any_mut().downcast_mut::<CounterVmHost>() else {
        return Err(VmError::TypeError(
          "expected timer/Promise job callback to receive embedder VmHost",
        ));
      };
      counter.count += 1;
      Ok(Value::Undefined)
    }

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<HostWithVmHost>::with_clock(clock);
    let mut host = HostWithVmHost::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<HostWithVmHost>(vm, realm, heap).unwrap();

      let mut scope = heap.scope();
      let global = realm.global_object();
      scope
        .push_root(Value::Object(global))
        .expect("push root global");
      let cb = make_callback(vm, &mut scope, global, "__bump_counter", bump_counter_native);
      set_prop(&mut scope, global, "__bump_counter", Value::Object(cb));
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        let mut hooks = VmJsEventLoopHooks::<HostWithVmHost>::new_with_host(host);
        let (vm_host, window) = host.vm_host_and_window_realm();
        window
          .exec_script_with_host_and_hooks(
            vm_host,
            &mut hooks,
            "setTimeout(globalThis.__bump_counter, 0);\n\
             Promise.resolve().then(globalThis.__bump_counter);\n",
          )
          .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        if let Some(err) = hooks.finish(window.heap_mut()) {
          return Err(err);
        }
        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.vm_host.count, 2);
    Ok(())
  }

  #[test]
  fn set_timeout_parses_hex_delay_string() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock.clone());
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();

      let mut scope = heap.scope();
      let global = realm.global_object();
      init_log(&mut scope, global);
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");
        let timeout_cb = make_callback(vm, &mut scope, global, "timeout_cb", cb_push_t);

        let delay_s = scope.alloc_string("0x10").unwrap();
        scope.push_root(Value::String(delay_s)).unwrap();
        vm.call_without_host(
          &mut scope,
          set_timeout,
          Value::Undefined,
          &[Value::Object(timeout_cb), Value::String(delay_s)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    let log = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_log(heap, realm)
    };
    assert!(
      log.is_empty(),
      "expected no timeout before advancing clock, got {log:?}"
    );

    clock.advance(Duration::from_millis(15));
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    let log = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_log(heap, realm)
    };
    assert!(log.is_empty(), "expected no timeout at 15ms, got {log:?}");

    clock.advance(Duration::from_millis(1));
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    let log = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_log(heap, realm)
    };
    assert_eq!(log, vec!["t".to_string()]);

    Ok(())
  }

  #[test]
  fn promise_jobs_enqueued_by_timer_callbacks_run_in_microtask_checkpoint(
  ) -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    let job_log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let _log_guard = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
      install_promise_job_log(heap, Arc::clone(&job_log))
    };

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();

      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");

        let timeout_cb =
          make_callback(vm, &mut scope, global, "timeout_cb", cb_enqueue_promise_job);
        let next_cb = make_callback(vm, &mut scope, global, "next_cb", cb_record_next);

        vm.call_without_host(
          &mut scope,
          set_timeout,
          Value::Undefined,
          &[Value::Object(timeout_cb), Value::Number(0.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;

        vm.call_without_host(
          &mut scope,
          set_timeout,
          Value::Undefined,
          &[Value::Object(next_cb), Value::Number(0.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    assert_eq!(
      &*job_log.lock().unwrap(),
      &["timeout", "timeout_end", "job", "next"]
    );
    Ok(())
  }

  #[test]
  fn webidl_host_slot_available_in_timer_and_nested_promise_jobs() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();

      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");

        let webidl_dispatch = make_callback(vm, &mut scope, global, "__webidl_dispatch", cb_webidl_dispatch);
        scope
          .push_root(Value::Object(webidl_dispatch))
          .expect("push root __webidl_dispatch");
        set_prop(
          &mut scope,
          global,
          "__webidl_dispatch",
          Value::Object(webidl_dispatch),
        );

        let timeout_cb = make_callback(
          vm,
          &mut scope,
          global,
          "timeout_cb",
          cb_timeout_calls_webidl_and_enqueues_jobs,
        );
        scope
          .push_root(Value::Object(timeout_cb))
          .expect("push root timeout_cb");

        vm.call_without_host(
          &mut scope,
          set_timeout,
          Value::Undefined,
          &[Value::Object(timeout_cb), Value::Number(0.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;

        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.bindings_host.webidl_dispatch_count, 3);
    Ok(())
  }

  #[test]
  fn cancellation_timeout() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
      let mut scope = heap.scope();
      let global = realm.global_object();
      init_log(&mut scope, global);
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");
        let clear_timeout = get_prop(&mut scope, global, "clearTimeout");
        let timeout_cb = make_callback(vm, &mut scope, global, "timeout_cb", cb_push_t);
        let id = vm
          .call_without_host(
            &mut scope,
            set_timeout,
            Value::Undefined,
            &[Value::Object(timeout_cb), Value::Number(0.0)],
          )
          .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        let _ = vm
          .call_without_host(&mut scope, clear_timeout, Value::Undefined, &[id])
          .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    let log = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_log(heap, realm)
    };
    assert!(log.is_empty());
    Ok(())
  }

  #[test]
  fn interval_repeats_and_can_be_cancelled() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
      let mut scope = heap.scope();
      let global = realm.global_object();
      init_log(&mut scope, global);
      set_prop(&mut scope, global, "__interval_count", Value::Number(0.0));
    }

    // Schedule the interval from Rust (like a script would).
    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      with_event_loop(&mut event_loop, || -> Result<(), crate::error::Error> {
        let mut scope = heap.scope();
        let set_interval = get_prop(&mut scope, global, "setInterval");
        let interval_cb = make_callback(vm, &mut scope, global, "interval_cb", cb_interval_tick);
        let id = vm
          .call_without_host(
            &mut scope,
            set_interval,
            Value::Undefined,
            &[Value::Object(interval_cb), Value::Number(0.0)],
          )
          .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        set_prop(&mut scope, global, "__interval_id", id);
        Ok(())
      })?;
    }

    assert_eq!(
      event_loop.run_until_idle(
        &mut host,
        RunLimits {
          max_tasks: 10,
          max_microtasks: 100,
          max_wall_time: None,
        },
      )?,
      RunUntilIdleOutcome::Idle
    );

    let count = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      match get_prop(&mut scope, global, "__interval_count") {
        Value::Number(n) => n as u32,
        other => panic!("expected number, got {other:?}"),
      }
    };
    assert_eq!(count, 3);
    Ok(())
  }

  #[test]
  fn timeout_passes_additional_args_to_callback() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
      let mut scope = heap.scope();
      let global = realm.global_object();
      set_prop(&mut scope, global, "__arg0", Value::Undefined);
      set_prop(&mut scope, global, "__arg1", Value::Undefined);
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");
        let cb = make_callback(vm, &mut scope, global, "cb", cb_capture_args);
        let x_s = scope.alloc_string("x").unwrap();
        scope
          .push_root(Value::String(x_s))
          .expect("push root arg string");
        vm.call_without_host(
          &mut scope,
          set_timeout,
          Value::Undefined,
          &[
            Value::Object(cb),
            Value::Number(0.0),
            Value::Number(1.0),
            Value::String(x_s),
          ],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    let (arg0, arg1) = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      (
        get_prop(&mut scope, global, "__arg0"),
        get_prop(&mut scope, global, "__arg1"),
      )
    };
    assert_eq!(arg0, Value::Number(1.0));
    match arg1 {
      Value::String(s) => {
        let (_, _, heap) = host.window.vm_realm_and_heap_mut();
        assert_eq!(heap.get_string(s).unwrap().to_utf8_lossy(), "x");
      }
      other => panic!("expected string, got {other:?}"),
    }

    Ok(())
  }

  #[test]
  fn string_handlers_throw_type_error() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
    }

    let err = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      with_event_loop(&mut event_loop, || {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");
        let handler_s = scope.alloc_string("alert(1)").unwrap();
        scope
          .push_root(Value::String(handler_s))
          .expect("push root handler string");
        vm.call_without_host(
          &mut scope,
          set_timeout,
          Value::Undefined,
          &[Value::String(handler_s), Value::Number(0.0)],
        )
      })
      .expect_err("string handlers should be rejected")
    };

    assert_type_error_contains(host.window.heap_mut(), err, "string handlers");

    Ok(())
  }

  #[test]
  fn queue_microtask_rejects_string_callback() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
    }

    let err = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      with_event_loop(&mut event_loop, || {
        let mut scope = heap.scope();
        let queue_microtask = get_prop(&mut scope, global, "queueMicrotask");
        let handler_s = scope.alloc_string("alert(1)").unwrap();
        scope
          .push_root(Value::String(handler_s))
          .expect("push root handler string");
        vm.call_without_host(
          &mut scope,
          queue_microtask,
          Value::Undefined,
          &[Value::String(handler_s)],
        )
      })
      .expect_err("string callbacks should be rejected")
    };

    assert_type_error_contains(host.window.heap_mut(), err, "string callbacks");

    Ok(())
  }

  #[test]
  fn queue_microtask_invokes_callback_with_undefined_this() -> crate::error::Result<()> {
    fn cb_record_this_is_undefined(
      _vm: &mut Vm,
      scope: &mut Scope<'_>,
      _host: &mut dyn VmHost,
      _hooks: &mut dyn VmHostHooks,
      callee: vm_js::GcObject,
      this: Value,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      let Value::Object(global) = get_prop(scope, callee, CALLBACK_GLOBAL_KEY) else {
        return Ok(Value::Undefined);
      };
      let is_undefined = matches!(this, Value::Undefined);
      set_prop(
        scope,
        global,
        "__microtask_this_is_undefined",
        Value::Bool(is_undefined),
      );
      Ok(Value::Undefined)
    }

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
      let mut scope = heap.scope();
      let global = realm.global_object();
      set_prop(
        &mut scope,
        global,
        "__microtask_this_is_undefined",
        Value::Bool(false),
      );
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        let mut scope = heap.scope();
        let queue_microtask = get_prop(&mut scope, global, "queueMicrotask");
        let cb = make_callback(
          vm,
          &mut scope,
          global,
          "micro_cb",
          cb_record_this_is_undefined,
        );
        vm.call_without_host(
          &mut scope,
          queue_microtask,
          Value::Undefined,
          &[Value::Object(cb)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    let flag = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      get_prop(&mut scope, global, "__microtask_this_is_undefined")
    };
    assert_eq!(flag, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn non_callable_queue_microtask_handlers_throw_type_error() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
    }

    let err = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      with_event_loop(&mut event_loop, || {
        let mut scope = heap.scope();
        let queue_microtask = get_prop(&mut scope, global, "queueMicrotask");
        let handler_obj = scope.alloc_object().unwrap();
        scope
          .push_root(Value::Object(handler_obj))
          .expect("push root handler object");
        vm.call_without_host(
          &mut scope,
          queue_microtask,
          Value::Undefined,
          &[Value::Object(handler_obj)],
        )
      })
      .expect_err("non-callable handlers should be rejected")
    };

    assert_type_error_contains(
      host.window.heap_mut(),
      err,
      QUEUE_MICROTASK_NOT_CALLABLE_ERROR,
    );

    Ok(())
  }

  #[test]
  fn timer_callback_restores_vm_budget() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
    }

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      with_event_loop(&mut event_loop, || -> Result<(), crate::error::Error> {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");
        let timeout_cb = make_callback(vm, &mut scope, global, "timeout_cb", cb_noop);
        let _ = vm
          .call_without_host(
            &mut scope,
            set_timeout,
            Value::Undefined,
            &[Value::Object(timeout_cb), Value::Number(0.0)],
          )
          .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        Ok(())
      })?;
    }

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    // Timer callbacks should run under a fresh budget scope and then restore the previous budget so
    // each entry point gets an independent limit.
    let budget = host.window.vm().budget();
    assert!(
      budget.fuel.is_none() && budget.deadline.is_none(),
      "expected timer callback budget to be restored"
    );

    Ok(())
  }

  #[test]
  fn promise_job_restores_vm_budget() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    let job_log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let _log_guard = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
      install_promise_job_log(heap, Arc::clone(&job_log))
    };

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();

      with_event_loop(event_loop, || -> Result<(), crate::error::Error> {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");

        let timeout_cb =
          make_callback(vm, &mut scope, global, "timeout_cb", cb_enqueue_promise_job);

        vm.call_without_host(
          &mut scope,
          set_timeout,
          Value::Object(global),
          &[Value::Object(timeout_cb), Value::Number(0.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;

        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    assert_eq!(
      &*job_log.lock().unwrap(),
      &["timeout", "timeout_end", "job"]
    );

    let budget = host.window.vm().budget();
    assert!(
      budget.fuel.is_none() && budget.deadline.is_none(),
      "expected Promise job budget to be restored"
    );

    Ok(())
  }

  #[test]
  fn vm_js_promise_jobs_run_after_a_task_and_before_the_next_task() -> crate::error::Result<()> {
    let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let log_for_task = Arc::clone(&log);
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();

    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      log_for_task.lock().unwrap().push("task1");

      with_event_loop(event_loop, || -> crate::error::Result<()> {
        let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
        let log1 = Arc::clone(&log_for_task);
        hooks.host_enqueue_promise_job(
          vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
            log1.lock().unwrap().push("job1");
            Ok(())
          }),
          None,
        );
        let log2 = Arc::clone(&log_for_task);
        hooks.host_enqueue_promise_job(
          vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
            log2.lock().unwrap().push("job2");
            Ok(())
          }),
          None,
        );
        if let Some(err) = hooks.finish(host.window_realm().heap_mut()) {
          return Err(err);
        }
        Ok(())
      })?;

      let log_for_task2 = Arc::clone(&log_for_task);
      event_loop.queue_task(TaskSource::Timer, move |_host, _event_loop| {
        log_for_task2.lock().unwrap().push("task2");
        Ok(())
      })?;
      Ok(())
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(&*log.lock().unwrap(), &["task1", "job1", "job2", "task2"]);
    Ok(())
  }

  #[test]
  fn vm_js_promise_jobs_enqueued_by_jobs_run_in_the_same_microtask_checkpoint(
  ) -> crate::error::Result<()> {
    let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();

    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host);
    let log_for_job1 = Arc::clone(&log);

    with_event_loop(&mut event_loop, || {
      hooks.host_enqueue_promise_job(
        vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, hooks| {
          log_for_job1.lock().unwrap().push("job1");

          let log_for_job2 = Arc::clone(&log_for_job1);
          hooks.host_enqueue_promise_job(
            vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
              log_for_job2.lock().unwrap().push("job2");
              Ok(())
            }),
            None,
          );

          Ok(())
        }),
        None,
      );
    });

    assert!(hooks.finish(host.window_realm().heap_mut()).is_none());
    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(&*log.lock().unwrap(), &["job1", "job2"]);
    Ok(())
  }

  #[test]
  fn vm_js_promise_jobs_discard_persistent_roots_when_enqueue_fails() -> crate::error::Result<()> {
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();
    let mut queue_limits = QueueLimits::unbounded();
    queue_limits.max_pending_microtasks = 0;
    event_loop.set_queue_limits(queue_limits);

    let ran1 = Arc::new(AtomicBool::new(false));
    let ran2 = Arc::new(AtomicBool::new(false));

    let (root1, job1) = {
      let ran = Arc::clone(&ran1);
      let mut job = vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
        ran.store(true, Ordering::Relaxed);
        Ok(())
      });
      let heap = host.window.heap_mut();
      let mut ctx = HeapRootContext { heap };
      let root = job.add_root(&mut ctx, Value::Null).unwrap();
      (root, job)
    };

    let (root2, job2) = {
      let ran = Arc::clone(&ran2);
      let mut job = vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
        ran.store(true, Ordering::Relaxed);
        Ok(())
      });
      let heap = host.window.heap_mut();
      let mut ctx = HeapRootContext { heap };
      let root = job.add_root(&mut ctx, Value::Undefined).unwrap();
      (root, job)
    };

    assert_eq!(host.window.heap().get_root(root1), Some(Value::Null));
    assert_eq!(host.window.heap().get_root(root2), Some(Value::Undefined));

    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host);
    with_event_loop(&mut event_loop, || {
      hooks.host_enqueue_promise_job(job1, None);
      hooks.host_enqueue_promise_job(job2, None);
    });

    let err = hooks.finish(host.window.heap_mut()).expect("expected enqueue error");
    assert!(
      err.to_string().contains("max pending microtasks"),
      "unexpected error: {err}"
    );

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert!(
      !ran1.load(Ordering::Relaxed),
      "job should not run when it could not be enqueued"
    );
    assert!(
      !ran2.load(Ordering::Relaxed),
      "job should not run when it could not be enqueued"
    );

    assert_eq!(host.window.heap().get_root(root1), None);
    assert_eq!(host.window.heap().get_root(root2), None);
    Ok(())
  }

  #[test]
  fn vm_js_promise_job_failure_is_propagated_to_the_event_loop() -> crate::error::Result<()> {
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();

    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host);
    with_event_loop(&mut event_loop, || {
      hooks.host_enqueue_promise_job(
        vm_js::Job::new(vm_js::JobKind::Promise, |_ctx, _hooks| {
          Err(vm_js::VmError::TypeError("boom"))
        }),
        None,
      );
    });
    assert!(hooks.finish(host.window.heap_mut()).is_none());

    let err = event_loop
      .perform_microtask_checkpoint(&mut host)
      .expect_err("expected job failure to surface via microtask checkpoint");
    assert!(
      err.to_string().contains("boom"),
      "expected error to mention boom, got: {err}"
    );
    Ok(())
  }

  #[test]
  fn vm_js_promise_jobs_call_under_the_enqueued_realm() -> crate::error::Result<()> {
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();

    let observed: CurrentRealmLog = Arc::new(Mutex::new(None));
    let _log_guard = install_current_realm_log(host.window.heap(), Arc::clone(&observed));

    let call_id = host
      .window
      .vm_mut()
      .register_native_call(record_current_realm_native)
      .expect("register_native_call");

    let callback_func = {
      let mut scope = host.window.heap_mut().scope();
      let name = scope.alloc_string("recordRealm").unwrap();
      scope.push_root(Value::String(name)).unwrap();
      scope.alloc_native_function(call_id, None, name, 0).unwrap()
    };

    let realm = host.window.realm().id();
    let previous_realm = host.window.vm().current_realm();

    let mut job = vm_js::Job::new(vm_js::JobKind::Promise, move |ctx, hooks| {
      ctx.call(
        hooks,
        Value::Object(callback_func),
        Value::Undefined,
        &[],
      )?;
      Ok(())
    });
    {
      let heap = host.window.heap_mut();
      let mut ctx = HeapRootContext { heap };
      job
        .add_root(&mut ctx, Value::Object(callback_func))
        .expect("root callback");
    }

    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host);
    with_event_loop(&mut event_loop, || {
      hooks.host_enqueue_promise_job(job, Some(realm));
    });
    assert!(hooks.finish(host.window.heap_mut()).is_none());

    event_loop.perform_microtask_checkpoint(&mut host)?;

    assert_eq!(*observed.lock().unwrap(), Some(Some(realm)));
    assert_eq!(
      host.window.vm().current_realm(),
      previous_realm,
      "execution_context_guard should restore the previous realm after the call returns"
    );
    Ok(())
  }

  #[test]
  fn vm_js_promise_jobs_root_captured_values_until_run() -> crate::error::Result<()> {
    let limits = vm_js::HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024);

    struct GcHost {
      host_ctx: (),
      window: WindowRealm,
    }

    impl WindowRealmHost for GcHost {
      fn vm_host_and_window_realm(&mut self) -> (&mut dyn VmHost, &mut WindowRealm) {
        let GcHost { host_ctx, window } = self;
        (host_ctx, window)
      }
    }
 
    impl WebIdlBindingsHost for GcHost {
      fn call_operation(
        &mut self,
        _vm: &mut Vm,
        _scope: &mut Scope<'_>,
        _receiver: Option<Value>,
        _interface: &'static str,
        _operation: &'static str,
        _overload: usize,
        _args: &[Value],
      ) -> Result<Value, VmError> {
        Ok(Value::Undefined)
      }
 
      fn call_constructor(
        &mut self,
        _vm: &mut Vm,
        _scope: &mut Scope<'_>,
        _interface: &'static str,
        _overload: usize,
        _args: &[Value],
        _new_target: Value,
      ) -> Result<Value, VmError> {
        Ok(Value::Undefined)
      }
    }

    let mut host = GcHost {
      host_ctx: (),
      window: WindowRealm::new(
        WindowRealmConfig::new("https://example.com").with_heap_limits(limits),
      )
      .expect("create WindowRealm"),
    };
    let mut event_loop = EventLoop::<GcHost>::new();

    let call_id = host
      .window
      .vm_mut()
      .register_native_call(cb_noop)
      .expect("register_native_call");

    // Queue a PromiseReactionJob that captures heap values, then run GC before the microtask runs.
    // The job should keep the captures alive until it executes and cleans up its roots.
    let mut callback_obj: Option<vm_js::GcObject> = None;
    let mut argument_obj: Option<vm_js::GcObject> = None;

    let mut hooks = VmJsEventLoopHooks::<GcHost>::new_with_host(&mut host);
    with_event_loop(&mut event_loop, || {
      let window = host.window_realm();
      let mut scope = window.heap_mut().scope();

      let callback = {
        let name = scope.alloc_string("onFulfilled").unwrap();
        scope
          .alloc_native_function(call_id, None, name, 1)
          .unwrap()
      };
      scope.push_root(Value::Object(callback)).unwrap();
      callback_obj = Some(callback);

      let argument = scope.alloc_object().unwrap();
      scope.push_root(Value::Object(argument)).unwrap();
      argument_obj = Some(argument);

      let job_callback = hooks.host_make_job_callback(callback);
      let fulfill_reaction = vm_js::PromiseReactionRecord {
        reaction_type: vm_js::PromiseReactionType::Fulfill,
        handler: Some(job_callback),
      };

      let current_realm = fulfill_reaction.handler.as_ref().and_then(|cb| cb.realm());
      let job = vm_js::new_promise_reaction_job(
        scope.heap_mut(),
        fulfill_reaction,
        Value::Object(argument),
      )
      .unwrap();
      hooks.host_enqueue_promise_job(job, current_realm);
    });
    assert!(hooks.finish(host.window.heap_mut()).is_none());

    let callback_obj = callback_obj.expect("callback_obj");
    let argument_obj = argument_obj.expect("argument_obj");

    host.window.heap_mut().collect_garbage();
    assert!(
      host.window.heap().is_valid_object(callback_obj),
      "Promise job should keep callback object alive until the microtask runs"
    );
    assert!(
      host.window.heap().is_valid_object(argument_obj),
      "Promise job should keep captured argument alive until the microtask runs"
    );

    event_loop.perform_microtask_checkpoint(&mut host)?;

    host.window.heap_mut().collect_garbage();
    assert!(
      !host.window.heap().is_valid_object(callback_obj),
      "Job::run should remove persistent roots after execution"
    );
    assert!(
      !host.window.heap().is_valid_object(argument_obj),
      "Job::run should remove persistent roots after execution"
    );
    Ok(())
  }

  #[test]
  fn vm_js_host_call_job_callback_invokes_the_callback() -> crate::error::Result<()> {
    let call_count_before = JOB_CALLBACK_CALLS.load(Ordering::SeqCst);

    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();

    let callback_func = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      let mut scope = heap.scope();
      make_callback(vm, &mut scope, global, "callback", record_callback_call)
    };

    let job_callback = vm_js::JobCallback::new(callback_func);

    let mut job = vm_js::Job::new(vm_js::JobKind::Promise, move |ctx, hooks| {
      hooks.host_call_job_callback(ctx, &job_callback, Value::Undefined, &[])?;
      Ok(())
    });
    {
      let heap = host.window.heap_mut();
      let mut ctx = HeapRootContext { heap };
      job
        .add_root(&mut ctx, Value::Object(callback_func))
        .expect("root callback func");
    }

    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host);
    with_event_loop(&mut event_loop, || {
      hooks.host_enqueue_promise_job(job, None);
    });
    if let Some(err) = hooks.finish(host.window.heap_mut()) {
      return Err(err);
    }

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(
      JOB_CALLBACK_CALLS.load(Ordering::SeqCst),
      call_count_before + 1,
      "host_call_job_callback should invoke the callback"
    );
    Ok(())
  }
}
