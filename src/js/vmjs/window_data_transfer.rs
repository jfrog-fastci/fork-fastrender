//! Minimal `DataTransfer` implementation for `vm-js` Window realms.
//!
//! Many real-world drag-and-drop libraries depend on `DataTransfer` existing and, in particular,
//! expect `event.dataTransfer.items` and/or `event.dataTransfer.files` to be present. FastRender
//! does not yet implement full drag/drop plumbing, but providing spec-shaped stubs avoids hard
//! crashes in user scripts.
//!
//! This module implements:
//! - `new DataTransfer()`
//! - `DataTransfer.prototype.getData`/`setData`/`clearData`
//! - `DataTransfer.prototype.items` (stable per-instance object with `{ length, add, remove, clear }`)
//! - `DataTransfer.prototype.files` (stable per-instance, currently-empty Array)
//!
//! The implementation intentionally keeps all user-controlled data in the JS heap (not in Rust
//! allocations) so it remains bounded by the configured `vm-js` heap limits.

use vm_js::{
  GcObject, GcString, Heap, HostSlots, NativeConstructId, NativeFunctionId, PropertyDescriptor,
  PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
};

// Brand `DataTransfer` as a platform object so `structuredClone()` rejects it with `DataCloneError`.
const DATA_TRANSFER_HOST_TAG: u64 = 0x4441_5441_5846_4552; // "DATAXFER"
const DATA_TRANSFER_ITEMS_HOST_TAG: u64 = 0x4454_4954_454D_4C53; // "DTITEMLS"

const INTERNAL_DATA_KEY: &str = "__fastrender_data_transfer_data";
const INTERNAL_TYPES_KEY: &str = "__fastrender_data_transfer_types";
const INTERNAL_ITEMS_CACHE_KEY: &str = "__fastrender_data_transfer_items";
const INTERNAL_FILES_CACHE_KEY: &str = "__fastrender_data_transfer_files";
const INTERNAL_ITEMS_OWNER_KEY: &str = "__fastrender_data_transfer_items_owner";
// Host-only cache: a non-configurable, non-writable reference to the installed `DataTransfer.prototype`.
//
// This lets embedding code create `DataTransfer` instances without reading the (mutable) global
// `DataTransfer` constructor (which hostile scripts can overwrite).
const INTERNAL_PROTO_CACHE_KEY: &str = "__fastrender_data_transfer_proto";

// Native slot indices for the `items` getter.
const ITEMS_GETTER_PROTO_SLOT: usize = 0;

fn data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data { value, writable },
  }
}

fn accessor_desc(get: Value, set: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Accessor { get, set },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn get_own_data_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Result<Value, VmError> {
  // Root the object while allocating the property key: `alloc_key` can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  let key = alloc_key(&mut scope, name)?;
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

fn require_data_transfer(scope: &Scope<'_>, this: Value) -> Result<GcObject, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let Some(slots) = scope.heap().object_host_slots(obj)? else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  if slots.a != DATA_TRANSFER_HOST_TAG {
    return Err(VmError::TypeError("Illegal invocation"));
  }
  Ok(obj)
}

fn require_items_object(scope: &Scope<'_>, this: Value) -> Result<GcObject, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let Some(slots) = scope.heap().object_host_slots(obj)? else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  if slots.a != DATA_TRANSFER_ITEMS_HOST_TAG {
    return Err(VmError::TypeError("Illegal invocation"));
  }
  Ok(obj)
}

fn items_proto_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots
    .get(ITEMS_GETTER_PROTO_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::InvariantViolation(
      "DataTransfer.items getter missing required items prototype slot",
    )),
  }
}

fn array_length(scope: &mut Scope<'_>, arr: GcObject) -> Result<usize, VmError> {
  let len = get_own_data_prop(scope, arr, "length")?;
  match len {
    Value::Number(n) if n.is_finite() && n >= 0.0 => Ok(n.trunc() as usize),
    _ => Ok(0),
  }
}

