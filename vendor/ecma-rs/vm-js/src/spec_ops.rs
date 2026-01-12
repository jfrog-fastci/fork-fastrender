//! Spec operations (ECMA-262 abstract operations).
//!
//! This module contains small helpers that mirror ECMA-262 abstract operations closely. These are
//! intended to be used by built-ins so their algorithms remain spec-shaped.

use crate::heap::MAX_PROTOTYPE_CHAIN;
use crate::{GcObject, PropertyDescriptorPatch, PropertyKey, Scope, Value, Vm, VmError, VmHost, VmHostHooks};
use std::mem;

// https://tc39.es/ecma262/#sec-tolength
fn to_length(n: f64) -> usize {
  // `ToLength` clamps to the safe integer range.
  const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0; // 2^53 - 1

  if n.is_nan() || n <= 0.0 {
    return 0;
  }
  if !n.is_finite() {
    // +Infinity
    return MAX_SAFE_INTEGER as usize;
  }

  let int = n.trunc();
  let clamped = int.min(MAX_SAFE_INTEGER);
  if clamped >= usize::MAX as f64 {
    usize::MAX
  } else {
    clamped as usize
  }
}

/// ECMAScript `[[Get]](P, Receiver)` internal method dispatch for ordinary and Proxy exotic objects.
///
/// This is a minimal implementation today:
/// - Ordinary objects delegate to `Scope::ordinary_get_with_host_and_hooks`
/// - Proxy objects implement `Proxy.[[Get]]` with support for the `"get"` trap (when present)
/// - Revoked proxies throw a TypeError
pub fn internal_get_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  key: PropertyKey,
  receiver: Value,
) -> Result<Value, VmError> {
  // Root `obj`/`receiver`/`key` across any allocations (trap lookup, trap calls).
  let mut scope = scope.reborrow();
  let key_root = match key {
    PropertyKey::String(s) => Value::String(s),
    PropertyKey::Symbol(s) => Value::Symbol(s),
  };
  scope.push_roots(&[Value::Object(obj), key_root, receiver])?;

  // Fast path: ordinary object.
  if !scope.heap().is_proxy_object(obj) {
    return scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, receiver);
  }

  // Proxy.[[Get]]
  let proxy = scope
    .heap()
    .get_proxy_data(obj)?
    .ok_or(VmError::invalid_handle())?;
  let (Some(target), Some(handler)) = (proxy.target, proxy.handler) else {
    return Err(VmError::TypeError(
      "Cannot perform 'get' on a proxy that has been revoked",
    ));
  };

  // trap = ? GetMethod(handler, "get")
  let get_key_s = scope.alloc_string("get")?;
  scope.push_root(Value::String(get_key_s))?;
  let get_key = PropertyKey::from_string(get_key_s);
  let trap = get_method_with_host_and_hooks(vm, &mut scope, host, hooks, Value::Object(handler), get_key)?;
  let Some(trap) = trap else {
    // No trap: forward to target with the original receiver.
    return internal_get_with_host_and_hooks(vm, &mut scope, host, hooks, target, key, receiver);
  };
  // Root the trap: it may be the result of an accessor getter and not otherwise reachable.
  scope.push_root(trap)?;

  // trapResult = ? Call(trap, handler, « target, P, Receiver »)
  let trap_args = [Value::Object(target), key_root, receiver];
  vm.call_with_host_and_hooks(host, &mut scope, hooks, trap, Value::Object(handler), &trap_args)
}

/// ECMAScript `[[HasProperty]](P)` internal method dispatch for ordinary and Proxy exotic objects.
///
/// This is host-aware because Proxy `"has"` traps can invoke user code.
pub fn internal_has_property_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  key: PropertyKey,
) -> Result<bool, VmError> {
  // Root `obj`/`key` for the duration of the operation.
  let mut scope = scope.reborrow();
  let key_root = match key {
    PropertyKey::String(s) => Value::String(s),
    PropertyKey::Symbol(s) => Value::Symbol(s),
  };
  scope.push_roots(&[Value::Object(obj), key_root])?;

  if !scope.heap().is_proxy_object(obj) {
    return scope.ordinary_has_property_with_tick(obj, key, || vm.tick());
  }

  // Proxy.[[HasProperty]]
  let proxy = scope
    .heap()
    .get_proxy_data(obj)?
    .ok_or(VmError::invalid_handle())?;
  let (Some(target), Some(handler)) = (proxy.target, proxy.handler) else {
    return Err(VmError::TypeError(
      "Cannot perform 'has' on a proxy that has been revoked",
    ));
  };

  // trap = ? GetMethod(handler, "has")
  let has_key_s = scope.alloc_string("has")?;
  scope.push_root(Value::String(has_key_s))?;
  let has_key = PropertyKey::from_string(has_key_s);
  let trap = get_method_with_host_and_hooks(vm, &mut scope, host, hooks, Value::Object(handler), has_key)?;
  let Some(trap) = trap else {
    // No trap: forward to target.
    return internal_has_property_with_host_and_hooks(vm, &mut scope, host, hooks, target, key);
  };
  // Root the trap: it may be the result of an accessor getter and not otherwise reachable.
  scope.push_root(trap)?;

  // trapResult = ToBoolean(? Call(trap, handler, « target, P »))
  let trap_args = [Value::Object(target), key_root];
  let trap_result =
    vm.call_with_host_and_hooks(host, &mut scope, hooks, trap, Value::Object(handler), &trap_args)?;
  scope.heap().to_boolean(trap_result)
}

