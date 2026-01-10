use crate::dom2::{NodeId, NodeKind};
use crate::web::events::EventTargetId;
use std::collections::HashMap;
use vm_js::{GcObject, Heap, Realm, RealmId, RootId, Scope, Value, VmError, WeakGcObject};

/// Primary interface brand for a `dom2` platform object wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomInterface {
  EventTarget,
  Node,
  Element,
  Document,
  DocumentFragment,
}

impl DomInterface {
  pub fn primary_for_node_kind(kind: &NodeKind) -> Self {
    match kind {
      NodeKind::Document { .. } => Self::Document,
      NodeKind::DocumentFragment => Self::DocumentFragment,
      NodeKind::Element { .. } | NodeKind::Slot { .. } => Self::Element,
      _ => Self::Node,
    }
  }

  pub fn implements(self, interface: DomInterface) -> bool {
    match (self, interface) {
      (a, b) if a == b => true,
      // Inheritance:
      // - Document/Element/DocumentFragment all inherit from Node
      // - Node inherits from EventTarget
      (Self::Document | Self::Element | Self::DocumentFragment, Self::Node) => true,
      (
        Self::Document | Self::Element | Self::DocumentFragment | Self::Node,
        Self::EventTarget,
      ) => true,
      _ => false,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomWrapperMeta {
  pub node_id: NodeId,
  pub primary_interface: DomInterface,
  pub realm_id: RealmId,
  pub dom_source_id: u64,
}

#[derive(Clone, Copy)]
struct DomPrototypes {
  event_target: GcObject,
  node: GcObject,
  element: GcObject,
  document: GcObject,
  document_fragment: GcObject,
}

/// Per-realm platform-object registry for `dom2` node wrappers inside a `vm-js` realm.
///
/// The registry provides:
/// - stable wrapper identity via `NodeId -> WeakGcObject` caching,
/// - host-owned wrapper metadata via `WeakGcObject -> DomWrapperMeta` tables, and
/// - pre-allocated prototype objects with a WebIDL-shaped inheritance chain.
///
/// `DomPlatform` is not traced by the `vm-js` GC, so any `GcObject` handles stored in the struct must
/// be rooted explicitly.
pub struct DomPlatform {
  realm_id: RealmId,
  dom_source_id: u64,
  prototypes: DomPrototypes,
  prototype_roots: Vec<RootId>,
  wrappers_by_node: HashMap<NodeId, WeakGcObject>,
  meta_by_wrapper: HashMap<WeakGcObject, DomWrapperMeta>,
  last_gc_runs: u64,
}

impl DomPlatform {
  pub fn new(scope: &mut Scope<'_>, realm: &Realm, dom_source_id: u64) -> Result<Self, VmError> {
    let realm_id = realm.id();

    // Prototype objects.
    let proto_event_target = scope.alloc_object()?;
    let proto_node = scope.alloc_object()?;
    let proto_element = scope.alloc_object()?;
    let proto_document = scope.alloc_object()?;
    let proto_document_fragment = scope.alloc_object()?;

    // Root prototypes: `DomPlatform` lives on the host side and is not traced by GC.
    let mut prototype_roots: Vec<RootId> = Vec::with_capacity(5);
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_event_target))?);
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_node))?);
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_element))?);
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_document))?);
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_document_fragment))?);

    // WebIDL / WHATWG DOM inheritance chain:
    //   Node -> EventTarget -> Object
    //   Element -> Node
    //   Document -> Node
    //   DocumentFragment -> Node
    scope.heap_mut().object_set_prototype(
      proto_event_target,
      Some(realm.intrinsics().object_prototype()),
    )?;
    scope
      .heap_mut()
      .object_set_prototype(proto_node, Some(proto_event_target))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_element, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_document, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_document_fragment, Some(proto_node))?;

    Ok(Self {
      realm_id,
      dom_source_id,
      prototypes: DomPrototypes {
        event_target: proto_event_target,
        node: proto_node,
        element: proto_element,
        document: proto_document,
        document_fragment: proto_document_fragment,
      },
      prototype_roots,
      wrappers_by_node: HashMap::new(),
      meta_by_wrapper: HashMap::new(),
      last_gc_runs: scope.heap().gc_runs(),
    })
  }

  pub fn teardown(&mut self, heap: &mut Heap) {
    for root in self.prototype_roots.drain(..) {
      heap.remove_root(root);
    }
  }

  pub fn dom_source_id(&self) -> u64 {
    self.dom_source_id
  }

  pub fn realm_id(&self) -> RealmId {
    self.realm_id
  }

  pub fn prototype_for(&self, interface: DomInterface) -> GcObject {
    match interface {
      DomInterface::EventTarget => self.prototypes.event_target,
      DomInterface::Node => self.prototypes.node,
      DomInterface::Element => self.prototypes.element,
      DomInterface::Document => self.prototypes.document,
      DomInterface::DocumentFragment => self.prototypes.document_fragment,
    }
  }

  fn sweep_dead_wrappers_if_needed(&mut self, heap: &Heap) {
    let gc_runs = heap.gc_runs();
    if gc_runs == self.last_gc_runs {
      return;
    }
    self.last_gc_runs = gc_runs;

    self
      .wrappers_by_node
      .retain(|_, weak| weak.upgrade(heap).is_some());
    self
      .meta_by_wrapper
      .retain(|weak, _| weak.upgrade(heap).is_some());
  }

  pub fn register_wrapper(
    &mut self,
    heap: &Heap,
    wrapper: GcObject,
    node_id: NodeId,
    primary_interface: DomInterface,
  ) {
    self.sweep_dead_wrappers_if_needed(heap);
    let weak = WeakGcObject::from(wrapper);
    self.wrappers_by_node.insert(node_id, weak);
    self.meta_by_wrapper.insert(
      weak,
      DomWrapperMeta {
        node_id,
        primary_interface,
        realm_id: self.realm_id,
        dom_source_id: self.dom_source_id,
      },
    );
  }

  /// Return an existing wrapper for `node_id` if still alive.
  pub fn get_existing_wrapper(&mut self, heap: &Heap, node_id: NodeId) -> Option<GcObject> {
    self.sweep_dead_wrappers_if_needed(heap);
    self
      .wrappers_by_node
      .get(&node_id)
      .copied()
      .and_then(|weak| weak.upgrade(heap))
  }

  pub fn get_or_create_wrapper(
    &mut self,
    scope: &mut Scope<'_>,
    node_id: NodeId,
    primary_interface: DomInterface,
  ) -> Result<GcObject, VmError> {
    if let Some(existing) = self.get_existing_wrapper(scope.heap(), node_id) {
      return Ok(existing);
    }

    let wrapper = scope.alloc_object()?;
    scope.heap_mut().object_set_prototype(
      wrapper,
      Some(self.prototype_for(primary_interface)),
    )?;
    self.register_wrapper(scope.heap(), wrapper, node_id, primary_interface);
    Ok(wrapper)
  }

  fn require_wrapper_meta(&mut self, heap: &Heap, value: Value) -> Result<DomWrapperMeta, VmError> {
    self.sweep_dead_wrappers_if_needed(heap);

    let Value::Object(obj) = value else {
      return Err(VmError::TypeError("Illegal invocation"));
    };
    if !heap.is_valid_object(obj) {
      return Err(VmError::TypeError("Illegal invocation"));
    }

    self
      .meta_by_wrapper
      .get(&WeakGcObject::from(obj))
      .copied()
      .ok_or(VmError::TypeError("Illegal invocation"))
  }

  pub fn require_node_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    let meta = self.require_wrapper_meta(heap, value)?;
    if !meta.primary_interface.implements(DomInterface::Node) {
      return Err(VmError::TypeError("Illegal invocation"));
    }
    Ok(meta.node_id)
  }

  pub fn require_element_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    let meta = self.require_wrapper_meta(heap, value)?;
    if !meta.primary_interface.implements(DomInterface::Element) {
      return Err(VmError::TypeError("Illegal invocation"));
    }
    Ok(meta.node_id)
  }

  pub fn require_document_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    let meta = self.require_wrapper_meta(heap, value)?;
    if !meta.primary_interface.implements(DomInterface::Document) {
      return Err(VmError::TypeError("Illegal invocation"));
    }
    Ok(meta.node_id)
  }

  pub fn require_document_fragment_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    let meta = self.require_wrapper_meta(heap, value)?;
    if !meta.primary_interface.implements(DomInterface::DocumentFragment) {
      return Err(VmError::TypeError("Illegal invocation"));
    }
    Ok(meta.node_id)
  }

  pub fn event_target_id_for_value(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<EventTargetId, VmError> {
    let node_id = self.require_node_id(heap, value)?;
    Ok(EventTargetId::Node(node_id).normalize())
  }
}

