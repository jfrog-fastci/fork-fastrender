//! `Window` timers (`setTimeout`/`setInterval`/`queueMicrotask`) backed by FastRender's [`EventLoop`]
//! and `vm-js` values.
//!
//! This replaces the old placeholder Rust-level timer API (fake `JsValue` + Rust closures) with
//! real JS-visible global functions.
//!
//! ## Safety / determinism
//! String handlers are intentionally rejected with a `TypeError` for now to avoid string-eval and
//! keep behavior deterministic.

use crate::js::event_loop::{EventLoop, IdleCallbackId, TaskSource, TimerId};
use crate::js::realm_module_loader::ModuleLoadOutcome;
use crate::js::time::duration_to_ms_f64;
use crate::js::vm_error_format;
use crate::js::window_realm::{
  dispatch_host_exotic_delete, dispatch_host_exotic_get, dispatch_host_exotic_set,
  CollectionsExoticContext, DatasetExoticContext, ExoticDispatchHandledBy,
  WindowRealmHost, WindowRealmUserData,
};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use vm_js::{
  ExecutionContext, Heap, HostDefined, ImportMetaProperty, Job, JobCallback, ModuleGraph, ModuleId,
  ModuleLoadPayload, ModuleReferrer, ModuleRequest, PromiseHandle, PromiseRejectionOperation,
  PromiseState, PropertyDescriptor, PropertyKey, PropertyKind, RealmId, RootId, Scope, StackFrame,
  Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext, NativeFunctionId,
};
use webidl_vm_js::VmJsHostHooksPayload;
use webidl_vm_js::WebIdlBindingsHost;
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
pub(crate) const REQUEST_IDLE_CALLBACK_NOT_CALLABLE_ERROR: &str =
  "requestIdleCallback callback is not callable";

const TIMER_REGISTRY_KEY: &str = "__fastrender_timer_registry";
const IDLE_CALLBACK_REGISTRY_KEY: &str = "__fastrender_idle_callback_registry";
// Internal copy of `queueMicrotask` used by host shims that need to schedule microtasks without
// being affected by user scripts overwriting `globalThis.queueMicrotask`.
pub(crate) const INTERNAL_QUEUE_MICROTASK_KEY: &str = "__fastrender_queue_microtask";
const TIMER_RECORD_CALLBACK_KEY: &str = "__callback";
const TIMER_RECORD_ARG_PREFIX: &str = "__arg";
const MUTATION_OBSERVER_NOTIFY_KEY: &str = "__fastrender_mutation_observer_notify";

// Native slot index on timer host functions that stores the owning global object.
const TIMER_GLOBAL_SLOT: usize = 0;
// Native slot index on `import.meta.resolve` host functions that stores the base URL string
// (or `undefined` when no base URL is available).
const IMPORT_META_RESOLVE_BASE_URL_SLOT: usize = 0;
// Native slot index on `requestIdleCallback` host function that stores the native call id used for
// `IdleDeadline.timeRemaining()`.
const REQUEST_IDLE_CALLBACK_TIME_REMAINING_CALL_ID_SLOT: usize = 1;

fn hooks_payload_mut<'a>(hooks: &'a mut dyn VmHostHooks) -> Option<&'a mut VmJsHostHooksPayload> {
  let any = hooks.as_any_mut()?;
  any.downcast_mut::<VmJsHostHooksPayload>()
}

pub(crate) fn event_loop_mut_from_hooks<Host: 'static>(
  hooks: &mut dyn VmHostHooks,
) -> Option<&mut EventLoop<Host>> {
  let payload = hooks_payload_mut(hooks)?;
  payload.event_loop_mut::<EventLoop<Host>>()
}

pub(crate) fn hooks_have_event_loop(hooks: &mut dyn VmHostHooks) -> bool {
  hooks_payload_mut(hooks).is_some_and(|payload| payload.has_event_loop())
}
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

fn get_import_meta_resolve_call_id(vm: &mut Vm) -> Result<vm_js::NativeFunctionId, VmError> {
  if let Some(id) = vm
    .user_data::<WindowRealmUserData>()
    .and_then(|data| data.import_meta_resolve_call_id)
  {
    return Ok(id);
  }

  let id = vm.register_native_call(import_meta_resolve_native)?;
  let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
    return Err(VmError::InvariantViolation(
      "window realm missing user data",
    ));
  };
  data.import_meta_resolve_call_id = Some(id);
  Ok(id)
}

/// Native implementation of `import.meta.resolve(specifier)`.
///
/// The resolved base URL is provided via the function's native slots.
pub(crate) fn import_meta_resolve_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let spec_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let spec_s = scope.to_string(vm, host, hooks, spec_value)?;
  let specifier = scope.heap().get_string(spec_s)?.to_utf8_lossy();

  let base_url_slot = scope
    .heap()
    .get_function_native_slots(callee)?
    .get(IMPORT_META_RESOLVE_BASE_URL_SLOT)
    .copied()
    .unwrap_or(Value::Undefined);
  let base_url = match base_url_slot {
    Value::String(s) => Some(scope.heap().get_string(s)?.to_utf8_lossy()),
    _ => None,
  };

  let module_loader = {
    let Some(data) = vm.user_data::<WindowRealmUserData>() else {
      return Err(VmError::InvariantViolation(
        "window realm missing user data",
      ));
    };
    data.module_loader.clone()
  };

  let resolved = module_loader
    .borrow_mut()
    .resolve_module_specifier_for_import_meta(&specifier, base_url.as_deref())?;

  let out_s = scope.alloc_string(&resolved)?;
  Ok(Value::String(out_s))
}

fn resolve_error_event_location(
  filename_hint: Option<&str>,
  first_frame: Option<&StackFrame>,
) -> (String, u32, u32) {
  if let Some(frame) = first_frame {
    let from_stack = frame.source.as_ref();
    // vm-js uses synthetic `<inline>` names for unnamed scripts; prefer a real document/script URL
    // when available so `window.onerror` gets a useful filename.
    let filename = if from_stack.starts_with('<') {
      filename_hint.unwrap_or(from_stack)
    } else {
      from_stack
    };
    (filename.to_string(), frame.line, frame.col)
  } else {
    (filename_hint.unwrap_or("").to_string(), 0, 0)
  }
}

fn dispatch_window_error_event(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  vm_host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  global_obj: vm_js::GcObject,
  message: &str,
  filename: &str,
  lineno: u32,
  colno: u32,
  error_value: Option<Value>,
) -> Result<bool, VmError> {
  // Root `global_obj` while allocating property keys: `alloc_key` can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(global_obj))?;

  let type_s = scope.alloc_string("error")?;
  scope.push_root(Value::String(type_s))?;

  // Build the init dict.
  let init_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(init_obj))?;

  let cancelable_key = alloc_key(&mut scope, "cancelable")?;
  scope.define_property(init_obj, cancelable_key, data_desc(Value::Bool(true)))?;

  // ErrorEventInit:
  let message_s = scope.alloc_string(message)?;
  scope.push_root(Value::String(message_s))?;
  let message_key = alloc_key(&mut scope, "message")?;
  scope.define_property(init_obj, message_key, data_desc(Value::String(message_s)))?;

  let filename_s = scope.alloc_string(filename)?;
  scope.push_root(Value::String(filename_s))?;
  let filename_key = alloc_key(&mut scope, "filename")?;
  scope.define_property(init_obj, filename_key, data_desc(Value::String(filename_s)))?;

  let lineno_key = alloc_key(&mut scope, "lineno")?;
  scope.define_property(
    init_obj,
    lineno_key,
    data_desc(Value::Number(lineno as f64)),
  )?;

  let colno_key = alloc_key(&mut scope, "colno")?;
  scope.define_property(
    init_obj,
    colno_key,
    data_desc(Value::Number(colno as f64)),
  )?;

  let error_key = alloc_key(&mut scope, "error")?;
  let error_value = error_value.unwrap_or(Value::Null);
  scope.push_root(error_value)?;
  scope.define_property(init_obj, error_key, data_desc(error_value))?;

  let error_event_ctor_key = alloc_key(&mut scope, "ErrorEvent")?;
  let error_event_ctor = vm.get_with_host_and_hooks(vm_host, &mut scope, hooks, global_obj, error_event_ctor_key)?;
  scope.push_root(error_event_ctor)?;

  let (event_value, needs_payload_define) = if scope
    .heap()
    .is_constructor(error_event_ctor)
    .unwrap_or(false)
  {
    (
      vm.construct_with_host_and_hooks(
        vm_host,
        &mut scope,
        hooks,
        error_event_ctor,
        &[Value::String(type_s), Value::Object(init_obj)],
        error_event_ctor,
      )?,
      false,
    )
  } else {
    let event_ctor_key = alloc_key(&mut scope, "Event")?;
    let event_ctor = vm.get_with_host_and_hooks(vm_host, &mut scope, hooks, global_obj, event_ctor_key)?;
    scope.push_root(event_ctor)?;
    (
      vm.construct_with_host_and_hooks(
        vm_host,
        &mut scope,
        hooks,
        event_ctor,
        &[Value::String(type_s), Value::Object(init_obj)],
        event_ctor,
      )?,
      true,
    )
  };

  let Value::Object(event_obj) = event_value else {
    return Err(VmError::Unimplemented(
      "ErrorEvent/Event constructor returned non-object",
    ));
  };
  scope.push_root(Value::Object(event_obj))?;

  if needs_payload_define {
    scope.define_property(event_obj, message_key, read_only_data_desc(Value::String(message_s)))?;
    scope.define_property(
      event_obj,
      filename_key,
      read_only_data_desc(Value::String(filename_s)),
    )?;
    scope.define_property(
      event_obj,
      lineno_key,
      read_only_data_desc(Value::Number(lineno as f64)),
    )?;
    scope.define_property(
      event_obj,
      colno_key,
      read_only_data_desc(Value::Number(colno as f64)),
    )?;
    scope.define_property(event_obj, error_key, read_only_data_desc(error_value))?;
  }

  let dispatch_key = alloc_key(&mut scope, "dispatchEvent")?;
  let dispatch = vm.get_with_host_and_hooks(vm_host, &mut scope, hooks, global_obj, dispatch_key)?;
  let dispatch_result = vm.call_with_host_and_hooks(
    vm_host,
    &mut scope,
    hooks,
    dispatch,
    Value::Object(global_obj),
    &[Value::Object(event_obj)],
  )?;

  Ok(matches!(dispatch_result, Value::Bool(true)))
}

#[derive(Debug)]
pub(crate) struct UncaughtErrorEventTaskPayload {
  pub(crate) message: String,
  pub(crate) filename: String,
  pub(crate) lineno: u32,
  pub(crate) colno: u32,
  pub(crate) error_root: Option<RootId>,
  pub(crate) host_error: String,
}

pub(crate) fn vm_error_to_uncaught_error_event_task_payload(
  vm: &mut Vm,
  heap: &mut Heap,
  err: VmError,
) -> UncaughtErrorEventTaskPayload {
  // Root the thrown value so it survives GC until the queued error-event task runs.
  let error_root: Option<RootId> = err
    .thrown_value()
    .and_then(|value| heap.add_root(value).ok());

  let first_frame = err
    .thrown_stack()
    .and_then(|stack| stack.first())
    .cloned();

  let (message, stack) = vm_error_format::vm_error_to_message_and_stack(heap, err);

  let mut host_error = message.clone();
  if let Some(stack) = stack.as_ref() {
    host_error.push('\n');
    host_error.push_str(stack);
  }

  let filename_hint = vm
    .user_data_mut::<WindowRealmUserData>()
    .map(|data| data.document_url().to_string())
    .unwrap_or_default();
  let hint = (!filename_hint.is_empty()).then_some(filename_hint.as_str());
  let (filename, lineno, colno) = resolve_error_event_location(hint, first_frame.as_ref());

  UncaughtErrorEventTaskPayload {
    message,
    filename,
    lineno,
    colno,
    error_root,
    host_error,
  }
}

fn make_type_error_value(vm: &Vm, scope: &mut Scope<'_>, message: &str) -> Result<Value, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("module loading requires intrinsics"))?;
  vm_js::new_type_error_object(scope, &intr, message)
}

fn make_syntax_error_value(
  vm: &Vm,
  scope: &mut Scope<'_>,
  message: &str,
) -> Result<Value, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("module loading requires intrinsics"))?;
  vm_js::new_syntax_error_object(scope, &intr, message)
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

