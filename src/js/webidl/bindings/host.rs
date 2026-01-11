use std::collections::BTreeMap;

use crate::js::webidl::DataPropertyAttributes;
use crate::js::webidl::WebIdlBindingsRuntime;
use webidl_vm_js::CallbackHandle;

/// A minimally-typed value container used by the generated binding shims when crossing into the
/// host.
///
/// This is intentionally small: it is *not* a full JS value model. Objects are passed through as
/// opaque `JsValue` handles, while primitives/dictionaries are converted to Rust-owned values.
#[derive(Debug)]
pub enum BindingValue<JsValue: Copy> {
  Undefined,
  Null,
  Bool(bool),
  Number(f64),
  String(String),
  Object(JsValue),
  Callback(CallbackHandle),
  Sequence(Vec<BindingValue<JsValue>>),
  FrozenArray(Vec<BindingValue<JsValue>>),
  Dictionary(BTreeMap<String, BindingValue<JsValue>>),
}

/// Convert a host-facing [`BindingValue`] back into a JS value.
///
/// This is used by generated bindings for return values from [`WebHostBindings`].
pub fn binding_value_to_js<Host, R>(
  rt: &mut R,
  value: BindingValue<R::JsValue>,
) -> Result<R::JsValue, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  match value {
    BindingValue::Undefined => Ok(rt.js_undefined()),
    BindingValue::Null => Ok(rt.js_null()),
    BindingValue::Bool(b) => Ok(rt.js_bool(b)),
    BindingValue::Number(n) => Ok(rt.js_number(n)),
    BindingValue::String(s) => rt.js_string(&s),
    BindingValue::Object(v) => Ok(v),
    BindingValue::Callback(_) => Err(rt.throw_type_error("cannot return callback handles to JavaScript")),
    BindingValue::Sequence(values) | BindingValue::FrozenArray(values) => {
      let obj = rt.create_array(values.len())?;
      rt.with_stack_roots(&[obj], |rt| {
        for (idx, item) in values.into_iter().enumerate() {
          let key = idx.to_string();
          let value = binding_value_to_js::<Host, R>(rt, item)?;
          rt.define_data_property_str(obj, &key, value, DataPropertyAttributes::new(true, true, true))?;
        }
        Ok(obj)
      })
    }
    BindingValue::Dictionary(map) => {
      let obj = rt.create_object()?;
      rt.with_stack_roots(&[obj], |rt| {
        for (key, item) in map {
          let value = binding_value_to_js::<Host, R>(rt, item)?;
          rt.define_data_property_str(obj, &key, value, DataPropertyAttributes::new(true, true, true))?;
        }
        Ok(obj)
      })
    }
  }
}

/// Host-defined behavior implementation for WebIDL bindings.
///
/// The generated bindings are responsible for:
/// - overload resolution
/// - argument conversion
/// - return value conversion
///
/// The host is responsible for implementing the actual DOM/Web API behavior and for maintaining
/// any per-object state associated with `JsValue` handles.
pub trait WebHostBindings<R>: Sized
where
  R: WebIdlBindingsRuntime<Self>,
{
  fn call_operation(
    &mut self,
    rt: &mut R,
    receiver: Option<R::JsValue>,
    interface: &'static str,
    operation: &'static str,
    overload: usize,
    args: Vec<BindingValue<R::JsValue>>,
  ) -> Result<BindingValue<R::JsValue>, R::Error>;

  fn get_attribute(
    &mut self,
    rt: &mut R,
    receiver: Option<R::JsValue>,
    interface: &'static str,
    name: &'static str,
  ) -> Result<BindingValue<R::JsValue>, R::Error> {
    let _ = (receiver, interface, name);
    Err(rt.throw_type_error("unimplemented host attribute getter"))
  }

  fn set_attribute(
    &mut self,
    rt: &mut R,
    receiver: Option<R::JsValue>,
    interface: &'static str,
    name: &'static str,
    value: BindingValue<R::JsValue>,
  ) -> Result<(), R::Error> {
    let _ = (receiver, interface, name, value);
    Err(rt.throw_type_error("unimplemented host attribute setter"))
  }
}
