//! WHATWG `AbortController` / `AbortSignal` bindings for the `vm-js` Window realm.
//!
//! This is a minimal, spec-shaped implementation intended for:
//! - libraries that expect `AbortController` to exist,
//! - `fetch()` request cancellation via `RequestInit.signal`.
//!
//! ## Error / reason shape
//!
//! FastRender's `vm-js` embedding does not currently expose a full `DOMException` implementation to
//! scripts. To keep abort behavior stable and testable, this module uses a deterministic
//! **DOMException-like** object as the default abort reason:
//!
//! ```js
//! { name: "AbortError", message: "This operation was aborted" }
//! ```
//!
//! and for `AbortSignal.timeout(ms)`:
//!
//! ```js
//! { name: "TimeoutError", message: "The operation timed out" }
//! ```
//!
//! These are plain objects (not `Error` instances); callers should key off `reason.name`.

use vm_js::iterator;
use vm_js::{
  GcObject, Heap, HostSlots, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm,
  VmError, VmHost, VmHostHooks,
};

const ABORT_CONTROLLER_HOST_TAG: u64 = 0x4142_4F52_5443_5452; // "ABORTCTR"
const ABORT_SIGNAL_HOST_TAG: u64 = 0x4142_4F52_5453_4947; // "ABORTSIG"

const CONTROLLER_SIGNAL_INTERNAL_KEY: &str = "__fastrender_abort_controller_signal";
const SIGNAL_BRAND_KEY: &str = "__fastrender_abort_signal";
const EVENT_TARGET_BRAND_KEY: &str = "__fastrender_event_target";
/// Internal reference to the realm's `Event` constructor.
///
/// AbortSignal dispatches an `abort` event when transitioning to the aborted state. Once
/// `EventTarget.dispatchEvent` becomes spec-correct (WebIDL conversion), the event argument must be
/// a real `Event` object rather than a duck-typed `{ type: "abort" }` plain object.
const SIGNAL_EVENT_CTOR_INTERNAL_KEY: &str = "__fastrender_abort_signal_event_ctor";
/// Hard cap on how many signals `AbortSignal.any(signals)` will consume from the provided iterable.
///
/// This is a hostile-input guardrail: the `signals` argument is specified as a WebIDL `sequence`,
/// meaning the caller controls an *iterable*. That iterable can be extremely large or even infinite,
/// so we cap the number of values we will consume. Keep this low enough that worst-case iteration
/// stays practical even in debug builds (the `vm-js` iterator helpers currently allocate per-step).
const MAX_ABORT_SIGNAL_ANY_INPUT_SIGNALS: u32 = 1_024;
const ABORT_SIGNAL_ANY_TOO_MANY_SIGNALS_ERROR: &str = "AbortSignal.any input is too large";

fn data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data { value, writable },
  }
}

fn ctor_link_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: false,
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

fn get_own_data_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Result<Value, VmError> {
  let key = alloc_key(scope, name)?;
  Ok(
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)?
      .unwrap_or(Value::Undefined),
  )
}

fn set_own_data_prop(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
  writable: bool,
) -> Result<(), VmError> {
  // Root `obj` + `value` while allocating the property key: `alloc_key` can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, data_desc(value, writable))
}

fn require_object(this: Value, err: &'static str) -> Result<GcObject, VmError> {
  match this {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::TypeError(err)),
  }
}

fn require_abort_controller(
  scope: &Scope<'_>,
  this: Value,
  err: &'static str,
) -> Result<GcObject, VmError> {
  let obj = require_object(this, err)?;
  let Some(slots) = scope.heap().object_host_slots(obj)? else {
    return Err(VmError::TypeError(err));
  };
  if slots.a == ABORT_CONTROLLER_HOST_TAG {
    Ok(obj)
  } else {
    Err(VmError::TypeError(err))
  }
}

fn require_abort_signal(
  scope: &Scope<'_>,
  this: Value,
  err: &'static str,
) -> Result<GcObject, VmError> {
  let obj = require_object(this, err)?;
  let Some(slots) = scope.heap().object_host_slots(obj)? else {
    return Err(VmError::TypeError(err));
  };
  if slots.a == ABORT_SIGNAL_HOST_TAG {
    Ok(obj)
  } else {
    Err(VmError::TypeError(err))
  }
}