fn get_idle_callback_registry(
  scope: &mut Scope<'_>,
  global: vm_js::GcObject,
) -> Result<vm_js::GcObject, VmError> {
  let key_s = scope.alloc_string(IDLE_CALLBACK_REGISTRY_KEY)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  match scope
    .heap()
    .object_get_own_data_property_value(global, &key)?
  {
    Some(Value::Object(obj)) => Ok(obj),
    _ => Err(VmError::Unimplemented(
      "idle callback registry missing on global object",
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
    Self {
      vm,
      heap,
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
    let mut scope = self.heap.scope();
    if let Some(realm) = self.realm {
      let mut vm = self
        .vm
        .execution_context_guard(ExecutionContext {
          realm,
          script_or_module: None,
        })?;
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
      let mut vm = self
        .vm
        .execution_context_guard(ExecutionContext {
          realm,
          script_or_module: None,
        })?;
      vm.construct_with_host_and_hooks(host, &mut scope, host_hooks, callee, args, new_target)
    } else {
      self
        .vm
        .construct_with_host_and_hooks(host, &mut scope, host_hooks, callee, args, new_target)
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
  heap_ptr: Option<NonNull<Heap>>,
  heap_alive: Option<Arc<AtomicBool>>,
  enqueue_error: Option<crate::error::Error>,
  dataset_ctx: DatasetExoticContext,
  collections_ctx: CollectionsExoticContext,
  _marker: std::marker::PhantomData<fn() -> Host>,
}

struct AutoDiscardJobCell {
  job: Option<Job>,
  heap_ptr: Option<NonNull<Heap>>,
  heap_alive: Option<Arc<AtomicBool>>,
}

impl AutoDiscardJobCell {
  fn take(&mut self) -> Option<Job> {
    self.job.take()
  }
}

impl Drop for AutoDiscardJobCell {
  fn drop(&mut self) {
    let Some(job) = self.job.take() else {
      return;
    };

    let heap_ptr = self.heap_alive.as_ref().and_then(|flag| {
      flag
        .load(Ordering::Relaxed)
        .then_some(self.heap_ptr)
        .flatten()
    });
    if let Some(mut heap_ptr) = heap_ptr {
      // SAFETY: the `heap_alive` flag is set to false before the owning `WindowRealm` drops its
      // heap. When it is still true, `heap_ptr` must point at that live heap.
      let heap = unsafe { heap_ptr.as_mut() };
      let mut ctx = HeapRootContext { heap };
      job.discard(&mut ctx);
    } else {
      // We have no way to safely clean up roots once the heap is gone (or if we do not have the
      // heap pointer). Leak the job to avoid a debug-assert panic inside `vm-js`'s `Drop`
      // implementation.
      std::mem::forget(job);
    }
  }
}

impl<Host: WindowRealmHost + 'static> VmJsEventLoopHooks<Host> {
  /// Create host hooks for `vm-js` execution.
  ///
  /// FastRender's canonical `WindowHost` pipeline enters JS via `*_with_host_and_hooks` APIs, so
  /// native call/construct handlers receive the embedder [`vm_js::VmHost`] context directly.
  ///
  /// Some `vm-js` convenience entry points accept only [`vm_js::VmHostHooks`] and therefore execute
  /// native handlers with a dummy [`vm_js::VmHost`] (`()`). To support those paths (and WebIDL host
  /// dispatch), this hook implementation also exposes a [`webidl_vm_js::VmJsHostHooksPayload`] via
  /// [`vm_js::VmHostHooks::as_any_mut`] containing:
  /// - a pointer to the active embedder [`vm_js::VmHost`] context, and
  /// - a [`webidl_vm_js::WebIdlBindingsHostSlot`] for WebIDL host dispatch.
  pub fn new(host_ctx: &mut dyn VmHost) -> Self {
    let mut any = VmJsHostHooksPayload::default();
    any.set_vm_host(host_ctx);
    Self {
      any,
      heap_ptr: None,
      heap_alive: None,
      enqueue_error: None,
      dataset_ctx: DatasetExoticContext::default(),
      collections_ctx: CollectionsExoticContext::default(),
      _marker: std::marker::PhantomData,
    }
  }

  pub fn new_with_host(host: &mut Host) -> crate::error::Result<Self> {
    // Initialize the payload with the active `VmHost` context.
    let mut hooks = {
      let (host_ctx, window_realm) = host.vm_host_and_window_realm()?;
      let mut hooks = Self::new(host_ctx);
      hooks
        .any
        .set_webidl_limits(window_realm.js_execution_options().webidl_limits);
      hooks.dataset_ctx = window_realm.dataset_exotic_context();
      hooks.collections_ctx = window_realm.collections_exotic_context();
      hooks.heap_ptr = Some(NonNull::from(window_realm.heap_mut()));
      hooks.heap_alive = Some(Arc::clone(window_realm.heap_alive_flag()));
      hooks
    };
    // Populate the WebIDL bindings host slot if the embedding provides one.
    if let Some(bindings_host) = host.webidl_bindings_host() {
      hooks.set_webidl_bindings_host(bindings_host);
    }
    // Expose the full host environment via `VmHostHooks::as_any_mut` so native hooks can recover
    // embedder state even when `vm-js` only threads a narrower `VmHost` context (e.g. a document
    // wrapper). This is primarily used for deterministic test shims (offline WPT runner).
    hooks.any.set_embedder_state(host);
    Ok(hooks)
  }

  /// Populate the WebIDL bindings host slot exposed via `VmHostHooks::as_any_mut`.
  ///
  /// This enables `webidl_vm_js::host_from_hooks()` for native call handlers running under these
  /// hooks.
  ///
  /// Note: vm-js WebIDL bindings do **not** dispatch through the `vm_js::VmHost` value passed to
  /// native calls (e.g. `BrowserDocumentDom2`). The generated bindings always look up the dispatch
  /// host through `VmHostHooks` payload slots.
  pub fn set_webidl_bindings_host(&mut self, host: &mut dyn WebIdlBindingsHost) {
    self.any.webidl_bindings_host_slot_mut().set(host);
  }

  /// Create host hooks when the embedding already has a borrow-split `(VmHost, WindowRealm)` pair.
  pub fn new_with_vm_host_and_window_realm(
    vm_host: &mut dyn VmHost,
    window_realm: &mut crate::js::WindowRealm,
    webidl_bindings_host: Option<&mut dyn WebIdlBindingsHost>,
  ) -> Self {
    let mut hooks = Self::new(vm_host);
    hooks
      .any
      .set_webidl_limits(window_realm.js_execution_options().webidl_limits);
    hooks.dataset_ctx = window_realm.dataset_exotic_context();
    hooks.collections_ctx = window_realm.collections_exotic_context();
    hooks.heap_ptr = Some(NonNull::from(window_realm.heap_mut()));
    hooks.heap_alive = Some(Arc::clone(window_realm.heap_alive_flag()));
    if let Some(bindings_host) = webidl_bindings_host {
      hooks.any.webidl_bindings_host_slot_mut().set(bindings_host);
    }
    hooks
  }

  /// Installs an embedder-defined "host environment" state pointer into the hooks payload.
  ///
  /// This is only meaningful for native code paths that recover embedder state by downcasting the
  /// [`VmJsHostHooksPayload`] exposed through [`VmHostHooks::as_any_mut`], for example:
  /// - WebIDL host dispatch helpers (`webidl_vm_js::host_from_hooks`), and
  /// - privileged JS bridges like the planned Chrome API dispatcher.
  ///
  /// Embeddings that construct hooks via [`Self::new_with_host`] get this automatically. Embeddings
  /// that can only construct hooks from a borrow-split `(vm_host, window_realm)` pair (via
  /// [`Self::new_with_vm_host_and_window_realm`]) can opt in by calling this method.
  pub fn set_embedder_state<State: 'static>(&mut self, state: &mut State) {
    self.any.set_embedder_state(state);
  }

  pub fn set_event_loop(&mut self, event_loop: &mut EventLoop<Host>) {
    self.any.set_event_loop(event_loop);
  }

  fn maybe_queue_mutation_observer_notify_microtask(&mut self) {
    if self.any.event_loop_mut::<EventLoop<Host>>().is_none() {
      return;
    }
    let needs_microtask = {
      let Some(vm_host) = self.any.vm_host_mut() else {
        return;
      };
      let Some(host_dom) = crate::js::dom_host::dom_host_vmjs(vm_host) else {
        return;
      };
      host_dom.take_mutation_observer_microtask_needed()
    };
    if !needs_microtask {
      return;
    }
    let Some(event_loop) = self.any.event_loop_mut::<EventLoop<Host>>() else {
      return;
    };
    let _ = event_loop.queue_microtask(mutation_observer_notify_microtask::<Host>);
  }

  pub fn finish(mut self, heap: &mut Heap) -> Option<crate::error::Error> {
    let _ = heap;
    self.enqueue_error.take()
  }
}

fn mutation_observer_notify_microtask<Host: WindowRealmHost + 'static>(
  host: &mut Host,
  event_loop: &mut EventLoop<Host>,
) -> crate::error::Result<()> {
  let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
  hooks.set_event_loop(event_loop);
  let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
  let global_obj = window_realm.global_object();
  window_realm.reset_interrupt();
  let budget = window_realm.vm_budget_now();
  let (vm, heap) = window_realm.vm_and_heap_mut();

  let mut vm = vm.push_budget(budget);
  let tick_result = vm.tick();
  let call_result = tick_result.and_then(|_| {
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global_obj))?;
    let document_key = alloc_key(&mut scope, "document")?;
    let document_value = scope
      .heap()
      .object_get_own_data_property_value(global_obj, &document_key)?
      .unwrap_or(Value::Undefined);
    let Value::Object(document_obj) = document_value else {
      return Ok(());
    };
    scope.push_root(Value::Object(document_obj))?;
    let notify_key = alloc_key(&mut scope, MUTATION_OBSERVER_NOTIFY_KEY)?;
    let notify = scope
      .heap()
      .object_get_own_data_property_value(document_obj, &notify_key)?
      .unwrap_or(Value::Undefined);
    if !matches!(notify, Value::Object(_)) || !scope.heap().is_callable(notify)? {
      return Ok(());
    }
    let _ = vm.call_with_host_and_hooks(vm_host, &mut scope, &mut hooks, notify, Value::Undefined, &[]);
    Ok(())
  });

  let result: crate::error::Result<()> = call_result
    .map_err(|err| vm_error_to_event_loop_error(heap, err))
    .map(|_| ());

  let drain_result: crate::error::Result<()> = {
    let drain_result = {
      let mut scope = heap.scope();
      crate::js::window_realm::drain_pending_dataset_mutation_observer_microtasks(
        &mut vm,
        &mut scope,
        vm_host,
        &mut hooks,
      )
    };
    drain_result
      .map_err(|err| vm_error_to_event_loop_error(heap, err))
      .map(|_| ())
  };

  // If the notify microtask succeeded, surface any failure to schedule pending dataset mutation
  // observer delivery. If notify already failed, preserve the original error.
  let result = match (result, drain_result) {
    (Ok(()), Err(err)) => Err(err),
    (other, _) => other,
  };

  let finish_err = hooks.finish(&mut *heap);
  if let Some(err) = finish_err {
    return Err(err);
  }

  result
}

