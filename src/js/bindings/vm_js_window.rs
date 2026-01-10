use super::host::{BindingValue, VmJsBindingsHost};
use vm_js::{
  GcObject, Heap, NativeConstructId, NativeFunctionId, PropertyDescriptor, PropertyKey,
  PropertyKind, Realm, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
};

const URLSP_CALL_WITHOUT_NEW_ERROR: &str = "URLSearchParams constructor must be called with new";
const ILLEGAL_INVOCATION_ERROR: &str = "Illegal invocation";

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

fn proto_method_desc(value: Value) -> PropertyDescriptor {
  // Prototype methods are typically non-enumerable, writable, configurable.
  data_desc(value)
}

fn ctor_link_desc(value: Value) -> PropertyDescriptor {
  // `prototype` and `constructor` links are typically non-enumerable and non-writable. Browsers also
  // make them non-configurable.
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

fn downcast_host_mut<Host: VmJsBindingsHost + 'static>(
  host: &mut dyn VmHost,
) -> Result<&mut Host, VmError> {
  host
    .as_any_mut()
    .downcast_mut::<Host>()
    .ok_or(VmError::InvariantViolation(
      "vm-js bindings invoked with unexpected host type",
    ))
}

fn params_proto_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let proto_slot = slots.get(0).copied().unwrap_or(Value::Undefined);
  match proto_slot {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::InvariantViolation(
      "URLSearchParams constructor missing prototype slot",
    )),
  }
}

fn value_to_rust_string(scope: &mut Scope<'_>, value: Value) -> Result<String, VmError> {
  // Match the behavior expected by the generated WebIDL scaffolding:
  // - `ToString` coercion first,
  // - then lossy UTF-8 conversion for host ownership.
  let s = scope.heap_mut().to_string(value)?;
  Ok(scope.heap().get_string(s)?.to_utf8_lossy())
}

fn binding_value_to_js(
  scope: &mut Scope<'_>,
  value: BindingValue<Value>,
) -> Result<Value, VmError> {
  match value {
    BindingValue::Undefined => Ok(Value::Undefined),
    BindingValue::Null => Ok(Value::Null),
    BindingValue::Bool(b) => Ok(Value::Bool(b)),
    BindingValue::Number(n) => Ok(Value::Number(n)),
    BindingValue::String(s) => Ok(Value::String(scope.alloc_string(&s)?)),
    BindingValue::Object(v) => Ok(v),
    BindingValue::Sequence(values) | BindingValue::FrozenArray(values) => {
      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;
      for (idx, item) in values.into_iter().enumerate() {
        let key = alloc_key(scope, &idx.to_string())?;
        let v = binding_value_to_js(scope, item)?;
        scope.define_property(
          obj,
          key,
          PropertyDescriptor {
            enumerable: true,
            configurable: true,
            kind: PropertyKind::Data {
              value: v,
              writable: true,
            },
          },
        )?;
      }
      Ok(Value::Object(obj))
    }
    BindingValue::Dictionary(map) => {
      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;
      for (k, item) in map.into_iter() {
        let key = alloc_key(scope, &k)?;
        let v = binding_value_to_js(scope, item)?;
        scope.define_property(
          obj,
          key,
          PropertyDescriptor {
            enumerable: true,
            configurable: true,
            kind: PropertyKind::Data {
              value: v,
              writable: true,
            },
          },
        )?;
      }
      Ok(Value::Object(obj))
    }
  }
}

fn url_search_params_call_without_new_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(URLSP_CALL_WITHOUT_NEW_ERROR))
}

fn url_search_params_construct_native<Host: VmJsBindingsHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let host = downcast_host_mut::<Host>(host)?;

  let mut scope = scope.reborrow();
  let params_proto = params_proto_from_callee(&scope, callee)?;
  scope.push_root(Value::Object(params_proto))?;

  // Allocate a fresh wrapper and brand it by setting the prototype.
  let obj = scope.alloc_object_with_prototype(Some(params_proto))?;
  scope.push_root(Value::Object(obj))?;

  // Match the existing generated binding behavior:
  // - missing/undefined init => empty string
  // - otherwise pass the raw JS value through as an opaque handle
  let init = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut converted_args: Vec<BindingValue<Value>> = Vec::with_capacity(1);
  converted_args.push(if matches!(init, Value::Undefined) {
    BindingValue::String(String::new())
  } else {
    BindingValue::Object(init)
  });

  let _ = host.call_operation(
    &mut scope,
    Some(Value::Object(obj)),
    "URLSearchParams",
    "constructor",
    0,
    converted_args,
  )?;

  Ok(Value::Object(obj))
}

