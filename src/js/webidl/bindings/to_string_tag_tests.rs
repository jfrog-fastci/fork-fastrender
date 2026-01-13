use super::{
  install_document_bindings_vm_js, install_element_bindings_vm_js, install_event_target_bindings_vm_js,
  install_node_bindings_vm_js,
};
use super::generated::window::{
  install_dom_token_list_bindings_vm_js, install_html_collection_bindings_vm_js,
  install_node_list_bindings_vm_js,
};
use std::any::Any;
use vm_js::{GcObject, Heap, HeapLimits, Job, JsRuntime, Realm, Scope, Value, Vm, VmError, VmHostHooks, VmOptions};
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

struct DummyDomCollectionsHost {
  element_proto: GcObject,
  node_list_proto: GcObject,
  html_collection_proto: GcObject,
  dom_token_list_proto: GcObject,
}

impl DummyDomCollectionsHost {
  fn new(
    vm: &mut Vm,
    heap: &mut Heap,
    realm: &Realm,
  ) -> Result<Self, VmError> {
    Ok(Self {
      element_proto: get_prototype(vm, heap, realm, "Element")?,
      node_list_proto: get_prototype(vm, heap, realm, "NodeList")?,
      html_collection_proto: get_prototype(vm, heap, realm, "HTMLCollection")?,
      dom_token_list_proto: get_prototype(vm, heap, realm, "DOMTokenList")?,
    })
  }
}

impl WebIdlBindingsHost for DummyDomCollectionsHost {
  fn call_operation(
    &mut self,
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    _overload: usize,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    match (interface, operation) {
      ("Document", "querySelectorAll") => {
        let obj = scope.alloc_object_with_prototype(Some(self.node_list_proto))?;
        Ok(Value::Object(obj))
      }
      ("Document", "body") => {
        let obj = scope.alloc_object_with_prototype(Some(self.element_proto))?;
        Ok(Value::Object(obj))
      }
      ("Document", "createElement") => {
        let obj = scope.alloc_object_with_prototype(Some(self.element_proto))?;
        Ok(Value::Object(obj))
      }
      ("Element", "children") => {
        let obj = scope.alloc_object_with_prototype(Some(self.html_collection_proto))?;
        Ok(Value::Object(obj))
      }
      ("Element", "classList") => {
        let obj = scope.alloc_object_with_prototype(Some(self.dom_token_list_proto))?;
        Ok(Value::Object(obj))
      }
      _ => Err(VmError::TypeError("unimplemented WebIDL host operation")),
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

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<vm_js::PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(vm_js::PropertyKey::from_string(s))
}

fn get_prototype(vm: &mut Vm, heap: &mut Heap, realm: &Realm, ctor_name: &str) -> Result<GcObject, VmError> {
  let global = realm.global_object();
  let mut scope = heap.scope();
  scope.push_root(Value::Object(global))?;

  let ctor_key = alloc_key(&mut scope, ctor_name)?;
  let ctor = vm.get(&mut scope, global, ctor_key)?;
  scope.push_root(ctor)?;
  let Value::Object(ctor_obj) = ctor else {
    return Err(VmError::TypeError("expected constructor to be an object"));
  };

  let proto_key = alloc_key(&mut scope, "prototype")?;
  let proto = vm.get(&mut scope, ctor_obj, proto_key)?;
  let Value::Object(proto_obj) = proto else {
    return Err(VmError::TypeError("expected prototype to be an object"));
  };
  Ok(proto_obj)
}

fn value_to_utf8(scope: &mut Scope<'_>, value: Value) -> Result<String, VmError> {
  let s = scope.heap_mut().to_string(value)?;
  Ok(scope.heap().get_string(s)?.to_utf8_lossy())
}

#[test]
fn webidl_dom_collections_have_to_string_tag() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Install only the WebIDL bindings that we need for this test.
  {
    let (vm, realm, heap) = rt.vm_realm_and_heap_mut();
    install_event_target_bindings_vm_js(vm, heap, realm)?;
    install_node_bindings_vm_js(vm, heap, realm)?;
    install_element_bindings_vm_js(vm, heap, realm)?;
    install_document_bindings_vm_js(vm, heap, realm)?;
    install_node_list_bindings_vm_js(vm, heap, realm)?;
    install_html_collection_bindings_vm_js(vm, heap, realm)?;
    install_dom_token_list_bindings_vm_js(vm, heap, realm)?;
  }

  // Snapshot the generated prototype objects so our dummy host can create wrapper objects branded
  // by them.
  let mut webidl_host = {
    let (vm, realm, heap) = rt.vm_realm_and_heap_mut();
    DummyDomCollectionsHost::new(vm, heap, realm)?
  };
  let mut hooks = HostHooksWithBindingsHost::new(&mut webidl_host);

  // The WebIDL generated native call wrappers look up their embedding via `host_from_hooks`, so the
  // VM-level host context can be a dummy.
  let mut vm_host = ();

  let out = rt.exec_script_with_host_and_hooks(
    &mut vm_host,
    &mut hooks,
    r#"
      (() => {
        // Create a Document object branded by the generated Document.prototype.
        globalThis.document = Object.create(Document.prototype);

        const a = Object.prototype.toString.call(document.querySelectorAll('div'));
        const b = Object.prototype.toString.call(document.body.children);
        const c = Object.prototype.toString.call(document.createElement('div').classList);
        return a + "|" + b + "|" + c;
      })()
    "#,
  )?;

  let mut scope = rt.heap.scope();
  scope.push_root(out)?;
  assert_eq!(
    value_to_utf8(&mut scope, out)?,
    "[object NodeList]|[object HTMLCollection]|[object DOMTokenList]"
  );
  Ok(())
}

