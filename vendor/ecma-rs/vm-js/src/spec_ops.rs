//! Spec operations (ECMA-262 abstract operations).
//!
//! This module contains small helpers that mirror ECMA-262 abstract operations closely. These are
//! intended to be used by built-ins so their algorithms remain spec-shaped.

use crate::{GcObject, PropertyDescriptorPatch, PropertyKey, Scope, Value, Vm, VmError, VmHost, VmHostHooks};
use std::mem;

/// `GetPrototypeFromConstructor(constructor, intrinsicDefaultProto)` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-getprototypefromconstructor>
pub fn get_prototype_from_constructor_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  constructor: Value,
  intrinsic_default_proto: GcObject,
) -> Result<GcObject, VmError> {
  let Value::Object(constructor_obj) = constructor else {
    // The spec asserts `IsConstructor(constructor)`; treat non-objects as "use default".
    return Ok(intrinsic_default_proto);
  };

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(constructor_obj))?;
  scope.push_root(Value::Object(intrinsic_default_proto))?;

  let key_s = scope.alloc_string("prototype")?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);

  let proto = scope.ordinary_get_with_host_and_hooks(
    vm,
    host,
    hooks,
    constructor_obj,
    key,
    Value::Object(constructor_obj),
  )?;
  match proto {
    Value::Object(o) => Ok(o),
    _ => Ok(intrinsic_default_proto),
  }
}

/// Convenience wrapper around [`get_prototype_from_constructor_with_host_and_hooks`] that passes a
/// dummy host context (`()`) and uses the VM-owned microtask queue as hooks.
///
/// ## ⚠️ Dummy `VmHost` context
///
/// `GetPrototypeFromConstructor` performs `Get(constructor, "prototype")`, which can invoke user JS
/// via accessors. Host embeddings that need native handlers to observe real host state should
/// prefer [`get_prototype_from_constructor_with_host_and_hooks`].
pub fn get_prototype_from_constructor(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  constructor: Value,
  intrinsic_default_proto: GcObject,
) -> Result<GcObject, VmError> {
  // Backwards-compatible wrapper that uses a dummy host context and the VM-owned microtask queue.
  let mut dummy_host = ();
  let mut hooks = mem::take(vm.microtask_queue_mut());
  let result = get_prototype_from_constructor_with_host_and_hooks(
    vm,
    scope,
    &mut dummy_host,
    &mut hooks,
    constructor,
    intrinsic_default_proto,
  );
  *vm.microtask_queue_mut() = hooks;
  result
}