fn gc_string_eq(heap: &Heap, a: GcString, b: GcString) -> Result<bool, VmError> {
  if a == b {
    return Ok(true);
  }
  Ok(heap.get_string(a)?.as_code_units() == heap.get_string(b)?.as_code_units())
}

fn types_array_for_transfer(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  transfer: GcObject,
) -> Result<GcObject, VmError> {
  let existing = get_own_data_prop(scope, transfer, INTERNAL_TYPES_KEY)?;
  if let Value::Object(obj) = existing {
    return Ok(obj);
  }

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let arr = scope.alloc_array(0)?;
  scope.push_root(Value::Object(arr))?;
  scope
    .heap_mut()
    .object_set_prototype(arr, Some(intr.array_prototype()))?;
  set_own_data_prop(
    scope,
    transfer,
    INTERNAL_TYPES_KEY,
    Value::Object(arr),
    /* writable */ true,
  )?;
  Ok(arr)
}

fn data_obj_for_transfer(scope: &mut Scope<'_>, transfer: GcObject) -> Result<GcObject, VmError> {
  let existing = get_own_data_prop(scope, transfer, INTERNAL_DATA_KEY)?;
  if let Value::Object(obj) = existing {
    return Ok(obj);
  }

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  set_own_data_prop(
    scope,
    transfer,
    INTERNAL_DATA_KEY,
    Value::Object(obj),
    /* writable */ true,
  )?;
  Ok(obj)
}

fn data_transfer_set_data_strings(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  transfer: GcObject,
  format_s: GcString,
  data_s: GcString,
) -> Result<(), VmError> {
  // Root values across any allocations performed while mutating internal state.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(transfer))?;
  scope.push_root(Value::String(format_s))?;
  scope.push_root(Value::String(data_s))?;

  let data_obj = data_obj_for_transfer(&mut scope, transfer)?;
  scope.push_root(Value::Object(data_obj))?;
  let types_arr = types_array_for_transfer(vm, &mut scope, transfer)?;
  scope.push_root(Value::Object(types_arr))?;

  let format_key = PropertyKey::from_string(format_s);
  let existed = scope
    .heap()
    .object_get_own_data_property_value(data_obj, &format_key)?
    .is_some();

  scope.define_property(
    data_obj,
    format_key,
    data_desc(Value::String(data_s), true),
  )?;

  if !existed {
    let len = array_length(&mut scope, types_arr)?;
    let key = alloc_key(&mut scope, &len.to_string())?;
    scope.define_property(
      types_arr,
      key,
      data_desc(Value::String(format_s), true),
    )?;
  }

  Ok(())
}

fn data_transfer_remove_by_index(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  transfer: GcObject,
  index: usize,
) -> Result<(), VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(transfer))?;

  let data_obj = data_obj_for_transfer(&mut scope, transfer)?;
  scope.push_root(Value::Object(data_obj))?;
  let types_arr = types_array_for_transfer(vm, &mut scope, transfer)?;
  scope.push_root(Value::Object(types_arr))?;

  let len = array_length(&mut scope, types_arr)?;
  if index >= len {
    return Ok(());
  }

  // Fetch the type string at the requested index.
  let index_key = alloc_key(&mut scope, &index.to_string())?;
  let Value::String(type_s) = scope
    .heap()
    .object_get_own_data_property_value(types_arr, &index_key)?
    .unwrap_or(Value::Undefined)
  else {
    return Ok(());
  };

  // Remove from the data map.
  let format_key = PropertyKey::from_string(type_s);
  let _ = scope.heap_mut().ordinary_delete(data_obj, format_key)?;

  // Rebuild the types array without the removed entry to keep indices deterministic.
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let new_arr = scope.alloc_array(len.saturating_sub(1))?;
  scope.push_root(Value::Object(new_arr))?;
  scope
    .heap_mut()
    .object_set_prototype(new_arr, Some(intr.array_prototype()))?;

  let mut out_idx = 0usize;
  for i in 0..len {
    if i == index {
      continue;
    }
    let key = alloc_key(&mut scope, &i.to_string())?;
    let Value::String(s) = scope
      .heap()
      .object_get_own_data_property_value(types_arr, &key)?
      .unwrap_or(Value::Undefined)
    else {
      continue;
    };
    scope.push_root(Value::String(s))?;
    let out_key = alloc_key(&mut scope, &out_idx.to_string())?;
    scope.define_property(new_arr, out_key, data_desc(Value::String(s), true))?;
    out_idx += 1;
  }

  set_own_data_prop(
    &mut scope,
    transfer,
    INTERNAL_TYPES_KEY,
    Value::Object(new_arr),
    /* writable */ true,
  )?;

  Ok(())
}

