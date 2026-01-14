//! Minimal `DOMRectReadOnly` / `DOMRect` bindings for `vm-js` Window realms.
//!
//! FastRender uses these for higher-fidelity DOM geometry APIs (e.g. observer entries and future
//! `Element.getBoundingClientRect()` support). The implementation is intentionally small and
//! deterministic:
//! - numeric fields are stored as hidden (non-enumerable) own data properties, and
//! - derived getters (`top`/`right`/`bottom`/`left`) are computed in Rust.

use vm_js::{
  Intrinsics, NativeConstructId, NativeFunctionId, PropertyDescriptor, PropertyKey, PropertyKind, Realm,
  Scope, Value, Vm, VmError, VmHost, VmHostHooks,
};

const INTERNAL_X_KEY: &str = "__fastrender_domrect_x";
const INTERNAL_Y_KEY: &str = "__fastrender_domrect_y";
const INTERNAL_WIDTH_KEY: &str = "__fastrender_domrect_width";
const INTERNAL_HEIGHT_KEY: &str = "__fastrender_domrect_height";

// Cached prototype objects stored on the realm global object so Rust can allocate DOMRect instances
// without going through JS construction paths.
const INTERNAL_DOMRECT_RO_PROTO_KEY: &str = "__fastrender_domrect_read_only_proto";
const INTERNAL_DOMRECT_PROTO_KEY: &str = "__fastrender_domrect_proto";

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

fn accessor_desc(get: Value, set: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Accessor { get, set },
  }
}

fn json_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn require_intrinsics(vm: &Vm) -> Result<Intrinsics, VmError> {
  vm.intrinsics().ok_or(VmError::Unimplemented(
    "DOMRect* requires intrinsics (create a Realm first)",
  ))
}

fn internal_number(scope: &mut Scope<'_>, obj: vm_js::GcObject, name: &str) -> Result<f64, VmError> {
  // Root `obj` while allocating the property key (`alloc_key` can trigger GC).
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  let key = alloc_key(&mut scope, name)?;
  match scope
    .heap()
    .object_get_own_data_property_value(obj, &key)?
  {
    Some(Value::Number(n)) => Ok(n),
    _ => Err(VmError::TypeError("Illegal invocation")),
  }
}

fn set_internal_number(
  scope: &mut Scope<'_>,
  obj: vm_js::GcObject,
  name: &str,
  value: f64,
) -> Result<(), VmError> {
  // Root `obj` while allocating the property key (`alloc_key` can trigger GC).
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, data_desc(Value::Number(value)))?;
  Ok(())
}

fn to_number_or_zero(heap: &mut vm_js::Heap, value: Option<Value>) -> Result<f64, VmError> {
  match value {
    Some(v) => heap.to_number(v),
    None => Ok(0.0),
  }
}

fn alloc_dom_rect_with_proto(
  scope: &mut Scope<'_>,
  proto: vm_js::GcObject,
  x: f64,
  y: f64,
  width: f64,
  height: f64,
) -> Result<vm_js::GcObject, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(proto))?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;

  set_internal_number(&mut scope, obj, INTERNAL_X_KEY, x)?;
  set_internal_number(&mut scope, obj, INTERNAL_Y_KEY, y)?;
  set_internal_number(&mut scope, obj, INTERNAL_WIDTH_KEY, width)?;
  set_internal_number(&mut scope, obj, INTERNAL_HEIGHT_KEY, height)?;

  Ok(obj)
}

fn dom_rect_ro_proto_from_global(
  scope: &mut Scope<'_>,
  global: vm_js::GcObject,
) -> Result<vm_js::GcObject, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(global))?;
  let key = alloc_key(&mut scope, INTERNAL_DOMRECT_RO_PROTO_KEY)?;
  match scope
    .heap()
    .object_get_own_data_property_value(global, &key)?
  {
    Some(Value::Object(proto)) => Ok(proto),
    _ => Err(VmError::InvariantViolation(
      "DOMRectReadOnly bindings missing cached prototype on global object",
    )),
  }
}