fn create_dom_exception_like(
  scope: &mut Scope<'_>,
  name: &str,
  message: &str,
) -> Result<Value, VmError> {
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;

  let name_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(name_s))?;
  let message_s = scope.alloc_string(message)?;
  scope.push_root(Value::String(message_s))?;

  let name_key = alloc_key(scope, "name")?;
  let message_key = alloc_key(scope, "message")?;

  scope.define_property(
    obj,
    name_key,
    data_desc(Value::String(name_s), /* writable */ false),
  )?;
  scope.define_property(
    obj,
    message_key,
    data_desc(Value::String(message_s), /* writable */ false),
  )?;

  Ok(Value::Object(obj))
}

fn create_default_abort_reason(scope: &mut Scope<'_>) -> Result<Value, VmError> {
  create_dom_exception_like(scope, "AbortError", "This operation was aborted")
}

fn create_timeout_reason(scope: &mut Scope<'_>) -> Result<Value, VmError> {
  create_dom_exception_like(scope, "TimeoutError", "The operation timed out")
}

fn create_abort_event(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  signal_obj: GcObject,
) -> Result<Value, VmError> {
  // Construct a real `Event` instance (`new Event("abort")`) so the event remains dispatchable once
  // `EventTarget.dispatchEvent` performs WebIDL conversion and rejects non-Event objects.
  //
  // The `Event` constructor is captured during `install_window_abort_bindings` and stored on each
  // signal so abort dispatch does not consult potentially-mutated globals.
  scope.push_root(Value::Object(signal_obj))?;
  let ctor = get_own_data_prop(scope, signal_obj, SIGNAL_EVENT_CTOR_INTERNAL_KEY)?;
  let Value::Object(ctor) = ctor else {
    return Err(VmError::InvariantViolation(
      "AbortSignal missing internal Event constructor",
    ));
  };

  // Root `ctor` and the argument string while constructing (both allocation and construction can
  // trigger GC).
  scope.push_root(Value::Object(ctor))?;
  let type_s = scope.alloc_string("abort")?;
  scope.push_root(Value::String(type_s))?;
  vm.construct_with_host_and_hooks(
    host,
    scope,
    host_hooks,
    Value::Object(ctor),
    &[Value::String(type_s)],
    Value::Object(ctor),
  )
}

fn abort_signal(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  signal_obj: GcObject,
  reason: Value,
  dispatch_event: bool,
) -> Result<(), VmError> {
  // `signal.aborted` must transition to true at most once.
  let already_aborted = matches!(
    get_own_data_prop(scope, signal_obj, "aborted")?,
    Value::Bool(true)
  );
  if already_aborted {
    return Ok(());
  }

  set_own_data_prop(
    scope,
    signal_obj,
    "aborted",
    Value::Bool(true),
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope, signal_obj, "reason", reason, /* writable */ false,
  )?;

  // Dispatch the `abort` event and call `onabort`.
  if dispatch_event {
    let ev = create_abort_event(vm, scope, host, host_hooks, signal_obj)?;
    scope.push_root(ev)?;

    let dispatch_fn = {
      let key = alloc_key(scope, "dispatchEvent")?;
      vm.get_with_host_and_hooks(host, scope, host_hooks, signal_obj, key)?
    };
    if scope.heap().is_callable(dispatch_fn).unwrap_or(false) {
      // Ignore the return value; for AbortSignal it is always used for notification, not for
      // cancelation.
      let _ = vm.call_with_host_and_hooks(
        host,
        scope,
        host_hooks,
        dispatch_fn,
        Value::Object(signal_obj),
        &[ev],
      );
    }

    let onabort = get_own_data_prop(scope, signal_obj, "onabort")?;
    if scope.heap().is_callable(onabort).unwrap_or(false) {
      // Like DOM event dispatch, exceptions from event handlers should not make abort() throw.
      let _ = vm.call_with_host_and_hooks(
        host,
        scope,
        host_hooks,
        onabort,
        Value::Object(signal_obj),
        &[ev],
      );
    }
  }

  Ok(())
}

fn abort_controller_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "AbortController constructor cannot be invoked without 'new'",
  ))
}