fn data_transfer_clear_all(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  transfer: GcObject,
) -> Result<(), VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(transfer))?;
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  let data_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(data_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(data_obj, Some(intr.object_prototype()))?;
  set_own_data_prop(
    &mut scope,
    transfer,
    INTERNAL_DATA_KEY,
    Value::Object(data_obj),
    /* writable */ true,
  )?;

  let types_arr = scope.alloc_array(0)?;
  scope.push_root(Value::Object(types_arr))?;
  scope
    .heap_mut()
    .object_set_prototype(types_arr, Some(intr.array_prototype()))?;
  set_own_data_prop(
    &mut scope,
    transfer,
    INTERNAL_TYPES_KEY,
    Value::Object(types_arr),
    /* writable */ true,
  )?;

  Ok(())
}

fn data_transfer_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "DataTransfer constructor cannot be invoked without 'new'",
  ))
}

fn data_transfer_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "DataTransfer requires intrinsics (create a Realm first)",
  ))?;

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(callee))?;

  let proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(proto) => proto,
      _ => intr.object_prototype(),
    }
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: DATA_TRANSFER_HOST_TAG,
      b: 0,
    },
  )?;

  // Internal state (data + insertion-ordered type list).
  let data_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(data_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(data_obj, Some(intr.object_prototype()))?;
  set_own_data_prop(
    &mut scope,
    obj,
    INTERNAL_DATA_KEY,
    Value::Object(data_obj),
    /* writable */ true,
  )?;

  let types_arr = scope.alloc_array(0)?;
  scope.push_root(Value::Object(types_arr))?;
  scope
    .heap_mut()
    .object_set_prototype(types_arr, Some(intr.array_prototype()))?;
  set_own_data_prop(
    &mut scope,
    obj,
    INTERNAL_TYPES_KEY,
    Value::Object(types_arr),
    /* writable */ true,
  )?;

  // Reasonable defaults for these mutable string attributes (not spec-complete, but common).
  let drop_effect_s = scope.alloc_string("none")?;
  scope.push_root(Value::String(drop_effect_s))?;
  let drop_key = alloc_key(&mut scope, "dropEffect")?;
  scope.define_property(
    obj,
    drop_key,
    data_desc(Value::String(drop_effect_s), true),
  )?;

  let effect_allowed_s = scope.alloc_string("all")?;
  scope.push_root(Value::String(effect_allowed_s))?;
  let effect_key = alloc_key(&mut scope, "effectAllowed")?;
  scope.define_property(
    obj,
    effect_key,
    data_desc(Value::String(effect_allowed_s), true),
  )?;

  Ok(Value::Object(obj))
}

fn data_transfer_get_data_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let transfer = require_data_transfer(scope, this)?;

  let format_v = args.get(0).copied().unwrap_or(Value::Undefined);
  let format_s = scope.to_string(vm, host, hooks, format_v)?;

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(transfer))?;
  scope.push_root(Value::String(format_s))?;

  let data_obj = data_obj_for_transfer(&mut scope, transfer)?;
  let key = PropertyKey::from_string(format_s);
  let value = scope
    .heap()
    .object_get_own_data_property_value(data_obj, &key)?
    .unwrap_or(Value::Undefined);

  if let Value::String(s) = value {
    return Ok(Value::String(s));
  }

  let empty = scope.alloc_string("")?;
  Ok(Value::String(empty))
}

