use crate::dom::HTML_NAMESPACE;
use crate::dom2::{Attribute, Document, DomError, NodeId, NodeKind, NULL_NAMESPACE};
use crate::js::bindings::DomExceptionClassVmJs;
use crate::js::cookie_jar::{CookieJar, MAX_COOKIE_STRING_BYTES};
use crate::js::CurrentScriptState;
use crate::resource::ResourceFetcher;
use crate::web::dom::DomException;
use std::cell::RefCell;
use std::char::decode_utf16;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use vm_js::{
  GcObject, GcString, Heap, HostSlots, NativeFunctionId, PropertyDescriptor, PropertyKey,
  PropertyKind, Realm, RootId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
enum DomKind {
  Node = 0,
  Element = 1,
  Document = 2,
}

impl DomKind {
  fn from_u64(n: u64) -> Option<Self> {
    match n {
      0 => Some(Self::Node),
      1 => Some(Self::Element),
      2 => Some(Self::Document),
      _ => None,
    }
  }
}

// Host-slot `b` tags for objects handled by `VmHostHooks::host_exotic_*`.
//
// These hooks are invoked for *all* objects, including objects that use host slots for unrelated
// purposes (e.g. TextDecoder flags/encoding ids). Use collision-resistant tags so our DOM shims do
// not accidentally treat other objects as platform shims.
//
// We use an 8-byte ASCII namespace ("FRDOM...") encoded as a big-endian `u64`. This makes collisions
// across independent shims vanishingly unlikely.
const DOM_TOKEN_LIST_HOST_KIND: u64 = u64::from_be_bytes(*b"FRDOMDTL");
const DOM_STRING_MAP_HOST_KIND: u64 = u64::from_be_bytes(*b"FRDOMDSM");
const CSS_STYLE_DECL_HOST_KIND: u64 = u64::from_be_bytes(*b"FRDOMCSS");

fn dom_kind_for_node_kind(kind: &NodeKind) -> DomKind {
  match kind {
    NodeKind::Document { .. } => DomKind::Document,
    NodeKind::Element { .. } | NodeKind::Slot { .. } => DomKind::Element,
    _ => DomKind::Node,
  }
}

fn data_desc(
  value: Value,
  writable: bool,
  enumerable: bool,
  configurable: bool,
) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable,
    configurable,
    kind: PropertyKind::Data { value, writable },
  }
}

fn method_desc(value: Value) -> PropertyDescriptor {
  data_desc(
    value, /* writable */ true, /* enumerable */ false, /* configurable */ true,
  )
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

fn accessor_desc_get_set(get: Value, set: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Accessor { get, set },
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LiveCollectionKind {
  TagName {
    qualified_name: String,
  },
  TagNameNS {
    namespace: Option<String>,
    local_name: String,
  },
  ClassName {
    required: Vec<String>,
  },
  Name {
    name: String,
  },
}

#[derive(Debug, Clone)]
struct LiveCollection {
  weak_array: WeakGcObject,
  root: NodeId,
  kind: LiveCollectionKind,
  /// Cached property keys for numeric indices ("0", "1", ...).
  ///
  /// This avoids allocating new key strings every time we resync a live collection. We intentionally
  /// keep previously-used index properties on the array (setting them to `undefined` when the
  /// collection shrinks) so these key handles remain GC-reachable.
  index_keys: Vec<GcString>,
  last_len: usize,
}

#[inline]
fn is_html_namespace(namespace: &str) -> bool {
  namespace.is_empty() || namespace == HTML_NAMESPACE
}

#[inline]
fn is_dom_ascii_whitespace(byte: u8) -> bool {
  // DOM "ASCII whitespace" excludes U+000B (vertical tab).
  matches!(byte, b'\t' | b'\n' | 0x0C | b'\r' | b' ')
}

fn split_dom_ascii_whitespace(input: &str) -> Vec<&str> {
  let mut out: Vec<&str> = Vec::new();
  let mut start: Option<usize> = None;
  for (idx, byte) in input.bytes().enumerate() {
    if is_dom_ascii_whitespace(byte) {
      if let Some(start) = start.take() {
        out.push(&input[start..idx]);
      }
    } else if start.is_none() {
      start = Some(idx);
    }
  }
  if let Some(start) = start {
    out.push(&input[start..]);
  }
  out
}

fn element_kind_parts(kind: &NodeKind) -> Option<(&str, &str, &Vec<Attribute>)> {
  match kind {
    NodeKind::Element {
      tag_name,
      namespace,
      attributes,
      ..
    } => Some((tag_name.as_str(), namespace.as_str(), attributes)),
    NodeKind::Slot {
      namespace,
      attributes,
      ..
    } => Some(("slot", namespace.as_str(), attributes)),
    _ => None,
  }
}

fn live_collection_matches(
  kind: &LiveCollectionKind,
  tag: &str,
  namespace: &str,
  attrs: &[Attribute],
) -> bool {
  match kind {
    LiveCollectionKind::TagName { qualified_name } => {
      if qualified_name.is_empty() {
        return false;
      }
      if qualified_name == "*" {
        return true;
      }
      if is_html_namespace(namespace) {
        tag.eq_ignore_ascii_case(qualified_name)
      } else {
        tag == qualified_name
      }
    }
    LiveCollectionKind::TagNameNS {
      namespace: ns,
      local_name,
    } => {
      if local_name.is_empty() {
        return false;
      }

      if let Some(ns) = ns.as_deref() {
        if ns != "*" && ns != "" && ns != HTML_NAMESPACE {
          return false;
        }
        if (ns == "" || ns == HTML_NAMESPACE) && !is_html_namespace(namespace) {
          return false;
        }
      }

      if local_name == "*" {
        return true;
      }
      if is_html_namespace(namespace) {
        tag.eq_ignore_ascii_case(local_name)
      } else {
        tag == local_name
      }
    }
    LiveCollectionKind::ClassName { required } => {
      if required.is_empty() {
        return false;
      }

      let class_attr = attrs
        .iter()
        .find(|attr| {
          if attr.namespace != NULL_NAMESPACE {
            return false;
          }
          if is_html_namespace(namespace) {
            attr.local_name.eq_ignore_ascii_case("class")
          } else {
            attr.local_name == "class"
          }
        })
        .map(|attr| attr.value.as_str());
      let Some(class_attr) = class_attr else {
        return false;
      };

      let have = split_dom_ascii_whitespace(class_attr);
      required
        .iter()
        .all(|required| have.iter().any(|token| token == required))
    }
    LiveCollectionKind::Name { name } => attrs.iter().any(|attr| {
      if attr.namespace != NULL_NAMESPACE {
        return false;
      }
      let name_ok = if is_html_namespace(namespace) {
        attr.local_name.eq_ignore_ascii_case("name")
      } else {
        attr.local_name == "name"
      };
      name_ok && attr.value == *name
    }),
  }
}

pub struct DomHost {
  dom: Rc<RefCell<Document>>,
  current_script: Rc<RefCell<CurrentScriptState>>,
  document_url: Option<String>,
  cookie_fetcher: Option<Arc<dyn ResourceFetcher>>,

  /// Maximum number of bytes allowed when converting JS strings to Rust `String`s for DOM APIs.
  ///
  /// JavaScript strings are UTF-16; converting to UTF-8 can expand the byte size (especially when
  /// the input contains lone surrogates that decode to U+FFFD). Keep this bounded so hostile input
  /// cannot force unbounded host allocations even when the VM heap is capped.
  max_string_bytes: usize,

  cookie_jar: CookieJar,

  // Identity cache: preserve wrapper identity without keeping wrappers alive.
  node_wrappers: HashMap<NodeId, WeakGcObject>,
  class_list_wrappers: HashMap<NodeId, WeakGcObject>,
  dataset_wrappers: HashMap<NodeId, WeakGcObject>,
  style_wrappers: HashMap<NodeId, WeakGcObject>,
  live_collections: Vec<LiveCollection>,

  // Persistent roots for cached objects. `DomHost` isn't traced by the GC.
  prototype_roots: Vec<RootId>,

  // Cached prototypes.
  proto_node: GcObject,
  proto_element: GcObject,
  proto_document: GcObject,
  proto_dom_token_list: GcObject,
  proto_dom_string_map: GcObject,
  proto_css_style_decl: GcObject,
  proto_html_collection: GcObject,

  // Cached constructor/prototype for DOMException.
  dom_exception: DomExceptionClassVmJs,
  type_error_prototype: GcObject,
}

fn host_mut(vm: &mut Vm) -> Result<&mut DomHost, VmError> {
  vm.user_data_mut::<DomHost>().ok_or(VmError::Unimplemented(
    "DOM bindings not installed (missing DomHost user_data)",
  ))
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
  js_string_to_rust_string_limited(scope, host, s, what)
}

fn js_string_to_rust_string_limited<'a>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  handle: vm_js::GcString,
  context: &str,
) -> Result<String, VmError> {
  let js = scope.heap().get_string(handle)?;
  let max_bytes = host.max_string_bytes;

  let code_units_len = js.len_code_units();
  // UTF-8 output bytes are always >= UTF-16 code unit length (and can grow by up to 3 bytes per
  // code unit when decoding lone surrogates as U+FFFD). Reject overly large strings up-front.
  if code_units_len > max_bytes {
    return throw_type_error(
      scope,
      host,
      &format!(
        "{context} exceeded max_string_bytes (len_code_units={code_units_len}, limit={max_bytes})"
      ),
    );
  }

  // Decode manually so we can enforce the byte limit without relying on the potentially-large
  // allocation performed by `String::from_utf16_lossy`.
  let capacity = code_units_len.saturating_mul(3).min(max_bytes);
  let mut out = String::with_capacity(capacity);
  let mut out_len = 0usize;

  for decoded in decode_utf16(js.as_code_units().iter().copied()) {
    let ch = decoded.unwrap_or('\u{FFFD}');
    let ch_len = ch.len_utf8();
    let next_len = out_len.checked_add(ch_len).unwrap_or(usize::MAX);
    if next_len > max_bytes {
      return throw_type_error(
        scope,
        host,
        &format!("{context} exceeded max_string_bytes (limit={max_bytes})"),
      );
    }
    out.push(ch);
    out_len = next_len;
  }

  Ok(out)
}

fn to_dom_string<'a>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  value: Value,
) -> Result<String, VmError> {
  match value {
    Value::Object(_) => Ok("[object Object]".to_string()),
    Value::Symbol(_) => throw_type_error(scope, host, "Cannot convert a Symbol value to a string"),
    other => {
      let s = match scope.heap_mut().to_string(other) {
        Ok(s) => s,
        Err(VmError::TypeError(msg)) => return throw_type_error(scope, host, msg),
        Err(e) => return Err(e),
      };
      js_string_to_rust_string_limited(scope, host, s, "DOMString conversion")
    }
  }
}

fn to_dom_string_nullable<'a>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  value: Value,
) -> Result<String, VmError> {
  match value {
    Value::Undefined | Value::Null => Ok(String::new()),
    other => to_dom_string(scope, host, other),
  }
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

  let slots = scope.heap().object_host_slots(obj)?;
  let Some(slots) = slots else {
    return throw_type_error(scope, host, "receiver is not a DOM wrapper object");
  };

  let Some(kind) = DomKind::from_u64(slots.b) else {
    return throw_type_error(scope, host, "receiver is not a DOM wrapper object");
  };

  let node_idx_u64 = slots.a;
  if node_idx_u64 > (usize::MAX as u64) {
    return throw_type_error(scope, host, "invalid node id on wrapper");
  }
  let node_idx = node_idx_u64 as usize;

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
    return throw_type_error(
      scope,
      host,
      "Document method called on incompatible receiver",
    );
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
    return throw_type_error(
      scope,
      host,
      "Element method called on incompatible receiver",
    );
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

fn require_element_arg<'a>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  value: Value,
) -> Result<NodeId, VmError> {
  let (kind, node_id) = wrapper_meta(scope, host, value)?;
  if kind != DomKind::Element {
    return throw_type_error(scope, host, "argument must be an Element");
  }
  Ok(node_id)
}

fn require_this_dom_token_list<'a>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  this: Value,
) -> Result<NodeId, VmError> {
  let obj = match this {
    Value::Object(o) => o,
    _ => return throw_type_error(scope, host, "receiver is not an object"),
  };

  let slots = scope.heap().object_host_slots(obj)?;
  let Some(slots) = slots else {
    return throw_type_error(
      scope,
      host,
      "DOMTokenList method called on incompatible receiver",
    );
  };

  if slots.b != DOM_TOKEN_LIST_HOST_KIND {
    return throw_type_error(
      scope,
      host,
      "DOMTokenList method called on incompatible receiver",
    );
  }

  let node_idx_u64 = slots.a;
  if node_idx_u64 > (usize::MAX as u64) {
    return throw_type_error(scope, host, "invalid node id on DOMTokenList");
  }

  let node_id = NodeId::from_index(node_idx_u64 as usize);
  if node_id.index() >= host.dom.borrow().nodes_len() {
    return throw_type_error(scope, host, "DOMTokenList refers to an unknown DOM node");
  }

  match &host.dom.borrow().node(node_id).kind {
    NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
    _ => return throw_type_error(scope, host, "DOMTokenList refers to a non-Element node"),
  }

  Ok(node_id)
}

fn require_this_css_style_decl<'a>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  this: Value,
) -> Result<NodeId, VmError> {
  let obj = match this {
    Value::Object(o) => o,
    _ => return throw_type_error(scope, host, "receiver is not an object"),
  };

  let slots = scope.heap().object_host_slots(obj)?;
  let Some(slots) = slots else {
    return throw_type_error(
      scope,
      host,
      "CSSStyleDeclaration method called on incompatible receiver",
    );
  };

  if slots.b != CSS_STYLE_DECL_HOST_KIND {
    return throw_type_error(
      scope,
      host,
      "CSSStyleDeclaration method called on incompatible receiver",
    );
  }

  let node_idx_u64 = slots.a;
  if node_idx_u64 > (usize::MAX as u64) {
    return throw_type_error(scope, host, "invalid node id on CSSStyleDeclaration");
  }

  let node_id = NodeId::from_index(node_idx_u64 as usize);
  if node_id.index() >= host.dom.borrow().nodes_len() {
    return throw_type_error(
      scope,
      host,
      "CSSStyleDeclaration refers to an unknown DOM node",
    );
  }

  match &host.dom.borrow().node(node_id).kind {
    NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
    _ => {
      return throw_type_error(
        scope,
        host,
        "CSSStyleDeclaration refers to a non-Element node",
      )
    }
  }

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
  scope
    .heap_mut()
    .object_set_prototype(obj, Some(prototype))?;

  let name_key = PropertyKey::from_string(scope.alloc_string("name")?);
  let message_key = PropertyKey::from_string(scope.alloc_string("message")?);

  let name_val = Value::String(scope.alloc_string(name)?);
  let message_val = Value::String(scope.alloc_string(message)?);

  scope.define_property(
    obj,
    name_key,
    data_desc(
      name_val, /* writable */ true, /* enumerable */ false, /* configurable */ true,
    ),
  )?;
  scope.define_property(
    obj,
    message_key,
    data_desc(
      message_val,
      /* writable */ true,
      /* enumerable */ false,
      /* configurable */ true,
    ),
  )?;

  Ok(Value::Object(obj))
}

fn throw_type_error<'a, T>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  message: &str,
) -> Result<T, VmError> {
  let err = alloc_error_object(scope, host.type_error_prototype, "TypeError", message)?;
  Err(VmError::Throw(err))
}

fn throw_dom_exception<'a, T>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  name: &str,
  message: &str,
) -> Result<T, VmError> {
  let err = host.dom_exception.new_instance(scope, name, message)?;
  Err(VmError::Throw(err))
}

fn throw_dom_error<'a, T>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  err: DomError,
) -> Result<T, VmError> {
  throw_dom_exception(scope, host, err.code(), err.code())
}

fn throw_web_dom_exception<'a, T>(
  scope: &mut Scope<'a>,
  host: &DomHost,
  err: DomException,
) -> Result<T, VmError> {
  let err = host.dom_exception.from_dom_exception(scope, &err)?;
  Err(VmError::Throw(err))
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
  scope
    .heap_mut()
    .object_set_prototype(wrapper, Some(proto))?;

  scope.heap_mut().object_set_host_slots(
    wrapper,
    HostSlots {
      a: node_id.index() as u64,
      b: kind as u64,
    },
  )?;

  host
    .node_wrappers
    .insert(node_id, WeakGcObject::from(wrapper));
  Ok(Value::Object(wrapper))
}

fn sync_one_live_collection<'a>(
  host: &mut DomHost,
  scope: &mut Scope<'a>,
  dom: &Document,
  array: GcObject,
  coll: &mut LiveCollection,
) -> Result<(), VmError> {
  let nodes = dom.nodes();

  let mut out_len: usize = 0;

  let Some(root_node) = nodes.get(coll.root.index()) else {
    return Ok(());
  };

  // Defensive bound to avoid infinite loops if the tree becomes corrupted.
  let mut remaining = nodes.len() + 1;

  let mut stack: Vec<(NodeId, NodeId)> = Vec::new();
  stack
    .try_reserve_exact(root_node.children.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for &child in root_node.children.iter().rev() {
    stack.push((coll.root, child));
  }

  while let Some((parent_id, node_id)) = stack.pop() {
    if remaining == 0 {
      break;
    }
    remaining -= 1;

    let Some(node) = nodes.get(node_id.index()) else {
      continue;
    };
    if node.parent != Some(parent_id) {
      continue;
    }

    if let Some((tag, namespace, attrs)) = element_kind_parts(&node.kind) {
      if live_collection_matches(&coll.kind, tag, namespace, attrs) {
        if out_len > u32::MAX as usize {
          return Err(VmError::Unimplemented("live collection length exceeds u32"));
        }

        let key_handle = if let Some(existing) = coll.index_keys.get(out_len).copied() {
          existing
        } else {
          let key_handle = scope.alloc_string(&out_len.to_string())?;
          // Root the freshly allocated key string until it's stored on the array.
          scope.push_root(Value::String(key_handle))?;
          coll.index_keys.push(key_handle);
          key_handle
        };

        let wrapper = wrap_node(host, scope, node_id, DomKind::Element)?;
        scope.define_property(
          array,
          PropertyKey::from_string(key_handle),
          data_desc(
            wrapper, /* writable */ true, /* enumerable */ true,
            /* configurable */ true,
          ),
        )?;
        out_len += 1;
      }
    }

    if node.inert_subtree {
      continue;
    }
    if matches!(&node.kind, NodeKind::ShadowRoot { .. }) {
      continue;
    }

    for &child in node.children.iter().rev() {
      let Some(child_node) = nodes.get(child.index()) else {
        continue;
      };
      if child_node.parent != Some(node_id) {
        continue;
      }
      if matches!(&child_node.kind, NodeKind::ShadowRoot { .. }) {
        continue;
      }
      stack.push((node_id, child));
    }
  }

  // Clear stale entries so `collection[i]` returns `undefined` for indices >= length.
  for idx in out_len..coll.last_len {
    if let Some(key_handle) = coll.index_keys.get(idx).copied() {
      scope.define_property(
        array,
        PropertyKey::from_string(key_handle),
        data_desc(
          Value::Undefined,
          /* writable */ true,
          /* enumerable */ true,
          /* configurable */ true,
        ),
      )?;
    }
  }

  // Ensure the `length` slot reflects the number of live matches.
  let length_key = PropertyKey::from_string(scope.alloc_string("length")?);
  scope.define_property(
    array,
    length_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(out_len as f64),
        writable: true,
      },
    },
  )?;

  coll.last_len = out_len;
  Ok(())
}

impl DomHost {
  fn sync_live_collections(&mut self, scope: &mut Scope<'_>) -> Result<(), VmError> {
    // Clone the `Rc` so we can hold a `Ref<Document>` while mutating other host state (wrapper
    // caches, live collection registry) without borrowing `self` immutably for the duration of
    // the sync.
    let dom_rc = Rc::clone(&self.dom);
    let dom = dom_rc.borrow();
    let mut collections = std::mem::take(&mut self.live_collections);
    let mut out: Vec<LiveCollection> = Vec::with_capacity(collections.len());

    for mut coll in collections.drain(..) {
      let Some(array) = coll.weak_array.upgrade(scope.heap()) else {
        continue;
      };

      let mut scope = scope.reborrow();
      scope.push_root(Value::Object(array))?;
      sync_one_live_collection(self, &mut scope, &dom, array, &mut coll)?;
      out.push(coll);
    }

    self.live_collections = out;
    Ok(())
  }

  pub fn set_cookie_fetcher_for_document(
    &mut self,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
  ) {
    self.document_url = Some(document_url.into());
    self.cookie_fetcher = Some(fetcher);
  }

  pub fn clear_cookie_fetcher(&mut self) {
    self.document_url = None;
    self.cookie_fetcher = None;
  }
}

// === Native call handlers ===

fn dom_html_collection_item(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let obj = match this {
    Value::Object(o) => o,
    _ => {
      return throw_type_error(
        scope,
        host,
        "HTMLCollection.item called on incompatible receiver",
      )
    }
  };

  let idx_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut idx = scope.heap_mut().to_number(idx_val)?;
  if !idx.is_finite() {
    idx = 0.0;
  }
  idx = idx.trunc();
  if idx < 0.0 {
    return Ok(Value::Null);
  }
  if idx > (usize::MAX as f64) {
    return Ok(Value::Null);
  }
  let idx = idx as usize;

  let length_key = PropertyKey::from_string(scope.alloc_string("length")?);
  let length_desc = scope.heap().get_property(obj, &length_key)?;
  let length_val = match length_desc.map(|d| d.kind) {
    Some(PropertyKind::Data { value, .. }) => value,
    Some(PropertyKind::Accessor { .. }) => Value::Number(0.0),
    None => Value::Number(0.0),
  };
  let len = match length_val {
    Value::Number(n) if n.is_finite() && n >= 0.0 => n as usize,
    _ => 0,
  };
  if idx >= len {
    return Ok(Value::Null);
  }

  let idx_key = PropertyKey::from_string(scope.alloc_string(&idx.to_string())?);
  let value = scope.heap().get(obj, &idx_key)?;
  if matches!(value, Value::Undefined) {
    Ok(Value::Null)
  } else {
    Ok(value)
  }
}

