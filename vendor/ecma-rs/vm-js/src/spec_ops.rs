//! Spec operations (ECMA-262 abstract operations).
//!
//! This module contains small helpers that mirror ECMA-262 abstract operations closely. These are
//! intended to be used by built-ins so their algorithms remain spec-shaped.

use crate::{
  GcBigInt, GcObject, GcString, PropertyDescriptorPatch, PropertyKey, Scope, Value, Vm, VmError,
  VmHost, VmHostHooks,
};
use std::mem;

/// ECMAScript `[[Get]](P, Receiver)` internal method dispatch.
pub fn internal_get_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  key: PropertyKey,
  receiver: Value,
) -> Result<Value, VmError> {
  scope.get_with_host_and_hooks(vm, host, hooks, obj, key, receiver)
}

/// ECMAScript `[[HasProperty]](P)` internal method dispatch.
pub fn internal_has_property_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  key: PropertyKey,
) -> Result<bool, VmError> {
  scope.has_property_with_host_and_hooks(vm, host, hooks, obj, key)
}

/// ECMAScript `[[Set]](P, V, Receiver)` internal method dispatch for ordinary and Proxy exotic
/// objects.
///
/// This is host-aware because Proxy `"set"` traps can invoke user code.
pub fn internal_set_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  key: PropertyKey,
  value: Value,
  receiver: Value,
) -> Result<bool, VmError> {
  // Root inputs across any allocations from trap lookup / invocation.
  let mut scope = scope.reborrow();
  let key_root = match key {
    PropertyKey::String(s) => Value::String(s),
    PropertyKey::Symbol(s) => Value::Symbol(s),
  };
  scope.push_roots(&[Value::Object(obj), key_root, value, receiver])?;
  scope.set_with_host_and_hooks(vm, host, hooks, obj, key, value, receiver)
}

/// ECMAScript `[[Delete]](P)` internal method dispatch for ordinary and Proxy exotic objects.
///
/// This is host-aware because Proxy `"deleteProperty"` traps can invoke user code.
pub fn internal_delete_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  key: PropertyKey,
) -> Result<bool, VmError> {
  // Root inputs across any allocations from trap lookup / invocation.
  let mut scope = scope.reborrow();
  let key_root = match key {
    PropertyKey::String(s) => Value::String(s),
    PropertyKey::Symbol(s) => Value::Symbol(s),
  };
  scope.push_roots(&[Value::Object(obj), key_root])?;

  scope.delete_with_host_and_hooks(vm, host, hooks, obj, key)
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
  obj: GcObject,
) -> Result<Vec<Value>, VmError> {
  scope.push_root(Value::Object(obj))?;

  let len = length_of_array_like_with_host_and_hooks(vm, scope, host, hooks, obj)?;

  let mut out: Vec<Value> = Vec::new();
  out.try_reserve_exact(len).map_err(|_| VmError::OutOfMemory)?;

  // Fast path for Arrays: most dense arrays store their indexed elements as ordinary data
  // properties. Avoid allocating `ToString(idx)` keys and avoid `Get` when the value is available
  // directly from the array's dense element table.
  //
  // This is particularly important for test262's `regExpUtils.js::buildString`, which uses
  // `String.fromCodePoint.apply(null, codePoints)` with 10k-element arrays in tight loops.
  // `CreateListFromArrayLike` is defined in terms of `Get(obj, key)` and therefore must observe
  // Proxy traps, even when the Proxy target is an Array exotic object. The array fast-path reads
  // indexed elements directly from the dense element table and would bypass `Proxy.[[Get]]`.
  //
  // Note: `Heap::object_is_array` is intentionally not Proxy-aware; calling it on a Proxy would
  // return `VmError::InvalidHandle` because Proxies are stored as a distinct heap allocation kind.
  let is_array = !scope.heap().is_proxy_object(obj) && scope.heap().object_is_array(obj)?;

  // Budget the per-element work (string allocation + `Get`) more aggressively than the default
  // 1024-iteration cadence used in many native loops. `CreateListFromArrayLike` is often invoked by
  // `Function.prototype.apply` on attacker-controlled "array-like" objects and can otherwise
  // allocate large numbers of temporary property keys before fuel is observed (see
  // `budget_integration::builtins_function_apply_consumes_fuel_in_native_loop`).
  const TICK_EVERY: usize = 64;
  for idx in 0..len {
    if idx % TICK_EVERY == 0 {
      vm.tick()?;
    }

    // `Get(O, ToString(idx))`
    let value = if is_array && idx <= u32::MAX as usize {
      match scope
        .heap()
        .array_fast_own_data_element_value(obj, idx as u32)?
      {
        Some(v) => v,
        None => {
          // Fallback for holes / accessors / sparse indices: use spec-shaped `Get`.
          let mut iter_scope = scope.reborrow();
          iter_scope.push_root(Value::Object(obj))?;

          let idx_s = alloc_u64_decimal_string(&mut iter_scope, idx as u64)?;
          iter_scope.push_root(Value::String(idx_s))?;
          let key = PropertyKey::from_string(idx_s);
          iter_scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?
        }
      }
    } else {
      // Generic array-like object: allocate the index key string and use `Get`.
      let mut iter_scope = scope.reborrow();
      iter_scope.push_root(Value::Object(obj))?;

      let idx_s = alloc_u64_decimal_string(&mut iter_scope, idx as u64)?;
      iter_scope.push_root(Value::String(idx_s))?;
      let key = PropertyKey::from_string(idx_s);
      iter_scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?
    };

    // Root each GC-managed element so values are kept alive across subsequent allocations and
    // potential GC. Primitive values (undefined/null/bool/number/bigint) do not need rooting.
    if matches!(value, Value::String(_) | Value::Symbol(_) | Value::Object(_)) {
      scope.push_root(value)?;
    }
    out.push(value);
  }

  Ok(out)
}