fn dom_rect_proto_from_global(scope: &mut Scope<'_>, global: vm_js::GcObject) -> Result<vm_js::GcObject, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(global))?;
  let key = alloc_key(&mut scope, INTERNAL_DOMRECT_PROTO_KEY)?;
  match scope
    .heap()
    .object_get_own_data_property_value(global, &key)?
  {
    Some(Value::Object(proto)) => Ok(proto),
    _ => Err(VmError::InvariantViolation(
      "DOMRect bindings missing cached prototype on global object",
    )),
  }
}

fn dom_rect_ro_proto_from_realm(scope: &mut Scope<'_>, realm: &Realm) -> Result<vm_js::GcObject, VmError> {
  dom_rect_ro_proto_from_global(scope, realm.global_object())
}

fn dom_rect_proto_from_realm(scope: &mut Scope<'_>, realm: &Realm) -> Result<vm_js::GcObject, VmError> {
  dom_rect_proto_from_global(scope, realm.global_object())
}

#[allow(dead_code)]
pub(crate) fn alloc_dom_rect_read_only_from_global(
  scope: &mut Scope<'_>,
  global: vm_js::GcObject,
  x: f64,
  y: f64,
  width: f64,
  height: f64,
) -> Result<vm_js::GcObject, VmError> {
  let proto = dom_rect_ro_proto_from_global(scope, global)?;
  alloc_dom_rect_with_proto(scope, proto, x, y, width, height)
}

#[allow(dead_code)]
pub(crate) fn alloc_dom_rect_from_global(
  scope: &mut Scope<'_>,
  global: vm_js::GcObject,
  x: f64,
  y: f64,
  width: f64,
  height: f64,
) -> Result<vm_js::GcObject, VmError> {
  let proto = dom_rect_proto_from_global(scope, global)?;
  alloc_dom_rect_with_proto(scope, proto, x, y, width, height)
}

#[allow(dead_code)]
pub(crate) fn alloc_dom_rect_read_only(
  scope: &mut Scope<'_>,
  realm: &Realm,
  x: f64,
  y: f64,
  width: f64,
  height: f64,
) -> Result<vm_js::GcObject, VmError> {
  alloc_dom_rect_read_only_from_global(scope, realm.global_object(), x, y, width, height)
}

#[allow(dead_code)]
pub(crate) fn alloc_dom_rect(
  scope: &mut Scope<'_>,
  realm: &Realm,
  x: f64,
  y: f64,
  width: f64,
  height: f64,
) -> Result<vm_js::GcObject, VmError> {
  alloc_dom_rect_from_global(scope, realm.global_object(), x, y, width, height)
}

fn require_dom_rect_obj(scope: &mut Scope<'_>, this: Value) -> Result<vm_js::GcObject, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  // Ensure the internal slots exist.
  let _ = internal_number(scope, obj, INTERNAL_X_KEY)?;
  let _ = internal_number(scope, obj, INTERNAL_Y_KEY)?;
  let _ = internal_number(scope, obj, INTERNAL_WIDTH_KEY)?;
  let _ = internal_number(scope, obj, INTERNAL_HEIGHT_KEY)?;
  Ok(obj)
}

fn dom_rect_ro_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "DOMRectReadOnly constructor cannot be invoked without 'new'",
  ))
}

fn dom_rect_ro_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(callee))?;

  let x = to_number_or_zero(scope.heap_mut(), args.get(0).copied())?;
  let y = to_number_or_zero(scope.heap_mut(), args.get(1).copied())?;
  let width = to_number_or_zero(scope.heap_mut(), args.get(2).copied())?;
  let height = to_number_or_zero(scope.heap_mut(), args.get(3).copied())?;

  let proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
    {
      Some(Value::Object(proto)) => proto,
      _ => intr.object_prototype(),
    }
  };

  let obj = alloc_dom_rect_with_proto(&mut scope, proto, x, y, width, height)?;
  Ok(Value::Object(obj))
}