/// `IsArray(argument)` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-isarray>
pub fn is_array_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<bool, VmError> {
  // `IsArray` is observable for Proxy objects: it must recurse through Proxy targets and throw if
  // the Proxy is revoked.
  let Value::Object(mut obj) = value else {
    return Ok(false);
  };

  let mut steps = 0usize;
  loop {
    if scope.heap().is_proxy_object(obj) {
      if steps != 0 && steps % 1024 == 0 {
        vm.tick()?;
      }
      steps = steps.saturating_add(1);

      let (target, handler) = (scope.heap().proxy_target(obj)?, scope.heap().proxy_handler(obj)?);
      let (Some(target), Some(_handler)) = (target, handler) else {
        return Err(VmError::TypeError(
          "Cannot perform 'IsArray' on a proxy that has been revoked",
        ));
      };
      obj = target;
      continue;
    }
    return scope.heap().object_is_array(obj);
  }
}

/// `IsConcatSpreadable(O)` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-isconcatspreadable>
pub fn is_concat_spreadable_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<bool, VmError> {
  let Value::Object(obj) = value else {
    return Ok(false);
  };

  // Root `obj` across potential allocations from `Get` (accessors).
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let sym = intr.well_known_symbols().is_concat_spreadable;
  scope.push_root(Value::Symbol(sym))?;
  let key = PropertyKey::from_symbol(sym);

  // 1. Let spreadable be ? Get(O, @@isConcatSpreadable).
  let spreadable = internal_get_with_host_and_hooks(
    vm,
    &mut scope,
    host,
    hooks,
    obj,
    key,
    Value::Object(obj),
  )?;
  scope.push_root(spreadable)?;

  // 2. If spreadable is not undefined, return ToBoolean(spreadable).
  if !matches!(spreadable, Value::Undefined) {
    return scope.heap().to_boolean(spreadable);
  }

  // 3. Return ? IsArray(O).
  is_array_with_host_and_hooks(vm, &mut scope, host, hooks, Value::Object(obj))
}

/// `CreateListFromArrayLike(obj)` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-createlistfromarraylike>
///
/// This implementation is host-aware because it performs `Get(obj, ...)`, which can invoke user JS
/// via accessors.
pub fn create_list_from_array_like_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<Vec<Value>, VmError> {
  // Spec: `CreateListFromArrayLike` is usually called by `Function.prototype.apply`. The spec
  // handles `null`/`undefined` by producing an empty list.
  if matches!(value, Value::Undefined | Value::Null) {
    return Ok(Vec::new());
  }

  let mut scope = scope.reborrow();

  // Root `value` across boxing (`ToObject`) and subsequent property lookups.
  scope.push_root(value)?;

  // `O = ToObject(value)`
  let obj = scope.to_object(vm, host, hooks, value)?;
  scope.push_root(Value::Object(obj))?;

  // `len = LengthOfArrayLike(O)`
  let len = length_of_array_like_with_host_and_hooks(vm, &mut scope, host, hooks, obj)?;

  let mut out: Vec<Value> = Vec::new();
  out.try_reserve_exact(len).map_err(|_| VmError::OutOfMemory)?;

  for idx in 0..len {
    if idx % 1024 == 0 {
      vm.tick()?;
    }

    // `Get(O, ToString(idx))`
    let value = {
      // Use a nested scope so per-element key roots do not accumulate.
      let mut iter_scope = scope.reborrow();
      let idx_s = iter_scope.alloc_string(&idx.to_string())?;
      iter_scope.push_root(Value::String(idx_s))?;
      let key = PropertyKey::from_string(idx_s);
      get_with_host_and_hooks(vm, &mut iter_scope, host, hooks, obj, key, Value::Object(obj))?
    };

    // Root each element so values are kept alive across subsequent allocations and potential GC.
    scope.push_root(value)?;
    out.push(value);
  }

  Ok(out)
}

/// `LengthOfArrayLike(obj)` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-lengthofarraylike>
fn length_of_array_like_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
) -> Result<usize, VmError> {
  // `len = ToLength(Get(obj, "length"))`
  let length_key_s = scope.alloc_string("length")?;
  scope.push_root(Value::String(length_key_s))?;
  let length_key = PropertyKey::from_string(length_key_s);

  let length_value =
    get_with_host_and_hooks(vm, scope, host, hooks, obj, length_key, Value::Object(obj))?;
  let length_number = scope.to_number(vm, host, hooks, length_value)?;
  Ok(to_length(length_number))
}

