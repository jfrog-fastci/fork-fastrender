use crate::web::dom::DomException;
use vm_js::{
  new_error, GcObject, Intrinsics, NativeConstructId, NativeFunctionId, PropertyDescriptor,
  PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
};

#[derive(Debug, Clone, Copy)]
pub struct DomExceptionClassVmJs {
  pub constructor: GcObject,
  pub prototype: GcObject,
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

fn method_desc(value: Value) -> PropertyDescriptor {
  // Prototype methods are writable, non-enumerable, configurable.
  data_desc(value)
}

impl DomExceptionClassVmJs {
  pub fn install(vm: &mut Vm, scope: &mut Scope<'_>, realm: &Realm) -> Result<Self, VmError> {
    Self::install_for_global(vm, scope, realm.global_object(), *realm.intrinsics())
  }

  pub fn install_for_global(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    global: GcObject,
    intr: Intrinsics,
  ) -> Result<Self, VmError> {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(global))?;

    // Idempotent install: if DOMException is already present, reuse it.
    let key_dom_exception_s = scope.alloc_string("DOMException")?;
    scope.push_root(Value::String(key_dom_exception_s))?;
    let key_dom_exception = PropertyKey::from_string(key_dom_exception_s);
    if let Some(Value::Object(existing_ctor)) = scope
      .heap()
      .object_get_own_data_property_value(global, &key_dom_exception)?
    {
      let key_prototype_s = scope.alloc_string("prototype")?;
      scope.push_root(Value::String(key_prototype_s))?;
      let key_prototype = PropertyKey::from_string(key_prototype_s);
      if let Some(Value::Object(existing_proto)) = scope
        .heap()
        .object_get_own_data_property_value(existing_ctor, &key_prototype)?
      {
        return Ok(Self {
          constructor: existing_ctor,
          prototype: existing_proto,
        });
      }
    }

    let call_id: NativeFunctionId = vm.register_native_call(dom_exception_call)?;
    let construct_id: NativeConstructId = vm.register_native_construct(dom_exception_construct)?;

    let ctor_name_s = scope.alloc_string("DOMException")?;
    scope.push_root(Value::String(ctor_name_s))?;
    let ctor = scope.alloc_native_function(call_id, Some(construct_id), ctor_name_s, 2)?;
    scope.push_root(Value::Object(ctor))?;
    scope
      .heap_mut()
      .object_set_prototype(ctor, Some(intr.function_prototype()))?;

