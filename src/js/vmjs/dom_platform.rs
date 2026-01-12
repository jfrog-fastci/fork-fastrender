use crate::dom2::{NodeId, NodeKind};
use crate::dom::HTML_NAMESPACE;
use crate::web::events::EventTargetId;
use std::collections::HashMap;
use vm_js::{
  GcObject, Heap, HostSlots, PropertyDescriptor, PropertyKey, PropertyKind, Realm, RealmId, RootId,
  Scope, Value, VmError, WeakGcObject,
};

// Must match `window_realm::NODE_ID_KEY`.
const INTERNAL_NODE_ID_KEY: &str = "__fastrender_node_id";

/// Uniquely identifies a `dom2::Document` within a JS realm.
///
/// Note: `dom2::NodeId` values are only unique within a document, not across documents.
pub type DocumentId = u64;

/// HostSlots tag used to brand DOM platform object wrappers (Document/Element/etc).
///
/// The `structuredClone()` implementation treats any object with `HostSlots` as a platform object
/// and throws `DataCloneError` (HTML structured clone algorithm).
pub const DOM_WRAPPER_HOST_TAG: u64 = 0x444F_4D57_5241_5050; // "DOMWRAPP"

/// Unique identity for a `dom2` node in a realm.
///
/// This is the cache key used by `DomPlatform` when maintaining stable wrapper identity across
/// multiple documents inside the same realm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DomNodeKey {
  pub document_id: DocumentId,
  pub node_id: NodeId,
}

impl DomNodeKey {
  pub const fn new(document_id: DocumentId, node_id: NodeId) -> Self {
    Self {
      document_id,
      node_id,
    }
  }
}

impl From<NodeId> for DomNodeKey {
  fn from(node_id: NodeId) -> Self {
    // Legacy single-document call sites still pass bare `NodeId` values. Treat those as belonging
    // to document 0 until callers are updated to pass an explicit `DocumentId`.
    Self::new(0, node_id)
  }
}

/// Primary interface brand for a `dom2` platform object wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomInterface {
  EventTarget,
  Node,
  DocumentType,
  Text,
  Element,
  HTMLElement,
  HTMLInputElement,
  HTMLSelectElement,
  HTMLTextAreaElement,
  HTMLOptionElement,
  HTMLFormElement,
  HTMLDivElement,
  HTMLSpanElement,
  HTMLParagraphElement,
  HTMLAnchorElement,
  HTMLImageElement,
  HTMLLinkElement,
  Document,
  DocumentFragment,
}

impl DomInterface {
  pub fn primary_for_node_kind(kind: &NodeKind) -> Self {
    match kind {
      NodeKind::Document { .. } => Self::Document,
      NodeKind::DocumentFragment => Self::DocumentFragment,
      NodeKind::Text { .. } => Self::Text,
      NodeKind::Element {
        tag_name, namespace, ..
      } => {
        let is_html_ns = namespace.is_empty() || namespace == HTML_NAMESPACE;
        if !is_html_ns {
          return Self::Element;
        }

        if tag_name.eq_ignore_ascii_case("input") {
          return Self::HTMLInputElement;
        }
        if tag_name.eq_ignore_ascii_case("select") {
          return Self::HTMLSelectElement;
        }
        if tag_name.eq_ignore_ascii_case("textarea") {
          return Self::HTMLTextAreaElement;
        }
        if tag_name.eq_ignore_ascii_case("option") {
          return Self::HTMLOptionElement;
        }
        if tag_name.eq_ignore_ascii_case("form") {
          return Self::HTMLFormElement;
        }

        if tag_name.eq_ignore_ascii_case("div") {
          return Self::HTMLDivElement;
        }
        if tag_name.eq_ignore_ascii_case("span") {
          return Self::HTMLSpanElement;
        }
        if tag_name.eq_ignore_ascii_case("p") {
          return Self::HTMLParagraphElement;
        }
        if tag_name.eq_ignore_ascii_case("a") {
          return Self::HTMLAnchorElement;
        }
        if tag_name.eq_ignore_ascii_case("img") {
          return Self::HTMLImageElement;
        }
        if tag_name.eq_ignore_ascii_case("link") {
          return Self::HTMLLinkElement;
        }

        Self::HTMLElement
      }
      NodeKind::Slot { .. } => Self::Element,
      NodeKind::Doctype { .. } => Self::DocumentType,
      _ => Self::Node,
    }
  }

