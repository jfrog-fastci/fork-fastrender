//! WebIDL conversion helpers for `vm-js` realm bindings.
//!
//! The WebIDL bindings generator should avoid emitting spec-shaped conversion algorithms inline.
//! Instead it should call into this module so conversion behaviour stays consistent across all
//! generated bindings and does not drift over time.
//!
//! This module is intentionally `vm-js` specific: it deals in `vm_js::Value` handles and must root
//! values across allocations to satisfy `vm-js` GC requirements.

use vm_js::{GcObject, PropertyKey, Value, VmError, VmHost, VmHostHooks};

use crate::bindings_runtime::{BindingsRuntime, DataPropertyAttributes};

/// Returns `true` if `obj` should be treated as iterable for union discrimination purposes.
///
/// This mirrors the generator's previous inline logic:
/// - Arrays are always treated as iterable (fast-path).
/// - Otherwise we check `@@iterator` on the object:
///   - `undefined`/`null` => not iterable
///   - non-callable => throw a TypeError
pub fn object_has_iterator<'a>(
  rt: &mut BindingsRuntime<'a>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
) -> Result<bool, VmError> {
  // Root `obj` for the duration of any property lookups (which may invoke user code and allocate).
  rt.scope.push_root(Value::Object(obj))?;

  if rt.scope.heap().object_is_array(obj)? {
    return Ok(true);
  }

  let intr = rt
    .vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let sym = intr.well_known_symbols().iterator;
  rt.scope.push_root(Value::Symbol(sym))?;
  let key = PropertyKey::from_symbol(sym);
  let method = rt.scope.ordinary_get_with_host_and_hooks(
    &mut *rt.vm,
    host,
    hooks,
    obj,
    key,
    Value::Object(obj),
  )?;
  if matches!(method, Value::Undefined | Value::Null) {
    return Ok(false);
  }
  if !rt.scope.heap().is_callable(method)? {
    return Err(rt.throw_type_error("GetMethod: target is not callable"));
  }
  Ok(true)
}

