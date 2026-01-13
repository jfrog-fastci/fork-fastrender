//! Shared helpers for reporting JavaScript exceptions via `window` error events.
//!
//! This is used by both the `BrowserTab` vm-js executor and the vm-js event loop hooks
//! (`window_timers`), keeping `ErrorEvent` construction/dispatch consistent across surfaces.

use vm_js::{
  PropertyDescriptor, PropertyKey, PropertyKind, Scope, StackFrame, Value, Vm, VmError, VmHost,
  VmHostHooks,
};

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
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

/// Resolve the `(filename, lineno, colno)` fields for a `window` `error` event.
///
/// `vm-js` uses synthetic `<inline>` names for unnamed scripts; prefer a real document/script URL
/// when available so `window.onerror` gets a useful filename.
pub(crate) fn resolve_error_event_location(
  filename_hint: Option<&str>,
  first_frame: Option<&StackFrame>,
) -> (String, u32, u32) {
  if let Some(frame) = first_frame {
    let from_stack = frame.source.as_ref();
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

/// Dispatch an `error` event on `window`.
///
/// Returns whether the event was *canceled* (`true` if canceled), matching `EventTarget.dispatchEvent`
/// semantics (`dispatchEvent` returns `false` when canceled).
///
/// Notes:
/// - Callers must ensure any `error_value` handle remains alive for the duration of this call.
///   (For thrown exceptions, this typically means rooting the value before allocating strings/objects
///   that may trigger GC.)
pub(crate) fn dispatch_window_error_event(
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

  // `ErrorEventInit.error` (nullable, default null).
  let error_key = alloc_key(&mut scope, "error")?;
  let error_value = error_value.unwrap_or(Value::Null);
  // Root while defining the property: the thrown error is not necessarily a persistent GC root.
  scope.push_root(error_value)?;
  scope.define_property(init_obj, error_key, data_desc(error_value))?;

  // Prefer a real `ErrorEvent` object when supported so `instanceof ErrorEvent` and prototype
  // semantics match the platform.
  //
  // When missing, fall back to a plain `Event` with read-only `ErrorEvent`-shaped payload fields.
  let error_event_ctor_key = alloc_key(&mut scope, "ErrorEvent")?;
  let error_event_ctor =
    vm.get_with_host_and_hooks(vm_host, &mut scope, hooks, global_obj, error_event_ctor_key)?;
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
    let event_ctor =
      vm.get_with_host_and_hooks(vm_host, &mut scope, hooks, global_obj, event_ctor_key)?;
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

  // `dispatchEvent` returns `false` when the event was canceled.
  let not_canceled = matches!(dispatch_result, Value::Bool(true));
  Ok(!not_canceled)
}

