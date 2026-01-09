use crate::dom2::{DomError, Document, NodeId, NodeKind};
use crate::js::CurrentScriptState;
use crate::web::dom::DomException;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use vm_js::{
  GcObject, GcSymbol, Heap, NativeFunctionId, PropertyDescriptor, PropertyKey, PropertyKind, Realm,
  Scope, Value, Vm, VmError, VmHostHooks, WeakGcObject,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DomKind {
  Node = 0,
  Element = 1,
  Document = 2,
}

impl DomKind {
  fn from_number(n: f64) -> Option<Self> {
    if !n.is_finite() || n.fract() != 0.0 {
      return None;
    }
    match n as i32 {
      0 => Some(Self::Node),
      1 => Some(Self::Element),
      2 => Some(Self::Document),
      _ => None,
    }
  }

  fn as_number(self) -> f64 {
    self as u8 as f64
  }
}

fn dom_kind_for_node_kind(kind: &NodeKind) -> DomKind {
  match kind {
    NodeKind::Document { .. } => DomKind::Document,
    NodeKind::Element { .. } | NodeKind::Slot { .. } => DomKind::Element,
    _ => DomKind::Node,
  }
}

fn data_desc(value: Value, writable: bool, enumerable: bool, configurable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable,
    configurable,
    kind: PropertyKind::Data { value, writable },
  }
}

fn hidden_desc(value: Value) -> PropertyDescriptor {
  // Hidden metadata properties should not be observable via enumeration and should not be
  // user-mutable.
  data_desc(value, /* writable */ false, /* enumerable */ false, /* configurable */ false)
}

fn method_desc(value: Value) -> PropertyDescriptor {
  data_desc(value, /* writable */ true, /* enumerable */ false, /* configurable */ true)
}

fn accessor_desc(get: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Accessor {
      get,
      set: Value::Undefined,
    },
  }
}

pub struct DomHost {
  dom: Rc<RefCell<Document>>,
  current_script: Rc<RefCell<CurrentScriptState>>,

  // Identity cache: preserve wrapper identity without keeping wrappers alive.
  node_wrappers: HashMap<NodeId, WeakGcObject>,

  // Hidden metadata keys stored on each wrapper.
  sym_dom_kind: GcSymbol,
  sym_node_id: GcSymbol,

  // Cached prototypes.
  proto_node: GcObject,
  proto_element: GcObject,
  proto_document: GcObject,

  // Cached prototypes for thrown objects.
  error_prototype: GcObject,
  type_error_prototype: GcObject,
}

fn host_mut(vm: &mut Vm) -> Result<&mut DomHost, VmError> {
  vm.user_data_mut::<DomHost>()
    .ok_or(VmError::Unimplemented("DOM bindings not installed (missing DomHost user_data)"))
}

fn require_string<'a>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  value: Value,
  what: &'static str,
) -> Result<String, VmError> {
  let s = match value {
    Value::String(s) => s,
    _ => return throw_type_error(scope, host, &format!("{what} must be a string")),
  };
  Ok(scope.heap().get_string(s)?.to_utf8_lossy())
}

fn wrapper_meta<'a>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  value: Value,
) -> Result<(DomKind, NodeId), VmError> {
  let obj = match value {
    Value::Object(obj) => obj,
    _ => return throw_type_error(scope, host, "receiver is not an object"),
  };

  let kind_val = scope
    .heap()
    .object_get_own_data_property_value(obj, &PropertyKey::from_symbol(host.sym_dom_kind))?;
  let Some(kind_val) = kind_val else {
    return throw_type_error(scope, host, "receiver is not a DOM wrapper object");
  };

  let node_id_val = scope
    .heap()
    .object_get_own_data_property_value(obj, &PropertyKey::from_symbol(host.sym_node_id))?;
  let Some(node_id_val) = node_id_val else {
    return throw_type_error(scope, host, "receiver is not a DOM wrapper object");
  };

  let kind = match kind_val {
    Value::Number(n) => {
      let Some(kind) = DomKind::from_number(n) else {
        return throw_type_error(scope, host, "receiver is not a DOM wrapper object");
      };
      kind
    }
    _ => return throw_type_error(scope, host, "receiver is not a DOM wrapper object"),
  };

  let node_idx = match node_id_val {
    Value::Number(n) => {
      if !n.is_finite() || n.fract() != 0.0 || n < 0.0 {
        return throw_type_error(scope, host, "invalid node id on wrapper");
      }
      // NodeId indices are usize; DOM trees are far smaller than 2^53 in practice, but reject
      // values that cannot be represented exactly.
      if n > (usize::MAX as f64) {
        return throw_type_error(scope, host, "invalid node id on wrapper");
      }
      n as usize
    }
    _ => return throw_type_error(scope, host, "receiver is not a DOM wrapper object"),
  };

  let node_id = NodeId::from_index(node_idx);
  if node_id.index() >= host.dom.borrow().nodes_len() {
    return throw_type_error(scope, host, "receiver refers to an unknown DOM node");
  }

  Ok((kind, node_id))
}

