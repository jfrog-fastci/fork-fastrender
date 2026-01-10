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
use crate::js::window_realm::WindowRealmHost;
use crate::render_control;
use std::time::{Duration, Instant};
use vm_js::{
  Budget, ExecutionContext, Heap, Job, JobCallback, PromiseHandle, PromiseRejectionOperation,
  PromiseState, PropertyDescriptor, PropertyKey, PropertyKind, RealmId, RootId, Scope, Value, Vm,
  VmError, VmHost, VmHostHooks, VmJobContext,
};
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
const TIMER_RECORD_CALLBACK_KEY: &str = "__callback";
const TIMER_RECORD_ARG_PREFIX: &str = "__arg";

// Native slot index on timer host functions that stores the owning global object.
const TIMER_GLOBAL_SLOT: usize = 0;

const DEFAULT_CALLBACK_FUEL: u64 = 1_000_000;
const DEFAULT_CHECK_TIME_EVERY: u32 = 100;
#[cfg(test)]
const SYMBOL_TO_NUMBER_ERROR: &str = "Cannot convert a Symbol value to a number";

fn callback_budget_from_render_deadline() -> Budget {
  // Prefer the root (outermost) render deadline so JS does not inherit internal per-stage budgets.
  let mut check_time_every = DEFAULT_CHECK_TIME_EVERY;
  let deadline = match render_control::root_deadline() {
    Some(deadline) => match deadline.remaining_timeout() {
      Some(remaining) => {
        // When no time remains, force the VM to check the deadline on the first `tick` so we can
        // immediately abort queued work (important for microtasks and Promise jobs).
        if remaining.is_zero() {
          check_time_every = 1;
        }
        Instant::now().checked_add(remaining)
      }
      None => {
        // `remaining_timeout` returns `None` both when no timeout is configured *and* when the
        // timeout has elapsed. Only treat this as an elapsed timeout when a timeout limit exists.
        if deadline.timeout_limit().is_some() {
          check_time_every = 1;
          Some(Instant::now())
        } else {
          None
        }
      }
    },
    None => None,
  };

  Budget {
    fuel: Some(DEFAULT_CALLBACK_FUEL),
    deadline,
    check_time_every,
  }
}

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

fn vm_error_to_event_loop_error(heap: &mut Heap, err: VmError) -> crate::error::Error {
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
  window_realm: &'a mut crate::js::window_realm::WindowRealm,
  host: &'a mut dyn VmHost,
  realm: Option<RealmId>,
}

impl<'a> WindowRealmJobContext<'a> {
  fn new(
    window_realm: &'a mut crate::js::window_realm::WindowRealm,
    host: &'a mut dyn VmHost,
    realm: Option<RealmId>,
  ) -> Self {
    Self {
      window_realm,
      host,
      realm,
    }
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
    let (vm, heap) = self.window_realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    if let Some(realm) = self.realm {
      let mut vm = vm.execution_context_guard(ExecutionContext {
        realm,
        script_or_module: None,
      });
      vm.call_with_host_and_hooks(host, &mut scope, host_hooks, callee, this, args)
    } else {
      vm.call_with_host_and_hooks(host, &mut scope, host_hooks, callee, this, args)
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
    let (vm, heap) = self.window_realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    if let Some(realm) = self.realm {
      let mut vm = vm.execution_context_guard(ExecutionContext {
        realm,
        script_or_module: None,
      });
      vm.construct_with_host_and_hooks(host, &mut scope, host_hooks, callee, args, new_target)
    } else {
      vm.construct_with_host_and_hooks(host, &mut scope, host_hooks, callee, args, new_target)
    }
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.window_realm.heap_mut().add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.window_realm.heap_mut().remove_root(id);
  }
}

pub struct VmJsEventLoopHooks<Host: WindowRealmHost + 'static> {
  pending_discard: Vec<Job>,
  enqueue_error: Option<crate::error::Error>,
  _marker: std::marker::PhantomData<fn() -> Host>,
}