/// Convert an ECMAScript value to an IDL `sequence<T>`/`FrozenArray<T>` value.
///
/// The bindings layer representation used by FastRender's `vm-js` WebIDL backend is a JavaScript
/// `Array` object containing the converted elements.
pub fn to_iterable_list<'a, F>(
  rt: &mut BindingsRuntime<'a>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  expected_object_message: &'static str,
  mut convert_elem: F,
) -> Result<Value, VmError>
where
  F: FnMut(&mut BindingsRuntime<'a>, &mut dyn VmHost, &mut dyn VmHostHooks, Value) -> Result<Value, VmError>,
{
  let v = value;
  let Value::Object(_obj) = v else {
    return Err(rt.throw_type_error(expected_object_message));
  };
  rt.scope.push_root(v)?;

  let mut iterator_record = vm_js::iterator::get_iterator(&mut *rt.vm, host, hooks, &mut rt.scope, v)?;
  rt.scope.push_root(iterator_record.iterator)?;
  rt.scope.push_root(iterator_record.next_method)?;

  let out = rt.alloc_array(0)?;

  let mut idx: usize = 0;
  while let Some(next) =
    vm_js::iterator::iterator_step_value(&mut *rt.vm, host, hooks, &mut rt.scope, &mut iterator_record)?
  {
    rt.scope.push_root(next)?;

    if idx >= rt.limits().max_sequence_length {
      return Err(rt.throw_range_error("sequence exceeds maximum length"));
    }

    let converted = convert_elem(rt, host, hooks, next)?;
    let converted = rt.scope.push_root(converted)?;

    // Root the key string across `convert_elem` (which may allocate/GC).
    let key_s = rt.scope.alloc_string(&idx.to_string())?;
    rt.scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    rt.scope.create_data_property_or_throw(out, key, converted)?;
    idx += 1;
  }

  Ok(Value::Object(out))
}

/// Convert an ECMAScript value to an IDL `record<K, V>` value.
///
/// This follows the WebIDL "js-to-record" algorithm shape:
/// - `ToObject` is applied (primitives are accepted; `null`/`undefined` throw).
/// - Only own enumerable **string** keys are included (symbols are ignored).
///
/// The bindings layer representation used by FastRender's `vm-js` WebIDL backend is a JavaScript
/// plain object containing the converted values.
pub fn to_record<'a, F>(
  rt: &mut BindingsRuntime<'a>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  expected_object_message: &'static str,
  mut convert_value: F,
) -> Result<Value, VmError>
where
  F: FnMut(&mut BindingsRuntime<'a>, &mut dyn VmHost, &mut dyn VmHostHooks, Value) -> Result<Value, VmError>,
{
  if matches!(value, Value::Undefined | Value::Null) {
    return Err(rt.throw_type_error(expected_object_message));
  }

  // Root `value` across `ToObject` (boxing may allocate/GC and `value` may contain GC handles).
  rt.scope.push_root(value)?;

  let input = match value {
    Value::Object(obj) => obj,
    other => rt.scope.to_object(&mut *rt.vm, host, hooks, other)?,
  };

  rt.scope.push_root(Value::Object(input))?;

  let out_obj = rt.alloc_object()?;

  let keys = rt.scope.ordinary_own_property_keys(input)?;
  let mut key_roots: Vec<Value> = Vec::with_capacity(keys.len());
  for key in &keys {
    match *key {
      PropertyKey::String(s) => key_roots.push(Value::String(s)),
      PropertyKey::Symbol(s) => key_roots.push(Value::Symbol(s)),
    }
  }
  if !key_roots.is_empty() {
    rt.scope.push_roots(&key_roots)?;
  }

  let mut entries: usize = 0;
  for key in keys {
    // Record keys are strings; symbol keys are ignored.
    let PropertyKey::String(_s) = key else {
      continue;
    };

    let Some(desc) = rt.scope.heap().object_get_own_property(input, &key)? else {
      continue;
    };
    if !desc.enumerable {
      continue;
    }

    if entries >= rt.limits().max_record_entries {
      return Err(rt.throw_range_error("record exceeds maximum entry count"));
    }

    let prop_value = rt.scope.ordinary_get_with_host_and_hooks(
      &mut *rt.vm,
      host,
      hooks,
      input,
      key,
      Value::Object(input),
    )?;
    rt.scope.push_root(prop_value)?;

    let converted = convert_value(rt, host, hooks, prop_value)?;
    rt.define_data_property(out_obj, key, converted, DataPropertyAttributes::new(true, true, true))?;

    entries += 1;
  }

  Ok(Value::Object(out_obj))
}

/// Convert an ECMAScript value to a WebIDL enumeration value.
///
/// Spec: <https://webidl.spec.whatwg.org/#js-to-enumeration>
pub fn to_enum<'a>(
  rt: &mut BindingsRuntime<'a>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  enum_name: &'static str,
  allowed_values: &[&'static str],
) -> Result<Value, VmError> {
  // Root `value` across `ToString`.
  rt.scope.push_root(value)?;

  let s = rt.scope.to_string(&mut *rt.vm, host, hooks, value)?;
  rt.scope.push_root(Value::String(s))?;
  let text = rt.scope.heap().get_string(s)?.to_utf8_lossy();
  if !allowed_values.iter().any(|v| *v == text.as_str()) {
    return Err(rt.throw_type_error(&format!(
      "Value is not a valid member of the `{enum_name}` enum"
    )));
  }
  Ok(Value::String(s))
}

/// Convert an ECMAScript value to a WebIDL callback function value.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-callback-function>
pub fn to_callback_function<'a>(
  rt: &mut BindingsRuntime<'a>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<Value, VmError> {
  let _ = (host, hooks);
  if !rt.scope.heap().is_callable(value)? {
    return Err(rt.throw_type_error("Value is not a callable callback function"));
  }
  Ok(value)
}

/// Convert an ECMAScript value to a WebIDL callback interface value.
///
/// A callback interface value is accepted if it is:
/// - callable, or
/// - an object with a callable `handleEvent` method.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-callback-interface>
pub fn to_callback_interface<'a>(
  rt: &mut BindingsRuntime<'a>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<Value, VmError> {
  let v = value;
  if rt.scope.heap().is_callable(v)? {
    return Ok(v);
  }

  let Value::Object(obj) = v else {
    return Err(rt.throw_type_error("Value is not a callable callback interface"));
  };

  // Root `v` across any allocations and user-code invoked by accessors.
  rt.scope.push_root(v)?;
  let key = rt.property_key("handleEvent")?;
  let method = rt.scope.ordinary_get_with_host_and_hooks(
    &mut *rt.vm,
    host,
    hooks,
    obj,
    key,
    v,
  )?;
  if matches!(method, Value::Undefined | Value::Null) {
    return Err(rt.throw_type_error(
      "Callback interface object is missing a callable handleEvent method",
    ));
  }
  if !rt.scope.heap().is_callable(method)? {
    return Err(rt.throw_type_error("GetMethod: target is not callable"));
  }

  Ok(v)
}