impl<Host: WindowRealmHost + 'static> VmHostHooks for VmJsEventLoopHooks<Host> {
  fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
    Some(&mut self.any)
  }

  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    // Once enqueueing fails (queue limit, missing EventLoop), we keep the first error and discard
    // all subsequent jobs (while the heap is still live) to avoid leaking persistent roots.
    if self.enqueue_error.is_some() {
      drop(AutoDiscardJobCell {
        job: Some(job),
        heap_ptr: self.heap_ptr,
        heap_alive: self.heap_alive.as_ref().map(Arc::clone),
      });
      return;
    }

    let mut job_cell = AutoDiscardJobCell {
      job: Some(job),
      heap_ptr: self.heap_ptr,
      heap_alive: self.heap_alive.as_ref().map(Arc::clone),
    };

    let enqueue_result: crate::error::Result<()> = (|| {
      let Some(event_loop) = self.any.event_loop_mut::<EventLoop<Host>>() else {
        return Err(crate::error::Error::Other(
          "vm-js Promise job enqueued without an active EventLoop".to_string(),
        ));
      };

      // `queue_microtask` accepts `FnOnce`, so we can move the job wrapper directly into the
      // runnable. If enqueue fails, the runnable (and the wrapper) will be dropped immediately,
      // triggering `AutoDiscardJobCell` to call `Job::discard(..)` while the heap is still alive.
      event_loop.queue_microtask(move |host, event_loop| {
        let Some(job) = job_cell.take() else {
          return Ok(());
        };

        let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
        hooks.set_event_loop(event_loop);
        let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
        window_realm.reset_interrupt();

        let budget = window_realm.vm_budget_now();
        let (vm, heap) = window_realm.vm_and_heap_mut();
        let mut vm = vm.push_budget(budget);
        let tick_result = vm.tick();

        let job_result = match tick_result {
          Ok(()) => {
            let mut ctx = WindowRealmJobContext::new(&mut vm, heap, vm_host, realm);
            job.run(&mut ctx, &mut hooks)
          }
          Err(err) => {
            // If the VM is already out of budget (deadline exceeded, interrupted, out of fuel),
            // we must still discard the job so any persistent roots it owns are cleaned up.
            let mut ctx = WindowRealmJobContext::new(&mut vm, heap, vm_host, realm);
            job.discard(&mut ctx);
            Err(err)
          }
        };

        let result: crate::error::Result<()> = job_result
          .map_err(|err| vm_error_to_event_loop_error(heap, err))
          .map(|_| ());

        let drain_result: crate::error::Result<()> = {
          let drain_result = {
            let mut scope = heap.scope();
            crate::js::window_realm::drain_pending_dataset_mutation_observer_microtasks(
              &mut vm,
              &mut scope,
              vm_host,
              &mut hooks,
            )
          };
          drain_result
            .map_err(|err| vm_error_to_event_loop_error(heap, err))
            .map(|_| ())
        };

        // If the job succeeded, propagate any failure to schedule mutation observer delivery.
        // If the job already failed (or terminated), preserve the original error.
        let result = match (result, drain_result) {
          (Ok(()), Err(err)) => Err(err),
          (other, _) => other,
        };

        if let Some(err) = hooks.finish(heap) {
          return Err(err);
        }
        result
      })
    })();

    if let Err(err) = enqueue_result {
      // `job_cell` is either still in scope (no EventLoop) or has already been dropped (enqueue
      // failure). Either way, the `AutoDiscardJobCell` drop path ensures the job is discarded while
      // the heap is live.
      self.enqueue_error = Some(err);
    }
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
    let _ = host_defined;

    // Convert a module-load result into the completion record expected by `vm-js`'s module loading
    // continuation, allocating real `TypeError`/`SyntaxError` objects where appropriate.
    //
    // IMPORTANT: The caller must **not** use `?` on this helper before ensuring the associated
    // `ModuleLoadPayload` has been finished/teardown'd. If error-object construction fails (OOM /
    // termination), we must still call `finish_loading_imported_module*` with `Err(err.clone())` so
    // the payload's persistent roots are released.
    fn map_module_result_to_completion(
      vm: &Vm,
      scope: &mut Scope<'_>,
      result: Result<ModuleId, VmError>,
    ) -> Result<Result<ModuleId, VmError>, VmError> {
      let completion = match result {
        Ok(id) => Ok(id),
        Err(VmError::Syntax(diags)) => {
          let message =
            vm_error_format::vm_error_to_string(scope.heap_mut(), VmError::Syntax(diags));
          let value = make_syntax_error_value(vm, scope, &message)?;
          Err(VmError::Throw(value))
        }
        Err(VmError::TypeError(message)) => {
          let value = make_type_error_value(vm, scope, message)?;
          Err(VmError::Throw(value))
        }
        Err(other) => Err(other),
      };
      Ok(completion)
    }

    let (module_loader, module_loading_enabled) = {
      let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
        // If the realm is missing its module loader state, we cannot complete the request normally.
        // Avoid leaking any persistent roots held by `payload`.
        payload.teardown_roots(scope.heap_mut());
        return Err(VmError::InvariantViolation(
          "window realm missing user data",
        ));
      };
      (data.module_loader.clone(), data.module_graph.is_some())
    };

    // `import()` is valid ECMAScript syntax even in classic scripts. However, FastRender currently
    // exposes module loading as an opt-in feature (`JsExecutionOptions::supports_module_scripts`).
    //
    // If the embedding did not enable module loading for this realm, complete the module request
    // immediately with a TypeError so dynamic imports reject without aborting the host event loop.
    if !module_loading_enabled {
      match make_type_error_value(vm, scope, "module loading is not enabled for this realm") {
        Ok(value) => {
          vm.finish_loading_imported_module(
            scope,
            modules,
            self,
            referrer,
            module_request,
            payload,
            Err(VmError::Throw(value)),
          )?;
          return Ok(());
        }
        Err(err) => {
          // Even if we cannot allocate the `TypeError` instance, we must still finish the payload so
          // its persistent roots are released.
          let _ = vm.finish_loading_imported_module(
            scope,
            modules,
            self,
            referrer,
            module_request,
            payload,
            Err(err.clone()),
          );
          return Err(err);
        }
      }
    }

    let outcome = module_loader
      .borrow_mut()
      .request_module(referrer, &module_request, &payload);

    match outcome {
      ModuleLoadOutcome::FinishNow(result) => {
        let (completion, conversion_err) = match map_module_result_to_completion(vm, scope, result) {
          Ok(completion) => (completion, None),
          Err(err) => (Err(err.clone()), Some(err)),
        };

        let finish_result = vm.finish_loading_imported_module(
          scope,
          modules,
          self,
          referrer,
          module_request,
          payload,
          completion,
        );

        // Preserve historical behavior: if error-object conversion failed, propagate that error
        // (even though we still finished the payload).
        if let Some(err) = conversion_err {
          let _ = finish_result;
          return Err(err);
        }

        finish_result?;
      }
      ModuleLoadOutcome::InFlight => {}
      ModuleLoadOutcome::StartFetch(key) => {
        let mut complete_fetch_synchronously = |hooks: &mut VmJsEventLoopHooks<Host>,
                                                 vm: &mut Vm,
                                                 scope: &mut Scope<'_>,
                                                 modules: &mut ModuleGraph,
                                                 key: crate::js::realm_module_loader::ModuleKey|
         -> Result<(), VmError> {
           let (waiters, result) = module_loader
              .borrow_mut()
              .fetch_and_register(scope.heap_mut(), modules, key)
              .ok_or(VmError::InvariantViolation(
                "module loader missing inflight continuation",
              ))?;

          let (completion, conversion_err) =
            match map_module_result_to_completion(vm, scope, result) {
              Ok(completion) => (completion, None),
              Err(err) => (Err(err.clone()), Some(err)),
            };

          // Always attempt to finish all waiters so their persistent roots are released. Record the
          // first `finish_loading_imported_module` error (if any) but keep going.
          let mut first_finish_err: Option<VmError> = None;
          for waiter in waiters {
            let res = vm.finish_loading_imported_module(
              scope,
              modules,
              hooks,
              waiter.referrer,
              waiter.request,
              waiter.payload,
              completion.clone(),
            );
            if let Err(err) = res {
              if first_finish_err.is_none() {
                first_finish_err = Some(err);
              }
            }
          }

          // Prefer propagating a conversion failure (matches prior behavior), otherwise surface the
          // first finish error.
          if let Some(err) = conversion_err {
            return Err(err);
          }
          if let Some(err) = first_finish_err {
            return Err(err);
          };

          Ok(())
        };

        // `vm-js` expects module loading to be observable synchronously in some embedder entry
        // points (e.g. `Vm::load_requested_modules` / executor unit tests) that call into the VM
        // directly rather than via a queued `EventLoop` task. In those cases,
        // `EventLoop::currently_running_task()` is `None`.
        //
        // We also load synchronously when already running inside a networking task (e.g. BrowserTab
        // module graph prefetch) to avoid queueing nested networking work.
        let running_task = self
          .any
          .event_loop_mut::<EventLoop<Host>>()
          .and_then(|event_loop| event_loop.currently_running_task());
        let should_load_synchronously = match running_task {
          None => true,
          Some(task) => task.source == TaskSource::Networking,
        };

        if should_load_synchronously {
          let result =
            complete_fetch_synchronously(self, vm, scope, modules, key.clone());
          if let Err(err) = result {
            // If synchronous fetch failed before completing waiters, tear down any in-flight payload
            // roots so dropping the module loader does not trip leaked-root assertions.
            let waiters = module_loader.borrow_mut().take_inflight(&key).unwrap_or_default();
            for waiter in waiters {
              waiter.payload.teardown_roots(scope.heap_mut());
            }
            payload.teardown_roots(scope.heap_mut());
            return Err(err);
          }
          return Ok(());
        }

        // Otherwise, enqueue a networking task that performs the fetch/parse and completes the
        // module-loading continuation later.
        let Some(event_loop) = self.any.event_loop_mut::<EventLoop<Host>>() else {
          // Not executing inside a FastRender `EventLoop`; fall back to synchronous loading.
          let result =
            complete_fetch_synchronously(self, vm, scope, modules, key.clone());
          if let Err(err) = result {
            let waiters = module_loader.borrow_mut().take_inflight(&key).unwrap_or_default();
            for waiter in waiters {
              waiter.payload.teardown_roots(scope.heap_mut());
            }
            payload.teardown_roots(scope.heap_mut());
            return Err(err);
          }
          return Ok(());
        };

        let module_loader_for_task = module_loader.clone();
        let key_for_task = key.clone();
        let enqueue_result =
          event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
            let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
            hooks.set_event_loop(event_loop);
            let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
            window_realm.reset_interrupt();

            let budget = window_realm.vm_budget_now();
            let (vm, heap) = window_realm.vm_and_heap_mut();

            let mut vm = vm.push_budget(budget);
            let tick_result = vm.tick();

            let result: Result<(), VmError> = tick_result.and_then(|_| {
              let Some(modules_ptr) = vm.module_graph_ptr() else {
                return Err(VmError::InvariantViolation(
                  "module loader requires an active module graph",
                ));
              };
              // SAFETY: `WindowRealm::enable_module_loader` installs a stable pointer to a
              // realm-owned boxed `ModuleGraph`, cleared during teardown.
              let modules = unsafe { &mut *(modules_ptr as *mut ModuleGraph) };

               let (waiters, result) = module_loader_for_task
                  .borrow_mut()
                  .fetch_and_register(heap, modules, key_for_task.clone())
                  .ok_or(VmError::InvariantViolation(
                    "module loader missing inflight continuation",
                  ))?;

               let mut scope = heap.scope();

              let (completion, conversion_err) =
                match map_module_result_to_completion(&vm, &mut scope, result) {
                  Ok(completion) => (completion, None),
                  Err(err) => (Err(err.clone()), Some(err)),
                };

              // Finish all waiters even if one completion fails so payload roots are not leaked.
              let mut first_finish_err: Option<VmError> = None;
              for waiter in waiters {
                let res = vm.finish_loading_imported_module_with_host_and_hooks(
                  vm_host,
                  &mut scope,
                  modules,
                  &mut hooks,
                  waiter.referrer,
                  waiter.request,
                  waiter.payload,
                  completion.clone(),
                );
                if let Err(err) = res {
                  if first_finish_err.is_none() {
                    first_finish_err = Some(err);
                  }
                }
              }

              if let Some(err) = conversion_err {
                return Err(err);
              }
              if let Some(err) = first_finish_err {
                return Err(err);
              }
              Ok(())
            });

            if result.is_err() {
              // We failed before the in-flight waiter list was completed. Tear down any payload
              // roots so abandoning this fetch task cannot leak persistent roots (and panic in
              // debug builds).
              let waiters = module_loader_for_task
                .borrow_mut()
                .take_inflight(&key_for_task)
                .unwrap_or_default();
              for waiter in waiters {
                waiter.payload.teardown_roots(heap);
              }
            }

            if let Some(err) = hooks.finish(heap) {
              return Err(err);
            }

            result
              .map_err(|err| vm_error_to_event_loop_error(heap, err))
              .map(|_| ())
          });

        if let Err(_err) = enqueue_result {
          // Failed to enqueue the networking task; reject all waiters immediately.
          let waiters = module_loader
            .borrow_mut()
            .take_inflight(&key)
            .unwrap_or_default();
          let (completion, conversion_err) =
            match make_type_error_value(vm, scope, "failed to enqueue module fetch task") {
              Ok(value) => (Err(VmError::Throw(value)), None),
              Err(err) => (Err(err.clone()), Some(err)),
            };

          if waiters.is_empty() {
            // Invariant violation: no inflight waiter list was recorded. Finish the original payload
            // to avoid dropping it with live persistent roots.
            let finish_result = vm.finish_loading_imported_module(
              scope,
              modules,
              self,
              referrer,
              module_request,
              payload,
              completion,
            );
            if let Some(err) = conversion_err {
              let _ = finish_result;
              return Err(err);
            }
            finish_result?;
            return Ok(());
          }

          let mut first_finish_err: Option<VmError> = None;
          for waiter in waiters {
            let res = vm.finish_loading_imported_module(
              scope,
              modules,
              self,
              waiter.referrer,
              waiter.request,
              waiter.payload,
              completion.clone(),
            );
            if let Err(err) = res {
              if first_finish_err.is_none() {
                first_finish_err = Some(err);
              }
            }
          }

          if let Some(err) = conversion_err {
            return Err(err);
          }
          if let Some(err) = first_finish_err {
            return Err(err);
          }
        }
      }
    }

    Ok(())
  }

  fn host_get_import_meta_properties(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    module: ModuleId,
  ) -> Result<Vec<ImportMetaProperty>, VmError> {
    let module_loader = {
      let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
        return Err(VmError::InvariantViolation(
          "window realm missing user data",
        ));
      };
      data.module_loader.clone()
    };

    let url = module_loader
      .borrow()
      .module_url(module)
      .unwrap_or("")
      .to_string();

    let mut scope = scope.reborrow();
    let key_s = scope.alloc_string("url")?;
    scope.push_root(Value::String(key_s))?;
    let value_s = scope.alloc_string(&url)?;
    scope.push_root(Value::String(value_s))?;

    let mut props = Vec::new();
    props.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    props.push(ImportMetaProperty {
      key: PropertyKey::from_string(key_s),
      value: Value::String(value_s),
    });
    Ok(props)
  }

  fn host_finalize_import_meta(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    import_meta: vm_js::GcObject,
    module: ModuleId,
  ) -> Result<(), VmError> {
    let module_loader = {
      let Some(data) = vm.user_data::<WindowRealmUserData>() else {
        return Err(VmError::InvariantViolation(
          "window realm missing user data",
        ));
      };
      data.module_loader.clone()
    };

    let base_url = module_loader.borrow().module_url(module).map(|s| s.to_string());

    let call_id = get_import_meta_resolve_call_id(vm)?;

    let Some(intr) = vm.intrinsics() else {
      return Err(VmError::Unimplemented(
        "import.meta.resolve requires intrinsics (create a Realm first before evaluating modules)",
      ));
    };

    let mut scope = scope.reborrow();
    // Root `import_meta` while allocating keys/functions: allocations may GC.
    scope.push_root(Value::Object(import_meta))?;

    let key_s = scope.alloc_string("resolve")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);

    let base_url_slot = if let Some(base_url) = base_url.as_deref() {
      let base_s = scope.alloc_string(base_url)?;
      scope.push_root(Value::String(base_s))?;
      Value::String(base_s)
    } else {
      Value::Undefined
    };

    let slots = [base_url_slot];
    let func = scope.alloc_native_function_with_slots(call_id, None, key_s, 1, &slots)?;
    scope
      .heap_mut()
      .object_set_prototype(func, Some(intr.function_prototype()))?;
    scope.push_root(Value::Object(func))?;

    scope.define_property(
      import_meta,
      key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Object(func),
          writable: true,
        },
      },
    )?;

    Ok(())
  }

  fn host_exotic_get(
    &mut self,
    scope: &mut Scope<'_>,
    obj: vm_js::GcObject,
    key: vm_js::PropertyKey,
    receiver: vm_js::Value,
  ) -> Result<Option<vm_js::Value>, VmError> {
    dispatch_host_exotic_get(
      scope,
      &mut self.any,
      &self.dataset_ctx,
      &self.collections_ctx,
      obj,
      key,
      receiver,
    )
  }

  fn host_exotic_set(
    &mut self,
    scope: &mut Scope<'_>,
    obj: vm_js::GcObject,
    key: vm_js::PropertyKey,
    value: vm_js::Value,
    receiver: vm_js::Value,
  ) -> Result<Option<bool>, VmError> {
    let result = dispatch_host_exotic_set(
      scope,
      &mut self.any,
      &self.dataset_ctx,
      &self.collections_ctx,
      obj,
      key,
      value,
      receiver,
    )?;
    if result.handled_by == Some(ExoticDispatchHandledBy::Dataset) {
      self.maybe_queue_mutation_observer_notify_microtask();
    }
    Ok(result.handled)
  }

  fn host_exotic_delete(
    &mut self,
    scope: &mut Scope<'_>,
    obj: vm_js::GcObject,
    key: vm_js::PropertyKey,
  ) -> Result<Option<bool>, VmError> {
    let result =
      dispatch_host_exotic_delete(scope, &mut self.any, &self.dataset_ctx, &self.collections_ctx, obj, key)?;
    if result.handled_by == Some(ExoticDispatchHandledBy::Dataset) {
      self.maybe_queue_mutation_observer_notify_microtask();
    }
    Ok(result.handled)
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
    let Some(event_loop) = self.any.event_loop_mut::<EventLoop<Host>>() else {
      // Not executing inside a FastRender `EventLoop` turn; ignore.
      return;
    };

    // Ensure we have a microtask checkpoint hook installed so we can dispatch events after the
    // microtask queue is drained (HTML "notify about rejected promises").
    // Note: this hook may be installed multiple times over the document lifetime (every time a
    // promise rejection transition is observed). Use the multiplexed hook registration API so we do
    // not clobber other checkpoint consumers (module TLA settling, etc).
    let _ = event_loop.register_microtask_checkpoint_hook(
      promise_rejection_microtask_checkpoint_hook::<Host>,
    );

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
    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
    let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
    hooks.set_event_loop(event_loop);
    window_realm.reset_interrupt();
    let global_obj = window_realm.global_object();
    let budget = window_realm.vm_budget_now();
    let (vm, heap) = window_realm.vm_and_heap_mut();

    let result: crate::error::Result<(bool, bool, Option<String>)> = (|| {
      let mut vm = vm.push_budget(budget);
      vm.tick()
        .map_err(|err| vm_error_to_event_loop_error(heap, err))?;
      let dispatch_outcome = (|| -> Result<(bool, bool, Option<String>), VmError> {
        let promise_value = heap.get_root(root).unwrap_or(Value::Undefined);
        let Value::Object(promise_obj) = promise_value else {
          // Root slot should always contain the promise object, but be defensive in release builds.
          return Ok((true, true, None));
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
        scope.define_property(init_obj, promise_key, data_desc(Value::Object(promise_obj)))?;
        let reason_key = alloc_key(&mut scope, "reason")?;
        scope.define_property(init_obj, reason_key, data_desc(reason))?;

        let promise_rejection_ctor_key = alloc_key(&mut scope, "PromiseRejectionEvent")?;
        let promise_rejection_ctor = vm.get_with_host_and_hooks(
          vm_host,
          &mut scope,
          &mut hooks,
          global_obj,
          promise_rejection_ctor_key,
        )?;
        scope.push_root(promise_rejection_ctor)?;

        let (event_value, needs_payload_define) = if scope
          .heap()
          .is_constructor(promise_rejection_ctor)
          .unwrap_or(false)
        {
          (
            vm.construct_with_host_and_hooks(
              vm_host,
              &mut scope,
              &mut hooks,
              promise_rejection_ctor,
              &[Value::String(type_s), Value::Object(init_obj)],
              promise_rejection_ctor,
            )?,
            false,
          )
        } else {
          let event_ctor_key = alloc_key(&mut scope, "Event")?;
          let event_ctor =
            vm.get_with_host_and_hooks(vm_host, &mut scope, &mut hooks, global_obj, event_ctor_key)?;
          scope.push_root(event_ctor)?;
          (
            vm.construct_with_host_and_hooks(
              vm_host,
              &mut scope,
              &mut hooks,
              event_ctor,
              &[Value::String(type_s), Value::Object(init_obj)],
              event_ctor,
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
          scope.define_property(event_obj, reason_key, read_only_data_desc(reason))?;
          scope.define_property(
            event_obj,
            promise_key,
            read_only_data_desc(Value::Object(promise_obj)),
          )?;
        }

        let dispatch_key = alloc_key(&mut scope, "dispatchEvent")?;
        let dispatch =
          vm.get_with_host_and_hooks(vm_host, &mut scope, &mut hooks, global_obj, dispatch_key)?;
        let dispatch_result = vm.call_with_host_and_hooks(
          vm_host,
          &mut scope,
          &mut hooks,
          dispatch,
          Value::Object(global_obj),
          &[Value::Object(event_obj)],
        )?;

        let not_canceled = matches!(dispatch_result, Value::Bool(true));
        let handled_after_dispatch = scope.heap().promise_is_handled(promise_obj)?;

        let host_error = if event_type == "unhandledrejection"
          && not_canceled
          && !handled_after_dispatch
        {
          let formatted_reason =
            vm_error_format::format_console_arguments_limited(scope.heap_mut(), &[reason]);
          Some(format!("Unhandled promise rejection: {formatted_reason}"))
        } else {
          None
        };

        Ok((handled_after_dispatch, not_canceled, host_error))
      })();

      dispatch_outcome.map_err(|err| vm_error_to_event_loop_error(heap, err))
    })();

    let finish_err = hooks.finish(heap);
    // Always remove the persistent root, even if dispatch failed.
    heap.remove_root(root);

    if let Some(err) = finish_err {
      return Err(err);
    }

    let (handled_after_dispatch, _not_canceled, host_error) = result?;

    // Only promises that remain unhandled after `unhandledrejection` dispatch should be eligible
    // for `rejectionhandled` later.
    if event_type == "unhandledrejection" && !handled_after_dispatch {
      let cap = event_loop.queue_limits().max_pending_tasks;
      let tracker = &mut event_loop.promise_rejection_tracker;
      if tracker.outstanding_rejected.len() < cap {
        tracker.outstanding_rejected.insert(promise);
      }
    }

    if let Some(host_error) = host_error {
      Err(crate::error::Error::Other(host_error))
    } else {
      Ok(())
    }
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

  let window_realm = host.window_realm()?;
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

pub(crate) fn queue_uncaught_error_event_task<Host: WindowRealmHost + 'static>(
  event_loop: &mut EventLoop<Host>,
  payload: UncaughtErrorEventTaskPayload,
) -> crate::error::Result<()> {
  event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
    let UncaughtErrorEventTaskPayload {
      message,
      filename,
      lineno,
      colno,
      error_root,
      mut host_error,
    } = payload;

    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
    let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
    hooks.set_event_loop(event_loop);
    window_realm.reset_interrupt();
    let global_obj = window_realm.global_object();
    let budget = window_realm.vm_budget_now();
    let (vm, heap) = window_realm.vm_and_heap_mut();

    // Best-effort: errors while reporting an error should not prevent the original exception from
    // being surfaced. If dispatch fails, include the dispatch failure in the host-side error
    // string so regressions are visible.
    let dispatch_result: std::result::Result<bool, VmError> = (|| {
      let mut vm = vm.push_budget(budget);
      vm.tick()?;

      let error_value = error_root.and_then(|root| heap.get_root(root));
      let mut scope = heap.scope();
      dispatch_window_error_event(
        &mut *vm,
        &mut scope,
        vm_host,
        &mut hooks,
        global_obj,
        &message,
        &filename,
        lineno,
        colno,
        error_value,
      )
    })();

    let finish_err = hooks.finish(heap);
    if let Some(root) = error_root {
      heap.remove_root(root);
    }
    if let Some(err) = finish_err {
      return Err(err);
    }

    let not_canceled = match dispatch_result {
      Ok(not_canceled) => not_canceled,
      Err(err) => {
        host_error.push_str("\n\nfailed to dispatch window error event: ");
        host_error.push_str(&err.to_string());
        true
      }
    };

    if not_canceled {
      Err(crate::error::Error::Other(host_error))
    } else {
      Ok(())
    }
  })?;

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
  hooks: &mut dyn VmHostHooks,
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

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(hooks) else {
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
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
      window_realm.reset_interrupt();
      let budget = window_realm.vm_budget_now();
      let (vm, heap) = window_realm.vm_and_heap_mut();

      let mut vm = vm.push_budget(budget);
      let tick_result = vm.tick();

      let call_result = tick_result.and_then(|_| {
        let mut scope = heap.scope();
        vm.call_with_host_and_hooks(
          vm_host,
          &mut scope,
          &mut hooks,
          callback,
          Value::Object(global_obj),
          &extra_args_for_cb,
        )
        .map(|_| ())
      });
      let mut callback_threw = false;
      let mut uncaught_error_payload: Option<UncaughtErrorEventTaskPayload> = None;
      let result: crate::error::Result<()> = match call_result {
        Ok(()) => Ok(()),
        Err(err) => {
          callback_threw = true;
          if vm_error_format::vm_error_is_js_exception(&err) {
            uncaught_error_payload =
              Some(vm_error_to_uncaught_error_event_task_payload(&mut *vm, heap, err));
            Ok(())
          } else {
            Err(vm_error_to_event_loop_error(heap, err))
          }
        }
      };

      let drain_result: crate::error::Result<()> = {
        let drain_result = {
          let mut scope = heap.scope();
          crate::js::window_realm::drain_pending_dataset_mutation_observer_microtasks(
            &mut vm,
            &mut scope,
            vm_host,
            &mut hooks,
          )
        };
        drain_result
          .map_err(|err| vm_error_to_event_loop_error(heap, err))
          .map(|_| ())
      };

      // If the callback succeeded, surface any failure to schedule pending dataset mutation observer
      // delivery. If the callback already failed, preserve the original error.
      let mut result = if callback_threw {
        result
      } else {
        match (result, drain_result) {
          (Ok(()), Err(err)) => Err(err),
          (other, _) => other,
        }
      };

      if let Some(payload) = uncaught_error_payload {
        let host_error = payload.host_error.clone();
        let error_root = payload.error_root;
        if queue_uncaught_error_event_task::<Host>(event_loop, payload).is_err() {
          if let Some(root) = error_root {
            heap.remove_root(root);
          }
          result = Err(crate::error::Error::Other(host_error));
        }
      }
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
  hooks: &mut dyn VmHostHooks,
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

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(hooks) else {
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
  hooks: &mut dyn VmHostHooks,
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

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(hooks) else {
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
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
      window_realm.reset_interrupt();
      let budget = window_realm.vm_budget_now();
      let (vm, heap) = window_realm.vm_and_heap_mut();

      let mut vm = vm.push_budget(budget);
      let tick_result = vm.tick();

      let call_result = tick_result.and_then(|_| {
        let mut scope = heap.scope();
        vm.call_with_host_and_hooks(
          vm_host,
          &mut scope,
          &mut hooks,
          callback,
          Value::Object(global_obj),
          &extra_args_for_cb,
        )
        .map(|_| ())
      });

      let mut callback_threw = false;
      let mut uncaught_error_payload: Option<UncaughtErrorEventTaskPayload> = None;
      let result: crate::error::Result<()> = match call_result {
        Ok(()) => Ok(()),
        Err(err) => {
          callback_threw = true;
          if vm_error_format::vm_error_is_js_exception(&err) {
            uncaught_error_payload =
              Some(vm_error_to_uncaught_error_event_task_payload(&mut *vm, heap, err));
            Ok(())
          } else {
            Err(vm_error_to_event_loop_error(heap, err))
          }
        }
      };

      let drain_result: crate::error::Result<()> = {
        let drain_result = {
          let mut scope = heap.scope();
          crate::js::window_realm::drain_pending_dataset_mutation_observer_microtasks(
            &mut vm,
            &mut scope,
            vm_host,
            &mut hooks,
          )
        };
        drain_result
          .map_err(|err| vm_error_to_event_loop_error(heap, err))
          .map(|_| ())
      };

      // If the callback succeeded, surface any failure to schedule pending dataset mutation observer
      // delivery. If the callback already failed, preserve the original error.
      let mut result = if callback_threw {
        result
      } else {
        match (result, drain_result) {
          (Ok(()), Err(err)) => Err(err),
          (other, _) => other,
        }
      };

      if let Some(payload) = uncaught_error_payload {
        let host_error = payload.host_error.clone();
        let error_root = payload.error_root;
        if queue_uncaught_error_event_task::<Host>(event_loop, payload).is_err() {
          if let Some(root) = error_root {
            heap.remove_root(root);
          }
          result = Err(crate::error::Error::Other(host_error));
        }
      }
      let finish_err = hooks.finish(&mut *heap);

      if finish_err.is_some() || result.is_err() {
        // Only cancel intervals for *fatal* host errors (e.g. VM termination, hook failures, or
        // failures to queue the uncaught `error` event task).
        //
        // When the callback throws a JS exception, we instead queue an uncaught `ErrorEvent` task
        // and keep the interval running, matching browser semantics (`setInterval` is not
        // automatically canceled by exceptions).
        event_loop.clear_interval(id);
        {
          let mut scope = heap.scope();
          let _ = clear_registry_entry(&mut scope, registry, id);
        }
      }
      if let Some(err) = finish_err {
        return Err(err);
      }
      if let Err(err) = result {
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
  hooks: &mut dyn VmHostHooks,
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

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(hooks) else {
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
  hooks: &mut dyn VmHostHooks,
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

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(hooks) else {
    return Err(throw_type_error(
      "queueMicrotask called without an active EventLoop",
    ));
  };

  // Keep the callback alive until the microtask runs.
  let root = scope.heap_mut().add_root(callback)?;
  event_loop
    .queue_microtask(move |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
      window_realm.reset_interrupt();
      let budget = window_realm.vm_budget_now();
      let _global_obj = window_realm.global_object();
      let (vm, heap) = window_realm.vm_and_heap_mut();
      let callback = heap.get_root(root).unwrap_or(Value::Undefined);

      let mut vm = vm.push_budget(budget);
      let tick_result = vm.tick();

      let call_result = tick_result.and_then(|_| {
        let call_result: Result<(), VmError> = (|| {
          let mut scope = heap.scope();
          // HTML `queueMicrotask` invokes callbacks with an `undefined` callback-this value.
          vm.call_with_host_and_hooks(
            vm_host,
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

      let mut callback_threw = false;
      let mut uncaught_error_payload: Option<UncaughtErrorEventTaskPayload> = None;
      let result: crate::error::Result<()> = match call_result {
        Ok(()) => Ok(()),
        Err(err) => {
          callback_threw = true;
          if vm_error_format::vm_error_is_js_exception(&err) {
            uncaught_error_payload =
              Some(vm_error_to_uncaught_error_event_task_payload(&mut *vm, heap, err));
            Ok(())
          } else {
            Err(vm_error_to_event_loop_error(heap, err))
          }
        }
      };

      let drain_result: crate::error::Result<()> = {
        let drain_result = {
          let mut scope = heap.scope();
          crate::js::window_realm::drain_pending_dataset_mutation_observer_microtasks(
            &mut vm,
            &mut scope,
            vm_host,
            &mut hooks,
          )
        };
        drain_result
          .map_err(|err| vm_error_to_event_loop_error(heap, err))
          .map(|_| ())
      };

      // If the callback succeeded, surface any failure to schedule pending dataset mutation observer
      // delivery. If the callback already failed, preserve the original error.
      let mut result = if callback_threw {
        result
      } else {
        match (result, drain_result) {
          (Ok(()), Err(err)) => Err(err),
          (other, _) => other,
        }
      };

      if let Some(payload) = uncaught_error_payload {
        let host_error = payload.host_error.clone();
        let error_root = payload.error_root;
        if queue_uncaught_error_event_task::<Host>(event_loop, payload).is_err() {
          if let Some(root) = error_root {
            heap.remove_root(root);
          }
          result = Err(crate::error::Error::Other(host_error));
        }
      }

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

// --- requestIdleCallback / cancelIdleCallback ---

const IDLE_DEADLINE_DEADLINE_MS_SLOT: usize = 0;
const IDLE_DEADLINE_DID_TIMEOUT_SLOT: usize = 1;

fn request_idle_callback_time_remaining_call_id_from_callee(
  scope: &Scope<'_>,
  callee: vm_js::GcObject,
) -> Result<NativeFunctionId, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots
    .get(REQUEST_IDLE_CALLBACK_TIME_REMAINING_CALL_ID_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u32::MAX as f64 => {
      Ok(NativeFunctionId(n as u32))
    }
    _ => Err(VmError::InvariantViolation(
      "requestIdleCallback missing timeRemaining native call id slot",
    )),
  }
}

fn idle_deadline_time_remaining_native<Host: WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let did_timeout = matches!(
    slots
      .get(IDLE_DEADLINE_DID_TIMEOUT_SLOT)
      .copied()
      .unwrap_or(Value::Undefined),
    Value::Bool(true)
  );
  if did_timeout {
    return Ok(Value::Number(0.0));
  }

  let deadline_ms = match slots
    .get(IDLE_DEADLINE_DEADLINE_MS_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Number(n) if n.is_finite() && !n.is_nan() && n >= 0.0 => n,
    _ => return Ok(Value::Number(0.0)),
  };

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(hooks) else {
    // Best-effort: if we do not have an active event loop, report no remaining idle time.
    return Ok(Value::Number(0.0));
  };
  let now_ms = duration_to_ms_f64(event_loop.now());
  Ok(Value::Number((deadline_ms - now_ms).max(0.0)))
}

fn make_idle_deadline_object(
  vm: &Vm,
  scope: &mut Scope<'_>,
  global_obj: vm_js::GcObject,
  did_timeout: bool,
  deadline_ms: f64,
  time_remaining_call_id: NativeFunctionId,
) -> Result<vm_js::GcObject, VmError> {
  let mut scope = scope.reborrow();
  // Root the global object while allocating property keys: `alloc_key` can trigger GC.
  scope.push_root(Value::Object(global_obj))?;
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;

  // Best-effort: if the realm exposes an `IdleDeadline` interface, set the prototype so
  // `deadline instanceof IdleDeadline` works for libraries that inspect it.
  //
  // Preserve determinism/safety by only consulting own *data* properties (no getters, no proxies).
  let own_data_value =
    |heap: &vm_js::Heap, obj: vm_js::GcObject, key: &PropertyKey| -> Result<Option<Value>, VmError> {
      match heap.object_get_own_data_property_value(obj, key) {
        Ok(value) => Ok(value),
        // Best-effort: ignore non-data properties so we don't trip over user-installed getters.
        Err(VmError::PropertyNotData) => Ok(None),
        Err(err) => Err(err),
      }
    };
  let idle_deadline_ctor_key = alloc_key(&mut scope, "IdleDeadline")?;
  let idle_deadline_ctor = own_data_value(scope.heap(), global_obj, &idle_deadline_ctor_key)?
    .and_then(|v| match v {
      Value::Object(obj) => Some(obj),
      _ => None,
    });
  if let Some(idle_deadline_ctor) = idle_deadline_ctor {
    scope.push_root(Value::Object(idle_deadline_ctor))?;
    let prototype_key = alloc_key(&mut scope, "prototype")?;
    if let Some(Value::Object(proto)) =
      own_data_value(scope.heap(), idle_deadline_ctor, &prototype_key)?
    {
      if let Err(err) = scope.heap_mut().object_set_prototype(obj, Some(proto)) {
        // Best-effort: ignore hostile prototype chains.
        if matches!(err, VmError::OutOfMemory) {
          return Err(err);
        }
      }
    }
  }

  let did_timeout_key = alloc_key(&mut scope, "didTimeout")?;
  scope.define_property(
    obj,
    did_timeout_key,
    read_only_data_desc(Value::Bool(did_timeout)),
  )?;

  let mut deadline_ms = deadline_ms;
  if !deadline_ms.is_finite() || deadline_ms.is_nan() || deadline_ms < 0.0 {
    deadline_ms = 0.0;
  }

  let name_s = scope.alloc_string("timeRemaining")?;
  scope.push_root(Value::String(name_s))?;
  let slots = [Value::Number(deadline_ms), Value::Bool(did_timeout)];
  let func = scope.alloc_native_function_with_slots(
    time_remaining_call_id,
    None,
    name_s,
    0,
    &slots,
  )?;
  if let Some(intrinsics) = vm.intrinsics() {
    scope.heap_mut().object_set_prototype(func, Some(intrinsics.function_prototype()))?;
  }
  scope.push_root(Value::Object(func))?;

  let time_remaining_key = alloc_key(&mut scope, "timeRemaining")?;
  scope.define_property(
    obj,
    time_remaining_key,
    read_only_data_desc(Value::Object(func)),
  )?;

  Ok(obj)
}

fn request_idle_callback_native<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if !is_callable(scope, callback) {
    return Err(throw_type_error(REQUEST_IDLE_CALLBACK_NOT_CALLABLE_ERROR));
  }

  let mut timeout: Option<Duration> = None;
  if let Some(Value::Object(options_obj)) = args.get(1).copied() {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(options_obj))?;
    let timeout_key = alloc_key(&mut scope, "timeout")?;
    let timeout_value = vm.get_with_host_and_hooks(host, &mut scope, hooks, options_obj, timeout_key)?;
    if !matches!(timeout_value, Value::Undefined) {
      let timeout_ms = normalize_delay_ms(scope.heap_mut(), timeout_value)?;
      timeout = Some(Duration::from_millis(timeout_ms));
    }
  }

  let time_remaining_call_id =
    request_idle_callback_time_remaining_call_id_from_callee(scope, callee)?;

  let global_obj = timer_global_from_this(
    scope,
    callee,
    this,
    "requestIdleCallback called with invalid this value",
  )?;
  let registry = get_idle_callback_registry(scope, global_obj)?;

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(hooks) else {
    return Err(throw_type_error(
      "requestIdleCallback called without an active EventLoop",
    ));
  };

  let id_cell = std::rc::Rc::new(std::cell::Cell::new(0));
  let id_cell_for_cb = id_cell.clone();

  let id = event_loop
    .request_idle_callback(timeout, move |host, event_loop, did_timeout, time_remaining_ms| {
      let id = id_cell_for_cb.get();
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
      window_realm.reset_interrupt();
      let budget = window_realm.vm_budget_now();
      let (vm, heap) = window_realm.vm_and_heap_mut();

      let mut vm = vm.push_budget(budget);
      let tick_result = vm.tick();

      let call_result = tick_result.and_then(|_| {
        let mut scope = heap.scope();
        let deadline_obj = make_idle_deadline_object(
          &*vm,
          &mut scope,
          global_obj,
          did_timeout,
          duration_to_ms_f64(event_loop.now()) + time_remaining_ms,
          time_remaining_call_id,
        )?;
        vm.call_with_host_and_hooks(
          vm_host,
          &mut scope,
          &mut hooks,
          callback,
          Value::Object(global_obj),
          &[Value::Object(deadline_obj)],
        )
        .map(|_| ())
      });

      let mut callback_threw = false;
      let mut uncaught_error_payload: Option<UncaughtErrorEventTaskPayload> = None;
      let result: crate::error::Result<()> = match call_result {
        Ok(()) => Ok(()),
        Err(err) => {
          callback_threw = true;
          if vm_error_format::vm_error_is_js_exception(&err) {
            uncaught_error_payload =
              Some(vm_error_to_uncaught_error_event_task_payload(&mut *vm, heap, err));
            Ok(())
          } else {
            Err(vm_error_to_event_loop_error(heap, err))
          }
        }
      };

      let drain_result: crate::error::Result<()> = {
        let drain_result = {
          let mut scope = heap.scope();
          crate::js::window_realm::drain_pending_dataset_mutation_observer_microtasks(
            &mut vm,
            &mut scope,
            vm_host,
            &mut hooks,
          )
        };
        drain_result
          .map_err(|err| vm_error_to_event_loop_error(heap, err))
          .map(|_| ())
      };

      // If the callback succeeded, surface any failure to schedule pending dataset mutation observer
      // delivery. If the callback already failed, preserve the original error.
      let mut result = if callback_threw {
        result
      } else {
        match (result, drain_result) {
          (Ok(()), Err(err)) => Err(err),
          (other, _) => other,
        }
      };

      if let Some(payload) = uncaught_error_payload {
        let host_error = payload.host_error.clone();
        let error_root = payload.error_root;
        if queue_uncaught_error_event_task::<Host>(event_loop, payload).is_err() {
          if let Some(root) = error_root {
            heap.remove_root(root);
          }
          result = Err(crate::error::Error::Other(host_error));
        }
      }

      let finish_err = hooks.finish(&mut *heap);
      {
        // Always clear the registry entry for one-shot idle callbacks, even if the callback throws.
        let mut scope = heap.scope();
        let _ = clear_registry_entry(&mut scope, registry, id);
      }

      if let Some(err) = finish_err {
        return Err(err);
      }

      result
    })
    .map_err(|e| throw_error(scope, &format!("{e}")))?;

  id_cell.set(id);
  if let Err(err) = store_timer_record(scope, registry, id, callback, &[]) {
    // If we cannot store the record, the callback value may be GC'd (Rust closures are not traced),
    // so we must cancel the idle callback to avoid UAF.
    event_loop.cancel_idle_callback(id);
    return Err(err);
  }

  Ok(Value::Number(id as f64))
}

fn cancel_idle_callback_native<Host: WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let id_value = args.get(0).copied().unwrap_or(Value::Number(0.0));
  let id: IdleCallbackId = normalize_timer_id(scope.heap_mut(), id_value)?;

  let global_obj = timer_global_from_this(
    scope,
    callee,
    this,
    "cancelIdleCallback called with invalid this value",
  )?;
  let registry = get_idle_callback_registry(scope, global_obj)?;

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(hooks) else {
    return Err(throw_type_error(
      "cancelIdleCallback called without an active EventLoop",
    ));
  };
  event_loop.cancel_idle_callback(id);
  let _ = clear_registry_entry(scope, registry, id);

  Ok(Value::Undefined)
}

/// Install `setTimeout`/`setInterval`/`clearTimeout`/`clearInterval`/`queueMicrotask`/
/// `requestIdleCallback`/`cancelIdleCallback` on the JS global.
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

  // Internal registry that keeps `requestIdleCallback` callbacks alive until they are fired or
  // canceled.
  let idle_registry = scope.alloc_object()?;
  scope.push_root(Value::Object(idle_registry))?;
  let idle_registry_key = alloc_key(&mut scope, IDLE_CALLBACK_REGISTRY_KEY)?;
  scope.define_property(global, idle_registry_key, data_desc(Value::Object(idle_registry)))?;

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

  let idle_deadline_time_remaining_id =
    vm.register_native_call(idle_deadline_time_remaining_native::<Host>)?;

  let request_idle_callback_id = vm.register_native_call(request_idle_callback_native::<Host>)?;
  let request_idle_callback_name = scope.alloc_string("requestIdleCallback")?;
  scope.push_root(Value::String(request_idle_callback_name))?;
  let request_idle_callback_slots = [
    Value::Object(global),
    Value::Number(idle_deadline_time_remaining_id.0 as f64),
  ];
  let request_idle_callback = scope.alloc_native_function_with_slots(
    request_idle_callback_id,
    None,
    request_idle_callback_name,
    1,
    &request_idle_callback_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    request_idle_callback,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(request_idle_callback))?;

  let cancel_idle_callback_id = vm.register_native_call(cancel_idle_callback_native::<Host>)?;
  let cancel_idle_callback_name = scope.alloc_string("cancelIdleCallback")?;
  scope.push_root(Value::String(cancel_idle_callback_name))?;
  let cancel_idle_callback = scope.alloc_native_function_with_slots(
    cancel_idle_callback_id,
    None,
    cancel_idle_callback_name,
    1,
    &global_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    cancel_idle_callback,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(cancel_idle_callback))?;

  let set_timeout_key = alloc_key(&mut scope, "setTimeout")?;
  let clear_timeout_key = alloc_key(&mut scope, "clearTimeout")?;
  let set_interval_key = alloc_key(&mut scope, "setInterval")?;
  let clear_interval_key = alloc_key(&mut scope, "clearInterval")?;
  let queue_microtask_key = alloc_key(&mut scope, "queueMicrotask")?;
  let request_idle_callback_key = alloc_key(&mut scope, "requestIdleCallback")?;
  let cancel_idle_callback_key = alloc_key(&mut scope, "cancelIdleCallback")?;

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
  scope.define_property(
    global,
    request_idle_callback_key,
    data_desc(Value::Object(request_idle_callback)),
  )?;
  scope.define_property(
    global,
    cancel_idle_callback_key,
    data_desc(Value::Object(cancel_idle_callback)),
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
  use crate::clock::{Clock, VirtualClock};
  use crate::js::event_loop::{EventLoop, QueueLimits, RunLimits, RunUntilIdleOutcome, TaskSource};
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use crate::js::JsExecutionOptions;
  use crate::resource::{FetchedResource, HttpFetcher, ResourceFetcher};
  use std::collections::HashMap;
  use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
  use std::sync::{Arc, Mutex, OnceLock};
  use std::time::Duration;
  use vm_js::Realm;
  use webidl_vm_js::{host_from_hooks, WebIdlBindingsHost};

  const CALLBACK_GLOBAL_KEY: &str = "__test_global";

  #[derive(Debug, Default)]
  struct NoFetchResourceFetcher;

  impl ResourceFetcher for NoFetchResourceFetcher {
    fn fetch(&self, url: &str) -> crate::error::Result<FetchedResource> {
      Err(crate::Error::Other(format!(
        "NoFetchResourceFetcher does not support fetch: {url}"
      )))
    }
  }

  fn make_window_host(
    dom: crate::dom2::Document,
    document_url: impl Into<String>,
  ) -> crate::error::Result<crate::js::WindowHost> {
    crate::js::WindowHost::new_with_fetcher(dom, document_url, Arc::new(NoFetchResourceFetcher))
  }

  #[test]
  fn auto_discard_job_cell_does_not_panic_when_heap_ptr_missing() {
    let heap_alive = Arc::new(AtomicBool::new(true));
    let job = Job::new(vm_js::JobKind::Promise, |_ctx, _hooks| Ok(())).unwrap();
    let cell = AutoDiscardJobCell {
      job: Some(job),
      heap_ptr: None,
      heap_alive: Some(heap_alive),
    };
    drop(cell);
  }

  #[test]
  fn dynamic_import_rejects_when_module_graph_is_not_installed() -> crate::error::Result<()> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>")?;
    let mut host = make_window_host(dom, "https://example.com/")?;

    let _ = host.exec_script(
      r#"
      globalThis.ok = false;
      import("https://example.com/mod.js").catch((e) => {
        globalThis.ok = (e instanceof TypeError);
      });
      "#,
    )?;

    host.perform_microtask_checkpoint()?;

    let ok = host.exec_script("globalThis.ok")?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn host_load_imported_module_does_not_leak_payload_roots_when_error_object_alloc_ooms(
  ) -> crate::error::Result<()> {
    use vm_js::TerminationReason;
 
    fn vm_err(err: VmError) -> crate::error::Error {
      crate::error::Error::Other(err.to_string())
    }
 
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>")?;
    let mut opts = JsExecutionOptions::default();
    // Keep this small so we can deterministically exhaust the heap in this unit test.
    opts.max_vm_heap_bytes = Some(4 * 1024 * 1024);
    let mut host = crate::js::WindowHost::new_with_fetcher_and_options(
      dom,
      "https://example.com/",
      Arc::new(NoFetchResourceFetcher),
      opts,
    )?;
    let mut hooks =
      VmJsEventLoopHooks::<crate::js::WindowHostState>::new_with_host(host.host_mut())?;
 
    // Capture a live `ModuleLoadPayload` by triggering `vm-js` module-graph loading and intercepting
    // the host hook call. We intentionally do *not* finish the payload here; that is the root-safety
    // invariant under test.
    struct CaptureHost {
      captured: Option<(ModuleReferrer, ModuleRequest, ModuleLoadPayload)>,
    }
 
    impl VmHostHooks for CaptureHost {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
 
      fn host_load_imported_module(
        &mut self,
        _vm: &mut Vm,
        _scope: &mut Scope<'_>,
        _modules: &mut ModuleGraph,
        referrer: ModuleReferrer,
        request: ModuleRequest,
        _host_defined: HostDefined,
        payload: ModuleLoadPayload,
      ) -> Result<(), VmError> {
        if self.captured.is_none() {
          self.captured = Some((referrer, request, payload));
        }
        Ok(())
      }
    }
 
    let window = host.host_mut().window_mut();
    let (vm, _realm, heap) = window.vm_realm_and_heap_mut();
    let mut modules = ModuleGraph::new();
    let (referrer, request, payload) = {
      let mut scope = heap.scope();
 
      let root_record = vm_js::SourceTextModuleRecord::parse(scope.heap_mut(), "import './dep.js';")
        .map_err(vm_err)?;
      let root_id = modules.add_module(root_record).map_err(vm_err)?;
 
      let mut capture_host = CaptureHost { captured: None };
      let _promise = vm_js::load_requested_modules(
        vm,
        &mut scope,
        &mut modules,
        &mut capture_host,
        root_id,
        HostDefined::default(),
      )
      .map_err(vm_err)?;
 
      capture_host
        .captured
        .take()
        .expect("expected module-load payload to be captured")
    };
 
    // Fill the heap with rooted ArrayBuffers until further allocations fail. This should make the
    // subsequent `TypeError` object allocation inside `host_load_imported_module` fail with OOM.
    let mut buffer_roots: Vec<RootId> = Vec::new();
    let mut chunk = 512 * 1024;
    loop {
      let attempt = {
        let mut scope = heap.scope();
        // Root the object handle while we add a persistent root: `add_root` can allocate.
        match scope.alloc_array_buffer(chunk) {
          Ok(obj) => {
            scope.push_root(Value::Object(obj)).map_err(vm_err)?;
            scope.heap_mut().add_root(Value::Object(obj)).map(Some)
          }
          Err(err) => Err(err),
        }
      };
 
      match attempt {
        Ok(Some(root)) => {
          buffer_roots.push(root);
        }
        Ok(None) => {}
        Err(VmError::OutOfMemory) => {
          if chunk <= 1 {
            break;
          }
          chunk = (chunk / 2).max(1);
        }
        Err(VmError::Termination(term)) if term.reason == TerminationReason::OutOfMemory => {
          if chunk <= 1 {
            break;
          }
          chunk = (chunk / 2).max(1);
        }
        Err(other) => return Err(crate::error::Error::Other(other.to_string())),
      }
    }
 
    // Now call the real host hook. Prior to the fix, if error-object construction OOM'd, the hook
    // would return early and drop `payload` with live persistent roots, tripping debug assertions in
    // `vm-js`. The regression test passes as long as this call returns an error *without panicking*.
    let err = {
      let mut scope = heap.scope();
      hooks
        .host_load_imported_module(
          vm,
          &mut scope,
          &mut modules,
          referrer,
          request,
          HostDefined::default(),
          payload,
        )
        .expect_err("expected module-load error-object allocation to fail under tiny heap")
    };
    assert!(
      matches!(err, VmError::OutOfMemory)
        || matches!(
          err,
          VmError::Termination(ref term) if term.reason == TerminationReason::OutOfMemory
        ),
      "expected out-of-memory error, got {err:?}"
    );
 
    // Discard any Promise jobs that `finish_loading_imported_module` may have attempted to enqueue.
    // This keeps the test focused on `ModuleLoadPayload` root teardown rather than job-queue wiring.
    let _ = hooks.finish(heap);
 
    for root in buffer_roots {
      heap.remove_root(root);
    }
 
    Ok(())
  }

  #[test]
  fn window_realm_teardown_releases_inflight_module_load_payload_roots() -> crate::error::Result<()> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>")?;
    let mut opts = JsExecutionOptions::default();
    opts.supports_module_scripts = true;
    let mut host = crate::js::WindowHost::new_with_fetcher_and_options(
      dom,
      "https://example.com/",
      Arc::new(NoFetchResourceFetcher),
      opts,
    )?;

    // Start a module graph load from inside an event loop task so `host_load_imported_module` uses
    // the async (networking-task) fetch path instead of the synchronous fast path.
    host.queue_task(TaskSource::Script, |host_state, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<crate::js::WindowHostState>::new_with_host(host_state)?;
      hooks.set_event_loop(event_loop);

      let (vm_host, window_realm) = host_state.vm_host_and_window_realm()?;
      window_realm.reset_interrupt();

      let module_loader = window_realm.module_loader_handle();
      let (vm, heap) = window_realm.vm_and_heap_mut();
      let Some(modules_ptr) = vm.module_graph_ptr() else {
        return Err(crate::error::Error::Other(
          "expected module graph to be installed when supports_module_scripts=true".to_string(),
        ));
      };
      // SAFETY: `WindowRealm::enable_module_loader` installs a stable pointer to a realm-owned boxed
      // `ModuleGraph`, cleared during teardown.
      let modules = unsafe { &mut *(modules_ptr as *mut ModuleGraph) };

      // Register an inline entry module through the host module loader so it has a URL/depth
      // recorded (needed for resolving its dependencies).
      let entry_key = crate::js::realm_module_loader::ModuleKey {
        url: "https://example.com/root.js".to_string(),
        attributes: Vec::new(),
      };
      let entry_id = module_loader
        .borrow_mut()
        .get_or_parse_inline_module(heap, modules, entry_key, "import './dep.js'; export const x = 1;")
        .map_err(|err| crate::error::Error::Other(err.to_string()))?;

      {
        let mut scope = heap.scope();
        let _promise = vm_js::load_requested_modules_with_host_and_hooks(
          vm,
          &mut scope,
          modules,
          vm_host,
          &mut hooks,
          entry_id,
          HostDefined::default(),
        )
        .map_err(|err| crate::error::Error::Other(err.to_string()))?;
      }

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      Ok(())
    })?;

    // Run exactly one task (the script task above), leaving the queued networking task unrun and
    // therefore leaving inflight module loader state behind.
    let _ = host.run_until_idle(RunLimits {
      max_tasks: 1,
      max_microtasks: 100,
      max_wall_time: None,
    })?;

    let inflight_len = host
      .host_mut()
      .window_mut()
      .module_loader_handle()
      .borrow()
      .inflight
      .len();
    assert!(
      inflight_len > 0,
      "expected module loader to have inflight entries before teardown"
    );

    // Dropping `host` will teardown the realm. This test passes as long as teardown does not panic
    // due to leaked persistent roots in `ModuleLoadPayload`.
    drop(host);
    Ok(())
  }

  #[test]
  fn host_load_imported_module_finishes_all_waiters_even_when_finish_errors() -> crate::error::Result<()> {
    use vm_js::ExecutionContext;

    fn vm_err(err: VmError) -> crate::error::Error {
      crate::error::Error::Other(err.to_string())
    }

    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>")?;
    let mut opts = JsExecutionOptions::default();
    opts.supports_module_scripts = true;
    // Give the networking task just enough fuel to:
    // 1) pass the task-level `vm.tick()`, and
    // 2) parse the fetched module,
    // but not enough fuel for `finish_loading_imported_module*` (which ticks again).
    //
    // This forces `finish_loading_imported_module*` to return an error for the *first* waiter,
    // exercising the "finish all waiters even if one finish errors" invariant.
    opts.max_instruction_count = Some(2);
    let mut host = crate::js::WindowHost::new_with_fetcher_and_options(
      dom,
      "https://example.com/",
      Arc::new(HttpFetcher::new()),
      opts,
    )?;

    // `data:` URL so we can fetch/parse without requiring a network fetcher.
    const MODULE_URL: &str = "data:,";

    host.queue_task(TaskSource::Script, |host_state, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<crate::js::WindowHostState>::new_with_host(host_state)?;
      hooks.set_event_loop(event_loop);

      let (vm_host, window_realm) = host_state.vm_host_and_window_realm()?;
      window_realm.reset_interrupt();

      let global_obj = window_realm.global_object();
      let realm_id = window_realm.realm().id();

      let (vm, heap) = window_realm.vm_and_heap_mut();
      let Some(modules_ptr) = vm.module_graph_ptr() else {
        return Err(crate::error::Error::Other(
          "expected module graph to be installed when supports_module_scripts=true".to_string(),
        ));
      };
      // SAFETY: `WindowRealm::enable_module_loader` installs a stable pointer to a realm-owned boxed
      // `ModuleGraph`, cleared during teardown.
      let modules = unsafe { &mut *(modules_ptr as *mut ModuleGraph) };

      // Ensure `start_dynamic_import*` sees a current realm (it requires an active execution
      // context).
      struct ExecCtxGuard {
        vm: *mut Vm,
        ctx: ExecutionContext,
      }

      impl ExecCtxGuard {
        fn new(vm: &mut Vm, ctx: ExecutionContext) -> Result<Self, VmError> {
          vm.push_execution_context(ctx)?;
          Ok(Self { vm: vm as *mut Vm, ctx })
        }
      }

      impl Drop for ExecCtxGuard {
        fn drop(&mut self) {
          // SAFETY: `ExecCtxGuard` is created from a live `&mut Vm` and dropped before that VM is
          // dropped.
          let vm = unsafe { &mut *self.vm };
          let popped = vm.pop_execution_context();
          debug_assert_eq!(popped, Some(self.ctx));
        }
      }

      let _ctx = ExecCtxGuard::new(
        vm,
        ExecutionContext {
          realm: realm_id,
          script_or_module: None,
        },
      )
      .map_err(vm_err)?;

      {
        let mut scope = heap.scope();
        // Root handles across `start_dynamic_import*`: the algorithm may allocate and GC.
        scope.push_root(Value::Object(global_obj)).map_err(vm_err)?;

        let spec_s = scope.alloc_string(MODULE_URL).map_err(vm_err)?;
        scope.push_root(Value::String(spec_s)).map_err(vm_err)?;
        let specifier = Value::String(spec_s);

        // Trigger *two* dynamic imports for the same module URL. The first call starts an in-flight
        // fetch; the second call attaches as a waiter to the same in-flight entry.
        vm_js::start_dynamic_import_with_host_and_hooks(
          vm,
          &mut scope,
          modules,
          vm_host,
          &mut hooks,
          global_obj,
          specifier,
          Value::Undefined,
        )
        .map_err(vm_err)?;
        vm_js::start_dynamic_import_with_host_and_hooks(
          vm,
          &mut scope,
          modules,
          vm_host,
          &mut hooks,
          global_obj,
          specifier,
          Value::Undefined,
        )
        .map_err(vm_err)?;
      }

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }
      Ok(())
    })?;

    // Running the event loop will execute the queued networking task. With the low VM fuel limit
    // above, `finish_loading_imported_module*` should fail (out-of-fuel), but the host hook must
    // still finish *all* waiter payloads so their roots are released.
    let err = host
      .run_until_idle(RunLimits::unbounded())
      .expect_err("expected module-load completion to fail under tiny VM fuel budget");

    let msg = err.to_string();
    assert!(
      msg.contains("OutOfFuel") || msg.to_ascii_lowercase().contains("out of fuel"),
      "expected out-of-fuel error, got: {msg}"
    );

    Ok(())
  }

  #[test]
  fn request_idle_callback_is_exposed_and_runs_when_idle() -> crate::error::Result<()> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>")?;
    let mut host = make_window_host(dom, "https://example.com/")?;

    let has_api = host.exec_script(
      "typeof requestIdleCallback === 'function' && typeof cancelIdleCallback === 'function'",
    )?;
    assert_eq!(has_api, Value::Bool(true));

    host.exec_script(
      r#"
      globalThis.__log = [];
      requestIdleCallback((deadline) => {
        __log.push(typeof deadline.didTimeout);
        __log.push(typeof deadline.timeRemaining);
        __log.push(typeof deadline.timeRemaining());
        __log.push(deadline.timeRemaining() >= 0 && Number.isFinite(deadline.timeRemaining()));
        __log.push(deadline.didTimeout === true || deadline.didTimeout === false);
        __log.push("idle");
        Promise.resolve().then(() => __log.push("micro"));
      });
      "#,
    )?;

    host.run_until_idle(RunLimits::unbounded())?;

    let ok = host.exec_script(
      r#"
      __log.length === 7 &&
      __log[0] === "boolean" &&
      __log[1] === "function" &&
      __log[2] === "number" &&
      __log[3] === true &&
      __log[4] === true &&
      __log[5] === "idle" &&
      __log[6] === "micro"
      "#,
    )?;
    assert_eq!(ok, Value::Bool(true));

    Ok(())
  }

  #[test]
  fn request_idle_callback_deadline_properties_are_read_only() -> crate::error::Result<()> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>")?;
    let mut host = make_window_host(dom, "https://example.com/")?;

    host.exec_script(
      r#"
      globalThis.__ok = false;
      requestIdleCallback((deadline) => {
        const didTimeoutDesc = Object.getOwnPropertyDescriptor(deadline, 'didTimeout');
        const timeRemainingDesc = Object.getOwnPropertyDescriptor(deadline, 'timeRemaining');
        globalThis.__ok =
          didTimeoutDesc !== undefined &&
          didTimeoutDesc.writable === false &&
          timeRemainingDesc !== undefined &&
          timeRemainingDesc.writable === false;
      });
      "#,
    )?;

    host.run_until_idle(RunLimits::unbounded())?;

    let ok = host.exec_script("globalThis.__ok")?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn request_idle_callback_deadline_prototype_lookup_is_best_effort() -> crate::error::Result<()> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>")?;
    let mut host = make_window_host(dom, "https://example.com/")?;

    host.exec_script(
      r#"
      globalThis.__called = false;
      globalThis.__ok = false;
      Object.defineProperty(globalThis, 'IdleDeadline', {
        get() {
          globalThis.__called = true;
          throw new Error('getter should not be invoked');
        },
        configurable: true,
      });
      requestIdleCallback(() => {
        globalThis.__ok = true;
      });
      "#,
    )?;

    host.run_until_idle(RunLimits::unbounded())?;

    let called = host.exec_script("globalThis.__called")?;
    assert_eq!(called, Value::Bool(false));
    let ok = host.exec_script("globalThis.__ok")?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn request_idle_callback_deadline_uses_idle_deadline_prototype_when_available(
  ) -> crate::error::Result<()> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>")?;
    let mut host = make_window_host(dom, "https://example.com/")?;

    host.exec_script(
      r#"
      globalThis.__ok = false;
      function IdleDeadline() {}
      requestIdleCallback((deadline) => {
        globalThis.__ok = Object.getPrototypeOf(deadline) === IdleDeadline.prototype;
      });
      "#,
    )?;

    host.run_until_idle(RunLimits::unbounded())?;

    let ok = host.exec_script("globalThis.__ok")?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn cancel_idle_callback_prevents_invocation() -> crate::error::Result<()> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>")?;
    let mut host = make_window_host(dom, "https://example.com/")?;

    host.exec_script(
      r#"
      globalThis.__called = false;
      const handle = requestIdleCallback(() => { globalThis.__called = true; });
      cancelIdleCallback(handle);
      "#,
    )?;
    host.run_until_idle(RunLimits::unbounded())?;

    let called = host.exec_script("globalThis.__called")?;
    assert_eq!(called, Value::Bool(false));
    Ok(())
  }

  #[test]
  fn request_idle_callback_timeout_fires_while_busy() -> crate::error::Result<()> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>")?;
    let clock = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::with_clock(clock.clone());
    let mut host = crate::js::WindowHost::new_with_fetcher_and_event_loop(
      dom,
      "https://example.com/",
      Arc::new(NoFetchResourceFetcher),
      event_loop,
    )?;

    host.exec_script(
      r#"
      globalThis.log = [];
      let count = 0;
      function tick() {
        log.push("t" + count);
        count++;
        if (count < 5) setTimeout(tick, 0);
      }
      setTimeout(tick, 0);

      requestIdleCallback((deadline) => {
        log.push("idle:" + deadline.didTimeout);
      }, { timeout: 10 });
      "#,
    )?;

    // Drive the event loop one task at a time while advancing the virtual clock so that the idle
    // callback times out before the timer chain has finished.
    let step_limits = RunLimits {
      max_tasks: 1,
      max_microtasks: 10_000,
      max_wall_time: None,
    };
    for step in 0..10 {
      let _ = host.run_until_idle(step_limits)?;
      if step < 2 {
        clock.advance(Duration::from_millis(5));
      }
    }
    host.run_until_idle(RunLimits::unbounded())?;

    let ok = host.exec_script(
      r#"
      (function () {
        const idxIdle = log.indexOf("idle:true");
        const idxT4 = log.indexOf("t4");
        return idxIdle !== -1 && idxT4 !== -1 && idxIdle < idxT4;
      })()
      "#,
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn idle_deadline_time_remaining_decreases_within_callback() -> crate::error::Result<()> {
    #[derive(Default)]
    struct ClockVmHost {
      clock: Arc<VirtualClock>,
    }

    struct ClockHost {
      window: WindowRealm,
      vm_host: ClockVmHost,
    }

    impl ClockHost {
      fn new(clock: Arc<VirtualClock>) -> Self {
        let window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).unwrap();
        Self {
          window,
          vm_host: ClockVmHost { clock },
        }
      }
    }

    impl WindowRealmHost for ClockHost {
      fn vm_host_and_window_realm(
        &mut self,
      ) -> crate::error::Result<(&mut dyn VmHost, &mut WindowRealm)> {
        Ok((&mut self.vm_host, &mut self.window))
      }
    }

    fn advance_clock_native(
      _vm: &mut Vm,
      scope: &mut Scope<'_>,
      host: &mut dyn VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: vm_js::GcObject,
      _this: Value,
      args: &[Value],
    ) -> Result<Value, VmError> {
      let ms_value = args.get(0).copied().unwrap_or(Value::Number(0.0));
      let ms = normalize_delay_ms(scope.heap_mut(), ms_value)?;
      let Some(host) = host.as_any_mut().downcast_mut::<ClockVmHost>() else {
        return Err(VmError::Unimplemented(
          "__advanceClock expected ClockVmHost",
        ));
      };
      host.clock.advance(Duration::from_millis(ms));
      Ok(Value::Undefined)
    }

    let clock = Arc::new(VirtualClock::new());
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let mut event_loop = EventLoop::<ClockHost>::with_clock(clock_dyn);
    let mut host = ClockHost::new(Arc::clone(&clock));

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<ClockHost>(vm, realm, heap).unwrap();

      let mut scope = heap.scope();
      let global = realm.global_object();
      scope
        .push_root(Value::Object(global))
        .map_err(|err| crate::error::Error::Other(err.to_string()))?;

      let advance_cb = make_callback(vm, &mut scope, global, "__advanceClock", advance_clock_native);
      set_prop(&mut scope, global, "__advanceClock", Value::Object(advance_cb));
      set_prop(&mut scope, global, "__ok", Value::Bool(false));
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<ClockHost>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (vm_host, window_realm) = host.vm_host_and_window_realm()?;

      window_realm
        .exec_script_with_host_and_hooks(
          vm_host,
          &mut hooks,
          r#"
          globalThis.__ok = false;
          requestIdleCallback((deadline) => {
            const t1 = deadline.timeRemaining();
            globalThis.__advanceClock(10);
            const t2 = deadline.timeRemaining();
            globalThis.__t1 = t1;
            globalThis.__t2 = t2;
            globalThis.__ok = t2 <= Math.max(0, t1 - 10);
          });
          "#,
        )
        .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))?;

      if let Some(err) = hooks.finish(window_realm.heap_mut()) {
        return Err(err);
      }

      Ok(())
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    let (ok, t1, t2) = {
      let (_vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      (
        get_prop(&mut scope, global, "__ok"),
        get_prop(&mut scope, global, "__t1"),
        get_prop(&mut scope, global, "__t2"),
      )
    };
    assert_eq!(ok, Value::Bool(true));

    let (Value::Number(t1), Value::Number(t2)) = (t1, t2) else {
      panic!("expected numeric __t1/__t2 values");
    };
    assert!(
      t2 <= t1,
      "expected deadline.timeRemaining() to be non-increasing, got t1={t1} t2={t2}"
    );

    Ok(())
  }

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
    fn vm_host_and_window_realm(
      &mut self,
    ) -> crate::error::Result<(&mut dyn VmHost, &mut WindowRealm)> {
      let Host {
        host_ctx, window, ..
      } = self;
      Ok((host_ctx, window))
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
    let job = vm_js::Job::new(vm_js::JobKind::Promise, move |ctx, hooks| {
      ctx.call(hooks, binding_value, Value::Undefined, &[])?;
      let job2 = vm_js::Job::new(vm_js::JobKind::Promise, move |ctx, hooks| {
        ctx.call(hooks, binding_value, Value::Undefined, &[])?;
        Ok(())
      })?;
      hooks.host_enqueue_promise_job(job2, None);
      Ok(())
    })?;
    hooks.host_enqueue_promise_job(job, None);

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

  fn read_string_array(heap: &mut Heap, realm: &Realm, name: &str) -> Vec<String> {
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope
      .push_root(Value::Object(global))
      .expect("push root global");
    let key_s = scope.alloc_string(name).unwrap();
    let key = PropertyKey::from_string(key_s);
    let value = scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .unwrap()
      .unwrap_or(Value::Undefined);
    let Value::Object(arr_obj) = value else {
      panic!("expected {name} to be an object, got {value:?}");
    };
    scope
      .push_root(Value::Object(arr_obj))
      .expect("push root array");
    let length_key_s = scope.alloc_string("length").unwrap();
    let length_key = PropertyKey::from_string(length_key_s);
    let length_value = scope
      .heap()
      .object_get_own_data_property_value(arr_obj, &length_key)
      .unwrap()
      .unwrap_or(Value::Undefined);
    let Value::Number(n) = length_value else {
      panic!("expected {name}.length to be a number, got {length_value:?}");
    };
    let len = n as u32;
    let mut out = Vec::new();
    for i in 0..len {
      let key_s = scope.alloc_string(&i.to_string()).unwrap();
      let key = PropertyKey::from_string(key_s);
      let v = scope
        .heap()
        .object_get_own_data_property_value(arr_obj, &key)
        .unwrap()
        .unwrap_or(Value::Undefined);
      let Value::String(s) = v else {
        panic!("expected {name}[{i}] to be a string, got {v:?}");
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
    let job = vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
      record_promise_job_log(heap_ptr_for_job, "job");
      Ok(())
    })?;
    hooks.host_enqueue_promise_job(job, None);

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
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let Value::Object(global) = get_prop(scope, callee, CALLBACK_GLOBAL_KEY) else {
      return Ok(Value::Undefined);
    };
    set_prop(
      scope,
      global,
      "__this_is_global",
      Value::Bool(matches!(this, Value::Object(obj) if obj == global)),
    );
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
      let _ =
        vm.call_with_host_and_hooks(host, scope, hooks, clear_interval, Value::Undefined, &[id])?;
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
      set_prop(
        &mut scope,
        global,
        "__budget_fuel_is_some",
        Value::Bool(false),
      );
      set_prop(
        &mut scope,
        global,
        "__budget_deadline_is_some",
        Value::Bool(false),
      );
      set_prop(
        &mut scope,
        global,
        "__budget_fuel_value",
        Value::Number(-1.0),
      );
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);

      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();

      {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");
        let timeout_cb = make_callback(vm, &mut scope, global, "timeout_cb", cb_record_vm_budget);
        vm.call_with_host_and_hooks(
          &mut host.host_ctx,
          &mut scope,
          &mut hooks,
          set_timeout,
          Value::Undefined,
          &[Value::Object(timeout_cb), Value::Number(0.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
      }

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      Ok(())
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
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);

      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();

      {
        let mut scope = heap.scope();

        let set_timeout = get_prop(&mut scope, global, "setTimeout");
        let queue_microtask = get_prop(&mut scope, global, "queueMicrotask");

        let timeout_cb = make_callback(vm, &mut scope, global, "timeout_cb", cb_push_t);
        let micro_cb = make_callback(vm, &mut scope, global, "micro_cb", cb_push_m);

        vm.call_with_host_and_hooks(
          &mut host.host_ctx,
          &mut scope,
          &mut hooks,
          set_timeout,
          Value::Undefined,
          &[Value::Object(timeout_cb), Value::Number(0.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;

        vm.call_with_host_and_hooks(
          &mut host.host_ctx,
          &mut scope,
          &mut hooks,
          queue_microtask,
          Value::Undefined,
          &[Value::Object(micro_cb)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;

        push_log(&mut scope, global, "sync");
      }

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      Ok(())
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
  fn microtask_ordering_between_queue_microtask_and_promise_reactions() -> crate::error::Result<()> {
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (_vm_host, window_realm) = host.vm_host_and_window_realm()?;
      window_realm.reset_interrupt();

      let result = window_realm.exec_script_with_hooks(
        &mut hooks,
        r#"
        globalThis.__log = [];
        queueMicrotask(() => __log.push('qm1'));
        Promise.resolve().then(() => __log.push('p1'));
        queueMicrotask(() => __log.push('qm2'));
        "#,
      );

      let heap = window_realm.heap_mut();
      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      result
        .map(|_| ())
        .map_err(|err| vm_error_to_event_loop_error(heap, err))
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    let log = {
      let (_vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_string_array(heap, realm, "__log")
    };
    assert_eq!(log, vec!["qm1", "p1", "qm2"]);
    Ok(())
  }

  #[test]
  fn microtask_ordering_nested_queue_microtask_and_promise_reactions_is_fifo(
  ) -> crate::error::Result<()> {
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (_vm_host, window_realm) = host.vm_host_and_window_realm()?;
      window_realm.reset_interrupt();

      let result = window_realm.exec_script_with_hooks(
        &mut hooks,
        r#"
        globalThis.__log = [];
        queueMicrotask(() => {
          __log.push('qm1');
          Promise.resolve().then(() => __log.push('p_in_qm'));
        });
        Promise.resolve().then(() => {
          __log.push('p1');
          queueMicrotask(() => __log.push('qm_in_p'));
        });
        "#,
      );

      let heap = window_realm.heap_mut();
      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      result
        .map(|_| ())
        .map_err(|err| vm_error_to_event_loop_error(heap, err))
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    let log = {
      let (_vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_string_array(heap, realm, "__log")
    };
    assert_eq!(log, vec!["qm1", "p1", "p_in_qm", "qm_in_p"]);
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
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
      window_realm.reset_interrupt();

      let result = window_realm.exec_script_with_hooks(
        &mut hooks,
        "queueMicrotask(() => {\n\
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
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
      window_realm.reset_interrupt();

      let result = window_realm.exec_script_with_hooks(
        &mut hooks,
        "setTimeout(() => {\n\
           while (true) {}\n\
           globalThis.__ran = true;\n\
         }, 0);",
      );

      if let Some(err) = hooks.finish(window_realm.heap_mut()) {
        return Err(err);
      }

      result
        .map(|_| ())
        .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))
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
      let (host_ctx, window_realm) = host.vm_host_and_window_realm()?;
      let mut hooks = VmJsEventLoopHooks::<Host>::new(&mut *host_ctx);
      hooks.set_event_loop(event_loop);
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
      set_prop(
        &mut scope,
        global,
        "__check_host_hooks",
        Value::Object(check),
      );
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
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
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.host_ctx.hook_downcast_count, 3);
    Ok(())
  }

  #[test]
  fn webidl_bindings_host_is_available_via_explicit_slot_for_hooks_new() -> crate::error::Result<()>
  {
    // This mirrors `BrowserTabJsExecutor` entrypoints which only have access to a `&mut dyn VmHost`
    // (the document) and therefore construct hooks via `VmJsEventLoopHooks::new(host_ctx)`.
    //
    // Without a surrounding `WindowRealmHost`, the WebIDL bindings host slot must be set
    // explicitly.
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
      set_prop(
        &mut scope,
        global,
        "__check_host_hooks",
        Value::Object(check),
      );
    }

    // Call the callback directly so we don't need an EventLoop turn; this is enough to validate
    // that `host_from_hooks` can retrieve the dispatch object through the hook payload.
    {
      let Host {
        host_ctx,
        bindings_host,
        window,
      } = &mut host;
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      let mut hooks = VmJsEventLoopHooks::<Host>::new(host_ctx);
      hooks.set_webidl_bindings_host(bindings_host);

      {
        let mut scope = heap.scope();
        let global = realm.global_object();
        let binding = get_prop(&mut scope, global, "__check_host_hooks");
        scope.push_root(binding).expect("push root binding");

        vm.call_with_host_and_hooks(
          host_ctx,
          &mut scope,
          &mut hooks,
          binding,
          Value::Undefined,
          &[],
        )
        .map_err(|err| crate::error::Error::Other(err.to_string()))?;
      }
      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }
    }

    assert_eq!(host.bindings_host.webidl_dispatch_count, 1);
    Ok(())
  }

  #[test]
  fn hooks_set_embedder_state_makes_state_available_to_native_calls_for_borrow_split_hooks(
  ) -> Result<(), VmError> {
    struct DummyWindowRealmHost;

    impl WindowRealmHost for DummyWindowRealmHost {
      fn vm_host_and_window_realm(
        &mut self,
      ) -> crate::error::Result<(&mut dyn VmHost, &mut WindowRealm)> {
        unreachable!("DummyWindowRealmHost is only used as a type parameter in this test")
      }
    }

    #[derive(Default)]
    struct TestState {
      calls: usize,
    }

    fn check_embedder_state_native(
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _host: &mut dyn VmHost,
      hooks: &mut dyn VmHostHooks,
      _callee: vm_js::GcObject,
      _this: Value,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      let Some(payload) = hooks_payload_mut(hooks) else {
        return Err(VmError::InvariantViolation(
          "expected VmJsEventLoopHooks to expose VmJsHostHooksPayload via as_any_mut",
        ));
      };

      if let Some(state) = payload.embedder_state_mut::<TestState>() {
        state.calls += 1;
        Ok(Value::Bool(true))
      } else {
        Ok(Value::Bool(false))
      }
    }

    let mut vm_host = ();
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let mut state = TestState::default();

    // Hooks constructed from a borrow-split (vm_host, window_realm) pair do not install embedder
    // state automatically.
    let mut hooks = VmJsEventLoopHooks::<DummyWindowRealmHost>::new_with_vm_host_and_window_realm(
      &mut vm_host,
      &mut window,
      None,
    );

    let call_id = window
      .vm_mut()
      .register_native_call(check_embedder_state_native)
      .expect("register_native_call");

    let func = {
      let mut scope = window.heap_mut().scope();
      let name = scope.alloc_string("checkEmbedderState").unwrap();
      scope.push_root(Value::String(name)).unwrap();
      scope.alloc_native_function(call_id, None, name, 0).unwrap()
    };

    let result_before = {
      let (vm, heap) = window.vm_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(func))?;
      vm.call_with_host_and_hooks(
        &mut vm_host,
        &mut scope,
        &mut hooks,
        Value::Object(func),
        Value::Undefined,
        &[],
      )
    }?;
    assert_eq!(result_before, Value::Bool(false));

    hooks.set_embedder_state(&mut state);

    let result_after = {
      let (vm, heap) = window.vm_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(func))?;
      vm.call_with_host_and_hooks(
        &mut vm_host,
        &mut scope,
        &mut hooks,
        Value::Object(func),
        Value::Undefined,
        &[],
      )
    }?;
    assert_eq!(result_after, Value::Bool(true));
    assert_eq!(state.calls, 1);

    assert!(hooks.finish(window.heap_mut()).is_none());
    Ok(())
  }

  #[test]
  fn host_exotic_get_delegates_to_webidl_bindings_host() -> Result<(), VmError> {
    struct DummyWindowRealmHost;

    impl WindowRealmHost for DummyWindowRealmHost {
      fn vm_host_and_window_realm(
        &mut self,
      ) -> crate::error::Result<(&mut dyn VmHost, &mut WindowRealm)> {
        unreachable!("DummyWindowRealmHost is only used as a type parameter in this test")
      }
    }

    const SENTINEL_SLOTS: vm_js::HostSlots = vm_js::HostSlots { a: 1, b: 0xEC71C };
    const SENTINEL_VALUE: Value = Value::Number(123.0);

    struct ExoticGetHost {
      calls: usize,
    }

    impl WebIdlBindingsHost for ExoticGetHost {
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

      fn exotic_get(
        &mut self,
        scope: &mut Scope<'_>,
        obj: vm_js::GcObject,
        key: vm_js::PropertyKey,
        receiver: Value,
      ) -> Result<Option<Value>, VmError> {
        let _ = (key, receiver);
        if scope.heap().object_host_slots(obj)? == Some(SENTINEL_SLOTS) {
          self.calls += 1;
          return Ok(Some(SENTINEL_VALUE));
        }
        Ok(None)
      }
    }

    let mut heap = Heap::new(vm_js::HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    // Ensure the object stays live across key allocations and hook dispatch.
    let obj = scope.alloc_object()?;
    scope.push_root(Value::Object(obj))?;
    scope.heap_mut().object_set_host_slots(obj, SENTINEL_SLOTS)?;

    let key_s = scope.alloc_string("sentinel")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);

    // No WebIDL host installed: should behave like an unhandled exotic get (i.e. no result).
    {
      let mut vm_host = ();
      let mut hooks = VmJsEventLoopHooks::<DummyWindowRealmHost>::new(&mut vm_host);
      let out = hooks.host_exotic_get(&mut scope, obj, key, Value::Object(obj))?;
      assert_eq!(out, None);
    }

    // With a WebIDL host installed: delegate to `WebIdlBindingsHost::exotic_get`.
    {
      let mut vm_host = ();
      let mut hooks = VmJsEventLoopHooks::<DummyWindowRealmHost>::new(&mut vm_host);
      let mut webidl_host = ExoticGetHost { calls: 0 };
      hooks.set_webidl_bindings_host(&mut webidl_host);

      let out = hooks.host_exotic_get(&mut scope, obj, key, Value::Object(obj))?;
      assert_eq!(out, Some(SENTINEL_VALUE));
      assert_eq!(webidl_host.calls, 1);
    }

    Ok(())
  }

  #[test]
  fn webidl_bindings_host_is_available_via_hooks_slot_for_script_and_tasks(
  ) -> crate::error::Result<()> {
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
      set_prop(
        &mut scope,
        global,
        "__check_host_hooks",
        Value::Object(check),
      );
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (_, window_realm) = host.vm_host_and_window_realm()?;
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
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (_, window_realm) = host.vm_host_and_window_realm()?;
      window_realm.reset_interrupt();

      let result = window_realm.exec_script_with_hooks(
        &mut hooks,
        "globalThis.__timeout_fired = false;\n\
         setTimeout(function(){ globalThis.__timeout_fired = true; }, 0);\n",
      );
      if let Some(err) = hooks.finish(window_realm.heap_mut()) {
        return Err(err);
      }
      result
        .map(|_| ())
        .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))
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
  fn timer_and_promise_jobs_invoke_callbacks_with_embedder_vm_host() -> crate::error::Result<()> {
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
      fn vm_host_and_window_realm(
        &mut self,
      ) -> crate::error::Result<(&mut dyn VmHost, &mut WindowRealm)> {
        Ok((&mut self.vm_host, &mut self.window))
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
      let cb = make_callback(
        vm,
        &mut scope,
        global,
        "__bump_counter",
        bump_counter_native,
      );
      set_prop(&mut scope, global, "__bump_counter", Value::Object(cb));
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<HostWithVmHost>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (vm_host, window) = host.vm_host_and_window_realm()?;
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
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);

      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();

      {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");
        let timeout_cb = make_callback(vm, &mut scope, global, "timeout_cb", cb_push_t);

        let delay_s = scope.alloc_string("0x10").unwrap();
        scope.push_root(Value::String(delay_s)).unwrap();
        vm.call_with_host_and_hooks(
          &mut host.host_ctx,
          &mut scope,
          &mut hooks,
          set_timeout,
          Value::Undefined,
          &[Value::Object(timeout_cb), Value::String(delay_s)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
      }

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      Ok(())
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
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);

      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();

      {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");

        let timeout_cb =
          make_callback(vm, &mut scope, global, "timeout_cb", cb_enqueue_promise_job);
        let next_cb = make_callback(vm, &mut scope, global, "next_cb", cb_record_next);

        vm.call_with_host_and_hooks(
          &mut host.host_ctx,
          &mut scope,
          &mut hooks,
          set_timeout,
          Value::Undefined,
          &[Value::Object(timeout_cb), Value::Number(0.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;

        vm.call_with_host_and_hooks(
          &mut host.host_ctx,
          &mut scope,
          &mut hooks,
          set_timeout,
          Value::Undefined,
          &[Value::Object(next_cb), Value::Number(0.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
      }

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      Ok(())
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
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);

      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();

      {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");

        let webidl_dispatch = make_callback(
          vm,
          &mut scope,
          global,
          "__webidl_dispatch",
          cb_webidl_dispatch,
        );
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

        vm.call_with_host_and_hooks(
          &mut host.host_ctx,
          &mut scope,
          &mut hooks,
          set_timeout,
          Value::Undefined,
          &[Value::Object(timeout_cb), Value::Number(0.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
      }

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      Ok(())
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.bindings_host.webidl_dispatch_count, 3);
    Ok(())
  }

  #[test]
  fn webidl_host_slot_available_in_script_promise_microtask_and_timeout() -> crate::error::Result<()>
  {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();

      let mut scope = heap.scope();
      let global = realm.global_object();
      let webidl_dispatch = make_callback(
        vm,
        &mut scope,
        global,
        "__webidl_dispatch",
        cb_webidl_dispatch,
      );
      scope
        .push_root(Value::Object(webidl_dispatch))
        .expect("push root __webidl_dispatch");
      set_prop(
        &mut scope,
        global,
        "__webidl_dispatch",
        Value::Object(webidl_dispatch),
      );
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (_, window_realm) = host.vm_host_and_window_realm()?;
      window_realm.reset_interrupt();

      let result = window_realm.exec_script_with_hooks(
        &mut hooks,
        "globalThis.__webidl_dispatch();\n\
         Promise.resolve().then(globalThis.__webidl_dispatch);\n\
         queueMicrotask(globalThis.__webidl_dispatch);\n\
         setTimeout(globalThis.__webidl_dispatch, 0);",
      );

      if let Some(err) = hooks.finish(window_realm.heap_mut()) {
        return Err(err);
      }

      result
        .map(|_| ())
        .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.bindings_host.webidl_dispatch_count, 4);
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
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);

      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();

      {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");
        let clear_timeout = get_prop(&mut scope, global, "clearTimeout");
        let timeout_cb = make_callback(vm, &mut scope, global, "timeout_cb", cb_push_t);
        let id = vm
          .call_with_host_and_hooks(
            &mut host.host_ctx,
            &mut scope,
            &mut hooks,
            set_timeout,
            Value::Undefined,
            &[Value::Object(timeout_cb), Value::Number(0.0)],
          )
          .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        let _ = vm
          .call_with_host_and_hooks(
            &mut host.host_ctx,
            &mut scope,
            &mut hooks,
            clear_timeout,
            Value::Undefined,
            &[id],
          )
          .map_err(|e| crate::error::Error::Other(e.to_string()))?;
      }

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      Ok(())
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
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);

      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      {
        let mut scope = heap.scope();
        let set_interval = get_prop(&mut scope, global, "setInterval");
        let interval_cb = make_callback(vm, &mut scope, global, "interval_cb", cb_interval_tick);
        let id = vm
          .call_with_host_and_hooks(
            &mut host.host_ctx,
            &mut scope,
            &mut hooks,
            set_interval,
            Value::Undefined,
            &[Value::Object(interval_cb), Value::Number(0.0)],
          )
          .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        set_prop(&mut scope, global, "__interval_id", id);
      }

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }
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
  fn interval_continues_after_uncaught_exception() -> crate::error::Result<()> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>")?;
    let clock = Arc::new(VirtualClock::new());
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let event_loop = EventLoop::<crate::js::WindowHostState>::with_clock(clock_dyn);
    let mut host =
      crate::js::WindowHost::new_with_fetcher_and_event_loop(
        dom,
        "https://example.com/",
        Arc::new(NoFetchResourceFetcher),
        event_loop,
      )?;

    host.exec_script(
      r#"
      globalThis.__interval_ticks = 0;
      globalThis.__interval_after_throw = 0;
      globalThis.__interval_error_is_instance = false;
      globalThis.__interval_error_message = "";
      globalThis.__interval_onerror_called = false;
      globalThis.__interval_onerror_message = "";

      addEventListener("error", (e) => {
        globalThis.__interval_error_is_instance = (e instanceof ErrorEvent);
        globalThis.__interval_error_message = String(e && e.message);
      });

      globalThis.onerror = function (message) {
        globalThis.__interval_onerror_called = true;
        globalThis.__interval_onerror_message = String(message);
        return true; // cancel default reporting
      };

      let threw = false;
      let id = setInterval(() => {
        globalThis.__interval_ticks++;
        if (!threw) {
          threw = true;
          throw new Error("boom");
        }
        globalThis.__interval_after_throw++;
        if (globalThis.__interval_after_throw === 2) {
          clearInterval(id);
        }
      }, 10);
      "#,
    )?;

    // Drive the event loop through a few interval ticks deterministically.
    for _ in 0..3 {
      clock.advance(Duration::from_millis(10));
      assert_eq!(
        host.run_until_idle(RunLimits::unbounded())?,
        RunUntilIdleOutcome::Idle
      );
    }

    let ok = host.exec_script(
      r#"
      globalThis.__interval_ticks === 3 &&
      globalThis.__interval_after_throw === 2 &&
      globalThis.__interval_error_is_instance === true &&
      globalThis.__interval_error_message.includes("boom") &&
      globalThis.__interval_onerror_called === true &&
      globalThis.__interval_onerror_message.includes("boom")
      "#,
    )?;
    assert_eq!(ok, Value::Bool(true));

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
      set_prop(&mut scope, global, "__this_is_global", Value::Bool(false));
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);

      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");
        let cb = make_callback(vm, &mut scope, global, "cb", cb_capture_args);
        let x_s = scope.alloc_string("x").unwrap();
        scope
          .push_root(Value::String(x_s))
          .expect("push root arg string");
        vm.call_with_host_and_hooks(
          &mut host.host_ctx,
          &mut scope,
          &mut hooks,
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
      }

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      Ok(())
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    let (arg0, arg1, this_is_global) = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      (
        get_prop(&mut scope, global, "__arg0"),
        get_prop(&mut scope, global, "__arg1"),
        get_prop(&mut scope, global, "__this_is_global"),
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
    assert_eq!(this_is_global, Value::Bool(true));

    Ok(())
  }

  #[test]
  fn set_timeout_orders_by_due_time_then_registration_order() -> crate::error::Result<()> {
    fn cb_push_t10(
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
      push_log(scope, global, "t10");
      Ok(Value::Undefined)
    }

    fn cb_push_t5a(
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
      push_log(scope, global, "t5a");
      Ok(Value::Undefined)
    }

    fn cb_push_t5b(
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
      push_log(scope, global, "t5b");
      Ok(Value::Undefined)
    }

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock.clone());
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
      let mut scope = heap.scope();
      init_log(&mut scope, realm.global_object());
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);

      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");

        let cb10 = make_callback(vm, &mut scope, global, "cb10", cb_push_t10);
        let cb5a = make_callback(vm, &mut scope, global, "cb5a", cb_push_t5a);
        let cb5b = make_callback(vm, &mut scope, global, "cb5b", cb_push_t5b);

        vm.call_with_host_and_hooks(
          &mut host.host_ctx,
          &mut scope,
          &mut hooks,
          set_timeout,
          Value::Undefined,
          &[Value::Object(cb10), Value::Number(10.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        vm.call_with_host_and_hooks(
          &mut host.host_ctx,
          &mut scope,
          &mut hooks,
          set_timeout,
          Value::Undefined,
          &[Value::Object(cb5a), Value::Number(5.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
        vm.call_with_host_and_hooks(
          &mut host.host_ctx,
          &mut scope,
          &mut hooks,
          set_timeout,
          Value::Undefined,
          &[Value::Object(cb5b), Value::Number(5.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
      }

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }
      Ok(())
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    let log = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_log(heap, realm)
    };
    assert!(log.is_empty(), "expected no timers to be due at t=0");

    clock.advance(Duration::from_millis(5));
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    let log = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_log(heap, realm)
    };
    assert_eq!(log, vec!["t5a".to_string(), "t5b".to_string()]);

    clock.advance(Duration::from_millis(5));
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    let log = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_log(heap, realm)
    };
    assert_eq!(
      log,
      vec!["t5a".to_string(), "t5b".to_string(), "t10".to_string()]
    );

    Ok(())
  }

  #[test]
  fn timers_reject_string_handlers() -> Result<(), VmError> {
    let mut host = Host::new();
    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap)?;
    }

    // Run the script with an active EventLoop in hooks so this test remains valid even if the
    // implementation changes the relative ordering of:
    // - "string handler" rejection, and
    // - "called without an active EventLoop" checks.
    let mut event_loop = EventLoop::<Host>::new();
    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_vm_host_and_window_realm(
      &mut host.host_ctx,
      &mut host.window,
      Some(&mut host.bindings_host),
    );
    hooks.set_event_loop(&mut event_loop);

    let err_timeout = host
      .window
      .exec_script_with_hooks(&mut hooks, "setTimeout('1+1', 0)")
      .expect_err("expected setTimeout(string) to throw TypeError");
    assert_type_error_contains(
      host.window.heap_mut(),
      err_timeout,
      SET_TIMEOUT_STRING_HANDLER_ERROR,
    );

    let err_interval = host
      .window
      .exec_script_with_hooks(&mut hooks, "setInterval('1+1', 0)")
      .expect_err("expected setInterval(string) to throw TypeError");
    assert_type_error_contains(
      host.window.heap_mut(),
      err_interval,
      SET_INTERVAL_STRING_HANDLER_ERROR,
    );
    assert!(
      hooks.finish(host.window.heap_mut()).is_none(),
      "unexpected host hook error"
    );

    Ok(())
  }

  #[test]
  fn set_timeout_without_event_loop_throws_type_error() -> crate::error::Result<()> {
    let mut host = Host::new();
    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
    }

    let err = {
      // Deliberately create hooks without installing an active `EventLoop`. This simulates calling
      // `setTimeout` outside a FastRender event-loop turn.
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host)?;

      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();

      let err = {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");
        let cb = make_callback(vm, &mut scope, global, "noop", cb_noop);
        vm.call_with_host_and_hooks(
          &mut host.host_ctx,
          &mut scope,
          &mut hooks,
          set_timeout,
          Value::Undefined,
          &[Value::Object(cb), Value::Number(0.0)],
        )
        .expect_err("expected setTimeout() without an active EventLoop to throw")
      };

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }
      err
    };

    let rendered = vm_error_format::vm_error_to_string(host.window.heap_mut(), err);
    let first_line = rendered.lines().next().unwrap_or("");
    assert_eq!(
      first_line, "TypeError: setTimeout called without an active EventLoop",
      "unexpected error: {rendered:?}"
    );
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
      .expect_err("string callbacks should be rejected")
    };

    assert_type_error_contains(host.window.heap_mut(), err, "string callbacks");

    Ok(())
  }

  #[test]
  fn queue_microtask_rejects_string_callback_from_js() -> Result<(), VmError> {
    let mut host = Host::new();
    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap)?;
    }

    let err = host
      .window
      .exec_script("queueMicrotask('1+1')")
      .expect_err("expected queueMicrotask(string) to throw TypeError");
    assert_type_error_contains(
      host.window.heap_mut(),
      err,
      QUEUE_MICROTASK_STRING_HANDLER_ERROR,
    );

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
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);

      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();

      {
        let mut scope = heap.scope();
        let queue_microtask = get_prop(&mut scope, global, "queueMicrotask");
        let cb = make_callback(
          vm,
          &mut scope,
          global,
          "micro_cb",
          cb_record_this_is_undefined,
        );
        vm.call_with_host_and_hooks(
          &mut host.host_ctx,
          &mut scope,
          &mut hooks,
          queue_microtask,
          Value::Undefined,
          &[Value::Object(cb)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
      }

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      Ok(())
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
  fn uncaught_timeout_exception_dispatches_error_event_and_onerror_can_cancel() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
      window_realm.reset_interrupt();

      let script = r#"
        globalThis.__error_is_instance = false;
        globalThis.__error_message = "";
        globalThis.__onerror_called = false;
        globalThis.__onerror_message = "";

        addEventListener("error", (e) => {
          globalThis.__error_is_instance = (e instanceof ErrorEvent);
          globalThis.__error_message = String(e && e.message);
        });

        globalThis.onerror = function (message) {
          globalThis.__onerror_called = true;
          globalThis.__onerror_message = String(message);
          return true; // cancel default reporting
        };

        setTimeout(() => { throw new Error("boom"); }, 0);
      "#;

      let result = window_realm.exec_script_with_host_and_hooks(vm_host, &mut hooks, script);
      if let Some(err) = hooks.finish(window_realm.heap_mut()) {
        return Err(err);
      }
      result
        .map(|_| ())
        .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))
    })?;

    let mut errors: Vec<String> = Vec::new();
    assert_eq!(
      event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |err| {
        errors.push(err.to_string());
      })?,
      RunUntilIdleOutcome::Idle
    );
    assert!(
      errors.is_empty(),
      "expected onerror cancellation to suppress host error reporting, got errors={errors:?}"
    );

    let (error_is_instance, error_message, onerror_called, onerror_message) = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      let error_is_instance = matches!(get_prop(&mut scope, global, "__error_is_instance"), Value::Bool(true));
      let error_message = match get_prop(&mut scope, global, "__error_message") {
        Value::String(s) => scope.heap().get_string(s).unwrap().to_utf8_lossy(),
        _ => String::new(),
      };
      let onerror_called = matches!(get_prop(&mut scope, global, "__onerror_called"), Value::Bool(true));
      let onerror_message = match get_prop(&mut scope, global, "__onerror_message") {
        Value::String(s) => scope.heap().get_string(s).unwrap().to_utf8_lossy(),
        _ => String::new(),
      };
      (error_is_instance, error_message, onerror_called, onerror_message)
    };

    assert!(error_is_instance, "expected `error` listener to see an ErrorEvent instance");
    assert!(
      error_message.contains("boom"),
      "expected ErrorEvent.message to contain thrown message, got {error_message:?}"
    );
    assert!(onerror_called, "expected window.onerror to run");
    assert!(
      onerror_message.contains("boom"),
      "expected onerror message arg to contain thrown message, got {onerror_message:?}"
    );

    Ok(())
  }

  #[test]
  fn uncaught_request_idle_callback_exception_dispatches_error_event_and_onerror_can_cancel(
  ) -> crate::error::Result<()> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>")?;
    let mut host = make_window_host(dom, "https://example.com/")?;

    host.exec_script(
      r#"
        globalThis.__idle_error_is_instance = false;
        globalThis.__idle_error_message = "";
        globalThis.__idle_onerror_called = false;

        addEventListener("error", (e) => {
          globalThis.__idle_error_is_instance = (e instanceof ErrorEvent);
          globalThis.__idle_error_message = String(e && e.message);
        });

        globalThis.onerror = function () {
          globalThis.__idle_onerror_called = true;
          return true; // cancel default reporting
        };

        requestIdleCallback(() => { throw new Error("idleboom"); });
      "#,
    )?;

    let mut errors: Vec<String> = Vec::new();
    assert_eq!(
      host.run_until_idle_handling_errors(RunLimits::unbounded(), |err| {
        errors.push(err.to_string());
      })?,
      RunUntilIdleOutcome::Idle
    );
    assert!(
      errors.is_empty(),
      "expected onerror cancellation to suppress host error reporting, got errors={errors:?}"
    );

    let ok = host.exec_script(
      r#"
        globalThis.__idle_error_is_instance === true &&
        globalThis.__idle_error_message.includes("idleboom") &&
        globalThis.__idle_onerror_called === true
      "#,
    )?;
    assert_eq!(ok, Value::Bool(true));

    Ok(())
  }

  #[test]
  fn uncaught_queue_microtask_exception_dispatches_error_event_and_onerror_can_cancel() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<Host>::with_clock(clock);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Host>(vm, realm, heap).unwrap();
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
      window_realm.reset_interrupt();

      let script = r#"
        globalThis.__micro_error_is_instance = false;
        globalThis.__micro_error_message = "";
        globalThis.__micro_onerror_called = false;

        addEventListener("error", (e) => {
          globalThis.__micro_error_is_instance = (e instanceof ErrorEvent);
          globalThis.__micro_error_message = String(e && e.message);
        });

        globalThis.onerror = function () {
          globalThis.__micro_onerror_called = true;
          return true; // cancel default reporting
        };

        queueMicrotask(() => { throw new Error("microboom"); });
      "#;

      let result = window_realm.exec_script_with_host_and_hooks(vm_host, &mut hooks, script);
      if let Some(err) = hooks.finish(window_realm.heap_mut()) {
        return Err(err);
      }
      result
        .map(|_| ())
        .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))
    })?;

    let mut errors: Vec<String> = Vec::new();
    assert_eq!(
      event_loop.run_until_idle_handling_errors(&mut host, RunLimits::unbounded(), |err| {
        errors.push(err.to_string());
      })?,
      RunUntilIdleOutcome::Idle
    );
    assert!(
      errors.is_empty(),
      "expected onerror cancellation to suppress host error reporting, got errors={errors:?}"
    );

    let (error_is_instance, error_message, onerror_called) = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      let error_is_instance = matches!(
        get_prop(&mut scope, global, "__micro_error_is_instance"),
        Value::Bool(true)
      );
      let error_message = match get_prop(&mut scope, global, "__micro_error_message") {
        Value::String(s) => scope.heap().get_string(s).unwrap().to_utf8_lossy(),
        _ => String::new(),
      };
      let onerror_called = matches!(
        get_prop(&mut scope, global, "__micro_onerror_called"),
        Value::Bool(true)
      );
      (error_is_instance, error_message, onerror_called)
    };

    assert!(error_is_instance, "expected `error` listener to see an ErrorEvent instance");
    assert!(
      error_message.contains("microboom"),
      "expected ErrorEvent.message to contain thrown message, got {error_message:?}"
    );
    assert!(onerror_called, "expected window.onerror to run");

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
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);

      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");
        let timeout_cb = make_callback(vm, &mut scope, global, "timeout_cb", cb_noop);
        let _ = vm
          .call_with_host_and_hooks(
            &mut host.host_ctx,
            &mut scope,
            &mut hooks,
            set_timeout,
            Value::Undefined,
            &[Value::Object(timeout_cb), Value::Number(0.0)],
          )
          .map_err(|e| crate::error::Error::Other(e.to_string()))?;
      }

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }
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
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);

      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();

      {
        let mut scope = heap.scope();
        let set_timeout = get_prop(&mut scope, global, "setTimeout");

        let timeout_cb =
          make_callback(vm, &mut scope, global, "timeout_cb", cb_enqueue_promise_job);

        vm.call_with_host_and_hooks(
          &mut host.host_ctx,
          &mut scope,
          &mut hooks,
          set_timeout,
          Value::Object(global),
          &[Value::Object(timeout_cb), Value::Number(0.0)],
        )
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
      }

      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }

      Ok(())
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

      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
      hooks.set_event_loop(event_loop);
      let log1 = Arc::clone(&log_for_task);
      hooks.host_enqueue_promise_job(
        vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
          log1.lock().unwrap().push("job1");
          Ok(())
        })
        .unwrap(),
        None,
      );
      let log2 = Arc::clone(&log_for_task);
      hooks.host_enqueue_promise_job(
        vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
          log2.lock().unwrap().push("job2");
          Ok(())
        })
        .unwrap(),
        None,
      );
      if let Some(err) = hooks.finish(host.window_realm()?.heap_mut()) {
        return Err(err);
      }

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

    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host)?;
    let log_for_job1 = Arc::clone(&log);
    hooks.set_event_loop(&mut event_loop);

    hooks.host_enqueue_promise_job(
      vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, hooks| {
        log_for_job1.lock().unwrap().push("job1");

        let log_for_job2 = Arc::clone(&log_for_job1);
        let job2 = vm_js::Job::new(vm_js::JobKind::Promise, move |_ctx, _hooks| {
          log_for_job2.lock().unwrap().push("job2");
          Ok(())
        })?;
        hooks.host_enqueue_promise_job(job2, None);

        Ok(())
      })
      .unwrap(),
      None,
    );

    assert!(hooks.finish(host.window_realm()?.heap_mut()).is_none());
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
      })
      .unwrap();
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
      })
      .unwrap();
      let heap = host.window.heap_mut();
      let mut ctx = HeapRootContext { heap };
      let root = job.add_root(&mut ctx, Value::Undefined).unwrap();
      (root, job)
    };

    assert_eq!(host.window.heap().get_root(root1), Some(Value::Null));
    assert_eq!(host.window.heap().get_root(root2), Some(Value::Undefined));

    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host)?;
    hooks.set_event_loop(&mut event_loop);
    hooks.host_enqueue_promise_job(job1, None);
    hooks.host_enqueue_promise_job(job2, None);

    let err = hooks
      .finish(host.window.heap_mut())
      .expect("expected enqueue error");
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

    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host)?;
    hooks.set_event_loop(&mut event_loop);
    hooks.host_enqueue_promise_job(
      vm_js::Job::new(vm_js::JobKind::Promise, |_ctx, _hooks| {
        Err(vm_js::VmError::TypeError("boom"))
      })
      .unwrap(),
      None,
    );
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
  fn microtask_checkpoint_fails_cleanly_when_window_realm_is_unavailable() -> crate::error::Result<()> {
    struct FlakyHost {
      host_ctx: (),
      window: WindowRealm,
      allow_realm: bool,
    }

    impl WindowRealmHost for FlakyHost {
      fn vm_host_and_window_realm(
        &mut self,
      ) -> crate::error::Result<(&mut dyn VmHost, &mut WindowRealm)> {
        if !self.allow_realm {
          return Err(crate::error::Error::Other(
            "WindowRealmHost failed to provide a WindowRealm".to_string(),
          ));
        }
        Ok((&mut self.host_ctx, &mut self.window))
      }
    }

    let clock = Arc::new(VirtualClock::new());
    clock.set_now(Duration::from_millis(0));
    let clock_for_loop: Arc<dyn crate::js::Clock> = clock.clone();
    let clock_for_window: Arc<dyn crate::js::Clock> = clock.clone();

    let mut host = FlakyHost {
      host_ctx: (),
      window: WindowRealm::new(
        WindowRealmConfig::new("https://example.invalid/").with_clock(clock_for_window),
      )
      .unwrap(),
      allow_realm: true,
    };
    let mut event_loop = EventLoop::<FlakyHost>::with_clock(clock_for_loop);

    // Install timer/microtask bindings and schedule a microtask while the realm is available.
    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<FlakyHost>(vm, realm, heap).unwrap();
    }

    {
      let mut hooks = VmJsEventLoopHooks::<FlakyHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
      let result = window_realm.exec_script_with_host_and_hooks(
        vm_host,
        &mut hooks,
        "queueMicrotask(() => { globalThis.__ran = true; });",
      );
      if let Some(err) = hooks.finish(window_realm.heap_mut()) {
        return Err(err);
      }
      result.map_err(|err| crate::error::Error::Other(err.to_string()))?;
    }

    // Simulate the embedding no longer being able to provide a realm (e.g. realm teardown / OOM).
    host.allow_realm = false;
    let err = event_loop
      .perform_microtask_checkpoint(&mut host)
      .expect_err("expected microtask checkpoint to fail without a WindowRealm");
    assert!(
      err.to_string()
        .contains("WindowRealmHost failed to provide a WindowRealm"),
      "unexpected error: {err}"
    );
    Ok(())
  }

  #[test]
  fn timer_task_fails_cleanly_when_window_realm_is_unavailable() -> crate::error::Result<()> {
    struct FlakyHost {
      host_ctx: (),
      window: WindowRealm,
      allow_realm: bool,
    }

    impl WindowRealmHost for FlakyHost {
      fn vm_host_and_window_realm(
        &mut self,
      ) -> crate::error::Result<(&mut dyn VmHost, &mut WindowRealm)> {
        if !self.allow_realm {
          return Err(crate::error::Error::Other(
            "WindowRealmHost failed to provide a WindowRealm".to_string(),
          ));
        }
        Ok((&mut self.host_ctx, &mut self.window))
      }
    }

    let clock = Arc::new(VirtualClock::new());
    clock.set_now(Duration::from_millis(0));
    let clock_for_loop: Arc<dyn crate::js::Clock> = clock.clone();
    let clock_for_window: Arc<dyn crate::js::Clock> = clock.clone();

    let mut host = FlakyHost {
      host_ctx: (),
      window: WindowRealm::new(
        WindowRealmConfig::new("https://example.invalid/").with_clock(clock_for_window),
      )
      .unwrap(),
      allow_realm: true,
    };
    let mut event_loop = EventLoop::<FlakyHost>::with_clock(clock_for_loop);

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<FlakyHost>(vm, realm, heap).unwrap();
    }

    // Schedule a timer callback while the realm is available.
    {
      let mut hooks = VmJsEventLoopHooks::<FlakyHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
      let result = window_realm.exec_script_with_host_and_hooks(
        vm_host,
        &mut hooks,
        "setTimeout(() => { globalThis.__ran = true; }, 0);",
      );
      if let Some(err) = hooks.finish(window_realm.heap_mut()) {
        return Err(err);
      }
      result.map_err(|err| crate::error::Error::Other(err.to_string()))?;
    }

    // Ensure the timer is due.
    clock.advance(Duration::from_millis(1));

    host.allow_realm = false;
    let err = event_loop
      .run_until_idle(&mut host, RunLimits::unbounded())
      .expect_err("expected timer callback to fail without a WindowRealm");
    assert!(
      err.to_string()
        .contains("WindowRealmHost failed to provide a WindowRealm"),
      "unexpected error: {err}"
    );
    Ok(())
  }

  #[test]
  fn browser_tab_window_realm_host_impl_does_not_abort() {
    assert!(
      !include_str!("../../api/browser_tab.rs").contains("std::process::abort"),
      "BrowserTabHost realm acquisition should not abort the process"
    );
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
      ctx.call(hooks, Value::Object(callback_func), Value::Undefined, &[])?;
      Ok(())
    })
    .unwrap();
    {
      let heap = host.window.heap_mut();
      let mut ctx = HeapRootContext { heap };
      job
        .add_root(&mut ctx, Value::Object(callback_func))
        .expect("root callback");
    }

    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host)?;
    hooks.set_event_loop(&mut event_loop);
    hooks.host_enqueue_promise_job(job, Some(realm));
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
      fn vm_host_and_window_realm(
        &mut self,
      ) -> crate::error::Result<(&mut dyn VmHost, &mut WindowRealm)> {
        let GcHost { host_ctx, window } = self;
        Ok((host_ctx, window))
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

    let mut hooks = VmJsEventLoopHooks::<GcHost>::new_with_host(&mut host)?;
    hooks.set_event_loop(&mut event_loop);
    {
      let window = host.window_realm()?;
      let mut scope = window.heap_mut().scope();

      let callback = {
        let name = scope.alloc_string("onFulfilled").unwrap();
        scope.alloc_native_function(call_id, None, name, 1).unwrap()
      };
      scope.push_root(Value::Object(callback)).unwrap();
      callback_obj = Some(callback);

      let argument = scope.alloc_object().unwrap();
      scope.push_root(Value::Object(argument)).unwrap();
      argument_obj = Some(argument);

      let job_callback = hooks
        .host_make_job_callback(callback)
        .expect("host_make_job_callback");
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
    }
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
  fn vm_js_promise_jobs_discard_roots_when_event_loop_is_dropped() -> crate::error::Result<()> {
    let mut host = Host::new();
    let mut event_loop = EventLoop::<Host>::new();

    let rooted_obj = {
      let heap = host.window.heap_mut();
      let mut scope = heap.scope();
      scope.alloc_object().expect("alloc object")
    };

    let (root, job) = {
      let mut job =
        vm_js::Job::new(vm_js::JobKind::Promise, |_ctx, _hooks| Ok(())).expect("create job");
      let heap = host.window.heap_mut();
      let mut ctx = HeapRootContext { heap };
      let root = job
        .add_root(&mut ctx, Value::Object(rooted_obj))
        .expect("root object");
      (root, job)
    };

    assert_eq!(
      host.window.heap().get_root(root),
      Some(Value::Object(rooted_obj))
    );

    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host)?;
    hooks.set_event_loop(&mut event_loop);
    hooks.host_enqueue_promise_job(job, None);
    assert!(hooks.finish(host.window.heap_mut()).is_none());

    // The job should keep the object alive until it is run or discarded.
    host.window.heap_mut().collect_garbage();
    assert!(
      host.window.heap().is_valid_object(rooted_obj),
      "expected object to stay alive while the Promise job is still queued"
    );

    // Drop the host microtask queue without running it: this should discard the job and clean up its
    // persistent roots while the heap is still alive.
    drop(event_loop);

    assert_eq!(
      host.window.heap().get_root(root),
      None,
      "expected Promise job to discard its persistent roots when the microtask is dropped"
    );

    host.window.heap_mut().collect_garbage();
    assert!(
      !host.window.heap().is_valid_object(rooted_obj),
      "expected rooted object to become collectible after job discard"
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

    let job_callback = vm_js::JobCallback::new(callback_func).expect("JobCallback::new");

    let mut job = vm_js::Job::new(vm_js::JobKind::Promise, move |ctx, hooks| {
      hooks.host_call_job_callback(ctx, &job_callback, Value::Undefined, &[])?;
      Ok(())
    })
    .unwrap();
    {
      let heap = host.window.heap_mut();
      let mut ctx = HeapRootContext { heap };
      job
        .add_root(&mut ctx, Value::Object(callback_func))
        .expect("root callback func");
    }

    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(&mut host)?;
    hooks.set_event_loop(&mut event_loop);
    hooks.host_enqueue_promise_job(job, None);
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
