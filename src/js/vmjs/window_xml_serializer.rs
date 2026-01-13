//! DOM `XMLSerializer` bindings for the `vm-js` Window realm.
//!
//! ## DOMException semantics
//!
//! Per DOM Parsing and Serialization, `XMLSerializer.serializeToString` must throw an
//! **InvalidStateError DOMException** when a node cannot be serialized. Some older Window shims
//! throw plain `{ name, message }` objects; for `XMLSerializer` we ensure the thrown value inherits
//! from `DOMException.prototype` so callers (and future WPT coverage) can assert
//! `e instanceof DOMException`.

use crate::js::bindings::DomExceptionClassVmJs;
use crate::js::dom_internal_keys::{NODE_ID_KEY, WRAPPER_DOCUMENT_KEY};
use vm_js::{
  GcObject, Heap, NativeConstructId, NativeFunctionId, PropertyDescriptor, PropertyKey, PropertyKind,
  Realm, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
};

const XML_SERIALIZER_BRAND_KEY: &str = "__fastrender_xml_serializer";

// Native slot indices for `XMLSerializer.prototype.serializeToString`.
const SERIALIZE_SLOT_DOM_EXCEPTION_PROTO: usize = 0;

fn gc_object_id(obj: GcObject) -> u64 {
  (obj.index() as u64) | ((obj.generation() as u64) << 32)
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

fn proto_data_desc(value: Value) -> PropertyDescriptor {
  // Prototype properties are usually non-enumerable, writable, configurable.
  data_desc(value)
}

fn ctor_link_desc(value: Value) -> PropertyDescriptor {
  // `prototype` and `constructor` links are typically non-enumerable.
  PropertyDescriptor {
    enumerable: false,
    configurable: false,
    kind: PropertyKind::Data {
      value,
      writable: false,
    },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn dom_exception_proto_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots.get(SERIALIZE_SLOT_DOM_EXCEPTION_PROTO).copied() {
    Some(Value::Object(obj)) => Ok(obj),
    _ => Err(VmError::InvariantViolation(
      "XMLSerializer.serializeToString missing DOMException prototype slot",
    )),
  }
}

fn make_dom_exception_instance(
  scope: &mut Scope<'_>,
  prototype: GcObject,
  name: &str,
  message: &str,
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(prototype))?;

  let name_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(name_s))?;
  let message_s = scope.alloc_string(message)?;
  scope.push_root(Value::String(message_s))?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(prototype))?;

  let name_key = alloc_key(&mut scope, "name")?;
  let message_key = alloc_key(&mut scope, "message")?;

  scope.define_property(obj, name_key, data_desc(Value::String(name_s)))?;
  scope.define_property(obj, message_key, data_desc(Value::String(message_s)))?;

  Ok(Value::Object(obj))
}

fn xml_serializer_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("Illegal constructor"))
}

fn xml_serializer_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("XMLSerializer requires intrinsics"))?;

  let ctor = match new_target {
    Value::Object(obj) => obj,
    _ => callee,
  };

  let proto = {
    let prototype_key = alloc_key(scope, "prototype")?;
    scope
      .heap()
      .object_get_own_data_property_value(ctor, &prototype_key)?
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      })
      .unwrap_or(intr.object_prototype())
  };

  let obj = scope.alloc_object_with_prototype(Some(proto))?;
  scope.push_root(Value::Object(obj))?;

  // Brand this object as an XMLSerializer instance so methods can enforce "Illegal invocation".
  let brand_key = alloc_key(scope, XML_SERIALIZER_BRAND_KEY)?;
  scope.define_property(
    obj,
    brand_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Bool(true),
        writable: false,
      },
    },
  )?;

  Ok(Value::Object(obj))
}

fn require_xml_serializer_instance(scope: &mut Scope<'_>, this: Value) -> Result<GcObject, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let key = alloc_key(scope, XML_SERIALIZER_BRAND_KEY)?;
  let brand = scope
    .heap()
    .object_get_own_data_property_value(obj, &key)?
    .unwrap_or(Value::Undefined);
  if matches!(brand, Value::Bool(true)) {
    Ok(obj)
  } else {
    Err(VmError::TypeError("Illegal invocation"))
  }
}