fn abort_controller_ctor_construct(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let ctor = match new_target {
    Value::Object(obj) => obj,
    _ => callee,
  };

  let prototype_key = alloc_key(scope, "prototype")?;
  let proto = scope
    .heap()
    .object_get_own_data_property_value(ctor, &prototype_key)?
    .and_then(|v| match v {
      Value::Object(obj) => Some(obj),
      _ => None,
    });

  let controller = scope.alloc_object()?;
  scope.push_root(Value::Object(controller))?;
  if let Some(proto) = proto {
    scope
      .heap_mut()
      .object_set_prototype(controller, Some(proto))?;
  }
  scope.heap_mut().object_set_host_slots(
    controller,
    HostSlots {
      a: ABORT_CONTROLLER_HOST_TAG,
      b: 0,
    },
  )?;

  // Create the associated signal.
  let signal_proto = match get_own_data_prop(scope, ctor, "__fastrender_abort_signal_proto")? {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "AbortController missing internal AbortSignal prototype",
      ))
    }
  };

  let signal = scope.alloc_object_with_prototype(Some(signal_proto))?;
  scope.push_root(Value::Object(signal))?;
  scope.heap_mut().object_set_host_slots(
    signal,
    HostSlots {
      a: ABORT_SIGNAL_HOST_TAG,
      b: 0,
    },
  )?;
  set_own_data_prop(
    scope,
    signal,
    EVENT_TARGET_BRAND_KEY,
    Value::Bool(true),
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    signal,
    SIGNAL_BRAND_KEY,
    Value::Bool(true),
    /* writable */ false,
  )?;
  let event_ctor = match get_own_data_prop(scope, signal_proto, SIGNAL_EVENT_CTOR_INTERNAL_KEY)? {
    Value::Object(obj) => Value::Object(obj),
    _ => {
      return Err(VmError::InvariantViolation(
        "AbortSignal prototype missing internal Event constructor",
      ))
    }
  };
  set_own_data_prop(
    scope,
    signal,
    SIGNAL_EVENT_CTOR_INTERNAL_KEY,
    event_ctor,
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    signal,
    "aborted",
    Value::Bool(false),
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    signal,
    "reason",
    Value::Undefined,
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    signal,
    "onabort",
    Value::Null,
    /* writable */ true,
  )?;

  // Public + internal links.
  set_own_data_prop(
    scope,
    controller,
    "signal",
    Value::Object(signal),
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    controller,
    CONTROLLER_SIGNAL_INTERNAL_KEY,
    Value::Object(signal),
    /* writable */ false,
  )?;

  Ok(Value::Object(controller))
}

fn abort_controller_abort_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let controller = require_abort_controller(scope, this, "AbortController.abort: illegal invocation")?;
  let signal_val = get_own_data_prop(scope, controller, CONTROLLER_SIGNAL_INTERNAL_KEY)?;
  let Value::Object(signal_obj) = signal_val else {
    return Err(VmError::TypeError(
      "AbortController.abort: illegal invocation",
    ));
  };

  let reason_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let reason = if matches!(reason_arg, Value::Undefined) {
    create_default_abort_reason(scope)?
  } else {
    reason_arg
  };
  scope.push_root(reason)?;

  abort_signal(
    vm, scope, host, host_hooks, signal_obj, reason, /* dispatch_event */ true,
  )?;
  Ok(Value::Undefined)
}

fn abort_signal_ctor_illegal(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("Illegal constructor"))
}

fn abort_signal_ctor_construct_illegal(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  Err(VmError::TypeError("Illegal constructor"))
}

fn abort_signal_throw_if_aborted_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let signal_obj = require_abort_signal(
    scope,
    this,
    "AbortSignal.throwIfAborted: illegal invocation",
  )?;
  let aborted = get_own_data_prop(scope, signal_obj, "aborted")?;
  if matches!(aborted, Value::Bool(true)) {
    let reason = get_own_data_prop(scope, signal_obj, "reason")?;
    return Err(VmError::Throw(reason));
  }
  Ok(Value::Undefined)
}

// Slot indices for `AbortSignal.*` static method native functions.
const SLOT_SIGNAL_PROTO: usize = 0;
const SLOT_GLOBAL: usize = 1;

fn signal_proto_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots
    .get(SLOT_SIGNAL_PROTO)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::InvariantViolation(
      "AbortSignal native missing signal prototype slot",
    )),
  }
}

fn global_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots.get(SLOT_GLOBAL).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::InvariantViolation(
      "AbortSignal native missing global slot",
    )),
  }
}