fn dom_rect_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "DOMRect constructor cannot be invoked without 'new'",
  ))
}

fn dom_rect_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let intr = require_intrinsics(vm)?;

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(callee))?;

  let x = to_number_or_zero(scope.heap_mut(), args.get(0).copied())?;
  let y = to_number_or_zero(scope.heap_mut(), args.get(1).copied())?;
  let width = to_number_or_zero(scope.heap_mut(), args.get(2).copied())?;
  let height = to_number_or_zero(scope.heap_mut(), args.get(3).copied())?;

  let proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
    {
      Some(Value::Object(proto)) => proto,
      _ => intr.object_prototype(),
    }
  };

  let obj = alloc_dom_rect_with_proto(&mut scope, proto, x, y, width, height)?;
  Ok(Value::Object(obj))
}

fn dom_rect_get_x(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_dom_rect_obj(scope, this)?;
  Ok(Value::Number(internal_number(scope, obj, INTERNAL_X_KEY)?))
}

fn dom_rect_get_y(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_dom_rect_obj(scope, this)?;
  Ok(Value::Number(internal_number(scope, obj, INTERNAL_Y_KEY)?))
}

fn dom_rect_get_width(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_dom_rect_obj(scope, this)?;
  Ok(Value::Number(internal_number(scope, obj, INTERNAL_WIDTH_KEY)?))
}

fn dom_rect_get_height(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_dom_rect_obj(scope, this)?;
  Ok(Value::Number(internal_number(scope, obj, INTERNAL_HEIGHT_KEY)?))
}

fn dom_rect_get_top(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_dom_rect_obj(scope, this)?;
  Ok(Value::Number(internal_number(scope, obj, INTERNAL_Y_KEY)?))
}

fn dom_rect_get_left(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_dom_rect_obj(scope, this)?;
  Ok(Value::Number(internal_number(scope, obj, INTERNAL_X_KEY)?))
}

fn dom_rect_get_right(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_dom_rect_obj(scope, this)?;
  let x = internal_number(scope, obj, INTERNAL_X_KEY)?;
  let width = internal_number(scope, obj, INTERNAL_WIDTH_KEY)?;
  Ok(Value::Number(x + width))
}

fn dom_rect_get_bottom(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_dom_rect_obj(scope, this)?;
  let y = internal_number(scope, obj, INTERNAL_Y_KEY)?;
  let height = internal_number(scope, obj, INTERNAL_HEIGHT_KEY)?;
  Ok(Value::Number(y + height))
}

fn dom_rect_set_x(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_dom_rect_obj(scope, this)?;
  let v = to_number_or_zero(scope.heap_mut(), args.get(0).copied())?;
  set_internal_number(scope, obj, INTERNAL_X_KEY, v)?;
  Ok(Value::Undefined)
}

fn dom_rect_set_y(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_dom_rect_obj(scope, this)?;
  let v = to_number_or_zero(scope.heap_mut(), args.get(0).copied())?;
  set_internal_number(scope, obj, INTERNAL_Y_KEY, v)?;
  Ok(Value::Undefined)
}

fn dom_rect_set_width(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_dom_rect_obj(scope, this)?;
  let v = to_number_or_zero(scope.heap_mut(), args.get(0).copied())?;
  set_internal_number(scope, obj, INTERNAL_WIDTH_KEY, v)?;
  Ok(Value::Undefined)
}

fn dom_rect_set_height(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_dom_rect_obj(scope, this)?;
  let v = to_number_or_zero(scope.heap_mut(), args.get(0).copied())?;
  set_internal_number(scope, obj, INTERNAL_HEIGHT_KEY, v)?;
  Ok(Value::Undefined)
}

