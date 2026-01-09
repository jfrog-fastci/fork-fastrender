//! WebIDL-driven JavaScript bindings.
//!
//! - [`generated`] contains generic WebIDL-to-host glue (calls into [`host`]).
//! - [`dom_generated`] contains a temporary `VmJsRuntime`-backed DOM scaffold used for early
//!   integration/testing.
//!
//! Alongside the generated scaffolding we keep a small set of handwritten helpers (e.g.
//! `DOMException`) that provide spec-shaped behaviour needed by early bindings/tests.

pub mod document;
pub mod dom_exception;
pub mod dom_generated;
pub mod generated;
pub mod host;
mod scaffold_selectors;

pub use document::install_document_query_selector_bindings;
pub use dom_exception::DomExceptionClass;
pub use dom_generated::install_dom_bindings as install_dom_bindings_generated;
pub use generated::install_window_bindings;
pub use host::{BindingValue, WebHostBindings};

/// Host-provided hooks for DOM bindings.
///
/// For the MVP DOM scaffold we only require a handle to the global object that bindings should be
/// installed onto. Future work will extend this trait to allocate real platform objects and wire
/// method bodies to DOM implementations.
pub trait DomHost {
  fn global_object(&mut self) -> vm_js::Value;
}

#[cfg(test)]
mod tests {
  use super::{install_window_bindings, BindingValue, WebHostBindings};
  use crate::js::{UrlLimits, UrlSearchParams};
  use std::collections::HashMap;
  use vm_js::{GcObject, PropertyKey, Value, VmError};
  use webidl_js_runtime::{
    JsRuntime as _, VmJsRuntime, WebIdlBindingsRuntime, WebIdlJsRuntime as _,
  };

  #[derive(Default)]
  struct UrlSearchParamsHost {
    limits: UrlLimits,
    params: HashMap<GcObject, UrlSearchParams>,
  }

  impl UrlSearchParamsHost {
    fn prototype_for(&mut self, rt: &mut VmJsRuntime, name: &str) -> Result<Value, VmError> {
      let global = <VmJsRuntime as WebIdlBindingsRuntime<Self>>::global_object(rt)?;
      let ctor_key: PropertyKey = rt.property_key_from_str(name)?;
      let ctor = rt.get(global, ctor_key)?;
      let proto_key: PropertyKey = rt.property_key_from_str("prototype")?;
      rt.get(ctor, proto_key)
    }

    fn require_params(
      &self,
      rt: &mut VmJsRuntime,
      receiver: Option<Value>,
    ) -> Result<&UrlSearchParams, VmError> {
      let Some(Value::Object(obj)) = receiver else {
        return Err(rt.throw_type_error("Illegal invocation"));
      };
      self
        .params
        .get(&obj)
        .ok_or_else(|| rt.throw_type_error("Illegal invocation"))
    }

    fn value_to_rust_string(rt: &mut VmJsRuntime, value: Value) -> Result<String, VmError> {
      let s = rt.to_string(value)?;
      rt.string_to_utf8_lossy(s)
    }
  }

  impl WebHostBindings<VmJsRuntime> for UrlSearchParamsHost {
    fn call_operation(
      &mut self,
      rt: &mut VmJsRuntime,
      receiver: Option<Value>,
      interface: &'static str,
      operation: &'static str,
      _overload: usize,
      args: Vec<BindingValue<Value>>,
    ) -> Result<BindingValue<Value>, VmError> {
      match (interface, operation) {
        ("URLSearchParams", "constructor") => {
          let init = match args.get(0) {
            None => String::new(),
            Some(BindingValue::String(s)) => s.clone(),
            Some(BindingValue::Object(v)) => Self::value_to_rust_string(rt, *v)?,
            Some(_) => String::new(),
          };

          let params = if init.is_empty() {
            UrlSearchParams::new(&self.limits)
          } else {
            UrlSearchParams::parse(&init, &self.limits).map_err(|e| {
              rt.throw_type_error(&format!("URLSearchParams constructor failed: {e}"))
            })?
          };

          let obj = rt.alloc_object_value()?;
          let proto = self.prototype_for(rt, "URLSearchParams")?;
          rt.set_prototype(obj, Some(proto))?;

          let Value::Object(obj_handle) = obj else {
            return Err(
              rt.throw_type_error("URLSearchParams constructor did not create an object"),
            );
          };
          let _ = rt.heap_mut().add_root(obj)?;
          self.params.insert(obj_handle, params);

          Ok(BindingValue::Object(obj))
        }
        ("URLSearchParams", "append") => {
          let params = self.require_params(rt, receiver)?;
          let Some(BindingValue::String(name)) = args.get(0) else {
            return Err(rt.throw_type_error("URLSearchParams.append: expected name"));
          };
          let Some(BindingValue::String(value)) = args.get(1) else {
            return Err(rt.throw_type_error("URLSearchParams.append: expected value"));
          };
          params
            .append(name, value)
            .map_err(|e| rt.throw_type_error(&format!("URLSearchParams.append failed: {e}")))?;
          Ok(BindingValue::Undefined)
        }
        ("URLSearchParams", "get") => {
          let params = self.require_params(rt, receiver)?;
          let Some(BindingValue::String(name)) = args.get(0) else {
            return Err(rt.throw_type_error("URLSearchParams.get: expected name"));
          };
          match params
            .get(name)
            .map_err(|e| rt.throw_type_error(&format!("URLSearchParams.get failed: {e}")))?
          {
            None => Ok(BindingValue::Null),
            Some(v) => Ok(BindingValue::String(v)),
          }
        }
        _ => Err(rt.throw_type_error("unimplemented host operation")),
      }
    }
  }

  #[test]
  fn generated_bindings_can_construct_and_use_url_search_params() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::new();
    let mut host = UrlSearchParamsHost::default();

    install_window_bindings(&mut rt, &mut host)?;

    let global =
      <VmJsRuntime as WebIdlBindingsRuntime<UrlSearchParamsHost>>::global_object(&mut rt)?;
    let ctor_key: PropertyKey = rt.property_key_from_str("URLSearchParams")?;
    let ctor = rt.get(global, ctor_key)?;
    let init = rt.alloc_string("?a=b")?;

    let params_obj =
      rt.with_host_context(&mut host, |rt| rt.call(ctor, rt.js_undefined(), &[init]))?;

    let append_key: PropertyKey = rt.property_key_from_str("append")?;
    let append = rt.get(params_obj, append_key)?;
    let a = rt.alloc_string("a")?;
    let c = rt.alloc_string("c")?;
    rt.with_host_context(&mut host, |rt| rt.call(append, params_obj, &[c, a]))?;

    let get_key: PropertyKey = rt.property_key_from_str("get")?;
    let get = rt.get(params_obj, get_key)?;
    let out = rt.with_host_context(&mut host, |rt| rt.call(get, params_obj, &[c]))?;

    let out_s = UrlSearchParamsHost::value_to_rust_string(&mut rt, out)?;
    assert_eq!(out_s, "a");
    Ok(())
  }
}

pub fn install_dom_bindings(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  host: &mut impl DomHost,
) -> Result<(), vm_js::VmError> {
  dom_generated::install_dom_bindings(rt, host)?;
  let global = host.global_object();
  let dom_exception = DomExceptionClass::install(rt, global)?;
  scaffold_selectors::install_scaffold_selector_bindings(rt, global, dom_exception)?;
  Ok(())
}