fn abort_signal_static_abort_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let proto = signal_proto_from_callee(scope, callee)?;

  let signal = scope.alloc_object_with_prototype(Some(proto))?;
  scope.push_root(Value::Object(signal))?;
  scope.heap_mut().object_set_host_slots(
    signal,
    HostSlots {
      a: ABORT_SIGNAL_HOST_TAG,
      b: 0,
    },
  )?;
  set_own_data_prop(
    scope,
    signal,
    EVENT_TARGET_BRAND_KEY,
    Value::Bool(true),
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    signal,
    SIGNAL_BRAND_KEY,
    Value::Bool(true),
    /* writable */ false,
  )?;
  let event_ctor = match get_own_data_prop(scope, proto, SIGNAL_EVENT_CTOR_INTERNAL_KEY)? {
    Value::Object(obj) => Value::Object(obj),
    _ => {
      return Err(VmError::InvariantViolation(
        "AbortSignal prototype missing internal Event constructor",
      ))
    }
  };
  set_own_data_prop(
    scope,
    signal,
    SIGNAL_EVENT_CTOR_INTERNAL_KEY,
    event_ctor,
    /* writable */ false,
  )?;

  let reason_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let reason = if matches!(reason_arg, Value::Undefined) {
    create_default_abort_reason(scope)?
  } else {
    reason_arg
  };
  scope.push_root(reason)?;

  set_own_data_prop(
    scope,
    signal,
    "aborted",
    Value::Bool(true),
    /* writable */ false,
  )?;
  set_own_data_prop(scope, signal, "reason", reason, /* writable */ false)?;
  set_own_data_prop(
    scope,
    signal,
    "onabort",
    Value::Null,
    /* writable */ true,
  )?;

  Ok(Value::Object(signal))
}

fn abort_signal_static_timeout_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host_ctx: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let global = global_from_callee(scope, callee)?;
  let proto = signal_proto_from_callee(scope, callee)?;

  let ms_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut ms = scope.heap_mut().to_number(ms_value)?;
  if !ms.is_finite() || ms.is_nan() {
    ms = 0.0;
  }
  ms = ms.trunc();
  if ms < 0.0 {
    ms = 0.0;
  }

  // Create the signal and schedule a `setTimeout` to abort it.
  let signal = scope.alloc_object_with_prototype(Some(proto))?;
  scope.push_root(Value::Object(signal))?;
  scope.heap_mut().object_set_host_slots(
    signal,
    HostSlots {
      a: ABORT_SIGNAL_HOST_TAG,
      b: 0,
    },
  )?;
  set_own_data_prop(
    scope,
    signal,
    EVENT_TARGET_BRAND_KEY,
    Value::Bool(true),
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    signal,
    SIGNAL_BRAND_KEY,
    Value::Bool(true),
    /* writable */ false,
  )?;
  let event_ctor = match get_own_data_prop(scope, proto, SIGNAL_EVENT_CTOR_INTERNAL_KEY)? {
    Value::Object(obj) => Value::Object(obj),
    _ => {
      return Err(VmError::InvariantViolation(
        "AbortSignal prototype missing internal Event constructor",
      ))
    }
  };
  set_own_data_prop(
    scope,
    signal,
    SIGNAL_EVENT_CTOR_INTERNAL_KEY,
    event_ctor,
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    signal,
    "aborted",
    Value::Bool(false),
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    signal,
    "reason",
    Value::Undefined,
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    signal,
    "onabort",
    Value::Null,
    /* writable */ true,
  )?;

  let set_timeout = get_own_data_prop(scope, global, "setTimeout")?;
  if !scope.heap().is_callable(set_timeout).unwrap_or(false) {
    return Err(VmError::TypeError(
      "AbortSignal.timeout requires setTimeout to be installed",
    ));
  }

  // Callback invoked by setTimeout to perform the abort.
  let abort_call_id = vm.register_native_call(abort_timeout_callback_native)?;
  let name = scope.alloc_string("AbortSignal.timeout callback")?;
  scope.push_root(Value::String(name))?;
  let callback = scope.alloc_native_function_with_slots(
    abort_call_id,
    None,
    name,
    0,
    &[Value::Object(signal)],
  )?;
  scope.heap_mut().object_set_prototype(
    callback,
    Some(
      vm.intrinsics()
        .ok_or(VmError::Unimplemented("missing intrinsics"))?
        .function_prototype(),
    ),
  )?;
  scope.push_root(Value::Object(callback))?;

  // Call setTimeout(callback, ms).
  let _ = vm.call_with_host_and_hooks(
    host_ctx,
    scope,
    host_hooks,
    set_timeout,
    Value::Object(global),
    &[Value::Object(callback), Value::Number(ms)],
  )?;

  Ok(Value::Object(signal))
}

