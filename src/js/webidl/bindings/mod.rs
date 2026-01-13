//! WebIDL-driven JavaScript bindings.
//!
//! - [`generated`] contains `vm-js` realm WebIDL-to-host glue (calls into the embedder via
//!   [`webidl_vm_js::WebIdlBindingsHost`] + [`webidl_vm_js::host_from_hooks`]).
//! - [`generated_legacy`] contains legacy heap-only bindings (backed by `webidl_js_runtime`, kept
//!   temporarily for
//!   migration and for unit tests that exercise the old bindings/runtime surface).
//! - The `vm-js` backend emits **per-interface installers** (e.g. [`install_url_bindings_vm_js`]) so
//!   embedders can incrementally adopt generated bindings without clobbering legacy DOM globals.
//!   Installers are **non-clobbering**: if the target global already exists on `globalThis`, the
//!   generated installer returns `Ok(())` and leaves the existing binding in place. For interfaces
//!   with prototype inheritance, re-running the installer may **patch the prototype chain** of a
//!   previously-generated constructor (detected via an internal marker) without replacing the
//!   global.
//!
//! Alongside the generated scaffolding we keep a small set of handwritten helpers (e.g.
//! `DOMException`) that provide spec-shaped behaviour needed by early bindings/tests.

pub mod document;
pub mod dom_exception;
pub mod dom_exception_vmjs;
pub mod shadow_root;
pub mod generated;
pub mod generated_legacy;
pub mod host;
pub use crate::js::vm_dom::{install_dom_bindings, install_dom_bindings_with_limits};
pub use document::install_document_query_selector_bindings;
pub use dom_exception::DomExceptionClass;
pub use dom_exception_vmjs::{dom_exception_from_rust, throw_dom_exception, DomExceptionClassVmJs};
pub use shadow_root::install_shadow_root_bindings_vm_js;
pub use generated::{
  install_character_data_bindings_vm_js, install_document_bindings_vm_js,
  install_document_fragment_bindings_vm_js, install_element_bindings_vm_js,
  install_custom_event_bindings_vm_js, install_event_bindings_vm_js,
  install_dom_token_list_bindings_vm_js, install_event_target_bindings_vm_js,
  install_html_collection_bindings_vm_js, install_node_bindings_vm_js,
  install_node_filter_bindings_vm_js, install_node_iterator_bindings_vm_js,
  install_node_list_bindings_vm_js, install_text_bindings_vm_js,
  install_tree_walker_bindings_vm_js, install_url_bindings_vm_js, install_url_search_params_bindings_vm_js,
  install_window_bindings_vm_js, install_window_ops_bindings_vm_js, install_worker_bindings_vm_js,
};
pub use generated_legacy::{install_window_bindings, install_worker_bindings};
pub use host::{binding_value_to_js, BindingValue, WebHostBindings};

#[cfg(test)]
#[allow(unused_imports)]
mod webidl_bindings_codegen_toy_generated_vmjs {
  include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/xtask/tests/goldens/webidl_bindings_codegen_expected_vmjs.rs"
  ));
}

#[cfg(test)]
mod legacy_vmjs_generated_tests;
#[cfg(test)]
mod regression_tests;
#[cfg(test)]
mod webidl_union_record_tests;
#[cfg(test)]
mod window_host_installer_tests;
#[cfg(test)]
mod to_string_tag_tests;

#[cfg(test)]
mod tests {
  use super::{
    install_window_bindings, install_window_bindings_vm_js, BindingValue, WebHostBindings,
  };
  use crate::js::webidl::{
    InterfaceId, VmJsWebIdlBindingsCx, VmJsWebIdlBindingsState, WebIdlHooks, WebIdlLimits,
  };
  use crate::js::webidl_runtime_vmjs::WebIdlBindingsRuntime;
  use crate::js::{UrlLimits, UrlSearchParams};
  use std::any::Any;
  use std::collections::HashMap;
  use vm_js::{
    GcObject, Heap, HeapLimits, Job, MicrotaskQueue, PropertyKey, PropertyKind, Realm, Scope,
    Value, Vm, VmError, VmHost, VmHostHooks, VmOptions, WeakGcObject,
  };
  use webidl_js_runtime::JsRuntime as _;
  use webidl_vm_js::{
    IterableKind, VmJsHostHooksPayload, WebIdlBindingsHost, WebIdlBindingsHostSlot,
  };

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

  struct HostHooksWithVmJsPayload {
    microtasks: MicrotaskQueue,
    payload: VmJsHostHooksPayload,
  }

  impl HostHooksWithVmJsPayload {
    fn new<Host: Any>(host: &mut Host) -> Self {
      let mut payload = VmJsHostHooksPayload::default();
      payload.set_embedder_state(host);
      Self {
        microtasks: MicrotaskQueue::new(),
        payload,
      }
    }
  }