/// `LengthOfArrayLike(obj)` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-lengthofarraylike>
pub fn length_of_array_like_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
) -> Result<usize, VmError> {
  // `len = ToLength(Get(obj, "length"))`
  //
  // Root `obj` across the `Get` and `ToLength` conversions, both of which can allocate and trigger
  // GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;

  let length_key_s = scope.alloc_string("length")?;
  scope.push_root(Value::String(length_key_s))?;
  let length_key = PropertyKey::from_string(length_key_s);

  let length_value =
    scope.get_with_host_and_hooks(vm, host, hooks, obj, length_key, Value::Object(obj))?;
  vm.tick()?;
  let len = scope.to_length(vm, host, hooks, length_value)?;
  vm.tick()?;
  Ok(len)
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

  // `Get(constructor, "prototype")` (Proxy-aware).
  let proto =
    scope.get_with_host_and_hooks(vm, host, hooks, constructor_obj, key, Value::Object(constructor_obj))?;
  if let Value::Object(o) = proto {
    return Ok(o);
  }

  // If `constructor.prototype` is not an object, fall back to the intrinsic default prototype from
  // the constructor's Realm (ECMA-262 `GetFunctionRealm` + `intrinsicDefaultProto` resolution).
  //
  // Note: `vm-js` stores a function's realm id in `[[JobRealm]]` (see `Heap::get_function_job_realm`)
  // and keeps per-realm intrinsics snapshots in `Vm`.
  let Some(realm_c) = scope.heap().get_function_job_realm(constructor_obj) else {
    return Ok(intrinsic_default_proto);
  };
  let Some(intr_c) = vm.intrinsics_for_realm(realm_c) else {
    return Ok(intrinsic_default_proto);
  };

  // Map the current-realm `intrinsicDefaultProto` to the corresponding intrinsic object from
  // `realm_c`.
  let Some(intr_this) = vm.intrinsics() else {
    return Ok(intrinsic_default_proto);
  };

  Ok(if intrinsic_default_proto == intr_this.object_prototype() {
    intr_c.object_prototype()
  } else if intrinsic_default_proto == intr_this.function_prototype() {
    intr_c.function_prototype()
  } else if intrinsic_default_proto == intr_this.array_prototype() {
    intr_c.array_prototype()
  } else if intrinsic_default_proto == intr_this.promise_prototype() {
    intr_c.promise_prototype()
  } else if intrinsic_default_proto == intr_this.suppressed_error_prototype() {
    intr_c.suppressed_error_prototype()
  } else if intrinsic_default_proto == intr_this.disposable_stack_prototype() {
    intr_c.disposable_stack_prototype()
  } else if intrinsic_default_proto == intr_this.async_disposable_stack_prototype() {
    intr_c.async_disposable_stack_prototype()
  } else if intrinsic_default_proto == intr_this.string_prototype() {
    intr_c.string_prototype()
  } else if intrinsic_default_proto == intr_this.regexp_prototype() {
    intr_c.regexp_prototype()
  } else if intrinsic_default_proto == intr_this.number_prototype() {
    intr_c.number_prototype()
  } else if intrinsic_default_proto == intr_this.boolean_prototype() {
    intr_c.boolean_prototype()
  } else if intrinsic_default_proto == intr_this.bigint_prototype() {
    intr_c.bigint_prototype()
  } else if intrinsic_default_proto == intr_this.date_prototype() {
    intr_c.date_prototype()
  } else if intrinsic_default_proto == intr_this.symbol_prototype() {
    intr_c.symbol_prototype()
  } else if intrinsic_default_proto == intr_this.array_buffer_prototype() {
    intr_c.array_buffer_prototype()
  } else if intrinsic_default_proto == intr_this.uint8_array_prototype() {
    intr_c.uint8_array_prototype()
  } else if intrinsic_default_proto == intr_this.int8_array_prototype() {
    intr_c.int8_array_prototype()
  } else if intrinsic_default_proto == intr_this.uint8_clamped_array_prototype() {
    intr_c.uint8_clamped_array_prototype()
  } else if intrinsic_default_proto == intr_this.int16_array_prototype() {
    intr_c.int16_array_prototype()
  } else if intrinsic_default_proto == intr_this.uint16_array_prototype() {
    intr_c.uint16_array_prototype()
  } else if intrinsic_default_proto == intr_this.int32_array_prototype() {
    intr_c.int32_array_prototype()
  } else if intrinsic_default_proto == intr_this.uint32_array_prototype() {
    intr_c.uint32_array_prototype()
  } else if intrinsic_default_proto == intr_this.float32_array_prototype() {
    intr_c.float32_array_prototype()
  } else if intrinsic_default_proto == intr_this.float64_array_prototype() {
    intr_c.float64_array_prototype()
  } else if intrinsic_default_proto == intr_this.bigint64_array_prototype() {
    intr_c.bigint64_array_prototype()
  } else if intrinsic_default_proto == intr_this.biguint64_array_prototype() {
    intr_c.biguint64_array_prototype()
  } else if intrinsic_default_proto == intr_this.data_view_prototype() {
    intr_c.data_view_prototype()
  } else if intrinsic_default_proto == intr_this.map_prototype() {
    intr_c.map_prototype()
  } else if intrinsic_default_proto == intr_this.set_prototype() {
    intr_c.set_prototype()
  } else if intrinsic_default_proto == intr_this.weak_map_prototype() {
    intr_c.weak_map_prototype()
  } else if intrinsic_default_proto == intr_this.weak_set_prototype() {
    intr_c.weak_set_prototype()
  } else if intrinsic_default_proto == intr_this.weak_ref_prototype() {
    intr_c.weak_ref_prototype()
  } else if intrinsic_default_proto == intr_this.finalization_registry_prototype() {
    intr_c.finalization_registry_prototype()
  } else if intrinsic_default_proto == intr_this.error_prototype() {
    intr_c.error_prototype()
  } else if intrinsic_default_proto == intr_this.type_error_prototype() {
    intr_c.type_error_prototype()
  } else if intrinsic_default_proto == intr_this.range_error_prototype() {
    intr_c.range_error_prototype()
  } else if intrinsic_default_proto == intr_this.reference_error_prototype() {
    intr_c.reference_error_prototype()
  } else if intrinsic_default_proto == intr_this.syntax_error_prototype() {
    intr_c.syntax_error_prototype()
  } else if intrinsic_default_proto == intr_this.eval_error_prototype() {
    intr_c.eval_error_prototype()
  } else if intrinsic_default_proto == intr_this.uri_error_prototype() {
    intr_c.uri_error_prototype()
  } else if intrinsic_default_proto == intr_this.aggregate_error_prototype() {
    intr_c.aggregate_error_prototype()
  } else {
    intrinsic_default_proto
  })
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
  // `Get(O, "constructor")` (Proxy-aware).
  let constructor = scope.get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;
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
  // `Get(C, @@species)` (Proxy-aware).
  let species = scope.get_with_host_and_hooks(
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

/// `CreateDataProperty(O, P, V)` (ECMA-262), using `[[DefineOwnProperty]]` dispatch that can invoke
/// user code (Proxy traps).
///
/// Spec: <https://tc39.es/ecma262/#sec-createdataproperty>
///
/// This is host-aware because Proxy `"defineProperty"` traps can invoke user JS.
pub fn create_data_property_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  key: PropertyKey,
  value: Value,
) -> Result<bool, VmError> {
  scope.define_own_property_with_host_and_hooks(
    vm,
    host,
    hooks,
    obj,
    key,
    PropertyDescriptorPatch {
      value: Some(value),
      writable: Some(true),
      enumerable: Some(true),
      configurable: Some(true),
      ..Default::default()
    },
  )
}

/// `CreateDataPropertyOrThrow(O, P, V)` (ECMA-262), using `[[DefineOwnProperty]]` dispatch that can
/// invoke user code (Proxy traps).
///
/// Spec: <https://tc39.es/ecma262/#sec-createdatapropertyorthrow>
pub fn create_data_property_or_throw_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  key: PropertyKey,
  value: Value,
) -> Result<(), VmError> {
  let ok = create_data_property_with_host_and_hooks(vm, scope, host, hooks, obj, key, value)?;
  if ok {
    Ok(())
  } else {
    Err(VmError::TypeError("CreateDataProperty rejected"))
  }
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

/// `DeletePropertyOrThrow(O, P)` (ECMA-262), using `[[Delete]]` dispatch that can invoke user code
/// (Proxy traps).
///
/// Spec: <https://tc39.es/ecma262/#sec-deletepropertyorthrow>
pub fn delete_property_or_throw_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  key: PropertyKey,
) -> Result<(), VmError> {
  let ok = internal_delete_with_host_and_hooks(vm, scope, host, hooks, obj, key)?;
  if ok {
    Ok(())
  } else {
    Err(VmError::TypeError("DeletePropertyOrThrow rejected"))
  }
}