/// ECMAScript `Get(O, P)` with minimal Proxy support.
///
/// This currently dispatches between:
/// - ordinary objects (including accessors via `ordinary_get_with_host_and_hooks`)
/// - Proxy objects (supports the `"get"` trap; forwards to target when missing).
///
/// Note: this does **not** yet implement Proxy invariants (non-configurable properties etc).
fn get_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  key: PropertyKey,
  receiver: Value,
) -> Result<Value, VmError> {
  let key_value = match key {
    PropertyKey::String(s) => Value::String(s),
    PropertyKey::Symbol(s) => Value::Symbol(s),
  };

  let mut scope = scope.reborrow();
  scope.push_roots(&[Value::Object(obj), key_value, receiver])?;

  let mut current = obj;
  for _ in 0..MAX_PROTOTYPE_CHAIN {
    let proxy = scope.heap().get_proxy_data(current)?;
    let Some(proxy) = proxy else {
      // Ordinary objects: `Get` uses `receiver` as `this` for accessors.
      return scope.ordinary_get_with_host_and_hooks(vm, host, hooks, current, key, receiver);
    };

    let (Some(target), Some(handler)) = (proxy.target, proxy.handler) else {
      return Err(VmError::TypeError(
        "Cannot perform 'get' on a proxy that has been revoked",
      ));
    };

    let get_key_s = scope.alloc_string("get")?;
    scope.push_root(Value::String(get_key_s))?;
    let get_key = PropertyKey::from_string(get_key_s);

    let trap = get_method_with_host_and_hooks(vm, &mut scope, host, hooks, Value::Object(handler), get_key)?;
    match trap {
      None => {
        current = target;
        continue;
      }
      Some(trap) => {
        let trap_args = [Value::Object(target), key_value, receiver];
        return vm.call_with_host_and_hooks(
          host,
          &mut scope,
          hooks,
          trap,
          Value::Object(handler),
          &trap_args,
        );
      }
    }
  }

  Err(VmError::TypeError("Proxy chain too deep"))
}

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
  scope.push_roots(&[
    Value::Object(constructor_obj),
    Value::Object(intrinsic_default_proto),
  ])?;

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

/// `SpeciesConstructor(O, defaultConstructor)` (ECMA-262), using an explicit embedder host context
/// and host hook implementation.
///
/// Spec: <https://tc39.es/ecma262/#sec-speciesconstructor>
pub fn species_constructor_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  default_constructor: Value,
) -> Result<Value, VmError> {
  // Note: `obj` is already a `GcObject`, so the spec assertion `Type(O) is Object` holds.

  // Root inputs and intermediate values across property lookups / calls, which can allocate and
  // trigger GC.
  let mut scope = scope.reborrow();
  scope.push_roots(&[Value::Object(obj), default_constructor])?;

  // 2. Let C be ? Get(O, "constructor").
  let key_s = scope.alloc_string("constructor")?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  let constructor = scope.ordinary_get_with_host_and_hooks(
    vm,
    host,
    hooks,
    obj,
    key,
    Value::Object(obj),
  )?;
  scope.push_root(constructor)?;

  // 3. If C is undefined, return defaultConstructor.
  if matches!(constructor, Value::Undefined) {
    return Ok(default_constructor);
  }

  // 4. If Type(C) is not Object, throw a TypeError exception.
  let Value::Object(constructor_obj) = constructor else {
    return Err(VmError::TypeError("SpeciesConstructor: constructor is not an object"));
  };

  // 5. Let S be ? Get(C, @@species).
  let species_sym = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented(
      "SpeciesConstructor requires intrinsics (create a Realm first)",
    ))?
    .well_known_symbols()
    .species;
  scope.push_root(Value::Symbol(species_sym))?;
  let species_key = PropertyKey::from_symbol(species_sym);
  let species = scope.ordinary_get_with_host_and_hooks(
    vm,
    host,
    hooks,
    constructor_obj,
    species_key,
    Value::Object(constructor_obj),
  )?;
  scope.push_root(species)?;

  // 6. If S is either undefined or null, return defaultConstructor.
  if matches!(species, Value::Undefined | Value::Null) {
    return Ok(default_constructor);
  }

  // 7. If IsConstructor(S) is true, return S.
  if scope.heap().is_constructor(species)? {
    return Ok(species);
  }

  // 8. Throw a TypeError exception.
  Err(VmError::TypeError("SpeciesConstructor: @@species is not a constructor"))
}

/// Convenience wrapper around [`species_constructor_with_host_and_hooks`] that passes a dummy host
/// context (`()`) and uses the VM-owned microtask queue as hooks.
///
/// ## ⚠️ Dummy `VmHost` context
///
/// `SpeciesConstructor` performs `Get` operations which can invoke user JS via accessors. Host
/// embeddings that need native handlers to observe real host state should prefer
/// [`species_constructor_with_host_and_hooks`].
pub fn species_constructor(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  obj: GcObject,
  default_constructor: Value,
) -> Result<Value, VmError> {
  let mut dummy_host = ();
  let mut hooks = mem::take(vm.microtask_queue_mut());
  let result = species_constructor_with_host_and_hooks(
    vm,
    scope,
    &mut dummy_host,
    &mut hooks,
    obj,
    default_constructor,
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
  scope.push_roots(&[new_target, Value::Object(proto)])?;

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