fn dom_document_get_elements_by_tag_name(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let root = require_this_document(scope, host, this)?;

  let Some(qname_val) = args.get(0).copied() else {
    return throw_type_error(scope, host, "getElementsByTagName requires 1 argument");
  };
  let qualified_name = require_string(scope, host, qname_val, "qualifiedName")?;

  let array = scope.alloc_array(0)?;
  scope.push_root(Value::Object(array))?;
  scope
    .heap_mut()
    .object_set_prototype(array, Some(host.proto_html_collection))?;

  host.live_collections.push(LiveCollection {
    weak_array: WeakGcObject::from(array),
    root,
    kind: LiveCollectionKind::TagName { qualified_name },
    index_keys: Vec::new(),
    last_len: 0,
  });

  host.sync_live_collections(scope)?;
  Ok(Value::Object(array))
}

fn dom_element_get_elements_by_tag_name(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let root = require_this_element(scope, host, this)?;

  let Some(qname_val) = args.get(0).copied() else {
    return throw_type_error(scope, host, "getElementsByTagName requires 1 argument");
  };
  let qualified_name = require_string(scope, host, qname_val, "qualifiedName")?;

  let array = scope.alloc_array(0)?;
  scope.push_root(Value::Object(array))?;
  scope
    .heap_mut()
    .object_set_prototype(array, Some(host.proto_html_collection))?;

  host.live_collections.push(LiveCollection {
    weak_array: WeakGcObject::from(array),
    root,
    kind: LiveCollectionKind::TagName { qualified_name },
    index_keys: Vec::new(),
    last_len: 0,
  });

  host.sync_live_collections(scope)?;
  Ok(Value::Object(array))
}

fn dom_document_get_elements_by_tag_name_ns(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let root = require_this_document(scope, host, this)?;

  let Some(namespace_val) = args.get(0).copied() else {
    return throw_type_error(scope, host, "getElementsByTagNameNS requires 2 arguments");
  };
  let Some(local_name_val) = args.get(1).copied() else {
    return throw_type_error(scope, host, "getElementsByTagNameNS requires 2 arguments");
  };

  let namespace = match namespace_val {
    Value::Null | Value::Undefined => None,
    other => Some(require_string(scope, host, other, "namespace")?),
  };
  let local_name = require_string(scope, host, local_name_val, "localName")?;

  let array = scope.alloc_array(0)?;
  scope.push_root(Value::Object(array))?;
  scope
    .heap_mut()
    .object_set_prototype(array, Some(host.proto_html_collection))?;

  host.live_collections.push(LiveCollection {
    weak_array: WeakGcObject::from(array),
    root,
    kind: LiveCollectionKind::TagNameNS {
      namespace,
      local_name,
    },
    index_keys: Vec::new(),
    last_len: 0,
  });

  host.sync_live_collections(scope)?;
  Ok(Value::Object(array))
}

fn dom_element_get_elements_by_tag_name_ns(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let root = require_this_element(scope, host, this)?;

  let Some(namespace_val) = args.get(0).copied() else {
    return throw_type_error(scope, host, "getElementsByTagNameNS requires 2 arguments");
  };
  let Some(local_name_val) = args.get(1).copied() else {
    return throw_type_error(scope, host, "getElementsByTagNameNS requires 2 arguments");
  };

  let namespace = match namespace_val {
    Value::Null | Value::Undefined => None,
    other => Some(require_string(scope, host, other, "namespace")?),
  };
  let local_name = require_string(scope, host, local_name_val, "localName")?;

  let array = scope.alloc_array(0)?;
  scope.push_root(Value::Object(array))?;
  scope
    .heap_mut()
    .object_set_prototype(array, Some(host.proto_html_collection))?;

  host.live_collections.push(LiveCollection {
    weak_array: WeakGcObject::from(array),
    root,
    kind: LiveCollectionKind::TagNameNS {
      namespace,
      local_name,
    },
    index_keys: Vec::new(),
    last_len: 0,
  });

  host.sync_live_collections(scope)?;
  Ok(Value::Object(array))
}

fn dom_document_get_elements_by_class_name(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let root = require_this_document(scope, host, this)?;

  let Some(class_names_val) = args.get(0).copied() else {
    return throw_type_error(scope, host, "getElementsByClassName requires 1 argument");
  };
  let class_names = require_string(scope, host, class_names_val, "classNames")?;

  let required: Vec<String> = split_dom_ascii_whitespace(&class_names)
    .into_iter()
    .map(|s| s.to_string())
    .collect();

  let array = scope.alloc_array(0)?;
  scope.push_root(Value::Object(array))?;
  scope
    .heap_mut()
    .object_set_prototype(array, Some(host.proto_html_collection))?;

  host.live_collections.push(LiveCollection {
    weak_array: WeakGcObject::from(array),
    root,
    kind: LiveCollectionKind::ClassName { required },
    index_keys: Vec::new(),
    last_len: 0,
  });

  host.sync_live_collections(scope)?;
  Ok(Value::Object(array))
}

fn dom_element_get_elements_by_class_name(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let root = require_this_element(scope, host, this)?;

  let Some(class_names_val) = args.get(0).copied() else {
    return throw_type_error(scope, host, "getElementsByClassName requires 1 argument");
  };
  let class_names = require_string(scope, host, class_names_val, "classNames")?;

  let required: Vec<String> = split_dom_ascii_whitespace(&class_names)
    .into_iter()
    .map(|s| s.to_string())
    .collect();

  let array = scope.alloc_array(0)?;
  scope.push_root(Value::Object(array))?;
  scope
    .heap_mut()
    .object_set_prototype(array, Some(host.proto_html_collection))?;

  host.live_collections.push(LiveCollection {
    weak_array: WeakGcObject::from(array),
    root,
    kind: LiveCollectionKind::ClassName { required },
    index_keys: Vec::new(),
    last_len: 0,
  });

  host.sync_live_collections(scope)?;
  Ok(Value::Object(array))
}

fn dom_document_get_elements_by_name(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let root = require_this_document(scope, host, this)?;

  let Some(name_val) = args.get(0).copied() else {
    return throw_type_error(scope, host, "getElementsByName requires 1 argument");
  };
  let name = require_string(scope, host, name_val, "name")?;

  let array = scope.alloc_array(0)?;
  scope.push_root(Value::Object(array))?;
  scope
    .heap_mut()
    .object_set_prototype(array, Some(host.proto_html_collection))?;

  host.live_collections.push(LiveCollection {
    weak_array: WeakGcObject::from(array),
    root,
    kind: LiveCollectionKind::Name { name },
    index_keys: Vec::new(),
    last_len: 0,
  });

  host.sync_live_collections(scope)?;
  Ok(Value::Object(array))
}

fn dom_illegal_constructor(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  throw_type_error(scope, host, "Illegal constructor")
}

fn dom_document_create_element(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
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
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
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
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
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

fn dom_document_write(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  require_this_document(scope, host, this)?;

  // HTML: concatenate arguments after applying ToString.
  let mut out = String::new();
  for &arg in args {
    out.push_str(&to_dom_string(scope, host, arg)?);
  }

  // Deterministic subset of HTML's ignore-destructive-writes behavior:
  // only allow writes while a streaming parser is active.
  if let Some(parser) = crate::html::document_write::current_streaming_parser() {
    parser.push_front_str(&out);
  }

  Ok(Value::Undefined)
}

fn dom_document_writeln(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  require_this_document(scope, host, this)?;

  let mut out = String::new();
  for &arg in args {
    out.push_str(&to_dom_string(scope, host, arg)?);
  }
  out.push('\n');

  if let Some(parser) = crate::html::document_write::current_streaming_parser() {
    parser.push_front_str(&out);
  }

  Ok(Value::Undefined)
}

fn dom_node_append_child(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
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

  let changed = match host.dom.borrow_mut().append_child(parent, child) {
    Ok(changed) => changed,
    Err(err) => return throw_dom_error(scope, host, err),
  };
  if changed {
    host.sync_live_collections(scope)?;
  }
  Ok(child_val)
}

fn dom_node_clone_node(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;

  // Per DOM, missing/undefined => false via ToBoolean.
  let deep_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let deep = scope.heap().to_boolean(deep_val)?;

  let cloned = {
    let mut dom = host.dom.borrow_mut();
    match dom.clone_node(node_id, deep) {
      Ok(id) => id,
      Err(err) => return throw_dom_error(scope, host, err),
    }
  };

  let kind = dom_kind_for_node_kind(&host.dom.borrow().node(cloned).kind);
  wrap_node(host, scope, cloned, kind)
}

fn dom_node_has_child_nodes(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;

  let has_children = {
    let dom = host.dom.borrow();
    let node = dom.node(node_id);
    if node.inert_subtree {
      false
    } else {
      !node.children.is_empty()
    }
  };
  Ok(Value::Bool(has_children))
}

fn dom_node_parent_node_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;

  let parent = match host.dom.borrow().parent(node_id) {
    Ok(parent) => parent,
    Err(err) => return throw_dom_error(scope, host, err),
  };
  let Some(parent_id) = parent else {
    return Ok(Value::Null);
  };

  // Template contents are stored under the `<template>` element in `dom2` but should behave like a
  // disconnected subtree for scripting and navigation. (See `dom2::Node::inert_subtree`.)
  if host.dom.borrow().node(parent_id).inert_subtree {
    return Ok(Value::Null);
  }

  let kind = dom_kind_for_node_kind(&host.dom.borrow().node(parent_id).kind);
  wrap_node(host, scope, parent_id, kind)
}

fn dom_node_parent_element_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;

  let parent = match host.dom.borrow().parent(node_id) {
    Ok(parent) => parent,
    Err(err) => return throw_dom_error(scope, host, err),
  };
  let Some(parent_id) = parent else {
    return Ok(Value::Null);
  };

  if host.dom.borrow().node(parent_id).inert_subtree {
    return Ok(Value::Null);
  }

  let is_element_parent = {
    let dom = host.dom.borrow();
    matches!(
      &dom.node(parent_id).kind,
      NodeKind::Element { .. } | NodeKind::Slot { .. }
    )
  };
  if is_element_parent {
    wrap_node(host, scope, parent_id, DomKind::Element)
  } else {
    Ok(Value::Null)
  }
}

fn dom_node_first_child_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;

  let (first_id, kind) = {
    let dom = host.dom.borrow();
    let node = dom.node(node_id);
    if node.inert_subtree {
      return Ok(Value::Null);
    }
    let Some(first_id) = node.children.first().copied() else {
      return Ok(Value::Null);
    };
    let kind = dom_kind_for_node_kind(&dom.node(first_id).kind);
    (first_id, kind)
  };
  wrap_node(host, scope, first_id, kind)
}

fn dom_node_last_child_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;

  let (last_id, kind) = {
    let dom = host.dom.borrow();
    let node = dom.node(node_id);
    if node.inert_subtree {
      return Ok(Value::Null);
    }
    let Some(last_id) = node.children.last().copied() else {
      return Ok(Value::Null);
    };
    let kind = dom_kind_for_node_kind(&dom.node(last_id).kind);
    (last_id, kind)
  };
  wrap_node(host, scope, last_id, kind)
}

fn dom_node_previous_sibling_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;

  let parent = match host.dom.borrow().parent(node_id) {
    Ok(parent) => parent,
    Err(err) => return throw_dom_error(scope, host, err),
  };
  let Some(parent_id) = parent else {
    return Ok(Value::Null);
  };

  if host.dom.borrow().node(parent_id).inert_subtree {
    return Ok(Value::Null);
  }

  let sibling_id = {
    let dom = host.dom.borrow();
    let siblings = match dom.children(parent_id) {
      Ok(children) => children,
      Err(err) => return throw_dom_error(scope, host, err),
    };
    let idx = siblings
      .iter()
      .position(|&id| id == node_id)
      .ok_or(VmError::InvariantViolation(
        "DOM node not found in parent's children list",
      ))?;
    if idx == 0 {
      return Ok(Value::Null);
    }
    siblings[idx - 1]
  };
  let kind = dom_kind_for_node_kind(&host.dom.borrow().node(sibling_id).kind);
  wrap_node(host, scope, sibling_id, kind)
}

fn dom_node_next_sibling_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;

  let parent = match host.dom.borrow().parent(node_id) {
    Ok(parent) => parent,
    Err(err) => return throw_dom_error(scope, host, err),
  };
  let Some(parent_id) = parent else {
    return Ok(Value::Null);
  };

  if host.dom.borrow().node(parent_id).inert_subtree {
    return Ok(Value::Null);
  }

  let sibling_id = {
    let dom = host.dom.borrow();
    let siblings = match dom.children(parent_id) {
      Ok(children) => children,
      Err(err) => return throw_dom_error(scope, host, err),
    };
    let idx = siblings
      .iter()
      .position(|&id| id == node_id)
      .ok_or(VmError::InvariantViolation(
        "DOM node not found in parent's children list",
      ))?;
    let Some(&sibling_id) = siblings.get(idx + 1) else {
      return Ok(Value::Null);
    };
    sibling_id
  };
  let kind = dom_kind_for_node_kind(&host.dom.borrow().node(sibling_id).kind);
  wrap_node(host, scope, sibling_id, kind)
}

fn dom_node_node_type_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;

  let node_type = match &host.dom.borrow().node(node_id).kind {
    NodeKind::Document { .. } => 9,
    NodeKind::DocumentFragment => 11,
    NodeKind::Comment { .. } => 8,
    NodeKind::ProcessingInstruction { .. } => 7,
    NodeKind::Doctype { .. } => 10,
    NodeKind::ShadowRoot { .. } => 11,
    NodeKind::Slot { .. } | NodeKind::Element { .. } => 1,
    NodeKind::Text { .. } => 3,
  };

  Ok(Value::Number(node_type as f64))
}

fn dom_node_node_name_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;

  let name = {
    let dom = host.dom.borrow();
    let node = dom.node(node_id);
    match &node.kind {
      NodeKind::Document { .. } => "#document".to_string(),
      NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => "#document-fragment".to_string(),
      NodeKind::Text { .. } => "#text".to_string(),
      NodeKind::Comment { .. } => "#comment".to_string(),
      NodeKind::ProcessingInstruction { target, .. } => target.to_string(),
      NodeKind::Doctype { name, .. } => name.to_string(),
      NodeKind::Slot { namespace, .. } => {
        if is_html_namespace(namespace) {
          "SLOT".to_string()
        } else {
          "slot".to_string()
        }
      }
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => {
        if is_html_namespace(namespace) {
          tag_name.to_ascii_uppercase()
        } else {
          tag_name.to_string()
        }
      }
    }
  };

  Ok(Value::String(scope.alloc_string(&name)?))
}

fn dom_node_node_value_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;

  let dom = host.dom.borrow();
  match &dom.node(node_id).kind {
    NodeKind::Text { content } => Ok(Value::String(scope.alloc_string(content)?)),
    NodeKind::Comment { content } => Ok(Value::String(scope.alloc_string(content)?)),
    NodeKind::ProcessingInstruction { data, .. } => Ok(Value::String(scope.alloc_string(data)?)),
    _ => Ok(Value::Null),
  }
}

fn dom_node_node_value_setter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;

  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  let new_value = to_dom_string_nullable(scope, host, value)?;

  enum TargetKind {
    Text,
    Comment,
    ProcessingInstruction,
    None,
  }

  let target = {
    let dom = host.dom.borrow();
    match &dom.node(node_id).kind {
      NodeKind::Text { .. } => TargetKind::Text,
      NodeKind::Comment { .. } => TargetKind::Comment,
      NodeKind::ProcessingInstruction { .. } => TargetKind::ProcessingInstruction,
      _ => TargetKind::None,
    }
  };

  let mut dom = host.dom.borrow_mut();
  match target {
    TargetKind::Text => {
      if let Err(err) = dom.set_text_data(node_id, &new_value) {
        return throw_dom_error(scope, host, err);
      }
    }
    TargetKind::Comment => {
      if let Err(err) = dom.set_comment_data(node_id, &new_value) {
        return throw_dom_error(scope, host, err);
      }
    }
    TargetKind::ProcessingInstruction => {
      if let Err(err) = dom.set_processing_instruction_data(node_id, &new_value) {
        return throw_dom_error(scope, host, err);
      }
    }
    TargetKind::None => {}
  }

  Ok(Value::Undefined)
}

fn dom_node_is_connected_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;
  Ok(Value::Bool(
    host.dom.borrow().is_connected_for_scripting(node_id),
  ))
}

fn dom_element_tag_name_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  let tag_name = {
    let dom = host.dom.borrow();
    match &dom.node(element_id).kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => {
        if is_html_namespace(namespace) {
          tag_name.to_ascii_uppercase()
        } else {
          tag_name.to_string()
        }
      }
      NodeKind::Slot { namespace, .. } => {
        if is_html_namespace(namespace) {
          "SLOT".to_string()
        } else {
          "slot".to_string()
        }
      }
      _ => return throw_type_error(scope, host, "Element.tagName called on non-Element node"),
    }
  };

  Ok(Value::String(scope.alloc_string(&tag_name)?))
}

fn dom_element_id_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  let id = match host.dom.borrow().id(element_id) {
    Ok(Some(id)) => id.to_string(),
    Ok(None) => String::new(),
    Err(err) => return throw_dom_error(scope, host, err),
  };
  Ok(Value::String(scope.alloc_string(&id)?))
}

fn dom_element_id_setter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  let id = to_dom_string_nullable(scope, host, value)?;
  if let Err(err) = host.dom.borrow_mut().set_attribute(element_id, "id", &id) {
    return throw_dom_error(scope, host, err);
  }
  Ok(Value::Undefined)
}

fn dom_element_class_name_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  let class_name = match host.dom.borrow().class_name(element_id) {
    Ok(Some(v)) => v.to_string(),
    Ok(None) => String::new(),
    Err(err) => return throw_dom_error(scope, host, err),
  };
  Ok(Value::String(scope.alloc_string(&class_name)?))
}

fn dom_element_class_name_setter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  let class_name = to_dom_string_nullable(scope, host, value)?;
  if let Err(err) = host
    .dom
    .borrow_mut()
    .set_attribute(element_id, "class", &class_name)
  {
    return throw_dom_error(scope, host, err);
  }
  Ok(Value::Undefined)
}

fn dom_element_set_attribute(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
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

  let changed = match host.dom.borrow_mut().set_attribute(node_id, &name, &value) {
    Ok(changed) => changed,
    Err(err) => return throw_dom_error(scope, host, err),
  };
  if changed {
    host.sync_live_collections(scope)?;
  }
  Ok(Value::Undefined)
}

fn dom_element_inner_html_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  match host.dom.borrow().inner_html(element_id) {
    Ok(html) => Ok(Value::String(scope.alloc_string(&html)?)),
    Err(err) => throw_dom_error(scope, host, err),
  }
}

fn dom_element_inner_html_setter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  let html_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let html = to_dom_string(scope, host, html_val)?;

  if let Err(err) = host.dom.borrow_mut().set_inner_html(element_id, &html) {
    return throw_dom_error(scope, host, err);
  }

  host.sync_live_collections(scope)?;
  Ok(Value::Undefined)
}

fn dom_element_outer_html_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  match host.dom.borrow().outer_html(element_id) {
    Ok(html) => Ok(Value::String(scope.alloc_string(&html)?)),
    Err(err) => throw_dom_error(scope, host, err),
  }
}

fn dom_element_outer_html_setter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  let html_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let html = to_dom_string(scope, host, html_val)?;

  if let Err(err) = host.dom.borrow_mut().set_outer_html(element_id, &html) {
    return throw_dom_error(scope, host, err);
  }

  host.sync_live_collections(scope)?;
  Ok(Value::Undefined)
}

fn dom_element_insert_adjacent_html(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  let position_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let html_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let position = to_dom_string(scope, host, position_val)?;
  let html = to_dom_string(scope, host, html_val)?;

  if let Err(err) = host
    .dom
    .borrow_mut()
    .insert_adjacent_html(element_id, &position, &html)
  {
    return throw_dom_error(scope, host, err);
  }

  host.sync_live_collections(scope)?;
  Ok(Value::Undefined)
}

fn dom_element_insert_adjacent_element(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  let where_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let new_element_val = args.get(1).copied().unwrap_or(Value::Undefined);

  let where_ = to_dom_string(scope, host, where_val)?;
  let new_element_id = require_element_arg(scope, host, new_element_val)?;

  let inserted =
    match host
      .dom
      .borrow_mut()
      .insert_adjacent_element(element_id, &where_, new_element_id)
    {
      Ok(v) => v,
      Err(err) => return throw_dom_error(scope, host, err),
    };

  let Some(inserted_id) = inserted else {
    return Ok(Value::Null);
  };

  host.sync_live_collections(scope)?;
  wrap_node(host, scope, inserted_id, DomKind::Element)
}

fn dom_element_insert_adjacent_text(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  let where_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let data_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let where_ = to_dom_string(scope, host, where_val)?;
  let data = to_dom_string(scope, host, data_val)?;

  if let Err(err) = host
    .dom
    .borrow_mut()
    .insert_adjacent_text(element_id, &where_, &data)
  {
    return throw_dom_error(scope, host, err);
  }

  host.sync_live_collections(scope)?;
  Ok(Value::Undefined)
}