fn require_this_document<'a>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  this: Value,
) -> Result<NodeId, VmError> {
  let (kind, node_id) = wrapper_meta(scope, host, this)?;
  if kind != DomKind::Document {
    return throw_type_error(scope, host, "Document method called on incompatible receiver");
  }
  Ok(node_id)
}

fn require_this_element<'a>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  this: Value,
) -> Result<NodeId, VmError> {
  let (kind, node_id) = wrapper_meta(scope, host, this)?;
  if kind != DomKind::Element {
    return throw_type_error(scope, host, "Element method called on incompatible receiver");
  }
  Ok(node_id)
}

fn require_this_node<'a>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  this: Value,
) -> Result<NodeId, VmError> {
  let (kind, node_id) = wrapper_meta(scope, host, this)?;
  match kind {
    DomKind::Node | DomKind::Element | DomKind::Document => Ok(node_id),
  }
}

fn require_node_arg<'a>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  value: Value,
) -> Result<NodeId, VmError> {
  let (_kind, node_id) = wrapper_meta(scope, host, value)?;
  Ok(node_id)
}

fn alloc_error_object<'a>(
  scope: &mut Scope<'a>,
  prototype: GcObject,
  name: &str,
  message: &str,
) -> Result<Value, VmError> {
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(prototype))?;

  let name_key = PropertyKey::from_string(scope.alloc_string("name")?);
  let message_key = PropertyKey::from_string(scope.alloc_string("message")?);

  let name_val = Value::String(scope.alloc_string(name)?);
  let message_val = Value::String(scope.alloc_string(message)?);

  scope.define_property(
    obj,
    name_key,
    data_desc(name_val, /* writable */ true, /* enumerable */ false, /* configurable */ true),
  )?;
  scope.define_property(
    obj,
    message_key,
    data_desc(message_val, /* writable */ true, /* enumerable */ false, /* configurable */ true),
  )?;

  Ok(Value::Object(obj))
}

fn throw_type_error<'a, T>(scope: &mut Scope<'a>, host: &DomHost, message: &str) -> Result<T, VmError> {
  let err = alloc_error_object(scope, host.type_error_prototype, "TypeError", message)?;
  Err(VmError::Throw(err))
}

fn throw_dom_exception<'a, T>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  name: &str,
  message: &str,
) -> Result<T, VmError> {
  let err = alloc_error_object(scope, host.error_prototype, name, message)?;
  Err(VmError::Throw(err))
}

fn throw_dom_error<'a, T>(scope: &mut Scope<'a>, host: &DomHost, err: DomError) -> Result<T, VmError> {
  throw_dom_exception(scope, host, err.code(), err.code())
}

fn throw_web_dom_exception<'a, T>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  err: DomException,
) -> Result<T, VmError> {
  match err {
    DomException::SyntaxError { message } => throw_dom_exception(scope, host, "SyntaxError", &message),
  }
}

fn wrap_node<'a>(
  host: &mut DomHost,
  scope: &mut Scope<'a>,
  node_id: NodeId,
  kind: DomKind,
) -> Result<Value, VmError> {
  if let Some(existing) = host
    .node_wrappers
    .get(&node_id)
    .copied()
    .and_then(|weak| weak.upgrade(scope.heap()))
  {
    return Ok(Value::Object(existing));
  }

  let wrapper = scope.alloc_object()?;
  scope.push_root(Value::Object(wrapper))?;

  let proto = match kind {
    DomKind::Node => host.proto_node,
    DomKind::Element => host.proto_element,
    DomKind::Document => host.proto_document,
  };
  scope.heap_mut().object_set_prototype(wrapper, Some(proto))?;

  scope.define_property(
    wrapper,
    PropertyKey::from_symbol(host.sym_dom_kind),
    hidden_desc(Value::Number(kind.as_number())),
  )?;
  scope.define_property(
    wrapper,
    PropertyKey::from_symbol(host.sym_node_id),
    hidden_desc(Value::Number(node_id.index() as f64)),
  )?;

  host.node_wrappers.insert(node_id, WeakGcObject::from(wrapper));
  Ok(Value::Object(wrapper))
}

