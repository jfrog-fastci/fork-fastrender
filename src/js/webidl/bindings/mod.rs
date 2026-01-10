//! WebIDL-driven JavaScript bindings.
//!
//! - [`generated`] contains `vm-js` realm WebIDL-to-host glue (calls into the embedder via
//!   [`webidl_vm_js::WebIdlBindingsHost`] + [`webidl_vm_js::host_from_hooks`]).
//! - [`generated_legacy`] contains legacy `webidl-js-runtime` bindings (kept temporarily for
//!   migration and for unit tests that exercise the old bindings/runtime surface).
//!
//! Alongside the generated scaffolding we keep a small set of handwritten helpers (e.g.
//! `DOMException`) that provide spec-shaped behaviour needed by early bindings/tests.

pub mod document;
pub mod dom_exception;
pub mod dom_exception_vmjs;
pub mod generated;
pub mod generated_legacy;
pub mod host;
pub use document::install_document_query_selector_bindings;
pub use dom_exception::DomExceptionClass;
pub use dom_exception_vmjs::{dom_exception_from_rust, throw_dom_exception, DomExceptionClassVmJs};
pub use generated::{install_window_bindings_vm_js, install_worker_bindings_vm_js};
pub use generated_legacy::{install_window_bindings, install_worker_bindings};
pub use host::{BindingValue, WebHostBindings};
pub use crate::js::vm_dom::{install_dom_bindings, install_dom_bindings_with_limits};

#[cfg(test)]
mod tests {
  use super::{
    install_window_bindings, install_window_bindings_vm_js, BindingValue, WebHostBindings,
  };
  use crate::js::{UrlLimits, UrlSearchParams};
  use crate::js::webidl::{
    InterfaceId, VmJsWebIdlBindingsCx, VmJsWebIdlBindingsState, WebIdlHooks, WebIdlLimits,
  };
  use crate::js::webidl_runtime_vmjs::WebIdlBindingsRuntime;
  use std::any::Any;
  use std::collections::HashMap;
  use vm_js::{
    Heap, HeapLimits, Job, MicrotaskQueue, PropertyKey, PropertyKind, Realm, Scope, Value, Vm,
    VmError, VmHostHooks, VmOptions, WeakGcObject,
  };
  use webidl_js_runtime::JsRuntime as _;
  use webidl_vm_js::{WebIdlBindingsHost, WebIdlBindingsHostSlot};

  struct HostHooksWithBindingsHost {
    slot: WebIdlBindingsHostSlot,
  }

  impl HostHooksWithBindingsHost {
    fn new(host: &mut dyn WebIdlBindingsHost) -> Self {
      Self {
        slot: WebIdlBindingsHostSlot::new(host),
      }
    }
  }

