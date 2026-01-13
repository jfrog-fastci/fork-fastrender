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

/// Perform a spec-shaped `GetMethod(obj, @@iterator)` lookup.
///
/// Returns:
/// - `Ok(Some(method))` if `obj[@@iterator]` is present and callable.
/// - `Ok(None)` if `obj[@@iterator]` is `undefined` or `null`.
/// - `Err(TypeError)` if `obj[@@iterator]` is present but not callable.
pub fn get_iterator_method<'a>(
  rt: &mut BindingsRuntime<'a>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
) -> Result<Option<Value>, VmError> {
  // Root `obj` for the duration of any property lookups (which may invoke user code and allocate).
  rt.scope.push_root(Value::Object(obj))?;

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
    return Ok(None);
  }
  if !rt.scope.heap().is_callable(method)? {
    return Err(rt.throw_type_error("GetMethod: target is not callable"));
  }
  Ok(Some(method))
}

/// Returns `true` if `obj` should be treated as iterable for union discrimination purposes.
///
/// This matches WebIDL union discrimination behaviour: `GetMethod(obj, @@iterator)` is used with no
/// special casing for Arrays.
pub fn object_has_iterator<'a>(
  rt: &mut BindingsRuntime<'a>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
) -> Result<bool, VmError> {
  Ok(get_iterator_method(rt, host, hooks, obj)?.is_some())
}

/// Returns `true` if `obj` is a boxed `String` object (i.e. has `[[StringData]]`).
///
/// WebIDL union conversions treat boxed `String` objects as strings when the union includes a
/// string type. This must be detected via the internal `[[StringData]]` slot marker (not by
/// prototype checks) so it remains true even if user code mutates `obj.[[Prototype]]`.
#[inline]
pub fn is_string_object<'a>(
  rt: &mut BindingsRuntime<'a>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
) -> Result<bool, VmError> {
  let _ = (&mut *host, &mut *hooks);
  // Root `obj` in case the caller performs allocations after this check.
  rt.scope.push_root(Value::Object(obj))?;
  rt.scope.heap().object_is_string_object(obj)
}

/// Convert an ECMAScript value to an IDL `sequence<T>`/`FrozenArray<T>` value.
///
/// The bindings layer representation used by the `vm-js` WebIDL backend is a JavaScript `Array`
/// object containing the converted elements.
pub fn to_iterable_list<'a, F>(
  rt: &mut BindingsRuntime<'a>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  expected_object_message: &'static str,
  mut convert_elem: F,
) -> Result<Value, VmError>
where
  F: FnMut(
    &mut BindingsRuntime<'a>,
    &mut dyn VmHost,
    &mut dyn VmHostHooks,
    Value,
  ) -> Result<Value, VmError>,
{
  // WebIDL `sequence<T>` conversion is intentionally *stricter* than ECMAScript iteration:
  //
  // - If `V` is not an Object, throw a TypeError.
  // - Then do `GetMethod(V, @@iterator)`; if it's undefined, throw a TypeError.
  //
  // Critically, this does **not** apply `ToObject(V)` (so primitives like strings must be
  // rejected, rather than being auto-boxed and iterated).
  let v = value;
  let Value::Object(_obj) = v else {
    return Err(rt.throw_type_error(expected_object_message));
  };
  rt.scope.push_root(v)?;

  let mut iterator_record = match vm_js::iterator::get_iterator(
    &mut *rt.vm,
    host,
    hooks,
    &mut rt.scope,
    v,
  ) {
    Ok(record) => record,
    Err(VmError::TypeError("GetIterator: value is not iterable")) => {
      return Err(rt.throw_type_error("Value is not iterable"));
    }
    Err(err) => return Err(err),
  };
  rt.scope.push_root(iterator_record.iterator)?;
  rt.scope.push_root(iterator_record.next_method)?;

  let out = rt.alloc_array(0)?;

  let mut idx: usize = 0;
  while let Some(next) = vm_js::iterator::iterator_step_value(
    &mut *rt.vm,
    host,
    hooks,
    &mut rt.scope,
    &mut iterator_record,
  )? {
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
    rt.scope
      .create_data_property_or_throw(out, key, converted)?;
    idx += 1;
  }

  Ok(Value::Object(out))
}

/// Convert an ECMAScript value to an IDL `sequence<T>`/`FrozenArray<T>` value, using a previously
/// resolved `@@iterator` method.
///
/// This follows the WebIDL "convert JS value to sequence" algorithm shape:
/// 1) `GetMethod(V, @@iterator)` once (performed by the caller),
/// 2) then `GetIteratorFromMethod(V, method)` to begin iteration (no second `GetMethod`).
pub fn to_iterable_list_from_method<'a, F>(
  rt: &mut BindingsRuntime<'a>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  method: Value,
  expected_object_message: &'static str,
  mut convert_elem: F,
) -> Result<Value, VmError>
where
  F: FnMut(
    &mut BindingsRuntime<'a>,
    &mut dyn VmHost,
    &mut dyn VmHostHooks,
    Value,
  ) -> Result<Value, VmError>,
{
  let v = value;
  let Value::Object(_obj) = v else {
    return Err(rt.throw_type_error(expected_object_message));
  };
  // Root the iterable + method across any allocations performed by iterator consumption.
  rt.scope.push_root(v)?;
  rt.scope.push_root(method)?;

  let mut iterator_record = vm_js::iterator::get_iterator_from_method(
    &mut *rt.vm,
    host,
    hooks,
    &mut rt.scope,
    v,
    method,
  )?;
  rt.scope.push_root(iterator_record.iterator)?;
  rt.scope.push_root(iterator_record.next_method)?;

  let out = rt.alloc_array(0)?;

  let mut idx: usize = 0;
  while let Some(next) = vm_js::iterator::iterator_step_value(
    &mut *rt.vm,
    host,
    hooks,
    &mut rt.scope,
    &mut iterator_record,
  )? {
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
    rt.scope
      .create_data_property_or_throw(out, key, converted)?;
    idx += 1;
  }

  Ok(Value::Object(out))
}

