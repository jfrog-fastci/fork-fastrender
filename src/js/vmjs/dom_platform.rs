use crate::dom::HTML_NAMESPACE;
use crate::dom2::{NodeId, NodeKind};
use crate::js::dom_internal_keys::{
  CSS_STYLE_DECL_PROTOTYPE_KEY, HTML_COLLECTION_PROTOTYPE_KEY, NODE_ID_KEY, NODE_LIST_PROTOTYPE_KEY,
  STYLE_CSS_TEXT_GET_KEY, STYLE_CSS_TEXT_SET_KEY, STYLE_CURSOR_GET_KEY, STYLE_CURSOR_SET_KEY,
  STYLE_DISPLAY_GET_KEY, STYLE_DISPLAY_SET_KEY, STYLE_GET_PROPERTY_VALUE_KEY, STYLE_HEIGHT_GET_KEY,
  STYLE_HEIGHT_SET_KEY, STYLE_REMOVE_PROPERTY_KEY, STYLE_SET_PROPERTY_KEY, STYLE_WIDTH_GET_KEY,
  STYLE_WIDTH_SET_KEY, WRAPPER_DOCUMENT_KEY,
};
use crate::web::events::EventTargetId;
use std::collections::HashMap;
use vm_js::{
  GcObject, Heap, HostSlots, PropertyDescriptor, PropertyKey, PropertyKind, Realm, RealmId, RootId,
  Scope, Value, VmError, WeakGcObject,
};

/// Uniquely identifies a `dom2::Document` within a JS realm.
///
/// Note: `dom2::NodeId` values are only unique within a document, not across documents.
pub type DocumentId = u64;

/// HostSlots tag used to brand DOM platform object wrappers (Document/Element/etc).
///
/// The `structuredClone()` implementation treats any object with `HostSlots` as a platform object
/// and throws `DataCloneError` (HTML structured clone algorithm).
pub const DOM_WRAPPER_HOST_TAG: u64 = 0x444F_4D57_5241_5050; // "DOMWRAPP"

/// HostSlots tag used to brand CSSStyleDeclaration-like objects cached on element wrappers.
///
/// Must match `window_realm::CSS_STYLE_DECL_HOST_TAG`.
const CSS_STYLE_DECL_HOST_TAG: u64 = u64::from_be_bytes(*b"FRDOMCSS");

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
  CharacterData,
  DocumentType,
  Text,
  Comment,
  ProcessingInstruction,
  Element,
  HTMLElement,
  HTMLMediaElement,
  HTMLVideoElement,
  HTMLAudioElement,
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
  HTMLIFrameElement,
  Document,
  DocumentFragment,
  ShadowRoot,
}

impl DomInterface {
  pub fn primary_for_node_kind(kind: &NodeKind) -> Self {
    match kind {
      NodeKind::Document { .. } => Self::Document,
      NodeKind::DocumentFragment => Self::DocumentFragment,
      NodeKind::ShadowRoot { .. } => Self::ShadowRoot,
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
        if tag_name.eq_ignore_ascii_case("video") {
          return Self::HTMLVideoElement;
        }
        if tag_name.eq_ignore_ascii_case("audio") {
          return Self::HTMLAudioElement;
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

        if tag_name.eq_ignore_ascii_case("video") {
          return Self::HTMLVideoElement;
        }
        if tag_name.eq_ignore_ascii_case("audio") {
          return Self::HTMLAudioElement;
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
        if tag_name.eq_ignore_ascii_case("iframe") {
          return Self::HTMLIFrameElement;
        }
        if tag_name.eq_ignore_ascii_case("video") {
          return Self::HTMLVideoElement;
        }
        if tag_name.eq_ignore_ascii_case("audio") {
          return Self::HTMLAudioElement;
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
      Self::CharacterData | Self::Element
      | Self::Document
      | Self::DocumentFragment
      | Self::DocumentType => Some(Self::Node),
      Self::Text | Self::Comment | Self::ProcessingInstruction => Some(Self::CharacterData),
      Self::ShadowRoot => Some(Self::DocumentFragment),
      Self::HTMLElement => Some(Self::Element),
      Self::HTMLMediaElement => Some(Self::HTMLElement),
      Self::HTMLVideoElement | Self::HTMLAudioElement => Some(Self::HTMLMediaElement),
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
      | Self::HTMLScriptElement
      | Self::HTMLIFrameElement => Some(Self::HTMLElement),
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
  pub character_data: GcObject,
  pub document_type: GcObject,
  pub text: GcObject,
  pub comment: GcObject,
  pub processing_instruction: GcObject,
  pub element: GcObject,
  pub html_element: GcObject,
  pub html_media_element: GcObject,
  pub html_video_element: GcObject,
  pub html_audio_element: GcObject,
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
  pub html_iframe_element: GcObject,
  pub document: GcObject,
  pub document_fragment: GcObject,
  pub shadow_root: GcObject,
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
    let mut prototype_roots: Vec<RootId> = Vec::with_capacity(27);
    for proto in [
      prototypes.event_target,
      prototypes.node,
      prototypes.character_data,
      prototypes.document_type,
      prototypes.text,
      prototypes.comment,
      prototypes.processing_instruction,
      prototypes.element,
      prototypes.html_element,
      prototypes.html_media_element,
      prototypes.html_video_element,
      prototypes.html_audio_element,
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
      prototypes.html_iframe_element,
      prototypes.document,
      prototypes.document_fragment,
      prototypes.shadow_root,
    ] {
      prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto))?);
    }

    // Ensure the media-element prototype chain matches WebIDL (in case the prototypes were
    // allocated by a caller that did not wire up inheritance).
    //
    // Do this *after* rooting so a tight heap limit can't collect prototypes if `object_set_prototype`
    // allocates internally.
    scope
      .heap_mut()
      .object_set_prototype(prototypes.html_media_element, Some(prototypes.html_element))?;
    scope
      .heap_mut()
      .object_set_prototype(prototypes.html_video_element, Some(prototypes.html_media_element))?;
    scope
      .heap_mut()
      .object_set_prototype(prototypes.html_audio_element, Some(prototypes.html_media_element))?;

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
    let mut prototype_roots: Vec<RootId> = Vec::with_capacity(27);

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
    let proto_character_data = lookup_proto!("CharacterData");
    let proto_document_type = lookup_proto!("DocumentType");
    let proto_text = lookup_proto!("Text");
    let proto_comment = lookup_proto!("Comment");
    let proto_processing_instruction = lookup_proto!("ProcessingInstruction");
    let proto_element = lookup_proto!("Element");
    let proto_html_element = lookup_proto!("HTMLElement");
    let proto_html_media_element = lookup_proto!("HTMLMediaElement");
    let proto_html_video_element = lookup_proto!("HTMLVideoElement");
    let proto_html_audio_element = lookup_proto!("HTMLAudioElement");
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
    let proto_html_iframe_element = lookup_proto!("HTMLIFrameElement");
    let proto_document = lookup_proto!("Document");
    let proto_document_fragment = lookup_proto!("DocumentFragment");
    let proto_shadow_root = lookup_proto!("ShadowRoot");

    // Ensure media-element inheritance chain matches WebIDL (in case prototypes were installed
    // out-of-order).
    scope
      .heap_mut()
      .object_set_prototype(proto_html_media_element, Some(proto_html_element))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_html_video_element, Some(proto_html_media_element))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_html_audio_element, Some(proto_html_media_element))?;