fn url_search_params_append_native<Host: VmJsBindingsHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = downcast_host_mut::<Host>(host)?;

  let mut scope = scope.reborrow();

  let v0 = args.get(0).copied().unwrap_or(Value::Undefined);
  let v1 = args.get(1).copied().unwrap_or(Value::Undefined);

  let name = value_to_rust_string(&mut scope, v0)?;
  let value = value_to_rust_string(&mut scope, v1)?;

  let mut converted_args: Vec<BindingValue<Value>> = Vec::with_capacity(2);
  converted_args.push(BindingValue::String(name));
  converted_args.push(BindingValue::String(value));

  let result = host.call_operation(
    &mut scope,
    Some(this),
    "URLSearchParams",
    "append",
    0,
    converted_args,
  )?;
  binding_value_to_js(&mut scope, result)
}

fn url_search_params_get_native<Host: VmJsBindingsHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = downcast_host_mut::<Host>(host)?;

  let mut scope = scope.reborrow();

  let v0 = args.get(0).copied().unwrap_or(Value::Undefined);
  let name = value_to_rust_string(&mut scope, v0)?;

  let mut converted_args: Vec<BindingValue<Value>> = Vec::with_capacity(1);
  converted_args.push(BindingValue::String(name));

  let result = host.call_operation(
    &mut scope,
    Some(this),
    "URLSearchParams",
    "get",
    0,
    converted_args,
  )?;
  binding_value_to_js(&mut scope, result)
}

fn install_method<Host: VmJsBindingsHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  realm: &Realm,
  proto: GcObject,
  name: &str,
  call: vm_js::NativeCall,
  length: u32,
) -> Result<(), VmError> {
  let call_id = vm.register_native_call(call)?;

  let func_name = scope.alloc_string(name)?;
  scope.push_root(Value::String(func_name))?;
  let func = scope.alloc_native_function(call_id, None, func_name, length)?;
  scope
    .heap_mut()
    .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(func))?;

  let key = alloc_key(scope, name)?;
  scope.define_property(proto, key, proto_method_desc(Value::Object(func)))?;
  Ok(())
}

/// Install WebIDL-generated `Window` bindings directly into a `vm-js` [`Realm`].
///
/// This is a realm-based alternative to [`super::generated::install_window_bindings`], intended for
/// the `vm-js` embedding path. For now this installs only the subset needed by unit tests:
/// `URLSearchParams`.
pub fn install_window_bindings<Host: VmJsBindingsHost + 'static>(
  vm: &mut Vm,
  heap: &mut Heap,
  realm: &Realm,
) -> Result<(), VmError> {
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // --- URLSearchParams prototype ---
  let params_proto =
    scope.alloc_object_with_prototype(Some(realm.intrinsics().object_prototype()))?;
  scope.push_root(Value::Object(params_proto))?;

  // --- URLSearchParams constructor ---
  let call_id: NativeFunctionId =
    vm.register_native_call(url_search_params_call_without_new_native)?;
  let construct_id: NativeConstructId =
    vm.register_native_construct(url_search_params_construct_native::<Host>)?;

  let ctor_name = scope.alloc_string("URLSearchParams")?;
  scope.push_root(Value::String(ctor_name))?;
  let slots = [Value::Object(params_proto)];
  let ctor = scope.alloc_native_function_with_slots(
    call_id,
    Some(construct_id),
    ctor_name,
    /* length */ 1,
    &slots,
  )?;
  scope
    .heap_mut()
    .object_set_prototype(ctor, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(ctor))?;

  // Expose `URLSearchParams` on the global object.
  let ctor_key = alloc_key(&mut scope, "URLSearchParams")?;
  scope.define_property(global, ctor_key, data_desc(Value::Object(ctor)))?;

  // Wire constructor <-> prototype links.
  let prototype_key = alloc_key(&mut scope, "prototype")?;
  scope.define_property(
    ctor,
    prototype_key,
    ctor_link_desc(Value::Object(params_proto)),
  )?;
  let constructor_key = alloc_key(&mut scope, "constructor")?;
  scope.define_property(
    params_proto,
    constructor_key,
    ctor_link_desc(Value::Object(ctor)),
  )?;

  // --- URLSearchParams prototype methods ---
  install_method::<Host>(
    vm,
    &mut scope,
    realm,
    params_proto,
    "append",
    url_search_params_append_native::<Host>,
    2,
  )?;
  install_method::<Host>(
    vm,
    &mut scope,
    realm,
    params_proto,
    "get",
    url_search_params_get_native::<Host>,
    1,
  )?;

  Ok(())
}

#[allow(dead_code)]
fn unimplemented_operation_error() -> VmError {
  VmError::TypeError("unimplemented host operation")
}

#[allow(dead_code)]
fn illegal_invocation_error() -> VmError {
  VmError::TypeError(ILLEGAL_INVOCATION_ERROR)
}