  fn parent(self) -> Option<Self> {
    match self {
      Self::EventTarget => None,
      Self::Node => Some(Self::EventTarget),
      Self::Text | Self::Element | Self::Document | Self::DocumentFragment | Self::DocumentType => {
        Some(Self::Node)
      }
      Self::HTMLElement => Some(Self::Element),
      Self::HTMLInputElement
      | Self::HTMLSelectElement
      | Self::HTMLTextAreaElement
      | Self::HTMLOptionElement
      | Self::HTMLFormElement
      | Self::HTMLDivElement
      | Self::HTMLSpanElement
      | Self::HTMLParagraphElement
      | Self::HTMLAnchorElement
      | Self::HTMLImageElement
      | Self::HTMLLinkElement => Some(Self::HTMLElement),
    }
  }

  pub fn implements(self, interface: DomInterface) -> bool {
    let mut current = Some(self);
    while let Some(cur) = current {
      if cur == interface {
        return true;
      }
      current = cur.parent();
    }
    false
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomWrapperMeta {
  pub document_id: DocumentId,
  pub node_id: NodeId,
  pub primary_interface: DomInterface,
  pub realm_id: RealmId,
}

#[derive(Clone, Copy)]
struct DomPrototypes {
  event_target: GcObject,
  node: GcObject,
  document_type: GcObject,
  text: GcObject,
  element: GcObject,
  html_element: GcObject,
  html_input_element: GcObject,
  html_select_element: GcObject,
  html_text_area_element: GcObject,
  html_option_element: GcObject,
  html_form_element: GcObject,
  html_div_element: GcObject,
  html_span_element: GcObject,
  html_paragraph_element: GcObject,
  html_anchor_element: GcObject,
  html_image_element: GcObject,
  html_link_element: GcObject,
  document: GcObject,
  document_fragment: GcObject,
}

/// Per-realm platform-object registry for `dom2` node wrappers inside a `vm-js` realm.
///
/// The registry provides:
/// - stable wrapper identity via `DomNodeKey -> WeakGcObject` caching,
/// - host-owned wrapper metadata via `WeakGcObject -> DomWrapperMeta` tables, and
/// - pre-allocated prototype objects with a WebIDL-shaped inheritance chain.
///
/// `DomPlatform` is not traced by the `vm-js` GC, so any `GcObject` handles stored in the struct must
/// be rooted explicitly.
pub struct DomPlatform {
  realm_id: RealmId,
  prototypes: DomPrototypes,
  prototype_roots: Vec<RootId>,
  wrappers_by_node: HashMap<DomNodeKey, WeakGcObject>,
  meta_by_wrapper: HashMap<WeakGcObject, DomWrapperMeta>,
  last_gc_runs: u64,
}

impl DomPlatform {
  pub fn new(scope: &mut Scope<'_>, realm: &Realm) -> Result<Self, VmError> {
    let realm_id = realm.id();

    // Root prototypes: `DomPlatform` lives on the host side and is not traced by GC.
    //
    // Root each object immediately after allocation. Under a tight heap limit, subsequent
    // allocations can trigger GC, and unrooted prototypes would be collected (turning their
    // handles into stale values).
    let mut prototype_roots: Vec<RootId> = Vec::with_capacity(18);

    // Prototype objects.
    let proto_event_target = scope.alloc_object()?;
    prototype_roots.push(
      scope
        .heap_mut()
        .add_root(Value::Object(proto_event_target))?,
    );
    let proto_node = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_node))?);
    let proto_document_type = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_document_type))?);
    let proto_text = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_text))?);
    let proto_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_element))?);
    let proto_html_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_element))?);
    let proto_html_input_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_input_element))?);
    let proto_html_select_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_select_element))?);
    let proto_html_text_area_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_text_area_element))?);
    let proto_html_option_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_option_element))?);
    let proto_html_form_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_form_element))?);
    let proto_html_div_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_div_element))?);
    let proto_html_span_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_span_element))?);
    let proto_html_paragraph_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_paragraph_element))?);
    let proto_html_anchor_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_anchor_element))?);
    let proto_html_image_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_image_element))?);
    let proto_html_link_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_link_element))?);
    let proto_document = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_document))?);
    let proto_document_fragment = scope.alloc_object()?;
    prototype_roots.push(
      scope
        .heap_mut()
        .add_root(Value::Object(proto_document_fragment))?,
    );

    // WebIDL / WHATWG DOM inheritance chain:
    //   EventTarget -> Object
    //   Node -> EventTarget
    //   DocumentType -> Node
    //   Text -> Node
    //   Element -> Node
    //   HTMLElement -> Element
    //   HTML*Element -> HTMLElement
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
      .object_set_prototype(proto_document_type, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_text, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_element, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_html_element, Some(proto_element))?;
    for proto in [
      proto_html_input_element,
      proto_html_select_element,
      proto_html_text_area_element,
      proto_html_option_element,
      proto_html_form_element,
      proto_html_div_element,
      proto_html_span_element,
      proto_html_paragraph_element,
      proto_html_anchor_element,
      proto_html_image_element,
      proto_html_link_element,
    ] {
      scope
        .heap_mut()
        .object_set_prototype(proto, Some(proto_html_element))?;
    }
    scope
      .heap_mut()
      .object_set_prototype(proto_document, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_document_fragment, Some(proto_node))?;

    Ok(Self {
      realm_id,
      prototypes: DomPrototypes {
        event_target: proto_event_target,
        node: proto_node,
        document_type: proto_document_type,
        text: proto_text,
        element: proto_element,
        html_element: proto_html_element,
        html_input_element: proto_html_input_element,
        html_select_element: proto_html_select_element,
        html_text_area_element: proto_html_text_area_element,
        html_option_element: proto_html_option_element,
        html_form_element: proto_html_form_element,
        html_div_element: proto_html_div_element,
        html_span_element: proto_html_span_element,
        html_paragraph_element: proto_html_paragraph_element,
        html_anchor_element: proto_html_anchor_element,
        html_image_element: proto_html_image_element,
        html_link_element: proto_html_link_element,
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

  pub fn realm_id(&self) -> RealmId {
    self.realm_id
  }

  pub fn prototype_for(&self, interface: DomInterface) -> GcObject {
    match interface {
      DomInterface::EventTarget => self.prototypes.event_target,
      DomInterface::Node => self.prototypes.node,
      DomInterface::DocumentType => self.prototypes.document_type,
      DomInterface::Text => self.prototypes.text,
      DomInterface::Element => self.prototypes.element,
      DomInterface::HTMLElement => self.prototypes.html_element,
      DomInterface::HTMLInputElement => self.prototypes.html_input_element,
      DomInterface::HTMLSelectElement => self.prototypes.html_select_element,
      DomInterface::HTMLTextAreaElement => self.prototypes.html_text_area_element,
      DomInterface::HTMLOptionElement => self.prototypes.html_option_element,
      DomInterface::HTMLFormElement => self.prototypes.html_form_element,
      DomInterface::HTMLDivElement => self.prototypes.html_div_element,
      DomInterface::HTMLSpanElement => self.prototypes.html_span_element,
      DomInterface::HTMLParagraphElement => self.prototypes.html_paragraph_element,
      DomInterface::HTMLAnchorElement => self.prototypes.html_anchor_element,
      DomInterface::HTMLImageElement => self.prototypes.html_image_element,
      DomInterface::HTMLLinkElement => self.prototypes.html_link_element,
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
    node: impl Into<DomNodeKey>,
    primary_interface: DomInterface,
  ) {
    self.sweep_dead_wrappers_if_needed(heap);
    let key = node.into();
    let weak = WeakGcObject::from(wrapper);
    self.wrappers_by_node.insert(key, weak);
    self.meta_by_wrapper.insert(
      weak,
      DomWrapperMeta {
        document_id: key.document_id,
        node_id: key.node_id,
        primary_interface,
        realm_id: self.realm_id,
      },
    );
  }

  /// Return an existing wrapper for `node_id` if still alive.
  pub fn get_existing_wrapper(&mut self, heap: &Heap, node: impl Into<DomNodeKey>) -> Option<GcObject> {
    self.sweep_dead_wrappers_if_needed(heap);
    let key = node.into();
    self
      .wrappers_by_node
      .get(&key)
      .copied()
      .and_then(|weak| weak.upgrade(heap))
  }

  pub fn get_or_create_wrapper(
    &mut self,
    scope: &mut Scope<'_>,
    node: impl Into<DomNodeKey>,
    primary_interface: DomInterface,
  ) -> Result<GcObject, VmError> {
    let key = node.into();
    if let Some(existing) = self.get_existing_wrapper(scope.heap(), key) {
      return Ok(existing);
    }

    let wrapper = scope.alloc_object()?;
    scope
      .heap_mut()
      .object_set_prototype(wrapper, Some(self.prototype_for(primary_interface)))?;
    scope.heap_mut().object_set_host_slots(
      wrapper,
      HostSlots {
        a: DOM_WRAPPER_HOST_TAG,
        b: 0,
      },
    )?;

    // Ensure wrappers always expose an up-to-date node ID property so native DOM operations that
    // read it directly (rather than consulting `DomPlatform` metadata) remain correct.
    //
    // This property is also updated by `remap_node_ids` when a DOM operation replaces a node's
    // underlying `dom2::NodeId` (e.g. adopt/import implemented as clone+mapping).
    {
      // Root `wrapper` while allocating the property key: `alloc_string` can trigger GC.
      let mut scope = scope.reborrow();
      scope.push_root(Value::Object(wrapper))?;

      let node_id_key = PropertyKey::from_string(scope.alloc_string(INTERNAL_NODE_ID_KEY)?);
      scope.define_property(
        wrapper,
        node_id_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Number(key.node_id.index() as f64),
            writable: true,
          },
        },
      )?;
    }
    self.register_wrapper(scope.heap(), wrapper, key, primary_interface);
    Ok(wrapper)
  }

  fn rebind_wrapper_impl(
    &mut self,
    heap: &mut Heap,
    node_id_key: &PropertyKey,
    old: DomNodeKey,
    new: DomNodeKey,
  ) -> Result<(), VmError> {
    if old == new {
      return Ok(());
    }

    self.sweep_dead_wrappers_if_needed(heap);

    let Some(weak) = self.wrappers_by_node.remove(&old) else {
      return Ok(());
    };
    let Some(wrapper) = weak.upgrade(heap) else {
      // Wrapper was collected since the last sweep; nothing to preserve.
      return Ok(());
    };

    // Overwrite any existing mapping for `new`. In the expected clone+mapping case, `new` refers
    // to freshly-created nodes with no wrappers yet.
    self.wrappers_by_node.insert(new, weak);

    if let Some(meta) = self.meta_by_wrapper.get_mut(&weak) {
      meta.document_id = new.document_id;
      meta.node_id = new.node_id;
    }

    // Keep the wrapper's own node ID property in sync so native methods that read it directly
    // continue to work.
    match heap.object_set_existing_data_property_value(
      wrapper,
      node_id_key,
      Value::Number(new.node_id.index() as f64),
    ) {
      Ok(()) => {}
      Err(VmError::PropertyNotFound | VmError::PropertyNotData) => {
        // Some wrappers (e.g. those constructed directly in unit tests) may not have the property
        // yet. Define it eagerly so future native calls can rely on its presence.
        let mut scope = heap.scope();
        scope.define_property(
          wrapper,
          *node_id_key,
          PropertyDescriptor {
            enumerable: false,
            configurable: true,
            kind: PropertyKind::Data {
              value: Value::Number(new.node_id.index() as f64),
              writable: true,
            },
          },
        )?;
      }
      Err(err) => return Err(err),
    }

    Ok(())
  }

  /// Move an existing live wrapper mapping from `old` to `new`, updating both host-side metadata and
  /// the wrapper's own `__fastrender_node_id` property.
  ///
  /// This is intended for DOM operations implemented as clone+mapping (e.g. cross-document adoption)
  /// that must preserve JS wrapper object identity even when the underlying `dom2::NodeId` changes.
  pub fn rebind_wrapper(
    &mut self,
    heap: &mut Heap,
    old: DomNodeKey,
    new: DomNodeKey,
  ) -> Result<(), VmError> {
    // Allocate the property key once. `PropertyKey` string comparisons are by content, so it will
    // match existing keys even if wrappers were created using a different `GcString` handle.
    let node_id_key = {
      let mut scope = heap.scope();
      PropertyKey::from_string(scope.alloc_string(INTERNAL_NODE_ID_KEY)?)
    };
    self.rebind_wrapper_impl(heap, &node_id_key, old, new)
  }

  /// Remap cached wrapper identity for nodes whose `dom2::NodeId` indices have changed.
  ///
  /// This is intended for DOM operations that are implemented as clone+mapping (rather than
  /// in-place moves) but must preserve JS object identity (e.g. `adoptNode()`-style operations).
  ///
  /// For each `(old_id -> new_id)` mapping, if a wrapper is still alive for `old_id`, it is moved
  /// to the `new_id` key and its metadata + `__fastrender_node_id` property are updated.
  pub fn remap_node_ids(
    &mut self,
    heap: &mut Heap,
    document_id: DocumentId,
    mapping: &HashMap<NodeId, NodeId>,
  ) -> Result<(), VmError> {
    if mapping.is_empty() {
      return Ok(());
    }

    let node_id_key = {
      let mut scope = heap.scope();
      PropertyKey::from_string(scope.alloc_string(INTERNAL_NODE_ID_KEY)?)
    };
    for (&old_id, &new_id) in mapping {
      self.rebind_wrapper_impl(
        heap,
        &node_id_key,
        DomNodeKey::new(document_id, old_id),
        DomNodeKey::new(document_id, new_id),
      )?;
    }
    Ok(())
  }

  /// Remap wrapper identity across documents (e.g. `adoptNode`-style moves implemented as
  /// clone+mapping).
  pub fn remap_node_ids_between_documents(
    &mut self,
    heap: &mut Heap,
    old_document_id: DocumentId,
    new_document_id: DocumentId,
    mapping: &HashMap<NodeId, NodeId>,
  ) -> Result<(), VmError> {
    if mapping.is_empty() {
      return Ok(());
    }

    let node_id_key = {
      let mut scope = heap.scope();
      PropertyKey::from_string(scope.alloc_string(INTERNAL_NODE_ID_KEY)?)
    };
    for (&old_id, &new_id) in mapping {
      self.rebind_wrapper_impl(
        heap,
        &node_id_key,
        DomNodeKey::new(old_document_id, old_id),
        DomNodeKey::new(new_document_id, new_id),
      )?;
    }
    Ok(())
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

  pub fn require_node_handle(&mut self, heap: &Heap, value: Value) -> Result<DomNodeKey, VmError> {
    let meta = self.require_wrapper_meta(heap, value)?;
    if !meta.primary_interface.implements(DomInterface::Node) {
      return Err(VmError::TypeError("Illegal invocation"));
    }
    Ok(DomNodeKey::new(meta.document_id, meta.node_id))
  }

  pub fn require_element_handle(&mut self, heap: &Heap, value: Value) -> Result<DomNodeKey, VmError> {
    let meta = self.require_wrapper_meta(heap, value)?;
    if !meta.primary_interface.implements(DomInterface::Element) {
      return Err(VmError::TypeError("Illegal invocation"));
    }
    Ok(DomNodeKey::new(meta.document_id, meta.node_id))
  }

  pub fn require_text_handle(&mut self, heap: &Heap, value: Value) -> Result<DomNodeKey, VmError> {
    let meta = self.require_wrapper_meta(heap, value)?;
    if !meta.primary_interface.implements(DomInterface::Text) {
      return Err(VmError::TypeError("Illegal invocation"));
    }
    Ok(DomNodeKey::new(meta.document_id, meta.node_id))
  }

  pub fn require_document_type_handle(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<DomNodeKey, VmError> {
    let meta = self.require_wrapper_meta(heap, value)?;
    if !meta.primary_interface.implements(DomInterface::DocumentType) {
      return Err(VmError::TypeError("Illegal invocation"));
    }
    Ok(DomNodeKey::new(meta.document_id, meta.node_id))
  }

  pub fn require_document_handle(&mut self, heap: &Heap, value: Value) -> Result<DomNodeKey, VmError> {
    let meta = self.require_wrapper_meta(heap, value)?;
    if !meta.primary_interface.implements(DomInterface::Document) {
      return Err(VmError::TypeError("Illegal invocation"));
    }
    Ok(DomNodeKey::new(meta.document_id, meta.node_id))
  }

  pub fn require_document_fragment_handle(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<DomNodeKey, VmError> {
    let meta = self.require_wrapper_meta(heap, value)?;
    if !meta
      .primary_interface
      .implements(DomInterface::DocumentFragment)
    {
      return Err(VmError::TypeError("Illegal invocation"));
    }
    Ok(DomNodeKey::new(meta.document_id, meta.node_id))
  }

  pub fn require_node_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    Ok(self.require_node_handle(heap, value)?.node_id)
  }

  pub fn require_element_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    Ok(self.require_element_handle(heap, value)?.node_id)
  }

  pub fn require_text_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    Ok(self.require_text_handle(heap, value)?.node_id)
  }

  pub fn require_document_type_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    Ok(self.require_document_type_handle(heap, value)?.node_id)
  }

  pub fn require_document_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    Ok(self.require_document_handle(heap, value)?.node_id)
  }

  pub fn require_document_fragment_id(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<NodeId, VmError> {
    Ok(self.require_document_fragment_handle(heap, value)?.node_id)
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
  use super::{DomInterface, DomNodeKey, DomPlatform};
  use crate::dom2::{NodeId, NodeKind};
  use std::collections::HashMap;
  use vm_js::{
    GcObject, Heap, HeapLimits, PropertyKey, Realm, Value, Vm, VmError, VmOptions, WeakGcObject,
  };

  fn gc_object_id(obj: GcObject) -> u64 {
    (obj.index() as u64) | ((obj.generation() as u64) << 32)
  }

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
    let mut platform = DomPlatform::new(&mut scope, realm)?;

    let key = DomNodeKey::new(1, NodeId::from_index(1));
    let wrapper1 = platform.get_or_create_wrapper(&mut scope, key, DomInterface::Element)?;
    let root = scope.heap_mut().add_root(Value::Object(wrapper1))?;

    let wrapper2 = platform.get_or_create_wrapper(&mut scope, key, DomInterface::Element)?;
    assert_eq!(wrapper1, wrapper2);

    scope.heap_mut().remove_root(root);
    Ok(())
  }

  #[test]
  fn wrapping_same_node_id_in_different_documents_does_not_collide() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (realm, heap) = split_runtime_realm(&mut runtime);
    let mut scope = heap.scope();
    let mut platform = DomPlatform::new(&mut scope, realm)?;

    let doc_a = scope.alloc_object()?;
    let doc_b = scope.alloc_object()?;
    let doc_id_a = gc_object_id(doc_a);
    let doc_id_b = gc_object_id(doc_b);
    let _doc_a_root = scope.heap_mut().add_root(Value::Object(doc_a))?;
    let _doc_b_root = scope.heap_mut().add_root(Value::Object(doc_b))?;

    let node_id = NodeId::from_index(1);
    let wrapper_a =
      platform.get_or_create_wrapper(&mut scope, DomNodeKey::new(doc_id_a, node_id), DomInterface::Element)?;
    let wrapper_b =
      platform.get_or_create_wrapper(&mut scope, DomNodeKey::new(doc_id_b, node_id), DomInterface::Element)?;

    assert_ne!(wrapper_a, wrapper_b);
    Ok(())
  }

  #[test]
  fn wrapper_can_be_collected_when_unreachable() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (realm, heap) = split_runtime_realm(&mut runtime);
    let mut scope = heap.scope();
    let mut platform = DomPlatform::new(&mut scope, realm)?;

    let key = DomNodeKey::new(1, NodeId::from_index(1));
    let wrapper = platform.get_or_create_wrapper(&mut scope, key, DomInterface::Element)?;
    let weak = WeakGcObject::from(wrapper);
    let root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    scope.heap_mut().remove_root(root);
    scope.heap_mut().collect_garbage();

    assert!(weak.upgrade(scope.heap()).is_none());

    // Re-wrapping after collection should succeed; identity may change.
    let wrapper2 = platform.get_or_create_wrapper(&mut scope, key, DomInterface::Element)?;
    assert_ne!(wrapper, wrapper2);
    Ok(())
  }

  #[test]
  fn brand_checks_throw_type_error_on_illegal_invocation() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (realm, heap) = split_runtime_realm(&mut runtime);
    let mut scope = heap.scope();
    let mut platform = DomPlatform::new(&mut scope, realm)?;

    let key = DomNodeKey::new(1, NodeId::from_index(1));
    let wrapper = platform.get_or_create_wrapper(&mut scope, key, DomInterface::Element)?;
    let _root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    assert_eq!(
      platform.require_node_handle(scope.heap(), Value::Object(wrapper))?,
      key
    );
    assert_eq!(
      platform.require_element_handle(scope.heap(), Value::Object(wrapper))?,
      key
    );

    let input_key = DomNodeKey::new(1, NodeId::from_index(2));
    let input_wrapper =
      platform.get_or_create_wrapper(&mut scope, input_key, DomInterface::HTMLInputElement)?;
    let _input_root = scope.heap_mut().add_root(Value::Object(input_wrapper))?;
    assert_eq!(
      platform.require_node_handle(scope.heap(), Value::Object(input_wrapper))?,
      input_key
    );
    assert_eq!(
      platform.require_element_handle(scope.heap(), Value::Object(input_wrapper))?,
      input_key
    );

    let err = platform.require_document_handle(scope.heap(), Value::Object(wrapper));
    assert!(matches!(err, Err(VmError::TypeError("Illegal invocation"))));

    let obj = scope.alloc_object()?;
    let err = platform.require_node_handle(scope.heap(), Value::Object(obj));
    assert!(matches!(err, Err(VmError::TypeError("Illegal invocation"))));

    let err = platform.require_node_handle(scope.heap(), Value::Undefined);
    assert!(matches!(err, Err(VmError::TypeError("Illegal invocation"))));
    Ok(())
  }

  #[test]
  fn remap_preserves_wrapper_identity() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (realm, heap) = split_runtime_realm(&mut runtime);
    let mut scope = heap.scope();
    let mut platform = DomPlatform::new(&mut scope, realm)?;

    let document_obj = scope.alloc_object()?;
    let document_id = gc_object_id(document_obj);
    let _doc_root = scope.heap_mut().add_root(Value::Object(document_obj))?;

    let old_id = NodeId::from_index(5);
    let wrapper = platform.get_or_create_wrapper(
      &mut scope,
      DomNodeKey::new(document_id, old_id),
      DomInterface::Element,
    )?;
    let root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    let new_id = NodeId::from_index(9);
    let mut mapping: HashMap<NodeId, NodeId> = HashMap::new();
    mapping.insert(old_id, new_id);

    platform.remap_node_ids(scope.heap_mut(), document_id, &mapping)?;

    let wrapper2 = platform.get_or_create_wrapper(
      &mut scope,
      DomNodeKey::new(document_id, new_id),
      DomInterface::Element,
    )?;
    assert_eq!(wrapper, wrapper2);

    let key = PropertyKey::from_string(scope.alloc_string(super::INTERNAL_NODE_ID_KEY)?);
    let value = scope
      .heap()
      .object_get_own_data_property_value(wrapper, &key)?
      .unwrap_or(Value::Undefined);
    assert_eq!(value, Value::Number(new_id.index() as f64));

    scope.heap_mut().remove_root(root);
    Ok(())
  }

  #[test]
  fn html_element_prototype_chain() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (realm, heap) = split_runtime_realm(&mut runtime);
    let mut scope = heap.scope();
    let platform = DomPlatform::new(&mut scope, realm)?;

    let element_proto = platform.prototype_for(DomInterface::Element);
    let html_element_proto = platform.prototype_for(DomInterface::HTMLElement);
    let html_input_proto = platform.prototype_for(DomInterface::HTMLInputElement);

    assert_eq!(
      scope.heap().object_prototype(html_element_proto)?,
      Some(element_proto)
    );
    assert_eq!(
      scope.heap().object_prototype(html_input_proto)?,
      Some(html_element_proto)
    );
    Ok(())
  }

  #[test]
  fn doctype_nodes_use_document_type_primary_interface() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (realm, heap) = split_runtime_realm(&mut runtime);
    let mut scope = heap.scope();
    let mut platform = DomPlatform::new(&mut scope, realm)?;

    let node_kind = NodeKind::Doctype {
      name: "html".to_string(),
      public_id: "p".to_string(),
      system_id: "s".to_string(),
    };
    let primary = DomInterface::primary_for_node_kind(&node_kind);
    assert_eq!(primary, DomInterface::DocumentType);

    let key = DomNodeKey::new(1, NodeId::from_index(1));
    let wrapper = platform.get_or_create_wrapper(&mut scope, key, primary)?;
    let _root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    assert_eq!(
      platform.require_document_type_handle(scope.heap(), Value::Object(wrapper))?,
      key
    );
    Ok(())
  }

  #[test]
  fn remap_across_documents_preserves_wrapper_identity() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (realm, heap) = split_runtime_realm(&mut runtime);
    let mut scope = heap.scope();
    let mut platform = DomPlatform::new(&mut scope, realm)?;

    let document_a = scope.alloc_object()?;
    let document_b = scope.alloc_object()?;
    let document_id_a = gc_object_id(document_a);
    let document_id_b = gc_object_id(document_b);
    let _doc_a_root = scope.heap_mut().add_root(Value::Object(document_a))?;
    let _doc_b_root = scope.heap_mut().add_root(Value::Object(document_b))?;

    let old_id = NodeId::from_index(5);
    let wrapper = platform.get_or_create_wrapper(
      &mut scope,
      DomNodeKey::new(document_id_a, old_id),
      DomInterface::Element,
    )?;
    let root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    let new_id = NodeId::from_index(9);
    let mut mapping: HashMap<NodeId, NodeId> = HashMap::new();
    mapping.insert(old_id, new_id);

    platform.remap_node_ids_between_documents(scope.heap_mut(), document_id_a, document_id_b, &mapping)?;

    let wrapper2 = platform.get_or_create_wrapper(
      &mut scope,
      DomNodeKey::new(document_id_b, new_id),
      DomInterface::Element,
    )?;
    assert_eq!(wrapper, wrapper2);

    assert_eq!(
      platform.require_node_handle(scope.heap(), Value::Object(wrapper))?,
      DomNodeKey::new(document_id_b, new_id)
    );

    let key = PropertyKey::from_string(scope.alloc_string(super::INTERNAL_NODE_ID_KEY)?);
    let value = scope
      .heap()
      .object_get_own_data_property_value(wrapper, &key)?
      .unwrap_or(Value::Undefined);
    assert_eq!(value, Value::Number(new_id.index() as f64));

    scope.heap_mut().remove_root(root);
    Ok(())
  }
}