fn abort_timeout_callback_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host_ctx: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let signal = slots.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(signal_obj) = signal else {
    return Err(VmError::InvariantViolation(
      "AbortSignal.timeout callback missing signal slot",
    ));
  };

  let reason = create_timeout_reason(scope)?;
  scope.push_root(reason)?;
  abort_signal(
    vm, scope, host_ctx, host_hooks, signal_obj, reason, /* dispatch_event */ true,
  )?;
  Ok(Value::Undefined)
}

fn abort_signal_static_any_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host_ctx: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let proto = signal_proto_from_callee(scope, callee)?;

  let iterable = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut record = iterator::get_iterator(vm, host_ctx, host_hooks, scope, iterable)?;

  let signal = scope.alloc_object_with_prototype(Some(proto))?;
  scope.push_root(Value::Object(signal))?;
  scope.heap_mut().object_set_host_slots(
    signal,
    HostSlots {
      a: ABORT_SIGNAL_HOST_TAG,
      b: 0,
    },
  )?;
  set_own_data_prop(
    scope,
    signal,
    EVENT_TARGET_BRAND_KEY,
    Value::Bool(true),
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    signal,
    SIGNAL_BRAND_KEY,
    Value::Bool(true),
    /* writable */ false,
  )?;
  let event_ctor = match get_own_data_prop(scope, proto, SIGNAL_EVENT_CTOR_INTERNAL_KEY)? {
    Value::Object(obj) => Value::Object(obj),
    _ => {
      return Err(VmError::InvariantViolation(
        "AbortSignal prototype missing internal Event constructor",
      ))
    }
  };
  set_own_data_prop(
    scope,
    signal,
    SIGNAL_EVENT_CTOR_INTERNAL_KEY,
    event_ctor,
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    signal,
    "aborted",
    Value::Bool(false),
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    signal,
    "reason",
    Value::Undefined,
    /* writable */ false,
  )?;
  set_own_data_prop(
    scope,
    signal,
    "onabort",
    Value::Null,
    /* writable */ true,
  )?;

  // Pre-allocate frequently accessed property keys so we do not repeatedly allocate strings while
  // consuming large iterables.
  let aborted_key = alloc_key(scope, "aborted")?;
  let reason_key = alloc_key(scope, "reason")?;

  // Root all collected signals on the stack so:
  // - aborted reasons remain reachable,
  // - we can delay `addEventListener` side effects until after we've validated input bounds.
  let mut sources: Vec<GcObject> = Vec::new();
  let mut count: u32 = 0;

  let mut pre_aborted_reason: Option<Value> = None;
  let mut pre_aborted = false;

  let collect_result: Result<(), VmError> = (|| {
    loop {
      // `AbortSignal.any` can be invoked with extremely large iterables. Ensure VM budgets apply even
      // while we're running this native loop: some iterators (notably `IteratorKind::Array`) do not
      // execute any JS bytecode per step, so the VM would otherwise not observe deadline/fuel
      // exhaustion until we return to the interpreter.
      vm.tick()?;

      let Some(item) = iterator::iterator_step_value(vm, host_ctx, host_hooks, scope, &mut record)?
      else {
        break;
      };
      count += 1;
      if count > MAX_ABORT_SIGNAL_ANY_INPUT_SIGNALS {
        return Err(VmError::TypeError(ABORT_SIGNAL_ANY_TOO_MANY_SIGNALS_ERROR));
      }

      let (source_signal, aborted_now, reason_now) = {
        // Root the yielded value while we validate it and read its state.
        let mut iter_scope = scope.reborrow();
        iter_scope.push_root(item)?;
        let source_signal =
          require_abort_signal(&iter_scope, item, "AbortSignal.any input is not an AbortSignal")?;

        let aborted_val = iter_scope
          .heap()
          .object_get_own_data_property_value(source_signal, &aborted_key)?
          .unwrap_or(Value::Undefined);
        let aborted_now = iter_scope.heap().to_boolean(aborted_val)?;

        if aborted_now {
          let reason_now = iter_scope
            .heap()
            .object_get_own_data_property_value(source_signal, &reason_key)?
            .unwrap_or(Value::Undefined);
          (source_signal, true, Some(reason_now))
        } else {
          (source_signal, false, None)
        }
      };

      // Root the source signal for the duration of this call so it can't be GC'd even if it isn't
      // reachable from user code.
      scope.push_root(Value::Object(source_signal))?;
      sources.push(source_signal);

      if aborted_now {
        pre_aborted = true;
        pre_aborted_reason = reason_now;
        break;
      }
    }
    Ok(())
  })();

  if let Err(err) = collect_result {
    if !record.done {
      // If iterator close throws, it overrides the original error (ECMA-262 `IteratorClose`).
      let original_is_throw = err.is_throw_completion();
      let pending_root = err.thrown_value().map(|v| scope.heap_mut().add_root(v)).transpose()?;
      let close_res = iterator::iterator_close(
        vm,
        host_ctx,
        host_hooks,
        scope,
        &record,
        iterator::CloseCompletionKind::Throw,
      );
      if let Some(root) = pending_root {
        scope.heap_mut().remove_root(root);
      }
      if let Err(close_err) = close_res {
        if original_is_throw {
          return Err(close_err);
        }
      }
    }
    return Err(err);
  }

  // If an input signal is already aborted, synchronously create an already-aborted composite signal.
  if pre_aborted {
    if !record.done {
      iterator::iterator_close(
        vm,
        host_ctx,
        host_hooks,
        scope,
        &record,
        iterator::CloseCompletionKind::NonThrow,
      )?;
    }
    let reason = pre_aborted_reason.unwrap_or(Value::Undefined);
    abort_signal(
      vm, scope, host_ctx, host_hooks, signal, reason, /* dispatch_event */ false,
    )?;
    return Ok(Value::Object(signal));
  }

  // Otherwise, add abort listeners that forward the reason to the composite signal.
  let listener_call_id = vm.register_native_call(abort_any_listener_native)?;
  let func_proto = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("missing intrinsics"))?
    .function_prototype();

  for source_signal in sources {
    let mut listener_scope = scope.reborrow();
    listener_scope.push_root(Value::Object(signal))?;
    listener_scope.push_root(Value::Object(source_signal))?;

    let name = listener_scope.alloc_string("AbortSignal.any listener")?;
    listener_scope.push_root(Value::String(name))?;
    let listener = listener_scope.alloc_native_function_with_slots(
      listener_call_id,
      None,
      name,
      1,
      &[Value::Object(signal), Value::Object(source_signal)],
    )?;
    listener_scope
      .heap_mut()
      .object_set_prototype(listener, Some(func_proto))?;
    listener_scope.push_root(Value::Object(listener))?;

    let add_key = alloc_key(&mut listener_scope, "addEventListener")?;
    let add = vm.get_with_host_and_hooks(
      host_ctx,
      &mut listener_scope,
      host_hooks,
      source_signal,
      add_key,
    )?;
    if listener_scope.heap().is_callable(add).unwrap_or(false) {
      let type_s = listener_scope.alloc_string("abort")?;
      listener_scope.push_root(Value::String(type_s))?;
      let _ = vm.call_with_host_and_hooks(
        host_ctx,
        &mut listener_scope,
        host_hooks,
        add,
        Value::Object(source_signal),
        &[Value::String(type_s), Value::Object(listener)],
      );
    }
  }

  Ok(Value::Object(signal))
}