/// Convert an ECMAScript value to an IDL `record<K, V>` value.
///
/// This follows the WebIDL "js-to-record" algorithm shape:
/// - If the input value is not an Object, throw a TypeError (no `ToObject` boxing).
/// - Only own enumerable keys are considered.
/// - Record conversion applies WebIDL `PropertyKeyToString` / ECMAScript `ToString(key)`, so
///   enumerable Symbol keys throw a TypeError (and only string keys contribute entries).
///
/// The bindings layer representation used by the `vm-js` WebIDL backend is a JavaScript plain
/// object containing the converted values.
pub fn to_record<'a, F>(
  rt: &mut BindingsRuntime<'a>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  expected_object_message: &'static str,
  mut convert_value: F,
) -> Result<Value, VmError>
where
  F: FnMut(
    &mut BindingsRuntime<'a>,
    &mut dyn VmHost,
    &mut dyn VmHostHooks,
    Value,
  ) -> Result<Value, VmError>,
{
  let Value::Object(input) = value else {
    return Err(rt.throw_type_error(expected_object_message));
  };
  rt.scope.push_root(Value::Object(input))?;

  let out_obj = rt.alloc_object()?;

  // WebIDL record conversion uses `O.[[OwnPropertyKeys]]()`, which must dispatch through Proxy
  // `ownKeys` traps when present.
  let keys = rt
    .scope
    .own_property_keys_with_host_and_hooks(&mut *rt.vm, host, hooks, input)?;
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
    // WebIDL record conversion uses `O.[[GetOwnProperty]](key)`, which must dispatch through Proxy
    // `getOwnPropertyDescriptor` traps when present.
    let Some(desc) =
      rt.scope
        .object_get_own_property_with_host_and_hooks(&mut *rt.vm, host, hooks, input, key)?
    else {
      continue;
    };
    if !desc.enumerable {
      continue;
    }

    // WebIDL record conversion uses `PropertyKeyToString` / `ToString(key)`. Enumerable symbol keys
    // therefore throw a TypeError (since `ToString(Symbol)` throws).
    match key {
      PropertyKey::Symbol(sym) => {
        match rt
          .scope
          .to_string(&mut *rt.vm, host, hooks, Value::Symbol(sym))
        {
          Ok(_) => {}
          Err(VmError::TypeError(message)) => return Err(rt.throw_type_error(message)),
          Err(err) => return Err(err),
        };
        // `ToString(Symbol)` always throws, so we should never reach here.
        continue;
      }
      PropertyKey::String(s) => {
        // WebIDL's key conversion enforces `max_string_code_units` after verifying that the
        // property is enumerable.
        let len = rt.scope.heap().get_string(s)?.as_code_units().len();
        if len > rt.limits().max_string_code_units {
          return Err(rt.throw_range_error("string exceeds maximum length"));
        }
      }
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
    rt.define_data_property(
      out_obj,
      key,
      converted,
      DataPropertyAttributes::new(true, true, true),
    )?;

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

  // WebIDL string conversions must enforce `max_string_code_units` (in UTF-16 code units).
  let len = rt.scope.heap().get_string(s)?.as_code_units().len();
  if len > rt.limits().max_string_code_units {
    return Err(rt.throw_range_error("string exceeds maximum length"));
  }

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
/// - an object with a callable `handleEvent` method, or
/// - an object with a callable `acceptNode` method.
/// - an object with a callable `lookupNamespaceURI` method.
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

  // NOTE: WebIDL callback interfaces are structural: non-callable objects are accepted only when
  // they expose a callable method for the callback's operation.
  //
  // `EventListener` uses `handleEvent`, `NodeFilter` uses `acceptNode`, and `XPathNSResolver` uses
  // `lookupNamespaceURI`. We support all of these here to avoid requiring generated bindings to
  // plumb the callback interface operation name through the conversion helper.
  //
  // Keep the existing `handleEvent` behavior intact, and only fall back to the other known
  // operation names when `handleEvent` is missing (`undefined`/`null`).
  let handle_event_key = rt.property_key("handleEvent")?;
  let method = rt
    .scope
    .ordinary_get_with_host_and_hooks(&mut *rt.vm, host, hooks, obj, handle_event_key, v)?;
  if !matches!(method, Value::Undefined | Value::Null) {
    if !rt.scope.heap().is_callable(method)? {
      return Err(rt.throw_type_error("GetMethod: target is not callable"));
    }
    return Ok(v);
  }

  let accept_node_key = rt.property_key("acceptNode")?;
  let method = rt
    .scope
    .ordinary_get_with_host_and_hooks(&mut *rt.vm, host, hooks, obj, accept_node_key, v)?;
  if !matches!(method, Value::Undefined | Value::Null) {
    if !rt.scope.heap().is_callable(method)? {
      return Err(rt.throw_type_error("GetMethod: target is not callable"));
    }
    return Ok(v);
  }

  let lookup_namespace_uri_key = rt.property_key("lookupNamespaceURI")?;
  let method = rt.scope.ordinary_get_with_host_and_hooks(
    &mut *rt.vm,
    host,
    hooks,
    obj,
    lookup_namespace_uri_key,
    v,
  )?;
  if matches!(method, Value::Undefined | Value::Null) {
    return Err(
      rt.throw_type_error("Callback interface object is missing a callable handleEvent method"),
    );
  }
  if !rt.scope.heap().is_callable(method)? {
    return Err(rt.throw_type_error("GetMethod: target is not callable"));
  }

  Ok(v)
}
