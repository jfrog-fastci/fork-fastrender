use crate::dom2::{Document, NodeId, NodeKind};
use rustc_hash::FxHashMap;
use vm_js::{
  GcObject, GcSymbol, PropertyDescriptor, PropertyKey, PropertyKind, RootId, Scope, Value, Vm,
  VmError, VmHostHooks, WeakGcObject,
};

#[derive(Debug)]
pub struct DomBindingsContext {
  dom: Document,

  /// `dom2::NodeId` → weak JS wrapper identity map.
  ///
  /// This must not keep JS wrappers alive; we only store weak handles.
  node_wrappers: FxHashMap<NodeId, WeakGcObject>,

  // Internal slot key used to store the underlying `NodeId` index on wrapper objects.
  node_id_symbol: Option<GcSymbol>,

  // Prototypes. These are rooted in the heap via `prototype_roots` because this host struct is not
  // traced by the GC.
  event_target_prototype: Option<GcObject>,
  node_prototype: Option<GcObject>,
  element_prototype: Option<GcObject>,
  document_prototype: Option<GcObject>,
  prototype_roots: Vec<RootId>,

  // Minimal EventTarget listener storage:
  // (target_node_id, type) -> roots to callback values.
  listeners: FxHashMap<(NodeId, String), Vec<RootId>>,
}

impl DomBindingsContext {
  pub fn new(dom: Document) -> Self {
    Self {
      dom,
      node_wrappers: FxHashMap::default(),
      node_id_symbol: None,
      event_target_prototype: None,
      node_prototype: None,
      element_prototype: None,
      document_prototype: None,
      prototype_roots: Vec::new(),
      listeners: FxHashMap::default(),
    }
  }

  pub fn dom(&self) -> &Document {
    &self.dom
  }

  pub fn dom_mut(&mut self) -> &mut Document {
    &mut self.dom
  }

  pub fn init(&mut self, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<(), VmError> {
    if self.node_id_symbol.is_some() {
      return Ok(());
    }

    // Create a stable symbol for the "internal slot" that stores the underlying node id.
    let key = scope.alloc_string("fastrender.dom.NodeId")?;
    let sym = scope.heap_mut().symbol_for(key)?;
    self.node_id_symbol = Some(sym);

    let event_target_proto = scope.alloc_object()?;
    scope.push_root(Value::Object(event_target_proto))?;
    let node_proto = scope.alloc_object()?;
    scope.push_root(Value::Object(node_proto))?;
    let element_proto = scope.alloc_object()?;
    scope.push_root(Value::Object(element_proto))?;
    let document_proto = scope.alloc_object()?;
    scope.push_root(Value::Object(document_proto))?;

    // Prototype chain:
    // Document/Element -> Node -> EventTarget -> null
    scope
      .heap_mut()
      .object_set_prototype(node_proto, Some(event_target_proto))?;
    scope
      .heap_mut()
      .object_set_prototype(element_proto, Some(node_proto))?;
    scope
      .heap_mut()
      .object_set_prototype(document_proto, Some(node_proto))?;

    // Root the prototypes so they remain live for the lifetime of this context.
    for obj in [
      event_target_proto,
      node_proto,
      element_proto,
      document_proto,
    ] {
      let root = scope.heap_mut().add_root(Value::Object(obj))?;
      self.prototype_roots.push(root);
    }

    self.event_target_prototype = Some(event_target_proto);
    self.node_prototype = Some(node_proto);
    self.element_prototype = Some(element_proto);
    self.document_prototype = Some(document_proto);

    // Install EventTarget methods.
    let add_event_listener_id = vm.register_native_call(event_target_add_event_listener)?;
    let remove_event_listener_id = vm.register_native_call(event_target_remove_event_listener)?;
    define_native_method(
      scope,
      event_target_proto,
      "addEventListener",
      add_event_listener_id,
      2,
    )?;
    define_native_method(
      scope,
      event_target_proto,
      "removeEventListener",
      remove_event_listener_id,
      2,
    )?;

    // Install DOM methods.
    let query_selector_id = vm.register_native_call(dom_query_selector)?;
    define_native_method(scope, node_proto, "querySelector", query_selector_id, 1)?;

    let get_attribute_id = vm.register_native_call(dom_get_attribute)?;
    define_native_method(
      scope,
      element_proto,
      "getAttribute",
      get_attribute_id,
      1,
    )?;

    Ok(())
  }

  pub fn get_or_create_node_wrapper(
    &mut self,
    scope: &mut Scope<'_>,
    node_id: NodeId,
  ) -> Result<GcObject, VmError> {
    if let Some(existing) = self.node_wrappers.get(&node_id).copied() {
      if let Some(obj) = existing.upgrade(scope.heap()) {
        return Ok(obj);
      }
    }

    let obj = scope.alloc_object()?;

    let proto = self.prototype_for_node(node_id)?;
    scope.heap_mut().object_set_prototype(obj, Some(proto))?;

    let sym = self.node_id_symbol.expect("DomBindingsContext not initialized");
    let key = PropertyKey::from_symbol(sym);
    let desc = PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(node_id.index() as f64),
        writable: false,
      },
    };
    scope.define_property(obj, key, desc)?;

