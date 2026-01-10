//! WebIDL-driven JavaScript bindings.
//!
//! - [`generated`] contains generic WebIDL-to-host glue (calls into [`host`]).
//!
//! Alongside the generated scaffolding we keep a small set of handwritten helpers (e.g.
//! `DOMException`) that provide spec-shaped behaviour needed by early bindings/tests.

pub mod document;
pub mod dom_exception;
pub mod dom_exception_vmjs;
pub mod generated;
pub mod host;
mod vm_js_window;
pub use document::install_document_query_selector_bindings;
pub use dom_exception::DomExceptionClass;
pub use dom_exception_vmjs::{dom_exception_from_rust, throw_dom_exception, DomExceptionClassVmJs};
pub use generated::install_window_bindings;
pub use generated::install_worker_bindings;
pub use host::{BindingValue, VmJsBindingsHost, WebHostBindings};
pub use crate::js::vm_dom::{install_dom_bindings, install_dom_bindings_with_limits};
pub use vm_js_window::install_window_bindings as install_window_bindings_vm_js;

#[cfg(test)]
mod tests {
  use super::{install_window_bindings_vm_js, BindingValue, VmJsBindingsHost};
  use crate::js::{UrlLimits, UrlSearchParams};
  use std::collections::HashMap;
  use vm_js::{
    Heap, HeapLimits, MicrotaskQueue, PropertyKey, Realm, Scope, Value, Vm, VmError, VmOptions,
    WeakGcObject,
  };

  #[derive(Default)]
  struct UrlSearchParamsHost {
    limits: UrlLimits,
    params: HashMap<WeakGcObject, UrlSearchParams>,
  }

  impl UrlSearchParamsHost {
    fn require_params(&self, receiver: Option<Value>) -> Result<&UrlSearchParams, VmError> {
      let Some(Value::Object(obj)) = receiver else {
        return Err(VmError::TypeError("Illegal invocation"));
      };
      self
        .params
        .get(&WeakGcObject::from(obj))
        .ok_or(VmError::TypeError("Illegal invocation"))
    }

    fn value_to_rust_string(scope: &mut Scope<'_>, value: Value) -> Result<String, VmError> {
      let s = scope.heap_mut().to_string(value)?;
      Ok(scope.heap().get_string(s)?.to_utf8_lossy())
    }
  }

  impl VmJsBindingsHost for UrlSearchParamsHost {
    fn call_operation(
      &mut self,
      scope: &mut Scope<'_>,
      receiver: Option<Value>,
      interface: &'static str,
      operation: &'static str,
      _overload: usize,
      args: Vec<BindingValue<Value>>,
    ) -> Result<BindingValue<Value>, VmError> {
      match (interface, operation) {
        ("URLSearchParams", "constructor") => {
          let Some(Value::Object(obj)) = receiver else {
            return Err(VmError::InvariantViolation(
              "URLSearchParams constructor called without wrapper object receiver",
            ));
          };

          let init = match args.get(0) {
            None => String::new(),
            Some(BindingValue::String(s)) => s.clone(),
            Some(BindingValue::Object(v)) => Self::value_to_rust_string(scope, *v)?,
            Some(_) => String::new(),
          };

          let params = if init.is_empty() {
            UrlSearchParams::new(&self.limits)
          } else {
            UrlSearchParams::parse(&init, &self.limits)
              .map_err(|_| VmError::TypeError("URLSearchParams constructor failed"))?
          };

          self.params.insert(WeakGcObject::from(obj), params);
          Ok(BindingValue::Undefined)
        }
        ("URLSearchParams", "append") => {
          let params = self.require_params(receiver)?;
          let Some(BindingValue::String(name)) = args.get(0) else {
            return Err(VmError::TypeError("URLSearchParams.append: expected name"));
          };
          let Some(BindingValue::String(value)) = args.get(1) else {
            return Err(VmError::TypeError("URLSearchParams.append: expected value"));
          };
          params
            .append(name, value)
            .map_err(|_| VmError::TypeError("URLSearchParams.append failed"))?;
          Ok(BindingValue::Undefined)
        }
        ("URLSearchParams", "get") => {
          let params = self.require_params(receiver)?;
          let Some(BindingValue::String(name)) = args.get(0) else {
            return Err(VmError::TypeError("URLSearchParams.get: expected name"));
          };
          match params
            .get(name)
            .map_err(|_| VmError::TypeError("URLSearchParams.get failed"))?
          {
            None => Ok(BindingValue::Null),
            Some(v) => Ok(BindingValue::String(v)),
          }
        }
        _ => Err(VmError::TypeError("unimplemented host operation")),
      }
    }
  }

  fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
    let s = scope.alloc_string(name)?;
    scope.push_root(Value::String(s))?;
    Ok(PropertyKey::from_string(s))
  }

  #[test]
  fn generated_bindings_can_construct_and_use_url_search_params() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    install_window_bindings_vm_js::<UrlSearchParamsHost>(&mut vm, &mut heap, &realm)?;

    let mut host = UrlSearchParamsHost::default();
    let mut hooks = MicrotaskQueue::new();
    let mut scope = heap.scope();

    // globalThis.URLSearchParams
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;
    let ctor_key = alloc_key(&mut scope, "URLSearchParams")?;
    let ctor = scope
      .heap()
      .object_get_own_data_property_value(global, &ctor_key)?
      .expect("globalThis.URLSearchParams should be defined");

    // new URLSearchParams("?a=b")
    let init_str = scope.alloc_string("?a=b")?;
    scope.push_root(Value::String(init_str))?;
    let init = Value::String(init_str);

    let params_val =
      vm.construct_with_host_and_hooks(&mut host, &mut scope, &mut hooks, ctor, &[init], ctor)?;
    scope.push_root(params_val)?;
    let Value::Object(params_obj) = params_val else {
      panic!("URLSearchParams constructor should return an object");
    };

    // params.append("c", "a")
    let append_key = alloc_key(&mut scope, "append")?;
    let append = vm.get(&mut scope, params_obj, append_key)?;
    let c_str = scope.alloc_string("c")?;
    scope.push_root(Value::String(c_str))?;
    let a_str = scope.alloc_string("a")?;
    scope.push_root(Value::String(a_str))?;
    let c = Value::String(c_str);
    let a = Value::String(a_str);
    vm.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      append,
      params_val,
      &[c, a],
    )?;

    // params.get("c") === "a"
    let get_key = alloc_key(&mut scope, "get")?;
    let get = vm.get(&mut scope, params_obj, get_key)?;
    let out =
      vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, get, params_val, &[c])?;

    let out_s = UrlSearchParamsHost::value_to_rust_string(&mut scope, out)?;
    assert_eq!(out_s, "a");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }
}