fn dom_rect_to_json(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let obj = require_dom_rect_obj(scope, this)?;

  let x = internal_number(scope, obj, INTERNAL_X_KEY)?;
  let y = internal_number(scope, obj, INTERNAL_Y_KEY)?;
  let width = internal_number(scope, obj, INTERNAL_WIDTH_KEY)?;
  let height = internal_number(scope, obj, INTERNAL_HEIGHT_KEY)?;
  let top = y;
  let left = x;
  let right = x + width;
  let bottom = y + height;

  let mut scope = scope.reborrow();
  let json = scope.alloc_object()?;
  scope.push_root(Value::Object(json))?;

  let entries = [
    ("x", x),
    ("y", y),
    ("width", width),
    ("height", height),
    ("top", top),
    ("right", right),
    ("bottom", bottom),
    ("left", left),
  ];
  for (name, value) in entries {
    let key = alloc_key(&mut scope, name)?;
    scope.define_property(json, key, json_data_desc(Value::Number(value)))?;
  }

  Ok(Value::Object(json))
}

pub(crate) fn install_window_dom_rect_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut vm_js::Heap,
) -> Result<(), VmError> {
  let intr = realm.intrinsics();
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // --- DOMRectReadOnly --------------------------------------------------------
  let ro_call_id: NativeFunctionId = vm.register_native_call(dom_rect_ro_call)?;
  let ro_construct_id: NativeConstructId = vm.register_native_construct(dom_rect_ro_construct)?;
  let ro_name_s = scope.alloc_string("DOMRectReadOnly")?;
  scope.push_root(Value::String(ro_name_s))?;
  let ro_ctor = scope.alloc_native_function(ro_call_id, Some(ro_construct_id), ro_name_s, 0)?;
  scope.push_root(Value::Object(ro_ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(ro_ctor, Some(intr.function_prototype()))?;

  let ro_proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(ro_ctor, &key)?
    {
      Some(Value::Object(obj)) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "DOMRectReadOnly constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(ro_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(ro_proto, Some(intr.object_prototype()))?;

  // Align with WebIDL/built-in constructor semantics:
  // - `DOMRectReadOnly.prototype` is non-writable and non-configurable.
  // - `DOMRectReadOnly.prototype.constructor` is non-writable and non-configurable.
  //
  // `vm-js` allocates native functions with normal JS `prototype` attributes (`writable: true`), so
  // tighten the descriptor after allocation.
  let ro_prototype_key = alloc_key(&mut scope, "prototype")?;
  scope.define_property(
    ro_ctor,
    ro_prototype_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Object(ro_proto),
        writable: false,
      },
    },
  )?;
  let ro_constructor_key = alloc_key(&mut scope, "constructor")?;
  scope.define_property(
    ro_proto,
    ro_constructor_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Object(ro_ctor),
        writable: false,
      },
    },
  )?;

  // Cache `DOMRectReadOnly.prototype` on the global for Rust allocation helpers.
  let ro_proto_key = alloc_key(&mut scope, INTERNAL_DOMRECT_RO_PROTO_KEY)?;
  scope.define_property(
    global,
    ro_proto_key,
    read_only_data_desc(Value::Object(ro_proto)),
  )?;

  // Shared getter functions for both DOMRectReadOnly and DOMRect.
  let get_x_id = vm.register_native_call(dom_rect_get_x)?;
  let get_x_name = scope.alloc_string("get x")?;
  scope.push_root(Value::String(get_x_name))?;
  let get_x_fn = scope.alloc_native_function(get_x_id, None, get_x_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(get_x_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(get_x_fn))?;

  let get_y_id = vm.register_native_call(dom_rect_get_y)?;
  let get_y_name = scope.alloc_string("get y")?;
  scope.push_root(Value::String(get_y_name))?;
  let get_y_fn = scope.alloc_native_function(get_y_id, None, get_y_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(get_y_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(get_y_fn))?;

  let get_width_id = vm.register_native_call(dom_rect_get_width)?;
  let get_width_name = scope.alloc_string("get width")?;
  scope.push_root(Value::String(get_width_name))?;
  let get_width_fn = scope.alloc_native_function(get_width_id, None, get_width_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(get_width_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(get_width_fn))?;

  let get_height_id = vm.register_native_call(dom_rect_get_height)?;
  let get_height_name = scope.alloc_string("get height")?;
  scope.push_root(Value::String(get_height_name))?;
  let get_height_fn = scope.alloc_native_function(get_height_id, None, get_height_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(get_height_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(get_height_fn))?;

  let get_top_id = vm.register_native_call(dom_rect_get_top)?;
  let get_top_name = scope.alloc_string("get top")?;
  scope.push_root(Value::String(get_top_name))?;
  let get_top_fn = scope.alloc_native_function(get_top_id, None, get_top_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(get_top_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(get_top_fn))?;

  let get_left_id = vm.register_native_call(dom_rect_get_left)?;
  let get_left_name = scope.alloc_string("get left")?;
  scope.push_root(Value::String(get_left_name))?;
  let get_left_fn = scope.alloc_native_function(get_left_id, None, get_left_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(get_left_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(get_left_fn))?;

  let get_right_id = vm.register_native_call(dom_rect_get_right)?;
  let get_right_name = scope.alloc_string("get right")?;
  scope.push_root(Value::String(get_right_name))?;
  let get_right_fn = scope.alloc_native_function(get_right_id, None, get_right_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(get_right_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(get_right_fn))?;

  let get_bottom_id = vm.register_native_call(dom_rect_get_bottom)?;
  let get_bottom_name = scope.alloc_string("get bottom")?;
  scope.push_root(Value::String(get_bottom_name))?;
  let get_bottom_fn = scope.alloc_native_function(get_bottom_id, None, get_bottom_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(get_bottom_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(get_bottom_fn))?;

  // DOMRectReadOnly accessors.
  let x_key = alloc_key(&mut scope, "x")?;
  scope.define_property(
    ro_proto,
    x_key,
    accessor_desc(Value::Object(get_x_fn), Value::Undefined),
  )?;
  let y_key = alloc_key(&mut scope, "y")?;
  scope.define_property(
    ro_proto,
    y_key,
    accessor_desc(Value::Object(get_y_fn), Value::Undefined),
  )?;
  let width_key = alloc_key(&mut scope, "width")?;
  scope.define_property(
    ro_proto,
    width_key,
    accessor_desc(Value::Object(get_width_fn), Value::Undefined),
  )?;
  let height_key = alloc_key(&mut scope, "height")?;
  scope.define_property(
    ro_proto,
    height_key,
    accessor_desc(Value::Object(get_height_fn), Value::Undefined),
  )?;
  let top_key = alloc_key(&mut scope, "top")?;
  scope.define_property(
    ro_proto,
    top_key,
    accessor_desc(Value::Object(get_top_fn), Value::Undefined),
  )?;
  let left_key = alloc_key(&mut scope, "left")?;
  scope.define_property(
    ro_proto,
    left_key,
    accessor_desc(Value::Object(get_left_fn), Value::Undefined),
  )?;
  let right_key = alloc_key(&mut scope, "right")?;
  scope.define_property(
    ro_proto,
    right_key,
    accessor_desc(Value::Object(get_right_fn), Value::Undefined),
  )?;
  let bottom_key = alloc_key(&mut scope, "bottom")?;
  scope.define_property(
    ro_proto,
    bottom_key,
    accessor_desc(Value::Object(get_bottom_fn), Value::Undefined),
  )?;

  // DOMRectReadOnly.prototype.toJSON
  let to_json_id = vm.register_native_call(dom_rect_to_json)?;
  let to_json_name = scope.alloc_string("toJSON")?;
  scope.push_root(Value::String(to_json_name))?;
  let to_json_fn = scope.alloc_native_function(to_json_id, None, to_json_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(to_json_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(to_json_fn))?;
  let to_json_key = alloc_key(&mut scope, "toJSON")?;
  scope.define_property(
    ro_proto,
    to_json_key,
    data_desc(Value::Object(to_json_fn)),
  )?;

  // Expose DOMRectReadOnly on the global.
  let ro_global_key = alloc_key(&mut scope, "DOMRectReadOnly")?;
  scope.define_property(
    global,
    ro_global_key,
    data_desc(Value::Object(ro_ctor)),
  )?;

  // --- DOMRect ---------------------------------------------------------------
  let rect_call_id: NativeFunctionId = vm.register_native_call(dom_rect_call)?;
  let rect_construct_id: NativeConstructId = vm.register_native_construct(dom_rect_construct)?;
  let rect_name_s = scope.alloc_string("DOMRect")?;
  scope.push_root(Value::String(rect_name_s))?;
  let rect_ctor =
    scope.alloc_native_function(rect_call_id, Some(rect_construct_id), rect_name_s, 0)?;
  scope.push_root(Value::Object(rect_ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(rect_ctor, Some(intr.function_prototype()))?;

  let rect_proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(rect_ctor, &key)?
    {
      Some(Value::Object(obj)) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "DOMRect constructor missing prototype object",
        ))
      }
    }
  };
  scope.push_root(Value::Object(rect_proto))?;
  // DOMRect.prototype inherits from DOMRectReadOnly.prototype.
  scope
    .heap_mut()
    .object_set_prototype(rect_proto, Some(ro_proto))?;

  // Align with WebIDL/built-in constructor semantics (see DOMRectReadOnly notes above).
  let rect_prototype_key = alloc_key(&mut scope, "prototype")?;
  scope.define_property(
    rect_ctor,
    rect_prototype_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Object(rect_proto),
        writable: false,
      },
    },
  )?;
  let rect_constructor_key = alloc_key(&mut scope, "constructor")?;
  scope.define_property(
    rect_proto,
    rect_constructor_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Object(rect_ctor),
        writable: false,
      },
    },
  )?;

  // Cache `DOMRect.prototype` on the global for Rust allocation helpers.
  let rect_proto_key = alloc_key(&mut scope, INTERNAL_DOMRECT_PROTO_KEY)?;
  scope.define_property(
    global,
    rect_proto_key,
    read_only_data_desc(Value::Object(rect_proto)),
  )?;

  // DOMRect setters.
  let set_x_id = vm.register_native_call(dom_rect_set_x)?;
  let set_x_name = scope.alloc_string("set x")?;
  scope.push_root(Value::String(set_x_name))?;
  let set_x_fn = scope.alloc_native_function(set_x_id, None, set_x_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(set_x_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(set_x_fn))?;

  let set_y_id = vm.register_native_call(dom_rect_set_y)?;
  let set_y_name = scope.alloc_string("set y")?;
  scope.push_root(Value::String(set_y_name))?;
  let set_y_fn = scope.alloc_native_function(set_y_id, None, set_y_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(set_y_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(set_y_fn))?;

  let set_width_id = vm.register_native_call(dom_rect_set_width)?;
  let set_width_name = scope.alloc_string("set width")?;
  scope.push_root(Value::String(set_width_name))?;
  let set_width_fn = scope.alloc_native_function(set_width_id, None, set_width_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(set_width_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(set_width_fn))?;

  let set_height_id = vm.register_native_call(dom_rect_set_height)?;
  let set_height_name = scope.alloc_string("set height")?;
  scope.push_root(Value::String(set_height_name))?;
  let set_height_fn = scope.alloc_native_function(set_height_id, None, set_height_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(set_height_fn, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(set_height_fn))?;

  // Override x/y/width/height with setters on DOMRect.prototype.
  let x_key = alloc_key(&mut scope, "x")?;
  scope.define_property(
    rect_proto,
    x_key,
    accessor_desc(Value::Object(get_x_fn), Value::Object(set_x_fn)),
  )?;
  let y_key = alloc_key(&mut scope, "y")?;
  scope.define_property(
    rect_proto,
    y_key,
    accessor_desc(Value::Object(get_y_fn), Value::Object(set_y_fn)),
  )?;
  let width_key = alloc_key(&mut scope, "width")?;
  scope.define_property(
    rect_proto,
    width_key,
    accessor_desc(Value::Object(get_width_fn), Value::Object(set_width_fn)),
  )?;
  let height_key = alloc_key(&mut scope, "height")?;
  scope.define_property(
    rect_proto,
    height_key,
    accessor_desc(Value::Object(get_height_fn), Value::Object(set_height_fn)),
  )?;

  // Expose DOMRect on the global.
  let rect_global_key = alloc_key(&mut scope, "DOMRect")?;
  scope.define_property(
    global,
    rect_global_key,
    data_desc(Value::Object(rect_ctor)),
  )?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};

  #[test]
  fn dom_rect_read_only_derived_getters() {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).unwrap();
    let v = window
      .exec_script(
        "(() => { const r = new DOMRectReadOnly(1, 2, 3, 4); return r.right === 4 && r.bottom === 6 && r.top === 2 && r.left === 1; })()",
      )
      .unwrap();
    assert_eq!(v, Value::Bool(true));

    let v = window
      .exec_script(
        "(() => { const j = new DOMRectReadOnly(1, 2, 3, 4).toJSON(); return j.x === 1 && j.y === 2 && j.width === 3 && j.height === 4 && j.right === 4 && j.bottom === 6 && j.left === 1 && j.top === 2; })()",
      )
      .unwrap();
    assert_eq!(v, Value::Bool(true));
  }

  #[test]
  fn dom_rect_setters_update_derived_getters() {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).unwrap();
    let v = window
      .exec_script(
        "(() => { const r = new DOMRect(1, 2, 3, 4); r.x = 10; r.y = 20; r.width = 5; r.height = 6; return r.x === 10 && r.y === 20 && r.width === 5 && r.height === 6 && r.left === 10 && r.top === 20 && r.right === 15 && r.bottom === 26; })()",
      )
      .unwrap();
    assert_eq!(v, Value::Bool(true));
  }

  #[test]
  fn dom_rect_constructors_require_new() {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).unwrap();
    let v = window
      .exec_script(
        "(() => { try { DOMRectReadOnly(); return false; } catch (e) { return e instanceof TypeError && String(e.message || e).includes(\"cannot be invoked without 'new'\"); } })()",
      )
      .unwrap();
    assert_eq!(v, Value::Bool(true));
    let v = window
      .exec_script(
        "(() => { try { DOMRect(); return false; } catch (e) { return e instanceof TypeError && String(e.message || e).includes(\"cannot be invoked without 'new'\"); } })()",
      )
      .unwrap();
    assert_eq!(v, Value::Bool(true));
  }

  #[test]
  fn observer_entry_rects_are_dom_rect_read_only_instances() {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).unwrap();
    let v = window
      .exec_script(
        r#"(() => {
          // Force the async delivery path so we can synchronously inspect `takeRecords()` regardless
          // of whether this realm has a native queueMicrotask binding.
          globalThis.__fastrender_queue_microtask = (cb) => { Promise.resolve().then(cb); };

          const target = {};
          const observer = new IntersectionObserver(() => {});
          observer.observe(target);
          const entries = observer.takeRecords();
          if (!Array.isArray(entries) || entries.length !== 1) return false;
          const e = entries[0];
          try {
            // Ensure the objects are real `DOMRectReadOnly` instances *and* expose derived getters
            // backed by the internal slots (missing slots should throw).
            const rects = [e.boundingClientRect, e.intersectionRect, e.rootBounds];
            for (const r of rects) {
              if (!(r instanceof DOMRectReadOnly)) return false;
              if (typeof r.right !== 'number') return false;
              if (typeof r.bottom !== 'number') return false;
            }
            return true;
          } catch (err) {
            return false;
          }
        })()"#,
      )
      .unwrap();

    // Drain the queued microtask to avoid leaving pending jobs across tests.
    window.perform_microtask_checkpoint().unwrap();
    assert_eq!(v, Value::Bool(true));
  }
}