fn data_transfer_set_data_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let transfer = require_data_transfer(scope, this)?;

  let format_v = args.get(0).copied().unwrap_or(Value::Undefined);
  let data_v = args.get(1).copied().unwrap_or(Value::Undefined);
  let format_s = scope.to_string(vm, host, hooks, format_v)?;
  let data_s = scope.to_string(vm, host, hooks, data_v)?;

  data_transfer_set_data_strings(vm, scope, transfer, format_s, data_s)?;
  Ok(Value::Undefined)
}

fn data_transfer_clear_data_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let transfer = require_data_transfer(scope, this)?;

  let format_v = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(format_v, Value::Undefined) {
    data_transfer_clear_all(vm, scope, transfer)?;
    return Ok(Value::Undefined);
  }

  let format_s = scope.to_string(vm, host, hooks, format_v)?;
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(transfer))?;
  scope.push_root(Value::String(format_s))?;

  let types_arr = types_array_for_transfer(vm, &mut scope, transfer)?;
  let len = array_length(&mut scope, types_arr)?;
  let mut found: Option<usize> = None;
  for i in 0..len {
    let key = alloc_key(&mut scope, &i.to_string())?;
    let Value::String(entry) = scope
      .heap()
      .object_get_own_data_property_value(types_arr, &key)?
      .unwrap_or(Value::Undefined)
    else {
      continue;
    };
    if gc_string_eq(scope.heap(), entry, format_s)? {
      found = Some(i);
      break;
    }
  }

  if let Some(idx) = found {
    data_transfer_remove_by_index(vm, &mut scope, transfer, idx)?;
    return Ok(Value::Undefined);
  }

  // Fallback: delete from the data map even if it wasn't present in the types list.
  let data_obj = data_obj_for_transfer(&mut scope, transfer)?;
  let format_key = PropertyKey::from_string(format_s);
  let _ = scope.heap_mut().ordinary_delete(data_obj, format_key)?;

  Ok(Value::Undefined)
}

fn data_transfer_set_drag_image_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Drag image rendering is not supported in the headless renderer; keep this as a no-op so
  // libraries can call it without crashing.
  Ok(Value::Undefined)
}

fn data_transfer_types_getter_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let transfer = require_data_transfer(scope, this)?;

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  let types_arr = types_array_for_transfer(vm, scope, transfer)?;
  let len = array_length(scope, types_arr)?;

  let out = scope.alloc_array(len)?;
  scope.push_root(Value::Object(out))?;
  scope
    .heap_mut()
    .object_set_prototype(out, Some(intr.array_prototype()))?;

  for i in 0..len {
    let key = alloc_key(scope, &i.to_string())?;
    let Some(Value::String(s)) = scope.heap().object_get_own_data_property_value(types_arr, &key)?
    else {
      continue;
    };
    scope.push_root(Value::String(s))?;
    let out_key = alloc_key(scope, &i.to_string())?;
    scope.define_property(out, out_key, data_desc(Value::String(s), true))?;
  }

  Ok(Value::Object(out))
}