#[cfg(test)]
mod tests {
  use super::{DomInterface, DomPlatform};
  use crate::dom2::NodeId;
  use vm_js::{Heap, HeapLimits, Realm, Value, Vm, VmError, VmOptions, WeakGcObject};

  fn split_runtime_realm(runtime: &mut vm_js::JsRuntime) -> (&Realm, &mut Heap) {
    // SAFETY: `realm` is stored separately from `vm` and `heap` inside `vm-js::JsRuntime`.
    let realm_ptr = runtime.realm() as *const Realm;
    let heap = &mut runtime.heap;
    let realm = unsafe { &*realm_ptr };
    (realm, heap)
  }

  fn make_runtime() -> Result<vm_js::JsRuntime, VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024));
    vm_js::JsRuntime::new(vm, heap)
  }

  #[test]
  fn wrapping_same_node_id_preserves_identity_while_alive() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (realm, heap) = split_runtime_realm(&mut runtime);
    let mut scope = heap.scope();
    let mut platform = DomPlatform::new(&mut scope, realm, /* dom_source_id */ 1)?;

    let node_id = NodeId::from_index(1);
    let wrapper1 = platform.get_or_create_wrapper(&mut scope, node_id, DomInterface::Element)?;
    let root = scope.heap_mut().add_root(Value::Object(wrapper1))?;

    let wrapper2 = platform.get_or_create_wrapper(&mut scope, node_id, DomInterface::Element)?;
    assert_eq!(wrapper1, wrapper2);

    scope.heap_mut().remove_root(root);
    Ok(())
  }

  #[test]
  fn wrapper_can_be_collected_when_unreachable() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (realm, heap) = split_runtime_realm(&mut runtime);
    let mut scope = heap.scope();
    let mut platform = DomPlatform::new(&mut scope, realm, /* dom_source_id */ 1)?;

    let node_id = NodeId::from_index(1);
    let wrapper = platform.get_or_create_wrapper(&mut scope, node_id, DomInterface::Element)?;
    let weak = WeakGcObject::from(wrapper);
    let root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    scope.heap_mut().remove_root(root);
    scope.heap_mut().collect_garbage();

    assert!(weak.upgrade(scope.heap()).is_none());

    // Re-wrapping after collection should succeed; identity may change.
    let wrapper2 = platform.get_or_create_wrapper(&mut scope, node_id, DomInterface::Element)?;
    assert_ne!(wrapper, wrapper2);
    Ok(())
  }

  #[test]
  fn brand_checks_throw_type_error_on_illegal_invocation() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (realm, heap) = split_runtime_realm(&mut runtime);
    let mut scope = heap.scope();
    let mut platform = DomPlatform::new(&mut scope, realm, /* dom_source_id */ 1)?;

    let node_id = NodeId::from_index(1);
    let wrapper = platform.get_or_create_wrapper(&mut scope, node_id, DomInterface::Element)?;
    let _root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    assert_eq!(
      platform.require_node_id(scope.heap(), Value::Object(wrapper))?,
      node_id
    );
    assert_eq!(
      platform.require_element_id(scope.heap(), Value::Object(wrapper))?,
      node_id
    );

    let err = platform.require_document_id(scope.heap(), Value::Object(wrapper));
    assert!(matches!(err, Err(VmError::TypeError("Illegal invocation"))));

    let obj = scope.alloc_object()?;
    let err = platform.require_node_id(scope.heap(), Value::Object(obj));
    assert!(matches!(err, Err(VmError::TypeError("Illegal invocation"))));

    let err = platform.require_node_id(scope.heap(), Value::Undefined);
    assert!(matches!(err, Err(VmError::TypeError("Illegal invocation"))));
    Ok(())
  }
}
