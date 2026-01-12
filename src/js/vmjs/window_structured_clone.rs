//! `structuredClone` implementation for `vm-js` Window realms.
//!
//! This is a minimal implementation of the HTML structured clone algorithm focused on the object /
//! array cloning path needed by real-world scripts.
//!
//! Key spec detail: the HTML algorithm uses the ECMAScript abstract operation
//! `EnumerableOwnProperties(value, kind)`, which (like `Object.keys`) enumerates **only string**
//! property keys. Enumerable symbol-keyed properties must be ignored.

use std::collections::HashMap;

use vm_js::{
  GcObject, GcString, GcSymbol, Intrinsics, PropertyDescriptor, PropertyKey, PropertyKind, Realm,
  Scope, Value, Vm, VmError, VmHost, VmHostHooks,
};

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn data_desc(value: Value, writable: bool, enumerable: bool, configurable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable,
    configurable,
    kind: PropertyKind::Data { value, writable },
  }
}

fn require_intrinsics(vm: &Vm) -> Result<Intrinsics, VmError> {
  vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))
}

fn create_array_object(vm: &mut Vm, scope: &mut Scope<'_>, len: usize) -> Result<GcObject, VmError> {
  let intr = require_intrinsics(vm)?;
  let array = scope.alloc_array(len)?;
  scope
    .heap_mut()
    .object_set_prototype(array, Some(intr.array_prototype()))?;
  Ok(array)
}

fn create_object_object(vm: &mut Vm, scope: &mut Scope<'_>) -> Result<GcObject, VmError> {
  let intr = require_intrinsics(vm)?;
  let obj = scope.alloc_object()?;
  scope
    .heap_mut()
    .object_set_prototype(obj, Some(intr.object_prototype()))?;
  Ok(obj)
}

fn clone_array_buffer(vm: &mut Vm, scope: &mut Scope<'_>, obj: GcObject) -> Result<GcObject, VmError> {
  let intr = require_intrinsics(vm)?;
  let bytes = scope.heap().array_buffer_data(obj)?;
  let mut out: Vec<u8> = Vec::new();
  out
    .try_reserve_exact(bytes.len())
    .map_err(|_| VmError::OutOfMemory)?;
  out.extend_from_slice(bytes);

  let ab = scope.alloc_array_buffer_from_u8_vec(out)?;
  scope
    .heap_mut()
    .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
  Ok(ab)
}

fn clone_uint8_array(vm: &mut Vm, scope: &mut Scope<'_>, obj: GcObject) -> Result<GcObject, VmError> {
  let intr = require_intrinsics(vm)?;
  let bytes = scope.heap().uint8_array_data(obj)?;
  let len = bytes.len();
  let mut out: Vec<u8> = Vec::new();
  out
    .try_reserve_exact(bytes.len())
    .map_err(|_| VmError::OutOfMemory)?;
  out.extend_from_slice(bytes);

  let buffer = scope.alloc_array_buffer_from_u8_vec(out)?;
  scope
    .heap_mut()
    .object_set_prototype(buffer, Some(intr.array_buffer_prototype()))?;

  let view = scope.alloc_uint8_array(buffer, 0, len)?;
  scope
    .heap_mut()
    .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;
  Ok(view)
}

fn enumerable_own_string_keys(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  obj: GcObject,
) -> Result<Vec<GcString>, VmError> {
  let own_keys = scope.ordinary_own_property_keys_with_tick(obj, || vm.tick())?;
  let mut keys: Vec<GcString> = Vec::new();
  keys
    .try_reserve_exact(own_keys.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for (i, key) in own_keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let PropertyKey::String(key_str) = key else {
      continue;
    };
    let Some(desc) = scope.ordinary_get_own_property_with_tick(obj, key, || vm.tick())? else {
      continue;
    };
    if desc.enumerable {
      keys.push(key_str);
    }
  }

  Ok(keys)
}

fn structured_clone_value(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  object_map: &mut HashMap<GcObject, GcObject>,
) -> Result<Value, VmError> {
  match value {
    Value::Undefined
    | Value::Null
    | Value::Bool(_)
    | Value::Number(_)
    | Value::BigInt(_)
    | Value::String(_) => Ok(value),
    Value::Symbol(_) => Err(VmError::TypeError("structuredClone cannot clone symbols")),
    Value::Object(obj) => structured_clone_object(vm, scope, host, hooks, obj, object_map),
  }
}