// === Native call handlers ===

fn dom_document_create_element(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  require_this_document(scope, host, this)?;

  let Some(tag_name_val) = args.get(0).copied() else {
    return throw_type_error(scope, host, "createElement requires 1 argument");
  };
  let tag_name = require_string(scope, host, tag_name_val, "tagName")?;

  let node_id = host.dom.borrow_mut().create_element(&tag_name, "");
  wrap_node(host, scope, node_id, DomKind::Element)
}

fn dom_document_get_element_by_id(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  require_this_document(scope, host, this)?;

  let Some(id_val) = args.get(0).copied() else {
    return throw_type_error(scope, host, "getElementById requires 1 argument");
  };
  let id = require_string(scope, host, id_val, "id")?;

  let node_id = { host.dom.borrow().get_element_by_id(&id) };
  let Some(node_id) = node_id else {
    return Ok(Value::Null);
  };
  let kind = dom_kind_for_node_kind(&host.dom.borrow().node(node_id).kind);
  wrap_node(host, scope, node_id, kind)
}

fn dom_document_query_selector(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  require_this_document(scope, host, this)?;

  let Some(selectors_val) = args.get(0).copied() else {
    return throw_type_error(scope, host, "querySelector requires 1 argument");
  };
  let selectors = require_string(scope, host, selectors_val, "selectors")?;

  let result = host.dom.borrow_mut().query_selector(&selectors, None);
  match result {
    Ok(Some(node_id)) => {
      let kind = dom_kind_for_node_kind(&host.dom.borrow().node(node_id).kind);
      wrap_node(host, scope, node_id, kind)
    }
    Ok(None) => Ok(Value::Null),
    Err(err) => throw_web_dom_exception(scope, host, err),
  }
}

fn dom_node_append_child(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let parent = require_this_node(scope, host, this)?;

  let Some(child_val) = args.get(0).copied() else {
    return throw_type_error(scope, host, "appendChild requires 1 argument");
  };
  let child = require_node_arg(scope, host, child_val)?;

  if let Err(err) = host.dom.borrow_mut().append_child(parent, child) {
    return throw_dom_error(scope, host, err);
  }
  Ok(child_val)
}

fn dom_element_set_attribute(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_element(scope, host, this)?;

  let Some(name_val) = args.get(0).copied() else {
    return throw_type_error(scope, host, "setAttribute requires 2 arguments");
  };
  let Some(value_val) = args.get(1).copied() else {
    return throw_type_error(scope, host, "setAttribute requires 2 arguments");
  };
  let name = require_string(scope, host, name_val, "name")?;
  let value = require_string(scope, host, value_val, "value")?;

  if let Err(err) = host.dom.borrow_mut().set_attribute(node_id, &name, &value) {
    return throw_dom_error(scope, host, err);
  }
  Ok(Value::Undefined)
}

fn dom_document_current_script_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  require_this_document(scope, host, this)?;

  let current = host.current_script.borrow().current_script;
  let Some(node_id) = current else {
    return Ok(Value::Null);
  };
  let kind = dom_kind_for_node_kind(&host.dom.borrow().node(node_id).kind);
  wrap_node(host, scope, node_id, kind)
}