    self.node_wrappers.insert(node_id, WeakGcObject::from(obj));
    Ok(obj)
  }

  fn prototype_for_node(&self, node_id: NodeId) -> Result<GcObject, VmError> {
    let node = self.dom.node(node_id);
    let obj = match node.kind {
      NodeKind::Document { .. } => self.document_prototype,
      NodeKind::Element { .. } | NodeKind::Slot { .. } => self.element_prototype,
      _ => self.node_prototype,
    };
    obj.ok_or(VmError::Unimplemented(
      "DomBindingsContext not initialized (missing prototype)",
    ))
  }

  fn node_id_from_wrapper(&self, heap: &vm_js::Heap, obj: GcObject) -> Result<NodeId, VmError> {
    let sym = self
      .node_id_symbol
      .ok_or(VmError::Unimplemented("DomBindingsContext not initialized"))?;
    let key = PropertyKey::from_symbol(sym);
    let value = heap
      .object_get_own_data_property_value(obj, &key)?
      .ok_or(VmError::Unimplemented("DOM wrapper missing NodeId slot"))?;
    let Value::Number(n) = value else {
      return Err(VmError::Unimplemented("DOM wrapper NodeId slot is not a number"));
    };
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
      return Err(VmError::Unimplemented("DOM wrapper NodeId slot is not an integer"));
    }
    let idx = n as usize;
    if idx >= self.dom.nodes_len() {
      return Err(VmError::InvalidHandle);
    }
    Ok(NodeId::from_index(idx))
  }
}

fn define_native_method(
  scope: &mut Scope<'_>,
  prototype: GcObject,
  name: &str,
  call_id: vm_js::NativeFunctionId,
  length: u32,
) -> Result<(), VmError> {
  let name_str = scope.alloc_string(name)?;
  let func = scope.alloc_native_function(call_id, None, name_str, length)?;
  let key = PropertyKey::from_string(name_str);
  let desc = PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value: Value::Object(func),
      writable: true,
    },
  };
  scope.define_property(prototype, key, desc)?;
  Ok(())
}

fn dom_query_selector(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let ctx = vm
    .user_data_mut::<DomBindingsContext>()
    .ok_or(VmError::Unimplemented("missing DomBindingsContext user data"))?;

  let Value::Object(this_obj) = this else {
    return Err(VmError::Unimplemented("querySelector: receiver is not an object"));
  };
  let this_node = ctx.node_id_from_wrapper(scope.heap(), this_obj)?;

  let selector_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::String(selector) = selector_value else {
    return Err(VmError::Unimplemented(
      "querySelector: selectors must be a string",
    ));
  };
  let selector = scope.heap().get_string(selector)?.to_utf8_lossy();

  // ParentNode.querySelector: for Document, scope is `None`; for elements, scope is the element.
  let scope_node = match ctx.dom.node(this_node).kind {
    NodeKind::Document { .. } => None,
    _ => Some(this_node),
  };

  let found = ctx
    .dom
    .query_selector(&selector, scope_node)
    .map_err(|_e| VmError::Unimplemented("querySelector: selector parse/match failed"))?;

  let Some(found) = found else {
    return Ok(Value::Null);
  };

  let wrapper = ctx.get_or_create_node_wrapper(scope, found)?;
  Ok(Value::Object(wrapper))
}