  impl VmHostHooks for HostHooksWithBindingsHost {
    fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<vm_js::RealmId>) {}

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
      Some(&mut self.slot)
    }
  }

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

  impl WebIdlBindingsHost for UrlSearchParamsHost {
    fn call_operation(
      &mut self,
      _vm: &mut Vm,
      scope: &mut Scope<'_>,
      receiver: Option<Value>,
      interface: &'static str,
      operation: &'static str,
      _overload: usize,
      args: &[Value],
    ) -> Result<Value, VmError> {
      match (interface, operation) {
        ("URLSearchParams", "constructor") => {
          let Some(Value::Object(obj)) = receiver else {
            return Err(VmError::InvariantViolation(
              "URLSearchParams constructor called without wrapper object receiver",
            ));
          };

          let init = match args.first().copied().unwrap_or(Value::Undefined) {
            Value::Undefined => String::new(),
            value => Self::value_to_rust_string(scope, value)?,
          };

          let params = if init.is_empty() {
            UrlSearchParams::new(&self.limits)
          } else {
            UrlSearchParams::parse(&init, &self.limits)
              .map_err(|_| VmError::TypeError("URLSearchParams constructor failed"))?
          };

          self.params.insert(WeakGcObject::from(obj), params);
          Ok(Value::Undefined)
        }
        ("URLSearchParams", "append") => {
          let params = self.require_params(receiver)?;
          let name = args.get(0).copied().unwrap_or(Value::Undefined);
          let value = args.get(1).copied().unwrap_or(Value::Undefined);
          let name = Self::value_to_rust_string(scope, name)?;
          let value = Self::value_to_rust_string(scope, value)?;
          params
            .append(&name, &value)
            .map_err(|_| VmError::TypeError("URLSearchParams.append failed"))?;
          Ok(Value::Undefined)
        }
        ("URLSearchParams", "get") => {
          let params = self.require_params(receiver)?;
          let name = args.get(0).copied().unwrap_or(Value::Undefined);
          let name = Self::value_to_rust_string(scope, name)?;
          match params
            .get(&name)
            .map_err(|_| VmError::TypeError("URLSearchParams.get failed"))?
          {
            None => Ok(Value::Null),
            Some(v) => Ok(Value::String(scope.alloc_string(&v)?)),
          }
        }
        _ => Err(VmError::TypeError("unimplemented host operation")),
      }
    }

    fn call_constructor(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _interface: &'static str,
      _overload: usize,
      _args: &[Value],
      _new_target: Value,
    ) -> Result<Value, VmError> {
      Err(VmError::TypeError("unimplemented host constructor"))
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

    let mut host = UrlSearchParamsHost::default();
    install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

    let mut hooks = HostHooksWithBindingsHost::new(&mut host);
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

    let params_val = vm.construct_with_host(&mut scope, &mut hooks, ctor, &[init], ctor)?;
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
    vm.call_with_host(&mut scope, &mut hooks, append, params_val, &[c, a])?;

    // params.get("c") === "a"
    let get_key = alloc_key(&mut scope, "get")?;
    let get = vm.get(&mut scope, params_obj, get_key)?;
    let out = vm.call_with_host(&mut scope, &mut hooks, get, params_val, &[c])?;

    let out_s = UrlSearchParamsHost::value_to_rust_string(&mut scope, out)?;
    assert_eq!(out_s, "a");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  struct NoHooks;

  impl WebIdlHooks<Value> for NoHooks {
    fn is_platform_object(&self, _value: Value) -> bool {
      false
    }

    fn implements_interface(&self, _value: Value, _interface: InterfaceId) -> bool {
      false
    }
  }

  #[derive(Default)]
  struct AlertHost {
    calls: Vec<usize>,
  }

  impl<'a> WebHostBindings<VmJsWebIdlBindingsCx<'a, AlertHost>> for AlertHost {
    fn call_operation(
      &mut self,
      _rt: &mut VmJsWebIdlBindingsCx<'a, AlertHost>,
      _receiver: Option<Value>,
      interface: &'static str,
      operation: &'static str,
      overload: usize,
      _args: Vec<BindingValue<Value>>,
    ) -> Result<BindingValue<Value>, VmError> {
      match (interface, operation) {
        ("Window", "alert") => {
          self.calls.push(overload);
          Ok(BindingValue::Undefined)
        }
        _ => Err(VmError::TypeError("unimplemented host operation")),
      }
    }
  }

  #[derive(Default)]
  struct AttributeAndConstHost {
    limits: UrlLimits,
    params: HashMap<WeakGcObject, UrlSearchParams>,
    urls: HashMap<WeakGcObject, String>,
    last_set_href: Option<String>,
  }

  impl AttributeAndConstHost {
    fn prototype_for<'a>(
      &mut self,
      rt: &mut VmJsWebIdlBindingsCx<'a, Self>,
      name: &str,
    ) -> Result<Value, VmError> {
      let global = rt.global_object()?;
      let ctor_key = rt.property_key(name)?;
      let ctor = rt.get(global, ctor_key)?;
      let proto_key = rt.property_key("prototype")?;
      rt.get(ctor, proto_key)
    }

    fn value_to_rust_string<'a>(
      rt: &mut VmJsWebIdlBindingsCx<'a, Self>,
      value: Value,
    ) -> Result<String, VmError> {
      let s = rt.to_string(value)?;
      rt.js_string_to_rust_string(s)
    }

    fn require_params<'a>(
      &self,
      rt: &mut VmJsWebIdlBindingsCx<'a, Self>,
      receiver: Option<Value>,
    ) -> Result<&UrlSearchParams, VmError> {
      let Some(Value::Object(obj)) = receiver else {
        return Err(rt.throw_type_error("Illegal invocation"));
      };
      self
        .params
        .get(&WeakGcObject::from(obj))
        .ok_or_else(|| rt.throw_type_error("Illegal invocation"))
    }

    fn require_url<'a>(
      &self,
      rt: &mut VmJsWebIdlBindingsCx<'a, Self>,
      receiver: Option<Value>,
    ) -> Result<&str, VmError> {
      let Some(Value::Object(obj)) = receiver else {
        return Err(rt.throw_type_error("Illegal invocation"));
      };
      self
        .urls
        .get(&WeakGcObject::from(obj))
        .map(String::as_str)
        .ok_or_else(|| rt.throw_type_error("Illegal invocation"))
    }
  }

  impl<'a> WebHostBindings<VmJsWebIdlBindingsCx<'a, AttributeAndConstHost>> for AttributeAndConstHost {
    fn call_operation(
      &mut self,
      rt: &mut VmJsWebIdlBindingsCx<'a, AttributeAndConstHost>,
      _receiver: Option<Value>,
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
            UrlSearchParams::parse(&init, &self.limits)
              .map_err(|_| rt.throw_type_error("URLSearchParams constructor failed"))?
          };

          // Root the prototype before allocating the wrapper object so it survives any GC triggered
          // during allocation.
          let proto = self.prototype_for(rt, "URLSearchParams")?;
          let obj = rt.create_object()?;
          rt.set_prototype(obj, Some(proto))?;

          let Value::Object(obj_handle) = obj else {
            return Err(rt.throw_type_error("URLSearchParams constructor did not create an object"));
          };
          self.params.insert(WeakGcObject::from(obj_handle), params);

          Ok(BindingValue::Object(obj))
        }
        ("URL", "constructor") => {
          let href = match args.get(0) {
            Some(BindingValue::String(s)) => s.clone(),
            Some(BindingValue::Object(v)) => Self::value_to_rust_string(rt, *v)?,
            _ => String::new(),
          };

          let proto = self.prototype_for(rt, "URL")?;
          let obj = rt.create_object()?;
          rt.set_prototype(obj, Some(proto))?;

          let Value::Object(obj_handle) = obj else {
            return Err(rt.throw_type_error("URL constructor did not create an object"));
          };
          self.urls.insert(WeakGcObject::from(obj_handle), href);

          Ok(BindingValue::Object(obj))
        }
        _ => Err(rt.throw_type_error("unimplemented host operation")),
      }
    }

    fn get_attribute(
      &mut self,
      rt: &mut VmJsWebIdlBindingsCx<'a, AttributeAndConstHost>,
      receiver: Option<Value>,
      interface: &'static str,
      name: &'static str,
    ) -> Result<BindingValue<Value>, VmError> {
      match (interface, name) {
        ("URLSearchParams", "size") => {
          let params = self.require_params(rt, receiver)?;
          let size = params
            .size()
            .map_err(|_| rt.throw_type_error("URLSearchParams.size failed"))?;
          Ok(BindingValue::Number(size as f64))
        }
        ("URL", "href") => {
          let href = self.require_url(rt, receiver)?;
          Ok(BindingValue::String(href.to_string()))
        }
        ("URL", "origin") => {
          let href = self.require_url(rt, receiver)?;
          // Minimal origin parsing for tests. This intentionally does not implement the full WHATWG
          // URL Standard: it only handles `scheme://host/...` inputs that appear in our binding
          // tests.
          let origin = if let Some(scheme_end) = href.find("://") {
            let after_scheme = scheme_end + "://".len();
            match href[after_scheme..].find('/') {
              Some(path_start) => &href[..after_scheme + path_start],
              None => href,
            }
          } else {
            href
          };
          Ok(BindingValue::String(origin.to_string()))
        }
        _ => Err(rt.throw_type_error("unimplemented host attribute getter")),
      }
    }

    fn set_attribute(
      &mut self,
      rt: &mut VmJsWebIdlBindingsCx<'a, AttributeAndConstHost>,
      receiver: Option<Value>,
      interface: &'static str,
      name: &'static str,
      value: BindingValue<Value>,
    ) -> Result<(), VmError> {
      match (interface, name) {
        ("URL", "href") => {
          let Some(Value::Object(obj)) = receiver else {
            return Err(rt.throw_type_error("Illegal invocation"));
          };
          let href = match value {
            BindingValue::String(s) => s,
            BindingValue::Object(v) => Self::value_to_rust_string(rt, v)?,
            _ => String::new(),
          };
          self.last_set_href = Some(href.clone());
          self.urls.insert(WeakGcObject::from(obj), href);
          Ok(())
        }
        _ => Err(rt.throw_type_error("unimplemented host attribute setter")),
      }
    }
  }

  #[test]
  fn generated_bindings_support_attributes_and_constants() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let state = Box::new(VmJsWebIdlBindingsState::<AttributeAndConstHost>::new(
      realm.global_object(),
      WebIdlLimits::default(),
      Box::new(NoHooks),
    ));

    let mut host = AttributeAndConstHost::default();
    {
      let mut rt = VmJsWebIdlBindingsCx::new(&mut vm, &mut heap, &state);
      install_window_bindings(&mut rt, &mut host)?;
    }

    let mut hooks = MicrotaskQueue::new();
    let mut scope = heap.scope();

    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    // --- Read a readonly attribute via the generated accessor getter ---
    let params_ctor_key = alloc_key(&mut scope, "URLSearchParams")?;
    let params_ctor = scope
      .heap()
      .object_get_own_data_property_value(global, &params_ctor_key)?
      .expect("globalThis.URLSearchParams should be defined");
    let Value::Object(params_ctor_obj) = params_ctor else {
      panic!("URLSearchParams constructor should be an object");
    };

    let init_str = scope.alloc_string("?a=b&c=d")?;
    scope.push_root(Value::String(init_str))?;
    let init = Value::String(init_str);

    let params_val = vm.construct_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      params_ctor,
      &[init],
      params_ctor,
    )?;
    scope.push_root(params_val)?;
    let Value::Object(_params_obj) = params_val else {
      panic!("URLSearchParams constructor should return an object");
    };

    let proto_key = alloc_key(&mut scope, "prototype")?;
    let params_proto_val = scope
      .heap()
      .object_get_own_data_property_value(params_ctor_obj, &proto_key)?
      .expect("URLSearchParams.prototype should be defined");
    scope.push_root(params_proto_val)?;
    let Value::Object(params_proto_obj) = params_proto_val else {
      panic!("URLSearchParams.prototype should be an object");
    };

    let size_key = alloc_key(&mut scope, "size")?;
    let Some(size_desc) = scope
      .heap()
      .object_get_own_property(params_proto_obj, &size_key)?
    else {
      panic!("missing URLSearchParams.prototype.size descriptor");
    };
    assert!(size_desc.enumerable);
    assert!(size_desc.configurable);
    let PropertyKind::Accessor { get, set } = size_desc.kind else {
      panic!("URLSearchParams.prototype.size is not an accessor property");
    };
    assert_eq!(set, Value::Undefined);
    let size_val = vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, get, params_val, &[])?;
    assert_eq!(size_val, Value::Number(2.0));

    // Calling the getter with an invalid receiver should throw a TypeError("Illegal invocation").
    {
      let err = vm
        .call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, get, Value::Undefined, &[])
        .expect_err("expected Illegal invocation error for URLSearchParams.prototype.size getter");
      let thrown = err
        .thrown_value()
        .expect("expected a thrown exception value");
      let Value::Object(thrown_obj) = thrown else {
        panic!("expected thrown error to be an object");
      };
      scope.push_root(thrown)?;
      let name_key = alloc_key(&mut scope, "name")?;
      let message_key = alloc_key(&mut scope, "message")?;
      let name_val = vm.get(&mut scope, thrown_obj, name_key)?;
      let message_val = vm.get(&mut scope, thrown_obj, message_key)?;
      let Value::String(name_s) = name_val else {
        panic!("expected error.name to be a string");
      };
      let Value::String(message_s) = message_val else {
        panic!("expected error.message to be a string");
      };
      assert_eq!(scope.heap().get_string(name_s)?.to_utf8_lossy(), "TypeError");
      assert_eq!(
        scope.heap().get_string(message_s)?.to_utf8_lossy(),
        "Illegal invocation"
      );
    }

    // --- Set a writable attribute via the generated accessor setter ---
    let url_ctor_key = alloc_key(&mut scope, "URL")?;
    let url_ctor = scope
      .heap()
      .object_get_own_data_property_value(global, &url_ctor_key)?
      .expect("globalThis.URL should be defined");
    let Value::Object(url_ctor_obj) = url_ctor else {
      panic!("URL constructor should be an object");
    };

    let url_arg_str = scope.alloc_string("https://example.test/")?;
    scope.push_root(Value::String(url_arg_str))?;
    let url_arg = Value::String(url_arg_str);

    let url_val = vm.construct_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      url_ctor,
      &[url_arg],
      url_ctor,
    )?;
    scope.push_root(url_val)?;
    let Value::Object(_url_obj) = url_val else {
      panic!("URL constructor should return an object");
    };

    let url_proto_val = scope
      .heap()
      .object_get_own_data_property_value(url_ctor_obj, &proto_key)?
      .expect("URL.prototype should be defined");
    scope.push_root(url_proto_val)?;
    let Value::Object(url_proto_obj) = url_proto_val else {
      panic!("URL.prototype should be an object");
    };

    let href_key = alloc_key(&mut scope, "href")?;
    let Some(href_desc) = scope
      .heap()
      .object_get_own_property(url_proto_obj, &href_key)?
    else {
      panic!("missing URL.prototype.href descriptor");
    };
    assert!(href_desc.enumerable);
    assert!(href_desc.configurable);
    let PropertyKind::Accessor { set, .. } = href_desc.kind else {
      panic!("URL.prototype.href is not an accessor property");
    };
    assert!(matches!(set, Value::Object(_)));

    // --- Read a readonly attribute with no setter ---
    let origin_key = alloc_key(&mut scope, "origin")?;
    let Some(origin_desc) = scope
      .heap()
      .object_get_own_property(url_proto_obj, &origin_key)?
    else {
      panic!("missing URL.prototype.origin descriptor");
    };
    assert!(origin_desc.enumerable);
    assert!(origin_desc.configurable);
    let PropertyKind::Accessor {
      get: origin_get,
      set: origin_set,
    } = origin_desc.kind
    else {
      panic!("URL.prototype.origin is not an accessor property");
    };
    assert_eq!(origin_set, Value::Undefined);
    let origin_val =
      vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, origin_get, url_val, &[])?;
    assert_eq!(
      UrlSearchParamsHost::value_to_rust_string(&mut scope, origin_val)?,
      "https://example.test"
    );

    let new_href_str = scope.alloc_string("https://changed.test/")?;
    scope.push_root(Value::String(new_href_str))?;
    let new_href = Value::String(new_href_str);
    vm.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      set,
      url_val,
      &[new_href],
    )?;
    assert_eq!(host.last_set_href.as_deref(), Some("https://changed.test/"));
    let origin_val =
      vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, origin_get, url_val, &[])?;
    assert_eq!(
      UrlSearchParamsHost::value_to_rust_string(&mut scope, origin_val)?,
      "https://changed.test"
    );

    // --- Read a constant defined on the interface object ---
    let node_key = alloc_key(&mut scope, "Node")?;
    let node_ctor = scope
      .heap()
      .object_get_own_data_property_value(global, &node_key)?
      .expect("globalThis.Node should be defined");
    let Value::Object(node_ctor_obj) = node_ctor else {
      panic!("Node constructor should be an object");
    };

    // Interfaces without a WebIDL constructor operation must still expose a constructable interface
    // object that throws for both `Node()` and `new Node()`.
    for err in [
      vm
        .call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, node_ctor, Value::Undefined, &[])
        .expect_err("expected Node() to throw"),
      vm
        .construct_with_host_and_hooks(&mut host, &mut scope, &mut hooks, node_ctor, &[], node_ctor)
        .expect_err("expected new Node() to throw"),
    ] {
      let thrown = err
        .thrown_value()
        .expect("expected a thrown exception value");
      let Value::Object(thrown_obj) = thrown else {
        panic!("expected thrown error to be an object");
      };
      scope.push_root(thrown)?;
      let name_key = alloc_key(&mut scope, "name")?;
      let message_key = alloc_key(&mut scope, "message")?;
      let name_val = vm.get(&mut scope, thrown_obj, name_key)?;
      let message_val = vm.get(&mut scope, thrown_obj, message_key)?;
      let Value::String(name_s) = name_val else {
        panic!("expected error.name to be a string");
      };
      let Value::String(message_s) = message_val else {
        panic!("expected error.message to be a string");
      };
      assert_eq!(scope.heap().get_string(name_s)?.to_utf8_lossy(), "TypeError");
      assert_eq!(
        scope.heap().get_string(message_s)?.to_utf8_lossy(),
        "Illegal constructor"
      );
    }

    let element_node_key = alloc_key(&mut scope, "ELEMENT_NODE")?;
    let Some(element_node_desc) = scope
      .heap()
      .object_get_own_property(node_ctor_obj, &element_node_key)?
    else {
      panic!("Node.ELEMENT_NODE should be defined");
    };
    assert!(element_node_desc.enumerable, "constants must be enumerable");
    assert!(
      !element_node_desc.configurable,
      "constants must be non-configurable"
    );
    let PropertyKind::Data { value, writable } = element_node_desc.kind else {
      panic!("Node.ELEMENT_NODE should be a data property");
    };
    assert_eq!(value, Value::Number(1.0));
    assert!(!writable, "constants must be non-writable");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[derive(Default)]
  struct GeneratedHost {
    calls: usize,
  }

  impl WebHostBindings<webidl_js_runtime::VmJsRuntime> for GeneratedHost {
    fn call_operation(
      &mut self,
      _rt: &mut webidl_js_runtime::VmJsRuntime,
      _receiver: Option<Value>,
      _interface: &'static str,
      _operation: &'static str,
      _overload: usize,
      _args: Vec<BindingValue<Value>>,
    ) -> Result<BindingValue<Value>, VmError> {
      self.calls += 1;
      Ok(BindingValue::Undefined)
    }
  }

  fn thrown_error_name(rt: &mut webidl_js_runtime::VmJsRuntime, err: VmError) -> Result<String, VmError> {
    let Some(thrown) = err.thrown_value() else {
      return Err(VmError::TypeError("expected thrown error"));
    };
    webidl_js_runtime::JsRuntime::with_stack_roots(rt, &[thrown], |rt| {
      let name_key: PropertyKey = rt.property_key_from_str("name")?;
      let name_value = webidl_js_runtime::JsRuntime::get(rt, thrown, name_key)?;
      webidl_js_runtime::JsRuntime::with_stack_roots(rt, &[name_value], |rt| {
        let s = webidl_js_runtime::JsRuntime::to_string(rt, name_value)?;
        rt.string_to_utf8_lossy(s)
      })
    })
  }

  #[test]
  fn generated_bindings_queue_microtask_rejects_non_callable() -> Result<(), VmError> {
    let mut rt = webidl_js_runtime::VmJsRuntime::new();
    let mut host = GeneratedHost::default();
    install_window_bindings(&mut rt, &mut host)?;

    let global = <webidl_js_runtime::VmJsRuntime as crate::js::webidl_runtime_vmjs::WebIdlBindingsRuntime<
      GeneratedHost,
    >>::global_object(&mut rt)?;
    webidl_js_runtime::JsRuntime::with_stack_roots(&mut rt, &[global], |rt| {
      let key = rt.property_key_from_str("queueMicrotask")?;
      let func = webidl_js_runtime::JsRuntime::get(rt, global, key)?;
      let err = rt
        .with_host_context(&mut host, |rt| {
          rt.call(func, webidl_js_runtime::JsRuntime::js_undefined(rt), &[Value::Number(1.0)])
        })
        .expect_err("expected queueMicrotask to throw on non-callable");
      assert_eq!(thrown_error_name(rt, err)?, "TypeError");
      assert_eq!(host.calls, 0, "host must not be called on conversion failure");
      Ok(())
    })
  }

  #[test]
  fn generated_bindings_add_event_listener_rejects_non_object_callback_interface() -> Result<(), VmError> {
    let mut rt = webidl_js_runtime::VmJsRuntime::new();
    let mut host = GeneratedHost::default();
    install_window_bindings(&mut rt, &mut host)?;

    let global = <webidl_js_runtime::VmJsRuntime as crate::js::webidl_runtime_vmjs::WebIdlBindingsRuntime<
      GeneratedHost,
    >>::global_object(&mut rt)?;
    webidl_js_runtime::JsRuntime::with_stack_roots(&mut rt, &[global], |rt| {
      // globalThis.EventTarget.prototype.addEventListener
      let ctor_key = rt.property_key_from_str("EventTarget")?;
      let ctor = webidl_js_runtime::JsRuntime::get(rt, global, ctor_key)?;
      let proto_key = rt.property_key_from_str("prototype")?;
      let proto = webidl_js_runtime::JsRuntime::get(rt, ctor, proto_key)?;
      let add_key = rt.property_key_from_str("addEventListener")?;
      let add = webidl_js_runtime::JsRuntime::get(rt, proto, add_key)?;

      let this_obj = rt.alloc_object_value()?;
      webidl_js_runtime::JsRuntime::with_stack_roots(rt, &[this_obj, add], |rt| {
        let type_str = rt.alloc_string_value("x")?;
        let err = rt
          .with_host_context(&mut host, |rt| {
            rt.call(add, this_obj, &[type_str, Value::Number(1.0)])
          })
          .expect_err("expected addEventListener to throw on non-object callback");
        assert_eq!(thrown_error_name(rt, err)?, "TypeError");
        assert_eq!(host.calls, 0, "host must not be called on conversion failure");
        Ok(())
      })
    })
  }

  #[test]
  fn generated_bindings_dispatch_window_alert_overloads() -> Result<(), VmError> {
    let limits = HeapLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let state = Box::new(VmJsWebIdlBindingsState::<AlertHost>::new(
      realm.global_object(),
      WebIdlLimits::default(),
      Box::new(NoHooks),
    ));

    let mut host = AlertHost::default();
    {
      let mut rt = VmJsWebIdlBindingsCx::new(&mut vm, &mut heap, &state);
      install_window_bindings(&mut rt, &mut host)?;
    }

    let mut hooks = MicrotaskQueue::new();
    let mut scope = heap.scope();

    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;
    let alert_key = alloc_key(&mut scope, "alert")?;
    let alert = scope
      .heap()
      .object_get_own_data_property_value(global, &alert_key)?
      .expect("globalThis.alert should be defined");

    // alert()
    vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, alert, Value::Undefined, &[])?;

    // alert("hi")
    let hi_str = scope.alloc_string("hi")?;
    scope.push_root(Value::String(hi_str))?;
    let hi = Value::String(hi_str);
    vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, alert, Value::Undefined, &[hi])?;

    // alert("a", "b") -> dispatch uses min(args.len(), maxarg) so should still pick overload #1.
    let a_str = scope.alloc_string("a")?;
    scope.push_root(Value::String(a_str))?;
    let b_str = scope.alloc_string("b")?;
    scope.push_root(Value::String(b_str))?;
    let a = Value::String(a_str);
    let b = Value::String(b_str);
    vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, alert, Value::Undefined, &[a, b])?;

    assert_eq!(host.calls, vec![0, 1, 1]);

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }
}