pub fn install_dom_bindings(
  vm: &mut Vm,
  heap: &mut Heap,
  realm: &Realm,
  dom: Rc<RefCell<Document>>,
  current_script: Rc<RefCell<CurrentScriptState>>,
) -> Result<(), VmError> {
  let mut scope = heap.scope();

  // Allocate hidden symbol keys first and root them until the document wrapper exists.
  let sym_dom_kind = scope.alloc_symbol(Some("fastrender.dom.kind"))?;
  let sym_node_id = scope.alloc_symbol(Some("fastrender.dom.nodeId"))?;
  scope.push_root(Value::Symbol(sym_dom_kind))?;
  scope.push_root(Value::Symbol(sym_node_id))?;

  // Prototype objects.
  let proto_node = scope.alloc_object()?;
  scope.push_root(Value::Object(proto_node))?;
  scope
    .heap_mut()
    .object_set_prototype(proto_node, Some(realm.intrinsics().object_prototype()))?;

  let proto_element = scope.alloc_object()?;
  scope.push_root(Value::Object(proto_element))?;
  scope
    .heap_mut()
    .object_set_prototype(proto_element, Some(proto_node))?;

  let proto_document = scope.alloc_object()?;
  scope.push_root(Value::Object(proto_document))?;
  scope
    .heap_mut()
    .object_set_prototype(proto_document, Some(proto_node))?;

  // Ensure `proto_element` stays alive even before any Element wrappers exist. Without this, a GC
  // cycle between `install_dom_bindings` and the first `createElement` could collect `proto_element`
  // because `DomHost` is not traced by the GC.
  let sym_proto_element_root = scope.alloc_symbol(Some("fastrender.dom.proto_element_root"))?;
  scope.push_root(Value::Symbol(sym_proto_element_root))?;
  scope.define_property(
    proto_node,
    PropertyKey::from_symbol(sym_proto_element_root),
    hidden_desc(Value::Object(proto_element)),
  )?;

  // Register native call handlers.
  let call_create_element = vm.register_native_call(dom_document_create_element)?;
  let call_get_element_by_id = vm.register_native_call(dom_document_get_element_by_id)?;
  let call_query_selector = vm.register_native_call(dom_document_query_selector)?;
  let call_append_child = vm.register_native_call(dom_node_append_child)?;
  let call_set_attribute = vm.register_native_call(dom_element_set_attribute)?;
  let call_current_script = vm.register_native_call(dom_document_current_script_getter)?;

  // Install methods/getters.
  install_method(&mut scope, proto_document, "createElement", call_create_element, 1)?;
  install_method(&mut scope, proto_document, "getElementById", call_get_element_by_id, 1)?;
  install_method(&mut scope, proto_document, "querySelector", call_query_selector, 1)?;
  install_method(&mut scope, proto_node, "appendChild", call_append_child, 1)?;
  install_method(&mut scope, proto_element, "setAttribute", call_set_attribute, 2)?;
  install_getter(&mut scope, proto_document, "currentScript", call_current_script)?;

  let mut host = DomHost {
    dom: dom.clone(),
    current_script: current_script.clone(),
    node_wrappers: HashMap::new(),
    sym_dom_kind,
    sym_node_id,
    proto_node,
    proto_element,
    proto_document,
    error_prototype: realm.intrinsics().error_prototype(),
    type_error_prototype: realm.intrinsics().type_error_prototype(),
  };

  // Create the single `document` wrapper and install it on the global object.
  let document_id = dom.borrow().root();
  let document_wrapper = wrap_node(&mut host, &mut scope, document_id, DomKind::Document)?;
  scope.push_root(document_wrapper)?;

  let global = realm.global_object();
  let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
  scope.define_property(
    global,
    key_document,
    data_desc(
      document_wrapper,
      /* writable */ false,
      /* enumerable */ false,
      /* configurable */ false,
    ),
  )?;

  vm.set_user_data(host);

  Ok(())
}

fn install_method(
  scope: &mut Scope<'_>,
  proto: GcObject,
  name: &str,
  call: NativeFunctionId,
  length: u32,
) -> Result<(), VmError> {
  let name_string = scope.alloc_string(name)?;
  let func = scope.alloc_native_function(call, None, name_string, length)?;
  scope.push_root(Value::Object(func))?;

  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  scope.define_property(proto, key, method_desc(Value::Object(func)))?;
  Ok(())
}

fn install_getter(
  scope: &mut Scope<'_>,
  proto: GcObject,
  name: &str,
  call: NativeFunctionId,
) -> Result<(), VmError> {
  let fn_name = format!("get {name}");
  let name_string = scope.alloc_string(&fn_name)?;
  let func = scope.alloc_native_function(call, None, name_string, 0)?;
  scope.push_root(Value::Object(func))?;

  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  scope.define_property(proto, key, accessor_desc(Value::Object(func)))?;
  Ok(())
}