fn data_transfer_items_getter_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let transfer = require_data_transfer(scope, this)?;

  let existing = get_own_data_prop(scope, transfer, INTERNAL_ITEMS_CACHE_KEY)?;
  if let Value::Object(obj) = existing {
    return Ok(Value::Object(obj));
  }

  let items_proto = items_proto_from_callee(scope, callee)?;
  let items_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(items_obj))?;
  scope
    .heap_mut()
    .object_set_prototype(items_obj, Some(items_proto))?;
  scope.heap_mut().object_set_host_slots(
    items_obj,
    HostSlots {
      a: DATA_TRANSFER_ITEMS_HOST_TAG,
      b: 0,
    },
  )?;

  // Link back to the owning DataTransfer so item list methods can mutate it.
  set_own_data_prop(
    scope,
    items_obj,
    INTERNAL_ITEMS_OWNER_KEY,
    Value::Object(transfer),
    /* writable */ false,
  )?;

  set_own_data_prop(
    scope,
    transfer,
    INTERNAL_ITEMS_CACHE_KEY,
    Value::Object(items_obj),
    /* writable */ false,
  )?;

  Ok(Value::Object(items_obj))
}

fn data_transfer_files_getter_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let transfer = require_data_transfer(scope, this)?;

  let existing = get_own_data_prop(scope, transfer, INTERNAL_FILES_CACHE_KEY)?;
  if let Value::Object(obj) = existing {
    return Ok(Value::Object(obj));
  }

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let arr = scope.alloc_array(0)?;
  scope.push_root(Value::Object(arr))?;
  scope
    .heap_mut()
    .object_set_prototype(arr, Some(intr.array_prototype()))?;

  set_own_data_prop(
    scope,
    transfer,
    INTERNAL_FILES_CACHE_KEY,
    Value::Object(arr),
    /* writable */ false,
  )?;

  Ok(Value::Object(arr))
}

fn items_owner_from_items(scope: &mut Scope<'_>, items_obj: GcObject) -> Result<GcObject, VmError> {
  let owner = get_own_data_prop(scope, items_obj, INTERNAL_ITEMS_OWNER_KEY)?;
  let Value::Object(obj) = owner else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  Ok(obj)
}

fn items_length_getter_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let items_obj = require_items_object(scope, this)?;
  let transfer = items_owner_from_items(scope, items_obj)?;

  let types_arr = types_array_for_transfer(vm, scope, transfer)?;
  let len = array_length(scope, types_arr)?;
  Ok(Value::Number(len as f64))
}

fn items_add_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let items_obj = require_items_object(scope, this)?;
  let transfer = items_owner_from_items(scope, items_obj)?;

  // Overload: add(data, type)
  let data_v = args.get(0).copied().unwrap_or(Value::Undefined);
  let type_v = args.get(1).copied().unwrap_or(Value::Undefined);
  if matches!(type_v, Value::Undefined) {
    // `add(File)` is not supported yet.
    return Ok(Value::Undefined);
  }

  let data_s = scope.to_string(vm, host, hooks, data_v)?;
  let type_s = scope.to_string(vm, host, hooks, type_v)?;
  data_transfer_set_data_strings(vm, scope, transfer, type_s, data_s)?;

  // Return a tiny DataTransferItem-like object for compatibility.
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let item = scope.alloc_object()?;
  scope.push_root(Value::Object(item))?;
  scope
    .heap_mut()
    .object_set_prototype(item, Some(intr.object_prototype()))?;

  let kind_s = scope.alloc_string("string")?;
  scope.push_root(Value::String(kind_s))?;
  let kind_key = alloc_key(scope, "kind")?;
  scope.define_property(item, kind_key, data_desc(Value::String(kind_s), false))?;

  scope.push_root(Value::String(type_s))?;
  let type_key = alloc_key(scope, "type")?;
  scope.define_property(item, type_key, data_desc(Value::String(type_s), false))?;

  Ok(Value::Object(item))
}

fn items_remove_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let items_obj = require_items_object(scope, this)?;
  let transfer = items_owner_from_items(scope, items_obj)?;

  let idx_v = args.get(0).copied().unwrap_or(Value::Undefined);
  let n = scope.to_number(vm, host, hooks, idx_v)?;
  if !n.is_finite() || n < 0.0 {
    return Ok(Value::Undefined);
  }
  let index = n.trunc() as usize;
  data_transfer_remove_by_index(vm, scope, transfer, index)?;
  Ok(Value::Undefined)
}

