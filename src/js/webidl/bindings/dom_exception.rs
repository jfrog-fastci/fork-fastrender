use crate::web::dom::DomException;
use vm_js::{GcString, PropertyKey, Value, VmError};
use webidl_js_runtime::{JsRuntime as _, VmJsRuntime, WebIdlBindingsRuntime, WebIdlJsRuntime as _};

#[derive(Debug, Clone, Copy)]
pub struct DomExceptionClass {
  pub constructor: Value,
  pub prototype: Value,
}

impl DomExceptionClass {
  pub fn install(rt: &mut VmJsRuntime, global: Value) -> Result<Self, VmError> {
    let prototype = rt.alloc_object_value()?;
    Self::install_with_prototype(rt, global, prototype)
  }

  pub fn install_with_prototype(
    rt: &mut VmJsRuntime,
    global: Value,
    prototype: Value,
  ) -> Result<Self, VmError> {
    let key_dom_exception = prop_key(rt, "DOMException")?;
    let key_name = prop_key(rt, "name")?;
    let key_message = prop_key(rt, "message")?;
    let key_code = prop_key(rt, "code")?;
    let key_to_string = prop_key(rt, "toString")?;
    let key_constructor = prop_key(rt, "constructor")?;
    let key_prototype = prop_key(rt, "prototype")?;
    if !rt.is_object(prototype) {
      return Err(rt.throw_type_error("DOMException prototype must be an object"));
    }

    // Minimal `DOMException.prototype.toString()`.
    let to_string_fn = rt.alloc_function_value(move |rt, this, _args| {
      if !rt.is_object(this) {
        return Err(
          rt.throw_type_error("DOMException.prototype.toString called with non-object receiver"),
        );
      }

      let name = rt.get(this, key_name)?;
      let name = if matches!(name, Value::Undefined) {
        rt.alloc_string_value("Error")?
      } else {
        rt.to_string(name)?
      };
      let name = value_to_rust_string(rt, name)?;

      let message = rt.get(this, key_message)?;
      let message = if matches!(message, Value::Undefined) {
        rt.alloc_string_value("")?
      } else {
        rt.to_string(message)?
      };
      let message = value_to_rust_string(rt, message)?;

      let formatted = if name.is_empty() {
        message
      } else if message.is_empty() {
        name
      } else {
        format!("{name}: {message}")
      };
      rt.alloc_string_value(&formatted)
    })?;
    rt.define_data_property(prototype, key_to_string, to_string_fn, false)?;

    let proto_for_ctor = prototype;

    // Minimal `DOMException` constructor: `new DOMException(message, name)`.
    let constructor = rt.alloc_function_value(move |rt, _this, args| {
      // WebIDL default arguments apply when the argument is missing OR explicitly `undefined`.
      let message = match args.get(0).copied() {
        None | Some(Value::Undefined) => rt.alloc_string_value("")?,
        Some(v) => rt.to_string(v)?,
      };
      let name = match args.get(1).copied() {
        None | Some(Value::Undefined) => rt.alloc_string_value("Error")?,
        Some(v) => rt.to_string(v)?,
      };

      let obj = rt.alloc_object_value()?;
      rt.set_prototype(obj, Some(proto_for_ctor))?;
      let name = message_or_name_string(rt, name)?;
      let message = message_or_name_string(rt, message)?;
      let name_rust = value_to_rust_string(rt, name)?;
      let code = legacy_code_for_name(&name_rust);
      rt.define_data_property(obj, key_name, name, false)?;
      rt.define_data_property(obj, key_message, message, false)?;
      rt.define_data_property(obj, key_code, Value::Number(code as f64), false)?;
      Ok(obj)
    })?;

    // Legacy DOMException numeric constants (deprecated but still present for web compatibility).
    // WebIDL constants are non-writable, enumerable, non-configurable.
    for (name, value) in LEGACY_CODE_CONSTANTS {
      let key = prop_key(rt, name)?;
      <VmJsRuntime as WebIdlBindingsRuntime<()>>::define_data_property_with_attrs(
        rt,
        constructor,
        key,
        Value::Number(*value as f64),
        /* writable */ false,
        /* enumerable */ true,
        /* configurable */ false,
      )?;
    }

    // Link `constructor.prototype` and `prototype.constructor`.
    rt.define_data_property(constructor, key_prototype, prototype, false)?;
    rt.define_data_property(prototype, key_constructor, constructor, false)?;

    // Expose on the global object.
    rt.define_data_property(global, key_dom_exception, constructor, false)?;

    Ok(Self {
      constructor,
      prototype,
    })
  }

  pub fn new_instance(
    &self,
    rt: &mut VmJsRuntime,
    message: &str,
    name: &str,
  ) -> Result<Value, VmError> {
    let key_name = prop_key(rt, "name")?;
    let key_message = prop_key(rt, "message")?;
    let key_code = prop_key(rt, "code")?;

    let obj = rt.alloc_object_value()?;
    rt.set_prototype(obj, Some(self.prototype))?;
    let name_value = rt.alloc_string_value(name)?;
    let message_value = rt.alloc_string_value(message)?;
    let code = legacy_code_for_name(name);
    rt.define_data_property(obj, key_name, name_value, false)?;
    rt.define_data_property(obj, key_message, message_value, false)?;
    rt.define_data_property(obj, key_code, Value::Number(code as f64), false)?;
    Ok(obj)
  }