fn dom_node_text_content_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;

  let dom = host.dom.borrow();
  let node = dom.node(node_id);

  match &node.kind {
    NodeKind::Text { content } => Ok(Value::String(scope.alloc_string(content)?)),
    NodeKind::Comment { content } => Ok(Value::String(scope.alloc_string(content)?)),
    NodeKind::ProcessingInstruction { data, .. } => Ok(Value::String(scope.alloc_string(data)?)),
    NodeKind::Doctype { .. } => Ok(Value::Null),
    // DOM `Node.textContent` returns `null` for `Document` nodes.
    //
    // https://dom.spec.whatwg.org/#dom-node-textcontent
    NodeKind::Document { .. } => Ok(Value::Null),

    NodeKind::Element { .. }
    | NodeKind::Slot { .. }
    | NodeKind::ShadowRoot { .. }
    | NodeKind::DocumentFragment => {
      let mut out = String::new();
      let mut stack: Vec<NodeId> = vec![node_id];
      while let Some(id) = stack.pop() {
        let n = dom.node(id);
        if let NodeKind::Text { content } = &n.kind {
          out.push_str(content);
        }
        if n.inert_subtree {
          continue;
        }
        for &child in n.children.iter().rev() {
          stack.push(child);
        }
      }
      Ok(Value::String(scope.alloc_string(&out)?))
    }
  }
}

fn dom_node_text_content_setter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let node_id = require_this_node(scope, host, this)?;

  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  let new_text = to_dom_string_nullable(scope, host, value)?;

  // Mutate the underlying DOM tree.
  let mut dom = host.dom.borrow_mut();
  match &dom.node(node_id).kind {
    // `Document.textContent = ...` is a no-op (the getter returns `null` too).
    NodeKind::Document { .. } => return Ok(Value::Undefined),
    NodeKind::Text { .. } => {
      if let Err(err) = dom.set_text_data(node_id, &new_text) {
        return throw_dom_error(scope, host, err);
      }
      return Ok(Value::Undefined);
    }
    NodeKind::Comment { .. } => {
      if let Err(err) = dom.set_comment_data(node_id, &new_text) {
        return throw_dom_error(scope, host, err);
      }
      return Ok(Value::Undefined);
    }
    NodeKind::ProcessingInstruction { .. } => {
      if let Err(err) = dom.set_processing_instruction_data(node_id, &new_text) {
        return throw_dom_error(scope, host, err);
      }
      return Ok(Value::Undefined);
    }
    NodeKind::Doctype { .. } => {
      // Per DOM, setting `textContent` on a doctype is a no-op.
      return Ok(Value::Undefined);
    }
    NodeKind::Element { .. }
    | NodeKind::Slot { .. }
    | NodeKind::ShadowRoot { .. }
    | NodeKind::DocumentFragment => {}
  }

  // Replace all children.
  //
  // Keep this routed through `dom2` mutation APIs so any live-mutation hooks (e.g. Range /
  // NodeIterator updates) and MutationObserver records are not bypassed.
  let mut changed = false;
  let old_children = dom.node(node_id).children.clone();
  let children_to_remove: Vec<NodeId> = old_children
    .into_iter()
    .filter(|&child| dom.node(child).parent == Some(node_id))
    .collect();
  for child in children_to_remove {
    changed |= match dom.remove_child(node_id, child) {
      Ok(v) => v,
      Err(err) => return throw_dom_error(scope, host, err),
    };
  }

  if !new_text.is_empty() {
    let text_id = dom.create_text(&new_text);
    changed |= match dom.append_child(node_id, text_id) {
      Ok(v) => v,
      Err(err) => return throw_dom_error(scope, host, err),
    };
  }

  drop(dom);
  if changed {
    host.sync_live_collections(scope)?;
  }
  Ok(Value::Undefined)
}

fn dom_element_class_list_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  if let Some(existing) = host
    .class_list_wrappers
    .get(&element_id)
    .copied()
    .and_then(|weak| weak.upgrade(scope.heap()))
  {
    return Ok(Value::Object(existing));
  }

  let wrapper = scope.alloc_object()?;
  scope.push_root(Value::Object(wrapper))?;
  scope
    .heap_mut()
    .object_set_prototype(wrapper, Some(host.proto_dom_token_list))?;

  scope.heap_mut().object_set_host_slots(
    wrapper,
    HostSlots {
      a: element_id.index() as u64,
      b: DOM_TOKEN_LIST_HOST_KIND,
    },
  )?;

  host
    .class_list_wrappers
    .insert(element_id, WeakGcObject::from(wrapper));

  Ok(Value::Object(wrapper))
}

fn dom_element_dataset_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  if let Some(existing) = host
    .dataset_wrappers
    .get(&element_id)
    .copied()
    .and_then(|weak| weak.upgrade(scope.heap()))
  {
    return Ok(Value::Object(existing));
  }

  let wrapper = scope.alloc_object()?;
  scope.push_root(Value::Object(wrapper))?;
  scope
    .heap_mut()
    .object_set_prototype(wrapper, Some(host.proto_dom_string_map))?;

  scope.heap_mut().object_set_host_slots(
    wrapper,
    HostSlots {
      a: element_id.index() as u64,
      b: DOM_STRING_MAP_HOST_KIND,
    },
  )?;

  host
    .dataset_wrappers
    .insert(element_id, WeakGcObject::from(wrapper));

  Ok(Value::Object(wrapper))
}

fn dom_element_style_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_element(scope, host, this)?;

  if let Some(existing) = host
    .style_wrappers
    .get(&element_id)
    .copied()
    .and_then(|weak| weak.upgrade(scope.heap()))
  {
    return Ok(Value::Object(existing));
  }

  let wrapper = scope.alloc_object()?;
  scope.push_root(Value::Object(wrapper))?;
  scope
    .heap_mut()
    .object_set_prototype(wrapper, Some(host.proto_css_style_decl))?;

  scope.heap_mut().object_set_host_slots(
    wrapper,
    HostSlots {
      a: element_id.index() as u64,
      b: CSS_STYLE_DECL_HOST_KIND,
    },
  )?;

  host
    .style_wrappers
    .insert(element_id, WeakGcObject::from(wrapper));

  Ok(Value::Object(wrapper))
}

fn dom_token_list_contains(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_dom_token_list(scope, host, this)?;
  let token_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let token = to_dom_string(scope, host, token_val)?;

  match host.dom.borrow().class_list_contains(element_id, &token) {
    Ok(v) => Ok(Value::Bool(v)),
    Err(e) => throw_dom_error(scope, host, e),
  }
}

fn dom_token_list_add(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_dom_token_list(scope, host, this)?;

  let mut tokens: Vec<String> = Vec::with_capacity(args.len());
  for &arg in args {
    tokens.push(to_dom_string(scope, host, arg)?);
  }
  let token_refs: Vec<&str> = tokens.iter().map(|s| s.as_str()).collect();

  let result = host
    .dom
    .borrow_mut()
    .class_list_add(element_id, token_refs.as_slice());
  match result {
    Ok(changed) => {
      if changed {
        host.sync_live_collections(scope)?;
      }
      Ok(Value::Undefined)
    }
    Err(e) => throw_dom_error(scope, host, e),
  }
}

fn dom_token_list_remove(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_dom_token_list(scope, host, this)?;

  let mut tokens: Vec<String> = Vec::with_capacity(args.len());
  for &arg in args {
    tokens.push(to_dom_string(scope, host, arg)?);
  }
  let token_refs: Vec<&str> = tokens.iter().map(|s| s.as_str()).collect();

  let result = host
    .dom
    .borrow_mut()
    .class_list_remove(element_id, token_refs.as_slice());
  match result {
    Ok(changed) => {
      if changed {
        host.sync_live_collections(scope)?;
      }
      Ok(Value::Undefined)
    }
    Err(e) => throw_dom_error(scope, host, e),
  }
}

fn dom_token_list_toggle(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_dom_token_list(scope, host, this)?;

  let token_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let token = to_dom_string(scope, host, token_val)?;

  let force = match args.get(1).copied() {
    None => None,
    Some(v) => Some(scope.heap().to_boolean(v)?),
  };

  let before = match host.dom.borrow().class_list_contains(element_id, &token) {
    Ok(v) => v,
    Err(e) => return throw_dom_error(scope, host, e),
  };

  let result = host
    .dom
    .borrow_mut()
    .class_list_toggle(element_id, &token, force);
  match result {
    Ok(after) => {
      if after != before {
        host.sync_live_collections(scope)?;
      }
      Ok(Value::Bool(after))
    }
    Err(e) => throw_dom_error(scope, host, e),
  }
}

fn dom_token_list_replace(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_dom_token_list(scope, host, this)?;

  let token_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let new_token_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let token = to_dom_string(scope, host, token_val)?;
  let new_token = to_dom_string(scope, host, new_token_val)?;

  let before = match host.dom.borrow().get_attribute(element_id, "class") {
    Ok(v) => v.map(str::to_string),
    Err(e) => return throw_dom_error(scope, host, e),
  };

  let found = match host
    .dom
    .borrow_mut()
    .class_list_replace(element_id, &token, &new_token)
  {
    Ok(v) => v,
    Err(e) => return throw_dom_error(scope, host, e),
  };

  let after = match host.dom.borrow().get_attribute(element_id, "class") {
    Ok(v) => v.map(str::to_string),
    Err(e) => return throw_dom_error(scope, host, e),
  };

  if before != after {
    host.sync_live_collections(scope)?;
  }

  Ok(Value::Bool(found))
}

fn css_style_decl_get_property_value(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_css_style_decl(scope, host, this)?;

  let name_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let name = to_dom_string(scope, host, name_val)?;

  let value = host
    .dom
    .borrow()
    .style_get_property_value(element_id, &name);
  Ok(Value::String(scope.alloc_string(&value)?))
}

fn css_style_decl_set_property(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  let element_id = require_this_css_style_decl(scope, host, this)?;

  let name_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let value_val = args.get(1).copied().unwrap_or(Value::Undefined);
  let name = to_dom_string(scope, host, name_val)?;
  let value = to_dom_string(scope, host, value_val)?;

  match host
    .dom
    .borrow_mut()
    .style_set_property(element_id, &name, &value)
  {
    Ok(_) => Ok(Value::Undefined),
    Err(e) => throw_dom_error(scope, host, e),
  }
}

fn dom_document_current_script_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
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

fn dom_document_cookie_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  require_this_document(scope, host, this)?;

  if let (Some(fetcher), Some(url)) = (host.cookie_fetcher.as_ref(), host.document_url.as_deref()) {
    if let Some(header) = fetcher.cookie_header_value(url) {
      host.cookie_jar.replace_from_cookie_header(&header);
    }
  }

  Ok(Value::String(
    scope.alloc_string(&host.cookie_jar.cookie_string())?,
  ))
}

fn dom_document_cookie_setter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host_mut(vm)?;
  require_this_document(scope, host, this)?;

  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  let cookie_string = match value {
    Value::Object(_) => "[object Object]".to_string(),
    Value::Symbol(_) => {
      return throw_type_error(scope, host, "Cannot convert a Symbol value to a string")
    }
    other => {
      let s = match scope.heap_mut().to_string(other) {
        Ok(s) => s,
        Err(VmError::TypeError(msg)) => return throw_type_error(scope, host, msg),
        Err(e) => return Err(e),
      };
      let js = scope.heap().get_string(s)?;
      if js.as_code_units().len() > MAX_COOKIE_STRING_BYTES {
        return Ok(Value::Undefined);
      }
      js.to_utf8_lossy()
    }
  };

  if let (Some(fetcher), Some(url)) = (host.cookie_fetcher.as_ref(), host.document_url.as_deref()) {
    fetcher.store_cookie_from_document(url, &cookie_string);
  }

  host.cookie_jar.set_cookie_string(&cookie_string);
  Ok(Value::Undefined)
}

pub fn install_dom_bindings(
  vm: &mut Vm,
  heap: &mut Heap,
  realm: &Realm,
  dom: Rc<RefCell<Document>>,
  current_script: Rc<RefCell<CurrentScriptState>>,
) -> Result<(), VmError> {
  const DEFAULT_MAX_DOM_STRING_BYTES: usize = 1024 * 1024;
  let max_string_bytes = DEFAULT_MAX_DOM_STRING_BYTES.min(heap.limits().max_bytes);
  install_dom_bindings_with_limits(vm, heap, realm, dom, current_script, max_string_bytes)
}

pub fn install_dom_bindings_with_limits(
  vm: &mut Vm,
  heap: &mut Heap,
  realm: &Realm,
  dom: Rc<RefCell<Document>>,
  current_script: Rc<RefCell<CurrentScriptState>>,
  max_string_bytes: usize,
) -> Result<(), VmError> {
  let mut scope = heap.scope();

  // `vm_dom` is a host-driven DOM binding layer; ensure a spec-shaped `DOMException` exists on the
  // realm global so scripts can construct/catch it (and host code can throw DOMException-like
  // objects).
  let dom_exception = DomExceptionClassVmJs::install(vm, &mut scope, realm)?;

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

  let proto_dom_token_list = scope.alloc_object()?;
  scope.push_root(Value::Object(proto_dom_token_list))?;
  scope.heap_mut().object_set_prototype(
    proto_dom_token_list,
    Some(realm.intrinsics().object_prototype()),
  )?;

  let proto_dom_string_map = scope.alloc_object()?;
  scope.push_root(Value::Object(proto_dom_string_map))?;
  scope.heap_mut().object_set_prototype(
    proto_dom_string_map,
    Some(realm.intrinsics().object_prototype()),
  )?;

  let proto_css_style_decl = scope.alloc_object()?;
  scope.push_root(Value::Object(proto_css_style_decl))?;
  scope.heap_mut().object_set_prototype(
    proto_css_style_decl,
    Some(realm.intrinsics().object_prototype()),
  )?;

  let proto_html_collection = scope.alloc_object()?;
  scope.push_root(Value::Object(proto_html_collection))?;
  scope.heap_mut().object_set_prototype(
    proto_html_collection,
    Some(realm.intrinsics().array_prototype()),
  )?;

  // Register native call handlers.
  let call_html_collection_item = vm.register_native_call(dom_html_collection_item)?;
  let call_create_element = vm.register_native_call(dom_document_create_element)?;
  let call_get_element_by_id = vm.register_native_call(dom_document_get_element_by_id)?;
  let call_query_selector = vm.register_native_call(dom_document_query_selector)?;
  let call_get_elements_by_tag_name =
    vm.register_native_call(dom_document_get_elements_by_tag_name)?;
  let call_get_elements_by_tag_name_ns =
    vm.register_native_call(dom_document_get_elements_by_tag_name_ns)?;
  let call_get_elements_by_class_name =
    vm.register_native_call(dom_document_get_elements_by_class_name)?;
  let call_get_elements_by_name = vm.register_native_call(dom_document_get_elements_by_name)?;
  let call_element_get_elements_by_tag_name =
    vm.register_native_call(dom_element_get_elements_by_tag_name)?;
  let call_element_get_elements_by_tag_name_ns =
    vm.register_native_call(dom_element_get_elements_by_tag_name_ns)?;
  let call_element_get_elements_by_class_name =
    vm.register_native_call(dom_element_get_elements_by_class_name)?;
  let call_append_child = vm.register_native_call(dom_node_append_child)?;
  let call_clone_node = vm.register_native_call(dom_node_clone_node)?;
  let call_has_child_nodes = vm.register_native_call(dom_node_has_child_nodes)?;
  let call_parent_node = vm.register_native_call(dom_node_parent_node_getter)?;
  let call_parent_element = vm.register_native_call(dom_node_parent_element_getter)?;
  let call_first_child = vm.register_native_call(dom_node_first_child_getter)?;
  let call_last_child = vm.register_native_call(dom_node_last_child_getter)?;
  let call_previous_sibling = vm.register_native_call(dom_node_previous_sibling_getter)?;
  let call_next_sibling = vm.register_native_call(dom_node_next_sibling_getter)?;
  let call_node_type = vm.register_native_call(dom_node_node_type_getter)?;
  let call_node_name = vm.register_native_call(dom_node_node_name_getter)?;
  let call_node_value_get = vm.register_native_call(dom_node_node_value_getter)?;
  let call_node_value_set = vm.register_native_call(dom_node_node_value_setter)?;
  let call_is_connected = vm.register_native_call(dom_node_is_connected_getter)?;
  let call_tag_name = vm.register_native_call(dom_element_tag_name_getter)?;
  let call_id_get = vm.register_native_call(dom_element_id_getter)?;
  let call_id_set = vm.register_native_call(dom_element_id_setter)?;
  let call_class_name_get = vm.register_native_call(dom_element_class_name_getter)?;
  let call_class_name_set = vm.register_native_call(dom_element_class_name_setter)?;
  let call_set_attribute = vm.register_native_call(dom_element_set_attribute)?;
  let call_inner_html_get = vm.register_native_call(dom_element_inner_html_getter)?;
  let call_inner_html_set = vm.register_native_call(dom_element_inner_html_setter)?;
  let call_outer_html_get = vm.register_native_call(dom_element_outer_html_getter)?;
  let call_outer_html_set = vm.register_native_call(dom_element_outer_html_setter)?;
  let call_insert_adjacent_html = vm.register_native_call(dom_element_insert_adjacent_html)?;
  let call_insert_adjacent_element =
    vm.register_native_call(dom_element_insert_adjacent_element)?;
  let call_insert_adjacent_text = vm.register_native_call(dom_element_insert_adjacent_text)?;
  let call_current_script = vm.register_native_call(dom_document_current_script_getter)?;
  let call_cookie_get = vm.register_native_call(dom_document_cookie_getter)?;
  let call_cookie_set = vm.register_native_call(dom_document_cookie_setter)?;
  let call_document_write = vm.register_native_call(dom_document_write)?;
  let call_document_writeln = vm.register_native_call(dom_document_writeln)?;
  let call_text_content_get = vm.register_native_call(dom_node_text_content_getter)?;
  let call_text_content_set = vm.register_native_call(dom_node_text_content_setter)?;
  let call_class_list_get = vm.register_native_call(dom_element_class_list_getter)?;
  let call_dataset_get = vm.register_native_call(dom_element_dataset_getter)?;
  let call_style_get = vm.register_native_call(dom_element_style_getter)?;
  let call_token_list_contains = vm.register_native_call(dom_token_list_contains)?;
  let call_token_list_add = vm.register_native_call(dom_token_list_add)?;
  let call_token_list_remove = vm.register_native_call(dom_token_list_remove)?;
  let call_token_list_toggle = vm.register_native_call(dom_token_list_toggle)?;
  let call_token_list_replace = vm.register_native_call(dom_token_list_replace)?;
  let call_style_get_prop = vm.register_native_call(css_style_decl_get_property_value)?;
  let call_style_set_prop = vm.register_native_call(css_style_decl_set_property)?;
  let call_illegal_constructor = vm.register_native_call(dom_illegal_constructor)?;

  // Install methods/getters.
  install_method(
    &mut scope,
    proto_document,
    "createElement",
    call_create_element,
    1,
  )?;
  install_method(
    &mut scope,
    proto_document,
    "getElementById",
    call_get_element_by_id,
    1,
  )?;
  install_method(
    &mut scope,
    proto_document,
    "querySelector",
    call_query_selector,
    1,
  )?;
  install_method(
    &mut scope,
    proto_document,
    "getElementsByTagName",
    call_get_elements_by_tag_name,
    1,
  )?;
  install_method(
    &mut scope,
    proto_document,
    "getElementsByTagNameNS",
    call_get_elements_by_tag_name_ns,
    2,
  )?;
  install_method(
    &mut scope,
    proto_document,
    "getElementsByClassName",
    call_get_elements_by_class_name,
    1,
  )?;
  install_method(
    &mut scope,
    proto_document,
    "getElementsByName",
    call_get_elements_by_name,
    1,
  )?;
  install_method(&mut scope, proto_document, "write", call_document_write, 0)?;
  install_method(
    &mut scope,
    proto_document,
    "writeln",
    call_document_writeln,
    0,
  )?;
  install_method(&mut scope, proto_node, "appendChild", call_append_child, 1)?;
  install_method(&mut scope, proto_node, "cloneNode", call_clone_node, 1)?;
  install_method(
    &mut scope,
    proto_node,
    "hasChildNodes",
    call_has_child_nodes,
    0,
  )?;
  install_getter(&mut scope, proto_node, "parentNode", call_parent_node)?;
  install_getter(&mut scope, proto_node, "parentElement", call_parent_element)?;
  install_getter(&mut scope, proto_node, "firstChild", call_first_child)?;
  install_getter(&mut scope, proto_node, "lastChild", call_last_child)?;
  install_getter(
    &mut scope,
    proto_node,
    "previousSibling",
    call_previous_sibling,
  )?;
  install_getter(&mut scope, proto_node, "nextSibling", call_next_sibling)?;
  install_getter(&mut scope, proto_node, "nodeType", call_node_type)?;
  install_getter(&mut scope, proto_node, "nodeName", call_node_name)?;
  install_accessor(
    &mut scope,
    proto_node,
    "nodeValue",
    call_node_value_get,
    call_node_value_set,
  )?;
  install_getter(&mut scope, proto_node, "isConnected", call_is_connected)?;
  install_getter(&mut scope, proto_element, "tagName", call_tag_name)?;
  install_accessor(&mut scope, proto_element, "id", call_id_get, call_id_set)?;
  install_accessor(
    &mut scope,
    proto_element,
    "className",
    call_class_name_get,
    call_class_name_set,
  )?;
  install_method(
    &mut scope,
    proto_element,
    "setAttribute",
    call_set_attribute,
    2,
  )?;
  install_method(
    &mut scope,
    proto_element,
    "insertAdjacentHTML",
    call_insert_adjacent_html,
    2,
  )?;
  install_method(
    &mut scope,
    proto_element,
    "insertAdjacentElement",
    call_insert_adjacent_element,
    2,
  )?;
  install_method(
    &mut scope,
    proto_element,
    "insertAdjacentText",
    call_insert_adjacent_text,
    2,
  )?;
  install_method(
    &mut scope,
    proto_element,
    "getElementsByTagName",
    call_element_get_elements_by_tag_name,
    1,
  )?;
  install_method(
    &mut scope,
    proto_element,
    "getElementsByTagNameNS",
    call_element_get_elements_by_tag_name_ns,
    2,
  )?;
  install_method(
    &mut scope,
    proto_element,
    "getElementsByClassName",
    call_element_get_elements_by_class_name,
    1,
  )?;
  install_accessor(
    &mut scope,
    proto_element,
    "innerHTML",
    call_inner_html_get,
    call_inner_html_set,
  )?;
  install_accessor(
    &mut scope,
    proto_element,
    "outerHTML",
    call_outer_html_get,
    call_outer_html_set,
  )?;
  install_getter(
    &mut scope,
    proto_document,
    "currentScript",
    call_current_script,
  )?;
  install_accessor(
    &mut scope,
    proto_document,
    "cookie",
    call_cookie_get,
    call_cookie_set,
  )?;
  install_accessor(
    &mut scope,
    proto_node,
    "textContent",
    call_text_content_get,
    call_text_content_set,
  )?;
  install_getter(&mut scope, proto_element, "classList", call_class_list_get)?;
  install_getter(&mut scope, proto_element, "dataset", call_dataset_get)?;
  install_getter(&mut scope, proto_element, "style", call_style_get)?;

  install_method(
    &mut scope,
    proto_dom_token_list,
    "contains",
    call_token_list_contains,
    1,
  )?;
  install_method(
    &mut scope,
    proto_dom_token_list,
    "add",
    call_token_list_add,
    0,
  )?;
  install_method(
    &mut scope,
    proto_dom_token_list,
    "remove",
    call_token_list_remove,
    0,
  )?;
  install_method(
    &mut scope,
    proto_dom_token_list,
    "toggle",
    call_token_list_toggle,
    1,
  )?;
  install_method(
    &mut scope,
    proto_dom_token_list,
    "replace",
    call_token_list_replace,
    2,
  )?;
  install_method(
    &mut scope,
    proto_css_style_decl,
    "getPropertyValue",
    call_style_get_prop,
    1,
  )?;
  install_method(
    &mut scope,
    proto_css_style_decl,
    "setProperty",
    call_style_set_prop,
    2,
  )?;
  install_method(
    &mut scope,
    proto_html_collection,
    "item",
    call_html_collection_item,
    1,
  )?;

  let mut host = DomHost {
    dom: dom.clone(),
    current_script: current_script.clone(),
    document_url: None,
    cookie_fetcher: None,
    max_string_bytes,
    cookie_jar: CookieJar::new(),
    node_wrappers: HashMap::new(),
    class_list_wrappers: HashMap::new(),
    dataset_wrappers: HashMap::new(),
    style_wrappers: HashMap::new(),
    live_collections: Vec::new(),
    prototype_roots: Vec::new(),
    proto_node,
    proto_element,
    proto_document,
    proto_dom_token_list,
    proto_dom_string_map,
    proto_css_style_decl,
    proto_html_collection,
    dom_exception,
    type_error_prototype: realm.intrinsics().type_error_prototype(),
  };

  // Create the single `document` wrapper and install it on the global object.
  let document_id = dom.borrow().root();
  let document_wrapper = wrap_node(&mut host, &mut scope, document_id, DomKind::Document)?;
  scope.push_root(document_wrapper)?;

  let global = realm.global_object();

  // Interface constructors (non-constructable; used for prototype access / `instanceof` checks).
  let ctor_node = install_interface_constructor(
    &mut scope,
    realm,
    global,
    "Node",
    proto_node,
    call_illegal_constructor,
  )?;
  install_node_constants(&mut scope, ctor_node, proto_node)?;
  install_interface_constructor(
    &mut scope,
    realm,
    global,
    "Element",
    proto_element,
    call_illegal_constructor,
  )?;
  install_interface_constructor(
    &mut scope,
    realm,
    global,
    "Document",
    proto_document,
    call_illegal_constructor,
  )?;

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

  // Root prototype objects so `DomHost` can safely store handles without being GC-traced.
  host.prototype_roots = vec![
    scope.heap_mut().add_root(Value::Object(proto_node))?,
    scope.heap_mut().add_root(Value::Object(proto_element))?,
    scope.heap_mut().add_root(Value::Object(proto_document))?,
    scope
      .heap_mut()
      .add_root(Value::Object(proto_dom_token_list))?,
    scope
      .heap_mut()
      .add_root(Value::Object(proto_dom_string_map))?,
    scope
      .heap_mut()
      .add_root(Value::Object(proto_css_style_decl))?,
    scope
      .heap_mut()
      .add_root(Value::Object(proto_html_collection))?,
    scope
      .heap_mut()
      .add_root(Value::Object(host.dom_exception.constructor))?,
    scope
      .heap_mut()
      .add_root(Value::Object(host.dom_exception.prototype))?,
  ];

  vm.set_user_data(host);

  Ok(())
}