  impl VmHostHooks for HostHooksWithVmJsPayload {
    fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<vm_js::RealmId>) {
      self.microtasks.enqueue_promise_job(job, realm);
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
      Some(&mut self.payload)
    }
  }

  #[derive(Default)]
  struct ConstructorDispatchHost {
    last_receiver: Option<Value>,
    last_interface: Option<&'static str>,
    last_member: Option<&'static str>,
    last_overload: Option<usize>,
    last_args: Vec<Value>,
  }

  impl WebIdlBindingsHost for ConstructorDispatchHost {
    fn call_operation(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      receiver: Option<Value>,
      interface: &'static str,
      operation: &'static str,
      overload: usize,
      args: &[Value],
    ) -> Result<Value, VmError> {
      self.last_receiver = receiver;
      self.last_interface = Some(interface);
      self.last_member = Some(operation);
      self.last_overload = Some(overload);
      self.last_args.clear();
      self.last_args.extend_from_slice(args);
      Ok(Value::Undefined)
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
      Err(VmError::Unimplemented("unimplemented host constructor"))
    }
  }

  fn alloc_key(scope: &mut vm_js::Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
    let s = scope.alloc_string(name)?;
    scope.push_root(Value::String(s))?;
    Ok(PropertyKey::from_string(s))
  }

  #[test]
  fn vmjs_bindings_constructor_uses_native_construct_and_installs_prototype_links(
  ) -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    // Install the generated vm-js bindings into the realm.
    install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

    let global = realm.global_object();
    let intr = *realm.intrinsics();

    let mut host_impl = ConstructorDispatchHost::default();
    let mut hooks = HostHooksWithBindingsHost::new(&mut host_impl);
    let mut dummy_host = ();
    let mut scope = heap.scope();

    // globalThis.URLSearchParams
    scope.push_root(Value::Object(global))?;
    let ctor_key = alloc_key(&mut scope, "URLSearchParams")?;
    let ctor = scope
      .heap()
      .object_get_own_data_property_value(global, &ctor_key)?
      .expect("globalThis.URLSearchParams should be defined");
    scope.push_root(ctor)?;

    let Value::Object(ctor_obj) = ctor else {
      return Err(VmError::TypeError(
        "URLSearchParams constructor should be an object",
      ));
    };

    // URLSearchParams.prototype
    let proto_key = alloc_key(&mut scope, "prototype")?;
    let proto = vm.get(&mut scope, ctor_obj, proto_key)?;
    let Value::Object(proto_obj) = proto else {
      return Err(VmError::TypeError(
        "URLSearchParams.prototype should be an object",
      ));
    };
    scope.push_root(Value::Object(proto_obj))?;

    // `.prototype` and `.constructor` links should be installed with spec-like attributes.
    let proto_desc = scope
      .heap()
      .object_get_own_property(ctor_obj, &proto_key)?
      .ok_or(VmError::TypeError(
        "missing URLSearchParams.prototype data property",
      ))?;
    assert!(!proto_desc.enumerable);
    assert!(!proto_desc.configurable);
    match proto_desc.kind {
      PropertyKind::Data { value, writable } => {
        assert!(!writable);
        assert_eq!(value, Value::Object(proto_obj));
      }
      _ => {
        return Err(VmError::TypeError(
          "URLSearchParams.prototype should be a data property",
        ))
      }
    }

    let ctor_prop_key = alloc_key(&mut scope, "constructor")?;
    let ctor_desc = scope
      .heap()
      .object_get_own_property(proto_obj, &ctor_prop_key)?
      .ok_or(VmError::TypeError(
        "missing URLSearchParams.prototype.constructor data property",
      ))?;
    assert!(!ctor_desc.enumerable);
    assert!(!ctor_desc.configurable);
    match ctor_desc.kind {
      PropertyKind::Data { value, writable } => {
        assert!(!writable);
        assert_eq!(value, Value::Object(ctor_obj));
      }
      _ => {
        return Err(VmError::TypeError(
          "URLSearchParams.prototype.constructor should be a data property",
        ))
      }
    }

    // new URLSearchParams("a=1") returns an object whose [[Prototype]] is URLSearchParams.prototype.
    let init_str = scope.alloc_string("a=1")?;
    scope.push_root(Value::String(init_str))?;
    let init = Value::String(init_str);

    let params_val = vm.construct_with_host_and_hooks(
      &mut dummy_host,
      &mut scope,
      &mut hooks,
      ctor,
      &[init],
      ctor,
    )?;
    scope.push_root(params_val)?;
    let Value::Object(params_obj) = params_val else {
      return Err(VmError::TypeError(
        "URLSearchParams constructor should return an object",
      ));
    };
    assert_eq!(scope.object_get_prototype(params_obj)?, Some(proto_obj));

    // Constructor dispatch should have been observed by the host with the wrapper object receiver.
    assert_eq!(host_impl.last_interface, Some("URLSearchParams"));
    assert_eq!(host_impl.last_member, Some("constructor"));
    assert_eq!(host_impl.last_receiver, Some(Value::Object(params_obj)));

    // Calling without `new` throws a TypeError.
    let err = vm
      .call_with_host_and_hooks(
        &mut dummy_host,
        &mut scope,
        &mut hooks,
        ctor,
        Value::Undefined,
        &[init],
      )
      .expect_err("expected calling URLSearchParams() without new to throw");

    match err {
      VmError::TypeError(_) => {}
      VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
        let Value::Object(obj) = value else {
          return Err(VmError::TypeError("expected thrown TypeError object"));
        };
        scope.push_root(Value::Object(obj))?;
        assert_eq!(
          scope.object_get_prototype(obj)?,
          Some(intr.type_error_prototype())
        );
      }
      other => {
        return Err(VmError::TypeError(match other.thrown_value() {
          Some(_) => "unexpected thrown error type (not TypeError)",
          None => "expected TypeError",
        }));
      }
    }

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn vmjs_bindings_event_target_constructor_passes_parent_arg_to_host() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

    let global = realm.global_object();

    let mut host_impl = ConstructorDispatchHost::default();
    let mut hooks = HostHooksWithBindingsHost::new(&mut host_impl);
    let mut dummy_host = ();
    let mut scope = heap.scope();

    scope.push_root(Value::Object(global))?;
    let ctor_key = alloc_key(&mut scope, "EventTarget")?;
    let ctor = scope
      .heap()
      .object_get_own_data_property_value(global, &ctor_key)?
      .expect("globalThis.EventTarget should be defined");
    scope.push_root(ctor)?;

    // Create a parent EventTarget instance.
    let parent = vm.construct_with_host_and_hooks(
      &mut dummy_host,
      &mut scope,
      &mut hooks,
      ctor,
      &[],
      ctor,
    )?;
    scope.push_root(parent)?;

    // Create a child EventTarget with an explicit parent.
    let _child = vm.construct_with_host_and_hooks(
      &mut dummy_host,
      &mut scope,
      &mut hooks,
      ctor,
      &[parent],
      ctor,
    )?;

    assert_eq!(host_impl.last_interface, Some("EventTarget"));
    assert_eq!(host_impl.last_member, Some("constructor"));
    assert_eq!(host_impl.last_overload, Some(1));
    assert_eq!(host_impl.last_args.get(0).copied(), Some(parent));

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn vmjs_bindings_install_dom_constructor_prototype_chains() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

    let global = realm.global_object();

    let mut scope = heap.scope();
    scope.push_root(Value::Object(global))?;

    let mut get_ctor = |name: &str| -> Result<GcObject, VmError> {
      let ctor_key = alloc_key(&mut scope, name)?;
      let ctor = scope
        .heap()
        .object_get_own_data_property_value(global, &ctor_key)?
        .ok_or(VmError::TypeError("missing global constructor"))?;
      scope.push_root(ctor)?;
      let Value::Object(obj) = ctor else {
        return Err(VmError::TypeError("global constructor should be an object"));
      };
      Ok(obj)
    };

    let event_target_ctor = get_ctor("EventTarget")?;
    let node_ctor = get_ctor("Node")?;
    let character_data_ctor = get_ctor("CharacterData")?;
    let text_ctor = get_ctor("Text")?;
    let document_ctor = get_ctor("Document")?;
    let element_ctor = get_ctor("Element")?;
    let document_fragment_ctor = get_ctor("DocumentFragment")?;

    let proto_key = alloc_key(&mut scope, "prototype")?;

    let event_target_proto = vm.get(&mut scope, event_target_ctor, proto_key)?;
    let node_proto = vm.get(&mut scope, node_ctor, proto_key)?;
    let character_data_proto = vm.get(&mut scope, character_data_ctor, proto_key)?;
    let text_proto = vm.get(&mut scope, text_ctor, proto_key)?;
    let document_proto = vm.get(&mut scope, document_ctor, proto_key)?;
    let element_proto = vm.get(&mut scope, element_ctor, proto_key)?;
    let document_fragment_proto = vm.get(&mut scope, document_fragment_ctor, proto_key)?;

    let Value::Object(event_target_proto_obj) = event_target_proto else {
      return Err(VmError::TypeError("EventTarget.prototype should be an object"));
    };
    let Value::Object(node_proto_obj) = node_proto else {
      return Err(VmError::TypeError("Node.prototype should be an object"));
    };
    let Value::Object(character_data_proto_obj) = character_data_proto else {
      return Err(VmError::TypeError("CharacterData.prototype should be an object"));
    };
    let Value::Object(text_proto_obj) = text_proto else {
      return Err(VmError::TypeError("Text.prototype should be an object"));
    };
    let Value::Object(document_proto_obj) = document_proto else {
      return Err(VmError::TypeError("Document.prototype should be an object"));
    };
    let Value::Object(element_proto_obj) = element_proto else {
      return Err(VmError::TypeError("Element.prototype should be an object"));
    };
    let Value::Object(document_fragment_proto_obj) = document_fragment_proto else {
      return Err(VmError::TypeError(
        "DocumentFragment.prototype should be an object",
      ));
    };

    assert_eq!(
      scope.object_get_prototype(node_proto_obj)?,
      Some(event_target_proto_obj),
      "Node.prototype should inherit from EventTarget.prototype",
    );
    assert_eq!(
      scope.object_get_prototype(character_data_proto_obj)?,
      Some(node_proto_obj),
      "CharacterData.prototype should inherit from Node.prototype",
    );
    assert_eq!(
      scope.object_get_prototype(text_proto_obj)?,
      Some(character_data_proto_obj),
      "Text.prototype should inherit from CharacterData.prototype",
    );
    assert_eq!(
      scope.object_get_prototype(document_proto_obj)?,
      Some(node_proto_obj),
      "Document.prototype should inherit from Node.prototype",
    );
    assert_eq!(
      scope.object_get_prototype(element_proto_obj)?,
      Some(node_proto_obj),
      "Element.prototype should inherit from Node.prototype",
    );
    assert_eq!(
      scope.object_get_prototype(document_fragment_proto_obj)?,
      Some(node_proto_obj),
      "DocumentFragment.prototype should inherit from Node.prototype",
    );

    // `.prototype` and `.constructor` links should be installed with spec-like attributes.
    let proto_desc = scope
      .heap()
      .object_get_own_property(text_ctor, &proto_key)?
      .ok_or(VmError::TypeError("missing Text.prototype data property"))?;
    assert!(!proto_desc.enumerable);
    assert!(!proto_desc.configurable);
    match proto_desc.kind {
      PropertyKind::Data { value, writable } => {
        assert!(!writable);
        assert_eq!(value, Value::Object(text_proto_obj));
      }
      _ => return Err(VmError::TypeError("Text.prototype should be a data property")),
    }

    let ctor_prop_key = alloc_key(&mut scope, "constructor")?;
    let ctor_desc = scope
      .heap()
      .object_get_own_property(text_proto_obj, &ctor_prop_key)?
      .ok_or(VmError::TypeError(
        "missing Text.prototype.constructor data property",
      ))?;
    assert!(!ctor_desc.enumerable);
    assert!(!ctor_desc.configurable);
    match ctor_desc.kind {
      PropertyKind::Data { value, writable } => {
        assert!(!writable);
        assert_eq!(value, Value::Object(text_ctor));
      }
      _ => {
        return Err(VmError::TypeError(
          "Text.prototype.constructor should be a data property",
        ))
      }
    }

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn vmjs_bindings_constructor_uses_new_target_prototype() -> Result<(), VmError> {
    fn dummy_call(
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _host: &mut dyn VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: GcObject,
      _this: Value,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      Ok(Value::Undefined)
    }

    fn dummy_construct(
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _host: &mut dyn VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: GcObject,
      _args: &[Value],
      _new_target: Value,
    ) -> Result<Value, VmError> {
      Ok(Value::Undefined)
    }

    let mut heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

    let global = realm.global_object();
    let intr = *realm.intrinsics();

    let mut host_impl = ConstructorDispatchHost::default();
    let mut hooks = HostHooksWithBindingsHost::new(&mut host_impl);
    let mut dummy_host = ();
    let mut scope = heap.scope();

    scope.push_root(Value::Object(global))?;
    let ctor_key = alloc_key(&mut scope, "URLSearchParams")?;
    let ctor = scope
      .heap()
      .object_get_own_data_property_value(global, &ctor_key)?
      .expect("globalThis.URLSearchParams should be defined");
    scope.push_root(ctor)?;

    // Use a custom prototype to ensure we observe `new_target` handling.
    let custom_proto = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
    scope.push_root(Value::Object(custom_proto))?;

    let call_id = vm.register_native_call(dummy_call)?;
    let construct_id = vm.register_native_construct(dummy_construct)?;

    let new_target_name = scope.alloc_string("SubURLSearchParams")?;
    scope.push_root(Value::String(new_target_name))?;
    let new_target_obj =
      scope.alloc_native_function(call_id, Some(construct_id), new_target_name, 0)?;
    scope.push_root(Value::Object(new_target_obj))?;
    scope
      .heap_mut()
      .object_set_prototype(new_target_obj, Some(intr.function_prototype()))?;

    // Set `new_target.prototype` so `GetPrototypeFromConstructor` resolves to our custom object.
    let proto_key = alloc_key(&mut scope, "prototype")?;
    scope.define_property(
      new_target_obj,
      proto_key,
      vm_js::PropertyDescriptor {
        enumerable: false,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Object(custom_proto),
          writable: false,
        },
      },
    )?;

    let params_val = vm.construct_with_host_and_hooks(
      &mut dummy_host,
      &mut scope,
      &mut hooks,
      ctor,
      &[],
      Value::Object(new_target_obj),
    )?;
    scope.push_root(params_val)?;
    let Value::Object(params_obj) = params_val else {
      return Err(VmError::TypeError(
        "URLSearchParams constructor should return an object",
      ));
    };
    assert_eq!(
      scope.object_get_prototype(params_obj)?,
      Some(custom_proto),
      "expected constructed wrapper to use new_target.prototype",
    );

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
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

    fn init_to_params(
      &self,
      scope: &mut Scope<'_>,
      init: Value,
    ) -> Result<UrlSearchParams, VmError> {
      match init {
        Value::Undefined => Ok(UrlSearchParams::new(&self.limits)),
        Value::String(_) => {
          let init = Self::value_to_rust_string(scope, init)?;
          if init.is_empty() {
            Ok(UrlSearchParams::new(&self.limits))
          } else {
            UrlSearchParams::parse(&init, &self.limits)
              .map_err(|_| VmError::TypeError("URLSearchParams constructor failed"))
          }
        }
        Value::Object(obj) => {
          if scope.heap().object_is_array(obj)? {
            // Canonicalized `sequence<sequence<USVString>>` from the vm-js bindings.
            let length_key_s = scope.alloc_string("length")?;
            scope.push_root(Value::String(length_key_s))?;
            let length_key = PropertyKey::from_string(length_key_s);
            let length = match scope.heap().get(obj, &length_key)? {
              Value::Number(n) if n.is_finite() && n >= 0.0 => n as u32,
              _ => {
                return Err(VmError::TypeError(
                  "URLSearchParams init array has invalid length",
                ))
              }
            };

            let params = UrlSearchParams::new(&self.limits);
            for idx in 0..length {
              let key_s = scope.alloc_string(&idx.to_string())?;
              scope.push_root(Value::String(key_s))?;
              let key = PropertyKey::from_string(key_s);
              let entry = scope.heap().get(obj, &key)?;
              let Value::Object(pair_obj) = entry else {
                return Err(VmError::TypeError(
                  "URLSearchParams init sequence element is not an object",
                ));
              };
              if !scope.heap().object_is_array(pair_obj)? {
                return Err(VmError::TypeError(
                  "URLSearchParams init pair is not an array",
                ));
              }

              let name_key_s = scope.alloc_string("0")?;
              scope.push_root(Value::String(name_key_s))?;
              let value_key_s = scope.alloc_string("1")?;
              scope.push_root(Value::String(value_key_s))?;
              let name_key = PropertyKey::from_string(name_key_s);
              let value_key = PropertyKey::from_string(value_key_s);
              let name = scope.heap().get(pair_obj, &name_key)?;
              let value = scope.heap().get(pair_obj, &value_key)?;
              let name = Self::value_to_rust_string(scope, name)?;
              let value = Self::value_to_rust_string(scope, value)?;
              params
                .append(&name, &value)
                .map_err(|_| VmError::TypeError("URLSearchParams constructor failed"))?;
            }
            Ok(params)
          } else {
            // Canonicalized `record<USVString, USVString>` from the vm-js bindings.
            let params = UrlSearchParams::new(&self.limits);
            let keys = scope.ordinary_own_property_keys(obj)?;
            for key in keys {
              let PropertyKey::String(key_s) = key else {
                continue;
              };

              let Some(desc) = scope.heap().object_get_own_property(obj, &key)? else {
                continue;
              };
              if !desc.enumerable {
                continue;
              }

              let value = scope.heap().get(obj, &key)?;
              let name = scope.heap().get_string(key_s)?.to_utf8_lossy();
              let value = Self::value_to_rust_string(scope, value)?;
              params
                .append(&name, &value)
                .map_err(|_| VmError::TypeError("URLSearchParams constructor failed"))?;
            }
            Ok(params)
          }
        }
        other => {
          // The bindings should have already canonicalized to USVString/sequence/record. Be
          // permissive and fall back to string parsing for unexpected values.
          let init = Self::value_to_rust_string(scope, other)?;
          if init.is_empty() {
            Ok(UrlSearchParams::new(&self.limits))
          } else {
            UrlSearchParams::parse(&init, &self.limits)
              .map_err(|_| VmError::TypeError("URLSearchParams constructor failed"))
          }
        }
      }
    }
  }

  impl WebIdlBindingsHost for UrlSearchParamsHost {
    fn call_operation(
      &mut self,
      vm: &mut Vm,
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
          let init = args.first().copied().unwrap_or(Value::Undefined);
          let params = self.init_to_params(scope, init)?;
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

    fn iterable_snapshot(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      receiver: Option<Value>,
      interface: &'static str,
      kind: IterableKind,
    ) -> Result<Vec<webidl_vm_js::bindings_runtime::BindingValue>, VmError> {
      match interface {
        "URLSearchParams" => {
          let params = self.require_params(receiver)?;
          let pairs = params
            .pairs()
            .map_err(|_| VmError::TypeError("URLSearchParams iteration failed"))?;
          let mut out: Vec<webidl_vm_js::bindings_runtime::BindingValue> =
            Vec::with_capacity(pairs.len());
          for (k, v) in pairs {
            match kind {
              IterableKind::Entries => out.push(
                webidl_vm_js::bindings_runtime::BindingValue::Sequence(vec![
                  webidl_vm_js::bindings_runtime::BindingValue::RustString(k),
                  webidl_vm_js::bindings_runtime::BindingValue::RustString(v),
                ]),
              ),
              IterableKind::Keys => {
                out.push(webidl_vm_js::bindings_runtime::BindingValue::RustString(k))
              }
              IterableKind::Values => {
                out.push(webidl_vm_js::bindings_runtime::BindingValue::RustString(v))
              }
            }
          }
          Ok(out)
        }
        _ => Err(VmError::TypeError("unimplemented host iterable snapshot")),
      }
    }
  }

  #[test]
  fn vmjs_bindings_can_construct_and_use_url_search_params() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let result =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), VmError> {
        let mut host = UrlSearchParamsHost::default();
        install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

        let mut hooks = HostHooksWithBindingsHost::new(&mut host);
        let mut scope = heap.scope();
        let intr = vm
          .intrinsics()
          .ok_or(VmError::InvariantViolation("missing intrinsics"))?;
        let array_proto = intr.array_prototype();
        scope.push_root(Value::Object(array_proto))?;

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

        // new URLSearchParams([["x", "1"], ["y", "2"]])
        let seq_outer = scope.alloc_array(0)?;
        scope.push_root(Value::Object(seq_outer))?;
        scope
          .heap_mut()
          .object_set_prototype(seq_outer, Some(array_proto))?;

        let pair0 = scope.alloc_array(0)?;
        scope.push_root(Value::Object(pair0))?;
        scope
          .heap_mut()
          .object_set_prototype(pair0, Some(array_proto))?;
        let x_str = scope.alloc_string("x")?;
        scope.push_root(Value::String(x_str))?;
        let idx0 = alloc_key(&mut scope, "0")?;
        let idx1 = alloc_key(&mut scope, "1")?;
        scope.create_data_property_or_throw(pair0, idx0, Value::String(x_str))?;
        scope.create_data_property_or_throw(pair0, idx1, Value::Number(1.0))?;

        let pair1 = scope.alloc_array(0)?;
        scope.push_root(Value::Object(pair1))?;
        scope
          .heap_mut()
          .object_set_prototype(pair1, Some(array_proto))?;
        let y_str = scope.alloc_string("y")?;
        scope.push_root(Value::String(y_str))?;
        let idx0 = alloc_key(&mut scope, "0")?;
        let idx1 = alloc_key(&mut scope, "1")?;
        scope.create_data_property_or_throw(pair1, idx0, Value::String(y_str))?;
        scope.create_data_property_or_throw(pair1, idx1, Value::Number(2.0))?;

        let outer0 = alloc_key(&mut scope, "0")?;
        let outer1 = alloc_key(&mut scope, "1")?;
        scope.create_data_property_or_throw(seq_outer, outer0, Value::Object(pair0))?;
        scope.create_data_property_or_throw(seq_outer, outer1, Value::Object(pair1))?;

        let seq_params_val = vm.construct_with_host(
          &mut scope,
          &mut hooks,
          ctor,
          &[Value::Object(seq_outer)],
          ctor,
        )?;
        scope.push_root(seq_params_val)?;
        let Value::Object(seq_params_obj) = seq_params_val else {
          panic!("URLSearchParams constructor (sequence init) should return an object");
        };

        let get = vm.get(&mut scope, seq_params_obj, get_key)?;
        let out = vm.call_with_host(
          &mut scope,
          &mut hooks,
          get,
          seq_params_val,
          &[Value::String(x_str)],
        )?;
        let out_s = UrlSearchParamsHost::value_to_rust_string(&mut scope, out)?;
        assert_eq!(out_s, "1");

        let get = vm.get(&mut scope, seq_params_obj, get_key)?;
        let out = vm.call_with_host(
          &mut scope,
          &mut hooks,
          get,
          seq_params_val,
          &[Value::String(y_str)],
        )?;
        let out_s = UrlSearchParamsHost::value_to_rust_string(&mut scope, out)?;
        assert_eq!(out_s, "2");

        // new URLSearchParams({ m: "3", n: "4" })
        let rec_init_obj = scope.alloc_object()?;
        scope.push_root(Value::Object(rec_init_obj))?;
        let m_key = alloc_key(&mut scope, "m")?;
        scope.create_data_property_or_throw(rec_init_obj, m_key, Value::Number(3.0))?;
        let n_key = alloc_key(&mut scope, "n")?;
        scope.create_data_property_or_throw(rec_init_obj, n_key, Value::Number(4.0))?;

        let rec_params_val = vm.construct_with_host(
          &mut scope,
          &mut hooks,
          ctor,
          &[Value::Object(rec_init_obj)],
          ctor,
        )?;
        scope.push_root(rec_params_val)?;
        let Value::Object(rec_params_obj) = rec_params_val else {
          panic!("URLSearchParams constructor (record init) should return an object");
        };

        let m_name_str = scope.alloc_string("m")?;
        scope.push_root(Value::String(m_name_str))?;
        let m_name = Value::String(m_name_str);
        let get = vm.get(&mut scope, rec_params_obj, get_key)?;
        let out = vm.call_with_host(&mut scope, &mut hooks, get, rec_params_val, &[m_name])?;
        let out_s = UrlSearchParamsHost::value_to_rust_string(&mut scope, out)?;
        assert_eq!(out_s, "3");

        let n_name_str = scope.alloc_string("n")?;
        scope.push_root(Value::String(n_name_str))?;
        let n_name = Value::String(n_name_str);
        let get = vm.get(&mut scope, rec_params_obj, get_key)?;
        let out = vm.call_with_host(&mut scope, &mut hooks, get, rec_params_val, &[n_name])?;
        let out_s = UrlSearchParamsHost::value_to_rust_string(&mut scope, out)?;
        assert_eq!(out_s, "4");
        Ok(())
      }));
    realm.teardown(&mut heap);
    match result {
      Ok(result) => result,
      Err(panic) => std::panic::resume_unwind(panic),
    }
  }

  fn assert_thrown_type_error(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    err: VmError,
    expected_message: &str,
  ) -> Result<(), VmError> {
    let thrown = err
      .thrown_value()
      .expect("expected a thrown exception value");
    let Value::Object(thrown_obj) = thrown else {
      panic!("expected thrown error to be an object");
    };
    scope.push_root(thrown)?;
    let name_key = alloc_key(scope, "name")?;
    let message_key = alloc_key(scope, "message")?;
    let name_val = vm.get(scope, thrown_obj, name_key)?;
    let message_val = vm.get(scope, thrown_obj, message_key)?;
    let Value::String(name_s) = name_val else {
      panic!("expected error.name to be a string");
    };
    let Value::String(message_s) = message_val else {
      panic!("expected error.message to be a string");
    };
    assert_eq!(
      scope.heap().get_string(name_s)?.to_utf8_lossy(),
      "TypeError"
    );
    assert_eq!(
      scope.heap().get_string(message_s)?.to_utf8_lossy(),
      expected_message
    );
    Ok(())
  }

  #[test]
  fn generated_bindings_url_search_params_is_iterable_vm_js() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

    let mut host = UrlSearchParamsHost::default();
    let mut hooks = HostHooksWithBindingsHost::new(&mut host);
    let mut scope = heap.scope();

    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;
    let ctor_key = alloc_key(&mut scope, "URLSearchParams")?;
    let ctor = scope
      .heap()
      .object_get_own_data_property_value(global, &ctor_key)?
      .expect("globalThis.URLSearchParams should be defined");
    scope.push_root(ctor)?;

    let init_s = scope.alloc_string("?a=b&c=d")?;
    scope.push_root(Value::String(init_s))?;
    let init = Value::String(init_s);

    let params_val = vm.construct_with_host(&mut scope, &mut hooks, ctor, &[init], ctor)?;
    scope.push_root(params_val)?;
    let Value::Object(params_obj) = params_val else {
      panic!("URLSearchParams constructor should return an object");
    };

    // Surface: entries/keys/values/forEach + @@iterator should be defined on the prototype.
    for name in ["entries", "keys", "values", "forEach"] {
      let key = alloc_key(&mut scope, name)?;
      let value = vm.get(&mut scope, params_obj, key)?;
      assert!(
        scope.heap().is_callable(value)?,
        "expected URLSearchParams.{name} to be callable"
      );
    }

    let iter_sym = realm.well_known_symbols().iterator;
    let iter_key = PropertyKey::from_symbol(iter_sym);
    let iter_method = vm
      .get_method(&mut scope, params_val, iter_key)?
      .ok_or(VmError::TypeError("missing URLSearchParams @@iterator"))?;
    assert!(scope.heap().is_callable(iter_method)?);

    let iter = vm.call_with_host(&mut scope, &mut hooks, iter_method, params_val, &[])?;
    scope.push_root(iter)?;
    let Value::Object(iter_obj) = iter else {
      return Err(VmError::TypeError("expected iterator object"));
    };

    let next_key = alloc_key(&mut scope, "next")?;
    let done_key = alloc_key(&mut scope, "done")?;
    let value_key = alloc_key(&mut scope, "value")?;
    let k0 = alloc_key(&mut scope, "0")?;
    let k1 = alloc_key(&mut scope, "1")?;

    let next = vm.get(&mut scope, iter_obj, next_key)?;
    scope.push_root(next)?;

    let mut pairs: Vec<(String, String)> = Vec::new();
    loop {
      let result = vm.call_with_host(&mut scope, &mut hooks, next, iter, &[])?;
      scope.push_root(result)?;
      let Value::Object(result_obj) = result else {
        return Err(VmError::TypeError("expected iterator result object"));
      };
      let done = vm.get(&mut scope, result_obj, done_key)?;
      if matches!(done, Value::Bool(true)) {
        break;
      }
      let pair = vm.get(&mut scope, result_obj, value_key)?;
      scope.push_root(pair)?;
      let Value::Object(pair_obj) = pair else {
        return Err(VmError::TypeError("expected [key, value] pair object"));
      };
      let key_v = vm.get(&mut scope, pair_obj, k0)?;
      let val_v = vm.get(&mut scope, pair_obj, k1)?;
      pairs.push((
        UrlSearchParamsHost::value_to_rust_string(&mut scope, key_v)?,
        UrlSearchParamsHost::value_to_rust_string(&mut scope, val_v)?,
      ));
    }

    assert_eq!(
      pairs,
      vec![
        ("a".to_string(), "b".to_string()),
        ("c".to_string(), "d".to_string())
      ]
    );

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[derive(Default)]
  struct ToyIterableHost {
    foo_objects: HashMap<WeakGcObject, ()>,
    bar_objects: HashMap<WeakGcObject, ()>,
  }

  impl WebIdlBindingsHost for ToyIterableHost {
    fn call_operation(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      receiver: Option<Value>,
      interface: &'static str,
      operation: &'static str,
      _overload: usize,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      match (interface, operation) {
        ("Foo", "constructor") => {
          let Some(Value::Object(obj)) = receiver else {
            return Err(VmError::InvariantViolation(
              "Foo constructor called without wrapper object receiver",
            ));
          };
          self.foo_objects.insert(WeakGcObject::from(obj), ());
          Ok(Value::Undefined)
        }
        ("Bar", "constructor") => {
          let Some(Value::Object(obj)) = receiver else {
            return Err(VmError::InvariantViolation(
              "Bar constructor called without wrapper object receiver",
            ));
          };
          self.bar_objects.insert(WeakGcObject::from(obj), ());
          Ok(Value::Undefined)
        }
        _ => Err(VmError::TypeError("unimplemented toy host operation")),
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
      Err(VmError::TypeError("unimplemented toy host constructor"))
    }

    fn iterable_snapshot(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      receiver: Option<Value>,
      interface: &'static str,
      kind: IterableKind,
    ) -> Result<Vec<webidl_vm_js::bindings_runtime::BindingValue>, VmError> {
      match interface {
        "Foo" => {
          let Some(Value::Object(obj)) = receiver else {
            return Err(VmError::TypeError("Illegal invocation"));
          };
          if !self.foo_objects.contains_key(&WeakGcObject::from(obj)) {
            return Err(VmError::TypeError("Illegal invocation"));
          }

          let entries = vec![
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
          ];
          let mut out: Vec<webidl_vm_js::bindings_runtime::BindingValue> =
            Vec::with_capacity(entries.len());
          for (k, v) in entries {
            match kind {
              IterableKind::Entries => out.push(
                webidl_vm_js::bindings_runtime::BindingValue::Sequence(vec![
                  webidl_vm_js::bindings_runtime::BindingValue::RustString(k),
                  webidl_vm_js::bindings_runtime::BindingValue::RustString(v),
                ]),
              ),
              IterableKind::Keys => {
                out.push(webidl_vm_js::bindings_runtime::BindingValue::RustString(k))
              }
              IterableKind::Values => {
                out.push(webidl_vm_js::bindings_runtime::BindingValue::RustString(v))
              }
            }
          }
          Ok(out)
        }
        "Bar" => {
          let Some(Value::Object(obj)) = receiver else {
            return Err(VmError::TypeError("Illegal invocation"));
          };
          if !self.bar_objects.contains_key(&WeakGcObject::from(obj)) {
            return Err(VmError::TypeError("Illegal invocation"));
          }

          let values = vec!["x".to_string(), "y".to_string()];
          let mut out: Vec<webidl_vm_js::bindings_runtime::BindingValue> =
            Vec::with_capacity(values.len());
          for v in values {
            match kind {
              // Like JS `Set`, `entries()` for a value iterable yields `[value, value]` pairs.
              IterableKind::Entries => out.push(
                webidl_vm_js::bindings_runtime::BindingValue::Sequence(vec![
                  webidl_vm_js::bindings_runtime::BindingValue::RustString(v.clone()),
                  webidl_vm_js::bindings_runtime::BindingValue::RustString(v),
                ]),
              ),
              IterableKind::Keys | IterableKind::Values => {
                out.push(webidl_vm_js::bindings_runtime::BindingValue::RustString(v))
              }
            }
          }
          Ok(out)
        }
        _ => Err(VmError::TypeError(
          "unimplemented toy host iterable snapshot",
        )),
      }
    }
  }

  #[test]
  fn generated_bindings_iterable_surface_works_for_toy_interface_vm_js() -> Result<(), VmError> {
    use super::webidl_bindings_codegen_toy_generated_vmjs as toy_bindings_vmjs;

    struct ForEachRecorder {
      expected_this: Value,
      expected_receiver: Value,
      saw: Vec<(String, String)>,
      this_matches: Vec<bool>,
      receiver_matches: Vec<bool>,
    }

    impl Default for ForEachRecorder {
      fn default() -> Self {
        Self {
          expected_this: Value::Undefined,
          expected_receiver: Value::Undefined,
          saw: Vec::new(),
          this_matches: Vec::new(),
          receiver_matches: Vec::new(),
        }
      }
    }

    fn for_each_callback(
      _vm: &mut Vm,
      scope: &mut Scope<'_>,
      host: &mut dyn VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: GcObject,
      this: Value,
      args: &[Value],
    ) -> Result<Value, VmError> {
      let Some(rec) = host.as_any_mut().downcast_mut::<ForEachRecorder>() else {
        return Err(VmError::InvariantViolation("missing ForEachRecorder host"));
      };

      let value_v = args.get(0).copied().unwrap_or(Value::Undefined);
      let key_v = args.get(1).copied().unwrap_or(Value::Undefined);
      let receiver_v = args.get(2).copied().unwrap_or(Value::Undefined);

      rec.this_matches.push(this == rec.expected_this);
      rec
        .receiver_matches
        .push(receiver_v == rec.expected_receiver);
      rec.saw.push((
        UrlSearchParamsHost::value_to_rust_string(scope, value_v)?,
        UrlSearchParamsHost::value_to_rust_string(scope, key_v)?,
      ));

      Ok(Value::Undefined)
    }

    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    toy_bindings_vmjs::install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

    let mut host = ToyIterableHost::default();
    let mut hooks = HostHooksWithBindingsHost::new(&mut host);

    let result = (|| {
      let mut scope = heap.scope();

      let global = realm.global_object();
      scope.push_root(Value::Object(global))?;
      let ctor_key = alloc_key(&mut scope, "Foo")?;
      let ctor = scope
        .heap()
        .object_get_own_data_property_value(global, &ctor_key)?
        .expect("globalThis.Foo should be defined");
      scope.push_root(ctor)?;

      let obj = vm.construct_with_host(&mut scope, &mut hooks, ctor, &[], ctor)?;
      scope.push_root(obj)?;
      let Value::Object(obj_handle) = obj else {
        panic!("Foo constructor should return an object");
      };

      let iter_sym = realm.well_known_symbols().iterator;
      let iter_key = PropertyKey::from_symbol(iter_sym);
      let iter_method = vm
        .get_method(&mut scope, obj, iter_key)?
        .ok_or(VmError::TypeError("missing Foo @@iterator"))?;
      assert!(scope.heap().is_callable(iter_method)?);

      // `.entries()` returns an iterator that yields `[key, value]` pairs.
      let entries_key = alloc_key(&mut scope, "entries")?;
      let entries = vm.get(&mut scope, obj_handle, entries_key)?;
      assert!(scope.heap().is_callable(entries)?);
      let iter = vm.call_with_host(&mut scope, &mut hooks, entries, obj, &[])?;
      scope.push_root(iter)?;
      let Value::Object(iter_obj) = iter else {
        return Err(VmError::TypeError("expected iterator object"));
      };

      let next_key = alloc_key(&mut scope, "next")?;
      let done_key = alloc_key(&mut scope, "done")?;
      let value_key = alloc_key(&mut scope, "value")?;
      let k0 = alloc_key(&mut scope, "0")?;
      let k1 = alloc_key(&mut scope, "1")?;

      let next = vm.get(&mut scope, iter_obj, next_key)?;
      scope.push_root(next)?;

      let mut pairs: Vec<(String, String)> = Vec::new();
      loop {
        let result = vm.call_with_host(&mut scope, &mut hooks, next, iter, &[])?;
        scope.push_root(result)?;
        let Value::Object(result_obj) = result else {
          return Err(VmError::TypeError("expected iterator result object"));
        };
        let done = vm.get(&mut scope, result_obj, done_key)?;
        if matches!(done, Value::Bool(true)) {
          break;
        }
        let pair = vm.get(&mut scope, result_obj, value_key)?;
        scope.push_root(pair)?;
        let Value::Object(pair_obj) = pair else {
          return Err(VmError::TypeError("expected [key, value] pair object"));
        };
        let key_v = vm.get(&mut scope, pair_obj, k0)?;
        let val_v = vm.get(&mut scope, pair_obj, k1)?;
        pairs.push((
          UrlSearchParamsHost::value_to_rust_string(&mut scope, key_v)?,
          UrlSearchParamsHost::value_to_rust_string(&mut scope, val_v)?,
        ));
      }

      assert_eq!(
        pairs,
        vec![
          ("a".to_string(), "1".to_string()),
          ("b".to_string(), "2".to_string())
        ]
      );

      // And `forEach(callback[, thisArg])` invokes the callback with (value, key, this) and the
      // provided `thisArg`.
      let this_arg_obj = scope.alloc_object()?;
      scope.push_root(Value::Object(this_arg_obj))?;
      let this_arg = Value::Object(this_arg_obj);

      let callback_name = scope.alloc_string("callback")?;
      scope.push_root(Value::String(callback_name))?;
      let callback_id = vm.register_native_call(for_each_callback)?;
      let callback_fn = scope.alloc_native_function(callback_id, None, callback_name, 0)?;
      scope.push_root(Value::Object(callback_fn))?;
      let callback = Value::Object(callback_fn);

      let for_each_key = alloc_key(&mut scope, "forEach")?;
      let for_each = vm.get(&mut scope, obj_handle, for_each_key)?;
      scope.push_root(for_each)?;
      let mut recorder = ForEachRecorder {
        expected_this: this_arg,
        expected_receiver: obj,
        ..Default::default()
      };

      vm.call_with_host_and_hooks(
        &mut recorder,
        &mut scope,
        &mut hooks,
        for_each,
        obj,
        &[callback, this_arg],
      )?;

      Ok(recorder)
    })();

    realm.teardown(&mut heap);
    let recorder = result?;
    assert_eq!(
      recorder.saw,
      vec![
        ("1".to_string(), "a".to_string()),
        ("2".to_string(), "b".to_string())
      ]
    );
    assert!(recorder.this_matches.iter().all(|v| *v));
    assert!(recorder.receiver_matches.iter().all(|v| *v));
    Ok(())
  }

  #[test]
  fn generated_bindings_value_iterable_surface_works_for_toy_interface_vm_js() -> Result<(), VmError>
  {
    use super::webidl_bindings_codegen_toy_generated_vmjs as toy_bindings_vmjs;

    struct ForEachRecorder {
      expected_this: Value,
      expected_receiver: Value,
      saw: Vec<(String, String)>,
      this_matches: Vec<bool>,
      receiver_matches: Vec<bool>,
    }

    impl Default for ForEachRecorder {
      fn default() -> Self {
        Self {
          expected_this: Value::Undefined,
          expected_receiver: Value::Undefined,
          saw: Vec::new(),
          this_matches: Vec::new(),
          receiver_matches: Vec::new(),
        }
      }
    }

    fn for_each_callback(
      _vm: &mut Vm,
      scope: &mut Scope<'_>,
      host: &mut dyn VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: GcObject,
      this: Value,
      args: &[Value],
    ) -> Result<Value, VmError> {
      let Some(rec) = host.as_any_mut().downcast_mut::<ForEachRecorder>() else {
        return Err(VmError::InvariantViolation("missing ForEachRecorder host"));
      };

      let value_v = args.get(0).copied().unwrap_or(Value::Undefined);
      let key_v = args.get(1).copied().unwrap_or(Value::Undefined);
      let receiver_v = args.get(2).copied().unwrap_or(Value::Undefined);

      rec.this_matches.push(this == rec.expected_this);
      rec
        .receiver_matches
        .push(receiver_v == rec.expected_receiver);
      rec.saw.push((
        UrlSearchParamsHost::value_to_rust_string(scope, value_v)?,
        UrlSearchParamsHost::value_to_rust_string(scope, key_v)?,
      ));

      Ok(Value::Undefined)
    }

    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    toy_bindings_vmjs::install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

    let mut host = ToyIterableHost::default();
    let mut hooks = HostHooksWithBindingsHost::new(&mut host);

    let result = (|| {
      let mut scope = heap.scope();

      let global = realm.global_object();
      scope.push_root(Value::Object(global))?;
      let ctor_key = alloc_key(&mut scope, "Bar")?;
      let ctor = scope
        .heap()
        .object_get_own_data_property_value(global, &ctor_key)?
        .expect("globalThis.Bar should be defined");
      scope.push_root(ctor)?;

      let obj = vm.construct_with_host(&mut scope, &mut hooks, ctor, &[], ctor)?;
      scope.push_root(obj)?;
      let Value::Object(obj_handle) = obj else {
        panic!("Bar constructor should return an object");
      };

      // @@iterator should yield values (not [key, value] pairs).
      let iter_sym = realm.well_known_symbols().iterator;
      let iter_key = PropertyKey::from_symbol(iter_sym);
      let iter_method = vm
        .get_method(&mut scope, obj, iter_key)?
        .ok_or(VmError::TypeError("missing Bar @@iterator"))?;
      assert!(scope.heap().is_callable(iter_method)?);

      let iter = vm.call_with_host(&mut scope, &mut hooks, iter_method, obj, &[])?;
      scope.push_root(iter)?;
      let Value::Object(iter_obj) = iter else {
        return Err(VmError::TypeError("expected iterator object"));
      };

      let next_key = alloc_key(&mut scope, "next")?;
      let done_key = alloc_key(&mut scope, "done")?;
      let value_key = alloc_key(&mut scope, "value")?;
      let next = vm.get(&mut scope, iter_obj, next_key)?;
      scope.push_root(next)?;

      let mut values: Vec<String> = Vec::new();
      loop {
        let result = vm.call_with_host(&mut scope, &mut hooks, next, iter, &[])?;
        scope.push_root(result)?;
        let Value::Object(result_obj) = result else {
          return Err(VmError::TypeError("expected iterator result object"));
        };
        let done = vm.get(&mut scope, result_obj, done_key)?;
        if matches!(done, Value::Bool(true)) {
          break;
        }
        let v = vm.get(&mut scope, result_obj, value_key)?;
        values.push(UrlSearchParamsHost::value_to_rust_string(&mut scope, v)?);
      }
      assert_eq!(values, vec!["x".to_string(), "y".to_string()]);

      // `.entries()` yields `[value, value]` pairs (Set-like).
      let entries_key = alloc_key(&mut scope, "entries")?;
      let entries = vm.get(&mut scope, obj_handle, entries_key)?;
      assert!(scope.heap().is_callable(entries)?);
      let iter = vm.call_with_host(&mut scope, &mut hooks, entries, obj, &[])?;
      scope.push_root(iter)?;
      let Value::Object(iter_obj) = iter else {
        return Err(VmError::TypeError("expected iterator object"));
      };

      let next_key = alloc_key(&mut scope, "next")?;
      let done_key = alloc_key(&mut scope, "done")?;
      let value_key = alloc_key(&mut scope, "value")?;
      let k0 = alloc_key(&mut scope, "0")?;
      let k1 = alloc_key(&mut scope, "1")?;

      let next = vm.get(&mut scope, iter_obj, next_key)?;
      scope.push_root(next)?;
      let mut entries_out: Vec<(String, String)> = Vec::new();
      loop {
        let result = vm.call_with_host(&mut scope, &mut hooks, next, iter, &[])?;
        scope.push_root(result)?;
        let Value::Object(result_obj) = result else {
          return Err(VmError::TypeError("expected iterator result object"));
        };
        let done = vm.get(&mut scope, result_obj, done_key)?;
        if matches!(done, Value::Bool(true)) {
          break;
        }
        let pair = vm.get(&mut scope, result_obj, value_key)?;
        scope.push_root(pair)?;
        let Value::Object(pair_obj) = pair else {
          return Err(VmError::TypeError("expected [value, value] pair object"));
        };
        let v0 = vm.get(&mut scope, pair_obj, k0)?;
        let v1 = vm.get(&mut scope, pair_obj, k1)?;
        entries_out.push((
          UrlSearchParamsHost::value_to_rust_string(&mut scope, v0)?,
          UrlSearchParamsHost::value_to_rust_string(&mut scope, v1)?,
        ));
      }
      assert_eq!(
        entries_out,
        vec![
          ("x".to_string(), "x".to_string()),
          ("y".to_string(), "y".to_string())
        ]
      );

      // And `forEach(callback[, thisArg])` invokes the callback with (value, key, this) where
      // value==key for a value iterable.
      let this_arg_obj = scope.alloc_object()?;
      scope.push_root(Value::Object(this_arg_obj))?;
      let this_arg = Value::Object(this_arg_obj);

      let callback_name = scope.alloc_string("callback")?;
      scope.push_root(Value::String(callback_name))?;
      let callback_id = vm.register_native_call(for_each_callback)?;
      let callback_fn = scope.alloc_native_function(callback_id, None, callback_name, 0)?;
      scope.push_root(Value::Object(callback_fn))?;
      let callback = Value::Object(callback_fn);

      let for_each_key = alloc_key(&mut scope, "forEach")?;
      let for_each = vm.get(&mut scope, obj_handle, for_each_key)?;
      scope.push_root(for_each)?;
      let mut recorder = ForEachRecorder {
        expected_this: this_arg,
        expected_receiver: obj,
        ..Default::default()
      };

      vm.call_with_host_and_hooks(
        &mut recorder,
        &mut scope,
        &mut hooks,
        for_each,
        obj,
        &[callback, this_arg],
      )?;
      Ok(recorder)
    })();

    realm.teardown(&mut heap);
    let recorder = result?;
    assert_eq!(
      recorder.saw,
      vec![
        ("x".to_string(), "x".to_string()),
        ("y".to_string(), "y".to_string())
      ]
    );
    assert!(recorder.this_matches.iter().all(|v| *v));
    assert!(recorder.receiver_matches.iter().all(|v| *v));
    Ok(())
  }

  #[derive(Default)]
  struct CountingHost {
    calls: usize,
  }

  impl WebIdlBindingsHost for CountingHost {
    fn call_operation(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _receiver: Option<Value>,
      _interface: &'static str,
      _operation: &'static str,
      _overload: usize,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      self.calls += 1;
      Ok(Value::Undefined)
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

  #[test]
  fn vmjs_bindings_queue_microtask_rejects_non_callable_without_host_dispatch(
  ) -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let result =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), VmError> {
        let mut host_impl = CountingHost::default();
        install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

        let mut hooks = HostHooksWithBindingsHost::new(&mut host_impl);
        let mut dummy_host = ();
        let mut scope = heap.scope();

        let global = realm.global_object();
        scope.push_root(Value::Object(global))?;
        let key = alloc_key(&mut scope, "queueMicrotask")?;
        let func = scope
          .heap()
          .object_get_own_data_property_value(global, &key)?
          .expect("globalThis.queueMicrotask should be defined");
        scope.push_root(func)?;

        let err = vm
          .call_with_host_and_hooks(
            &mut dummy_host,
            &mut scope,
            &mut hooks,
            func,
            Value::Undefined,
            &[Value::Number(1.0)],
          )
          .expect_err("expected queueMicrotask to throw");
        assert_thrown_type_error(
          &mut vm,
          &mut scope,
          err,
          "Value is not a callable callback function",
        )?;
        assert_eq!(
          host_impl.calls, 0,
          "host must not be called on conversion failure"
        );
        Ok(())
      }));
    realm.teardown(&mut heap);
    match result {
      Ok(result) => result,
      Err(panic) => std::panic::resume_unwind(panic),
    }
  }

  #[test]
  fn vmjs_bindings_add_event_listener_rejects_invalid_callback_without_host_dispatch(
  ) -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let result =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), VmError> {
        let mut host_impl = CountingHost::default();
        install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

        let mut hooks = HostHooksWithBindingsHost::new(&mut host_impl);
        let mut dummy_host = ();
        let mut scope = heap.scope();

        let global = realm.global_object();
        scope.push_root(Value::Object(global))?;
        let ctor_key = alloc_key(&mut scope, "EventTarget")?;
        let ctor = scope
          .heap()
          .object_get_own_data_property_value(global, &ctor_key)?
          .expect("globalThis.EventTarget should be defined");
        scope.push_root(ctor)?;
        let Value::Object(ctor_obj) = ctor else {
          return Err(VmError::TypeError(
            "EventTarget constructor should be an object",
          ));
        };

        let proto_key = alloc_key(&mut scope, "prototype")?;
        let proto = vm.get(&mut scope, ctor_obj, proto_key)?;
        let Value::Object(proto_obj) = proto else {
          return Err(VmError::TypeError(
            "EventTarget.prototype should be an object",
          ));
        };
        scope.push_root(Value::Object(proto_obj))?;

        let add_key = alloc_key(&mut scope, "addEventListener")?;
        let add = vm.get(&mut scope, proto_obj, add_key)?;
        scope.push_root(add)?;

        let this_obj = scope.alloc_object()?;
        scope.push_root(Value::Object(this_obj))?;
        let type_s = scope.alloc_string("x")?;
        scope.push_root(Value::String(type_s))?;
        let type_val = Value::String(type_s);

        let err = vm
          .call_with_host_and_hooks(
            &mut dummy_host,
            &mut scope,
            &mut hooks,
            add,
            Value::Object(this_obj),
            &[type_val, Value::Number(1.0)],
          )
          .expect_err("expected addEventListener to throw");
        assert_thrown_type_error(
          &mut vm,
          &mut scope,
          err,
          "Value is not a callable callback interface",
        )?;
        assert_eq!(
          host_impl.calls, 0,
          "host must not be called on conversion failure"
        );
        Ok(())
      }));
    realm.teardown(&mut heap);
    match result {
      Ok(result) => result,
      Err(panic) => std::panic::resume_unwind(panic),
    }
  }

  #[test]
  fn vmjs_bindings_set_timeout_rejects_string_handler_without_host_dispatch() -> Result<(), VmError>
  {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let result =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), VmError> {
        let mut host_impl = CountingHost::default();
        install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

        let mut hooks = HostHooksWithBindingsHost::new(&mut host_impl);
        let mut dummy_host = ();
        let mut scope = heap.scope();

        let global = realm.global_object();
        scope.push_root(Value::Object(global))?;
        let key = alloc_key(&mut scope, "setTimeout")?;
        let func = scope
          .heap()
          .object_get_own_data_property_value(global, &key)?
          .expect("globalThis.setTimeout should be defined");
        scope.push_root(func)?;

        let handler_s = scope.alloc_string("alert(1)")?;
        scope.push_root(Value::String(handler_s))?;
        let handler = Value::String(handler_s);

        let err = vm
          .call_with_host_and_hooks(
            &mut dummy_host,
            &mut scope,
            &mut hooks,
            func,
            Value::Undefined,
            &[handler],
          )
          .expect_err("expected setTimeout to throw");
        assert_thrown_type_error(
          &mut vm,
          &mut scope,
          err,
          "setTimeout does not currently support string handlers",
        )?;
        assert_eq!(
          host_impl.calls, 0,
          "host must not be called on conversion failure"
        );
        Ok(())
      }));
    realm.teardown(&mut heap);
    match result {
      Ok(result) => result,
      Err(panic) => std::panic::resume_unwind(panic),
    }
  }

  #[test]
  fn vmjs_bindings_set_timeout_flattens_variadic_args_before_host_dispatch() -> Result<(), VmError>
  {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    #[derive(Default)]
    struct RecordingHost {
      calls: usize,
      last_interface: Option<&'static str>,
      last_operation: Option<&'static str>,
      last_args: Vec<Value>,
    }

    impl WebIdlBindingsHost for RecordingHost {
      fn call_operation(
        &mut self,
        _vm: &mut Vm,
        _scope: &mut Scope<'_>,
        _receiver: Option<Value>,
        interface: &'static str,
        operation: &'static str,
        _overload: usize,
        args: &[Value],
      ) -> Result<Value, VmError> {
        self.calls += 1;
        self.last_interface = Some(interface);
        self.last_operation = Some(operation);
        self.last_args = args.to_vec();
        Ok(Value::Undefined)
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

    let result =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), VmError> {
        let mut host_impl = RecordingHost::default();
        install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

        let mut hooks = HostHooksWithBindingsHost::new(&mut host_impl);
        let mut dummy_host = ();
        let mut scope = heap.scope();

        let global = realm.global_object();
        scope.push_root(Value::Object(global))?;
        let key = alloc_key(&mut scope, "setTimeout")?;
        let func = scope
          .heap()
          .object_get_own_data_property_value(global, &key)?
          .expect("globalThis.setTimeout should be defined");
        scope.push_root(func)?;

        fn noop_cb(
          _vm: &mut Vm,
          _scope: &mut Scope<'_>,
          _host: &mut dyn VmHost,
          _hooks: &mut dyn VmHostHooks,
          _callee: GcObject,
          _this: Value,
          _args: &[Value],
        ) -> Result<Value, VmError> {
          Ok(Value::Undefined)
        }

        let cb_name = scope.alloc_string("cb")?;
        scope.push_root(Value::String(cb_name))?;
        let cb_id = vm.register_native_call(noop_cb)?;
        let cb_obj = scope.alloc_native_function(cb_id, None, cb_name, 0)?;
        scope.push_root(Value::Object(cb_obj))?;
        let callback = Value::Object(cb_obj);

        let a_s = scope.alloc_string("a")?;
        scope.push_root(Value::String(a_s))?;
        let b_s = scope.alloc_string("b")?;
        scope.push_root(Value::String(b_s))?;

        let out = vm.call_with_host_and_hooks(
          &mut dummy_host,
          &mut scope,
          &mut hooks,
          func,
          Value::Undefined,
          &[
            callback,
            Value::Number(0.0),
            Value::String(a_s),
            Value::String(b_s),
          ],
        )?;
        assert_eq!(out, Value::Undefined);

        assert_eq!(host_impl.calls, 1);
        assert_eq!(host_impl.last_interface, Some("Window"));
        assert_eq!(host_impl.last_operation, Some("setTimeout"));
        assert_eq!(
          host_impl.last_args.len(),
          4,
          "expected variadic args to be flattened for host dispatch"
        );
        assert_eq!(host_impl.last_args[0], callback);
        assert_eq!(host_impl.last_args[1], Value::Number(0.0));
        assert_eq!(host_impl.last_args[2], Value::String(a_s));
        assert_eq!(host_impl.last_args[3], Value::String(b_s));
        Ok(())
      }));
    realm.teardown(&mut heap);
    match result {
      Ok(result) => result,
      Err(panic) => std::panic::resume_unwind(panic),
    }
  }

  #[derive(Default)]
  struct VmjsAttributeAndConstHost {
    limits: UrlLimits,
    params: HashMap<WeakGcObject, UrlSearchParams>,
    urls: HashMap<WeakGcObject, String>,
    last_set_href: Option<String>,
  }

  impl VmjsAttributeAndConstHost {
    fn require_params(&self, receiver: Option<Value>) -> Result<&UrlSearchParams, VmError> {
      let Some(Value::Object(obj)) = receiver else {
        return Err(VmError::TypeError("Illegal invocation"));
      };
      self
        .params
        .get(&WeakGcObject::from(obj))
        .ok_or(VmError::TypeError("Illegal invocation"))
    }

    fn require_url(&self, receiver: Option<Value>) -> Result<&str, VmError> {
      let Some(Value::Object(obj)) = receiver else {
        return Err(VmError::TypeError("Illegal invocation"));
      };
      self
        .urls
        .get(&WeakGcObject::from(obj))
        .map(String::as_str)
        .ok_or(VmError::TypeError("Illegal invocation"))
    }

    fn value_to_rust_string(scope: &mut Scope<'_>, value: Value) -> Result<String, VmError> {
      match value {
        Value::String(s) => Ok(scope.heap().get_string(s)?.to_utf8_lossy()),
        Value::Undefined => Ok(String::new()),
        other => {
          let s = scope.heap_mut().to_string(other)?;
          Ok(scope.heap().get_string(s)?.to_utf8_lossy())
        }
      }
    }
  }

  impl WebIdlBindingsHost for VmjsAttributeAndConstHost {
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
        // Attribute accessors are dispatched through the same `call_operation` hook. Getter calls
        // use `args = []`, setter calls use `args = [value]`.
        ("URLSearchParams", "size") => {
          let params = self.require_params(receiver)?;
          let size = params
            .size()
            .map_err(|_| VmError::TypeError("URLSearchParams.size failed"))?;
          Ok(Value::Number(size as f64))
        }
        ("URL", "constructor") => {
          let Some(Value::Object(obj)) = receiver else {
            return Err(VmError::InvariantViolation(
              "URL constructor called without wrapper object receiver",
            ));
          };

          let href = match args.get(0).copied().unwrap_or(Value::Undefined) {
            Value::Undefined => String::new(),
            value => Self::value_to_rust_string(scope, value)?,
          };
          self.urls.insert(WeakGcObject::from(obj), href);
          Ok(Value::Undefined)
        }
        ("URL", "href") => {
          if args.is_empty() {
            let href = self.require_url(receiver)?;
            Ok(Value::String(scope.alloc_string(href)?))
          } else {
            let Some(Value::Object(obj)) = receiver else {
              return Err(VmError::TypeError("Illegal invocation"));
            };
            let href = Self::value_to_rust_string(scope, args[0])?;
            self.last_set_href = Some(href.clone());
            self.urls.insert(WeakGcObject::from(obj), href);
            Ok(Value::Undefined)
          }
        }
        ("URL", "origin") => {
          let href = self.require_url(receiver)?;
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
          Ok(Value::String(scope.alloc_string(origin)?))
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

  #[test]
  fn generated_bindings_vmjs_support_attributes_and_constants() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let mut host = VmjsAttributeAndConstHost::default();
    install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

    let mut hooks = HostHooksWithBindingsHost::new(&mut host);
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

    let params_val =
      vm.construct_with_host(&mut scope, &mut hooks, params_ctor, &[init], params_ctor)?;
    scope.push_root(params_val)?;

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
    let size_val = vm.call_with_host(&mut scope, &mut hooks, get, params_val, &[])?;
    assert_eq!(size_val, Value::Number(2.0));

    // Calling the getter with an invalid receiver should throw a TypeError("Illegal invocation").
    {
      let err = vm
        .call_with_host(&mut scope, &mut hooks, get, Value::Undefined, &[])
        .expect_err("expected Illegal invocation error for URLSearchParams.prototype.size getter");
      assert_thrown_type_error(&mut vm, &mut scope, err, "Illegal invocation")?;
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

    let url_val = vm.construct_with_host(&mut scope, &mut hooks, url_ctor, &[url_arg], url_ctor)?;
    scope.push_root(url_val)?;

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
    let PropertyKind::Accessor {
      get: _href_get,
      set: href_set,
    } = href_desc.kind
    else {
      panic!("URL.prototype.href is not an accessor property");
    };
    assert!(matches!(href_set, Value::Object(_)));

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

    let origin_val = vm.call_with_host(&mut scope, &mut hooks, origin_get, url_val, &[])?;
    let origin_s = VmjsAttributeAndConstHost::value_to_rust_string(&mut scope, origin_val)?;
    assert_eq!(origin_s, "https://example.test");

    let new_href_str = scope.alloc_string("https://changed.test/")?;
    scope.push_root(Value::String(new_href_str))?;
    let new_href = Value::String(new_href_str);
    vm.call_with_host(&mut scope, &mut hooks, href_set, url_val, &[new_href])?;
    assert_eq!(host.last_set_href.as_deref(), Some("https://changed.test/"));

    let origin_val = vm.call_with_host(&mut scope, &mut hooks, origin_get, url_val, &[])?;
    let origin_s = VmjsAttributeAndConstHost::value_to_rust_string(&mut scope, origin_val)?;
    assert_eq!(origin_s, "https://changed.test");

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
      vm.call_with_host(&mut scope, &mut hooks, node_ctor, Value::Undefined, &[])
        .expect_err("expected Node() to throw"),
      vm.construct_with_host(&mut scope, &mut hooks, node_ctor, &[], node_ctor)
        .expect_err("expected new Node() to throw"),
    ] {
      assert_thrown_type_error(&mut vm, &mut scope, err, "Illegal constructor")?;
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

    // Constants should also be defined on the interface prototype object so instances can access
    // them via the prototype chain (e.g. `node.ELEMENT_NODE`).
    let node_proto_val = scope
      .heap()
      .object_get_own_data_property_value(node_ctor_obj, &proto_key)?
      .expect("Node.prototype should be defined");
    scope.push_root(node_proto_val)?;
    let Value::Object(node_proto_obj) = node_proto_val else {
      panic!("Node.prototype should be an object");
    };
    let Some(element_node_desc) = scope
      .heap()
      .object_get_own_property(node_proto_obj, &element_node_key)?
    else {
      panic!("Node.prototype.ELEMENT_NODE should be defined");
    };
    assert!(element_node_desc.enumerable, "constants must be enumerable");
    assert!(
      !element_node_desc.configurable,
      "constants must be non-configurable"
    );
    let PropertyKind::Data { value, writable } = element_node_desc.kind else {
      panic!("Node.prototype.ELEMENT_NODE should be a data property");
    };
    assert_eq!(value, Value::Number(1.0));
    assert!(!writable, "constants must be non-writable");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[derive(Default)]
  struct VmjsAlertHost {
    calls: Vec<usize>,
    messages: Vec<Option<String>>,
  }

  impl WebIdlBindingsHost for VmjsAlertHost {
    fn call_operation(
      &mut self,
      _vm: &mut Vm,
      scope: &mut Scope<'_>,
      _receiver: Option<Value>,
      interface: &'static str,
      operation: &'static str,
      overload: usize,
      args: &[Value],
    ) -> Result<Value, VmError> {
      match (interface, operation) {
        ("Window", "alert") => {
          self.calls.push(overload);
          if args.is_empty() {
            self.messages.push(None);
          } else {
            let Value::String(s) = args[0] else {
              return Err(VmError::TypeError("expected alert message to be a string"));
            };
            self
              .messages
              .push(Some(scope.heap().get_string(s)?.to_utf8_lossy()));
          }
          Ok(Value::Undefined)
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
      Err(VmError::Unimplemented("unimplemented host constructor"))
    }
  }

  #[test]
  fn vmjs_bindings_dispatch_window_alert_overloads() -> Result<(), VmError> {
    let limits = HeapLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let result =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), VmError> {
        let mut host = VmjsAlertHost::default();
        install_window_bindings_vm_js(&mut vm, &mut heap, &realm)?;

        let mut hooks = HostHooksWithBindingsHost::new(&mut host);
        let mut dummy_host = ();
        let mut scope = heap.scope();

        let global = realm.global_object();
        scope.push_root(Value::Object(global))?;
        let alert_key = alloc_key(&mut scope, "alert")?;
        let alert = scope
          .heap()
          .object_get_own_data_property_value(global, &alert_key)?
          .expect("globalThis.alert should be defined");

        // alert()
        vm.call_with_host_and_hooks(
          &mut dummy_host,
          &mut scope,
          &mut hooks,
          alert,
          Value::Undefined,
          &[],
        )?;

        // alert("hi")
        let hi_str = scope.alloc_string("hi")?;
        scope.push_root(Value::String(hi_str))?;
        let hi = Value::String(hi_str);
        vm.call_with_host_and_hooks(
          &mut dummy_host,
          &mut scope,
          &mut hooks,
          alert,
          Value::Undefined,
          &[hi],
        )?;

        // alert("a", "b") -> overload resolution ignores extra arguments.
        let a_str = scope.alloc_string("a")?;
        scope.push_root(Value::String(a_str))?;
        let b_str = scope.alloc_string("b")?;
        scope.push_root(Value::String(b_str))?;
        let a = Value::String(a_str);
        let b = Value::String(b_str);
        vm.call_with_host_and_hooks(
          &mut dummy_host,
          &mut scope,
          &mut hooks,
          alert,
          Value::Undefined,
          &[a, b],
        )?;

        // alert(123) -> dispatch selects the 1-arg overload and converts via WebIDL.
        vm.call_with_host_and_hooks(
          &mut dummy_host,
          &mut scope,
          &mut hooks,
          alert,
          Value::Undefined,
          &[Value::Number(123.0)],
        )?;

        assert_eq!(host.calls, vec![0, 1, 1, 1]);
        assert_eq!(
          host.messages,
          vec![
            None,
            Some("hi".to_string()),
            Some("a".to_string()),
            Some("123".to_string()),
          ]
        );
        Ok(())
      }));

    realm.teardown(&mut heap);
    match result {
      Ok(result) => result,
      Err(panic) => std::panic::resume_unwind(panic),
    }
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
  struct UrlSearchParamsConstructorHost {
    calls: usize,
  }

  impl<'a> WebHostBindings<VmJsWebIdlBindingsCx<'a, UrlSearchParamsConstructorHost>>
    for UrlSearchParamsConstructorHost
  {
    fn call_operation(
      &mut self,
      _rt: &mut VmJsWebIdlBindingsCx<'a, UrlSearchParamsConstructorHost>,
      receiver: Option<Value>,
      interface: &'static str,
      operation: &'static str,
      _overload: usize,
      _args: Vec<BindingValue<Value>>,
    ) -> Result<BindingValue<Value>, VmError> {
      match (interface, operation) {
        ("URLSearchParams", "constructor") => {
          self.calls += 1;
          let _ = receiver;
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
    fn value_to_rust_string<'a>(
      rt: &mut VmJsWebIdlBindingsCx<'a, Self>,
      host: &mut Self,
      value: Value,
    ) -> Result<String, VmError> {
      // Disambiguate `to_string` between the legacy `webidl_js_runtime::JsRuntime` surface (used by
      // `generated_legacy`) and the `vm-js` bindings runtime (`WebIdlBindingsRuntime`).
      let s = WebIdlBindingsRuntime::to_string(rt, host, value)?;
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

  impl<'a> WebHostBindings<VmJsWebIdlBindingsCx<'a, AttributeAndConstHost>>
    for AttributeAndConstHost
  {
    fn call_operation(
      &mut self,
      rt: &mut VmJsWebIdlBindingsCx<'a, AttributeAndConstHost>,
      receiver: Option<Value>,
      interface: &'static str,
      operation: &'static str,
      _overload: usize,
      args: Vec<BindingValue<Value>>,
    ) -> Result<BindingValue<Value>, VmError> {
      match (interface, operation) {
        ("URLSearchParams", "constructor") => {
          let Some(Value::Object(obj_handle)) = receiver else {
            return Err(rt.throw_type_error(
              "URLSearchParams constructor called without wrapper object receiver",
            ));
          };

          let mut init = args.into_iter().next().unwrap_or(BindingValue::Undefined);
          // Generated bindings preserve union member selection by wrapping the converted argument in
          // `BindingValue::Union`. For this test host we only care about the resolved value.
          while let BindingValue::Union { value, .. } = init {
            init = *value;
          }

          let params = match init {
            BindingValue::Undefined | BindingValue::Null => UrlSearchParams::new(&self.limits),
            BindingValue::String(s) => {
              if s.is_empty() {
                UrlSearchParams::new(&self.limits)
              } else {
                UrlSearchParams::parse(&s, &self.limits)
                  .map_err(|_| rt.throw_type_error("URLSearchParams constructor failed"))?
              }
            }
            BindingValue::Sequence(values) => {
              // `sequence<sequence<USVString>>`: interpret as list of [name, value] pairs.
              let params = UrlSearchParams::new(&self.limits);
              for item in values {
                let pair = match item {
                  BindingValue::Sequence(pair) | BindingValue::FrozenArray(pair) => pair,
                  _ => {
                    return Err(rt.throw_type_error(
                      "URLSearchParams constructor init sequence contains a non-sequence item",
                    ));
                  }
                };
                if pair.len() != 2 {
                  return Err(rt.throw_type_error(
                    "URLSearchParams constructor init sequence items must have length 2",
                  ));
                }
                let BindingValue::String(name) = &pair[0] else {
                  return Err(rt.throw_type_error(
                    "URLSearchParams constructor init pair name must be a string",
                  ));
                };
                let BindingValue::String(value) = &pair[1] else {
                  return Err(rt.throw_type_error(
                    "URLSearchParams constructor init pair value must be a string",
                  ));
                };
                params
                  .append(name, value)
                  .map_err(|_| rt.throw_type_error("URLSearchParams constructor failed"))?;
              }
              params
            }
            BindingValue::Record(entries) => {
              // `record<USVString, USVString>`: append each key/value pair in `[[OwnPropertyKeys]]`
              // order.
              let params = UrlSearchParams::new(&self.limits);
              for (name, value) in entries {
                let value = match value {
                  BindingValue::String(value) => value,
                  BindingValue::Object(v) => Self::value_to_rust_string(rt, self, v)?,
                  _ => {
                    return Err(rt.throw_type_error(
                      "URLSearchParams constructor init record values must be strings",
                    ));
                  }
                };
                params
                  .append(&name, &value)
                  .map_err(|_| rt.throw_type_error("URLSearchParams constructor failed"))?;
              }
              params
            }
            BindingValue::Dictionary(map) => {
              // Legacy test hosts still accept `BindingValue::Dictionary` for record conversions.
              // Newer bindings use `BindingValue::Record` to preserve property order.
              let params = UrlSearchParams::new(&self.limits);
              for (name, value) in map {
                let value = match value {
                  BindingValue::String(value) => value,
                  BindingValue::Object(v) => Self::value_to_rust_string(rt, self, v)?,
                  _ => {
                    return Err(rt.throw_type_error(
                      "URLSearchParams constructor init record values must be strings",
                    ));
                  }
                };
                params
                  .append(&name, &value)
                  .map_err(|_| rt.throw_type_error("URLSearchParams constructor failed"))?;
              }
              params
            }
            BindingValue::Object(v) => {
              // Fallback: treat as string (legacy behaviour); union conversions should avoid this
              // branch by converting object inputs to sequence/record when appropriate.
              let init = Self::value_to_rust_string(rt, self, v)?;
              if init.is_empty() {
                UrlSearchParams::new(&self.limits)
              } else {
                UrlSearchParams::parse(&init, &self.limits)
                  .map_err(|_| rt.throw_type_error("URLSearchParams constructor failed"))?
              }
            }
            other => {
              return Err(rt.throw_type_error(&format!(
                "URLSearchParams constructor init has invalid type: {other:?}"
              )));
            }
          };

          self.params.insert(WeakGcObject::from(obj_handle), params);
          Ok(BindingValue::Undefined)
        }
        ("URLSearchParams", "get") => {
          let name = match args.get(0) {
            Some(BindingValue::String(s)) => s.clone(),
            Some(BindingValue::Object(v)) => Self::value_to_rust_string(rt, self, *v)?,
            _ => String::new(),
          };
          let params = self.require_params(rt, receiver)?;
          match params
            .get(&name)
            .map_err(|_| rt.throw_type_error("URLSearchParams.get failed"))?
          {
            None => Ok(BindingValue::Null),
            Some(v) => Ok(BindingValue::String(v)),
          }
        }
        ("URL", "constructor") => {
          let Some(Value::Object(obj_handle)) = receiver else {
            return Err(
              rt.throw_type_error("URL constructor called without wrapper object receiver"),
            );
          };

          let href = match args.get(0) {
            Some(BindingValue::String(s)) => s.clone(),
            Some(BindingValue::Object(v)) => Self::value_to_rust_string(rt, self, *v)?,
            _ => String::new(),
          };
          self.urls.insert(WeakGcObject::from(obj_handle), href);
          Ok(BindingValue::Undefined)
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
            BindingValue::Object(v) => Self::value_to_rust_string(rt, self, v)?,
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

    // Keep `state` alive until after `teardown` so native dispatch pointers stored on function
    // objects remain valid for the lifetime of the realm.
    let state = Box::new(VmJsWebIdlBindingsState::<AttributeAndConstHost>::new(
      realm.global_object(),
      WebIdlLimits::default(),
      Box::new(NoHooks),
    ));

    let result =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), VmError> {
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
        let size_val =
          vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, get, params_val, &[])?;
        assert_eq!(size_val, Value::Number(2.0));

        // Calling the getter with an invalid receiver should throw a TypeError("Illegal invocation").
        {
          let err = vm
            .call_with_host_and_hooks(
              &mut host,
              &mut scope,
              &mut hooks,
              get,
              Value::Undefined,
              &[],
            )
            .expect_err(
              "expected Illegal invocation error for URLSearchParams.prototype.size getter",
            );
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
          assert_eq!(
            scope.heap().get_string(name_s)?.to_utf8_lossy(),
            "TypeError"
          );
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
        vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, set, url_val, &[new_href])?;
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
          vm.call_with_host_and_hooks(
            &mut host,
            &mut scope,
            &mut hooks,
            node_ctor,
            Value::Undefined,
            &[],
          )
          .expect_err("expected Node() to throw"),
          vm.construct_with_host_and_hooks(
            &mut host,
            &mut scope,
            &mut hooks,
            node_ctor,
            &[],
            node_ctor,
          )
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
          assert_eq!(
            scope.heap().get_string(name_s)?.to_utf8_lossy(),
            "TypeError"
          );
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

        // Constants must also be exposed on the interface prototype object.
        let node_proto_val = scope
          .heap()
          .object_get_own_data_property_value(node_ctor_obj, &proto_key)?
          .expect("Node.prototype should be defined");
        scope.push_root(node_proto_val)?;
        let Value::Object(node_proto_obj) = node_proto_val else {
          panic!("Node.prototype should be an object");
        };
        let Some(proto_constant_desc) = scope
          .heap()
          .object_get_own_property(node_proto_obj, &element_node_key)?
        else {
          panic!("Node.prototype.ELEMENT_NODE should be defined");
        };
        assert!(
          proto_constant_desc.enumerable,
          "constants must be enumerable on prototypes"
        );
        assert!(
          !proto_constant_desc.configurable,
          "constants must be non-configurable on prototypes"
        );
        let PropertyKind::Data {
          value: proto_value,
          writable: proto_writable,
        } = proto_constant_desc.kind
        else {
          panic!("Node.prototype.ELEMENT_NODE should be a data property");
        };
        assert_eq!(proto_value, Value::Number(1.0));
        assert!(
          !proto_writable,
          "constants must be non-writable on prototypes"
        );
        Ok(())
      }));

    realm.teardown(&mut heap);
    match result {
      Ok(result) => result,
      Err(panic) => std::panic::resume_unwind(panic),
    }
  }

  #[test]
  fn generated_bindings_url_search_params_constructor_accepts_sequence_and_record_init(
  ) -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let state = Box::new(VmJsWebIdlBindingsState::<AttributeAndConstHost>::new(
      realm.global_object(),
      WebIdlLimits::default(),
      Box::new(NoHooks),
    ));

    let result = (|| -> Result<(), VmError> {
      let mut host = AttributeAndConstHost::default();
      {
        let mut rt = VmJsWebIdlBindingsCx::new(&mut vm, &mut heap, &state);
        install_window_bindings(&mut rt, &mut host)?;
      }

      let mut hooks = MicrotaskQueue::new();
      let mut scope = heap.scope();
      let intr = vm
        .intrinsics()
        .expect("vm-js intrinsics should be installed after Realm::new");
      let array_proto = intr.array_prototype();
      scope.push_root(Value::Object(array_proto))?;

      let global = realm.global_object();
      scope.push_root(Value::Object(global))?;

      let params_ctor_key = alloc_key(&mut scope, "URLSearchParams")?;
      let params_ctor = scope
        .heap()
        .object_get_own_data_property_value(global, &params_ctor_key)?
        .expect("globalThis.URLSearchParams should be defined");

      // --- sequence<sequence<USVString>> init ---
      let idx0 = alloc_key(&mut scope, "0")?;
      let idx1 = alloc_key(&mut scope, "1")?;

      let outer = scope.alloc_array(2)?;
      scope.push_root(Value::Object(outer))?;
      scope
        .heap_mut()
        .object_set_prototype(outer, Some(array_proto))?;

      let pair0 = scope.alloc_array(2)?;
      scope.push_root(Value::Object(pair0))?;
      scope
        .heap_mut()
        .object_set_prototype(pair0, Some(array_proto))?;
      let a_s = scope.alloc_string("a")?;
      scope.push_root(Value::String(a_s))?;
      let b_s = scope.alloc_string("b")?;
      scope.push_root(Value::String(b_s))?;
      scope.create_data_property_or_throw(pair0, idx0, Value::String(a_s))?;
      scope.create_data_property_or_throw(pair0, idx1, Value::String(b_s))?;

      let pair1 = scope.alloc_array(2)?;
      scope.push_root(Value::Object(pair1))?;
      scope
        .heap_mut()
        .object_set_prototype(pair1, Some(array_proto))?;
      let c_s = scope.alloc_string("c")?;
      scope.push_root(Value::String(c_s))?;
      let d_s = scope.alloc_string("d")?;
      scope.push_root(Value::String(d_s))?;
      scope.create_data_property_or_throw(pair1, idx0, Value::String(c_s))?;
      scope.create_data_property_or_throw(pair1, idx1, Value::String(d_s))?;

      scope.create_data_property_or_throw(outer, idx0, Value::Object(pair0))?;
      scope.create_data_property_or_throw(outer, idx1, Value::Object(pair1))?;

      let params_val = vm.construct_with_host_and_hooks(
        &mut host,
        &mut scope,
        &mut hooks,
        params_ctor,
        &[Value::Object(outer)],
        params_ctor,
      )?;
      scope.push_root(params_val)?;
      let Value::Object(params_obj) = params_val else {
        panic!("URLSearchParams constructor should return an object");
      };

      let get_key = alloc_key(&mut scope, "get")?;
      let get = vm.get(&mut scope, params_obj, get_key)?;

      let out = vm.call_with_host_and_hooks(
        &mut host,
        &mut scope,
        &mut hooks,
        get,
        params_val,
        &[Value::String(a_s)],
      )?;
      let Value::String(out_s) = out else {
        panic!("URLSearchParams.get should return a string for existing key");
      };
      assert_eq!(scope.heap().get_string(out_s)?.to_utf8_lossy(), "b");

      let out = vm.call_with_host_and_hooks(
        &mut host,
        &mut scope,
        &mut hooks,
        get,
        params_val,
        &[Value::String(c_s)],
      )?;
      let Value::String(out_s) = out else {
        panic!("URLSearchParams.get should return a string for existing key");
      };
      assert_eq!(scope.heap().get_string(out_s)?.to_utf8_lossy(), "d");

      // --- record<USVString, USVString> init ---
      let record_obj = scope.alloc_object()?;
      scope.push_root(Value::Object(record_obj))?;
      let key_a = alloc_key(&mut scope, "a")?;
      let key_c = alloc_key(&mut scope, "c")?;
      let b2_s = scope.alloc_string("b")?;
      scope.push_root(Value::String(b2_s))?;
      let d2_s = scope.alloc_string("d")?;
      scope.push_root(Value::String(d2_s))?;
      scope.create_data_property_or_throw(record_obj, key_a, Value::String(b2_s))?;
      scope.create_data_property_or_throw(record_obj, key_c, Value::String(d2_s))?;

      let params_val = vm.construct_with_host_and_hooks(
        &mut host,
        &mut scope,
        &mut hooks,
        params_ctor,
        &[Value::Object(record_obj)],
        params_ctor,
      )?;
      scope.push_root(params_val)?;
      let Value::Object(params_obj) = params_val else {
        panic!("URLSearchParams constructor should return an object");
      };

      let get = vm.get(&mut scope, params_obj, get_key)?;
      let out = vm.call_with_host_and_hooks(
        &mut host,
        &mut scope,
        &mut hooks,
        get,
        params_val,
        &[Value::String(a_s)],
      )?;
      let Value::String(out_s) = out else {
        panic!("URLSearchParams.get should return a string for existing key");
      };
      assert_eq!(scope.heap().get_string(out_s)?.to_utf8_lossy(), "b");

      // Invalid sequence shape: inner item must have length 2.
      let outer_bad = scope.alloc_array(1)?;
      scope.push_root(Value::Object(outer_bad))?;
      scope
        .heap_mut()
        .object_set_prototype(outer_bad, Some(array_proto))?;
      let inner_bad = scope.alloc_array(1)?;
      scope.push_root(Value::Object(inner_bad))?;
      scope
        .heap_mut()
        .object_set_prototype(inner_bad, Some(array_proto))?;
      scope.create_data_property_or_throw(inner_bad, idx0, Value::String(a_s))?;
      scope.create_data_property_or_throw(outer_bad, idx0, Value::Object(inner_bad))?;

      let err = vm
        .construct_with_host_and_hooks(
          &mut host,
          &mut scope,
          &mut hooks,
          params_ctor,
          &[Value::Object(outer_bad)],
          params_ctor,
        )
        .expect_err("expected URLSearchParams constructor to throw on invalid init shape");
      let thrown = err.thrown_value().expect("expected a thrown error value");
      let Value::Object(thrown_obj) = thrown else {
        panic!("expected thrown error to be an object");
      };
      scope.push_root(thrown)?;
      let name_key = alloc_key(&mut scope, "name")?;
      let name_val = vm.get(&mut scope, thrown_obj, name_key)?;
      let Value::String(name_s) = name_val else {
        panic!("expected error.name to be a string");
      };
      assert_eq!(
        scope.heap().get_string(name_s)?.to_utf8_lossy(),
        "TypeError"
      );

      Ok(())
    })();
    realm.teardown(&mut heap);
    result
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

  fn thrown_error_name(
    rt: &mut webidl_js_runtime::VmJsRuntime,
    err: VmError,
  ) -> Result<String, VmError> {
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

    let global =
      <webidl_js_runtime::VmJsRuntime as crate::js::webidl_runtime_vmjs::WebIdlBindingsRuntime<
        GeneratedHost,
      >>::global_object(&mut rt)?;
    webidl_js_runtime::JsRuntime::with_stack_roots(&mut rt, &[global], |rt| {
      let key = rt.property_key_from_str("queueMicrotask")?;
      let func = webidl_js_runtime::JsRuntime::get(rt, global, key)?;
      let err = rt
        .with_host_context(&mut host, |rt| {
          webidl_js_runtime::JsRuntime::call(
            rt,
            func,
            webidl_js_runtime::JsRuntime::js_undefined(rt),
            &[Value::Number(1.0)],
          )
        })
        .expect_err("expected queueMicrotask to throw on non-callable");
      assert_eq!(thrown_error_name(rt, err)?, "TypeError");
      assert_eq!(
        host.calls, 0,
        "host must not be called on conversion failure"
      );
      Ok(())
    })
  }

  #[test]
  fn generated_bindings_add_event_listener_rejects_non_object_callback_interface(
  ) -> Result<(), VmError> {
    let mut rt = webidl_js_runtime::VmJsRuntime::new();
    let mut host = GeneratedHost::default();
    install_window_bindings(&mut rt, &mut host)?;

    let global =
      <webidl_js_runtime::VmJsRuntime as crate::js::webidl_runtime_vmjs::WebIdlBindingsRuntime<
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
            webidl_js_runtime::JsRuntime::call(rt, add, this_obj, &[type_str, Value::Number(1.0)])
          })
          .expect_err("expected addEventListener to throw on non-object callback");
        assert_eq!(thrown_error_name(rt, err)?, "TypeError");
        assert_eq!(
          host.calls, 0,
          "host must not be called on conversion failure"
        );
        Ok(())
      })
    })
  }

  #[test]
  fn generated_bindings_url_origin_getter_dispatches_to_host() -> Result<(), VmError> {
    #[derive(Default)]
    struct UrlOriginHost {
      calls: usize,
      last_interface: Option<&'static str>,
      last_name: Option<&'static str>,
    }

    impl WebHostBindings<webidl_js_runtime::VmJsRuntime> for UrlOriginHost {
      fn call_operation(
        &mut self,
        _rt: &mut webidl_js_runtime::VmJsRuntime,
        _receiver: Option<Value>,
        _interface: &'static str,
        _operation: &'static str,
        _overload: usize,
        _args: Vec<BindingValue<Value>>,
      ) -> Result<BindingValue<Value>, VmError> {
        Ok(BindingValue::Undefined)
      }

      fn get_attribute(
        &mut self,
        _rt: &mut webidl_js_runtime::VmJsRuntime,
        receiver: Option<Value>,
        interface: &'static str,
        name: &'static str,
      ) -> Result<BindingValue<Value>, VmError> {
        let _ = receiver;
        self.calls += 1;
        self.last_interface = Some(interface);
        self.last_name = Some(name);
        Ok(BindingValue::String("https://example.com".to_string()))
      }
    }

    let mut rt = webidl_js_runtime::VmJsRuntime::new();
    let mut host = UrlOriginHost::default();
    install_window_bindings(&mut rt, &mut host)?;

    let global =
      <webidl_js_runtime::VmJsRuntime as crate::js::webidl_runtime_vmjs::WebIdlBindingsRuntime<
        UrlOriginHost,
      >>::global_object(&mut rt)?;
    let origin = webidl_js_runtime::JsRuntime::with_stack_roots(&mut rt, &[global], |rt| {
      // globalThis.URL.prototype.origin
      let ctor_key = rt.property_key_from_str("URL")?;
      let ctor = webidl_js_runtime::JsRuntime::get(rt, global, ctor_key)?;
      webidl_js_runtime::JsRuntime::with_stack_roots(rt, &[ctor], |rt| {
        let proto_key = rt.property_key_from_str("prototype")?;
        let proto = webidl_js_runtime::JsRuntime::get(rt, ctor, proto_key)?;
        webidl_js_runtime::JsRuntime::with_stack_roots(rt, &[proto], |rt| {
          let origin_key = rt.property_key_from_str("origin")?;
          let origin_value = rt.with_host_context(&mut host, |rt| {
            webidl_js_runtime::JsRuntime::get(rt, proto, origin_key)
          })?;
          webidl_js_runtime::JsRuntime::with_stack_roots(rt, &[origin_value], |rt| {
            let s = webidl_js_runtime::JsRuntime::to_string(rt, origin_value)?;
            rt.string_to_utf8_lossy(s)
          })
        })
      })
    })?;

    assert_eq!(origin, "https://example.com");
    assert_eq!(host.calls, 1);
    assert_eq!(host.last_interface, Some("URL"));
    assert_eq!(host.last_name, Some("origin"));
    Ok(())
  }

  #[test]
  fn generated_bindings_dispatch_window_alert_overloads() -> Result<(), VmError> {
    let limits = HeapLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    // Keep `state` alive until after `teardown` so native dispatch pointers stored on function
    // objects remain valid for the lifetime of the realm.
    let state = Box::new(VmJsWebIdlBindingsState::<AlertHost>::new(
      realm.global_object(),
      WebIdlLimits::default(),
      Box::new(NoHooks),
    ));

    let result =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), VmError> {
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
        vm.call_with_host_and_hooks(
          &mut host,
          &mut scope,
          &mut hooks,
          alert,
          Value::Undefined,
          &[],
        )?;

        // alert("hi")
        let hi_str = scope.alloc_string("hi")?;
        scope.push_root(Value::String(hi_str))?;
        let hi = Value::String(hi_str);
        vm.call_with_host_and_hooks(
          &mut host,
          &mut scope,
          &mut hooks,
          alert,
          Value::Undefined,
          &[hi],
        )?;

        // alert("a", "b") -> dispatch uses min(args.len(), maxarg) so should still pick overload #1.
        let a_str = scope.alloc_string("a")?;
        scope.push_root(Value::String(a_str))?;
        let b_str = scope.alloc_string("b")?;
        scope.push_root(Value::String(b_str))?;
        let a = Value::String(a_str);
        let b = Value::String(b_str);
        vm.call_with_host_and_hooks(
          &mut host,
          &mut scope,
          &mut hooks,
          alert,
          Value::Undefined,
          &[a, b],
        )?;

        assert_eq!(host.calls, vec![0, 1, 1]);
        Ok(())
      }));

    realm.teardown(&mut heap);
    match result {
      Ok(result) => result,
      Err(panic) => std::panic::resume_unwind(panic),
    }
  }

  #[test]
  fn generated_bindings_call_with_host_recovers_host_from_hooks_payload() -> Result<(), VmError> {
    let limits = HeapLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    // Keep `state` alive until after `teardown` so native dispatch pointers stored on function
    // objects remain valid for the lifetime of the realm.
    let state = Box::new(VmJsWebIdlBindingsState::<AlertHost>::new(
      realm.global_object(),
      WebIdlLimits::default(),
      Box::new(NoHooks),
    ));

    let result =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), VmError> {
        let mut host = AlertHost::default();
        {
          let mut rt = VmJsWebIdlBindingsCx::new(&mut vm, &mut heap, &state);
          install_window_bindings(&mut rt, &mut host)?;
        }

        let mut hooks = HostHooksWithVmJsPayload::new(&mut host);
        let mut scope = heap.scope();

        let global = realm.global_object();
        scope.push_root(Value::Object(global))?;
        let alert_key = alloc_key(&mut scope, "alert")?;
        let alert = scope
          .heap()
          .object_get_own_data_property_value(global, &alert_key)?
          .expect("globalThis.alert should be defined");

        // `Vm::call_with_host` passes a dummy `VmHost` (`()`), so native dispatch must recover the
        // real embedder host state via `VmHostHooks::as_any_mut`.
        vm.call_with_host(&mut scope, &mut hooks, alert, Value::Undefined, &[])?;
        assert_eq!(host.calls, vec![0]);
        Ok(())
      }));

    realm.teardown(&mut heap);
    match result {
      Ok(result) => result,
      Err(panic) => std::panic::resume_unwind(panic),
    }
  }

  #[test]
  fn generated_bindings_construct_with_host_recovers_host_from_hooks_payload() -> Result<(), VmError>
  {
    let limits = HeapLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    // Keep `state` alive until after `teardown` so native dispatch pointers stored on function
    // objects remain valid for the lifetime of the realm.
    let state = Box::new(
      VmJsWebIdlBindingsState::<UrlSearchParamsConstructorHost>::new(
        realm.global_object(),
        WebIdlLimits::default(),
        Box::new(NoHooks),
      ),
    );

    let result =
      std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), VmError> {
        let mut host = UrlSearchParamsConstructorHost::default();
        {
          let mut rt = VmJsWebIdlBindingsCx::new(&mut vm, &mut heap, &state);
          install_window_bindings(&mut rt, &mut host)?;
        }

        let mut hooks = HostHooksWithVmJsPayload::new(&mut host);
        let mut scope = heap.scope();

        // globalThis.URLSearchParams
        let global = realm.global_object();
        scope.push_root(Value::Object(global))?;
        let ctor_key = alloc_key(&mut scope, "URLSearchParams")?;
        let ctor = scope
          .heap()
          .object_get_own_data_property_value(global, &ctor_key)?
          .expect("globalThis.URLSearchParams should be defined");
        scope.push_root(ctor)?;

        let init_str = scope.alloc_string("a=1")?;
        scope.push_root(Value::String(init_str))?;
        let init = Value::String(init_str);

        // Like `Vm::call_with_host`, `Vm::construct_with_host` passes a dummy `VmHost` (`()`).
        let params = vm.construct_with_host(&mut scope, &mut hooks, ctor, &[init], ctor)?;
        scope.push_root(params)?;
        let Value::Object(params_obj) = params else {
          return Err(VmError::TypeError(
            "URLSearchParams constructor should return an object",
          ));
        };

        assert_eq!(host.calls, 1);
        let _ = params_obj;
        Ok(())
      }));

    realm.teardown(&mut heap);
    match result {
      Ok(result) => result,
      Err(panic) => std::panic::resume_unwind(panic),
    }
  }
}