  pub fn from_dom_exception(
    &self,
    rt: &mut VmJsRuntime,
    err: &DomException,
  ) -> Result<Value, VmError> {
    match err {
      DomException::SyntaxError { message } => self.new_instance(rt, message, "SyntaxError"),
      DomException::NoModificationAllowedError { message } => {
        self.new_instance(rt, message, "NoModificationAllowedError")
      }
      DomException::NotSupportedError { message } => {
        self.new_instance(rt, message, "NotSupportedError")
      }
      DomException::InvalidStateError { message } => {
        self.new_instance(rt, message, "InvalidStateError")
      }
    }
  }
}

fn prop_key(rt: &mut VmJsRuntime, s: &str) -> Result<PropertyKey, VmError> {
  let v = rt.alloc_string_value(s)?;
  let Value::String(handle) = v else {
    return Err(rt.throw_type_error("expected string value"));
  };
  Ok(PropertyKey::String(handle))
}

fn message_or_name_string(rt: &mut VmJsRuntime, value: Value) -> Result<Value, VmError> {
  // Ensure we always store a primitive string value (not a String object).
  match value {
    Value::String(_) => Ok(value),
    _ => rt.to_string(value),
  }
}

fn value_to_rust_string(rt: &mut VmJsRuntime, value: Value) -> Result<String, VmError> {
  rt.string_to_utf8_lossy(value)
}

// DOMException "legacy codes" as specified by the DOM Standard.
//
// <https://dom.spec.whatwg.org/#dom-domexception-code>
const LEGACY_CODE_CONSTANTS: &[(&str, u16)] = &[
  ("INDEX_SIZE_ERR", 1),
  ("DOMSTRING_SIZE_ERR", 2),
  ("HIERARCHY_REQUEST_ERR", 3),
  ("WRONG_DOCUMENT_ERR", 4),
  ("INVALID_CHARACTER_ERR", 5),
  ("NO_DATA_ALLOWED_ERR", 6),
  ("NO_MODIFICATION_ALLOWED_ERR", 7),
  ("NOT_FOUND_ERR", 8),
  ("NOT_SUPPORTED_ERR", 9),
  ("INUSE_ATTRIBUTE_ERR", 10),
  ("INVALID_STATE_ERR", 11),
  ("SYNTAX_ERR", 12),
  ("INVALID_MODIFICATION_ERR", 13),
  ("NAMESPACE_ERR", 14),
  ("INVALID_ACCESS_ERR", 15),
  ("VALIDATION_ERR", 16),
  ("TYPE_MISMATCH_ERR", 17),
  ("SECURITY_ERR", 18),
  ("NETWORK_ERR", 19),
  ("ABORT_ERR", 20),
  ("URL_MISMATCH_ERR", 21),
  ("QUOTA_EXCEEDED_ERR", 22),
  ("TIMEOUT_ERR", 23),
  ("INVALID_NODE_TYPE_ERR", 24),
  ("DATA_CLONE_ERR", 25),
];

pub(crate) fn legacy_code_for_name(name: &str) -> u16 {
  match name {
    "IndexSizeError" => 1,
    "DOMStringSizeError" => 2,
    "HierarchyRequestError" => 3,
    "WrongDocumentError" => 4,
    "InvalidCharacterError" => 5,
    "NoDataAllowedError" => 6,
    "NoModificationAllowedError" => 7,
    "NotFoundError" => 8,
    "NotSupportedError" => 9,
    "InUseAttributeError" => 10,
    "InvalidStateError" => 11,
    "SyntaxError" => 12,
    "InvalidModificationError" => 13,
    "NamespaceError" => 14,
    "InvalidAccessError" => 15,
    "ValidationError" => 16,
    "TypeMismatchError" => 17,
    "SecurityError" => 18,
    "NetworkError" => 19,
    "AbortError" => 20,
    "URLMismatchError" => 21,
    "QuotaExceededError" => 22,
    "TimeoutError" => 23,
    // `dom2::DomError` historically used a shortened name here; accept both spellings.
    "InvalidNodeType" | "InvalidNodeTypeError" => 24,
    "DataCloneError" => 25,
    _ => 0,
  }
}

// We store DOMException `name` and `message` as JS strings; keep the handle type alias local so the
// above helpers stay readable.
#[allow(dead_code)]
type _DomExceptionStringHandle = GcString;

#[cfg(test)]
mod tests {
  use super::*;
  use vm_js::{PropertyKind, Value, VmError};
  use webidl_js_runtime::{VmJsRuntime, WebIdlBindingsRuntime};