    // Extract the `.prototype` object created by `vm-js`'s `make_constructor`.
    let key_prototype_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_prototype_s))?;
    let key_prototype = PropertyKey::from_string(key_prototype_s);
    let Some(Value::Object(proto)) = scope
      .heap()
      .object_get_own_data_property_value(ctor, &key_prototype)?
    else {
      return Err(VmError::InvariantViolation(
        "DOMException constructor missing prototype object",
      ));
    };
    scope.push_root(Value::Object(proto))?;
    scope
      .heap_mut()
      .object_set_prototype(proto, Some(intr.error_prototype()))?;

    // DOMException.prototype.toString (minimal).
    let to_string_call_id: NativeFunctionId = vm.register_native_call(dom_exception_to_string)?;
    let to_string_name_s = scope.alloc_string("toString")?;
    scope.push_root(Value::String(to_string_name_s))?;
    let to_string_fn = scope.alloc_native_function(to_string_call_id, None, to_string_name_s, 0)?;
    scope.push_root(Value::Object(to_string_fn))?;
    scope
      .heap_mut()
      .object_set_prototype(to_string_fn, Some(intr.function_prototype()))?;

    let key_to_string_s = scope.alloc_string("toString")?;
    scope.push_root(Value::String(key_to_string_s))?;
    let key_to_string = PropertyKey::from_string(key_to_string_s);
    scope.define_property(
      proto,
      key_to_string,
      method_desc(Value::Object(to_string_fn)),
    )?;

    // Expose DOMException on the global object.
    scope.define_property(global, key_dom_exception, data_desc(Value::Object(ctor)))?;

    Ok(Self {
      constructor: ctor,
      prototype: proto,
    })
  }

  pub fn new_instance(
    &self,
    scope: &mut Scope<'_>,
    name: &str,
    message: &str,
  ) -> Result<Value, VmError> {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(self.prototype))?;

    let name_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(name_s))?;
    let message_s = scope.alloc_string(message)?;
    scope.push_root(Value::String(message_s))?;

    let obj = scope.alloc_object()?;
    scope.push_root(Value::Object(obj))?;
    scope
      .heap_mut()
      .object_set_prototype(obj, Some(self.prototype))?;

    let key_name_s = scope.alloc_string("name")?;
    scope.push_root(Value::String(key_name_s))?;
    let key_name = PropertyKey::from_string(key_name_s);
    scope.define_property(obj, key_name, data_desc(Value::String(name_s)))?;

    let key_message_s = scope.alloc_string("message")?;
    scope.push_root(Value::String(key_message_s))?;
    let key_message = PropertyKey::from_string(key_message_s);
    scope.define_property(obj, key_message, data_desc(Value::String(message_s)))?;

    Ok(Value::Object(obj))
  }

  pub fn from_dom_exception(
    &self,
    scope: &mut Scope<'_>,
    err: &DomException,
  ) -> Result<Value, VmError> {
    match err {
      DomException::SyntaxError { message } => {
        self.new_instance(scope, "SyntaxError", message.as_str())
      }
      DomException::NoModificationAllowedError { message } => {
        self.new_instance(scope, "NoModificationAllowedError", message.as_str())
      }
      DomException::NotSupportedError { message } => {
        self.new_instance(scope, "NotSupportedError", message.as_str())
      }
      DomException::InvalidStateError { message } => {
        self.new_instance(scope, "InvalidStateError", message.as_str())
      }
    }
  }
}

fn dom_exception_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  dom_exception_create_instance(vm, scope, callee, args)
}

fn dom_exception_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  dom_exception_create_instance(vm, scope, callee, args)
}

fn dom_exception_create_instance(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(callee))?;

  let prototype = {
    let prototype_key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(prototype_key_s))?;
    let prototype_key = PropertyKey::from_string(prototype_key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &prototype_key)?
    {
      Some(Value::Object(o)) => o,
      _ => {
        // This should not happen unless user code has mutated `DOMException.prototype`.
        // Fall back to the realm's `%Object.prototype%` when available.
        let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
          "DOMException requires intrinsics (create a Realm first)",
        ))?;
        intr.object_prototype()
      }
    }
  };

  let message_s = match args.get(0).copied() {
    None | Some(Value::Undefined) => scope.alloc_string("")?,
    Some(Value::String(s)) => s,
    Some(v) => scope.heap_mut().to_string(v)?,
  };
  scope.push_root(Value::String(message_s))?;

  let name_s = match args.get(1).copied() {
    None | Some(Value::Undefined) => scope.alloc_string("Error")?,
    Some(Value::String(s)) => s,
    Some(v) => scope.heap_mut().to_string(v)?,
  };
  scope.push_root(Value::String(name_s))?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope
    .heap_mut()
    .object_set_prototype(obj, Some(prototype))?;

  let key_name_s = scope.alloc_string("name")?;
  scope.push_root(Value::String(key_name_s))?;
  let key_name = PropertyKey::from_string(key_name_s);
  scope.define_property(obj, key_name, data_desc(Value::String(name_s)))?;

  let key_message_s = scope.alloc_string("message")?;
  scope.push_root(Value::String(key_message_s))?;
  let key_message = PropertyKey::from_string(key_message_s);
  scope.define_property(obj, key_message, data_desc(Value::String(message_s)))?;

  Ok(Value::Object(obj))
}