/// `OrdinaryCreateFromConstructor(constructor, intrinsicDefaultProto, internalSlotsList)`
/// (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-ordinarycreatefromconstructor>
///
/// ## ⚠️ Dummy `VmHost` context
///
/// This wrapper passes a **dummy host context** (`()`) and uses the VM-owned microtask queue as
/// hooks.
///
/// `OrdinaryCreateFromConstructor` performs `GetPrototypeFromConstructor`, which can invoke user JS
/// via accessors. Host embeddings that need native handlers to observe real host state should
/// prefer [`ordinary_create_from_constructor_with_host_and_hooks`].
pub fn ordinary_create_from_constructor<F>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  new_target: Value,
  intrinsic_default_proto: GcObject,
  _internal_slots_list: &[&'static str],
  allocate: F,
) -> Result<GcObject, VmError>
where
  F: FnOnce(&mut Scope<'_>) -> Result<GcObject, VmError>,
{
  // Backwards-compatible wrapper that uses a dummy host context and the VM-owned microtask queue
  // as hooks.
  let mut dummy_host = ();
  let mut hooks = mem::take(vm.microtask_queue_mut());
  let result = ordinary_create_from_constructor_with_host_and_hooks(
    vm,
    scope,
    &mut dummy_host,
    &mut hooks,
    new_target,
    intrinsic_default_proto,
    _internal_slots_list,
    allocate,
  );
  *vm.microtask_queue_mut() = hooks;
  result
}

/// `OrdinaryCreateFromConstructor(constructor, intrinsicDefaultProto, internalSlotsList)`
/// (ECMA-262), using an explicit embedder host context and host hook implementation.
///
/// Spec: <https://tc39.es/ecma262/#sec-ordinarycreatefromconstructor>
pub fn ordinary_create_from_constructor_with_host_and_hooks<F>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  new_target: Value,
  intrinsic_default_proto: GcObject,
  _internal_slots_list: &[&'static str],
  allocate: F,
) -> Result<GcObject, VmError>
where
  F: FnOnce(&mut Scope<'_>) -> Result<GcObject, VmError>,
{
  let proto = get_prototype_from_constructor_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    new_target,
    intrinsic_default_proto,
  )?;

  // Root `new_target`/`proto` across allocation in case it triggers GC.
  let mut scope = scope.reborrow();
  scope.push_root(new_target)?;
  scope.push_root(Value::Object(proto))?;

  let obj = allocate(&mut scope)?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  Ok(obj)
}

/// `CreateDataProperty(O, P, V)` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-createdataproperty>
#[inline]
pub fn create_data_property(
  scope: &mut Scope<'_>,
  obj: GcObject,
  key: PropertyKey,
  value: Value,
) -> Result<bool, VmError> {
  scope.create_data_property(obj, key, value)
}

/// `CreateDataPropertyOrThrow(O, P, V)` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-createdatapropertyorthrow>
#[inline]
pub fn create_data_property_or_throw(
  scope: &mut Scope<'_>,
  obj: GcObject,
  key: PropertyKey,
  value: Value,
) -> Result<(), VmError> {
  scope.create_data_property_or_throw(obj, key, value)
}

/// `DefinePropertyOrThrow(O, P, desc)` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-definepropertyorthrow>
#[inline]
pub fn define_property_or_throw(
  scope: &mut Scope<'_>,
  obj: GcObject,
  key: PropertyKey,
  desc: PropertyDescriptorPatch,
) -> Result<(), VmError> {
  scope.define_property_or_throw(obj, key, desc)
}

/// `DeletePropertyOrThrow(O, P)` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-deletepropertyorthrow>
#[inline]
pub fn delete_property_or_throw(
  scope: &mut Scope<'_>,
  obj: GcObject,
  key: PropertyKey,
) -> Result<(), VmError> {
  scope.delete_property_or_throw(obj, key)
}

/// `GetMethod(V, P)` (ECMA-262) (partial).
///
/// Spec: <https://tc39.es/ecma262/#sec-getmethod>
pub fn get_method_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  key: PropertyKey,
) -> Result<Option<Value>, VmError> {
  // `GetMethod` uses `GetV`, which in turn uses `ToObject`. Full `ToObject` boxing semantics are
  // implemented via the intrinsic `Object` constructor and can allocate. `Get` can invoke user JS
  // via accessors, so this operation must be host-aware.
  let mut scope = scope.reborrow();

  let key_root = match key {
    PropertyKey::String(s) => Value::String(s),
    PropertyKey::Symbol(s) => Value::Symbol(s),
  };
  scope.push_roots(&[value, key_root])?;

  // `GetV(V, P)`: ToObject(V) then `Get(O, P)` with `receiver = V`.
  let receiver = value;
  let obj = match value {
    Value::Object(obj) => obj,
    Value::Undefined | Value::Null => {
      return Err(VmError::TypeError(
        "GetMethod: cannot convert null/undefined to object",
      ))
    }
    other => {
      let wrapped_obj = scope.to_object(vm, host, hooks, other)?;
      scope.push_root(Value::Object(wrapped_obj))?;
      wrapped_obj
    }
  };

  // GetMethod: callability checks and `null`/`undefined` normalization.
  let func = scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, receiver)?;
  if matches!(func, Value::Undefined | Value::Null) {
    return Ok(None);
  }
  if !scope.heap().is_callable(func)? {
    return Err(VmError::TypeError("GetMethod: target is not callable"));
  }
  Ok(Some(func))
}

/// `GetMethod(V, P)` (ECMA-262) (partial).
///
/// ## ⚠️ Dummy `VmHost` context
///
/// This wrapper can invoke user JS via accessors but will pass a **dummy host context** (`()`) to
/// any native call/construct handlers reached through those invocations.
///
/// Embeddings that need native handlers to observe real host state should prefer
/// [`get_method_with_host_and_hooks`].
#[inline]
pub fn get_method(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  value: Value,
  key: PropertyKey,
) -> Result<Option<Value>, VmError> {
  vm.get_method(scope, value, key)
}