fn xml_serializer_serialize_to_string_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let _serializer_obj = require_xml_serializer_instance(scope, this)?;

  let root = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(root_obj) = root else {
    return Err(VmError::TypeError(
      "XMLSerializer.serializeToString: root must be a Node",
    ));
  };

  // Determine which `dom2::Document` this wrapper belongs to.
  let wrapper_document_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(root_obj, &wrapper_document_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "XMLSerializer.serializeToString: root must be a Node",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let root_index = match scope
    .heap()
    .object_get_own_data_property_value(root_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "XMLSerializer.serializeToString: root must be a Node",
      ));
    }
  };

  let document_id = gc_object_id(document_obj);
  let Some(dom_ptr) = crate::js::window_realm::dom_ptr_for_document_id_read(vm, host, document_id) else {
    return Err(VmError::TypeError(
      "XMLSerializer.serializeToString requires a DOM-backed node",
    ));
  };
  // SAFETY: `dom_ptr` is valid for the duration of this native call.
  let dom = unsafe { dom_ptr.as_ref() };

  let node_id = dom
    .node_id_from_index(root_index)
    .map_err(|_| VmError::TypeError("XMLSerializer.serializeToString: root must be a Node"))?;

  let serialized = match dom.xml_serialize(node_id) {
    Ok(s) => s,
    Err(err) => {
      let (name, message) = match err {
        crate::web::dom::DomException::SyntaxError { message } => ("SyntaxError", message),
        crate::web::dom::DomException::NoModificationAllowedError { message } => {
          ("NoModificationAllowedError", message)
        }
        crate::web::dom::DomException::NotSupportedError { message } => ("NotSupportedError", message),
        crate::web::dom::DomException::InvalidStateError { message } => ("InvalidStateError", message),
      };

      let proto = dom_exception_proto_from_callee(scope, callee)?;
      let ex = make_dom_exception_instance(scope, proto, name, &message)?;
      return Err(VmError::Throw(ex));
    }
  };

  let out = scope.alloc_string(&serialized)?;
  Ok(Value::String(out))
}

/// Installs `XMLSerializer` on the realm global object.
pub fn install_window_xml_serializer_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
) -> Result<(), VmError> {
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // Ensure `DOMException` exists and capture its prototype for throws.
  let dom_exception = DomExceptionClassVmJs::install(vm, &mut scope, realm)?;
  scope.push_root(Value::Object(dom_exception.prototype))?;

  let func_proto = realm.intrinsics().function_prototype();

  // --- Prototype -------------------------------------------------------------------------------
  let proto = scope.alloc_object()?;
  scope.push_root(Value::Object(proto))?;
  scope
    .heap_mut()
    .object_set_prototype(proto, Some(realm.intrinsics().object_prototype()))?;

  // Prototype method: serializeToString(root)
  let serialize_call_id: NativeFunctionId =
    vm.register_native_call(xml_serializer_serialize_to_string_native)?;
  let serialize_name = scope.alloc_string("serializeToString")?;
  scope.push_root(Value::String(serialize_name))?;
  let serialize_slots = [Value::Object(dom_exception.prototype)];
  let serialize_func = scope.alloc_native_function_with_slots(
    serialize_call_id,
    None,
    serialize_name,
    1,
    &serialize_slots,
  )?;
  scope
    .heap_mut()
    .object_set_prototype(serialize_func, Some(func_proto))?;
  scope.push_root(Value::Object(serialize_func))?;
  let serialize_key = alloc_key(&mut scope, "serializeToString")?;
  scope.define_property(proto, serialize_key, proto_data_desc(Value::Object(serialize_func)))?;

  // --- Constructor -----------------------------------------------------------------------------
  let call_id: NativeFunctionId = vm.register_native_call(xml_serializer_ctor_call)?;
  let construct_id: NativeConstructId = vm.register_native_construct(xml_serializer_ctor_construct)?;
  let name = scope.alloc_string("XMLSerializer")?;
  scope.push_root(Value::String(name))?;
  let ctor = scope.alloc_native_function(call_id, Some(construct_id), name, 0)?;
  scope.push_root(Value::Object(ctor))?;
  scope.heap_mut().object_set_prototype(ctor, Some(func_proto))?;

  // Expose global.
  let ctor_key = alloc_key(&mut scope, "XMLSerializer")?;
  scope.define_property(global, ctor_key, data_desc(Value::Object(ctor)))?;

  // Link constructor <-> prototype.
  let prototype_key = alloc_key(&mut scope, "prototype")?;
  let constructor_key = alloc_key(&mut scope, "constructor")?;
  scope.define_property(ctor, prototype_key, ctor_link_desc(Value::Object(proto)))?;
  scope.define_property(proto, constructor_key, ctor_link_desc(Value::Object(ctor)))?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use crate::dom2;
  use crate::error::Result;
  use crate::js::WindowHost;
  use crate::resource::{FetchedResource, ResourceFetcher};
  use selectors::context::QuirksMode;
  use std::sync::Arc;
  use vm_js::Value;

  #[derive(Debug, Default)]
  struct NoFetchResourceFetcher;

  impl ResourceFetcher for NoFetchResourceFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      Err(crate::Error::Other(format!(
        "NoFetchResourceFetcher does not support fetch: {url}"
      )))
    }
  }

  fn make_host(dom: dom2::Document, document_url: impl Into<String>) -> Result<WindowHost> {
    WindowHost::new_with_fetcher(dom, document_url, Arc::new(NoFetchResourceFetcher))
  }

  #[test]
  fn serialize_to_string_throws_dom_exception_instance_for_invalid_comment() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.com/")?;

    let value = host.exec_script(
      r#"
      (() => {
        const bad = document.createComment('--');
        try {
          new XMLSerializer().serializeToString(bad);
          return false;
        } catch (e) {
          return e instanceof DOMException && e.name === 'InvalidStateError';
        }
      })()
      "#,
    )?;

    assert_eq!(value, Value::Bool(true));
    Ok(())
  }
}