fn items_clear_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let items_obj = require_items_object(scope, this)?;
  let transfer = items_owner_from_items(scope, items_obj)?;
  data_transfer_clear_all(vm, scope, transfer)?;
  Ok(Value::Undefined)
}

pub fn install_window_data_transfer_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
) -> Result<(), VmError> {
  let intr = realm.intrinsics();
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // ---------------------------------------------------------------------------
  // DataTransferItemList prototype (internal; returned by DataTransfer.items)
  // ---------------------------------------------------------------------------
  let items_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(items_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(items_proto, Some(intr.object_prototype()))?;

  // @@toStringTag for `Object.prototype.toString.call(dt.items)`.
  let tag_value = scope.alloc_string("DataTransferItemList")?;
  scope.push_root(Value::String(tag_value))?;
  scope.define_property(
    items_proto,
    PropertyKey::from_symbol(intr.well_known_symbols().to_string_tag),
    data_desc(Value::String(tag_value), false),
  )?;

  // length (getter only)
  let length_get_id: NativeFunctionId = vm.register_native_call(items_length_getter_native)?;
  let length_name = scope.alloc_string("get length")?;
  scope.push_root(Value::String(length_name))?;
  let length_get = scope.alloc_native_function(length_get_id, None, length_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(length_get, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(length_get))?;
  let length_key = alloc_key(&mut scope, "length")?;
  scope.define_property(
    items_proto,
    length_key,
    accessor_desc(Value::Object(length_get), Value::Undefined),
  )?;

  let add_id: NativeFunctionId = vm.register_native_call(items_add_native)?;
  let add_name = scope.alloc_string("add")?;
  scope.push_root(Value::String(add_name))?;
  let add_fn = scope.alloc_native_function(add_id, None, add_name, 2)?;
  scope
    .heap_mut()
    .object_set_prototype(add_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(add_fn))?;
  let add_key = alloc_key(&mut scope, "add")?;
  scope.define_property(items_proto, add_key, data_desc(Value::Object(add_fn), true))?;

  let remove_id: NativeFunctionId = vm.register_native_call(items_remove_native)?;
  let remove_name = scope.alloc_string("remove")?;
  scope.push_root(Value::String(remove_name))?;
  let remove_fn = scope.alloc_native_function(remove_id, None, remove_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(remove_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(remove_fn))?;
  let remove_key = alloc_key(&mut scope, "remove")?;
  scope.define_property(
    items_proto,
    remove_key,
    data_desc(Value::Object(remove_fn), true),
  )?;

  let clear_id: NativeFunctionId = vm.register_native_call(items_clear_native)?;
  let clear_name = scope.alloc_string("clear")?;
  scope.push_root(Value::String(clear_name))?;
  let clear_fn = scope.alloc_native_function(clear_id, None, clear_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(clear_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(clear_fn))?;
  let clear_key = alloc_key(&mut scope, "clear")?;
  scope.define_property(
    items_proto,
    clear_key,
    data_desc(Value::Object(clear_fn), true),
  )?;

  // ---------------------------------------------------------------------------
  // DataTransfer constructor + prototype
  // ---------------------------------------------------------------------------
  let call_id: NativeFunctionId = vm.register_native_call(data_transfer_ctor_call)?;
  let construct_id: NativeConstructId = vm.register_native_construct(data_transfer_ctor_construct)?;

  let name = scope.alloc_string("DataTransfer")?;
  scope.push_root(Value::String(name))?;
  let ctor = scope.alloc_native_function_with_slots(call_id, Some(construct_id), name, 0, &[])?;
  scope.push_root(Value::Object(ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(ctor, Some(intr.function_prototype()))?;

  let proto = {
    let key = alloc_key(&mut scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(ctor, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "DataTransfer constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(proto))?;
  scope
    .heap_mut()
    .object_set_prototype(proto, Some(intr.object_prototype()))?;

  let dt_tag = scope.alloc_string("DataTransfer")?;
  scope.push_root(Value::String(dt_tag))?;
  scope.define_property(
    proto,
    PropertyKey::from_symbol(intr.well_known_symbols().to_string_tag),
    data_desc(Value::String(dt_tag), false),
  )?;

  // getData/setData/clearData
  let get_id: NativeFunctionId = vm.register_native_call(data_transfer_get_data_native)?;
  let get_name = scope.alloc_string("getData")?;
  scope.push_root(Value::String(get_name))?;
  let get_fn = scope.alloc_native_function(get_id, None, get_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(get_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(get_fn))?;
  let get_key = alloc_key(&mut scope, "getData")?;
  scope.define_property(proto, get_key, data_desc(Value::Object(get_fn), true))?;

  let set_id: NativeFunctionId = vm.register_native_call(data_transfer_set_data_native)?;
  let set_name = scope.alloc_string("setData")?;
  scope.push_root(Value::String(set_name))?;
  let set_fn = scope.alloc_native_function(set_id, None, set_name, 2)?;
  scope
    .heap_mut()
    .object_set_prototype(set_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(set_fn))?;
  let set_key = alloc_key(&mut scope, "setData")?;
  scope.define_property(proto, set_key, data_desc(Value::Object(set_fn), true))?;

  let clear_dt_id: NativeFunctionId = vm.register_native_call(data_transfer_clear_data_native)?;
  let clear_dt_name = scope.alloc_string("clearData")?;
  scope.push_root(Value::String(clear_dt_name))?;
  let clear_dt_fn = scope.alloc_native_function(clear_dt_id, None, clear_dt_name, 1)?;
  scope.heap_mut().object_set_prototype(
    clear_dt_fn,
    Some(intr.function_prototype()),
  )?;
  scope.push_root(Value::Object(clear_dt_fn))?;
  let clear_dt_key = alloc_key(&mut scope, "clearData")?;
  scope.define_property(
    proto,
    clear_dt_key,
    data_desc(Value::Object(clear_dt_fn), true),
  )?;

  let set_drag_image_id: NativeFunctionId =
    vm.register_native_call(data_transfer_set_drag_image_native)?;
  let set_drag_image_name = scope.alloc_string("setDragImage")?;
  scope.push_root(Value::String(set_drag_image_name))?;
  let set_drag_image_fn =
    scope.alloc_native_function(set_drag_image_id, None, set_drag_image_name, 3)?;
  scope.heap_mut().object_set_prototype(
    set_drag_image_fn,
    Some(intr.function_prototype()),
  )?;
  scope.push_root(Value::Object(set_drag_image_fn))?;
  let set_drag_image_key = alloc_key(&mut scope, "setDragImage")?;
  scope.define_property(
    proto,
    set_drag_image_key,
    data_desc(Value::Object(set_drag_image_fn), true),
  )?;

  // items/files/types accessors.
  let items_get_id: NativeFunctionId = vm.register_native_call(data_transfer_items_getter_native)?;
  let items_get_name = scope.alloc_string("get items")?;
  scope.push_root(Value::String(items_get_name))?;
  let items_get_fn = scope.alloc_native_function_with_slots(
    items_get_id,
    None,
    items_get_name,
    0,
    &[Value::Object(items_proto)],
  )?;
  scope.heap_mut().object_set_prototype(
    items_get_fn,
    Some(intr.function_prototype()),
  )?;
  scope.push_root(Value::Object(items_get_fn))?;
  let items_key = alloc_key(&mut scope, "items")?;
  scope.define_property(
    proto,
    items_key,
    accessor_desc(Value::Object(items_get_fn), Value::Undefined),
  )?;

  let files_get_id: NativeFunctionId = vm.register_native_call(data_transfer_files_getter_native)?;
  let files_get_name = scope.alloc_string("get files")?;
  scope.push_root(Value::String(files_get_name))?;
  let files_get_fn = scope.alloc_native_function(files_get_id, None, files_get_name, 0)?;
  scope.heap_mut().object_set_prototype(
    files_get_fn,
    Some(intr.function_prototype()),
  )?;
  scope.push_root(Value::Object(files_get_fn))?;
  let files_key = alloc_key(&mut scope, "files")?;
  scope.define_property(
    proto,
    files_key,
    accessor_desc(Value::Object(files_get_fn), Value::Undefined),
  )?;

  let types_get_id: NativeFunctionId = vm.register_native_call(data_transfer_types_getter_native)?;
  let types_get_name = scope.alloc_string("get types")?;
  scope.push_root(Value::String(types_get_name))?;
  let types_get_fn = scope.alloc_native_function(types_get_id, None, types_get_name, 0)?;
  scope.heap_mut().object_set_prototype(
    types_get_fn,
    Some(intr.function_prototype()),
  )?;
  scope.push_root(Value::Object(types_get_fn))?;
  let types_key = alloc_key(&mut scope, "types")?;
  scope.define_property(
    proto,
    types_key,
    accessor_desc(Value::Object(types_get_fn), Value::Undefined),
  )?;

  let ctor_key = alloc_key(&mut scope, "DataTransfer")?;
  scope.define_property(global, ctor_key, data_desc(Value::Object(ctor), true))?;

  // Cache the prototype for host-only creation paths. Keep it non-configurable/non-writable so JS
  // cannot replace it (it may still mutate the object itself, like `DataTransfer.prototype`).
  let proto_cache_key = alloc_key(&mut scope, INTERNAL_PROTO_CACHE_KEY)?;
  scope.define_property(
    global,
    proto_cache_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Object(proto),
        writable: false,
      },
    },
  )?;

  Ok(())
}

/// Creates a new `DataTransfer` instance and initializes `text/plain`.
///
/// This is intended for host integrations (drag-and-drop, clipboard-like flows) that need a
/// `DataTransfer` payload without executing arbitrary JS (e.g. without calling overridden globals).
pub fn create_data_transfer_with_text_plain(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  text_plain: &str,
) -> Result<GcObject, VmError> {
  let mut scope = heap.scope();

  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let proto_value = get_own_data_prop(&mut scope, global, INTERNAL_PROTO_CACHE_KEY)?;
  let Value::Object(proto) = proto_value else {
    return Err(VmError::InvariantViolation(
      "DataTransfer bindings missing internal prototype cache (install_window_data_transfer_bindings must run first)",
    ));
  };

  // Allocate the JS wrapper object and brand it so `DataTransfer.prototype` methods can
  // brand-check it.
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: DATA_TRANSFER_HOST_TAG,
      b: 0,
    },
  )?;

  // Reasonable defaults for these mutable string attributes.
  let drop_effect_s = scope.alloc_string("none")?;
  scope.push_root(Value::String(drop_effect_s))?;
  let drop_key = alloc_key(&mut scope, "dropEffect")?;
  scope.define_property(
    obj,
    drop_key,
    data_desc(Value::String(drop_effect_s), true),
  )?;

  let effect_allowed_s = scope.alloc_string("all")?;
  scope.push_root(Value::String(effect_allowed_s))?;
  let effect_key = alloc_key(&mut scope, "effectAllowed")?;
  scope.define_property(
    obj,
    effect_key,
    data_desc(Value::String(effect_allowed_s), true),
  )?;

  // Seed `text/plain`.
  let type_s = scope.alloc_string("text/plain")?;
  scope.push_root(Value::String(type_s))?;
  let data_s = scope.alloc_string(text_plain)?;
  scope.push_root(Value::String(data_s))?;
  data_transfer_set_data_strings(vm, &mut scope, obj, type_s, data_s)?;

  Ok(obj)
}