    Ok(Self {
      realm_id,
      prototypes: DomPlatformPrototypes {
        event_target: proto_event_target,
        node: proto_node,
        character_data: proto_character_data,
        document_type: proto_document_type,
        text: proto_text,
        comment: proto_comment,
        processing_instruction: proto_processing_instruction,
        element: proto_element,
        html_element: proto_html_element,
        html_media_element: proto_html_media_element,
        html_video_element: proto_html_video_element,
        html_audio_element: proto_html_audio_element,
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
        html_iframe_element: proto_html_iframe_element,
        document: proto_document,
        document_fragment: proto_document_fragment,
        shadow_root: proto_shadow_root,
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
    let mut prototype_roots: Vec<RootId> = Vec::with_capacity(27);

    // Prototype objects.
    let proto_event_target = scope.alloc_object()?;
    prototype_roots.push(
      scope
        .heap_mut()
        .add_root(Value::Object(proto_event_target))?,
    );
    let proto_node = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_node))?);
    let proto_character_data = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_character_data))?);
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
    let proto_html_media_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_media_element))?);
    let proto_html_video_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_video_element))?);
    let proto_html_audio_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_audio_element))?);
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
    let proto_html_iframe_element = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_html_iframe_element))?);
    let proto_document = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_document))?);
    let proto_document_fragment = scope.alloc_object()?;
    prototype_roots.push(
      scope
        .heap_mut()
        .add_root(Value::Object(proto_document_fragment))?,
    );
    let proto_shadow_root = scope.alloc_object()?;
    prototype_roots.push(scope.heap_mut().add_root(Value::Object(proto_shadow_root))?);

    // WebIDL / WHATWG DOM inheritance chain:
    //   EventTarget -> Object
    //   Node -> EventTarget
    //   CharacterData -> Node
    //   DocumentType -> Node
    //   Text -> CharacterData
    //   Comment -> CharacterData
    //   ProcessingInstruction -> CharacterData
    //   Element -> Node
    //   HTMLElement -> Element
    //   HTMLMediaElement -> HTMLElement
    //   HTMLVideoElement -> HTMLMediaElement
    //   HTMLAudioElement -> HTMLMediaElement
    //   HTML*Element -> HTMLElement
    //   Document -> Node
    //   DocumentFragment -> Node
    //   ShadowRoot -> DocumentFragment
    scope.heap_mut().object_set_prototype(
      proto_event_target,
      Some(realm.intrinsics().object_prototype()),
    )?;
    scope
      .heap_mut()
      .object_set_prototype(proto_node, Some(proto_event_target))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_character_data, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_document_type, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_text, Some(proto_character_data))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_comment, Some(proto_character_data))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_processing_instruction, Some(proto_character_data))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_element, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_html_element, Some(proto_element))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_html_media_element, Some(proto_html_element))?;
    for proto in [proto_html_video_element, proto_html_audio_element] {
      scope
        .heap_mut()
        .object_set_prototype(proto, Some(proto_html_media_element))?;
    }
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
      proto_html_iframe_element,
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
    scope
      .heap_mut()
      .object_set_prototype(proto_shadow_root, Some(proto_document_fragment))?;

    Ok(Self {
      realm_id,
      prototypes: DomPlatformPrototypes {
        event_target: proto_event_target,
        node: proto_node,
        character_data: proto_character_data,
        document_type: proto_document_type,
        text: proto_text,
        comment: proto_comment,
        processing_instruction: proto_processing_instruction,
        element: proto_element,
        html_element: proto_html_element,
        html_media_element: proto_html_media_element,
        html_video_element: proto_html_video_element,
        html_audio_element: proto_html_audio_element,
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
        html_iframe_element: proto_html_iframe_element,
        document: proto_document,
        document_fragment: proto_document_fragment,
        shadow_root: proto_shadow_root,
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
  /// the host (e.g. `document.createElement(..)`) inherit from the same JS-visible prototype objects
  /// as WebIDL-generated constructors (crucial for correct `instanceof` behavior).
  pub fn new_from_global_prototypes(scope: &mut Scope<'_>, realm: &Realm) -> Result<Self, VmError> {
    let global = realm.global_object();
    let base = scope.heap().stack_root_len();

    // Adopt WebIDL-generated prototypes for the base interfaces we want to share between the
    // generated constructor objects and native wrappers.
    let proto_event_target = Self::lookup_global_interface_prototype(
      scope,
      global,
      "EventTarget",
      "DomPlatform::new_from_global_prototypes expected globalThis.EventTarget.prototype",
    )?;
    let proto_node = Self::lookup_global_interface_prototype(
      scope,
      global,
      "Node",
      "DomPlatform::new_from_global_prototypes expected globalThis.Node.prototype",
    )?;
    let proto_character_data = Self::lookup_global_interface_prototype(
      scope,
      global,
      "CharacterData",
      "DomPlatform::new_from_global_prototypes expected globalThis.CharacterData.prototype",
    )?;
    let proto_text = Self::lookup_global_interface_prototype(
      scope,
      global,
      "Text",
      "DomPlatform::new_from_global_prototypes expected globalThis.Text.prototype",
    )?;
    let proto_element = Self::lookup_global_interface_prototype(
      scope,
      global,
      "Element",
      "DomPlatform::new_from_global_prototypes expected globalThis.Element.prototype",
    )?;
    let proto_document = Self::lookup_global_interface_prototype(
      scope,
      global,
      "Document",
      "DomPlatform::new_from_global_prototypes expected globalThis.Document.prototype",
    )?;
    let proto_document_fragment = Self::lookup_global_interface_prototype(
      scope,
      global,
      "DocumentFragment",
      "DomPlatform::new_from_global_prototypes expected globalThis.DocumentFragment.prototype",
    )?;
    let proto_shadow_root = Self::lookup_global_interface_prototype(
      scope,
      global,
      "ShadowRoot",
      "DomPlatform::new_from_global_prototypes expected globalThis.ShadowRoot.prototype",
    )?;

    // Root adopted prototypes while we allocate the remaining ones (tight heap limits can trigger
    // GC during allocation).
    for proto in [
      proto_event_target,
      proto_node,
      proto_character_data,
      proto_text,
      proto_element,
      proto_document,
      proto_document_fragment,
      proto_shadow_root,
    ] {
      scope.push_root(Value::Object(proto))?;
    }

    // Allocate prototypes for interfaces that are still provided by handwritten bindings.
    let proto_document_type = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_document_type))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_document_type, Some(proto_node))?;

    let proto_comment = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_comment))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_comment, Some(proto_character_data))?;

    let proto_processing_instruction = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_processing_instruction))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_processing_instruction, Some(proto_character_data))?;

    let proto_html_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_element))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_html_element, Some(proto_element))?;

    let proto_html_media_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_media_element))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_html_media_element, Some(proto_html_element))?;

    let proto_html_video_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_video_element))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_html_video_element, Some(proto_html_media_element))?;

    let proto_html_audio_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_audio_element))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_html_audio_element, Some(proto_html_media_element))?;

    let proto_html_input_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_input_element))?;
    let proto_html_select_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_select_element))?;
    let proto_html_text_area_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_text_area_element))?;
    let proto_html_option_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_option_element))?;
    let proto_html_form_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_form_element))?;
    let proto_html_div_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_div_element))?;
    let proto_html_span_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_span_element))?;
    let proto_html_paragraph_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_paragraph_element))?;
    let proto_html_anchor_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_anchor_element))?;
    let proto_html_image_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_image_element))?;
    let proto_html_link_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_link_element))?;
    let proto_html_script_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_script_element))?;
    let proto_html_iframe_element = scope.alloc_object()?;
    scope.push_root(Value::Object(proto_html_iframe_element))?;

    for proto in [
      proto_html_media_element,
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
      proto_html_iframe_element,
    ] {
      scope
        .heap_mut()
        .object_set_prototype(proto, Some(proto_html_element))?;
    }

    // Ensure adopted base prototypes follow the expected inheritance chain in case the generated
    // installers were invoked out-of-order.
    scope
      .heap_mut()
      .object_set_prototype(proto_node, Some(proto_event_target))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_character_data, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_text, Some(proto_character_data))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_element, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_document, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_document_fragment, Some(proto_node))?;
    scope
      .heap_mut()
      .object_set_prototype(proto_shadow_root, Some(proto_document_fragment))?;

    let result = Self::new_with_prototypes(
      scope,
      realm,
      DomPlatformPrototypes {
        event_target: proto_event_target,
        node: proto_node,
        character_data: proto_character_data,
        document_type: proto_document_type,
        text: proto_text,
        comment: proto_comment,
        processing_instruction: proto_processing_instruction,
        element: proto_element,
        html_element: proto_html_element,
        html_media_element: proto_html_media_element,
        html_video_element: proto_html_video_element,
        html_audio_element: proto_html_audio_element,
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
        html_iframe_element: proto_html_iframe_element,
        document: proto_document,
        document_fragment: proto_document_fragment,
        shadow_root: proto_shadow_root,
      },
    );
    scope.heap_mut().truncate_stack_roots(base);
    result
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
      DomInterface::CharacterData => self.prototypes.character_data,
      DomInterface::DocumentType => self.prototypes.document_type,
      DomInterface::Text => self.prototypes.text,
      DomInterface::Comment => self.prototypes.comment,
      DomInterface::ProcessingInstruction => self.prototypes.processing_instruction,
      DomInterface::Element => self.prototypes.element,
      DomInterface::HTMLElement => self.prototypes.html_element,
      DomInterface::HTMLMediaElement => self.prototypes.html_media_element,
      DomInterface::HTMLVideoElement => self.prototypes.html_video_element,
      DomInterface::HTMLAudioElement => self.prototypes.html_audio_element,
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
      DomInterface::HTMLIFrameElement => self.prototypes.html_iframe_element,
      DomInterface::Document => self.prototypes.document,
      DomInterface::DocumentFragment => self.prototypes.document_fragment,
      DomInterface::ShadowRoot => self.prototypes.shadow_root,
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

  fn register_wrapper_for_document_id(
    &mut self,
    heap: &Heap,
    wrapper: GcObject,
    document_id: DocumentId,
    node_id: NodeId,
    primary_interface: DomInterface,
  ) {
    self.sweep_dead_wrappers_if_needed(heap);
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

  pub fn register_wrapper(
    &mut self,
    heap: &Heap,
    wrapper: GcObject,
    document_key: WeakGcObject,
    node_id: NodeId,
    primary_interface: DomInterface,
  ) {
    let document_id = document_id_from_key(document_key);
    self.register_wrapper_for_document_id(heap, wrapper, document_id, node_id, primary_interface);
  }

  /// Register a JS-created alias `Document` wrapper (e.g. `Object.create(document)`).
  ///
  /// Some WPT tests intentionally fabricate a second `Document` wrapper identity using
  /// `Object.create(document)` to exercise cross-document wrapper adoption logic without a true
  /// multi-document `dom2::Document` backend.
  ///
  /// The alias object:
  /// - *must not* allocate a fresh wrapper; the alias object itself becomes the platform wrapper for
  ///   its derived `DocumentId`,
  /// - is registered only when it inherits from an already-registered `Document` wrapper, and
  /// - is mutated to match the internal wrapper shape expected by older shims
  ///   (`__fastrender_node_id`, `__fastrender_wrapper_document`, `HostSlots` brand).
  pub fn maybe_register_document_alias_wrapper(
    &mut self,
    scope: &mut Scope<'_>,
    alias_obj: GcObject,
  ) -> Result<(), VmError> {
    self.sweep_dead_wrappers_if_needed(scope.heap());

    // Fast path: already registered.
    let weak = WeakGcObject::from(alias_obj);
    if self.meta_by_wrapper.contains_key(&weak) {
      return Ok(());
    }

    // Only treat objects as `Document` aliases if their prototype chain includes a registered
    // `Document` wrapper.
    let mut proto = scope.object_get_prototype(alias_obj)?;
    let mut proto_document_obj: Option<GcObject> = None;
    while let Some(proto_obj) = proto {
      if let Some(meta) = self.meta_by_wrapper.get(&WeakGcObject::from(proto_obj)) {
        if meta.primary_interface.implements(DomInterface::Document) {
          proto_document_obj = Some(proto_obj);
          break;
        }
      }
      proto = scope.object_get_prototype(proto_obj)?;
    }
    let Some(proto_document_obj) = proto_document_obj else {
      return Ok(());
    };

    // Mutate the alias object to look like a platform wrapper: allocation can trigger GC, so root
    // it (and any temporary strings) via a nested scope.
    {
      let mut scope = scope.reborrow();
      scope.push_root(Value::Object(alias_obj))?;
      scope.push_root(Value::Object(proto_document_obj))?;

      // Brand for structuredClone() / host-side wrapper detection.
      scope.heap_mut().object_set_host_slots(
        alias_obj,
        HostSlots {
          a: DOM_WRAPPER_HOST_TAG,
          b: 0,
        },
      )?;

      // __fastrender_node_id = 0
      let node_id_s = scope.alloc_string(NODE_ID_KEY)?;
      scope.push_root(Value::String(node_id_s))?;
      let node_id_key = PropertyKey::from_string(node_id_s);
      scope.define_property(
        alias_obj,
        node_id_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Number(0.0),
            writable: true,
          },
        },
      )?;

      // __fastrender_wrapper_document = alias_obj
      let wrapper_document_s = scope.alloc_string(WRAPPER_DOCUMENT_KEY)?;
      scope.push_root(Value::String(wrapper_document_s))?;
      let wrapper_document_key = PropertyKey::from_string(wrapper_document_s);
      scope.define_property(
        alias_obj,
        wrapper_document_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Object(alias_obj),
            writable: true,
          },
        },
      )?;

      // Copy internal per-document shims stored as own-properties on the canonical Document wrapper.
      //
      // Many WebIDL dispatch paths use `object_get_own_data_property_value(document_obj, ...)` to
      // retrieve these prototypes/functions. `Object.create(document)` would otherwise inherit them
      // and fail those own-property lookups.
      for key_str in [
        NODE_LIST_PROTOTYPE_KEY,
        HTML_COLLECTION_PROTOTYPE_KEY,
        CSS_STYLE_DECL_PROTOTYPE_KEY,
        STYLE_GET_PROPERTY_VALUE_KEY,
        STYLE_SET_PROPERTY_KEY,
        STYLE_REMOVE_PROPERTY_KEY,
        STYLE_CSS_TEXT_GET_KEY,
        STYLE_CSS_TEXT_SET_KEY,
        STYLE_DISPLAY_GET_KEY,
        STYLE_DISPLAY_SET_KEY,
        STYLE_CURSOR_GET_KEY,
        STYLE_CURSOR_SET_KEY,
        STYLE_HEIGHT_GET_KEY,
        STYLE_HEIGHT_SET_KEY,
        STYLE_WIDTH_GET_KEY,
        STYLE_WIDTH_SET_KEY,
      ] {
        let key_s = scope.alloc_string(key_str)?;
        scope.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);
        let Some(value) = scope
          .heap()
          .object_get_own_data_property_value(proto_document_obj, &key)?
        else {
          continue;
        };
        scope.define_property(
          alias_obj,
          key,
          PropertyDescriptor {
            enumerable: false,
            configurable: false,
            kind: PropertyKind::Data {
              value,
              writable: false,
            },
          },
        )?;
      }
    }

    // Register alias wrapper metadata + node wrapper cache entry for the document node (`NodeId(0)`).
    let document_id = document_id_from_key(weak);
    let node_id = NodeId::from_index(0);
    self.wrappers_by_node.insert(DomNodeKey::new(document_id, node_id), weak);
    self.meta_by_wrapper.insert(
      weak,
      DomWrapperMeta {
        document_id,
        node_id,
        primary_interface: DomInterface::Document,
        realm_id: self.realm_id,
      },
    );
    Ok(())
  }

  /// Return an existing wrapper for `node_id` if still alive.
  pub fn get_existing_wrapper(
    &mut self,
    heap: &Heap,
    document_key: WeakGcObject,
    node_id: NodeId,
  ) -> Option<GcObject> {
    self.get_existing_wrapper_for_document_id(heap, document_id_from_key(document_key), node_id)
  }

  pub fn get_existing_wrapper_for_document_id(
    &mut self,
    heap: &Heap,
    document_id: DocumentId,
    node_id: NodeId,
  ) -> Option<GcObject> {
    self.sweep_dead_wrappers_if_needed(heap);
    let key = DomNodeKey::new(document_id, node_id);
    self
      .wrappers_by_node
      .get(&key)
      .copied()
      .and_then(|weak| weak.upgrade(heap))
  }

  fn get_existing_wrapper_for_node_key(&mut self, heap: &Heap, key: DomNodeKey) -> Option<GcObject> {
    self.sweep_dead_wrappers_if_needed(heap);
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
    self.get_or_create_wrapper_for_document_id(
      scope,
      document_id_from_key(document_key),
      node_id,
      primary_interface,
    )
  }

  pub fn get_or_create_wrapper_for_document_id(
    &mut self,
    scope: &mut Scope<'_>,
    document_id: DocumentId,
    node_id: NodeId,
    primary_interface: DomInterface,
  ) -> Result<GcObject, VmError> {
    if let Some(existing) =
      self.get_existing_wrapper_for_document_id(scope.heap(), document_id, node_id)
    {
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

      let document_wrapper_obj = if node_id.index() != 0 {
        Some(
          self
            .get_existing_wrapper_for_node_key(
              scope.heap(),
              DomNodeKey::new(document_id, NodeId::from_index(0)),
            )
            .ok_or(VmError::InvariantViolation(
              "missing wrapper for document node",
            ))?,
        )
      } else {
        None
      };
      if let Some(document_wrapper_obj) = document_wrapper_obj {
        // Root the document wrapper across string allocations.
        scope.push_root(Value::Object(document_wrapper_obj))?;
      }

      let node_id_key = PropertyKey::from_string(scope.alloc_string(NODE_ID_KEY)?);
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

      // Some handwritten DOM shims still rely on wrappers having an own data property pointing at
      // the owning `Document` wrapper. Ensure WebIDL-created wrappers provide it too.
      if let Some(document_wrapper_obj) = document_wrapper_obj {
        let wrapper_document_key =
          PropertyKey::from_string(scope.alloc_string(WRAPPER_DOCUMENT_KEY)?);
        scope.define_property(
          wrapper,
          wrapper_document_key,
          PropertyDescriptor {
            enumerable: false,
            configurable: false,
            kind: PropertyKind::Data {
              value: Value::Object(document_wrapper_obj),
              writable: true,
            },
          },
        )?;
      }
    }

    // Keep the wrapper's owning document reference in sync with the handwritten vm-js DOM helpers
    // (`window_realm.rs`), which identify nodes by `(wrapper_document, __fastrender_node_id)`.
    //
    // This property is intentionally *not* used by the WebIDL host dispatch (it uses the
    // `meta_by_wrapper` table instead), but it enables transitional fallback shims and older native
    // helpers to work against platform-object wrappers created via `DomPlatform`.
    {
      // Root wrapper while allocating strings / potentially allocating the document wrapper.
      let mut scope = scope.reborrow();
      scope.push_root(Value::Object(wrapper))?;

      // Ensure a stable document wrapper exists for this document ID so we can stash it.
      let document_obj = if node_id.index() == 0 {
        wrapper
      } else {
        self.get_or_create_wrapper_for_document_id(
          &mut scope,
          document_id,
          NodeId::from_index(0),
          DomInterface::Document,
        )?
      };
      scope.push_root(Value::Object(document_obj))?;

      let wrapper_document_key =
        PropertyKey::from_string(scope.alloc_string(WRAPPER_DOCUMENT_KEY)?);
      scope.define_property(
        wrapper,
        wrapper_document_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Object(document_obj),
            // Allow document rebinding for clone+mapping operations (e.g. adoptNode-style moves).
            writable: true,
          },
        },
      )?;
    }

    self.register_wrapper_for_document_id(
      scope.heap(),
      wrapper,
      document_id,
      node_id,
      primary_interface,
    );
    Ok(wrapper)
  }

  /// Update the primary interface (brand) for an existing wrapper object.
  ///
  /// This is primarily used when a wrapper was created without access to the backing `dom2::Document`
  /// (so its node kind could not be inspected) and needs to be upgraded to a more specific
  /// interface like `DocumentType`.
  pub fn rebrand_wrapper(
    &mut self,
    scope: &mut Scope<'_>,
    wrapper: GcObject,
    primary_interface: DomInterface,
  ) -> Result<(), VmError> {
    self.sweep_dead_wrappers_if_needed(scope.heap());

    let weak = WeakGcObject::from(wrapper);
    let meta = self
      .meta_by_wrapper
      .get_mut(&weak)
      .ok_or(VmError::TypeError("Illegal invocation"))?;
    if meta.realm_id != self.realm_id {
      return Err(VmError::TypeError("Illegal invocation"));
    }

    meta.primary_interface = primary_interface;
    scope.heap_mut().object_set_prototype(
      wrapper,
      Some(self.prototype_for(primary_interface)),
    )?;
    Ok(())
  }

  fn rebind_wrapper_impl(
    &mut self,
    heap: &mut Heap,
    node_id_key: &PropertyKey,
    wrapper_document_key: &PropertyKey,
    style_key: &PropertyKey,
    new_document_obj: Option<GcObject>,
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

    // Keep the wrapper's document back-reference in sync when moving between documents.
    if old.document_id != new.document_id {
      let Some(new_document_obj) = new_document_obj else {
        return Err(VmError::InvariantViolation(
          "missing destination document object for wrapper rebinding",
        ));
      };
      match heap.object_set_existing_data_property_value(
        wrapper,
        wrapper_document_key,
        Value::Object(new_document_obj),
      ) {
        Ok(()) => {}
        Err(VmError::PropertyNotFound | VmError::PropertyNotData) => {
          // Some wrappers (e.g. those constructed directly in unit tests) may not have the property
          // yet. Define it eagerly so future native calls can rely on its presence.
          let mut scope = heap.scope();
          scope.push_root(Value::Object(wrapper))?;
          match *wrapper_document_key {
            PropertyKey::String(s) => scope.push_root(Value::String(s))?,
            PropertyKey::Symbol(s) => scope.push_root(Value::Symbol(s))?,
          };
          scope.push_root(Value::Object(new_document_obj))?;
          scope.define_property(
            wrapper,
            *wrapper_document_key,
            PropertyDescriptor {
              enumerable: false,
              configurable: false,
              kind: PropertyKind::Data {
                value: Value::Object(new_document_obj),
                writable: true,
              },
            },
          )?;
        }
        Err(err) => return Err(err),
      }
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
        scope.push_root(Value::Object(wrapper))?;
        match *node_id_key {
          PropertyKey::String(s) => scope.push_root(Value::String(s))?,
          PropertyKey::Symbol(s) => scope.push_root(Value::Symbol(s))?,
        };
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

    // Keep any cached Element.style object (CSSStyleDeclaration-like shim) in sync.
    //
    // WebIDL host dispatch may cache `el.style` as an own data property on the element wrapper.
    // When wrapper identity is preserved across clone+mapping operations, the wrapper's own node id
    // updates are not sufficient: the nested style object can still point at the old node id.
    //
    // Best-effort: scripts can overwrite `el.style`; only touch objects that look like our shim.
    if let Ok(Some(Value::Object(style_obj))) =
      heap.object_get_own_data_property_value(wrapper, style_key)
    {
      if let Ok(Some(slots)) = heap.object_host_slots(style_obj) {
        if slots.b == CSS_STYLE_DECL_HOST_TAG {
          // Update host slots used by `VmHostHooks::host_exotic_*`.
          let _ = heap.object_set_host_slots(
            style_obj,
            HostSlots {
              a: new.node_id.index() as u64,
              b: CSS_STYLE_DECL_HOST_TAG,
            },
          );

          // Update `__fastrender_node_id` on the style object so native shims that read it directly
          // continue to work.
          match heap.object_set_existing_data_property_value(
            style_obj,
            node_id_key,
            Value::Number(new.node_id.index() as f64),
          ) {
            Ok(()) => {}
            Err(VmError::PropertyNotFound | VmError::PropertyNotData) => {
              let mut scope = heap.scope();
              scope.push_root(Value::Object(style_obj))?;
              match *node_id_key {
                PropertyKey::String(s) => scope.push_root(Value::String(s))?,
                PropertyKey::Symbol(s) => scope.push_root(Value::Symbol(s))?,
              };
              scope.define_property(
                style_obj,
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
            Err(_) => {
              // Ignore unexpected errors to avoid breaking remap operations when user code tampers
              // with `el.style`.
            }
          }

          // Keep the style object's document back-reference in sync when moving between documents.
          if old.document_id != new.document_id {
            if let Some(new_document_obj) = new_document_obj {
              match heap.object_set_existing_data_property_value(
                style_obj,
                wrapper_document_key,
                Value::Object(new_document_obj),
              ) {
                Ok(()) => {}
                Err(VmError::PropertyNotFound | VmError::PropertyNotData) => {
                  let mut scope = heap.scope();
                  scope.push_root(Value::Object(style_obj))?;
                  match *wrapper_document_key {
                    PropertyKey::String(s) => scope.push_root(Value::String(s))?,
                    PropertyKey::Symbol(s) => scope.push_root(Value::Symbol(s))?,
                  };
                  scope.push_root(Value::Object(new_document_obj))?;
                  scope.define_property(
                    style_obj,
                    *wrapper_document_key,
                    PropertyDescriptor {
                      enumerable: false,
                      configurable: false,
                      kind: PropertyKind::Data {
                        value: Value::Object(new_document_obj),
                        writable: true,
                      },
                    },
                  )?;
                }
                Err(_) => {
                  // Ignore unexpected errors to avoid breaking remap operations when user code
                  // tampers with `el.style`.
                }
              }
            }
          }
        }
      }
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
    // Allocate the property keys once. `PropertyKey` string comparisons are by content, so it will
    // match existing keys even if wrappers were created using a different `GcString` handle.
    //
    // Root the strings for the duration of the operation so GC during subsequent allocations can't
    // collect them (these keys are not stored on any object graph).
    let mut scope = heap.scope();
    let node_id_s = scope.alloc_string(NODE_ID_KEY)?;
    scope.push_root(Value::String(node_id_s))?;
    let node_id_key = PropertyKey::from_string(node_id_s);
    let wrapper_document_s = scope.alloc_string(WRAPPER_DOCUMENT_KEY)?;
    scope.push_root(Value::String(wrapper_document_s))?;
    let wrapper_document_key = PropertyKey::from_string(wrapper_document_s);
    let style_s = scope.alloc_string("style")?;
    scope.push_root(Value::String(style_s))?;
    let style_key = PropertyKey::from_string(style_s);

    let new_document_obj = if old.document_id != new.document_id {
      Some(
        self
          .get_existing_wrapper_for_node_key(
            scope.heap(),
            DomNodeKey::new(new.document_id, NodeId::from_index(0)),
          )
          .ok_or(VmError::InvariantViolation(
            "missing wrapper for destination document node",
          ))?,
      )
    } else {
      None
    };
    self.rebind_wrapper_impl(
      scope.heap_mut(),
      &node_id_key,
      &wrapper_document_key,
      &style_key,
      new_document_obj,
      old,
      new,
    )
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

    // Root strings for the duration of the remap: they are not stored on the object graph.
    let mut scope = heap.scope();
    let node_id_s = scope.alloc_string(NODE_ID_KEY)?;
    scope.push_root(Value::String(node_id_s))?;
    let node_id_key = PropertyKey::from_string(node_id_s);
    let wrapper_document_s = scope.alloc_string(WRAPPER_DOCUMENT_KEY)?;
    scope.push_root(Value::String(wrapper_document_s))?;
    let wrapper_document_key = PropertyKey::from_string(wrapper_document_s);
    let style_s = scope.alloc_string("style")?;
    scope.push_root(Value::String(style_s))?;
    let style_key = PropertyKey::from_string(style_s);
    for (&old_id, &new_id) in mapping {
      self.rebind_wrapper_impl(
        scope.heap_mut(),
        &node_id_key,
        &wrapper_document_key,
        &style_key,
        None,
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

    // Root strings for the duration of the remap: they are not stored on the object graph.
    let mut scope = heap.scope();
    let node_id_s = scope.alloc_string(NODE_ID_KEY)?;
    scope.push_root(Value::String(node_id_s))?;
    let node_id_key = PropertyKey::from_string(node_id_s);
    let wrapper_document_s = scope.alloc_string(WRAPPER_DOCUMENT_KEY)?;
    scope.push_root(Value::String(wrapper_document_s))?;
    let wrapper_document_key = PropertyKey::from_string(wrapper_document_s);
    let style_s = scope.alloc_string("style")?;
    scope.push_root(Value::String(style_s))?;
    let style_key = PropertyKey::from_string(style_s);

    let new_document_obj = self
      .get_existing_wrapper_for_node_key(
        scope.heap(),
        DomNodeKey::new(new_document_id, NodeId::from_index(0)),
      )
      .ok_or(VmError::InvariantViolation(
        "missing wrapper for destination document node",
      ))?;
    for (&old_id, &new_id) in mapping {
      self.rebind_wrapper_impl(
        scope.heap_mut(),
        &node_id_key,
        &wrapper_document_key,
        &style_key,
        Some(new_document_obj),
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

  pub fn require_html_text_area_element_handle(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<DomNodeKey, VmError> {
    let meta = self.require_wrapper_meta(heap, value)?;
    if !meta
      .primary_interface
      .implements(DomInterface::HTMLTextAreaElement)
    {
      return Err(VmError::TypeError("Illegal invocation"));
    }
    Ok(DomNodeKey::new(meta.document_id, meta.node_id))
  }

  pub fn require_html_media_element_handle(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<DomNodeKey, VmError> {
    self.require_interface_node_handle(heap, value, DomInterface::HTMLMediaElement)
  }

  pub fn require_html_video_element_handle(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<DomNodeKey, VmError> {
    self.require_interface_node_handle(heap, value, DomInterface::HTMLVideoElement)
  }

  pub fn require_html_audio_element_handle(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<DomNodeKey, VmError> {
    self.require_interface_node_handle(heap, value, DomInterface::HTMLAudioElement)
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

  pub fn require_html_text_area_element_id(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<NodeId, VmError> {
    Ok(self
      .require_html_text_area_element_handle(heap, value)?
      .node_id)
  }

  pub fn require_html_media_element_id(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<NodeId, VmError> {
    self.require_interface_node_id(heap, value, DomInterface::HTMLMediaElement)
  }

  pub fn require_html_video_element_id(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<NodeId, VmError> {
    self.require_interface_node_id(heap, value, DomInterface::HTMLVideoElement)
  }

  pub fn require_html_audio_element_id(
    &mut self,
    heap: &Heap,
    value: Value,
  ) -> Result<NodeId, VmError> {
    self.require_interface_node_id(heap, value, DomInterface::HTMLAudioElement)
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
    GcObject, Heap, HeapLimits, HostSlots, PropertyDescriptor, PropertyKey, PropertyKind, Realm,
    Value, Vm, VmError, VmOptions, WeakGcObject,
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
    // CharacterData sits between Node and Text/Comment/ProcessingInstruction.
    let character_data_proto = install_stub_interface(scope, global, "CharacterData", node_proto)?;

    // Interfaces inheriting from Node.
    for name in ["DocumentType", "Document", "DocumentFragment"] {
      let _ = install_stub_interface(scope, global, name, node_proto)?;
    }

    // Interfaces inheriting from CharacterData.
    for name in ["Text", "Comment", "ProcessingInstruction"] {
      let _ = install_stub_interface(scope, global, name, character_data_proto)?;
    }

    // ShadowRoot inherits from DocumentFragment.
    let document_fragment_proto = get_global_interface_prototype(scope, global, "DocumentFragment")?;
    let _ = install_stub_interface(scope, global, "ShadowRoot", document_fragment_proto)?;

    // Element + HTMLElement + HTML*Element chain.
    let element_proto = install_stub_interface(scope, global, "Element", node_proto)?;
    let html_element_proto = install_stub_interface(scope, global, "HTMLElement", element_proto)?;
    // HTMLMediaElement inherits from HTMLElement, and video/audio inherit from HTMLMediaElement.
    let html_media_element_proto =
      install_stub_interface(scope, global, "HTMLMediaElement", html_element_proto)?;
    for name in ["HTMLVideoElement", "HTMLAudioElement"] {
      let _ = install_stub_interface(scope, global, name, html_media_element_proto)?;
    }
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
    platform.register_wrapper(
      scope.heap(),
      document_obj,
      document_key,
      NodeId::from_index(0),
      DomInterface::Document,
    );

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
    platform.register_wrapper(
      scope.heap(),
      doc_a,
      doc_key_a,
      NodeId::from_index(0),
      DomInterface::Document,
    );
    platform.register_wrapper(
      scope.heap(),
      doc_b,
      doc_key_b,
      NodeId::from_index(0),
      DomInterface::Document,
    );

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
    platform.register_wrapper(
      scope.heap(),
      document_obj,
      document_key,
      NodeId::from_index(0),
      DomInterface::Document,
    );

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
    platform.register_wrapper(
      scope.heap(),
      document_obj,
      document_key,
      NodeId::from_index(0),
      DomInterface::Document,
    );
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
  fn media_element_brand_checks_allow_video_and_audio() -> Result<(), VmError> {
    let mut runtime = make_runtime()?;
    let (realm, heap) = split_runtime_realm(&mut runtime);
    let mut scope = heap.scope();
    let mut platform = DomPlatform::new(&mut scope, realm)?;

    let document_obj = scope.alloc_object()?;
    let document_key = WeakGcObject::from(document_obj);
    let _doc_root = scope.heap_mut().add_root(Value::Object(document_obj))?;
    platform.register_wrapper(
      scope.heap(),
      document_obj,
      document_key,
      NodeId::from_index(0),
      DomInterface::Document,
    );
    let document_id = super::document_id_from_key(document_key);

    // HTMLVideoElement implements HTMLMediaElement.
    let video_id = NodeId::from_index(1);
    let video_key = DomNodeKey::new(document_id, video_id);
    let video_wrapper = platform.get_or_create_wrapper(
      &mut scope,
      document_key,
      video_id,
      DomInterface::HTMLVideoElement,
    )?;
    let _video_root = scope.heap_mut().add_root(Value::Object(video_wrapper))?;

    assert_eq!(
      platform.require_html_media_element_handle(scope.heap(), Value::Object(video_wrapper))?,
      video_key
    );
    assert_eq!(
      platform.require_html_video_element_handle(scope.heap(), Value::Object(video_wrapper))?,
      video_key
    );
    assert_eq!(
      platform.require_html_media_element_id(scope.heap(), Value::Object(video_wrapper))?,
      video_id
    );
    assert_eq!(
      platform.require_html_video_element_id(scope.heap(), Value::Object(video_wrapper))?,
      video_id
    );
    let err = platform.require_html_audio_element_handle(scope.heap(), Value::Object(video_wrapper));
    assert!(matches!(err, Err(VmError::TypeError("Illegal invocation"))));

    // HTMLAudioElement implements HTMLMediaElement.
    let audio_id = NodeId::from_index(2);
    let audio_key = DomNodeKey::new(document_id, audio_id);
    let audio_wrapper = platform.get_or_create_wrapper(
      &mut scope,
      document_key,
      audio_id,
      DomInterface::HTMLAudioElement,
    )?;
    let _audio_root = scope.heap_mut().add_root(Value::Object(audio_wrapper))?;

    assert_eq!(
      platform.require_html_media_element_handle(scope.heap(), Value::Object(audio_wrapper))?,
      audio_key
    );
    assert_eq!(
      platform.require_html_audio_element_handle(scope.heap(), Value::Object(audio_wrapper))?,
      audio_key
    );
    assert_eq!(
      platform.require_html_media_element_id(scope.heap(), Value::Object(audio_wrapper))?,
      audio_id
    );
    assert_eq!(
      platform.require_html_audio_element_id(scope.heap(), Value::Object(audio_wrapper))?,
      audio_id
    );
    let err = platform.require_html_video_element_handle(scope.heap(), Value::Object(audio_wrapper));
    assert!(matches!(err, Err(VmError::TypeError("Illegal invocation"))));

    // Non-media elements fail the HTMLMediaElement check.
    let div_wrapper = platform.get_or_create_wrapper(
      &mut scope,
      document_key,
      NodeId::from_index(3),
      DomInterface::HTMLDivElement,
    )?;
    let _div_root = scope.heap_mut().add_root(Value::Object(div_wrapper))?;
    let err = platform.require_html_media_element_handle(scope.heap(), Value::Object(div_wrapper));
    assert!(matches!(err, Err(VmError::TypeError("Illegal invocation"))));

    // Non-objects and non-wrapper objects should also fail brand checks.
    let err = platform.require_html_media_element_handle(scope.heap(), Value::Undefined);
    assert!(matches!(err, Err(VmError::TypeError("Illegal invocation"))));

    let obj = scope.alloc_object()?;
    let err = platform.require_html_media_element_handle(scope.heap(), Value::Object(obj));
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
    platform.register_wrapper(
      scope.heap(),
      document_obj,
      document_key,
      NodeId::from_index(0),
      DomInterface::Document,
    );
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

    let key = PropertyKey::from_string(scope.alloc_string(super::NODE_ID_KEY)?);
    let value = scope
      .heap()
      .object_get_own_data_property_value(wrapper, &key)?
      .unwrap_or(Value::Undefined);
    assert_eq!(value, Value::Number(new_id.index() as f64));

    scope.heap_mut().remove_root(root);
    Ok(())
  }

  #[test]
  fn remap_preserves_wrapper_identity_between_documents() -> Result<(), VmError> {
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

    // Register document-node wrappers so `remap_node_ids_between_documents` can resolve the target
    // document object when updating `__fastrender_wrapper_document`.
    platform.register_wrapper(
      scope.heap(),
      document_a,
      document_key_a,
      NodeId::from_index(0),
      DomInterface::Document,
    );
    platform.register_wrapper(
      scope.heap(),
      document_b,
      document_key_b,
      NodeId::from_index(0),
      DomInterface::Document,
    );

    let old_id = NodeId::from_index(5);
    let wrapper =
      platform.get_or_create_wrapper(&mut scope, document_key_a, old_id, DomInterface::Element)?;
    let root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    let wrapper_doc_key =
      PropertyKey::from_string(scope.alloc_string(super::WRAPPER_DOCUMENT_KEY)?);
    let wrapper_doc_value = scope
      .heap()
      .object_get_own_data_property_value(wrapper, &wrapper_doc_key)?
      .unwrap_or(Value::Undefined);
    assert_eq!(wrapper_doc_value, Value::Object(document_a));

    let new_id = NodeId::from_index(9);
    let mut mapping: HashMap<NodeId, NodeId> = HashMap::new();
    mapping.insert(old_id, new_id);
    platform.remap_node_ids_between_documents(scope.heap_mut(), document_id_a, document_id_b, &mapping)?;

    let wrapper2 =
      platform.get_or_create_wrapper(&mut scope, document_key_b, new_id, DomInterface::Element)?;
    assert_eq!(wrapper, wrapper2);

    let key = PropertyKey::from_string(scope.alloc_string(super::NODE_ID_KEY)?);
    let value = scope
      .heap()
      .object_get_own_data_property_value(wrapper, &key)?
      .unwrap_or(Value::Undefined);
    assert_eq!(value, Value::Number(new_id.index() as f64));

    let wrapper_doc_key =
      PropertyKey::from_string(scope.alloc_string(super::WRAPPER_DOCUMENT_KEY)?);
    let wrapper_doc_value = scope
      .heap()
      .object_get_own_data_property_value(wrapper, &wrapper_doc_key)?
      .unwrap_or(Value::Undefined);
    assert_eq!(wrapper_doc_value, Value::Object(document_b));

    scope.heap_mut().remove_root(root);
    Ok(())
  }

  #[test]
  fn remap_updates_cached_style_object_between_documents() -> Result<(), VmError> {
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

    // Register document-node wrappers so `remap_node_ids_between_documents` can resolve the target
    // document object when updating `__fastrender_wrapper_document`.
    platform.register_wrapper(
      scope.heap(),
      document_a,
      document_key_a,
      NodeId::from_index(0),
      DomInterface::Document,
    );
    platform.register_wrapper(
      scope.heap(),
      document_b,
      document_key_b,
      NodeId::from_index(0),
      DomInterface::Document,
    );

    let old_id = NodeId::from_index(5);
    let wrapper =
      platform.get_or_create_wrapper(&mut scope, document_key_a, old_id, DomInterface::Element)?;
    let _wrapper_root = scope.heap_mut().add_root(Value::Object(wrapper))?;

    // Mirror real `window_realm` wrappers by associating the wrapper with its originating document.
    {
      let mut scope = scope.reborrow();
      scope.push_root(Value::Object(wrapper))?;
      let key = alloc_key(&mut scope, super::WRAPPER_DOCUMENT_KEY)?;
      scope.define_property(
        wrapper,
        key,
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Object(document_a),
            writable: true,
          },
        },
      )?;
    }

    // Create and cache a CSSStyleDeclaration-like shim on the element wrapper.
    let style_obj = scope.alloc_object()?;
    {
      let mut scope = scope.reborrow();
      scope.push_root(Value::Object(wrapper))?;
      scope.push_root(Value::Object(style_obj))?;

      scope.heap_mut().object_set_host_slots(
        style_obj,
        HostSlots {
          a: old_id.index() as u64,
          b: super::CSS_STYLE_DECL_HOST_TAG,
        },
      )?;

      let node_id_key = alloc_key(&mut scope, super::NODE_ID_KEY)?;
      scope.define_property(
        style_obj,
        node_id_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Number(old_id.index() as f64),
            writable: true,
          },
        },
      )?;

      let wrapper_document_key = alloc_key(&mut scope, super::WRAPPER_DOCUMENT_KEY)?;
      scope.define_property(
        style_obj,
        wrapper_document_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Object(document_a),
            writable: false,
          },
        },
      )?;

      let style_key = alloc_key(&mut scope, "style")?;
      scope.define_property(
        wrapper,
        style_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(style_obj),
            writable: true,
          },
        },
      )?;
    }

    let new_id = NodeId::from_index(9);
    let mut mapping: HashMap<NodeId, NodeId> = HashMap::new();
    mapping.insert(old_id, new_id);
    platform.remap_node_ids_between_documents(
      scope.heap_mut(),
      document_id_a,
      document_id_b,
      &mapping,
    )?;

    // Element wrapper identity is preserved and the cached style object stays attached.
    let wrapper2 =
      platform.get_or_create_wrapper(&mut scope, document_key_b, new_id, DomInterface::Element)?;
    assert_eq!(wrapper, wrapper2);
    let style_key = PropertyKey::from_string(scope.alloc_string("style")?);
    let style_val = scope
      .heap()
      .object_get_own_data_property_value(wrapper, &style_key)?
      .unwrap_or(Value::Undefined);
    let Value::Object(style_obj2) = style_val else {
      return Err(VmError::InvariantViolation("expected element wrapper to have cached style object"));
    };
    assert_eq!(style_obj2, style_obj);

    // The style object now points at the new node id + destination document.
    let slots = scope
      .heap()
      .object_host_slots(style_obj2)?
      .ok_or(VmError::InvariantViolation("expected style object to have host slots"))?;
    assert_eq!(slots.b, super::CSS_STYLE_DECL_HOST_TAG);
    assert_eq!(slots.a, new_id.index() as u64);

    let node_id_key = PropertyKey::from_string(scope.alloc_string(super::NODE_ID_KEY)?);
    let node_id_val = scope
      .heap()
      .object_get_own_data_property_value(style_obj2, &node_id_key)?
      .unwrap_or(Value::Undefined);
    assert_eq!(node_id_val, Value::Number(new_id.index() as f64));

    let wrapper_document_key =
      PropertyKey::from_string(scope.alloc_string(super::WRAPPER_DOCUMENT_KEY)?);
    let wrapper_document_val = scope
      .heap()
      .object_get_own_data_property_value(style_obj2, &wrapper_document_key)?
      .unwrap_or(Value::Undefined);
    assert_eq!(wrapper_document_val, Value::Object(document_b));

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
    let html_media_proto = platform.prototype_for(DomInterface::HTMLMediaElement);
    let html_video_proto = platform.prototype_for(DomInterface::HTMLVideoElement);
    let html_audio_proto = platform.prototype_for(DomInterface::HTMLAudioElement);
    let html_input_proto = platform.prototype_for(DomInterface::HTMLInputElement);
    let html_script_proto = platform.prototype_for(DomInterface::HTMLScriptElement);

    assert_eq!(
      scope.heap().object_prototype(html_element_proto)?,
      Some(element_proto)
    );
    assert_eq!(
      scope.heap().object_prototype(html_media_proto)?,
      Some(html_element_proto)
    );
    assert_eq!(
      scope.heap().object_prototype(html_video_proto)?,
      Some(html_media_proto)
    );
    assert_eq!(
      scope.heap().object_prototype(html_audio_proto)?,
      Some(html_media_proto)
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
    platform.register_wrapper(
      scope.heap(),
      document_obj,
      document_key,
      NodeId::from_index(0),
      DomInterface::Document,
    );
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
    platform.register_wrapper(
      scope.heap(),
      document_a,
      document_key_a,
      NodeId::from_index(0),
      DomInterface::Document,
    );
    platform.register_wrapper(
      scope.heap(),
      document_b,
      document_key_b,
      NodeId::from_index(0),
      DomInterface::Document,
    );

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

    let key = PropertyKey::from_string(scope.alloc_string(super::NODE_ID_KEY)?);
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
    platform.register_wrapper(
      scope.heap(),
      document_obj,
      document_key,
      NodeId::from_index(0),
      DomInterface::Document,
    );

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
    platform.register_wrapper(
      scope.heap(),
      document_obj,
      document_key,
      NodeId::from_index(0),
      DomInterface::Document,
    );

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

    assert!(DomInterface::HTMLMediaElement.implements(DomInterface::HTMLElement));
    assert!(DomInterface::HTMLMediaElement.implements(DomInterface::Element));
    assert!(DomInterface::HTMLMediaElement.implements(DomInterface::Node));
    assert!(DomInterface::HTMLMediaElement.implements(DomInterface::EventTarget));

    assert!(DomInterface::HTMLVideoElement.implements(DomInterface::HTMLMediaElement));
    assert!(DomInterface::HTMLVideoElement.implements(DomInterface::HTMLElement));
    assert!(DomInterface::HTMLVideoElement.implements(DomInterface::Element));

    assert!(DomInterface::HTMLAudioElement.implements(DomInterface::HTMLMediaElement));
    assert!(DomInterface::HTMLAudioElement.implements(DomInterface::HTMLElement));
    assert!(DomInterface::HTMLAudioElement.implements(DomInterface::Element));

    assert!(DomInterface::HTMLInputElement.implements(DomInterface::HTMLElement));
    assert!(DomInterface::HTMLInputElement.implements(DomInterface::Element));
    assert!(DomInterface::HTMLInputElement.implements(DomInterface::Node));
    assert!(DomInterface::HTMLInputElement.implements(DomInterface::EventTarget));

    assert!(!DomInterface::HTMLElement.implements(DomInterface::HTMLInputElement));
    assert!(!DomInterface::Element.implements(DomInterface::HTMLElement));
    assert!(!DomInterface::HTMLMediaElement.implements(DomInterface::HTMLVideoElement));
  }

  #[test]
  fn primary_for_node_kind_maps_html_tags_to_interfaces() {
    let kind = NodeKind::Element {
      tag_name: "INPUT".into(),
      namespace: "".into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLInputElement
    );

    let kind = NodeKind::Element {
      tag_name: "video".into(),
      namespace: "".into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLVideoElement
    );

    let kind = NodeKind::Element {
      tag_name: "AUDIO".into(),
      namespace: HTML_NAMESPACE.into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLAudioElement
    );

    let kind = NodeKind::Element {
      tag_name: "textarea".into(),
      namespace: HTML_NAMESPACE.into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLTextAreaElement
    );

    let kind = NodeKind::Element {
      tag_name: "select".into(),
      namespace: "".into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLSelectElement
    );

    let kind = NodeKind::Element {
      tag_name: "video".into(),
      namespace: "".into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLVideoElement
    );

    let kind = NodeKind::Element {
      tag_name: "audio".into(),
      namespace: "".into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLAudioElement
    );

    let kind = NodeKind::Element {
      tag_name: "option".into(),
      namespace: "".into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLOptionElement
    );

    let kind = NodeKind::Element {
      tag_name: "form".into(),
      namespace: "".into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLFormElement
    );

    let kind = NodeKind::Element {
      tag_name: "img".into(),
      namespace: "".into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLImageElement
    );

    let kind = NodeKind::Element {
      tag_name: "a".into(),
      namespace: "".into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLAnchorElement
    );

    let kind = NodeKind::Element {
      tag_name: "link".into(),
      namespace: "".into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLLinkElement
    );

    let kind = NodeKind::Element {
      tag_name: "script".into(),
      namespace: "".into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLScriptElement
    );

    let kind = NodeKind::Element {
      tag_name: "video".into(),
      namespace: "".into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLVideoElement
    );

    let kind = NodeKind::Element {
      tag_name: "audio".into(),
      namespace: HTML_NAMESPACE.into(),
      prefix: None,
      attributes: vec![],
    };
    assert_eq!(
      DomInterface::primary_for_node_kind(&kind),
      DomInterface::HTMLAudioElement
    );

    let kind = NodeKind::Element {
      tag_name: "div".into(),
      namespace: HTML_NAMESPACE.into(),
      prefix: None,
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
      prefix: None,
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
      prefix: None,
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