  fn assert_str(rt: &mut VmJsRuntime, v: Value, expected: &str) -> Result<(), VmError> {
    let s = rt.string_to_utf8_lossy(v)?;
    assert_eq!(s, expected);
    Ok(())
  }

  #[test]
  fn dom_exception_constructs_and_has_code_and_constants() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::new();
    let global = <VmJsRuntime as WebIdlBindingsRuntime<()>>::global_object(&mut rt)?;

    let _class = DomExceptionClass::install(&mut rt, global)?;

    // Call the JS-level constructor: DOMException("m", "SyntaxError").
    let ctor_key = rt.property_key_from_str("DOMException")?;
    let ctor = rt.get(global, ctor_key)?;

    let msg = rt.alloc_string_value("m")?;
    let name = rt.alloc_string_value("SyntaxError")?;
    let obj = rt.call(ctor, Value::Undefined, &[msg, name])?;
    let Value::Object(obj_handle) = obj else {
      return Err(rt.throw_type_error("DOMException constructor did not return an object"));
    };

    // .name === "SyntaxError"
    let name_key = rt.property_key_from_str("name")?;
    let name_val = rt.get(obj, name_key)?;
    assert_str(&mut rt, name_val, "SyntaxError")?;

    // .message === "m"
    let message_key = rt.property_key_from_str("message")?;
    let message_val = rt.get(obj, message_key)?;
    assert_str(&mut rt, message_val, "m")?;

    // .code === 12
    let code_key = rt.property_key_from_str("code")?;
    let code_val = rt.get(obj, code_key)?;
    assert_eq!(code_val, Value::Number(12.0));

    // toString() === "SyntaxError: m"
    let to_string_key = rt.property_key_from_str("toString")?;
    let to_string_fn = rt.get(obj, to_string_key)?;
    let out = rt.call(to_string_fn, obj, &[])?;
    assert_str(&mut rt, out, "SyntaxError: m")?;

    // Own properties should be non-enumerable.
    let name_desc = rt
      .heap()
      .object_get_own_property(obj_handle, &name_key)?
      .expect("expected own name property");
    assert!(!name_desc.enumerable);

    let message_desc = rt
      .heap()
      .object_get_own_property(obj_handle, &message_key)?
      .expect("expected own message property");
    assert!(!message_desc.enumerable);

    let code_desc = rt
      .heap()
      .object_get_own_property(obj_handle, &code_key)?
      .expect("expected own code property");
    assert!(!code_desc.enumerable);

    // Constructor constants exist and use WebIDL const-like attributes.
    let Value::Object(ctor_obj) = ctor else {
      return Err(rt.throw_type_error("DOMException constructor is not an object"));
    };
    let syntax_err_key = rt.property_key_from_str("SYNTAX_ERR")?;
    let syntax_err_val = rt.get(ctor, syntax_err_key)?;
    assert_eq!(syntax_err_val, Value::Number(12.0));

    let desc = rt
      .heap()
      .object_get_own_property(ctor_obj, &syntax_err_key)?
      .expect("expected SYNTAX_ERR constant");
    assert!(desc.enumerable);
    assert!(!desc.configurable);
    match desc.kind {
      PropertyKind::Data { value, writable } => {
        assert!(!writable);
        assert_eq!(value, Value::Number(12.0));
      }
      _ => panic!("expected SYNTAX_ERR to be a data property"),
    }

    // Spot-check another legacy constant + name->code mapping.
    let data_clone_err_key = rt.property_key_from_str("DATA_CLONE_ERR")?;
    let data_clone_err_val = rt.get(ctor, data_clone_err_key)?;
    assert_eq!(data_clone_err_val, Value::Number(25.0));

    let msg2 = rt.alloc_string_value("m2")?;
    let name2 = rt.alloc_string_value("DataCloneError")?;
    let obj2 = rt.call(ctor, Value::Undefined, &[msg2, name2])?;
    let code2 = rt.get(obj2, code_key)?;
    assert_eq!(code2, Value::Number(25.0));

    // Default arguments should behave like WebIDL defaults: missing/undefined args apply defaults.
    let obj3 = rt.call(ctor, Value::Undefined, &[Value::Undefined, Value::Undefined])?;
    let name3 = rt.get(obj3, name_key)?;
    assert_str(&mut rt, name3, "Error")?;
    let message3 = rt.get(obj3, message_key)?;
    assert_str(&mut rt, message3, "")?;
    let code3 = rt.get(obj3, code_key)?;
    assert_eq!(code3, Value::Number(0.0));

    let to_string_fn3 = rt.get(obj3, to_string_key)?;
    let out3 = rt.call(to_string_fn3, obj3, &[])?;
    assert_str(&mut rt, out3, "Error")?;

    // `toString` should return `message` if `name` is the empty string.
    let empty = rt.alloc_string_value("")?;
    let obj4 = rt.call(ctor, Value::Undefined, &[msg, empty])?;
    let to_string_fn4 = rt.get(obj4, to_string_key)?;
    let out4 = rt.call(to_string_fn4, obj4, &[])?;
    assert_str(&mut rt, out4, "m")?;

    Ok(())
  }
}
