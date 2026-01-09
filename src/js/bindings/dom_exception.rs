use webidl_js_runtime::{JsRuntime as _, VmJsRuntime, WebIdlJsRuntime as _};
use crate::web::dom::DomException;
use vm_js::{GcString, PropertyKey, Value, VmError};

#[derive(Debug, Clone, Copy)]
pub struct DomExceptionClass {
  pub constructor: Value,
  pub prototype: Value,
}

impl DomExceptionClass {
  pub fn install(rt: &mut VmJsRuntime, global: Value) -> Result<Self, VmError> {
    let key_dom_exception = prop_key(rt, "DOMException")?;
    let key_name = prop_key(rt, "name")?;
    let key_message = prop_key(rt, "message")?;
    let key_to_string = prop_key(rt, "toString")?;
    let key_constructor = prop_key(rt, "constructor")?;
    let key_prototype = prop_key(rt, "prototype")?;

    let prototype = rt.alloc_object_value()?;

    // Minimal `DOMException.prototype.toString()`.
    let to_string_fn = rt.alloc_function_value(move |rt, this, _args| {
      if !rt.is_object(this) {
        return Err(rt.throw_type_error(
          "DOMException.prototype.toString called with non-object receiver",
        ));
      }

      let name = rt.get(this, key_name)?;
      let name = rt.to_string(name)?;
      let name = value_to_rust_string(rt, name)?;

      let message = rt.get(this, key_message)?;
      let message = rt.to_string(message)?;
      let message = value_to_rust_string(rt, message)?;

      let formatted = if message.is_empty() {
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
      let message = match args.get(0).copied() {
        Some(v) => rt.to_string(v)?,
        None => rt.alloc_string_value("")?,
      };
      let name = match args.get(1).copied() {
        Some(v) => rt.to_string(v)?,
        None => rt.alloc_string_value("Error")?,
      };

      let obj = rt.alloc_object_value()?;
      rt.set_prototype(obj, Some(proto_for_ctor))?;
      let name = message_or_name_string(rt, name)?;
      let message = message_or_name_string(rt, message)?;
      rt.define_data_property(obj, key_name, name, false)?;
      rt.define_data_property(obj, key_message, message, false)?;
      Ok(obj)
    })?;

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

    let obj = rt.alloc_object_value()?;
    rt.set_prototype(obj, Some(self.prototype))?;
    let name_value = rt.alloc_string_value(name)?;
    let message_value = rt.alloc_string_value(message)?;
    rt.define_data_property(obj, key_name, name_value, false)?;
    rt.define_data_property(obj, key_message, message_value, false)?;
    Ok(obj)
  }

  pub fn from_dom_exception(&self, rt: &mut VmJsRuntime, err: &DomException) -> Result<Value, VmError> {
    match err {
      DomException::SyntaxError { message } => self.new_instance(rt, message, "SyntaxError"),
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

fn value_to_rust_string(rt: &VmJsRuntime, value: Value) -> Result<String, VmError> {
  let Value::String(s) = value else {
    return Err(VmError::Unimplemented("expected string value"));
  };
  Ok(rt.heap().get_string(s)?.to_utf8_lossy())
}

// We store DOMException `name` and `message` as JS strings; keep the handle type alias local so the
// above helpers stay readable.
#[allow(dead_code)]
type _DomExceptionStringHandle = GcString;