fn abort_any_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host_ctx: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let composite = slots.get(0).copied().unwrap_or(Value::Undefined);
  let source = slots.get(1).copied().unwrap_or(Value::Undefined);
  let (Value::Object(composite_obj), Value::Object(source_obj)) = (composite, source) else {
    return Err(VmError::InvariantViolation(
      "AbortSignal.any listener missing expected slots",
    ));
  };

  let reason_key = alloc_key(scope, "reason")?;
  let reason = vm.get_with_host_and_hooks(host_ctx, scope, host_hooks, source_obj, reason_key)?;
  scope.push_root(reason)?;
  abort_signal(
    vm,
    scope,
    host_ctx,
    host_hooks,
    composite_obj,
    reason,
    /* dispatch_event */ true,
  )?;
  Ok(Value::Undefined)
}

/// Install `AbortController`/`AbortSignal` onto the global object of a `vm-js` Window realm.
pub fn install_window_abort_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
) -> Result<(), VmError> {
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // Capture the realm's `Event` constructor once so abort-event dispatch can always create a real
  // `Event` instance (even if user code overwrites `globalThis.Event` later).
  let event_ctor = match get_own_data_prop(&mut scope, global, "Event")? {
    Value::Object(obj) => obj,
    _ => return Err(VmError::Unimplemented("Event is not installed on the global object")),
  };

  // Look up the existing `EventTarget.prototype` installed by `WindowRealm`.
  let event_target_proto = {
    let event_target_key = alloc_key(&mut scope, "EventTarget")?;
    let ctor = scope
      .heap()
      .object_get_own_data_property_value(global, &event_target_key)?
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      })
      .ok_or(VmError::Unimplemented(
        "EventTarget is not installed on the global object",
      ))?;

    let proto_key = alloc_key(&mut scope, "prototype")?;
    scope
      .heap()
      .object_get_own_data_property_value(ctor, &proto_key)?
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      })
      .ok_or(VmError::Unimplemented("EventTarget.prototype is missing"))?
  };

  let func_proto = realm.intrinsics().function_prototype();

  // --- AbortSignal (illegal constructor, but has static methods) ------------------------------
  let abort_signal_proto = scope.alloc_object_with_prototype(Some(event_target_proto))?;
  scope.push_root(Value::Object(abort_signal_proto))?;
  set_own_data_prop(
    &mut scope,
    abort_signal_proto,
    SIGNAL_EVENT_CTOR_INTERNAL_KEY,
    Value::Object(event_ctor),
    /* writable */ false,
  )?;

  // @@toStringTag branding for platform object detection (`Object.prototype.toString.call(x)`).
  let abort_signal_tag = scope.alloc_string("AbortSignal")?;
  scope.push_root(Value::String(abort_signal_tag))?;
  scope.define_property(
    abort_signal_proto,
    PropertyKey::from_symbol(realm.intrinsics().well_known_symbols().to_string_tag),
    data_desc(Value::String(abort_signal_tag), false),
  )?;

  // Prototype method: throwIfAborted()
  let throw_if_aborted_id = vm.register_native_call(abort_signal_throw_if_aborted_native)?;
  let throw_if_aborted_name = scope.alloc_string("throwIfAborted")?;
  scope.push_root(Value::String(throw_if_aborted_name))?;
  let throw_if_aborted =
    scope.alloc_native_function(throw_if_aborted_id, None, throw_if_aborted_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(throw_if_aborted, Some(func_proto))?;
  set_own_data_prop(
    &mut scope,
    abort_signal_proto,
    "throwIfAborted",
    Value::Object(throw_if_aborted),
    /* writable */ true,
  )?;

  let abort_signal_ctor_call_id = vm.register_native_call(abort_signal_ctor_illegal)?;
  let abort_signal_ctor_construct_id =
    vm.register_native_construct(abort_signal_ctor_construct_illegal)?;
  let abort_signal_name = scope.alloc_string("AbortSignal")?;
  scope.push_root(Value::String(abort_signal_name))?;
  let abort_signal_ctor = scope.alloc_native_function(
    abort_signal_ctor_call_id,
    Some(abort_signal_ctor_construct_id),
    abort_signal_name,
    0,
  )?;
  scope
    .heap_mut()
    .object_set_prototype(abort_signal_ctor, Some(func_proto))?;
  scope.push_root(Value::Object(abort_signal_ctor))?;

  // Link constructor <-> prototype.
  let prototype_key = alloc_key(&mut scope, "prototype")?;
  let constructor_key = alloc_key(&mut scope, "constructor")?;
  scope.define_property(
    abort_signal_ctor,
    prototype_key,
    ctor_link_desc(Value::Object(abort_signal_proto)),
  )?;
  scope.define_property(
    abort_signal_proto,
    constructor_key,
    ctor_link_desc(Value::Object(abort_signal_ctor)),
  )?;

  // Static AbortSignal.abort(reason?)
  let abort_static_id = vm.register_native_call(abort_signal_static_abort_native)?;
  let abort_static_name = scope.alloc_string("abort")?;
  scope.push_root(Value::String(abort_static_name))?;
  let abort_static = scope.alloc_native_function_with_slots(
    abort_static_id,
    None,
    abort_static_name,
    0,
    &[Value::Object(abort_signal_proto)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(abort_static, Some(func_proto))?;
  set_own_data_prop(
    &mut scope,
    abort_signal_ctor,
    "abort",
    Value::Object(abort_static),
    /* writable */ true,
  )?;

  // Static AbortSignal.timeout(ms)
  let timeout_static_id = vm.register_native_call(abort_signal_static_timeout_native)?;
  let timeout_static_name = scope.alloc_string("timeout")?;
  scope.push_root(Value::String(timeout_static_name))?;
  let timeout_static = scope.alloc_native_function_with_slots(
    timeout_static_id,
    None,
    timeout_static_name,
    1,
    &[Value::Object(abort_signal_proto), Value::Object(global)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(timeout_static, Some(func_proto))?;
  set_own_data_prop(
    &mut scope,
    abort_signal_ctor,
    "timeout",
    Value::Object(timeout_static),
    /* writable */ true,
  )?;

  // Static AbortSignal.any(signals)
  let any_static_id = vm.register_native_call(abort_signal_static_any_native)?;
  let any_static_name = scope.alloc_string("any")?;
  scope.push_root(Value::String(any_static_name))?;
  let any_static = scope.alloc_native_function_with_slots(
    any_static_id,
    None,
    any_static_name,
    1,
    &[Value::Object(abort_signal_proto), Value::Object(global)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(any_static, Some(func_proto))?;
  set_own_data_prop(
    &mut scope,
    abort_signal_ctor,
    "any",
    Value::Object(any_static),
    /* writable */ true,
  )?;

  // Expose on global.
  let abort_signal_key = alloc_key(&mut scope, "AbortSignal")?;
  scope.define_property(
    global,
    abort_signal_key,
    data_desc(Value::Object(abort_signal_ctor), true),
  )?;

  // --- AbortController (constructible) -------------------------------------------------------
  let abort_controller_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(abort_controller_proto))?;
  scope.heap_mut().object_set_prototype(
    abort_controller_proto,
    Some(realm.intrinsics().object_prototype()),
  )?;

  // @@toStringTag branding for platform object detection (`Object.prototype.toString.call(x)`).
  let abort_controller_tag = scope.alloc_string("AbortController")?;
  scope.push_root(Value::String(abort_controller_tag))?;
  scope.define_property(
    abort_controller_proto,
    PropertyKey::from_symbol(realm.intrinsics().well_known_symbols().to_string_tag),
    data_desc(Value::String(abort_controller_tag), false),
  )?;

  let abort_id = vm.register_native_call(abort_controller_abort_native)?;
  let abort_name = scope.alloc_string("abort")?;
  scope.push_root(Value::String(abort_name))?;
  let abort_fn = scope.alloc_native_function(abort_id, None, abort_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(abort_fn, Some(func_proto))?;
  set_own_data_prop(
    &mut scope,
    abort_controller_proto,
    "abort",
    Value::Object(abort_fn),
    /* writable */ true,
  )?;

  let abort_controller_call_id = vm.register_native_call(abort_controller_ctor_call)?;
  let abort_controller_construct_id =
    vm.register_native_construct(abort_controller_ctor_construct)?;
  let abort_controller_name = scope.alloc_string("AbortController")?;
  scope.push_root(Value::String(abort_controller_name))?;
  let abort_controller_ctor = scope.alloc_native_function(
    abort_controller_call_id,
    Some(abort_controller_construct_id),
    abort_controller_name,
    0,
  )?;
  scope
    .heap_mut()
    .object_set_prototype(abort_controller_ctor, Some(func_proto))?;
  scope.push_root(Value::Object(abort_controller_ctor))?;

  // Store the AbortSignal prototype on the constructor so the construct hook can create signals.
  set_own_data_prop(
    &mut scope,
    abort_controller_ctor,
    "__fastrender_abort_signal_proto",
    Value::Object(abort_signal_proto),
    /* writable */ false,
  )?;

  scope.define_property(
    abort_controller_ctor,
    prototype_key,
    ctor_link_desc(Value::Object(abort_controller_proto)),
  )?;
  scope.define_property(
    abort_controller_proto,
    constructor_key,
    ctor_link_desc(Value::Object(abort_controller_ctor)),
  )?;

  let abort_controller_key = alloc_key(&mut scope, "AbortController")?;
  scope.define_property(
    global,
    abort_controller_key,
    data_desc(Value::Object(abort_controller_ctor), true),
  )?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value, got {value:?}");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  #[test]
  fn object_prototype_to_string_uses_abort_to_string_tags() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let controller =
      realm.exec_script("Object.prototype.toString.call(new AbortController())")?;
    assert_eq!(get_string(realm.heap(), controller), "[object AbortController]");

    let signal =
      realm.exec_script("Object.prototype.toString.call((new AbortController()).signal)")?;
    assert_eq!(get_string(realm.heap(), signal), "[object AbortSignal]");

    realm.teardown();
    Ok(())
  }
}
