use crate::dom::HTML_NAMESPACE;
use crate::dom2::{NodeId, NodeKind};
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

#[inline]
fn document_id_from_key(document_key: WeakGcObject) -> DocumentId {
  (document_key.index() as u64) | ((document_key.generation() as u64) << 32)
}

/// Unique identity for a `dom2` node in a realm.
///
/// This is the cache key used by `DomPlatform` when maintaining stable wrapper identity across
/// multiple documents inside the same realm.
///
/// Note: `dom2::NodeId` values are only unique within a document, not across documents.
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

/// Primary interface brand for a `dom2` platform object wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomInterface {
  EventTarget,
  Node,
  DocumentType,
  Text,
  Comment,
  ProcessingInstruction,
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
  HTMLScriptElement,
  Document,
  DocumentFragment,
}

impl DomInterface {
  pub fn primary_for_node_kind(kind: &NodeKind) -> Self {
    match kind {
      NodeKind::Document { .. } => Self::Document,
      NodeKind::DocumentFragment => Self::DocumentFragment,
      NodeKind::Text { .. } => Self::Text,
      NodeKind::Comment { .. } => Self::Comment,
      NodeKind::ProcessingInstruction { .. } => Self::ProcessingInstruction,
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
        if tag_name.eq_ignore_ascii_case("script") {
          return Self::HTMLScriptElement;
        }

        Self::HTMLElement
      }
      NodeKind::Slot { .. } => Self::HTMLElement,
      NodeKind::Doctype { .. } => Self::DocumentType,
      _ => Self::Node,
    }
  }

  fn parent(self) -> Option<Self> {
    match self {
      Self::EventTarget => None,
      Self::Node => Some(Self::EventTarget),
      Self::Text
      | Self::Comment
      | Self::ProcessingInstruction
      | Self::Element
      | Self::Document
      | Self::DocumentFragment
      | Self::DocumentType => Some(Self::Node),
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
      | Self::HTMLLinkElement
      | Self::HTMLScriptElement => Some(Self::HTMLElement),
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

#[derive(Debug, Clone, Copy)]
pub struct DomPlatformPrototypes {
  pub event_target: GcObject,
  pub node: GcObject,
  pub document_type: GcObject,
  pub text: GcObject,
  pub comment: GcObject,
  pub processing_instruction: GcObject,
  pub element: GcObject,
  pub html_element: GcObject,
  pub html_input_element: GcObject,
  pub html_select_element: GcObject,
  pub html_text_area_element: GcObject,
  pub html_option_element: GcObject,
  pub html_form_element: GcObject,
  pub html_div_element: GcObject,
  pub html_span_element: GcObject,
  pub html_paragraph_element: GcObject,
  pub html_anchor_element: GcObject,
  pub html_image_element: GcObject,
  pub html_link_element: GcObject,
  pub html_script_element: GcObject,
  pub document: GcObject,
  pub document_fragment: GcObject,
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
  prototypes: DomPlatformPrototypes,
  prototype_roots: Vec<RootId>,
  wrappers_by_node: HashMap<DomNodeKey, WeakGcObject>,
  meta_by_wrapper: HashMap<WeakGcObject, DomWrapperMeta>,
  last_gc_runs: u64,
}

impl DomPlatform {
  fn lookup_global_interface_prototype(
    scope: &mut Scope<'_>,
    global: GcObject,
    interface: &'static str,
    err: &'static str,
  ) -> Result<GcObject, VmError> {
    let base = scope.heap().stack_root_len();
    scope.push_root(Value::Object(global))?;
    let ctor_key_s = scope.alloc_string(interface)?;
    scope.push_root(Value::String(ctor_key_s))?;
    let ctor_key = PropertyKey::from_string(ctor_key_s);
    let ctor_obj = match scope
      .heap()
      .object_get_own_data_property_value(global, &ctor_key)
    {
      Ok(Some(Value::Object(obj))) => obj,
      Ok(Some(_)) | Ok(None) | Err(VmError::PropertyNotData) => {
        scope.heap_mut().truncate_stack_roots(base);
        return Err(VmError::InvariantViolation(err));
      }
      Err(other) => {
        scope.heap_mut().truncate_stack_roots(base);
        return Err(other);
      }
    };
    scope.push_root(Value::Object(ctor_obj))?;

    let proto_key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(proto_key_s))?;
    let proto_key = PropertyKey::from_string(proto_key_s);
    let proto_obj = match scope
      .heap()
      .object_get_own_data_property_value(ctor_obj, &proto_key)
    {
      Ok(Some(Value::Object(obj))) => obj,
      Ok(Some(_)) | Ok(None) | Err(VmError::PropertyNotData) => {
        scope.heap_mut().truncate_stack_roots(base);
        return Err(VmError::InvariantViolation(err));
      }
      Err(other) => {
        scope.heap_mut().truncate_stack_roots(base);
        return Err(other);
      }
    };

    scope.heap_mut().truncate_stack_roots(base);
    Ok(proto_obj)
  }

  pub fn new_with_prototypes(
    scope: &mut Scope<'_>,
    realm: &Realm,
    prototypes: DomPlatformPrototypes,
  ) -> Result<Self, VmError> {
    let realm_id = realm.id();

    // Root prototypes: `DomPlatform` lives on the host side and is not traced by GC.
    //
    // Root each object immediately after acquiring it. Under a tight heap limit, subsequent
    // allocations can trigger GC, and unrooted prototypes would be collected (turning their
    // handles into stale values).
    let mut prototype_roots: Vec<RootId> = Vec::with_capacity(22);
    for proto in [
      prototypes.event_target,
      prototypes.node,
      prototypes.document_type,
      prototypes.text,
      prototypes.comment,
      prototypes.processing_instruction,
      prototypes.element,
      prototypes.html_element,
      prototypes.html_input_element,
      prototypes.html_select_element,
      prototypes.html_text_area_element,
      prototypes.html_option_element,
      prototypes.html_form_element,
      prototypes.html_div_element,
      prototypes.html_span_element,
      prototypes.html_paragraph_element,
      prototypes.html_anchor_element,
      prototypes.html_image_element,
      prototypes.html_link_element,
      prototypes.html_script_element,
      prototypes.document,
      prototypes.document_fragment,
    ] {
      prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto))?);
    }

    Ok(Self {
      realm_id,
      prototypes,
      prototype_roots,
      wrappers_by_node: HashMap::new(),
      meta_by_wrapper: HashMap::new(),
      last_gc_runs: scope.heap().gc_runs(),
    })
  }

  pub fn new_from_global(
    scope: &mut Scope<'_>,
    realm: &Realm,
    global: GcObject,
  ) -> Result<Self, VmError> {
    let realm_id = realm.id();

    // Root prototypes: `DomPlatform` lives on the host side and is not traced by GC.
    //
    // Root each object immediately after lookup. Under a tight heap limit, subsequent allocations
    // can trigger GC, and unrooted prototypes would be collected (turning their handles into stale
    // values).
    let mut prototype_roots: Vec<RootId> = Vec::with_capacity(22);

    macro_rules! lookup_proto {
      ($name:literal) => {{
        let proto = Self::lookup_global_interface_prototype(
          scope,
          global,
          $name,
          concat!(
            "DomPlatform::new_from_global expected globalThis.",
            $name,
            ".prototype"
          ),
        )?;
        prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto))?);
        proto
      }};
    }

    let proto_event_target = lookup_proto!("EventTarget");
    let proto_node = lookup_proto!("Node");
    let proto_document_type = lookup_proto!("DocumentType");
    let proto_text = lookup_proto!("Text");
    let proto_comment = lookup_proto!("Comment");
    let proto_processing_instruction = lookup_proto!("ProcessingInstruction");
    let proto_element = lookup_proto!("Element");
    let proto_html_element = lookup_proto!("HTMLElement");
    let proto_html_input_element = lookup_proto!("HTMLInputElement");
    let proto_html_select_element = lookup_proto!("HTMLSelectElement");
    let proto_html_text_area_element = lookup_proto!("HTMLTextAreaElement");
    let proto_html_option_element = lookup_proto!("HTMLOptionElement");
    let proto_html_form_element = lookup_proto!("HTMLFormElement");
    let proto_html_div_element = lookup_proto!("HTMLDivElement");
    let proto_html_span_element = lookup_proto!("HTMLSpanElement");
    let proto_html_paragraph_element = lookup_proto!("HTMLParagraphElement");
    let proto_html_anchor_element = lookup_proto!("HTMLAnchorElement");
    let proto_html_image_element = lookup_proto!("HTMLImageElement");
    let proto_html_link_element = lookup_proto!("HTMLLinkElement");
    let proto_html_script_element = lookup_proto!("HTMLScriptElement");
    let proto_document = lookup_proto!("Document");
    let proto_document_fragment = lookup_proto!("DocumentFragment");

    Ok(Self {
      realm_id,
      prototypes: DomPlatformPrototypes {
        event_target: proto_event_target,
        node: proto_node,
        document_type: proto_document_type,
        text: proto_text,
        comment: proto_comment,
        processing_instruction: proto_processing_instruction,
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
        html_script_element: proto_html_script_element,
        document: proto_document,
        document_fragment: proto_document_fragment,
      },
      prototype_roots,
      wrappers_by_node: HashMap::new(),
      meta_by_wrapper: HashMap::new(),
      last_gc_runs: scope.heap().gc_runs(),
    })
  }

  pub fn new(scope: &mut Scope<'_>, realm: &Realm) -> Result<Self, VmError> {
    let realm_id = realm.id();

    // Root prototypes: `DomPlatform` lives on the host side and is not traced by GC.
    //
    // Root each object immediately after allocation. Under a tight heap limit, subsequent
    // allocations can trigger GC, and unrooted prototypes would be collected (turning their
    // handles into stale values).
    let mut prototype_roots: Vec<RootId> = Vec::with_capacity(22);

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
    let proto_comment = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_comment))?);
    let proto_processing_instruction = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_processing_instruction))?);
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
    let proto_html_script_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_script_element))?);
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
    //   Comment -> Node
    //   ProcessingInstruction -> Node
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
      .object_set_prototype(proto_comment, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_processing_instruction, Some(proto_node))?;
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
      proto_html_script_element,
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
      prototypes: DomPlatformPrototypes {
        event_target: proto_event_target,
        node: proto_node,
        document_type: proto_document_type,
        text: proto_text,
        comment: proto_comment,
        processing_instruction: proto_processing_instruction,
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
        html_script_element: proto_html_script_element,
        document: proto_document,
        document_fragment: proto_document_fragment,
      },
      prototype_roots,
      wrappers_by_node: HashMap::new(),
      meta_by_wrapper: HashMap::new(),
      last_gc_runs: scope.heap().gc_runs(),
    })
  }

  /// Construct a [`DomPlatform`] by reusing prototype objects already installed on the realm's
  /// global object.
  ///
  /// This is used by the WebIDL-first DOM bindings backend so that native DOM wrappers created by
  /// the host (e.g. `document.createElement(..)`) inherit from the same `EventTarget.prototype` /
  /// `Node.prototype` objects as WebIDL-generated constructors.
  pub fn new_from_global_prototypes(scope: &mut Scope<'_>, realm: &Realm) -> Result<Self, VmError> {
    fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
      let s = scope.alloc_string(name)?;
      scope.push_root(Value::String(s))?;
      Ok(PropertyKey::from_string(s))
    }

    fn proto_from_global_ctor(
      scope: &mut Scope<'_>,
      global: GcObject,
      ctor: &str,
    ) -> Result<GcObject, VmError> {
      fn msg(ctor: &str, kind: &str) -> &'static str {
        match (ctor, kind) {
          ("EventTarget", "missing_ctor") => {
            "DomPlatform global prototype lookup missing EventTarget constructor"
          }
          ("EventTarget", "ctor_not_object") => {
            "DomPlatform global EventTarget constructor is not an object"
          }
          ("EventTarget", "missing_proto") => {
            "DomPlatform global EventTarget constructor missing prototype"
          }
          ("EventTarget", "proto_not_object") => {
            "DomPlatform global EventTarget constructor prototype is not an object"
          }
          ("Node", "missing_ctor") => "DomPlatform global prototype lookup missing Node constructor",
          ("Node", "ctor_not_object") => "DomPlatform global Node constructor is not an object",
          ("Node", "missing_proto") => "DomPlatform global Node constructor missing prototype",
          ("Node", "proto_not_object") => "DomPlatform global Node constructor prototype is not an object",
          _ => "DomPlatform global prototype lookup failed",
        }
      }

      scope.push_root(Value::Object(global))?;

      let ctor_key = alloc_key(scope, ctor)?;
      let Some(ctor_val) = scope
        .heap()
        .object_get_own_data_property_value(global, &ctor_key)?
      else {
        return Err(VmError::InvariantViolation(msg(ctor, "missing_ctor")));
      };
      let Value::Object(ctor_obj) = ctor_val else {
        return Err(VmError::InvariantViolation(msg(ctor, "ctor_not_object")));
      };
      scope.push_root(Value::Object(ctor_obj))?;

      let proto_key = alloc_key(scope, "prototype")?;
      let Some(proto_val) = scope
        .heap()
        .object_get_own_data_property_value(ctor_obj, &proto_key)?
      else {
        return Err(VmError::InvariantViolation(msg(ctor, "missing_proto")));
      };
      let Value::Object(proto_obj) = proto_val else {
        return Err(VmError::InvariantViolation(msg(ctor, "proto_not_object")));
      };
      Ok(proto_obj)
    }

    let realm_id = realm.id();
    let global = realm.global_object();

    // Root prototypes: `DomPlatform` lives on the host side and is not traced by GC.
    //
    // Root each object immediately after allocation. Under a tight heap limit, subsequent
    // allocations can trigger GC, and unrooted prototypes would be collected (turning their
    // handles into stale values).
    let mut prototype_roots: Vec<RootId> = Vec::with_capacity(22);

    // Reuse WebIDL-installed prototypes for the base interfaces we want to share across bindings
    // backends.
    let proto_event_target = proto_from_global_ctor(scope, global, "EventTarget")?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_event_target))?);
    let proto_node = proto_from_global_ctor(scope, global, "Node")?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_node))?);

    // Allocate remaining prototype objects in this realm, inheriting from the shared base
    // prototypes so `instanceof Node` works for wrappers created by the host.
    let proto_document_type = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_document_type))?);
    let proto_text = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_text))?);
    let proto_comment = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_comment))?);
    let proto_processing_instruction = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_processing_instruction))?);
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
    let proto_html_script_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_script_element))?);
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
    //   Comment -> Node
    //   ProcessingInstruction -> Node
    //   Element -> Node
    //   HTMLElement -> Element
    //   HTML*Element -> HTMLElement
    //   Document -> Node
    //   DocumentFragment -> Node
    scope
      .heap_mut()
      .object_set_prototype(proto_document_type, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_text, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_comment, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_processing_instruction, Some(proto_node))?;
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
      proto_html_script_element,
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
      prototypes: DomPlatformPrototypes {
        event_target: proto_event_target,
        node: proto_node,
        document_type: proto_document_type,
        text: proto_text,
        comment: proto_comment,
        processing_instruction: proto_processing_instruction,
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
        html_script_element: proto_html_script_element,
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
      DomInterface::Comment => self.prototypes.comment,
      DomInterface::ProcessingInstruction => self.prototypes.processing_instruction,
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
      DomInterface::HTMLScriptElement => self.prototypes.html_script_element,
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
    document_key: WeakGcObject,
    node_id: NodeId,
    primary_interface: DomInterface,
  ) {
    self.sweep_dead_wrappers_if_needed(heap);
    let document_id = document_id_from_key(document_key);
    let key = DomNodeKey::new(document_id, node_id);
    let weak = WeakGcObject::from(wrapper);
    self.wrappers_by_node.insert(key, weak);
    self.meta_by_wrapper.insert(
      weak,
      DomWrapperMeta {
        document_id,
        node_id,
        primary_interface,
        realm_id: self.realm_id,
      },
    );
  }

  /// Return an existing wrapper for `node_id` if still alive.
  pub fn get_existing_wrapper(
    &mut self,
    heap: &Heap,
    document_key: WeakGcObject,
    node_id: NodeId,
  ) -> Option<GcObject> {
    self.sweep_dead_wrappers_if_needed(heap);
    let key = DomNodeKey::new(document_id_from_key(document_key), node_id);
    self
      .wrappers_by_node
      .get(&key)
      .copied()
      .and_then(|weak| weak.upgrade(heap))
  }

  pub fn get_or_create_wrapper(
    &mut self,
    scope: &mut Scope<'_>,
    document_key: WeakGcObject,
    node_id: NodeId,
    primary_interface: DomInterface,
  ) -> Result<GcObject, VmError> {
    if let Some(existing) = self.get_existing_wrapper(scope.heap(), document_key, node_id) {
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
            value: Value::Number(node_id.index() as f64),
            writable: true,
          },
        },
      )?;
    }

    self.register_wrapper(
      scope.heap(),
      wrapper,
      document_key,
      node_id,
      primary_interface,
    );
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

  pub fn require_interface_node_handle(
    &mut self,
    heap: &Heap,
    value: Value,
    interface: DomInterface,
  ) -> Result<DomNodeKey, VmError> {
    let meta = self.require_wrapper_meta(heap, value)?;
    if !meta.primary_interface.implements(interface) {
      return Err(VmError::TypeError("Illegal invocation"));
    }
    Ok(DomNodeKey::new(meta.document_id, meta.node_id))
  }

  pub fn require_interface_node_id(
    &mut self,
    heap: &Heap,
    value: Value,
    interface: DomInterface,
  ) -> Result<NodeId, VmError> {
    Ok(self
      .require_interface_node_handle(heap, value, interface)?
      .node_id)
  }

  pub fn require_node_handle(&mut self, heap: &Heap, value: Value) -> Result<DomNodeKey, VmError> {
    self.require_interface_node_handle(heap, value, DomInterface::Node)
  }

  pub fn require_element_handle(&mut self, heap: &Heap, value: Value) -> Result<DomNodeKey, VmError> {
    self.require_interface_node_handle(heap, value, DomInterface::Element)
  }

  pub fn require_text_handle(&mut self, heap: &Heap, value: Value) -> Result<DomNodeKey, VmError> {
    self.require_interface_node_handle(heap, value, DomInterface::Text)
  }

  pub fn require_comment_handle(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<DomNodeKey, VmError> {
    self.require_interface_node_handle(heap, value, DomInterface::Comment)
  }

  pub fn require_processing_instruction_handle(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<DomNodeKey, VmError> {
    self.require_interface_node_handle(heap, value, DomInterface::ProcessingInstruction)
  }

  pub fn require_document_type_handle(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<DomNodeKey, VmError> {
    self.require_interface_node_handle(heap, value, DomInterface::DocumentType)
  }

  pub fn require_document_handle(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<DomNodeKey, VmError> {
    self.require_interface_node_handle(heap, value, DomInterface::Document)
  }

  pub fn require_document_fragment_handle(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<DomNodeKey, VmError> {
    self.require_interface_node_handle(heap, value, DomInterface::DocumentFragment)
  }

  pub fn require_node_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    self.require_interface_node_id(heap, value, DomInterface::Node)
  }

  pub fn require_element_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    self.require_interface_node_id(heap, value, DomInterface::Element)
  }

  pub fn require_text_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    self.require_interface_node_id(heap, value, DomInterface::Text)
  }

  pub fn require_comment_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    Ok(self.require_comment_handle(heap, value)?.node_id)
  }

  pub fn require_processing_instruction_id(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<NodeId, VmError> {
    Ok(self.require_processing_instruction_handle(heap, value)?.node_id)
  }

  pub fn require_document_type_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    self.require_interface_node_id(heap, value, DomInterface::DocumentType)
  }

  pub fn require_document_id(&mut self, heap: &Heap, value: Value) -> Result<NodeId, VmError> {
    self.require_interface_node_id(heap, value, DomInterface::Document)
  }

  pub fn require_document_fragment_id(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<NodeId, VmError> {
    self.require_interface_node_id(heap, value, DomInterface::DocumentFragment)
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
  use crate::dom::{HTML_NAMESPACE, SVG_NAMESPACE};
  use crate::dom2::{NodeId, NodeKind};
  use std::collections::HashMap;
  use vm_js::{
    GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Value, Vm,
    VmError, VmOptions, WeakGcObject,
  };

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

  fn alloc_key(scope: &mut vm_js::Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
    let s = scope.alloc_string(name)?;
    scope.push_root(Value::String(s))?;
    Ok(PropertyKey::from_string(s))
  }

  fn get_global_interface_prototype(
    scope: &mut vm_js::Scope<'_>,
    global: GcObject,
    name: &str,
  ) -> Result<GcObject, VmError> {
    let ctor_key = alloc_key(scope, name)?;
    let ctor_val = scope
      .heap()
      .object_get_own_data_property_value(global, &ctor_key)?
      .ok_or(VmError::TypeError("missing global constructor"))?;
    let Value::Object(ctor_obj) = ctor_val else {
      return Err(VmError::TypeError("global constructor is not an object"));
    };
    scope.push_root(Value::Object(ctor_obj))?;

    let proto_key = alloc_key(scope, "prototype")?;
    let proto_val = scope
      .heap()
      .object_get_own_data_property_value(ctor_obj, &proto_key)?
      .ok_or(VmError::TypeError("missing constructor.prototype"))?;
    let Value::Object(proto_obj) = proto_val else {
      return Err(VmError::TypeError("constructor.prototype is not an object"));
    };
    Ok(proto_obj)
  }

  fn install_stub_interface(
    scope: &mut vm_js::Scope<'_>,
    global: GcObject,
    name: &str,
    parent_proto: GcObject,
  ) -> Result<GcObject, VmError> {
    // If a real interface already exists (e.g. installed by WebIDL bindings), reuse it.
    if let Ok(existing) = get_global_interface_prototype(scope, global, name) {
      return Ok(existing);
    }

    let proto = scope.alloc_object()?;
    scope.push_root(Value::Object(proto))?;
    scope
      .heap_mut()
      .object_set_prototype(proto, Some(parent_proto))?;

    let ctor = scope.alloc_object()?;
    scope.push_root(Value::Object(ctor))?;

    let proto_key = alloc_key(scope, "prototype")?;
    scope.define_property(
      ctor,
      proto_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Object(proto),
          writable: true,
        },
      },
    )?;

    let ctor_key = alloc_key(scope, name)?;
    scope.define_property(
      global,
      ctor_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Object(ctor),
          writable: true,
        },
      },
    )?;
    Ok(proto)
  }

  fn install_dom_interface_stubs_for_platform(
    scope: &mut vm_js::Scope<'_>,
    global: GcObject,
    node_proto: GcObject,
  ) -> Result<(GcObject, GcObject), VmError> {
    // Interfaces inheriting from Node.
    for name in [
      "DocumentType",
      "Text",
      "Comment",
      "ProcessingInstruction",
      "Document",
      "DocumentFragment",
    ] {
      let _ = install_stub_interface(scope, global, name, node_proto)?;
    }

    // Element + HTMLElement + HTML*Element chain.
    let element_proto = install_stub_interface(scope, global, "Element", node_proto)?;
    let html_element_proto = install_stub_interface(scope, global, "HTMLElement", element_proto)?;
    for name in [
      "HTMLInputElement",
      "HTMLSelectElement",
      "HTMLTextAreaElement",
      "HTMLOptionElement",
      "HTMLFormElement",
      "HTMLDivElement",
      "HTMLSpanElement",
      "HTMLParagraphElement",
      "HTMLAnchorElement",
      "HTMLImageElement",
      "HTMLLinkElement",
      "HTMLScriptElement",
    ] {
      let _ = install_stub_interface(scope, global, name, html_element_proto)?;
    }
    Ok((element_proto, html_element_proto))
  }

  #[test]
  fn wrapping_same_node_id_preserves_identity_while_alive() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (realm, heap) = split_runtime_realm(&mut runtime);
    let mut scope = heap.scope();
    let mut platform = DomPlatform::new(&mut scope, realm)?;

    let document_obj = scope.alloc_object()?;
    let document_key = WeakGcObject::from(document_obj);
    let _doc_root = scope.heap_mut().add_root(Value::Object(document_obj))?;

    let node_id = NodeId::from_index(1);
    let wrapper1 =
      platform.get_or_create_wrapper(&mut scope, document_key, node_id, DomInterface::Element)?;
    let root = scope.heap_mut().add_root(Value::Object(wrapper1))?;

    let wrapper2 =
      platform.get_or_create_wrapper(&mut scope, document_key, node_id, DomInterface::Element)?;
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
    let doc_key_a = WeakGcObject::from(doc_a);
    let doc_key_b = WeakGcObject::from(doc_b);
    let _doc_a_root = scope.heap_mut().add_root(Value::Object(doc_a))?;
    let _doc_b_root = scope.heap_mut().add_root(Value::Object(doc_b))?;

    let node_id = NodeId::from_index(1);
    let wrapper_a =
      platform.get_or_create_wrapper(&mut scope, doc_key_a, node_id, DomInterface::Element)?;
    let wrapper_b =
      platform.get_or_create_wrapper(&mut scope, doc_key_b, node_id, DomInterface::Element)?;

    assert_ne!(wrapper_a, wrapper_b);
    Ok(())
  }

  #[test]
  fn wrapper_can_be_collected_when_unreachable() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (realm, heap) = split_runtime_realm(&mut runtime);
    let mut scope = heap.scope();
    let mut platform = DomPlatform::new(&mut scope, realm)?;

    let document_obj = scope.alloc_object()?;
    let document_key = WeakGcObject::from(document_obj);
    let _doc_root = scope.heap_mut().add_root(Value::Object(document_obj))?;

    let node_id = NodeId::from_index(1);
    let wrapper =
      platform.get_or_create_wrapper(&mut scope, document_key, node_id, DomInterface::Element)?;
    let weak = WeakGcObject::from(wrapper);
    let root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    scope.heap_mut().remove_root(root);
    scope.heap_mut().collect_garbage();

    assert!(weak.upgrade(scope.heap()).is_none());

    // Re-wrapping after collection should succeed; identity may change.
    let wrapper2 =
      platform.get_or_create_wrapper(&mut scope, document_key, node_id, DomInterface::Element)?;
    assert_ne!(wrapper, wrapper2);
    Ok(())
  }

  #[test]
  fn brand_checks_throw_type_error_on_illegal_invocation() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (realm, heap) = split_runtime_realm(&mut runtime);
    let mut scope = heap.scope();
    let mut platform = DomPlatform::new(&mut scope, realm)?;

    let document_obj = scope.alloc_object()?;
    let document_key = WeakGcObject::from(document_obj);
    let _doc_root = scope.heap_mut().add_root(Value::Object(document_obj))?;
    let document_id = super::document_id_from_key(document_key);

    let node_id = NodeId::from_index(1);
    let key = DomNodeKey::new(document_id, node_id);
    let wrapper =
      platform.get_or_create_wrapper(&mut scope, document_key, node_id, DomInterface::Element)?;
    let _root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    assert_eq!(
      platform.require_node_handle(scope.heap(), Value::Object(wrapper))?,
      key
    );
    assert_eq!(
      platform.require_element_handle(scope.heap(), Value::Object(wrapper))?,
      key
    );

    let input_node_id = NodeId::from_index(2);
    let input_key = DomNodeKey::new(document_id, input_node_id);
    let input_wrapper = platform.get_or_create_wrapper(
      &mut scope,
      document_key,
      input_node_id,
      DomInterface::HTMLInputElement,
    )?;
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
    let document_key = WeakGcObject::from(document_obj);
    let _doc_root = scope.heap_mut().add_root(Value::Object(document_obj))?;
    let document_id = super::document_id_from_key(document_key);

    let old_id = NodeId::from_index(5);
    let wrapper =
      platform.get_or_create_wrapper(&mut scope, document_key, old_id, DomInterface::Element)?;
    let root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    let new_id = NodeId::from_index(9);
    let mut mapping: HashMap<NodeId, NodeId> = HashMap::new();
    mapping.insert(old_id, new_id);

    platform.remap_node_ids(scope.heap_mut(), document_id, &mapping)?;

    let wrapper2 =
      platform.get_or_create_wrapper(&mut scope, document_key, new_id, DomInterface::Element)?;
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
    let html_script_proto = platform.prototype_for(DomInterface::HTMLScriptElement);

    assert_eq!(
      scope.heap().object_prototype(html_element_proto)?,
      Some(element_proto)
    );
    assert_eq!(
      scope.heap().object_prototype(html_input_proto)?,
      Some(html_element_proto)
    );
    assert_eq!(
      scope.heap().object_prototype(html_script_proto)?,
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

    let document_obj = scope.alloc_object()?;
    let document_key = WeakGcObject::from(document_obj);
    let _doc_root = scope.heap_mut().add_root(Value::Object(document_obj))?;
    let document_id = super::document_id_from_key(document_key);

    let node_kind = NodeKind::Doctype {
      name: "html".to_string(),
      public_id: "p".to_string(),
      system_id: "s".to_string(),
    };
    let primary = DomInterface::primary_for_node_kind(&node_kind);
    assert_eq!(primary, DomInterface::DocumentType);

    let node_id = NodeId::from_index(1);
    let key = DomNodeKey::new(document_id, node_id);
    let wrapper = platform.get_or_create_wrapper(&mut scope, document_key, node_id, primary)?;
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
    let document_key_a = WeakGcObject::from(document_a);
    let document_key_b = WeakGcObject::from(document_b);
    let document_id_a = super::document_id_from_key(document_key_a);
    let document_id_b = super::document_id_from_key(document_key_b);
    let _doc_a_root = scope.heap_mut().add_root(Value::Object(document_a))?;
    let _doc_b_root = scope.heap_mut().add_root(Value::Object(document_b))?;

    let old_id = NodeId::from_index(5);
    let wrapper =
      platform.get_or_create_wrapper(&mut scope, document_key_a, old_id, DomInterface::Element)?;
    let root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    let new_id = NodeId::from_index(9);
    let mut mapping: HashMap<NodeId, NodeId> = HashMap::new();
    mapping.insert(old_id, new_id);

    platform.remap_node_ids_between_documents(
      scope.heap_mut(),
      document_id_a,
      document_id_b,
      &mapping,
    )?;

    let wrapper2 =
      platform.get_or_create_wrapper(&mut scope, document_key_b, new_id, DomInterface::Element)?;
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

  #[test]
  fn new_from_global_adopts_realm_interface_prototypes_for_wrappers() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (vm, realm, heap) = runtime.vm_realm_and_heap_mut();

    crate::js::bindings::install_event_target_bindings_vm_js(vm, heap, realm)?;
    crate::js::bindings::install_node_bindings_vm_js(vm, heap, realm)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    let node_proto = get_global_interface_prototype(&mut scope, global, "Node")?;
    scope.push_root(Value::Object(node_proto))?;

    let _ = install_dom_interface_stubs_for_platform(&mut scope, global, node_proto)?;

    let mut platform = DomPlatform::new_from_global(&mut scope, realm, global)?;

    let document_obj = scope.alloc_object()?;
    let document_key = WeakGcObject::from(document_obj);
    let _doc_root = scope.heap_mut().add_root(Value::Object(document_obj))?;

    let node_id = NodeId::from_index(1);
    let wrapper =
      platform.get_or_create_wrapper(&mut scope, document_key, node_id, DomInterface::Node)?;
    let _wrapper_root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    assert_eq!(scope.heap().object_prototype(wrapper)?, Some(node_proto));
    Ok(())
  }

  #[test]
  fn wrapper_prototype_chain_matches_realm_interface_chain() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (vm, realm, heap) = runtime.vm_realm_and_heap_mut();

    crate::js::bindings::install_event_target_bindings_vm_js(vm, heap, realm)?;
    crate::js::bindings::install_node_bindings_vm_js(vm, heap, realm)?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    let event_target_proto = get_global_interface_prototype(&mut scope, global, "EventTarget")?;
    scope.push_root(Value::Object(event_target_proto))?;
    let node_proto = get_global_interface_prototype(&mut scope, global, "Node")?;
    scope.push_root(Value::Object(node_proto))?;

    let (_element_proto, _html_element_proto) =
      install_dom_interface_stubs_for_platform(&mut scope, global, node_proto)?;

    let html_input_proto = get_global_interface_prototype(&mut scope, global, "HTMLInputElement")?;
    let html_element_proto = get_global_interface_prototype(&mut scope, global, "HTMLElement")?;
    let element_proto = get_global_interface_prototype(&mut scope, global, "Element")?;

    let mut platform = DomPlatform::new_from_global(&mut scope, realm, global)?;

    let document_obj = scope.alloc_object()?;
    let document_key = WeakGcObject::from(document_obj);
    let _doc_root = scope.heap_mut().add_root(Value::Object(document_obj))?;

    let node_id = NodeId::from_index(1);
    let wrapper = platform.get_or_create_wrapper(
      &mut scope,
      document_key,
      node_id,
      DomInterface::HTMLInputElement,
    )?;
    let _wrapper_root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    assert_eq!(scope.heap().object_prototype(wrapper)?, Some(html_input_proto));
    assert_eq!(
      scope.heap().object_prototype(html_input_proto)?,
      Some(html_element_proto)
    );
    assert_eq!(
      scope.heap().object_prototype(html_element_proto)?,
      Some(element_proto)
    );
    assert_eq!(scope.heap().object_prototype(element_proto)?, Some(node_proto));
    assert_eq!(
      scope.heap().object_prototype(node_proto)?,
      Some(event_target_proto)
    );
    Ok(())
  }

  #[test]
  fn implements_follows_html_element_inheritance_chain() {
    assert!(DomInterface::HTMLElement.implements(DomInterface::Element));
    assert!(DomInterface::HTMLElement.implements(DomInterface::Node));
    assert!(DomInterface::HTMLElement.implements(DomInterface::EventTarget));

    assert!(DomInterface::HTMLInputElement.implements(DomInterface::HTMLElement));
    assert!(DomInterface::HTMLInputElement.implements(DomInterface::Element));
    assert!(DomInterface::HTMLInputElement.implements(DomInterface::Node));
    assert!(DomInterface::HTMLInputElement.implements(DomInterface::EventTarget));

    assert!(!DomInterface::HTMLElement.implements(DomInterface::HTMLInputElement));
    assert!(!DomInterface::Element.implements(DomInterface::HTMLElement));
  }

  #[test]
  fn primary_for_node_kind_maps_html_tags_to_interfaces() {
    let kind = NodeKind::Element {
      tag_name: "INPUT".into(),
      namespace: "".into(),
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLInputElement
    );

    let kind = NodeKind::Element {
      tag_name: "textarea".into(),
      namespace: HTML_NAMESPACE.into(),
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLTextAreaElement
    );

    let kind = NodeKind::Element {
      tag_name: "select".into(),
      namespace: "".into(),
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLSelectElement
    );

    let kind = NodeKind::Element {
      tag_name: "option".into(),
      namespace: "".into(),
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLOptionElement
    );

    let kind = NodeKind::Element {
      tag_name: "form".into(),
      namespace: "".into(),
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLFormElement
    );

    let kind = NodeKind::Element {
      tag_name: "img".into(),
      namespace: "".into(),
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLImageElement
    );

    let kind = NodeKind::Element {
      tag_name: "a".into(),
      namespace: "".into(),
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLAnchorElement
    );

    let kind = NodeKind::Element {
      tag_name: "link".into(),
      namespace: "".into(),
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLLinkElement
    );

    let kind = NodeKind::Element {
      tag_name: "script".into(),
      namespace: "".into(),
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLScriptElement
    );

    let kind = NodeKind::Element {
      tag_name: "div".into(),
      namespace: HTML_NAMESPACE.into(),
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLDivElement
    );

    // Unknown HTML tags still brand as generic HTMLElement.
    let kind = NodeKind::Element {
      tag_name: "article".into(),
      namespace: HTML_NAMESPACE.into(),
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLElement
    );

    // Non-HTML namespace always falls back to the generic Element brand.
    let kind = NodeKind::Element {
      tag_name: "input".into(),
      namespace: SVG_NAMESPACE.into(),
      attributes: vec![],
    };
    assert_eq!(DomInterface::primary_for_node_kind(&kind), DomInterface::Element);

    let kind = NodeKind::Slot {
      namespace: "".into(),
      attributes: vec![],
      assigned: false,
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLElement
    );
  }
}
