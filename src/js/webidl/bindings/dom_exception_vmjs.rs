use crate::web::dom::DomException;
use vm_js::{
  new_error, GcObject, HostSlots, Intrinsics, NativeConstructId, NativeFunctionId,
  PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks,
};

const DOM_EXCEPTION_HOST_SLOTS_TAG: u64 = u64::from_le_bytes(*b"DOMExcpt");

#[derive(Debug, Clone, Copy)]
pub struct DomExceptionClassVmJs {
  pub constructor: GcObject,
  pub prototype: GcObject,
}

#[derive(Debug, Clone, Copy)]
struct LegacyDomExceptionCode {
  legacy_name: &'static str,
  modern_name: &'static str,
  code: u16,
}

// DOMException legacy codes:
// https://webidl.spec.whatwg.org/#idl-DOMException
// (plus historical legacy aliases that some test harnesses still use)
const LEGACY_DOM_EXCEPTION_CODES: &[LegacyDomExceptionCode] = &[
  LegacyDomExceptionCode {
    legacy_name: "INDEX_SIZE_ERR",
    modern_name: "IndexSizeError",
    code: 1,
  },
  LegacyDomExceptionCode {
    legacy_name: "DOMSTRING_SIZE_ERR",
    modern_name: "DOMStringSizeError",
    code: 2,
  },
  LegacyDomExceptionCode {
    legacy_name: "HIERARCHY_REQUEST_ERR",
    modern_name: "HierarchyRequestError",
    code: 3,
  },
  LegacyDomExceptionCode {
    legacy_name: "WRONG_DOCUMENT_ERR",
    modern_name: "WrongDocumentError",
    code: 4,
  },
  LegacyDomExceptionCode {
    legacy_name: "INVALID_CHARACTER_ERR",
    modern_name: "InvalidCharacterError",
    code: 5,
  },
  LegacyDomExceptionCode {
    legacy_name: "NO_DATA_ALLOWED_ERR",
    modern_name: "NoDataAllowedError",
    code: 6,
  },
  LegacyDomExceptionCode {
    legacy_name: "NO_MODIFICATION_ALLOWED_ERR",
    modern_name: "NoModificationAllowedError",
    code: 7,
  },
  LegacyDomExceptionCode {
    legacy_name: "NOT_FOUND_ERR",
    modern_name: "NotFoundError",
    code: 8,
  },
  LegacyDomExceptionCode {
    legacy_name: "NOT_SUPPORTED_ERR",
    modern_name: "NotSupportedError",
    code: 9,
  },
  LegacyDomExceptionCode {
    legacy_name: "INUSE_ATTRIBUTE_ERR",
    modern_name: "InUseAttributeError",
    code: 10,
  },
  LegacyDomExceptionCode {
    legacy_name: "INVALID_STATE_ERR",
    modern_name: "InvalidStateError",
    code: 11,
  },
  LegacyDomExceptionCode {
    legacy_name: "SYNTAX_ERR",
    modern_name: "SyntaxError",
    code: 12,
  },
  LegacyDomExceptionCode {
    legacy_name: "INVALID_MODIFICATION_ERR",
    modern_name: "InvalidModificationError",
    code: 13,
  },
  LegacyDomExceptionCode {
    legacy_name: "NAMESPACE_ERR",
    modern_name: "NamespaceError",
    code: 14,
  },
  LegacyDomExceptionCode {
    legacy_name: "INVALID_ACCESS_ERR",
    modern_name: "InvalidAccessError",
    code: 15,
  },
  LegacyDomExceptionCode {
    legacy_name: "VALIDATION_ERR",
    modern_name: "ValidationError",
    code: 16,
  },
  LegacyDomExceptionCode {
    legacy_name: "TYPE_MISMATCH_ERR",
    modern_name: "TypeMismatchError",
    code: 17,
  },
  LegacyDomExceptionCode {
    legacy_name: "SECURITY_ERR",
    modern_name: "SecurityError",
    code: 18,
  },
  LegacyDomExceptionCode {
    legacy_name: "NETWORK_ERR",
    modern_name: "NetworkError",
    code: 19,
  },
  LegacyDomExceptionCode {
    legacy_name: "ABORT_ERR",
    modern_name: "AbortError",
    code: 20,
  },
  LegacyDomExceptionCode {
    legacy_name: "URL_MISMATCH_ERR",
    modern_name: "URLMismatchError",
    code: 21,
  },
  LegacyDomExceptionCode {
    legacy_name: "QUOTA_EXCEEDED_ERR",
    modern_name: "QuotaExceededError",
    code: 22,
  },
  LegacyDomExceptionCode {
    legacy_name: "TIMEOUT_ERR",
    modern_name: "TimeoutError",
    code: 23,
  },
  LegacyDomExceptionCode {
    legacy_name: "INVALID_NODE_TYPE_ERR",
    modern_name: "InvalidNodeTypeError",
    code: 24,
  },
  LegacyDomExceptionCode {
    legacy_name: "DATA_CLONE_ERR",
    modern_name: "DataCloneError",
    code: 25,
  },
];