fn install_interface_constructor(
  scope: &mut Scope<'_>,
  realm: &Realm,
  global: GcObject,
  name: &str,
  proto: GcObject,
  call: NativeFunctionId,
) -> Result<GcObject, VmError> {
  let name_string = scope.alloc_string(name)?;
  let ctor = scope.alloc_native_function(call, None, name_string, 0)?;
  scope.push_root(Value::Object(ctor))?;

  scope
    .heap_mut()
    .object_set_prototype(ctor, Some(realm.intrinsics().function_prototype()))?;

  // ctor.prototype = proto
  let prototype_key = PropertyKey::from_string(scope.alloc_string("prototype")?);
  scope.define_property(
    ctor,
    prototype_key,
    data_desc(
      Value::Object(proto),
      /* writable */ false,
      /* enumerable */ false,
      /* configurable */ false,
    ),
  )?;

  // proto.constructor = ctor
  let constructor_key = PropertyKey::from_string(scope.alloc_string("constructor")?);
  scope.define_property(
    proto,
    constructor_key,
    data_desc(
      Value::Object(ctor),
      /* writable */ false,
      /* enumerable */ false,
      /* configurable */ false,
    ),
  )?;

  // global[name] = ctor
  let global_key = PropertyKey::from_string(scope.alloc_string(name)?);
  scope.define_property(global, global_key, method_desc(Value::Object(ctor)))?;

  Ok(ctor)
}