impl<Host: WindowRealmHost + 'static> VmJsEventLoopHooks<Host> {
  pub fn new() -> Self {
    Self {
      pending_discard: Vec::new(),
      enqueue_error: None,
      _marker: std::marker::PhantomData,
    }
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

        // Borrow-split the host so we can pass both:
        // - a real `VmHost` context to native calls, and
        // - a mutable `WindowRealm` for executing the job.
        let (host_ctx, window_realm) = host.vm_host_and_window_realm();
        window_realm.reset_interrupt();

        with_event_loop(event_loop, || {
          let vm = window_realm.vm_mut();
          vm.set_budget(callback_budget_from_render_deadline());
          let tick_result = vm.tick();

          let mut hooks = VmJsEventLoopHooks::<Host>::new();
          let job_result = match tick_result {
            Ok(()) => {
              let mut ctx = WindowRealmJobContext::new(window_realm, host_ctx, realm);
              job.run(&mut ctx, &mut hooks)
            }
            Err(err) => {
              // If the VM is already out of budget (deadline exceeded, interrupted, out of fuel),
              // we must still discard the job so any persistent roots it owns are cleaned up.
              let mut ctx = WindowRealmJobContext::new(window_realm, host_ctx, realm);
              job.discard(&mut ctx);
              Err(err)
            }
          };

          if let Some(err) = hooks.finish(window_realm.heap_mut()) {
            return Err(err);
          }

          job_result
            .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))
            .map(|_| ())
        })
      })
    })();

    if let Err(err) = enqueue_result {
      if let Some(job) = job_cell.borrow_mut().take() {
        self.pending_discard.push(job);
      }
      self.enqueue_error = Some(err);
    }
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
    let (host_ctx, window_realm) = host.vm_host_and_window_realm();
    window_realm.reset_interrupt();
    let global_obj = window_realm.global_object();
    let (vm, heap) = window_realm.vm_and_heap_mut();

    let result: crate::error::Result<bool> = with_event_loop(event_loop, || {
      vm.set_budget(callback_budget_from_render_deadline());
      vm.tick()
        .map_err(|err| vm_error_to_event_loop_error(heap, err))?;

      let mut hooks = VmJsEventLoopHooks::<Host>::new();
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

        // Minimal event object: `dispatchEvent` only requires `{ type: string }`.
        let event_obj = scope.alloc_object()?;
        scope.push_root(Value::Object(event_obj))?;

        let type_s = scope.alloc_string(event_type)?;
        scope.push_root(Value::String(type_s))?;
        let type_key = alloc_key(&mut scope, "type")?;
        scope.define_property(event_obj, type_key, data_desc(Value::String(type_s)))?;

        // `PromiseRejectionEvent`-like payload (minimal).
        let reason_key = alloc_key(&mut scope, "reason")?;
        scope.define_property(event_obj, reason_key, data_desc(reason))?;
        let promise_key = alloc_key(&mut scope, "promise")?;
        scope.define_property(event_obj, promise_key, data_desc(Value::Object(promise_obj)))?;

        // Make the event cancelable to match `unhandledrejection` behavior on the web platform.
        let cancelable_key = alloc_key(&mut scope, "cancelable")?;
        scope.define_property(event_obj, cancelable_key, data_desc(Value::Bool(true)))?;

        let dispatch_key = alloc_key(&mut scope, "dispatchEvent")?;
        let dispatch = vm.get(&mut scope, global_obj, dispatch_key)?;
        let _ = vm.call_with_host_and_hooks(
          host_ctx,
          &mut scope,
          &mut hooks,
          dispatch,
          Value::Object(global_obj),
          &[Value::Object(event_obj)],
        )?;

        Ok(scope.heap().promise_is_handled(promise_obj)?)
      })();

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      handled_after_dispatch
        .map_err(|err| vm_error_to_event_loop_error(heap, err))
    });

    // Always remove the persistent root, even if dispatch failed.
    heap.remove_root(root);

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
      let (host_ctx, window_realm) = host.vm_host_and_window_realm();
      window_realm.reset_interrupt();
      let (vm, heap) = window_realm.vm_and_heap_mut();

      let result: crate::error::Result<()> = with_event_loop(event_loop, || {
        vm.set_budget(callback_budget_from_render_deadline());
        let tick_result = vm.tick();

        let mut hooks = VmJsEventLoopHooks::<Host>::new();
        let call_result = tick_result.and_then(|_| {
          let call_result: Result<(), VmError> = (|| {
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
          })();
          call_result
        });

        if let Some(err) = hooks.finish(heap) {
          return Err(err);
        }

        call_result
          .map_err(|err| vm_error_to_event_loop_error(heap, err))
          .map(|_| ())
      });

      {
        let mut scope = heap.scope();
        // Always clear the registry entry for one-shot timeouts, even if the callback throws.
        let _ = clear_registry_entry(&mut scope, registry, id);
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
      let (host_ctx, window_realm) = host.vm_host_and_window_realm();
      window_realm.reset_interrupt();
      let (vm, heap) = window_realm.vm_and_heap_mut();

      let result: crate::error::Result<()> = with_event_loop(event_loop, || {
        vm.set_budget(callback_budget_from_render_deadline());
        let tick_result = vm.tick();

        let mut hooks = VmJsEventLoopHooks::<Host>::new();
        let call_result = tick_result.and_then(|_| {
          let call_result: Result<(), VmError> = (|| {
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
          })();
          call_result
        });

        if let Some(err) = hooks.finish(heap) {
          return Err(err);
        }

        call_result
          .map_err(|err| vm_error_to_event_loop_error(heap, err))
          .map(|_| ())
      });

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
      let (host_ctx, window_realm) = host.vm_host_and_window_realm();
      window_realm.reset_interrupt();
      let (vm, heap) = window_realm.vm_and_heap_mut();
      let callback = heap.get_root(root).unwrap_or(Value::Undefined);

      let result: crate::error::Result<()> = with_event_loop(event_loop, || {
        vm.set_budget(callback_budget_from_render_deadline());
        let tick_result = vm.tick();

        let mut hooks = VmJsEventLoopHooks::<Host>::new();
        let call_result = tick_result.and_then(|_| {
          let call_result: Result<(), VmError> = (|| {
            let mut scope = heap.scope();
            // HTML `queueMicrotask` invokes callbacks with an `undefined` callback-this value.
            vm.call_with_host_and_hooks(host_ctx, &mut scope, &mut hooks, callback, Value::Undefined, &[])
              .map(|_| ())
          })();
          call_result
        });

        if let Some(err) = hooks.finish(heap) {
          return Err(err);
        }

        call_result
          .map_err(|err| vm_error_to_event_loop_error(heap, err))
          .map(|_| ())
      });

      heap.remove_root(root);

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

  Ok(())
}

  #[cfg(test)]
  mod tests {
    use super::*;
    use crate::js::clock::VirtualClock;
    use crate::js::event_loop::{EventLoop, RunLimits, RunUntilIdleOutcome, TaskSource};
    use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;
    use vm_js::Realm;

  const CALLBACK_GLOBAL_KEY: &str = "__test_global";

  static PROMISE_JOB_LOGS: OnceLock<Mutex<HashMap<usize, Arc<Mutex<Vec<&'static str>>>>>> =
    OnceLock::new();

  fn promise_job_logs() -> &'static Mutex<HashMap<usize, Arc<Mutex<Vec<&'static str>>>>> {
    PROMISE_JOB_LOGS.get_or_init(|| Mutex::new(HashMap::new()))
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

  struct Host {
    host_ctx: (),
    window: WindowRealm,
  }

  impl Host {
    fn new() -> Self {
      let window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).unwrap();
      Self {
        host_ctx: (),
        window,
      }
    }
  }

  impl WindowRealmHost for Host {
    fn vm_host_and_window_realm(&mut self) -> (&mut dyn VmHost, &mut WindowRealm) {
      let Host { host_ctx, window } = self;
      (host_ctx, window)
    }
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

    let Err(VmError::TypeError(msg)) = err else {
      panic!("expected setTimeout to return VmError::TypeError for non-callable callback");
    };
    assert_eq!(msg, SET_TIMEOUT_NOT_CALLABLE_ERROR);

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

    let Err(VmError::TypeError(msg)) = err else {
      panic!("expected TypeError for invalid this");
    };
    assert_eq!(msg, "setTimeout called with invalid this value");
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

    let Err(VmError::TypeError(msg)) = err else {
      panic!("expected setTimeout to throw a TypeError for Symbol delay");
    };
    assert_eq!(msg, SYMBOL_TO_NUMBER_ERROR);
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

    let Err(VmError::TypeError(msg)) = err else {
      panic!("expected clearTimeout to throw a TypeError for Symbol handle");
    };
    assert_eq!(msg, SYMBOL_TO_NUMBER_ERROR);
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

    match err {
      VmError::TypeError(msg) => {
        assert!(msg.contains("string handlers"), "msg={msg}");
      }
      other => panic!("expected TypeError, got {other:?}"),
    }

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

    match err {
      VmError::TypeError(msg) => {
        assert!(msg.contains("string callbacks"), "msg={msg}");
      }
      other => panic!("expected TypeError, got {other:?}"),
    }

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

    match err {
      VmError::TypeError(msg) => {
        assert_eq!(msg, QUEUE_MICROTASK_NOT_CALLABLE_ERROR);
      }
      other => panic!("expected TypeError, got {other:?}"),
    }

    Ok(())
  }

  #[test]
  fn timer_callback_does_not_reset_vm_budget_to_unlimited() -> crate::error::Result<()> {
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

    // Timer callbacks set a fresh budget based on the renderer deadline. We intentionally keep that
    // budget active after the callback returns so Promise jobs (queued via `then`) run under the
    // same budget during the subsequent microtask checkpoint.
    let budget = host.window.vm().budget();
    assert!(
      budget.fuel.is_some() || budget.deadline.is_some(),
      "expected timer callback budget to remain set"
    );

    Ok(())
  }

  #[test]
  fn promise_job_does_not_reset_vm_budget_to_unlimited() -> crate::error::Result<()> {
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
      budget.fuel.is_some() || budget.deadline.is_some(),
      "expected Promise job budget to remain set"
    );

    Ok(())
  }
}
