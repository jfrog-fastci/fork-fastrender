use crate::dom::HTML_NAMESPACE;
use crate::dom2::{DomError, Document, NodeId, NodeKind};
use crate::js::cookie_jar::{CookieJar, MAX_COOKIE_STRING_BYTES};
use crate::js::CurrentScriptState;
use crate::web::dom::DomException;
use std::char::decode_utf16;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use vm_js::{
  GcObject, GcString, Heap, HostSlots, NativeFunctionId, PropertyDescriptor, PropertyKey, PropertyKind,
  Realm, RootId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
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

const DOM_TOKEN_LIST_HOST_KIND: u64 = 3;

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

fn accessor_desc_get_set(get: Value, set: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Accessor { get, set },
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LiveCollectionKind {
  TagName { qualified_name: String },
  TagNameNS {
    namespace: Option<String>,
    local_name: String,
  },
  ClassName { required: Vec<String> },
  Name { name: String },
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

fn element_kind_parts(kind: &NodeKind) -> Option<(&str, &str, &Vec<(String, String)>)> {
  match kind {
    NodeKind::Element {
      tag_name,
      namespace,
      attributes,
    } => Some((tag_name.as_str(), namespace.as_str(), attributes)),
    NodeKind::Slot {
      namespace,
      attributes,
      ..
    } => Some(("slot", namespace.as_str(), attributes)),
    _ => None,
  }
}

fn live_collection_matches(kind: &LiveCollectionKind, tag: &str, namespace: &str, attrs: &[(String, String)]) -> bool {
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
        .find(|(name, _)| {
          if is_html_namespace(namespace) {
            name.eq_ignore_ascii_case("class")
          } else {
            name == "class"
          }
        })
        .map(|(_, value)| value.as_str());
      let Some(class_attr) = class_attr else {
        return false;
      };

      let have = split_dom_ascii_whitespace(class_attr);
      required
        .iter()
        .all(|required| have.iter().any(|token| token == required))
    }
    LiveCollectionKind::Name { name } => attrs.iter().any(|(attr_name, value)| {
      (if is_html_namespace(namespace) {
        attr_name.eq_ignore_ascii_case("name")
      } else {
        attr_name == "name"
      }) && value == name
    }),
  }
}

pub struct DomHost {
  dom: Rc<RefCell<Document>>,
  current_script: Rc<RefCell<CurrentScriptState>>,

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
  live_collections: Vec<LiveCollection>,

  // Persistent roots for cached prototype objects. `DomHost` isn't traced by the GC.
  prototype_roots: Vec<RootId>,

  // Cached prototypes.
  proto_node: GcObject,
  proto_element: GcObject,
  proto_document: GcObject,
  proto_dom_token_list: GcObject,
  proto_html_collection: GcObject,

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

fn to_dom_string<'a>(scope: &mut Scope<'a>, host: &DomHost, value: Value) -> Result<String, VmError> {
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
    return throw_type_error(scope, host, "DOMTokenList method called on incompatible receiver");
  };

  if slots.b != DOM_TOKEN_LIST_HOST_KIND {
    return throw_type_error(scope, host, "DOMTokenList method called on incompatible receiver");
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
    DomException::NoModificationAllowedError { message } => {
      throw_dom_exception(scope, host, "NoModificationAllowedError", &message)
    }
    DomException::NotSupportedError { message } => {
      throw_dom_exception(scope, host, "NotSupportedError", &message)
    }
    DomException::InvalidStateError { message } => {
      throw_dom_exception(scope, host, "InvalidStateError", &message)
    }
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

  scope.heap_mut().object_set_host_slots(
    wrapper,
    HostSlots {
      a: node_id.index() as u64,
      b: kind as u64,
    },
  )?;

  host.node_wrappers.insert(node_id, WeakGcObject::from(wrapper));
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
          data_desc(wrapper, /* writable */ true, /* enumerable */ true, /* configurable */ true),
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
    _ => return throw_type_error(scope, host, "HTMLCollection.item called on incompatible receiver"),
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
      NodeKind::Element { tag_name, namespace, .. } => {
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
      let node = dom.node_mut(node_id);
      let NodeKind::Comment { content } = &mut node.kind else {
        return Ok(Value::Undefined);
      };
      content.clear();
      content.push_str(&new_value);
    }
    TargetKind::ProcessingInstruction => {
      let node = dom.node_mut(node_id);
      let NodeKind::ProcessingInstruction { data, .. } = &mut node.kind else {
        return Ok(Value::Undefined);
      };
      data.clear();
      data.push_str(&new_value);
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
  Ok(Value::Bool(host.dom.borrow().is_connected_for_scripting(node_id)))
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
      NodeKind::Element { tag_name, namespace, .. } => {
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

  let inserted = match host
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
      let node = dom.node_mut(node_id);
      if let NodeKind::Comment { content } = &mut node.kind {
        content.clear();
        content.push_str(&new_text);
      }
      return Ok(Value::Undefined);
    }
    NodeKind::ProcessingInstruction { .. } => {
      let node = dom.node_mut(node_id);
      if let NodeKind::ProcessingInstruction { data, .. } = &mut node.kind {
        data.clear();
        data.push_str(&new_text);
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
  let old_children = dom.node(node_id).children.clone();
  for child in &old_children {
    dom.node_mut(*child).parent = None;
  }
  dom.node_mut(node_id).children.clear();

  if !new_text.is_empty() {
    let text_id = dom.create_text(&new_text);
    dom.node_mut(text_id).parent = Some(node_id);
    dom.node_mut(node_id).children.push(text_id);
  }

  drop(dom);
  host.sync_live_collections(scope)?;
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
    Ok(_) => {
      host.sync_live_collections(scope)?;
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
    Ok(_) => {
      host.sync_live_collections(scope)?;
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

  let result = host
    .dom
    .borrow_mut()
    .class_list_toggle(element_id, &token, force);
  match result {
    Ok(v) => {
      host.sync_live_collections(scope)?;
      Ok(Value::Bool(v))
    }
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
  Ok(Value::String(scope.alloc_string(&host.cookie_jar.cookie_string())?))
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
    Value::Symbol(_) => return throw_type_error(scope, host, "Cannot convert a Symbol value to a string"),
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
  let call_get_elements_by_tag_name = vm.register_native_call(dom_document_get_elements_by_tag_name)?;
  let call_get_elements_by_tag_name_ns = vm.register_native_call(dom_document_get_elements_by_tag_name_ns)?;
  let call_get_elements_by_class_name = vm.register_native_call(dom_document_get_elements_by_class_name)?;
  let call_get_elements_by_name = vm.register_native_call(dom_document_get_elements_by_name)?;
  let call_element_get_elements_by_tag_name = vm.register_native_call(dom_element_get_elements_by_tag_name)?;
  let call_element_get_elements_by_tag_name_ns =
    vm.register_native_call(dom_element_get_elements_by_tag_name_ns)?;
  let call_element_get_elements_by_class_name = vm.register_native_call(dom_element_get_elements_by_class_name)?;
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
  let call_insert_adjacent_element = vm.register_native_call(dom_element_insert_adjacent_element)?;
  let call_insert_adjacent_text = vm.register_native_call(dom_element_insert_adjacent_text)?;
  let call_current_script = vm.register_native_call(dom_document_current_script_getter)?;
  let call_cookie_get = vm.register_native_call(dom_document_cookie_getter)?;
  let call_cookie_set = vm.register_native_call(dom_document_cookie_setter)?;
  let call_text_content_get = vm.register_native_call(dom_node_text_content_getter)?;
  let call_text_content_set = vm.register_native_call(dom_node_text_content_setter)?;
  let call_class_list_get = vm.register_native_call(dom_element_class_list_getter)?;
  let call_token_list_contains = vm.register_native_call(dom_token_list_contains)?;
  let call_token_list_add = vm.register_native_call(dom_token_list_add)?;
  let call_token_list_remove = vm.register_native_call(dom_token_list_remove)?;
  let call_token_list_toggle = vm.register_native_call(dom_token_list_toggle)?;

  // Install methods/getters.
  install_method(&mut scope, proto_document, "createElement", call_create_element, 1)?;
  install_method(&mut scope, proto_document, "getElementById", call_get_element_by_id, 1)?;
  install_method(&mut scope, proto_document, "querySelector", call_query_selector, 1)?;
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
  install_method(&mut scope, proto_node, "appendChild", call_append_child, 1)?;
  install_method(&mut scope, proto_node, "cloneNode", call_clone_node, 1)?;
  install_method(&mut scope, proto_node, "hasChildNodes", call_has_child_nodes, 0)?;
  install_getter(&mut scope, proto_node, "parentNode", call_parent_node)?;
  install_getter(&mut scope, proto_node, "parentElement", call_parent_element)?;
  install_getter(&mut scope, proto_node, "firstChild", call_first_child)?;
  install_getter(&mut scope, proto_node, "lastChild", call_last_child)?;
  install_getter(&mut scope, proto_node, "previousSibling", call_previous_sibling)?;
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
  install_method(&mut scope, proto_element, "setAttribute", call_set_attribute, 2)?;
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
  install_getter(&mut scope, proto_document, "currentScript", call_current_script)?;
  install_accessor(&mut scope, proto_document, "cookie", call_cookie_get, call_cookie_set)?;
  install_accessor(
    &mut scope,
    proto_node,
    "textContent",
    call_text_content_get,
    call_text_content_set,
  )?;
  install_getter(&mut scope, proto_element, "classList", call_class_list_get)?;

  install_method(&mut scope, proto_dom_token_list, "contains", call_token_list_contains, 1)?;
  install_method(&mut scope, proto_dom_token_list, "add", call_token_list_add, 0)?;
  install_method(&mut scope, proto_dom_token_list, "remove", call_token_list_remove, 0)?;
  install_method(&mut scope, proto_dom_token_list, "toggle", call_token_list_toggle, 1)?;
  install_method(&mut scope, proto_html_collection, "item", call_html_collection_item, 1)?;

  let mut host = DomHost {
    dom: dom.clone(),
    current_script: current_script.clone(),
    max_string_bytes,
    cookie_jar: CookieJar::new(),
    node_wrappers: HashMap::new(),
    class_list_wrappers: HashMap::new(),
    live_collections: Vec::new(),
    prototype_roots: Vec::new(),
    proto_node,
    proto_element,
    proto_document,
    proto_dom_token_list,
    proto_html_collection,
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

  // Root prototype objects so `DomHost` can safely store handles without being GC-traced.
  host.prototype_roots = vec![
    scope.heap_mut().add_root(Value::Object(proto_node))?,
    scope.heap_mut().add_root(Value::Object(proto_element))?,
    scope.heap_mut().add_root(Value::Object(proto_document))?,
    scope.heap_mut().add_root(Value::Object(proto_dom_token_list))?,
    scope.heap_mut().add_root(Value::Object(proto_html_collection))?,
  ];

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

  use selectors::context::QuirksMode;
  use std::cell::RefCell;
  use std::rc::Rc;
  use vm_js::{Heap, HeapLimits, PropertyKey, PropertyKind, Realm, Value, Vm, VmError, VmOptions};

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
    assert!(scope.heap().get_string(cookie_s)?.to_utf8_lossy().is_empty());

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
}