fn dom_get_attribute(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let ctx = vm
    .user_data_mut::<DomBindingsContext>()
    .ok_or(VmError::Unimplemented("missing DomBindingsContext user data"))?;

  let Value::Object(this_obj) = this else {
    return Err(VmError::Unimplemented("getAttribute: receiver is not an object"));
  };
  let node_id = ctx.node_id_from_wrapper(scope.heap(), this_obj)?;

  let name_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::String(name) = name_value else {
    return Err(VmError::Unimplemented("getAttribute: name must be a string"));
  };
  let name = scope.heap().get_string(name)?.to_utf8_lossy();

  let attrs = match &ctx.dom.node(node_id).kind {
    NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
    _ => return Ok(Value::Null),
  };

  let value = attrs
    .iter()
    .find(|(k, _)| k.eq_ignore_ascii_case(&name))
    .map(|(_k, v)| v.as_str());

  let Some(value) = value else {
    return Ok(Value::Null);
  };

  let s = scope.alloc_string(value)?;
  Ok(Value::String(s))
}

fn event_target_add_event_listener(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let ctx = vm
    .user_data_mut::<DomBindingsContext>()
    .ok_or(VmError::Unimplemented("missing DomBindingsContext user data"))?;

  let Value::Object(this_obj) = this else {
    return Err(VmError::Unimplemented(
      "addEventListener: receiver is not an object",
    ));
  };
  let target = ctx.node_id_from_wrapper(scope.heap(), this_obj)?;

  let type_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let callback = args.get(1).copied().unwrap_or(Value::Undefined);
  let Value::String(type_) = type_value else {
    return Err(VmError::Unimplemented(
      "addEventListener: type must be a string",
    ));
  };
  // `null`/`undefined` callbacks are a no-op in the platform.
  if matches!(callback, Value::Null | Value::Undefined) {
    return Ok(Value::Undefined);
  }

  let type_ = scope.heap().get_string(type_)?.to_utf8_lossy();
  let root = scope.heap_mut().add_root(callback)?;
  ctx.listeners.entry((target, type_)).or_default().push(root);
  Ok(Value::Undefined)
}

fn event_target_remove_event_listener(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let ctx = vm
    .user_data_mut::<DomBindingsContext>()
    .ok_or(VmError::Unimplemented("missing DomBindingsContext user data"))?;

  let Value::Object(this_obj) = this else {
    return Err(VmError::Unimplemented(
      "removeEventListener: receiver is not an object",
    ));
  };
  let target = ctx.node_id_from_wrapper(scope.heap(), this_obj)?;

  let type_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let callback = args.get(1).copied().unwrap_or(Value::Undefined);
  let Value::String(type_) = type_value else {
    return Err(VmError::Unimplemented(
      "removeEventListener: type must be a string",
    ));
  };
  if matches!(callback, Value::Null | Value::Undefined) {
    return Ok(Value::Undefined);
  }

  let type_ = scope.heap().get_string(type_)?.to_utf8_lossy();
  let key = (target, type_.clone());
  let Some(list) = ctx.listeners.get_mut(&key) else {
    return Ok(Value::Undefined);
  };

  let mut removed_roots: Vec<RootId> = Vec::new();
  {
    let heap = scope.heap();
    list.retain(|root_id| {
      let keep = heap.get_root(*root_id) != Some(callback);
      if !keep {
        removed_roots.push(*root_id);
      }
      keep
    });
  }

  for root_id in removed_roots {
    scope.heap_mut().remove_root(root_id);
  }

  if list.is_empty() {
    ctx.listeners.remove(&key);
  }

  Ok(Value::Undefined)
}