fn structured_clone_object(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  obj: GcObject,
  object_map: &mut HashMap<GcObject, GcObject>,
) -> Result<Value, VmError> {
  if let Some(existing) = object_map.get(&obj).copied() {
    return Ok(Value::Object(existing));
  }

  if scope.heap().is_callable(Value::Object(obj))? {
    return Err(VmError::TypeError("structuredClone cannot clone functions"));
  }

  // Typed arrays / ArrayBuffer are used by Web APIs (TextEncoder, fetch bodies). Handle the subset
  // `vm-js` currently implements.
  if scope.heap().is_array_buffer_object(obj) {
    let cloned = clone_array_buffer(vm, scope, obj)?;
    object_map
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    object_map.insert(obj, cloned);
    return Ok(Value::Object(cloned));
  }
  if scope.heap().is_uint8_array_object(obj) {
    let cloned = clone_uint8_array(vm, scope, obj)?;
    object_map
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    object_map.insert(obj, cloned);
    return Ok(Value::Object(cloned));
  }

  let is_array = scope.heap().object_is_array(obj)?;
  let cloned_obj = if is_array {
    // Preserve array length even for trailing holes by allocating the clone with the source
    // length upfront.
    let length_key = alloc_key(scope, "length")?;
    let length_val = scope
      .heap()
      .object_get_own_data_property_value(obj, &length_key)?
      .unwrap_or(Value::Number(0.0));
    let len = match length_val {
      Value::Number(n) if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= (u32::MAX as f64) => {
        n as usize
      }
      _ => 0,
    };
    create_array_object(vm, scope, len)?
  } else {
    create_object_object(vm, scope)?
  };

  // Record the mapping before cloning properties so cycles can be resolved.
  object_map
    .try_reserve(1)
    .map_err(|_| VmError::OutOfMemory)?;
  object_map.insert(obj, cloned_obj);

  // Snapshot enumerable own string keys (EnumerableOwnProperties semantics: **string keys only**).
  let keys = enumerable_own_string_keys(vm, scope, obj)?;
  for (i, key_str) in keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }

    let mut iter_scope = scope.reborrow();
    iter_scope.push_root(Value::Object(obj))?;
    iter_scope.push_root(Value::Object(cloned_obj))?;
    iter_scope.push_root(Value::String(key_str))?;

    // Use `[[Get]]` to fetch the property value (invokes getters) with the real host context.
    let value = vm.get_with_host_and_hooks(
      host,
      &mut iter_scope,
      hooks,
      obj,
      PropertyKey::from_string(key_str),
    )?;
    let value = iter_scope.push_root(value)?;

    let cloned_value = structured_clone_value(vm, &mut iter_scope, host, hooks, value, object_map)?;
    iter_scope.define_property(
      cloned_obj,
      PropertyKey::from_string(key_str),
      // Per HTML structured clone: define as a new data property with default attributes.
      data_desc(cloned_value, true, true, true),
    )?;
  }

  Ok(Value::Object(cloned_obj))
}

fn object_get_own_property_symbols_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let obj_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let obj = scope.to_object(vm, host, hooks, obj_val)?;
  scope.push_root(Value::Object(obj))?;

  let own_keys = scope.ordinary_own_property_keys_with_tick(obj, || vm.tick())?;
  let mut syms: Vec<GcSymbol> = Vec::new();
  syms
    .try_reserve_exact(own_keys.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for (i, key) in own_keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    if let PropertyKey::Symbol(sym) = key {
      syms.push(sym);
    }
  }

  let array = create_array_object(vm, scope, syms.len())?;
  scope.push_root(Value::Object(array))?;

  for (i, sym) in syms.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let mut iter_scope = scope.reborrow();
    iter_scope.push_root(Value::Object(array))?;
    iter_scope.push_root(Value::Symbol(sym))?;
    let idx_s = iter_scope.alloc_string(&i.to_string())?;
    iter_scope.push_root(Value::String(idx_s))?;
    let idx_key = PropertyKey::from_string(idx_s);
    iter_scope.define_property(
      array,
      idx_key,
      data_desc(Value::Symbol(sym), true, true, true),
    )?;
  }

  Ok(Value::Object(array))
}

fn structured_clone_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let value = args.get(0).copied().unwrap_or(Value::Undefined);

  let mut scope = scope.reborrow();
  let value = scope.push_root(value)?;
  let mut object_map: HashMap<GcObject, GcObject> = HashMap::new();

  structured_clone_value(vm, &mut scope, host, hooks, value, &mut object_map)
}

pub(crate) fn install_window_structured_clone(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  realm: &Realm,
  global: GcObject,
) -> Result<(), VmError> {
  // Install `Object.getOwnPropertySymbols` if the engine doesn't provide it yet. Tests for
  // structuredClone rely on this standard library API.
  {
    let object_ctor = realm.intrinsics().object_constructor();
    scope.push_root(Value::Object(object_ctor))?;
    let key = alloc_key(scope, "getOwnPropertySymbols")?;
    if scope
      .heap()
      .object_get_own_property(object_ctor, &key)?
      .is_none()
    {
      let call_id = vm.register_native_call(object_get_own_property_symbols_native)?;
      let name_s = scope.alloc_string("getOwnPropertySymbols")?;
      scope.push_root(Value::String(name_s))?;
      let func = scope.alloc_native_function(call_id, None, name_s, 1)?;
      scope.heap_mut().object_set_prototype(
        func,
        Some(realm.intrinsics().function_prototype()),
      )?;
      scope.push_root(Value::Object(func))?;
      scope.define_property(object_ctor, key, data_desc(Value::Object(func), true, false, true))?;
    }
  }

  let call_id = vm.register_native_call(structured_clone_native)?;
  let name_s = scope.alloc_string("structuredClone")?;
  scope.push_root(Value::String(name_s))?;
  let func = scope.alloc_native_function(call_id, None, name_s, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(func))?;

  let key = alloc_key(scope, "structuredClone")?;
  scope.define_property(global, key, data_desc(Value::Object(func), true, false, true))?;
  Ok(())
}