fn install_node_constants(scope: &mut Scope<'_>, ctor: GcObject, proto: GcObject) -> Result<(), VmError> {
  // https://dom.spec.whatwg.org/#interface-node
  //
  // WebIDL constants are:
  // - writable: false
  // - enumerable: true
  // - configurable: false
  fn define(scope: &mut Scope<'_>, obj: GcObject, name: &str, value: f64) -> Result<(), VmError> {
    let key = PropertyKey::from_string(scope.alloc_string(name)?);
    scope.define_property(
      obj,
      key,
      data_desc(
        Value::Number(value),
        /* writable */ false,
        /* enumerable */ true,
        /* configurable */ false,
      ),
    )?;
    Ok(())
  }

  for obj in [ctor, proto] {
    // Node types.
    define(scope, obj, "ELEMENT_NODE", 1.0)?;
    define(scope, obj, "ATTRIBUTE_NODE", 2.0)?;
    define(scope, obj, "TEXT_NODE", 3.0)?;
    define(scope, obj, "CDATA_SECTION_NODE", 4.0)?;
    define(scope, obj, "ENTITY_REFERENCE_NODE", 5.0)?;
    define(scope, obj, "ENTITY_NODE", 6.0)?;
    define(scope, obj, "PROCESSING_INSTRUCTION_NODE", 7.0)?;
    define(scope, obj, "COMMENT_NODE", 8.0)?;
    define(scope, obj, "DOCUMENT_NODE", 9.0)?;
    define(scope, obj, "DOCUMENT_TYPE_NODE", 10.0)?;
    define(scope, obj, "DOCUMENT_FRAGMENT_NODE", 11.0)?;
    define(scope, obj, "NOTATION_NODE", 12.0)?;

    // NodeDocumentPosition bits.
    define(scope, obj, "DOCUMENT_POSITION_DISCONNECTED", 1.0)?;
    define(scope, obj, "DOCUMENT_POSITION_PRECEDING", 2.0)?;
    define(scope, obj, "DOCUMENT_POSITION_FOLLOWING", 4.0)?;
    define(scope, obj, "DOCUMENT_POSITION_CONTAINS", 8.0)?;
    define(scope, obj, "DOCUMENT_POSITION_CONTAINED_BY", 16.0)?;
    define(scope, obj, "DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC", 32.0)?;
  }

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

fn install_accessor(
  scope: &mut Scope<'_>,
  proto: GcObject,
  name: &str,
  get_call: NativeFunctionId,
  set_call: NativeFunctionId,
) -> Result<(), VmError> {
  let get_name = format!("get {name}");
  let get_name_string = scope.alloc_string(&get_name)?;
  let get_func = scope.alloc_native_function(get_call, None, get_name_string, 0)?;
  scope.push_root(Value::Object(get_func))?;

  let set_name = format!("set {name}");
  let set_name_string = scope.alloc_string(&set_name)?;
  let set_func = scope.alloc_native_function(set_call, None, set_name_string, 1)?;
  scope.push_root(Value::Object(set_func))?;

  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  scope.define_property(
    proto,
    key,
    accessor_desc_get_set(Value::Object(get_func), Value::Object(set_func)),
  )?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  use crate::error::Error;
  use crate::html::document_write::with_active_streaming_parser;
  use crate::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};
  use crate::resource::FetchedResource;
  use selectors::context::QuirksMode;
  use std::cell::RefCell;
  use std::rc::Rc;
  use std::sync::{Arc, Mutex};
  use vm_js::{
    Heap, HeapLimits, Job, JsRuntime, PropertyKey, PropertyKind, Realm, RealmId, Scope, Value, Vm,
    VmError, VmHostHooks, VmOptions,
  };

  fn get_accessor_getter(heap: &Heap, obj: vm_js::GcObject, key: &PropertyKey) -> Option<Value> {
    heap
      .get_property(obj, key)
      .ok()
      .flatten()
      .and_then(|desc| match desc.kind {
        PropertyKind::Accessor { get, .. } => Some(get),
        PropertyKind::Data { .. } => None,
      })
  }

  fn get_accessor_setter(heap: &Heap, obj: vm_js::GcObject, key: &PropertyKey) -> Option<Value> {
    heap
      .get_property(obj, key)
      .ok()
      .flatten()
      .and_then(|desc| match desc.kind {
        PropertyKind::Accessor { set, .. } => Some(set),
        PropertyKind::Data { .. } => None,
      })
  }

  fn get_data_property_value(heap: &Heap, obj: vm_js::GcObject, key: &PropertyKey) -> Option<Value> {
    heap
      .get_property(obj, key)
      .ok()
      .flatten()
      .and_then(|desc| match desc.kind {
        PropertyKind::Data { value, .. } => Some(value),
        PropertyKind::Accessor { .. } => None,
      })
  }

  #[test]
  fn dom_bindings_smoke() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());

    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));

    install_dom_bindings(
      &mut vm,
      &mut heap,
      &realm,
      dom.clone(),
      current_script.clone(),
    )?;

    let mut scope = heap.scope();

    // Fetch globalThis.document.
    let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
    let document_val = scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key_document)?
      .expect("globalThis.document should be defined");
    let document_obj = match document_val {
      Value::Object(o) => o,
      _ => panic!("document should be an object"),
    };

    // document.hasChildNodes() should be false on a new empty document.
    let key_has_child_nodes = PropertyKey::from_string(scope.alloc_string("hasChildNodes")?);
    let has_child_nodes = get_data_property_value(scope.heap(), document_obj, &key_has_child_nodes)
      .expect("document.hasChildNodes should exist");
    let has_children = vm.call_without_host(&mut scope, has_child_nodes, document_val, &[])?;
    assert_eq!(has_children, Value::Bool(false));

    // document.createElement("div") -> Element wrapper.
    let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
    let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
      .expect("document.createElement should exist");

    let tag_div = Value::String(scope.alloc_string("div")?);
    let el_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
    let el_obj = match el_val {
      Value::Object(o) => o,
      _ => panic!("createElement should return an object"),
    };

    // Identity/shape getters.
    let key_is_connected = PropertyKey::from_string(scope.alloc_string("isConnected")?);
    let is_connected_get = get_accessor_getter(scope.heap(), el_obj, &key_is_connected)
      .expect("isConnected getter should exist");
    let key_node_name = PropertyKey::from_string(scope.alloc_string("nodeName")?);
    let node_name_get = get_accessor_getter(scope.heap(), el_obj, &key_node_name)
      .expect("nodeName getter should exist");
    let key_node_value = PropertyKey::from_string(scope.alloc_string("nodeValue")?);
    let node_value_get = get_accessor_getter(scope.heap(), el_obj, &key_node_value)
      .expect("nodeValue getter should exist");
    let node_value_set = get_accessor_setter(scope.heap(), el_obj, &key_node_value)
      .expect("nodeValue setter should exist");
    let key_tag_name = PropertyKey::from_string(scope.alloc_string("tagName")?);
    let tag_name_get =
      get_accessor_getter(scope.heap(), el_obj, &key_tag_name).expect("tagName getter should exist");
    let key_id_prop = PropertyKey::from_string(scope.alloc_string("id")?);
    let id_get = get_accessor_getter(scope.heap(), el_obj, &key_id_prop).expect("id getter exists");
    let key_class_name = PropertyKey::from_string(scope.alloc_string("className")?);
    let class_name_get =
      get_accessor_getter(scope.heap(), el_obj, &key_class_name).expect("className getter exists");
    let class_name_set =
      get_accessor_setter(scope.heap(), el_obj, &key_class_name).expect("className setter exists");
    let key_text_content = PropertyKey::from_string(scope.alloc_string("textContent")?);
    let text_content_get = get_accessor_getter(scope.heap(), el_obj, &key_text_content)
      .expect("textContent getter exists");
    let text_content_set = get_accessor_setter(scope.heap(), el_obj, &key_text_content)
      .expect("textContent setter exists");

    // A freshly created element is not yet connected to the document tree.
    assert_eq!(
      vm.call_without_host(&mut scope, is_connected_get, el_val, &[])?,
      Value::Bool(false)
    );

    let node_name = vm.call_without_host(&mut scope, node_name_get, document_val, &[])?;
    let Value::String(node_name_str) = node_name else {
      panic!("expected nodeName string");
    };
    assert_eq!(
      scope.heap().get_string(node_name_str)?.to_utf8_lossy(),
      "#document"
    );

    let node_name = vm.call_without_host(&mut scope, node_name_get, el_val, &[])?;
    let Value::String(node_name_str) = node_name else {
      panic!("expected nodeName string");
    };
    assert_eq!(
      scope.heap().get_string(node_name_str)?.to_utf8_lossy(),
      "DIV"
    );

    let tag_name = vm.call_without_host(&mut scope, tag_name_get, el_val, &[])?;
    let Value::String(tag_name_str) = tag_name else {
      panic!("expected tagName string");
    };
    assert_eq!(
      scope.heap().get_string(tag_name_str)?.to_utf8_lossy(),
      "DIV"
    );

    assert!(matches!(
      vm.call_without_host(&mut scope, node_value_get, document_val, &[])?,
      Value::Null
    ));
    assert!(matches!(
      vm.call_without_host(&mut scope, node_value_get, el_val, &[])?,
      Value::Null
    ));

    let id = vm.call_without_host(&mut scope, id_get, el_val, &[])?;
    let Value::String(id_str) = id else {
      panic!("expected id string");
    };
    assert!(scope.heap().get_string(id_str)?.to_utf8_lossy().is_empty());

    let class_name = vm.call_without_host(&mut scope, class_name_get, el_val, &[])?;
    let Value::String(class_name_str) = class_name else {
      panic!("expected className string");
    };
    assert!(scope
      .heap()
      .get_string(class_name_str)?
      .to_utf8_lossy()
      .is_empty());

    // Element wrappers should also expose Node.hasChildNodes.
    let el_has_children = vm.call_without_host(&mut scope, has_child_nodes, el_val, &[])?;
    assert_eq!(el_has_children, Value::Bool(false));

    // el.setAttribute("id", "foo")
    let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
    let set_attribute =
      get_data_property_value(scope.heap(), el_obj, &key_set_attribute).expect("setAttribute exists");
    let arg_id = Value::String(scope.alloc_string("id")?);
    let arg_foo = Value::String(scope.alloc_string("foo")?);
    let r = vm.call_without_host(&mut scope, set_attribute, el_val, &[arg_id, arg_foo])?;
    assert!(matches!(r, Value::Undefined));

    let id = vm.call_without_host(&mut scope, id_get, el_val, &[])?;
    let Value::String(id_str) = id else {
      panic!("expected id string");
    };
    assert_eq!(scope.heap().get_string(id_str)?.to_utf8_lossy(), "foo");

    // document.appendChild(el)
    let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
    let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
      .expect("appendChild exists");
    let appended = vm.call_without_host(&mut scope, append_child, document_val, &[el_val])?;
    assert_eq!(appended, el_val, "appendChild should return the child");

    assert_eq!(
      vm.call_without_host(&mut scope, is_connected_get, el_val, &[])?,
      Value::Bool(true)
    );

    // document.hasChildNodes() should now return true.
    let doc_has_children = vm.call_without_host(&mut scope, has_child_nodes, document_val, &[])?;
    assert_eq!(doc_has_children, Value::Bool(true));

    // className setter updates the backing attribute.
    let arg_class = Value::String(scope.alloc_string("a b")?);
    vm.call_without_host(&mut scope, class_name_set, el_val, &[arg_class])?;
    let class_name = vm.call_without_host(&mut scope, class_name_get, el_val, &[])?;
    let Value::String(class_name_str) = class_name else {
      panic!("expected className string");
    };
    assert_eq!(
      scope.heap().get_string(class_name_str)?.to_utf8_lossy(),
      "a b"
    );

    // Basic Node navigation getters.
    let key_parent_node = PropertyKey::from_string(scope.alloc_string("parentNode")?);
    let parent_node_get = get_accessor_getter(scope.heap(), el_obj, &key_parent_node)
      .expect("parentNode getter should exist");
    let key_parent_element = PropertyKey::from_string(scope.alloc_string("parentElement")?);
    let parent_element_get = get_accessor_getter(scope.heap(), el_obj, &key_parent_element)
      .expect("parentElement getter should exist");
    let key_first_child = PropertyKey::from_string(scope.alloc_string("firstChild")?);
    let first_child_get = get_accessor_getter(scope.heap(), el_obj, &key_first_child)
      .expect("firstChild getter should exist");
    let key_last_child = PropertyKey::from_string(scope.alloc_string("lastChild")?);
    let last_child_get = get_accessor_getter(scope.heap(), el_obj, &key_last_child)
      .expect("lastChild getter should exist");
    let key_previous_sibling = PropertyKey::from_string(scope.alloc_string("previousSibling")?);
    let previous_sibling_get = get_accessor_getter(scope.heap(), el_obj, &key_previous_sibling)
      .expect("previousSibling getter should exist");
    let key_next_sibling = PropertyKey::from_string(scope.alloc_string("nextSibling")?);
    let next_sibling_get = get_accessor_getter(scope.heap(), el_obj, &key_next_sibling)
      .expect("nextSibling getter should exist");
    let key_node_type = PropertyKey::from_string(scope.alloc_string("nodeType")?);
    let node_type_get = get_accessor_getter(scope.heap(), el_obj, &key_node_type)
      .expect("nodeType getter should exist");

    assert_eq!(
      vm.call_without_host(&mut scope, parent_node_get, document_val, &[])?,
      Value::Null
    );
    assert_eq!(
      vm.call_without_host(&mut scope, parent_node_get, el_val, &[])?,
      document_val
    );
    assert_eq!(
      vm.call_without_host(&mut scope, parent_element_get, el_val, &[])?,
      Value::Null
    );
    assert_eq!(
      vm.call_without_host(&mut scope, first_child_get, document_val, &[])?,
      el_val
    );
    assert_eq!(
      vm.call_without_host(&mut scope, last_child_get, document_val, &[])?,
      el_val
    );
    assert_eq!(
      vm.call_without_host(&mut scope, previous_sibling_get, el_val, &[])?,
      Value::Null
    );
    assert_eq!(
      vm.call_without_host(&mut scope, next_sibling_get, el_val, &[])?,
      Value::Null
    );
    assert_eq!(
      vm.call_without_host(&mut scope, node_type_get, document_val, &[])?,
      Value::Number(9.0)
    );
    assert_eq!(
      vm.call_without_host(&mut scope, node_type_get, el_val, &[])?,
      Value::Number(1.0)
    );

    // Add two child nodes under `<div id="foo">` so we can validate sibling relationships.
    let tag_span = Value::String(scope.alloc_string("span")?);
    let child1 = vm.call_without_host(&mut scope, create_element, document_val, &[tag_span])?;
    let Value::Object(child1_obj) = child1 else {
      panic!("createElement should return an object");
    };
    let child2 = vm.call_without_host(&mut scope, create_element, document_val, &[tag_span])?;
    let Value::Object(child2_obj) = child2 else {
      panic!("createElement should return an object");
    };
    vm.call_without_host(&mut scope, append_child, el_val, &[child1])?;
    vm.call_without_host(&mut scope, append_child, el_val, &[child2])?;

    assert_eq!(
      vm.call_without_host(&mut scope, first_child_get, el_val, &[])?,
      child1
    );
    assert_eq!(
      vm.call_without_host(&mut scope, last_child_get, el_val, &[])?,
      child2
    );
    assert_eq!(
      vm.call_without_host(&mut scope, parent_node_get, child1, &[])?,
      el_val
    );
    assert_eq!(
      vm.call_without_host(&mut scope, parent_element_get, child1, &[])?,
      el_val
    );
    assert_eq!(
      vm.call_without_host(&mut scope, node_type_get, child1, &[])?,
      Value::Number(1.0)
    );

    let next_sibling_get = get_accessor_getter(scope.heap(), child1_obj, &key_next_sibling)
      .expect("nextSibling getter should exist");
    let previous_sibling_get = get_accessor_getter(scope.heap(), child2_obj, &key_previous_sibling)
      .expect("previousSibling getter should exist");
    assert_eq!(
      vm.call_without_host(&mut scope, next_sibling_get, child1, &[])?,
      child2
    );
    assert_eq!(
      vm.call_without_host(&mut scope, previous_sibling_get, child2, &[])?,
      child1
    );

    // nodeValue behavior for Text nodes.
    let arg_hello = Value::String(scope.alloc_string("hello")?);
    vm.call_without_host(&mut scope, text_content_set, child1, &[arg_hello])?;

    let text_node = vm.call_without_host(&mut scope, first_child_get, child1, &[])?;
    assert_eq!(
      vm.call_without_host(&mut scope, node_type_get, text_node, &[])?,
      Value::Number(3.0)
    );

    let text_node_name = vm.call_without_host(&mut scope, node_name_get, text_node, &[])?;
    let Value::String(text_node_name_str) = text_node_name else {
      panic!("expected nodeName string");
    };
    assert_eq!(
      scope.heap().get_string(text_node_name_str)?.to_utf8_lossy(),
      "#text"
    );

    let text_node_value = vm.call_without_host(&mut scope, node_value_get, text_node, &[])?;
    let Value::String(text_node_value_str) = text_node_value else {
      panic!("expected nodeValue string");
    };
    assert_eq!(
      scope
        .heap()
        .get_string(text_node_value_str)?
        .to_utf8_lossy(),
      "hello"
    );

    let arg_bye = Value::String(scope.alloc_string("bye")?);
    vm.call_without_host(&mut scope, node_value_set, text_node, &[arg_bye])?;

    let text_node_value = vm.call_without_host(&mut scope, node_value_get, text_node, &[])?;
    let Value::String(text_node_value_str) = text_node_value else {
      panic!("expected nodeValue string");
    };
    assert_eq!(
      scope
        .heap()
        .get_string(text_node_value_str)?
        .to_utf8_lossy(),
      "bye"
    );

    let child_text = vm.call_without_host(&mut scope, text_content_get, child1, &[])?;
    let Value::String(child_text_str) = child_text else {
      panic!("expected textContent string");
    };
    assert_eq!(
      scope.heap().get_string(child_text_str)?.to_utf8_lossy(),
      "bye"
    );

    // Inert template contents: `<template>` should not expose children via Node navigation.
    let tag_template = Value::String(scope.alloc_string("template")?);
    let template = vm.call_without_host(&mut scope, create_element, document_val, &[tag_template])?;
    vm.call_without_host(&mut scope, append_child, el_val, &[template])?;

    let arg_inert = Value::String(scope.alloc_string("INERT")?);
    vm.call_without_host(&mut scope, text_content_set, template, &[arg_inert])?;

    let template_has_children = vm.call_without_host(&mut scope, has_child_nodes, template, &[])?;
    assert_eq!(template_has_children, Value::Bool(false));
    assert_eq!(
      vm.call_without_host(&mut scope, first_child_get, template, &[])?,
      Value::Null
    );

    // Validate DOM mutation.
    let root = dom.borrow().root();
    let found = dom
      .borrow()
      .get_element_by_id("foo")
      .expect("id should be set");
    assert_eq!(dom.borrow().parent(found).unwrap(), Some(root));
    assert!(dom
      .borrow()
      .children(root)
      .unwrap()
      .iter()
      .any(|&c| c == found));

    // document.getElementById("foo") returns wrapper identity.
    let key_get_element_by_id = PropertyKey::from_string(scope.alloc_string("getElementById")?);
    let get_element_by_id =
      get_data_property_value(scope.heap(), document_obj, &key_get_element_by_id)
        .expect("getElementById exists");
    let arg_foo2 = Value::String(scope.alloc_string("foo")?);
    let got = vm.call_without_host(&mut scope, get_element_by_id, document_val, &[arg_foo2])?;
    assert_eq!(got, el_val, "wrapper identity should be preserved");

    let arg_nope = Value::String(scope.alloc_string("nope")?);
    let missing = vm.call_without_host(&mut scope, get_element_by_id, document_val, &[arg_nope])?;
    assert!(matches!(missing, Value::Null));

    // document.querySelector invalid selector throws a DOMException-like object with name == "SyntaxError".
    let key_query_selector = PropertyKey::from_string(scope.alloc_string("querySelector")?);
    let query_selector = get_data_property_value(scope.heap(), document_obj, &key_query_selector)
      .expect("querySelector exists");
    let arg_bad = Value::String(scope.alloc_string("???")?);
    let thrown = match vm.call_without_host(&mut scope, query_selector, document_val, &[arg_bad]) {
      Ok(_) => panic!("expected querySelector to throw"),
      Err(err) => match err.thrown_value() {
        Some(v) => v,
        None => return Err(err),
      },
    };
    let thrown_obj = match thrown {
      Value::Object(o) => o,
      _ => panic!("thrown value should be an object"),
    };
    let key_name = PropertyKey::from_string(scope.alloc_string("name")?);
    let name_val = get_data_property_value(scope.heap(), thrown_obj, &key_name)
      .expect("thrown object should have .name");
    let name_str = match name_val {
      Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
      _ => panic!(".name should be a string"),
    };
    assert_eq!(name_str, "SyntaxError");

    // document.currentScript getter returns null by default, then a wrapper when set.
    let key_current_script = PropertyKey::from_string(scope.alloc_string("currentScript")?);
    let current_script_get = get_accessor_getter(scope.heap(), document_obj, &key_current_script)
      .expect("currentScript getter should exist");
    let cs0 = vm.call_without_host(&mut scope, current_script_get, document_val, &[])?;
    assert!(matches!(cs0, Value::Null));

    // Create a <script> node and set CurrentScriptState.
    let script_id = dom.borrow_mut().create_element("script", "");
    // Document nodes only allow a single element child; append under the existing <div id="foo">.
    dom.borrow_mut().append_child(found, script_id).unwrap();
    current_script.borrow_mut().current_script = Some(script_id);

    // The <div id="foo"> wrapper should observe the new child.
    let el_has_children = vm.call_without_host(&mut scope, has_child_nodes, el_val, &[])?;
    assert_eq!(el_has_children, Value::Bool(true));

    let cs1 = vm.call_without_host(&mut scope, current_script_get, document_val, &[])?;
    assert!(matches!(cs1, Value::Object(_)));

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn dom_bindings_rejects_strings_over_max_string_bytes() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));

    // Use a tiny conversion limit so multi-byte strings can exceed it even though the UTF-16 input
    // is short.
    install_dom_bindings_with_limits(
      &mut vm,
      &mut heap,
      &realm,
      dom.clone(),
      current_script.clone(),
      5,
    )?;

    let mut scope = heap.scope();
    let msg: Result<String, VmError> = (|| {
      let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
      let document_val = scope
        .heap()
        .object_get_own_data_property_value(realm.global_object(), &key_document)?
        .expect("globalThis.document should be defined");

      let document_obj = match document_val {
        Value::Object(o) => o,
        _ => panic!("document should be an object"),
      };

      let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
      let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
        .expect("document.createElement should exist");

      // "ééé" is 3 UTF-16 code units but 6 UTF-8 bytes.
      let tag = Value::String(scope.alloc_string("ééé")?);
      let err = vm
        .call_without_host(&mut scope, create_element, document_val, &[tag])
        .expect_err("expected createElement to throw");

      let thrown = match err.thrown_value() {
        Some(v) => v,
        None => return Err(err),
      };

      let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
      let message = match thrown {
        Value::Object(obj) => get_data_property_value(scope.heap(), obj, &message_key)
          .expect("thrown error should have message"),
        other => panic!("expected error object, got {other:?}"),
      };
      let Value::String(message_str) = message else {
        panic!("expected message string, got {message:?}");
      };
      Ok(
        scope
          .heap()
          .get_string(message_str)?
          .to_utf8_lossy()
          .to_string(),
      )
    })();

    // Ensure teardown runs even if assertions fail, otherwise `Realm` will panic in Drop while the
    // test is already unwinding.
    drop(scope);
    realm.teardown(&mut heap);

    let msg = msg?;
    assert!(
      msg.contains("max_string_bytes"),
      "unexpected error message: {msg}"
    );
    Ok(())
  }

  #[test]
  fn node_text_content_getter_and_setter() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());

    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;

    let mut scope = heap.scope();
    let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
    let document_val = scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key_document)?
      .expect("globalThis.document should exist");
    let document_obj = match document_val {
      Value::Object(o) => o,
      _ => panic!("document should be an object"),
    };

    // Create an element wrapper and attach it to the document so `getElementById` can find it.
    let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
    let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
      .expect("document.createElement should exist");
    let tag_div = Value::String(scope.alloc_string("div")?);
    let el_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
    let el_obj = match el_val {
      Value::Object(o) => o,
      _ => panic!("createElement should return an object"),
    };

    let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
    let set_attribute =
      get_data_property_value(scope.heap(), el_obj, &key_set_attribute).expect("setAttribute exists");
    let arg_id = Value::String(scope.alloc_string("id")?);
    let arg_root = Value::String(scope.alloc_string("root")?);
    vm.call_without_host(&mut scope, set_attribute, el_val, &[arg_id, arg_root])?;

    let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
    let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
      .expect("appendChild exists");
    vm.call_without_host(&mut scope, append_child, document_val, &[el_val])?;

    let root_id = dom
      .borrow()
      .get_element_by_id("root")
      .expect("missing #root node id");

    // Build a nested subtree:
    // <div id=root>
    //   "Hello "
    //   <span>"World"</span>
    //   <template>"INERT"</template>  (should be skipped)
    //   "!"
    // </div>
    {
      let mut d = dom.borrow_mut();
      let text_hello = d.create_text("Hello ");
      d.append_child(root_id, text_hello).unwrap();

      let span = d.create_element("span", "");
      d.append_child(root_id, span).unwrap();
      let text_world = d.create_text("World");
      d.append_child(span, text_world).unwrap();

      let template = d.create_element("template", "");
      d.append_child(root_id, template).unwrap();
      let inert = d.create_text("INERT");
      d.append_child(template, inert).unwrap();

      let text_bang = d.create_text("!");
      d.append_child(root_id, text_bang).unwrap();
    }

    let key_text_content = PropertyKey::from_string(scope.alloc_string("textContent")?);
    let text_content_get = get_accessor_getter(scope.heap(), el_obj, &key_text_content)
      .expect("textContent getter exists");

    // DOM: `Document.textContent` is `null`.
    let doc_text = vm.call_without_host(&mut scope, text_content_get, document_val, &[])?;
    assert!(matches!(doc_text, Value::Null));

    let got = vm.call_without_host(&mut scope, text_content_get, el_val, &[])?;
    let got_s = match got {
      Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
      _ => panic!("textContent getter should return string"),
    };
    assert_eq!(got_s, "Hello World!");

    let text_content_set = get_accessor_setter(scope.heap(), el_obj, &key_text_content)
      .expect("textContent setter exists");

    // DOM: setting `Document.textContent` is a no-op.
    let arg_ignored = Value::String(scope.alloc_string("ignored")?);
    vm.call_without_host(&mut scope, text_content_set, document_val, &[arg_ignored])?;
    assert!(dom.borrow().get_element_by_id("root").is_some());

    let arg_replaced = Value::String(scope.alloc_string("replaced")?);
    let r = vm.call_without_host(&mut scope, text_content_set, el_val, &[arg_replaced])?;
    assert!(matches!(r, Value::Undefined));

    let children = dom.borrow().children(root_id).unwrap().to_vec();
    assert_eq!(children.len(), 1);
    let only_child = children[0];
    match &dom.borrow().node(only_child).kind {
      NodeKind::Text { content } => assert_eq!(content, "replaced"),
      other => panic!("expected a single Text node child, got {other:?}"),
    }

    // Setting to empty clears children.
    let arg_empty = Value::String(scope.alloc_string("")?);
    vm.call_without_host(&mut scope, text_content_set, el_val, &[arg_empty])?;
    assert!(dom.borrow().children(root_id).unwrap().is_empty());

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn node_text_content_setter_updates_node_iterator_pre_remove_steps() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;
 
    let mut scope = heap.scope();
    let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
    let document_val = scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key_document)?
      .expect("globalThis.document should exist");
    let Value::Object(document_obj) = document_val else {
      panic!("document should be an object");
    };
 
    let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
    let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
      .expect("document.createElement should exist");
 
    let tag_parent = Value::String(scope.alloc_string("div")?);
    let parent_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_parent])?;
    let Value::Object(parent_obj) = parent_val else {
      panic!("createElement should return an object");
    };
 
    let tag_a = Value::String(scope.alloc_string("a")?);
    let a_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_a])?;
    let tag_b = Value::String(scope.alloc_string("b")?);
    let b_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_b])?;
 
    let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
    let append_child = get_data_property_value(scope.heap(), parent_obj, &key_append_child)
      .expect("appendChild exists on nodes");
 
    // parent.appendChild(a); parent.appendChild(b)
    vm.call_without_host(&mut scope, append_child, parent_val, &[a_val])?;
    vm.call_without_host(&mut scope, append_child, parent_val, &[b_val])?;
 
    // Create a NodeIterator rooted at `parent` and point it at the soon-to-be-removed child.
    let host = host_mut(&mut vm)?;
    let (_kind, parent_id) = wrapper_meta(&mut scope, host, parent_val)?;
    let (_kind, a_id) = wrapper_meta(&mut scope, host, a_val)?;
    let node_iter = dom.borrow_mut().create_node_iterator(parent_id);
    dom
      .borrow_mut()
      .set_node_iterator_reference_and_pointer(node_iter, a_id, true);
 
    let key_text_content = PropertyKey::from_string(scope.alloc_string("textContent")?);
    let text_content_set = get_accessor_setter(scope.heap(), parent_obj, &key_text_content)
      .expect("textContent setter exists");
    let arg_replaced = Value::String(scope.alloc_string("x")?);
    vm.call_without_host(&mut scope, text_content_set, parent_val, &[arg_replaced])?;
 
    // If the legacy setter bypasses dom2 removal APIs, NodeIterator pre-removing steps won't run
    // and the iterator will keep pointing at the removed child. It should instead be updated to
    // point at the root with pointer_before_reference=false.
    let doc = dom.borrow();
    assert_eq!(doc.node_iterator_reference(node_iter), Some(parent_id));
    assert_eq!(doc.node_iterator_pointer_before_reference(node_iter), Some(false));
 
    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }
 
  #[test]
  fn node_text_content_setter_for_comment_queues_character_data_mutation_record() -> Result<(), VmError> {
    use crate::dom2::{MutationObserverInit, MutationRecordType};
 
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;
 
    let comment_id = dom.borrow_mut().create_comment("hi");
    dom
      .borrow_mut()
      .mutation_observer_observe(
        1,
        comment_id,
        MutationObserverInit {
          character_data: true,
          character_data_old_value: true,
          ..MutationObserverInit::default()
        },
      )
      .expect("observe");
 
    let mut scope = heap.scope();
    let host = host_mut(&mut vm)?;
    let comment_val = wrap_node(host, &mut scope, comment_id, DomKind::Node)?;
    let Value::Object(comment_obj) = comment_val else {
      panic!("expected comment wrapper object");
    };
 
    let key_text_content = PropertyKey::from_string(scope.alloc_string("textContent")?);
    let text_content_set = get_accessor_setter(scope.heap(), comment_obj, &key_text_content)
      .expect("textContent setter exists");
 
    let arg_replaced = Value::String(scope.alloc_string("bye")?);
    vm.call_without_host(&mut scope, text_content_set, comment_val, &[arg_replaced])?;
 
    let records = dom.borrow_mut().mutation_observer_take_records(1);
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.type_, MutationRecordType::CharacterData);
    assert_eq!(record.target, comment_id);
    assert_eq!(record.old_value.as_deref(), Some("hi"));
 
    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }
 
  #[test]
  fn node_text_content_setter_for_processing_instruction_queues_character_data_mutation_record() -> Result<(), VmError> {
    use crate::dom2::{MutationObserverInit, MutationRecordType};
 
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;
 
    // dom2 doesn't currently expose a public ProcessingInstruction constructor; build one by
    // repurposing a detached node for the purposes of this test.
    let pi_id = dom.borrow_mut().create_comment("");
    dom.borrow_mut().node_mut(pi_id).kind = NodeKind::ProcessingInstruction {
      target: "x".to_string(),
      data: "hi".to_string(),
    };
 
    dom
      .borrow_mut()
      .mutation_observer_observe(
        1,
        pi_id,
        MutationObserverInit {
          character_data: true,
          character_data_old_value: true,
          ..MutationObserverInit::default()
        },
      )
      .expect("observe");
 
    let mut scope = heap.scope();
    let host = host_mut(&mut vm)?;
    let pi_val = wrap_node(host, &mut scope, pi_id, DomKind::Node)?;
    let Value::Object(pi_obj) = pi_val else {
      panic!("expected PI wrapper object");
    };
 
    let key_text_content = PropertyKey::from_string(scope.alloc_string("textContent")?);
    let text_content_set =
      get_accessor_setter(scope.heap(), pi_obj, &key_text_content).expect("textContent setter exists");
 
    let arg_replaced = Value::String(scope.alloc_string("bye")?);
    vm.call_without_host(&mut scope, text_content_set, pi_val, &[arg_replaced])?;
 
    let records = dom.borrow_mut().mutation_observer_take_records(1);
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.type_, MutationRecordType::CharacterData);
    assert_eq!(record.target, pi_id);
    assert_eq!(record.old_value.as_deref(), Some("hi"));
 
    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }
 
  #[test]
  fn element_inner_html_and_outer_html_round_trip() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());

    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;

    struct Recorded {
      inner_html_initial: String,
      inner_html_round_trip: String,
      outer_html_round_trip: String,
      span_collection_len_initial: f64,
      span_collection_len_after_insert: f64,
      span_collection_0_identity_preserved: bool,
      span_collection_len_after_script: f64,
      span_node_type: f64,
      tail_node_type: f64,
      span_text: String,
      tail_text: String,
      div_text: String,
      child_identity_preserved: bool,
      script_already_started: bool,
      old_wrapper_disconnected: bool,
      replaced_parent_is_body: bool,
      replaced_text: String,
      target_node_detached: bool,
    }

    let mut scope = heap.scope();
    let recorded: Result<Recorded, VmError> = (|| {
      let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
      let document_val = scope
        .heap()
        .object_get_own_data_property_value(realm.global_object(), &key_document)?
        .ok_or(VmError::InvariantViolation(
          "globalThis.document should be defined",
        ))?;
      let Value::Object(document_obj) = document_val else {
        return Err(VmError::InvariantViolation("document should be an object"));
      };

      let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
      let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
        .ok_or(VmError::InvariantViolation(
        "document.createElement should exist",
      ))?;

      let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
      let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
        .ok_or(VmError::InvariantViolation("appendChild should exist"))?;

      // Document nodes can only have one element child; use a `<body>` element as the root parent so
      // `outerHTML` replacement does not attempt to modify a direct child of the `Document`.
      let tag_body = Value::String(scope.alloc_string("body")?);
      let body_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_body])?;
      let Value::Object(_body_obj) = body_val else {
        return Err(VmError::InvariantViolation(
          "createElement(body) should return an object",
        ));
      };
      vm.call_without_host(&mut scope, append_child, document_val, &[body_val])?;

      let tag_div = Value::String(scope.alloc_string("div")?);
      let div_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
      let Value::Object(div_obj) = div_val else {
        return Err(VmError::InvariantViolation(
          "createElement(div) should return an object",
        ));
      };

      let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
      let set_attribute = get_data_property_value(scope.heap(), div_obj, &key_set_attribute)
        .ok_or(VmError::InvariantViolation("setAttribute should exist"))?;
      let arg_id = Value::String(scope.alloc_string("id")?);
      let arg_target = Value::String(scope.alloc_string("target")?);
      vm.call_without_host(&mut scope, set_attribute, div_val, &[arg_id, arg_target])?;

      vm.call_without_host(&mut scope, append_child, body_val, &[div_val])?;

      // Create a live HTMLCollection before inserting any matching elements so we can ensure updates
      // occur when `innerHTML` mutates the DOM.
      let key_get_elements_by_tag_name =
        PropertyKey::from_string(scope.alloc_string("getElementsByTagName")?);
      let get_elements_by_tag_name =
        get_data_property_value(scope.heap(), document_obj, &key_get_elements_by_tag_name).ok_or(
          VmError::InvariantViolation("document.getElementsByTagName should exist"),
        )?;
      let arg_span = Value::String(scope.alloc_string("span")?);
      let span_coll_val = vm.call_without_host(
        &mut scope,
        get_elements_by_tag_name,
        document_val,
        &[arg_span],
      )?;
      let Value::Object(span_coll_obj) = span_coll_val else {
        return Err(VmError::InvariantViolation(
          "expected an HTMLCollection object",
        ));
      };
      let key_length = PropertyKey::from_string(scope.alloc_string("length")?);
      let span_collection_len_initial =
        get_data_property_value(scope.heap(), span_coll_obj, &key_length).ok_or(
          VmError::InvariantViolation("HTMLCollection.length should exist"),
        )?;
      let Value::Number(span_collection_len_initial) = span_collection_len_initial else {
        return Err(VmError::InvariantViolation(
          "HTMLCollection.length should be a number",
        ));
      };

      let (body_id, target_id) = {
        let dom_ref = dom.borrow();
        let body_id = dom_ref
          .document_element()
          .ok_or(VmError::InvariantViolation("missing body element"))?;
        let target_id = dom_ref
          .get_element_by_id("target")
          .ok_or(VmError::InvariantViolation("missing #target element"))?;
        (body_id, target_id)
      };

      let key_inner_html = PropertyKey::from_string(scope.alloc_string("innerHTML")?);
      let inner_html_get = get_accessor_getter(scope.heap(), div_obj, &key_inner_html)
        .ok_or(VmError::InvariantViolation("innerHTML getter should exist"))?;
      let inner_html_set = get_accessor_setter(scope.heap(), div_obj, &key_inner_html)
        .ok_or(VmError::InvariantViolation("innerHTML setter should exist"))?;

      let inner_html_initial = vm.call_without_host(&mut scope, inner_html_get, div_val, &[])?;
      let Value::String(inner_html_initial) = inner_html_initial else {
        return Err(VmError::InvariantViolation(
          "innerHTML getter should return a string",
        ));
      };
      let inner_html_initial = scope
        .heap()
        .get_string(inner_html_initial)?
        .to_utf8_lossy()
        .to_string();

      let arg_html = Value::String(scope.alloc_string("<span id=child>hi</span>tail")?);
      vm.call_without_host(&mut scope, inner_html_set, div_val, &[arg_html])?;

      let span_collection_len_after_insert =
        get_data_property_value(scope.heap(), span_coll_obj, &key_length).ok_or(
          VmError::InvariantViolation("HTMLCollection.length should exist after innerHTML set"),
        )?;
      let Value::Number(span_collection_len_after_insert) = span_collection_len_after_insert else {
        return Err(VmError::InvariantViolation(
          "HTMLCollection.length should be a number after innerHTML set",
        ));
      };

      let inner_html_round_trip = vm.call_without_host(&mut scope, inner_html_get, div_val, &[])?;
      let Value::String(inner_html_round_trip) = inner_html_round_trip else {
        return Err(VmError::InvariantViolation(
          "innerHTML getter should return a string",
        ));
      };
      let inner_html_round_trip = scope
        .heap()
        .get_string(inner_html_round_trip)?
        .to_utf8_lossy()
        .to_string();

      // Validate Node navigation for the newly inserted children.
      let key_first_child = PropertyKey::from_string(scope.alloc_string("firstChild")?);
      let first_child_get = get_accessor_getter(scope.heap(), div_obj, &key_first_child).ok_or(
        VmError::InvariantViolation("firstChild getter should exist"),
      )?;
      let key_next_sibling = PropertyKey::from_string(scope.alloc_string("nextSibling")?);
      let next_sibling_get = get_accessor_getter(scope.heap(), div_obj, &key_next_sibling).ok_or(
        VmError::InvariantViolation("nextSibling getter should exist"),
      )?;
      let key_node_type = PropertyKey::from_string(scope.alloc_string("nodeType")?);
      let node_type_get = get_accessor_getter(scope.heap(), div_obj, &key_node_type)
        .ok_or(VmError::InvariantViolation("nodeType getter should exist"))?;
      let key_text_content = PropertyKey::from_string(scope.alloc_string("textContent")?);
      let text_content_get = get_accessor_getter(scope.heap(), div_obj, &key_text_content).ok_or(
        VmError::InvariantViolation("textContent getter should exist"),
      )?;

      let span_val = vm.call_without_host(&mut scope, first_child_get, div_val, &[])?;
      let Value::Object(_span_obj) = span_val else {
        return Err(VmError::InvariantViolation(
          "firstChild should return an object",
        ));
      };

      let span_collection_0_identity_preserved = {
        let key_0 = PropertyKey::from_string(scope.alloc_string("0")?);
        let v0 = get_data_property_value(scope.heap(), span_coll_obj, &key_0).ok_or(
          VmError::InvariantViolation("HTMLCollection[0] should exist"),
        )?;
        v0 == span_val
      };

      let tail_val = vm.call_without_host(&mut scope, next_sibling_get, span_val, &[])?;
      let Value::Object(_tail_obj) = tail_val else {
        return Err(VmError::InvariantViolation(
          "nextSibling should return an object",
        ));
      };

      let span_node_type = vm.call_without_host(&mut scope, node_type_get, span_val, &[])?;
      let Value::Number(span_node_type) = span_node_type else {
        return Err(VmError::InvariantViolation(
          "nodeType should return a number",
        ));
      };
      let tail_node_type = vm.call_without_host(&mut scope, node_type_get, tail_val, &[])?;
      let Value::Number(tail_node_type) = tail_node_type else {
        return Err(VmError::InvariantViolation(
          "nodeType should return a number",
        ));
      };

      let span_text = vm.call_without_host(&mut scope, text_content_get, span_val, &[])?;
      let Value::String(span_text) = span_text else {
        return Err(VmError::InvariantViolation(
          "textContent should return a string",
        ));
      };
      let span_text = scope
        .heap()
        .get_string(span_text)?
        .to_utf8_lossy()
        .to_string();

      let tail_text = vm.call_without_host(&mut scope, text_content_get, tail_val, &[])?;
      let Value::String(tail_text) = tail_text else {
        return Err(VmError::InvariantViolation(
          "textContent should return a string",
        ));
      };
      let tail_text = scope
        .heap()
        .get_string(tail_text)?
        .to_utf8_lossy()
        .to_string();

      let div_text = vm.call_without_host(&mut scope, text_content_get, div_val, &[])?;
      let Value::String(div_text) = div_text else {
        return Err(VmError::InvariantViolation(
          "textContent should return a string",
        ));
      };
      let div_text = scope
        .heap()
        .get_string(div_text)?
        .to_utf8_lossy()
        .to_string();

      // document.getElementById should be able to find the inserted child element and return the same
      // wrapper object (identity cache).
      let key_get_element_by_id = PropertyKey::from_string(scope.alloc_string("getElementById")?);
      let get_element_by_id =
        get_data_property_value(scope.heap(), document_obj, &key_get_element_by_id)
          .ok_or(VmError::InvariantViolation("getElementById should exist"))?;
      let arg_child = Value::String(scope.alloc_string("child")?);
      let child_val =
        vm.call_without_host(&mut scope, get_element_by_id, document_val, &[arg_child])?;
      let child_identity_preserved = child_val == span_val;

      // outerHTML getter serializes the element itself.
      let key_outer_html = PropertyKey::from_string(scope.alloc_string("outerHTML")?);
      let outer_html_get = get_accessor_getter(scope.heap(), div_obj, &key_outer_html)
        .ok_or(VmError::InvariantViolation("outerHTML getter should exist"))?;
      let outer_html_set = get_accessor_setter(scope.heap(), div_obj, &key_outer_html)
        .ok_or(VmError::InvariantViolation("outerHTML setter should exist"))?;

      let outer_html_round_trip = vm.call_without_host(&mut scope, outer_html_get, div_val, &[])?;
      let Value::String(outer_html_round_trip) = outer_html_round_trip else {
        return Err(VmError::InvariantViolation(
          "outerHTML getter should return a string",
        ));
      };
      let outer_html_round_trip = scope
        .heap()
        .get_string(outer_html_round_trip)?
        .to_utf8_lossy()
        .to_string();

      // Insert a script via innerHTML; the Rust-side DOM should mark it as already started.
      let arg_script = Value::String(scope.alloc_string("<script id=s>console.log(1)</script>")?);
      vm.call_without_host(&mut scope, inner_html_set, div_val, &[arg_script])?;

      let span_collection_len_after_script =
        get_data_property_value(scope.heap(), span_coll_obj, &key_length).ok_or(
          VmError::InvariantViolation("HTMLCollection.length should exist after script innerHTML"),
        )?;
      let Value::Number(span_collection_len_after_script) = span_collection_len_after_script else {
        return Err(VmError::InvariantViolation(
          "HTMLCollection.length should be a number after script innerHTML",
        ));
      };

      let script_already_started = {
        let dom_ref = dom.borrow();
        let script_id = dom_ref
          .get_element_by_id("s")
          .ok_or(VmError::InvariantViolation(
            "expected script inserted via innerHTML",
          ))?;
        dom_ref.node(script_id).script_already_started
      };

      // outerHTML setter replaces the element and should disconnect the old wrapper (parentNode=null).
      let arg_replacement = Value::String(scope.alloc_string("<p id=replaced>ok</p>")?);
      vm.call_without_host(&mut scope, outer_html_set, div_val, &[arg_replacement])?;

      let key_parent_node = PropertyKey::from_string(scope.alloc_string("parentNode")?);
      let parent_node_get = get_accessor_getter(scope.heap(), div_obj, &key_parent_node).ok_or(
        VmError::InvariantViolation("parentNode getter should exist"),
      )?;
      let div_parent = vm.call_without_host(&mut scope, parent_node_get, div_val, &[])?;
      let old_wrapper_disconnected = matches!(div_parent, Value::Null);

      let arg_replaced = Value::String(scope.alloc_string("replaced")?);
      let replaced_val =
        vm.call_without_host(&mut scope, get_element_by_id, document_val, &[arg_replaced])?;
      let Value::Object(_replaced_obj) = replaced_val else {
        return Err(VmError::InvariantViolation(
          "expected replaced element wrapper",
        ));
      };
      let replaced_parent = vm.call_without_host(&mut scope, parent_node_get, replaced_val, &[])?;
      let replaced_parent_is_body = replaced_parent == body_val;

      let replaced_text = vm.call_without_host(&mut scope, text_content_get, replaced_val, &[])?;
      let Value::String(replaced_text) = replaced_text else {
        return Err(VmError::InvariantViolation(
          "textContent should return a string",
        ));
      };
      let replaced_text = scope
        .heap()
        .get_string(replaced_text)?
        .to_utf8_lossy()
        .to_string();

      let target_node_detached = {
        let dom_ref = dom.borrow();
        let parent = dom_ref
          .parent(target_id)
          .map_err(|_| VmError::InvariantViolation("dom.parent failed for original target"))?;
        parent.is_none()
      };

      // Also validate that the new element is connected under the original `<body>` element.
      {
        let dom_ref = dom.borrow();
        let replaced_id =
          dom_ref
            .get_element_by_id("replaced")
            .ok_or(VmError::InvariantViolation(
              "expected #replaced to exist in DOM",
            ))?;
        let replaced_parent = dom_ref
          .parent(replaced_id)
          .map_err(|_| VmError::InvariantViolation("dom.parent failed for #replaced"))?;
        if replaced_parent != Some(body_id) {
          return Err(VmError::InvariantViolation(
            "#replaced should be a child of the body element",
          ));
        }
      }

      Ok(Recorded {
        inner_html_initial,
        inner_html_round_trip,
        outer_html_round_trip,
        span_collection_len_initial,
        span_collection_len_after_insert,
        span_collection_0_identity_preserved,
        span_collection_len_after_script,
        span_node_type,
        tail_node_type,
        span_text,
        tail_text,
        div_text,
        child_identity_preserved,
        script_already_started,
        old_wrapper_disconnected,
        replaced_parent_is_body,
        replaced_text,
        target_node_detached,
      })
    })();

    drop(scope);
    realm.teardown(&mut heap);

    let recorded = recorded?;

    assert_eq!(recorded.inner_html_initial, "");
    assert_eq!(
      recorded.inner_html_round_trip,
      "<span id=\"child\">hi</span>tail"
    );
    assert_eq!(recorded.span_collection_len_initial, 0.0);
    assert_eq!(recorded.span_collection_len_after_insert, 1.0);
    assert!(recorded.span_collection_0_identity_preserved);
    assert_eq!(recorded.span_collection_len_after_script, 0.0);
    assert_eq!(recorded.span_node_type, 1.0);
    assert_eq!(recorded.tail_node_type, 3.0);
    assert_eq!(recorded.span_text, "hi");
    assert_eq!(recorded.tail_text, "tail");
    assert_eq!(recorded.div_text, "hitail");
    assert!(recorded.child_identity_preserved);
    assert_eq!(
      recorded.outer_html_round_trip,
      "<div id=\"target\"><span id=\"child\">hi</span>tail</div>"
    );
    assert!(recorded.script_already_started);
    assert!(recorded.old_wrapper_disconnected);
    assert!(recorded.replaced_parent_is_body);
    assert_eq!(recorded.replaced_text, "ok");
    assert!(recorded.target_node_detached);

    Ok(())
  }

  #[test]
  fn element_insert_adjacent_html_element_and_text() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());

    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;

    struct Recorded {
      bad_position_error_name: String,
      insert_adjacent_element_returns_arg: bool,
      body_child_ids: Vec<Option<String>>,
      target_child_repr: Vec<String>,
      script_already_started: bool,
    }

    let mut scope = heap.scope();
    let recorded: Result<Recorded, VmError> = (|| {
      let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
      let document_val = scope
        .heap()
        .object_get_own_data_property_value(realm.global_object(), &key_document)?
        .ok_or(VmError::InvariantViolation(
          "globalThis.document should be defined",
        ))?;
      let Value::Object(document_obj) = document_val else {
        return Err(VmError::InvariantViolation("document should be an object"));
      };

      let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
      let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
        .ok_or(VmError::InvariantViolation(
        "document.createElement should exist",
      ))?;

      let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
      let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
        .ok_or(VmError::InvariantViolation("appendChild should exist"))?;

      let tag_body = Value::String(scope.alloc_string("body")?);
      let body_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_body])?;
      vm.call_without_host(&mut scope, append_child, document_val, &[body_val])?;

      // <div id="target"></div>
      let tag_div = Value::String(scope.alloc_string("div")?);
      let target_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
      let Value::Object(target_obj) = target_val else {
        return Err(VmError::InvariantViolation(
          "expected target element wrapper",
        ));
      };
      let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
      let set_attribute = get_data_property_value(scope.heap(), target_obj, &key_set_attribute)
        .ok_or(VmError::InvariantViolation("setAttribute should exist"))?;
      let arg_id = Value::String(scope.alloc_string("id")?);
      let arg_target = Value::String(scope.alloc_string("target")?);
      vm.call_without_host(&mut scope, set_attribute, target_val, &[arg_id, arg_target])?;
      vm.call_without_host(&mut scope, append_child, body_val, &[target_val])?;

      // Grab insertAdjacent* methods.
      let key_insert_adjacent_html =
        PropertyKey::from_string(scope.alloc_string("insertAdjacentHTML")?);
      let insert_adjacent_html =
        get_data_property_value(scope.heap(), target_obj, &key_insert_adjacent_html).ok_or(
          VmError::InvariantViolation("insertAdjacentHTML should exist"),
        )?;
      let key_insert_adjacent_element =
        PropertyKey::from_string(scope.alloc_string("insertAdjacentElement")?);
      let insert_adjacent_element =
        get_data_property_value(scope.heap(), target_obj, &key_insert_adjacent_element).ok_or(
          VmError::InvariantViolation("insertAdjacentElement should exist"),
        )?;
      let key_insert_adjacent_text =
        PropertyKey::from_string(scope.alloc_string("insertAdjacentText")?);
      let insert_adjacent_text =
        get_data_property_value(scope.heap(), target_obj, &key_insert_adjacent_text).ok_or(
          VmError::InvariantViolation("insertAdjacentText should exist"),
        )?;

      // Invalid position throws SyntaxError.
      let bad_position_error_name = {
        let bad = Value::String(scope.alloc_string("nope")?);
        let html = Value::String(scope.alloc_string("<b>bad</b>")?);
        let thrown =
          match vm.call_without_host(&mut scope, insert_adjacent_html, target_val, &[bad, html]) {
            Ok(_) => {
              return Err(VmError::InvariantViolation(
                "expected insertAdjacentHTML to throw",
              ));
            }
            Err(err) => match err.thrown_value() {
              Some(v) => v,
              None => return Err(err),
            },
          };
        let Value::Object(thrown_obj) = thrown else {
          return Err(VmError::InvariantViolation(
            "thrown value should be an object",
          ));
        };
        let key_name = PropertyKey::from_string(scope.alloc_string("name")?);
        let name_val = get_data_property_value(scope.heap(), thrown_obj, &key_name).ok_or(
          VmError::InvariantViolation("thrown error should have .name"),
        )?;
        let Value::String(name_val) = name_val else {
          return Err(VmError::InvariantViolation(".name should be a string"));
        };
        scope
          .heap()
          .get_string(name_val)?
          .to_utf8_lossy()
          .to_string()
      };

      // beforebegin + afterend around the target.
      let pos_before = Value::String(scope.alloc_string("beforebegin")?);
      let html_before = Value::String(scope.alloc_string("<p id=before>one</p>")?);
      vm.call_without_host(
        &mut scope,
        insert_adjacent_html,
        target_val,
        &[pos_before, html_before],
      )?;

      let pos_after = Value::String(scope.alloc_string("afterend")?);
      let html_after = Value::String(scope.alloc_string("<p id=after>two</p>")?);
      vm.call_without_host(
        &mut scope,
        insert_adjacent_html,
        target_val,
        &[pos_after, html_after],
      )?;

      // afterbegin + beforeend inside the target.
      let pos_after_begin = Value::String(scope.alloc_string("afterbegin")?);
      let html_first = Value::String(scope.alloc_string("<span id=first>first</span>")?);
      vm.call_without_host(
        &mut scope,
        insert_adjacent_html,
        target_val,
        &[pos_after_begin, html_first],
      )?;

      let pos_before_end = Value::String(scope.alloc_string("beforeend")?);
      let html_last = Value::String(scope.alloc_string("<span id=last>last</span>")?);
      vm.call_without_host(
        &mut scope,
        insert_adjacent_html,
        target_val,
        &[pos_before_end, html_last],
      )?;

      // insertAdjacentElement(beforebegin, <section id=moved>).
      let tag_section = Value::String(scope.alloc_string("section")?);
      let moved_val =
        vm.call_without_host(&mut scope, create_element, document_val, &[tag_section])?;
      let Value::Object(moved_obj) = moved_val else {
        return Err(VmError::InvariantViolation(
          "expected moved element wrapper",
        ));
      };
      let set_attribute_moved = get_data_property_value(scope.heap(), moved_obj, &key_set_attribute)
        .ok_or(VmError::InvariantViolation(
          "setAttribute should exist on moved element",
        ))?;
      let arg_id2 = Value::String(scope.alloc_string("id")?);
      let arg_moved = Value::String(scope.alloc_string("moved")?);
      vm.call_without_host(
        &mut scope,
        set_attribute_moved,
        moved_val,
        &[arg_id2, arg_moved],
      )?;

      let where_before_begin = Value::String(scope.alloc_string("beforebegin")?);
      let inserted = vm.call_without_host(
        &mut scope,
        insert_adjacent_element,
        target_val,
        &[where_before_begin, moved_val],
      )?;
      let insert_adjacent_element_returns_arg = inserted == moved_val;

      // insertAdjacentText(beforeend, "tail") and then a script.
      let where_before_end = Value::String(scope.alloc_string("beforeend")?);
      let data_tail = Value::String(scope.alloc_string("tail")?);
      vm.call_without_host(
        &mut scope,
        insert_adjacent_text,
        target_val,
        &[where_before_end, data_tail],
      )?;

      let where_before_end2 = Value::String(scope.alloc_string("beforeend")?);
      let html_script = Value::String(scope.alloc_string("<script id=s>console.log(1)</script>")?);
      vm.call_without_host(
        &mut scope,
        insert_adjacent_html,
        target_val,
        &[where_before_end2, html_script],
      )?;

      // Inspect the Rust-side dom2 tree for structure and script flags.
      let (body_child_ids, target_child_repr, script_already_started) = {
        let dom_ref = dom.borrow();
        let body_id = dom_ref
          .document_element()
          .ok_or(VmError::InvariantViolation("missing <body> element"))?;

        let body_child_ids: Vec<Option<String>> = dom_ref
          .children(body_id)
          .map_err(|_| VmError::InvariantViolation("dom.children(body) failed"))?
          .iter()
          .map(|&child| {
            dom_ref
              .get_attribute(child, "id")
              .ok()
              .flatten()
              .map(str::to_string)
          })
          .collect();

        let target_id = dom_ref
          .get_element_by_id("target")
          .ok_or(VmError::InvariantViolation("missing #target element"))?;
        let target_children = dom_ref
          .children(target_id)
          .map_err(|_| VmError::InvariantViolation("dom.children(target) failed"))?;

        let target_child_repr: Vec<String> = target_children
          .iter()
          .map(|&child| {
            let node = dom_ref.node(child);
            match &node.kind {
              NodeKind::Text { content } => format!("#text:{content}"),
              NodeKind::Element { tag_name, .. } => {
                let tag = tag_name.to_ascii_lowercase();
                let id = dom_ref
                  .get_attribute(child, "id")
                  .ok()
                  .flatten()
                  .unwrap_or("");
                if id.is_empty() {
                  tag
                } else {
                  format!("{tag}#{id}")
                }
              }
              NodeKind::Slot { .. } => "slot".to_string(),
              other => format!("{other:?}"),
            }
          })
          .collect();

        let script_id = dom_ref
          .get_element_by_id("s")
          .ok_or(VmError::InvariantViolation("missing inserted script"))?;
        let script_already_started = dom_ref.node(script_id).script_already_started;

        (body_child_ids, target_child_repr, script_already_started)
      };

      Ok(Recorded {
        bad_position_error_name,
        insert_adjacent_element_returns_arg,
        body_child_ids,
        target_child_repr,
        script_already_started,
      })
    })();

    drop(scope);
    realm.teardown(&mut heap);

    let recorded = recorded?;
    assert_eq!(recorded.bad_position_error_name, "SyntaxError");
    assert!(recorded.insert_adjacent_element_returns_arg);

    let body_ids: Vec<Option<&str>> = recorded
      .body_child_ids
      .iter()
      .map(|v| v.as_deref())
      .collect();
    assert_eq!(
      body_ids,
      vec![Some("before"), Some("moved"), Some("target"), Some("after")]
    );

    // Expected child order:
    // <span id=first>, <span id=last>, "tail", <script id=s>
    assert_eq!(
      recorded.target_child_repr,
      vec![
        "span#first".to_string(),
        "span#last".to_string(),
        "#text:tail".to_string(),
        "script#s".to_string(),
      ]
    );
    assert!(recorded.script_already_started);
    Ok(())
  }

  #[test]
  fn element_class_list_dom_token_list() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());

    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;

    let mut scope = heap.scope();
    let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
    let document_val = scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key_document)?
      .expect("globalThis.document should exist");
    let document_obj = match document_val {
      Value::Object(o) => o,
      _ => panic!("document should be an object"),
    };

    // Create an element, attach it, and set class="a".
    let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
    let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
      .expect("document.createElement should exist");
    let tag_div = Value::String(scope.alloc_string("div")?);
    let el_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
    let el_obj = match el_val {
      Value::Object(o) => o,
      _ => panic!("createElement should return an object"),
    };

    let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
    let set_attribute =
      get_data_property_value(scope.heap(), el_obj, &key_set_attribute).expect("setAttribute exists");
    let arg_id = Value::String(scope.alloc_string("id")?);
    let arg_e1 = Value::String(scope.alloc_string("e1")?);
    vm.call_without_host(&mut scope, set_attribute, el_val, &[arg_id, arg_e1])?;

    let arg_class = Value::String(scope.alloc_string("class")?);
    let arg_a = Value::String(scope.alloc_string("a")?);
    vm.call_without_host(&mut scope, set_attribute, el_val, &[arg_class, arg_a])?;

    let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
    let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
      .expect("appendChild exists");
    vm.call_without_host(&mut scope, append_child, document_val, &[el_val])?;

    let e1_id = dom.borrow().get_element_by_id("e1").expect("missing #e1");
    assert_eq!(
      dom.borrow().get_attribute(e1_id, "class").unwrap(),
      Some("a")
    );

    // classList getter returns a DOMTokenList wrapper with identity.
    let key_class_list = PropertyKey::from_string(scope.alloc_string("classList")?);
    let class_list_get =
      get_accessor_getter(scope.heap(), el_obj, &key_class_list).expect("classList getter exists");
    let list1 = vm.call_without_host(&mut scope, class_list_get, el_val, &[])?;
    let list2 = vm.call_without_host(&mut scope, class_list_get, el_val, &[])?;
    assert_eq!(list1, list2, "classList should preserve wrapper identity");

    let list_obj = match list1 {
      Value::Object(o) => o,
      _ => panic!("classList should return an object"),
    };

    let key_contains = PropertyKey::from_string(scope.alloc_string("contains")?);
    let contains =
      get_data_property_value(scope.heap(), list_obj, &key_contains).expect("contains exists");

    let arg_a2 = Value::String(scope.alloc_string("a")?);
    let arg_b = Value::String(scope.alloc_string("b")?);
    let has_a = vm.call_without_host(&mut scope, contains, list1, &[arg_a2])?;
    let has_b = vm.call_without_host(&mut scope, contains, list1, &[arg_b])?;
    assert_eq!(has_a, Value::Bool(true));
    assert_eq!(has_b, Value::Bool(false));

    let key_add = PropertyKey::from_string(scope.alloc_string("add")?);
    let add = get_data_property_value(scope.heap(), list_obj, &key_add).expect("add exists");
    let arg_b2 = Value::String(scope.alloc_string("b")?);
    assert!(matches!(
      vm.call_without_host(&mut scope, add, list1, &[arg_b2])?,
      Value::Undefined
    ));
    assert_eq!(
      dom.borrow().get_attribute(e1_id, "class").unwrap(),
      Some("a b")
    );

    let key_remove = PropertyKey::from_string(scope.alloc_string("remove")?);
    let remove = get_data_property_value(scope.heap(), list_obj, &key_remove).expect("remove exists");
    let arg_a3 = Value::String(scope.alloc_string("a")?);
    vm.call_without_host(&mut scope, remove, list1, &[arg_a3])?;
    assert_eq!(
      dom.borrow().get_attribute(e1_id, "class").unwrap(),
      Some("b")
    );

    let key_toggle = PropertyKey::from_string(scope.alloc_string("toggle")?);
    let toggle = get_data_property_value(scope.heap(), list_obj, &key_toggle).expect("toggle exists");

    let arg_c = Value::String(scope.alloc_string("c")?);
    let added = vm.call_without_host(&mut scope, toggle, list1, &[arg_c])?;
    assert_eq!(added, Value::Bool(true));
    assert_eq!(
      dom.borrow().get_attribute(e1_id, "class").unwrap(),
      Some("b c")
    );

    let arg_c2 = Value::String(scope.alloc_string("c")?);
    let removed = vm.call_without_host(&mut scope, toggle, list1, &[arg_c2])?;
    assert_eq!(removed, Value::Bool(false));
    assert_eq!(
      dom.borrow().get_attribute(e1_id, "class").unwrap(),
      Some("b")
    );

    // replace(token, newToken) reflects to the backing `class` attribute.
    let key_replace = PropertyKey::from_string(scope.alloc_string("replace")?);
    let replace =
      get_data_property_value(scope.heap(), list_obj, &key_replace).expect("replace exists");
    let arg_b3 = Value::String(scope.alloc_string("b")?);
    let arg_d = Value::String(scope.alloc_string("d")?);
    let replaced = vm.call_without_host(&mut scope, replace, list1, &[arg_b3, arg_d])?;
    assert_eq!(replaced, Value::Bool(true));
    assert_eq!(
      dom.borrow().get_attribute(e1_id, "class").unwrap(),
      Some("d")
    );

    let arg_nope = Value::String(scope.alloc_string("nope")?);
    let arg_x = Value::String(scope.alloc_string("x")?);
    let replaced = vm.call_without_host(&mut scope, replace, list1, &[arg_nope, arg_x])?;
    assert_eq!(replaced, Value::Bool(false));
    assert_eq!(
      dom.borrow().get_attribute(e1_id, "class").unwrap(),
      Some("d")
    );

    // Invalid tokens containing ASCII whitespace throw InvalidCharacterError (DOMTokenList).
    let bad = Value::String(scope.alloc_string("bad token")?);
    let thrown = match vm.call_without_host(&mut scope, add, list1, &[bad]) {
      Ok(_) => panic!("expected classList.add to throw for invalid token"),
      Err(err) => match err.thrown_value() {
        Some(v) => v,
        None => return Err(err),
      },
    };
    let thrown_obj = match thrown {
      Value::Object(o) => o,
      _ => panic!("thrown value should be an object"),
    };
    let key_name = PropertyKey::from_string(scope.alloc_string("name")?);
    let name_val = get_data_property_value(scope.heap(), thrown_obj, &key_name)
      .expect("thrown object should have .name");
    let name_str = match name_val {
      Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
      _ => panic!(".name should be a string"),
    };
    assert_eq!(name_str, "InvalidCharacterError");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn dataset_and_style_shims_reflect_to_attributes() -> Result<(), VmError> {

    #[derive(Clone)]
    struct DatasetHooks {
      dom: Rc<RefCell<Document>>,
    }

    impl VmHostHooks for DatasetHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {
        // This test does not enqueue Promise jobs. If that changes, the hook should discard the job
        // via a real `VmJobContext` to avoid leaking persistent roots.
        panic!("unexpected Promise job in dataset/style shim test");
      }

      fn host_exotic_get(
        &mut self,
        scope: &mut Scope<'_>,
        obj: vm_js::GcObject,
        key: PropertyKey,
        receiver: Value,
      ) -> Result<Option<Value>, VmError> {
        let _ = receiver;

        let slots = scope.heap().object_host_slots(obj)?;
        let Some(slots) = slots else {
          return Ok(None);
        };
        if slots.b != DOM_STRING_MAP_HOST_KIND {
          return Ok(None);
        }

        let PropertyKey::String(prop_s) = key else {
          return Ok(None);
        };

        let node_index = match usize::try_from(slots.a) {
          Ok(v) => v,
          Err(_) => return Ok(None),
        };
        let node_id = match self.dom.borrow().node_id_from_index(node_index) {
          Ok(id) => id,
          Err(_) => return Ok(None),
        };

        let prop = scope.heap().get_string(prop_s)?.to_utf8_lossy();
        let dom = self.dom.borrow();
        let Some(value) = dom.dataset_get(node_id, &prop) else {
          return Ok(None);
        };
        Ok(Some(Value::String(scope.alloc_string(value)?)))
      }

      fn host_exotic_set(
        &mut self,
        scope: &mut Scope<'_>,
        obj: vm_js::GcObject,
        key: PropertyKey,
        value: Value,
        receiver: Value,
      ) -> Result<Option<bool>, VmError> {
        let _ = receiver;

        let slots = scope.heap().object_host_slots(obj)?;
        let Some(slots) = slots else {
          return Ok(None);
        };
        if slots.b != DOM_STRING_MAP_HOST_KIND {
          return Ok(None);
        }

        let PropertyKey::String(prop_s) = key else {
          return Ok(None);
        };

        let node_index = match usize::try_from(slots.a) {
          Ok(v) => v,
          Err(_) => return Ok(None),
        };
        let node_id = match self.dom.borrow().node_id_from_index(node_index) {
          Ok(id) => id,
          Err(_) => return Ok(None),
        };

        let prop = scope.heap().get_string(prop_s)?.to_utf8_lossy();

        let value_s = scope.heap_mut().to_string(value)?;
        let value = scope.heap().get_string(value_s)?.to_utf8_lossy();

        self
          .dom
          .borrow_mut()
          .dataset_set(node_id, &prop, &value)
          .map_err(|_| VmError::TypeError("failed to set dataset property"))?;

        Ok(Some(true))
      }

      fn host_exotic_delete(
        &mut self,
        scope: &mut Scope<'_>,
        obj: vm_js::GcObject,
        key: PropertyKey,
      ) -> Result<Option<bool>, VmError> {
        let slots = scope.heap().object_host_slots(obj)?;
        let Some(slots) = slots else {
          return Ok(None);
        };
        if slots.b != DOM_STRING_MAP_HOST_KIND {
          return Ok(None);
        }

        let PropertyKey::String(prop_s) = key else {
          return Ok(None);
        };

        let node_index = match usize::try_from(slots.a) {
          Ok(v) => v,
          Err(_) => return Ok(None),
        };
        let node_id = match self.dom.borrow().node_id_from_index(node_index) {
          Ok(id) => id,
          Err(_) => return Ok(None),
        };

        let prop = scope.heap().get_string(prop_s)?.to_utf8_lossy();

        self
          .dom
          .borrow_mut()
          .dataset_delete(node_id, &prop)
          .map_err(|_| VmError::TypeError("failed to delete dataset property"))?;

        Ok(Some(true))
      }
    }

    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(limits);
    let mut rt = JsRuntime::new(vm, heap)?;

    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    let realm_ptr = rt.realm() as *const Realm;
    // SAFETY: `vm-js::JsRuntime` stores `vm`, `heap`, and `realm` as disjoint fields. We do not move
    // `rt` while these borrows are live.
    let realm = unsafe { &*realm_ptr };
    install_dom_bindings(&mut rt.vm, &mut rt.heap, realm, dom.clone(), current_script)?;

    let mut hooks = DatasetHooks { dom: dom.clone() };
    let ok = rt.exec_script_with_hooks(
      &mut hooks,
      "(() => {\n\
        const el = document.createElement('div');\n\
        el.id = 't';\n\
        document.appendChild(el);\n\
        el.dataset.fooBar = 'baz';\n\
        const got = el.dataset.fooBar;\n\
        delete el.dataset.fooBar;\n\
        const missing = el.dataset.fooBar;\n\
        el.style.setProperty('backgroundColor', 'red');\n\
        const style = el.style.getPropertyValue('background-color');\n\
        return got === 'baz' && missing === undefined && style === 'red';\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    let t = dom.borrow().get_element_by_id("t").expect("missing #t");
    assert_eq!(dom.borrow().get_attribute(t, "data-foo-bar").unwrap(), None);
    assert_eq!(
      dom.borrow().get_attribute(t, "style").unwrap(),
      Some("background-color: red;")
    );

    Ok(())
  }

  #[test]
  fn get_elements_by_tag_name_is_live_and_skips_inert_template_contents() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());

    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;

    let mut scope = heap.scope();
    let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
    let document_val = scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key_document)?
      .expect("globalThis.document should exist");
    let document_obj = match document_val {
      Value::Object(o) => o,
      _ => panic!("document should be an object"),
    };

    let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
    let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
      .expect("document.createElement should exist");

    let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
    let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
    let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
      .expect("appendChild exists");

    // Build a minimal tree:
    // document
    //   <body>
    //     <div id=a></div>
    //     <div id=b></div>
    //     <template>
    //       <div id=inside></div>  (inert, should be skipped)
    //     </template>
    let tag_body = Value::String(scope.alloc_string("body")?);
    let body_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_body])?;

    vm.call_without_host(&mut scope, append_child, document_val, &[body_val])?;

    let tag_div = Value::String(scope.alloc_string("div")?);
    let d1_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
    let d1_obj = match d1_val {
      Value::Object(o) => o,
      _ => panic!("expected div wrapper"),
    };
    let set_attr =
      get_data_property_value(scope.heap(), d1_obj, &key_set_attribute).expect("setAttribute exists");
    let arg_id = Value::String(scope.alloc_string("id")?);
    let arg_a = Value::String(scope.alloc_string("a")?);
    vm.call_without_host(&mut scope, set_attr, d1_val, &[arg_id, arg_a])?;
    vm.call_without_host(&mut scope, append_child, body_val, &[d1_val])?;

    let tag_div2 = Value::String(scope.alloc_string("div")?);
    let d2_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div2])?;
    let d2_obj = match d2_val {
      Value::Object(o) => o,
      _ => panic!("expected div wrapper"),
    };
    let set_attr2 =
      get_data_property_value(scope.heap(), d2_obj, &key_set_attribute).expect("setAttribute exists");
    let arg_id2 = Value::String(scope.alloc_string("id")?);
    let arg_b = Value::String(scope.alloc_string("b")?);
    vm.call_without_host(&mut scope, set_attr2, d2_val, &[arg_id2, arg_b])?;

    // Call getElementsByTagName before inserting d2 to exercise liveness.
    let key_get_elements_by_tag_name =
      PropertyKey::from_string(scope.alloc_string("getElementsByTagName")?);
    let get_elements_by_tag_name =
      get_data_property_value(scope.heap(), document_obj, &key_get_elements_by_tag_name)
        .expect("getElementsByTagName should exist");

    let arg_div = Value::String(scope.alloc_string("div")?);
    let coll_val = vm.call_without_host(
      &mut scope,
      get_elements_by_tag_name,
      document_val,
      &[arg_div],
    )?;
    let coll_obj = match coll_val {
      Value::Object(o) => o,
      _ => panic!("expected an object collection"),
    };

    let key_length = PropertyKey::from_string(scope.alloc_string("length")?);
    let len1 = get_data_property_value(scope.heap(), coll_obj, &key_length).expect("length exists");
    assert_eq!(len1, Value::Number(1.0));

    let key_0 = PropertyKey::from_string(scope.alloc_string("0")?);
    let v0 = get_data_property_value(scope.heap(), coll_obj, &key_0).expect("coll[0] exists");
    assert_eq!(v0, d1_val);

    let key_item = PropertyKey::from_string(scope.alloc_string("item")?);
    let item = get_data_property_value(scope.heap(), coll_obj, &key_item).expect("item exists");
    let item0 = vm.call_without_host(&mut scope, item, coll_val, &[Value::Number(0.0)])?;
    assert_eq!(item0, d1_val);
    let item_neg = vm.call_without_host(&mut scope, item, coll_val, &[Value::Number(-1.0)])?;
    assert!(matches!(item_neg, Value::Null));

    // Append d2 and ensure the same collection object updates.
    vm.call_without_host(&mut scope, append_child, body_val, &[d2_val])?;
    let len2 = get_data_property_value(scope.heap(), coll_obj, &key_length).expect("length exists");
    assert_eq!(len2, Value::Number(2.0));
    let key_1 = PropertyKey::from_string(scope.alloc_string("1")?);
    let v1 = get_data_property_value(scope.heap(), coll_obj, &key_1).expect("coll[1] exists");
    assert_eq!(v1, d2_val);

    // Append a <template><div></div></template> and ensure inert contents are skipped.
    let tag_template = Value::String(scope.alloc_string("template")?);
    let tmpl_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_template])?;
    vm.call_without_host(&mut scope, append_child, body_val, &[tmpl_val])?;

    let inside_val = {
      let tag_div3 = Value::String(scope.alloc_string("div")?);
      let inside = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div3])?;
      let inside_obj = match inside {
        Value::Object(o) => o,
        _ => panic!("expected div wrapper"),
      };
      let set_attr3 = get_data_property_value(scope.heap(), inside_obj, &key_set_attribute)
        .expect("setAttribute exists");
      let arg_id3 = Value::String(scope.alloc_string("id")?);
      let arg_inside = Value::String(scope.alloc_string("inside")?);
      vm.call_without_host(&mut scope, set_attr3, inside, &[arg_id3, arg_inside])?;
      inside
    };
    vm.call_without_host(&mut scope, append_child, tmpl_val, &[inside_val])?;

    let len3 = get_data_property_value(scope.heap(), coll_obj, &key_length).expect("length exists");
    assert_eq!(len3, Value::Number(2.0));

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn get_elements_by_class_name_tokenizes_and_is_live() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());

    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;

    let mut scope = heap.scope();
    let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
    let document_val = scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key_document)?
      .expect("globalThis.document should exist");
    let document_obj = match document_val {
      Value::Object(o) => o,
      _ => panic!("document should be an object"),
    };

    let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
    let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
      .expect("document.createElement should exist");

    let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
    let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
    let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
      .expect("appendChild exists");

    let tag_body = Value::String(scope.alloc_string("body")?);
    let body_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_body])?;
    vm.call_without_host(&mut scope, append_child, document_val, &[body_val])?;

    let tag_div = Value::String(scope.alloc_string("div")?);
    let d1_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
    let d1_obj = match d1_val {
      Value::Object(o) => o,
      _ => panic!("expected div wrapper"),
    };
    let set_attr =
      get_data_property_value(scope.heap(), d1_obj, &key_set_attribute).expect("setAttribute exists");
    let arg_class = Value::String(scope.alloc_string("class")?);
    let arg_foo_bar = Value::String(scope.alloc_string("foo bar")?);
    vm.call_without_host(&mut scope, set_attr, d1_val, &[arg_class, arg_foo_bar])?;
    vm.call_without_host(&mut scope, append_child, body_val, &[d1_val])?;

    let tag_div2 = Value::String(scope.alloc_string("div")?);
    let d2_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div2])?;
    let d2_obj = match d2_val {
      Value::Object(o) => o,
      _ => panic!("expected div wrapper"),
    };
    let set_attr2 =
      get_data_property_value(scope.heap(), d2_obj, &key_set_attribute).expect("setAttribute exists");
    let arg_class2 = Value::String(scope.alloc_string("class")?);
    let arg_foo = Value::String(scope.alloc_string("foo")?);
    vm.call_without_host(&mut scope, set_attr2, d2_val, &[arg_class2, arg_foo])?;
    vm.call_without_host(&mut scope, append_child, body_val, &[d2_val])?;

    let key_get_elements_by_class_name =
      PropertyKey::from_string(scope.alloc_string("getElementsByClassName")?);
    let get_elements_by_class_name =
      get_data_property_value(scope.heap(), document_obj, &key_get_elements_by_class_name)
        .expect("getElementsByClassName should exist");

    let arg_query = Value::String(scope.alloc_string("foo  bar")?);
    let coll_val = vm.call_without_host(
      &mut scope,
      get_elements_by_class_name,
      document_val,
      &[arg_query],
    )?;
    let coll_obj = match coll_val {
      Value::Object(o) => o,
      _ => panic!("expected collection object"),
    };

    let key_length = PropertyKey::from_string(scope.alloc_string("length")?);
    let len1 = get_data_property_value(scope.heap(), coll_obj, &key_length).expect("length exists");
    assert_eq!(len1, Value::Number(1.0));

    let key_0 = PropertyKey::from_string(scope.alloc_string("0")?);
    let v0 = get_data_property_value(scope.heap(), coll_obj, &key_0).expect("coll[0] exists");
    assert_eq!(v0, d1_val);

    // Add a third element with both classes; the collection should update.
    let tag_div3 = Value::String(scope.alloc_string("div")?);
    let d3_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div3])?;
    let d3_obj = match d3_val {
      Value::Object(o) => o,
      _ => panic!("expected div wrapper"),
    };
    let set_attr3 =
      get_data_property_value(scope.heap(), d3_obj, &key_set_attribute).expect("setAttribute exists");
    let arg_class3 = Value::String(scope.alloc_string("class")?);
    let arg_bar_tab_foo = Value::String(scope.alloc_string("bar\tfoo baz")?);
    vm.call_without_host(
      &mut scope,
      set_attr3,
      d3_val,
      &[arg_class3, arg_bar_tab_foo],
    )?;
    vm.call_without_host(&mut scope, append_child, body_val, &[d3_val])?;

    let len2 = get_data_property_value(scope.heap(), coll_obj, &key_length).expect("length exists");
    assert_eq!(len2, Value::Number(2.0));
    let key_1 = PropertyKey::from_string(scope.alloc_string("1")?);
    let v1 = get_data_property_value(scope.heap(), coll_obj, &key_1).expect("coll[1] exists");
    assert_eq!(v1, d3_val);

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn get_elements_by_name_matches_name_attribute() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());

    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;

    let mut scope = heap.scope();
    let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
    let document_val = scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key_document)?
      .expect("globalThis.document should exist");
    let document_obj = match document_val {
      Value::Object(o) => o,
      _ => panic!("document should be an object"),
    };

    let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
    let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
      .expect("document.createElement should exist");

    let key_set_attribute = PropertyKey::from_string(scope.alloc_string("setAttribute")?);
    let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
    let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
      .expect("appendChild exists");

    let tag_body = Value::String(scope.alloc_string("body")?);
    let body_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_body])?;
    vm.call_without_host(&mut scope, append_child, document_val, &[body_val])?;

    let tag_input = Value::String(scope.alloc_string("input")?);
    let i1_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_input])?;
    let i1_obj = match i1_val {
      Value::Object(o) => o,
      _ => panic!("expected input wrapper"),
    };
    let set_attr =
      get_data_property_value(scope.heap(), i1_obj, &key_set_attribute).expect("setAttribute exists");
    let arg_name = Value::String(scope.alloc_string("name")?);
    let arg_n = Value::String(scope.alloc_string("n")?);
    vm.call_without_host(&mut scope, set_attr, i1_val, &[arg_name, arg_n])?;
    vm.call_without_host(&mut scope, append_child, body_val, &[i1_val])?;

    let tag_div = Value::String(scope.alloc_string("div")?);
    let d_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
    let d_obj = match d_val {
      Value::Object(o) => o,
      _ => panic!("expected div wrapper"),
    };
    let set_attr2 =
      get_data_property_value(scope.heap(), d_obj, &key_set_attribute).expect("setAttribute exists");
    let arg_name2 = Value::String(scope.alloc_string("name")?);
    let arg_n2 = Value::String(scope.alloc_string("n")?);
    vm.call_without_host(&mut scope, set_attr2, d_val, &[arg_name2, arg_n2])?;
    vm.call_without_host(&mut scope, append_child, body_val, &[d_val])?;

    let key_get_elements_by_name = PropertyKey::from_string(scope.alloc_string("getElementsByName")?);
    let get_elements_by_name =
      get_data_property_value(scope.heap(), document_obj, &key_get_elements_by_name)
        .expect("getElementsByName should exist");

    let arg_q = Value::String(scope.alloc_string("n")?);
    let coll_val = vm.call_without_host(&mut scope, get_elements_by_name, document_val, &[arg_q])?;
    let coll_obj = match coll_val {
      Value::Object(o) => o,
      _ => panic!("expected collection object"),
    };

    let key_length = PropertyKey::from_string(scope.alloc_string("length")?);
    let len = get_data_property_value(scope.heap(), coll_obj, &key_length).expect("length exists");
    assert_eq!(len, Value::Number(2.0));

    let key_0 = PropertyKey::from_string(scope.alloc_string("0")?);
    let key_1 = PropertyKey::from_string(scope.alloc_string("1")?);
    let v0 = get_data_property_value(scope.heap(), coll_obj, &key_0).expect("coll[0] exists");
    let v1 = get_data_property_value(scope.heap(), coll_obj, &key_1).expect("coll[1] exists");
    assert_eq!(v0, i1_val);
    assert_eq!(v1, d_val);

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn get_elements_by_tag_name_ns_supports_html_namespace_and_wildcards() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());

    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom.clone(), current_script)?;

    let mut scope = heap.scope();
    let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
    let document_val = scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key_document)?
      .expect("globalThis.document should exist");
    let document_obj = match document_val {
      Value::Object(o) => o,
      _ => panic!("document should be an object"),
    };

    let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
    let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
      .expect("document.createElement should exist");

    let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
    let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
      .expect("appendChild exists");

    let tag_body = Value::String(scope.alloc_string("body")?);
    let body_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_body])?;
    vm.call_without_host(&mut scope, append_child, document_val, &[body_val])?;

    let tag_div = Value::String(scope.alloc_string("div")?);
    let d_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
    vm.call_without_host(&mut scope, append_child, body_val, &[d_val])?;

    let key_get_elements_by_tag_name_ns =
      PropertyKey::from_string(scope.alloc_string("getElementsByTagNameNS")?);
    let get_elements_by_tag_name_ns =
      get_data_property_value(scope.heap(), document_obj, &key_get_elements_by_tag_name_ns)
        .expect("getElementsByTagNameNS should exist");

    let arg_ns = Value::String(scope.alloc_string("http://www.w3.org/1999/xhtml")?);
    let arg_div = Value::String(scope.alloc_string("DIV")?);
    let coll_val = vm.call_without_host(
      &mut scope,
      get_elements_by_tag_name_ns,
      document_val,
      &[arg_ns, arg_div],
    )?;
    let coll_obj = match coll_val {
      Value::Object(o) => o,
      _ => panic!("expected collection object"),
    };

    let key_length = PropertyKey::from_string(scope.alloc_string("length")?);
    let len = get_data_property_value(scope.heap(), coll_obj, &key_length).expect("length exists");
    assert_eq!(len, Value::Number(1.0));

    let arg_ns2 = Value::String(scope.alloc_string("*")?);
    let arg_div2 = Value::String(scope.alloc_string("div")?);
    let coll2_val = vm.call_without_host(
      &mut scope,
      get_elements_by_tag_name_ns,
      document_val,
      &[arg_ns2, arg_div2],
    )?;
    let coll2_obj = match coll2_val {
      Value::Object(o) => o,
      _ => panic!("expected collection object"),
    };
    let len2 = get_data_property_value(scope.heap(), coll2_obj, &key_length).expect("length exists");
    assert_eq!(len2, Value::Number(1.0));

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn node_clone_node_deep_clones_detached_subtree() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());

    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom, current_script)?;

    let mut scope = heap.scope();

    let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
    let document_val = scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key_document)?
      .expect("globalThis.document should be defined");
    let document_obj = match document_val {
      Value::Object(o) => o,
      _ => panic!("document should be an object"),
    };

    let key_create_element = PropertyKey::from_string(scope.alloc_string("createElement")?);
    let create_element = get_data_property_value(scope.heap(), document_obj, &key_create_element)
      .expect("document.createElement should exist");

    let key_append_child = PropertyKey::from_string(scope.alloc_string("appendChild")?);
    let append_child = get_data_property_value(scope.heap(), document_obj, &key_append_child)
      .expect("appendChild exists");

    // document.createElement("div")
    let tag_div = Value::String(scope.alloc_string("div")?);
    let div_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_div])?;
    let Value::Object(div_obj) = div_val else {
      panic!("expected div wrapper");
    };

    // div.id = "src" (via setter).
    let key_id = PropertyKey::from_string(scope.alloc_string("id")?);
    let id_set = get_accessor_setter(scope.heap(), div_obj, &key_id).expect("id setter exists");
    let arg_src = Value::String(scope.alloc_string("src")?);
    vm.call_without_host(&mut scope, id_set, div_val, &[arg_src])?;

    // Connect the original: document.appendChild(div)
    vm.call_without_host(&mut scope, append_child, document_val, &[div_val])?;

    // Add a child: <span>hello</span>
    let tag_span = Value::String(scope.alloc_string("span")?);
    let span_val = vm.call_without_host(&mut scope, create_element, document_val, &[tag_span])?;
    vm.call_without_host(&mut scope, append_child, div_val, &[span_val])?;

    let Value::Object(span_obj) = span_val else {
      panic!("expected span wrapper");
    };
    let key_text_content = PropertyKey::from_string(scope.alloc_string("textContent")?);
    let text_content_set = get_accessor_setter(scope.heap(), span_obj, &key_text_content)
      .expect("textContent setter exists");
    let arg_hello = Value::String(scope.alloc_string("hello")?);
    vm.call_without_host(&mut scope, text_content_set, span_val, &[arg_hello])?;

    // div.cloneNode(true)
    let key_clone_node = PropertyKey::from_string(scope.alloc_string("cloneNode")?);
    let clone_node =
      get_data_property_value(scope.heap(), div_obj, &key_clone_node).expect("cloneNode exists");
    let clone_val = vm.call_without_host(&mut scope, clone_node, div_val, &[Value::Bool(true)])?;
    let Value::Object(clone_obj) = clone_val else {
      panic!("expected clone wrapper");
    };

    assert_ne!(clone_val, div_val, "cloneNode must allocate a new wrapper");

    // Detached clone: parentNode === null, isConnected === false.
    let key_parent_node = PropertyKey::from_string(scope.alloc_string("parentNode")?);
    let parent_node_get = get_accessor_getter(scope.heap(), clone_obj, &key_parent_node)
      .expect("parentNode getter exists");
    assert_eq!(
      vm.call_without_host(&mut scope, parent_node_get, clone_val, &[])?,
      Value::Null
    );

    let key_is_connected = PropertyKey::from_string(scope.alloc_string("isConnected")?);
    let is_connected_get = get_accessor_getter(scope.heap(), clone_obj, &key_is_connected)
      .expect("isConnected getter exists");
    assert_eq!(
      vm.call_without_host(&mut scope, is_connected_get, clone_val, &[])?,
      Value::Bool(false)
    );

    // Reflected attribute is cloned.
    let id_get = get_accessor_getter(scope.heap(), clone_obj, &key_id).expect("id getter exists");
    let id_val = vm.call_without_host(&mut scope, id_get, clone_val, &[])?;
    let Value::String(id_str) = id_val else {
      panic!("expected id string");
    };
    assert_eq!(scope.heap().get_string(id_str)?.to_utf8_lossy(), "src");

    // Deep clone should include children but preserve identity (cloneChild !== span).
    let key_first_child = PropertyKey::from_string(scope.alloc_string("firstChild")?);
    let first_child_get = get_accessor_getter(scope.heap(), clone_obj, &key_first_child)
      .expect("firstChild getter exists");
    let clone_span = vm.call_without_host(&mut scope, first_child_get, clone_val, &[])?;
    assert_ne!(clone_span, Value::Null);
    assert_ne!(clone_span, span_val);

    let Value::Object(clone_span_obj) = clone_span else {
      panic!("expected cloned span wrapper");
    };

    let key_node_name = PropertyKey::from_string(scope.alloc_string("nodeName")?);
    let node_name_get = get_accessor_getter(scope.heap(), clone_span_obj, &key_node_name)
      .expect("nodeName getter exists");
    let node_name = vm.call_without_host(&mut scope, node_name_get, clone_span, &[])?;
    let Value::String(node_name_str) = node_name else {
      panic!("expected nodeName string");
    };
    assert_eq!(
      scope.heap().get_string(node_name_str)?.to_utf8_lossy(),
      "SPAN"
    );

    // Validate nested text node value.
    let clone_text = vm.call_without_host(&mut scope, first_child_get, clone_span, &[])?;
    let Value::Object(clone_text_obj) = clone_text else {
      panic!("expected cloned text wrapper");
    };
    let key_node_value = PropertyKey::from_string(scope.alloc_string("nodeValue")?);
    let node_value_get = get_accessor_getter(scope.heap(), clone_text_obj, &key_node_value)
      .expect("nodeValue getter exists");
    let v = vm.call_without_host(&mut scope, node_value_get, clone_text, &[])?;
    let Value::String(v_str) = v else {
      panic!("expected nodeValue string");
    };
    assert_eq!(scope.heap().get_string(v_str)?.to_utf8_lossy(), "hello");

    // Shallow clone: no children.
    let shallow_val = vm.call_without_host(&mut scope, clone_node, div_val, &[])?;
    let Value::Object(shallow_obj) = shallow_val else {
      panic!("expected shallow clone wrapper");
    };
    let shallow_first_child_get = get_accessor_getter(scope.heap(), shallow_obj, &key_first_child)
      .expect("firstChild getter exists");
    let shallow_first =
      vm.call_without_host(&mut scope, shallow_first_child_get, shallow_val, &[])?;
    assert_eq!(shallow_first, Value::Null);

    // Document.cloneNode(false) returns a detached Document with no children.
    let document_clone = get_data_property_value(scope.heap(), document_obj, &key_clone_node)
      .expect("document.cloneNode exists");
    let doc_shallow = vm.call_without_host(&mut scope, document_clone, document_val, &[])?;
    let Value::Object(_doc_shallow_obj) = doc_shallow else {
      panic!("expected cloned Document wrapper");
    };
    assert_eq!(
      vm.call_without_host(&mut scope, parent_node_get, doc_shallow, &[])?,
      Value::Null
    );
    assert_eq!(
      vm.call_without_host(&mut scope, is_connected_get, doc_shallow, &[])?,
      Value::Bool(false)
    );
    let doc_shallow_first = vm.call_without_host(&mut scope, first_child_get, doc_shallow, &[])?;
    assert_eq!(doc_shallow_first, Value::Null);

    // Document.cloneNode(true) deep clones the document subtree.
    let doc_deep = vm.call_without_host(
      &mut scope,
      document_clone,
      document_val,
      &[Value::Bool(true)],
    )?;
    let Value::Object(doc_deep_obj) = doc_deep else {
      panic!("expected cloned Document wrapper");
    };
    assert_ne!(doc_deep, document_val);
    assert_ne!(doc_deep_obj, document_obj);
    assert_eq!(
      vm.call_without_host(&mut scope, parent_node_get, doc_deep, &[])?,
      Value::Null
    );
    assert_eq!(
      vm.call_without_host(&mut scope, is_connected_get, doc_deep, &[])?,
      Value::Bool(false)
    );

    let doc_deep_div = vm.call_without_host(&mut scope, first_child_get, doc_deep, &[])?;
    assert_ne!(doc_deep_div, Value::Null);
    assert_ne!(doc_deep_div, div_val);
    let Value::Object(doc_deep_div_obj) = doc_deep_div else {
      panic!("expected deep-cloned div wrapper");
    };

    let doc_deep_id_get =
      get_accessor_getter(scope.heap(), doc_deep_div_obj, &key_id).expect("id getter exists");
    let doc_deep_id_val = vm.call_without_host(&mut scope, doc_deep_id_get, doc_deep_div, &[])?;
    let Value::String(doc_deep_id_str) = doc_deep_id_val else {
      panic!("expected id string");
    };
    assert_eq!(
      scope.heap().get_string(doc_deep_id_str)?.to_utf8_lossy(),
      "src"
    );

    // Mutating the clone must not affect the original.
    let arg_cloned = Value::String(scope.alloc_string("cloned")?);
    vm.call_without_host(&mut scope, id_set, doc_deep_div, &[arg_cloned])?;
    let original_id_val = vm.call_without_host(&mut scope, id_get, div_val, &[])?;
    let Value::String(original_id_str) = original_id_val else {
      panic!("expected id string");
    };
    assert_eq!(
      scope.heap().get_string(original_id_str)?.to_utf8_lossy(),
      "src"
    );

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[derive(Default)]
  struct CookieRecordingFetcher {
    cookies: Mutex<Vec<(String, String)>>,
  }

  impl CookieRecordingFetcher {
    fn cookie_header(&self) -> String {
      let lock = self
        .cookies
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      lock
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
    }
  }

  impl ResourceFetcher for CookieRecordingFetcher {
    fn fetch(&self, url: &str) -> crate::error::Result<FetchedResource> {
      Err(Error::Other(format!(
        "CookieRecordingFetcher does not support fetch: {url}"
      )))
    }

    fn cookie_header_value(&self, _url: &str) -> Option<String> {
      Some(self.cookie_header())
    }

    fn store_cookie_from_document(&self, _url: &str, cookie_string: &str) {
      let first = cookie_string
        .split_once(';')
        .map(|(a, _)| a)
        .unwrap_or(cookie_string);
      let first = first.trim_matches(|c: char| c.is_ascii_whitespace());
      let Some((name, value)) = first.split_once('=') else {
        return;
      };
      let name = name.trim_matches(|c: char| c.is_ascii_whitespace());
      if name.is_empty() {
        return;
      }

      let mut lock = self
        .cookies
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      if let Some(existing) = lock.iter_mut().find(|(n, _)| n == name) {
        existing.1 = value.to_string();
      } else {
        lock.push((name.to_string(), value.to_string()));
      }
    }
  }

  #[test]
  fn document_cookie_round_trip_is_deterministic() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom, current_script)?;

    let mut scope = heap.scope();

    let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
    let document_val = scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key_document)?
      .expect("globalThis.document should exist");
    let document_obj = match document_val {
      Value::Object(o) => o,
      other => panic!("expected document object, got {other:?}"),
    };

    let key_cookie = PropertyKey::from_string(scope.alloc_string("cookie")?);
    let cookie_get = get_accessor_getter(scope.heap(), document_obj, &key_cookie)
      .expect("document.cookie getter should exist");
    let cookie_set = get_accessor_setter(scope.heap(), document_obj, &key_cookie)
      .expect("document.cookie setter should exist");

    let cookie = vm.call_without_host(&mut scope, cookie_get, document_val, &[])?;
    let Value::String(cookie_s) = cookie else {
      panic!("expected cookie string, got {cookie:?}");
    };
    assert!(scope
      .heap()
      .get_string(cookie_s)?
      .to_utf8_lossy()
      .is_empty());

    let b = Value::String(scope.alloc_string("b=c; Path=/")?);
    vm.call_without_host(&mut scope, cookie_set, document_val, &[b])?;
    let a = Value::String(scope.alloc_string("a=b")?);
    vm.call_without_host(&mut scope, cookie_set, document_val, &[a])?;

    let cookie = vm.call_without_host(&mut scope, cookie_get, document_val, &[])?;
    let Value::String(cookie_s) = cookie else {
      panic!("expected cookie string, got {cookie:?}");
    };
    assert_eq!(
      scope.heap().get_string(cookie_s)?.to_utf8_lossy(),
      "a=b; b=c"
    );

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn document_cookie_syncs_with_fetcher_cookie_store() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom, current_script)?;

    let fetcher = Arc::new(CookieRecordingFetcher::default());
    fetcher.store_cookie_from_document("https://example.invalid/", "z=1");
    vm.user_data_mut::<DomHost>()
      .expect("DomHost user_data should be installed")
      .set_cookie_fetcher_for_document("https://example.invalid/", fetcher.clone());

    let mut scope = heap.scope();

    let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
    let document_val = scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key_document)?
      .expect("globalThis.document should exist");
    let document_obj = match document_val {
      Value::Object(o) => o,
      other => panic!("expected document object, got {other:?}"),
    };

    let key_cookie = PropertyKey::from_string(scope.alloc_string("cookie")?);
    let cookie_get = get_accessor_getter(scope.heap(), document_obj, &key_cookie)
      .expect("document.cookie getter should exist");
    let cookie_set = get_accessor_setter(scope.heap(), document_obj, &key_cookie)
      .expect("document.cookie setter should exist");

    let cookie = vm.call_without_host(&mut scope, cookie_get, document_val, &[])?;
    let Value::String(cookie_s) = cookie else {
      panic!("expected cookie string, got {cookie:?}");
    };
    assert_eq!(
      scope.heap().get_string(cookie_s)?.to_utf8_lossy(),
      "z=1",
      "cookie getter should mirror fetcher cookie state"
    );

    let b = Value::String(scope.alloc_string("b=c; Path=/")?);
    vm.call_without_host(&mut scope, cookie_set, document_val, &[b])?;
    let a = Value::String(scope.alloc_string("a=b")?);
    vm.call_without_host(&mut scope, cookie_set, document_val, &[a])?;

    assert_eq!(
      fetcher
        .cookie_header_value("https://example.invalid/")
        .unwrap_or_default(),
      "z=1; b=c; a=b"
    );

    let cookie = vm.call_without_host(&mut scope, cookie_get, document_val, &[])?;
    let Value::String(cookie_s) = cookie else {
      panic!("expected cookie string, got {cookie:?}");
    };
    assert_eq!(
      scope.heap().get_string(cookie_s)?.to_utf8_lossy(),
      "a=b; b=c; z=1"
    );

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn document_write_injects_into_streaming_parser_when_active() -> Result<(), VmError> {
    let limits = HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024);
    let mut heap = Heap::new(limits);
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let dom = Rc::new(RefCell::new(Document::new(QuirksMode::NoQuirks)));
    let current_script = Rc::new(RefCell::new(CurrentScriptState::default()));
    install_dom_bindings(&mut vm, &mut heap, &realm, dom, current_script)?;

    let mut scope = heap.scope();

    // Fetch globalThis.document.
    let key_document = PropertyKey::from_string(scope.alloc_string("document")?);
    let document_val = scope
      .heap()
      .object_get_own_data_property_value(realm.global_object(), &key_document)?
      .expect("globalThis.document should exist");
    let Value::Object(document_obj) = document_val else {
      panic!("expected document object, got {document_val:?}");
    };

    let key_write = PropertyKey::from_string(scope.alloc_string("write")?);
    let write = vm.get(&mut scope, document_obj, key_write)?;

    let html =
      "<!doctype html><html><body><script>noop</script><div id=\"after\"></div></body></html>";
    let mut parser = StreamingHtmlParser::new(Some("https://example.com/"));
    parser.push_str(html);
    parser.set_eof();

    // Pause at the </script> boundary (parser-inserted script).
    let script_node = match parser.pump().expect("pump") {
      StreamingParserYield::Script { script, .. } => script,
      other => panic!("expected Script yield, got {other:?}"),
    };

    // Call `document.write(...)` while the parser is active; it should inject the HTML into the
    // input stream before the already-buffered remainder (`<div id=after>`).
    let injected_html = Value::String(scope.alloc_string("<div id=\"x\"></div>")?);
    let result = with_active_streaming_parser(&parser, || {
      vm.call_without_host(&mut scope, write, document_val, &[injected_html])
    })?;
    assert_eq!(result, Value::Undefined);

    let finished = loop {
      match parser.pump().expect("pump") {
        StreamingParserYield::Finished { document } => break document,
        StreamingParserYield::NeedMoreInput => {
          panic!("parser unexpectedly requested more input after EOF")
        }
        StreamingParserYield::Script { .. } => panic!("unexpected additional script yield"),
      }
    };

    let body = finished.body().expect("missing <body>");
    let injected = finished
      .get_element_by_id("x")
      .expect("expected element inserted by document.write");
    let after = finished
      .get_element_by_id("after")
      .expect("expected #after element");

    let element_children: Vec<NodeId> = finished
      .node(body)
      .children
      .iter()
      .copied()
      .filter(|&id| matches!(finished.node(id).kind, NodeKind::Element { .. }))
      .collect();
    assert_eq!(
      element_children.len(),
      3,
      "expected <body> to have <script>, injected <div>, and following <div>"
    );
    assert_eq!(element_children[0], script_node);
    assert_eq!(element_children[1], injected);
    assert_eq!(element_children[2], after);

    // `document.writeln(...)` should also return undefined and be a no-op without an active parser.
    let key_writeln = PropertyKey::from_string(scope.alloc_string("writeln")?);
    let writeln = vm.get(&mut scope, document_obj, key_writeln)?;
    let arg_a = Value::String(scope.alloc_string("a")?);
    let writeln_result = vm.call_without_host(&mut scope, writeln, document_val, &[arg_a])?;
    assert_eq!(writeln_result, Value::Undefined);

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }
}