/// `CopyDataProperties(target, source, excludedItems)` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-copydataproperties>
///
/// This is the abstract operation used by:
/// - object spread (`{...src}`),
/// - object rest (`{a, ...rest}`),
/// - and other spec algorithms like `Object.assign`.
///
/// This implementation is **Proxy-aware**: it uses the internal-method dispatch layer
/// (`[[OwnPropertyKeys]]`, `[[GetOwnProperty]]`, `[[Get]]`) so Proxy traps are observed.
pub fn copy_data_properties_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  target: GcObject,
  source: Value,
  excluded_items: &[PropertyKey],
) -> Result<(), VmError> {
  // Keep all temporary roots local to this operation.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(target))?;
  scope.push_root(source)?;

  // 1. If source is null or undefined, return target.
  if matches!(source, Value::Undefined | Value::Null) {
    return Ok(());
  }

  // 2. Let from be ! ToObject(source).
  let from = scope.to_object(vm, host, hooks, source)?;
  scope.push_root(Value::Object(from))?;

  // 3. Let keys be ? from.[[OwnPropertyKeys]]().
  let keys = scope.object_own_property_keys_with_host_and_hooks(vm, host, hooks, from)?;

  // Root returned keys as values so they stay alive even if they are not otherwise reachable
  // (notably: Proxy `ownKeys` traps can synthesize fresh String/Symbol values).
  let mut key_roots: Vec<Value> = Vec::new();
  key_roots
    .try_reserve_exact(keys.len().saturating_add(excluded_items.len()))
    .map_err(|_| VmError::OutOfMemory)?;
  for key in &keys {
    key_roots.push(match key {
      PropertyKey::String(s) => Value::String(*s),
      PropertyKey::Symbol(s) => Value::Symbol(*s),
    });
  }
  for key in excluded_items {
    key_roots.push(match key {
      PropertyKey::String(s) => Value::String(*s),
      PropertyKey::Symbol(s) => Value::Symbol(*s),
    });
  }
  scope.push_roots(&key_roots)?;

  for next_key in keys {
    // Per-copied-property tick: spreading/rest-copying can iterate many keys without evaluating
    // nested expressions.
    vm.tick()?;

    // 4a. If nextKey is in excludedItems, continue.
    if excluded_items
      .iter()
      .any(|excluded| scope.heap().property_key_eq(excluded, &next_key))
    {
      continue;
    }

    // 4b. Let desc be ? from.[[GetOwnProperty]](nextKey).
    let Some(desc) = scope.object_get_own_property_with_host_and_hooks(vm, host, hooks, from, next_key)?
    else {
      continue;
    };

    // 4c. If desc.[[Enumerable]] is false, continue.
    if !desc.enumerable {
      continue;
    }

    // 4d. Let propValue be ? Get(from, nextKey).
    let prop_value =
      scope.get_with_host_and_hooks(vm, host, hooks, from, next_key, Value::Object(from))?;

    // 4e. Perform ! CreateDataProperty(target, nextKey, propValue).
    let ok = scope.create_data_property(target, next_key, prop_value)?;
    if !ok {
      return Err(VmError::Unimplemented("CreateDataProperty returned false"));
    }
  }

  Ok(())
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
  let func = scope.get_with_host_and_hooks(vm, host, hooks, obj, key, receiver)?;
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

/// `CreateArrayFromList(elements)` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-createarrayfromlist>
///
/// This is used by Proxy `[[Call]]` / `[[Construct]]` to materialize the `argumentsList` into a
/// real Array object passed to apply/construct traps.
pub fn create_array_from_list(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  elements: &[Value],
) -> Result<GcObject, VmError> {
  let mut scope = scope.reborrow();

  // Root `elements` before any allocations (array allocation, key-string allocation) so a GC cannot
  // collect an argument value that is only kept alive by the host stack.
  //
  // Use chunking so the root-stack growth path treats the still-unpushed tail as extra roots.
  const CHUNK_SIZE: usize = 256;
  let mut start = 0usize;
  while start < elements.len() {
    let end = elements
      .len()
      .min(start.saturating_add(CHUNK_SIZE));
    let chunk = &elements[start..end];
    let remaining = &elements[end..];
    scope.push_roots_with_extra_roots(chunk, remaining, &[])?;
    start = end;
    if start < elements.len() {
      vm.tick()?;
    }
  }

  // `CreateArrayFromList` uses `ArrayCreate(0)` and then populates the array with
  // `CreateDataPropertyOrThrow`.
  let array = scope.alloc_array(0)?;
  scope.push_root(Value::Object(array))?;

  // `ArrayCreate` sets `[[Prototype]]` to `%Array.prototype%`.
  if let Some(intr) = vm.intrinsics() {
    scope
      .heap_mut()
      .object_set_prototype(array, Some(intr.array_prototype()))?;
  }

  const TICK_EVERY: usize = 256;
  for (i, &elem) in elements.iter().enumerate() {
    if i != 0 && i % TICK_EVERY == 0 {
      vm.tick()?;
    }

    // Root `array` and the current element across allocation of the key string and any subsequent
    // heap growth performed by `CreateDataPropertyOrThrow`.
    let mut elem_scope = scope.reborrow();
    elem_scope.push_root(Value::Object(array))?;
    elem_scope.push_root(elem)?;

    let key_s = alloc_u64_decimal_string(&mut elem_scope, i as u64)?;
    // Root the key string until it has been stored into the array's property table.
    elem_scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    elem_scope.create_data_property_or_throw(array, key, elem)?;
  }

  Ok(array)
}

fn alloc_u64_decimal_string(scope: &mut Scope<'_>, mut n: u64) -> Result<GcString, VmError> {
  let mut buf = [0u8; 20];
  let mut i = buf.len();
  if n == 0 {
    i = i.saturating_sub(1);
    buf[i] = b'0';
  } else {
    while n != 0 {
      i = i.saturating_sub(1);
      buf[i] = b'0' + (n % 10) as u8;
      n /= 10;
    }
  }

  let s = std::str::from_utf8(&buf[i..])
    .map_err(|_| VmError::InvariantViolation("u64 decimal string conversion produced invalid utf-8"))?;
  scope.alloc_string(s)
}

/// `RequireObjectCoercible(argument)` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-requireobjectcoercible>
#[inline]
pub fn require_object_coercible(value: Value) -> Result<Value, VmError> {
  match value {
    Value::Undefined | Value::Null => Err(VmError::TypeError(
      "Cannot convert undefined or null to object",
    )),
    other => Ok(other),
  }
}

/// `IsRegExp(argument)` (ECMA-262).
///
/// Spec: <https://tc39.es/ecma262/#sec-isregexp>
///
/// This operation is observable for Proxy objects and can invoke user JS via:
/// - `Get(argument, @@match)` (accessors / Proxy traps)
///
/// Important: Per spec, primitive values are **not** boxed during `IsRegExp`. This avoids observable
/// prototype lookups like `Boolean.prototype[Symbol.match]` for primitive `argument`s.
pub fn is_regexp_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<bool, VmError> {
  // 1. If Type(argument) is not Object, return false.
  let Value::Object(obj) = value else {
    return Ok(false);
  };

  // Root `obj` across `Get` which can allocate and/or invoke user JS.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let sym = intr.well_known_symbols().match_;
  scope.push_root(Value::Symbol(sym))?;
  let key = PropertyKey::from_symbol(sym);

  // 2. Let matcher be ? Get(argument, @@match).
  let matcher = internal_get_with_host_and_hooks(vm, &mut scope, host, hooks, obj, key, Value::Object(obj))?;
  scope.push_root(matcher)?;

  // 3. If matcher is not undefined, return ToBoolean(matcher).
  if !matches!(matcher, Value::Undefined) {
    return scope.heap().to_boolean(matcher);
  }

  // 4. If argument has a [[RegExpMatcher]] internal slot, return true.
  // 5. Return false.
  Ok(scope.heap().is_regexp_object(obj))
}

fn get_internal_data_property(
  scope: &mut Scope<'_>,
  obj: GcObject,
  marker: &str,
) -> Result<Option<Value>, VmError> {
  // Root `obj` while ensuring the marker symbol. This may allocate and GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;

  let marker_sym = match marker {
    "vm-js.internal.StringData" => match scope.heap().internal_string_data_symbol() {
      Some(sym) => sym,
      None => scope.heap_mut().ensure_internal_string_data_symbol()?,
    },
    "vm-js.internal.SymbolData" => match scope.heap().internal_symbol_data_symbol() {
      Some(sym) => sym,
      None => scope.heap_mut().ensure_internal_symbol_data_symbol()?,
    },
    "vm-js.internal.BooleanData" => match scope.heap().internal_boolean_data_symbol() {
      Some(sym) => sym,
      None => scope.heap_mut().ensure_internal_boolean_data_symbol()?,
    },
    "vm-js.internal.NumberData" => match scope.heap().internal_number_data_symbol() {
      Some(sym) => sym,
      None => scope.heap_mut().ensure_internal_number_data_symbol()?,
    },
    "vm-js.internal.BigIntData" => match scope.heap().internal_bigint_data_symbol() {
      Some(sym) => sym,
      None => scope.heap_mut().ensure_internal_bigint_data_symbol()?,
    },
    _ => {
      return Err(VmError::InvariantViolation(
        "unknown internal data property marker",
      ))
    }
  };
  let marker_key = PropertyKey::from_symbol(marker_sym);

  match scope.heap().object_get_own_data_property_value(obj, &marker_key) {
    Ok(v) => Ok(v),
    // If the user mutated the marker property into an accessor, treat it as "missing" so the
    // caller can throw a TypeError.
    Err(VmError::PropertyNotData) => Ok(None),
    Err(e) => Err(e),
  }
}

/// `thisStringValue(value)` (ECMA-262) for `String` builtins.
///
/// Spec: <https://tc39.es/ecma262/#sec-thisstringvalue>
pub fn this_string_value(scope: &mut Scope<'_>, value: Value) -> Result<GcString, VmError> {
  match value {
    Value::String(s) => Ok(s),
    Value::Object(obj) => match get_internal_data_property(scope, obj, "vm-js.internal.StringData")?
    {
      Some(Value::String(s)) => Ok(s),
      _ => Err(VmError::TypeError(
        "String.prototype.toString called on incompatible receiver",
      )),
    },
    _ => Err(VmError::TypeError(
      "String.prototype.toString called on incompatible receiver",
    )),
  }
}

/// `thisNumberValue(value)` (ECMA-262) for `Number` builtins.
///
/// Spec: <https://tc39.es/ecma262/#sec-thisnumbervalue>
pub fn this_number_value(scope: &mut Scope<'_>, value: Value) -> Result<f64, VmError> {
  match value {
    Value::Number(n) => Ok(n),
    Value::Object(obj) => match get_internal_data_property(scope, obj, "vm-js.internal.NumberData")?
    {
      Some(Value::Number(n)) => Ok(n),
      _ => Err(VmError::TypeError(
        "Number.prototype.valueOf called on incompatible receiver",
      )),
    },
    _ => Err(VmError::TypeError(
      "Number.prototype.valueOf called on incompatible receiver",
    )),
  }
}

/// `thisBooleanValue(value)` (ECMA-262) for `Boolean` builtins.
///
/// Spec: <https://tc39.es/ecma262/#sec-thisbooleanvalue>
pub fn this_boolean_value(scope: &mut Scope<'_>, value: Value) -> Result<bool, VmError> {
  match value {
    Value::Bool(b) => Ok(b),
    Value::Object(obj) => {
      match get_internal_data_property(scope, obj, "vm-js.internal.BooleanData")? {
        Some(Value::Bool(b)) => Ok(b),
        _ => Err(VmError::TypeError(
          "Boolean.prototype.valueOf called on incompatible receiver",
        )),
      }
    }
    _ => Err(VmError::TypeError(
      "Boolean.prototype.valueOf called on incompatible receiver",
    )),
  }
}

/// `thisBigIntValue(value)` (ECMA-262) for `BigInt` builtins.
///
/// Spec: <https://tc39.es/ecma262/#sec-thisbigintvalue>
pub fn this_bigint_value(scope: &mut Scope<'_>, value: Value) -> Result<GcBigInt, VmError> {
  match value {
    Value::BigInt(b) => {
      // Validate the handle (and preserve the old `thisBigIntValue` contract of surfacing
      // `InvalidHandle` for corrupted wrapper state) without cloning the BigInt payload.
      scope.heap().get_bigint(b)?;
      Ok(b)
    }
    Value::Object(obj) => {
      // Fast path: wrapper objects created by vm-js store the BigInt value under an internal symbol
      // (not `Symbol.for`).
      // `thisBigIntValue` checks for an internal slot and must not invoke Proxy traps or user code.
      // BigInt wrapper internal slots are not present on Proxy objects, even when the Proxy target
      // is a BigInt wrapper object.
      if scope.heap().is_proxy_object(obj) {
        return Err(VmError::TypeError(
          "BigInt.prototype.valueOf called on incompatible receiver",
        ));
      }

      let marker_sym = match scope.heap().internal_bigint_data_symbol() {
        Some(sym) => sym,
        None => scope.heap_mut().ensure_internal_bigint_data_symbol()?,
      };
      let marker_key = PropertyKey::from_symbol(marker_sym);
      if let Some(Value::BigInt(b)) = scope
        .heap()
        .object_get_own_data_property_value(obj, &marker_key)?
      {
        scope.heap().get_bigint(b)?;
        return Ok(b);
      }

      // Compatibility path: older callers used `Symbol.for("vm-js.internal.BigIntData")` markers.
      match get_internal_data_property(scope, obj, "vm-js.internal.BigIntData")? {
        Some(Value::BigInt(b)) => {
          scope.heap().get_bigint(b)?;
          Ok(b)
        }
        _ => Err(VmError::TypeError(
          "BigInt.prototype.valueOf called on incompatible receiver",
        )),
      }
    }
    _ => Err(VmError::TypeError(
      "BigInt.prototype.valueOf called on incompatible receiver",
    )),
  }
}