/// Returns the legacy numeric `DOMException.code` for a given DOMException name.
///
/// WebIDL defines modern `DOMException.name` strings (e.g. `"InvalidStateError"`) but still
/// specifies legacy numeric codes for backwards compatibility. Some shims/tests also use the legacy
/// constant names (e.g. `"INVALID_STATE_ERR"`), so we accept both forms. Unknown names map to `0`.
///
/// Kept as a stable helper for legacy call sites that still construct plain `{ name, message }`
/// objects instead of proper DOMException instances.
pub fn legacy_code_for_dom_exception_name(name: &str) -> u16 {
  LEGACY_DOM_EXCEPTION_CODES
    .iter()
    .find_map(|entry| {
      if entry.modern_name == name || entry.legacy_name == name {
        Some(entry.code)
      } else {
        None
      }
    })
    .unwrap_or(0)
}

/// Return the legacy `DOMException` numeric code for a given exception name.
///
/// Accepts both legacy constant names (e.g. `HIERARCHY_REQUEST_ERR`) and modern DOMException names
/// (e.g. `HierarchyRequestError`).
///
/// This is used by handwritten `vm-js` DOM shims that need to synthesize a DOMException instance
/// with a legacy `code` field.
pub fn legacy_code_for_dom_exception_name(name: &str) -> u16 {
  legacy_dom_exception_code_from_name(name)
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

fn const_desc(value: Value) -> PropertyDescriptor {
  // WebIDL `const`: non-writable, enumerable, non-configurable.
  PropertyDescriptor {
    enumerable: true,
    configurable: false,
    kind: PropertyKind::Data {
      value,
      writable: false,
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

    // Legacy DOMException numeric constants.
    //
    // WPT's Range harness (`tests/wpt_dom/tests/dom/common.js`) expects these to be enumerable on an
    // exception instance via prototype-chain enumeration.
    for entry in LEGACY_DOM_EXCEPTION_CODES {
      let key_s = scope.alloc_string(entry.legacy_name)?;
      scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);
      let value = Value::Number(entry.code as f64);
      // Expose on both the prototype and constructor (WebIDL-style).
      scope.define_property(proto, key, const_desc(value))?;
      scope.define_property(ctor, key, const_desc(value))?;
    }

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
    scope.heap_mut().object_set_host_slots(
      obj,
      HostSlots {
        a: DOM_EXCEPTION_HOST_SLOTS_TAG,
        b: 0,
      },
    )?;
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

    // Legacy `DOMException.code`.
    let code = legacy_code_for_dom_exception_name(name);
    let key_code_s = scope.alloc_string("code")?;
    scope.push_root(Value::String(key_code_s))?;
    let key_code = PropertyKey::from_string(key_code_s);
    scope.define_property(obj, key_code, data_desc(Value::Number(code as f64)))?;

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
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: DOM_EXCEPTION_HOST_SLOTS_TAG,
      b: 0,
    },
  )?;
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

  // Legacy `DOMException.code`.
  let name_utf8 = scope.heap().get_string(name_s)?.to_utf8_lossy();
  let code = legacy_code_for_dom_exception_name(name_utf8.as_ref());
  let key_code_s = scope.alloc_string("code")?;
  scope.push_root(Value::String(key_code_s))?;
  let key_code = PropertyKey::from_string(key_code_s);
  scope.define_property(obj, key_code, data_desc(Value::Number(code as f64)))?;

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

#[cfg(test)]
mod tests {
  use super::*;
  use vm_js::{
    ExecutionContext, Heap, HeapLimits, PropertyKey, PropertyKind, Realm, Scope, Value, Vm,
    VmError, VmOptions,
  };

  #[test]
  fn legacy_code_for_dom_exception_name_maps_known_errors() {
    assert_eq!(legacy_code_for_dom_exception_name("InvalidCharacterError"), 5);
    assert_eq!(legacy_code_for_dom_exception_name("INVALID_CHARACTER_ERR"), 5);
    assert_eq!(legacy_code_for_dom_exception_name("NotSupportedError"), 9);
    assert_eq!(legacy_code_for_dom_exception_name("NOT_SUPPORTED_ERR"), 9);
    assert_eq!(legacy_code_for_dom_exception_name("SomeMadeUpError"), 0);
  }

  fn key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
    let s = scope.alloc_string(name)?;
    scope.push_root(Value::String(s))?;
    Ok(PropertyKey::from_string(s))
  }

  fn as_utf8(scope: &Scope<'_>, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string, got {value:?}");
    };
    scope
      .heap()
      .get_string(s)
      .expect("string handle should be valid")
      .to_utf8_lossy()
  }

  fn as_f64(value: Value) -> f64 {
    let Value::Number(n) = value else {
      panic!("expected number, got {value:?}");
    };
    n
  }

  #[test]
  fn dom_exception_constructs_and_has_name_message_and_to_string() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    // Install the DOMException class into the realm global object.
    let class = {
      let mut scope = heap.scope();
      DomExceptionClassVmJs::install(&mut vm, &mut scope, &realm)?
    };

    {
      let mut scope = heap.scope();
      let msg_s = scope.alloc_string("m")?;
      scope.push_root(Value::String(msg_s))?;
      let name_s = scope.alloc_string("SyntaxError")?;
      scope.push_root(Value::String(name_s))?;

      let realm_id = realm.id();
      let mut vm = vm
        .execution_context_guard(ExecutionContext {
          realm: realm_id,
          script_or_module: None,
        })?;

      let obj = vm.construct_without_host(
        &mut scope,
        Value::Object(class.constructor),
        &[Value::String(msg_s), Value::String(name_s)],
        Value::Object(class.constructor),
      )?;
      scope.push_root(obj)?;
      let Value::Object(obj_handle) = obj else {
        panic!("expected DOMException constructor to return an object, got {obj:?}");
      };

      // .name === "SyntaxError"
      let name_key = key(&mut scope, "name")?;
      let name_value = vm.get(&mut scope, obj_handle, name_key)?;
      assert_eq!(as_utf8(&scope, name_value), "SyntaxError");

      // .message === "m"
      let message_key = key(&mut scope, "message")?;
      let message_value = vm.get(&mut scope, obj_handle, message_key)?;
      assert_eq!(as_utf8(&scope, message_value), "m");

      // .code === 12
      let code_key = key(&mut scope, "code")?;
      let code_value = vm.get(&mut scope, obj_handle, code_key)?;
      assert_eq!(as_f64(code_value), 12.0);

      // toString() === "SyntaxError: m"
      let to_string_key = key(&mut scope, "toString")?;
      let to_string_fn = vm.get(&mut scope, obj_handle, to_string_key)?;
      let out = vm.call_without_host(&mut scope, to_string_fn, Value::Object(obj_handle), &[])?;
      assert_eq!(as_utf8(&scope, out), "SyntaxError: m");

      // Verify property attributes for own `name`/`message`: non-enumerable.
      let name_desc = scope
        .heap()
        .object_get_own_property(obj_handle, &name_key)?
        .expect("expected own name property");
      assert!(!name_desc.enumerable);
      let PropertyKind::Data { .. } = name_desc.kind else {
        panic!("expected name to be a data property");
      };

      let message_desc = scope
        .heap()
        .object_get_own_property(obj_handle, &message_key)?
        .expect("expected own message property");
      assert!(!message_desc.enumerable);
      let PropertyKind::Data { .. } = message_desc.kind else {
        panic!("expected message to be a data property");
      };

      let code_desc = scope
        .heap()
        .object_get_own_property(obj_handle, &code_key)?
        .expect("expected own code property");
      assert!(!code_desc.enumerable);
      let PropertyKind::Data { .. } = code_desc.kind else {
        panic!("expected code to be a data property");
      };
    }

    realm.teardown(&mut heap);

    Ok(())
  }

  #[test]
  fn dom_exception_from_dom_exception_has_code_and_constants() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let class = {
      let mut scope = heap.scope();
      DomExceptionClassVmJs::install(&mut vm, &mut scope, &realm)?
    };

    {
      let mut scope = heap.scope();
      scope.push_root(Value::Object(class.constructor))?;

      let err = DomException::invalid_state_error("bad state");
      let obj = class.from_dom_exception(&mut scope, &err)?;
      scope.push_root(obj)?;
      let Value::Object(obj_handle) = obj else {
        panic!("expected DOMException instance to be an object, got {obj:?}");
      };

      let realm_id = realm.id();
      let mut vm = vm
        .execution_context_guard(ExecutionContext {
          realm: realm_id,
          script_or_module: None,
        })?;

      let name_key = key(&mut scope, "name")?;
      let name_value = vm.get(&mut scope, obj_handle, name_key)?;
      assert_eq!(as_utf8(&scope, name_value), "InvalidStateError");

      let message_key = key(&mut scope, "message")?;
      let message_value = vm.get(&mut scope, obj_handle, message_key)?;
      assert_eq!(as_utf8(&scope, message_value), "bad state");

      let code_key = key(&mut scope, "code")?;
      let code_value = vm.get(&mut scope, obj_handle, code_key)?;
      assert_eq!(as_f64(code_value), 11.0);

      // DOMException.INVALID_STATE_ERR === 11
      let invalid_state_err_key = key(&mut scope, "INVALID_STATE_ERR")?;
      let invalid_state_err_value = vm.get(&mut scope, class.constructor, invalid_state_err_key)?;
      assert_eq!(as_f64(invalid_state_err_value), 11.0);

      // DOMException.SYNTAX_ERR === 12
      let syntax_err_key = key(&mut scope, "SYNTAX_ERR")?;
      let syntax_err_value = vm.get(&mut scope, class.constructor, syntax_err_key)?;
      assert_eq!(as_f64(syntax_err_value), 12.0);

      // Verify property attributes for a legacy constant: enumerable, non-configurable, non-writable.
      let desc = scope
        .heap()
        .object_get_own_property(class.constructor, &invalid_state_err_key)?
        .expect("expected DOMException.INVALID_STATE_ERR to exist");
      assert!(desc.enumerable);
      assert!(!desc.configurable);
      let PropertyKind::Data { writable, .. } = desc.kind else {
        panic!("expected DOMException.INVALID_STATE_ERR to be a data property");
      };
      assert!(!writable);
    }

    realm.teardown(&mut heap);

    Ok(())
  }

  #[test]
  fn dom_exception_has_legacy_code_and_constants() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let class = {
      let mut scope = heap.scope();
      DomExceptionClassVmJs::install(&mut vm, &mut scope, &realm)?
    };

    {
      let mut scope = heap.scope();
      let realm_id = realm.id();
      let mut vm = vm
        .execution_context_guard(ExecutionContext {
          realm: realm_id,
          script_or_module: None,
        })?;

      // Representative legacy constants must exist and be enumerable on the prototype.
      let key_hierarchy = key(&mut scope, "HIERARCHY_REQUEST_ERR")?;
      let hierarchy_desc = scope
        .heap()
        .object_get_own_property(class.prototype, &key_hierarchy)?
        .expect("expected HIERARCHY_REQUEST_ERR on DOMException.prototype");
      assert!(
        hierarchy_desc.enumerable,
        "legacy constants must be enumerable for WPT `for..in` discovery"
      );
      let PropertyKind::Data { value, .. } = hierarchy_desc.kind else {
        panic!("expected HIERARCHY_REQUEST_ERR to be a data property");
      };
      assert_eq!(as_f64(value), 3.0);

      // `.code` must be derived from the exception name, supporting both modern and legacy names.
      let code_key = key(&mut scope, "code")?;

      for (name, expected_code) in [
        ("HierarchyRequestError", 3.0),
        ("InvalidStateError", 11.0),
        ("InvalidNodeTypeError", 24.0),
        ("IndexSizeError", 1.0),
        // Legacy name passed to the constructor.
        ("HIERARCHY_REQUEST_ERR", 3.0),
      ] {
        let msg_s = scope.alloc_string("m")?;
        scope.push_root(Value::String(msg_s))?;
        let name_s = scope.alloc_string(name)?;
        scope.push_root(Value::String(name_s))?;

        let obj = vm.construct_without_host(
          &mut scope,
          Value::Object(class.constructor),
          &[Value::String(msg_s), Value::String(name_s)],
          Value::Object(class.constructor),
        )?;
        scope.push_root(obj)?;
        let Value::Object(obj_handle) = obj else {
          panic!("expected DOMException constructor to return an object, got {obj:?}");
        };

        let code = vm.get(&mut scope, obj_handle, code_key)?;
        assert_eq!(as_f64(code), expected_code, "wrong .code for name {name}");

        // Prototype-chain lookup: instance["HIERARCHY_REQUEST_ERR"] === 3
        let hierarchy_value = vm.get(&mut scope, obj_handle, key_hierarchy)?;
        assert_eq!(as_f64(hierarchy_value), 3.0);
      }

      // Ensure `DomExceptionClassVmJs::new_instance` also sets `.code`.
      let obj = class.new_instance(&mut scope, "InvalidStateError", "m")?;
      scope.push_root(obj)?;
      let Value::Object(obj_handle) = obj else {
        panic!("expected DOMException instance to be an object");
      };
      let code = vm.get(&mut scope, obj_handle, code_key)?;
      assert_eq!(as_f64(code), 11.0);
    }

    realm.teardown(&mut heap);
    Ok(())
  }
}