fn dom_exception_to_string(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let this_obj = match this {
    Value::Object(o) => o,
    _ => {
      return Err(VmError::TypeError(
        "DOMException.prototype.toString called on non-object",
      ))
    }
  };

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(this_obj))?;

  let name_key_s = scope.alloc_string("name")?;
  scope.push_root(Value::String(name_key_s))?;
  let name_key = PropertyKey::from_string(name_key_s);
  let name_value = scope
    .heap()
    .get_property(this_obj, &name_key)?
    .map(|desc| match desc.kind {
      PropertyKind::Data { value, .. } => Ok(value),
      PropertyKind::Accessor { .. } => Err(VmError::PropertyNotData),
    })
    .transpose()?
    .unwrap_or(Value::Undefined);

  let message_key_s = scope.alloc_string("message")?;
  scope.push_root(Value::String(message_key_s))?;
  let message_key = PropertyKey::from_string(message_key_s);
  let message_value = scope
    .heap()
    .get_property(this_obj, &message_key)?
    .map(|desc| match desc.kind {
      PropertyKind::Data { value, .. } => Ok(value),
      PropertyKind::Accessor { .. } => Err(VmError::PropertyNotData),
    })
    .transpose()?
    .unwrap_or(Value::Undefined);

  let name_s = match name_value {
    Value::Undefined => scope.alloc_string("Error")?,
    Value::String(s) => s,
    other => scope.heap_mut().to_string(other)?,
  };
  scope.push_root(Value::String(name_s))?;

  let message_s = match message_value {
    Value::Undefined => scope.alloc_string("")?,
    Value::String(s) => s,
    other => scope.heap_mut().to_string(other)?,
  };
  scope.push_root(Value::String(message_s))?;

  let name_units = scope.heap().get_string(name_s)?.as_code_units();
  let message_units = scope.heap().get_string(message_s)?.as_code_units();

  if name_units.is_empty() {
    return Ok(Value::String(message_s));
  }
  if message_units.is_empty() {
    return Ok(Value::String(name_s));
  }

  let mut out: Vec<u16> = Vec::new();
  out
    .try_reserve(
      name_units
        .len()
        .saturating_add(2)
        .saturating_add(message_units.len()),
    )
    .map_err(|_| VmError::OutOfMemory)?;
  out.extend_from_slice(name_units);
  out.push(b':' as u16);
  out.push(b' ' as u16);
  out.extend_from_slice(message_units);

  let s = scope.alloc_string_from_u16_vec(out)?;
  Ok(Value::String(s))
}

pub fn throw_dom_exception(
  scope: &mut Scope<'_>,
  dom_exception: DomExceptionClassVmJs,
  name: &str,
  message: &str,
) -> VmError {
  match dom_exception.new_instance(scope, name, message) {
    Ok(value) => VmError::Throw(value),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

pub fn dom_exception_from_rust(
  scope: &mut Scope<'_>,
  dom_exception: DomExceptionClassVmJs,
  err: &DomException,
) -> Value {
  dom_exception
    .from_dom_exception(scope, err)
    .unwrap_or(Value::Undefined)
}

pub fn throw_dom_exception_like_error(
  scope: &mut Scope<'_>,
  intr: Intrinsics,
  name: &str,
  message: &str,
) -> VmError {
  match new_error(scope, intr.error_prototype(), name, message) {
    Ok(value) => VmError::Throw(value),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

pub fn dom_exception_from_rust_like_error(
  scope: &mut Scope<'_>,
  intr: Intrinsics,
  err: &DomException,
) -> Value {
  let (name, message) = match err {
    DomException::SyntaxError { message } => ("SyntaxError", message.as_str()),
    DomException::NoModificationAllowedError { message } => {
      ("NoModificationAllowedError", message.as_str())
    }
    DomException::NotSupportedError { message } => ("NotSupportedError", message.as_str()),
    DomException::InvalidStateError { message } => ("InvalidStateError", message.as_str()),
  };
  new_error(scope, intr.error_prototype(), name, message).unwrap_or(Value::Undefined)
}
