use crate::api::BrowserDocumentDom2;
use crate::dom::HTML_NAMESPACE;
use crate::dom2::{DomError, NodeId, NodeIteratorId, NodeKind, RangeId};
use crate::geometry::{Point, Rect};
use crate::js::bindings::DomExceptionClassVmJs;
use crate::js::dom_internal_keys::{
  COLLECTION_LENGTH_KEY, CSS_STYLE_DECL_PROTOTYPE_KEY, EVENT_BRAND_KEY, EVENT_IMMEDIATE_STOP_KEY,
  EVENT_INITIALIZED_KEY, EVENT_KIND_KEY, HTML_COLLECTION_PROTOTYPE_KEY, HTML_COLLECTION_ROOT_KEY,
  NODE_CHILD_NODES_KEY, NODE_CHILDREN_KEY, NODE_ID_KEY, NODE_LIST_PROTOTYPE_KEY,
  STYLE_CSS_TEXT_GET_KEY, STYLE_CSS_TEXT_SET_KEY,
  STYLE_CURSOR_GET_KEY,
  STYLE_CURSOR_SET_KEY, STYLE_DISPLAY_GET_KEY, STYLE_DISPLAY_SET_KEY, STYLE_GET_PROPERTY_VALUE_KEY,
  STYLE_HEIGHT_GET_KEY, STYLE_HEIGHT_SET_KEY, STYLE_REMOVE_PROPERTY_KEY, STYLE_SET_PROPERTY_KEY,
  STYLE_WIDTH_GET_KEY, STYLE_WIDTH_SET_KEY, WRAPPER_DOCUMENT_KEY,
};
use crate::js::dom2_bindings;
use crate::js::dom_host::DomHostVmJs;
use crate::js::dom_platform::{DocumentId, DomInterface, DomNodeKey, DomPlatform};
use crate::js::window_realm::{
  abort_signal_listener_cleanup_native, event_target_add_event_listener_dom2,
  dom_ptr_for_document_id_read, event_target_dispatch_event_dom2, event_target_remove_event_listener_dom2,
  WindowRealmUserData, EVENT_TARGET_HOST_TAG,
};
use crate::js::window_timers::{
  event_loop_mut_from_hooks, vm_error_to_event_loop_error, VmJsEventLoopHooks,
  QUEUE_MICROTASK_NOT_CALLABLE_ERROR, QUEUE_MICROTASK_STRING_HANDLER_ERROR,
  SET_INTERVAL_NOT_CALLABLE_ERROR, SET_INTERVAL_STRING_HANDLER_ERROR,
  SET_TIMEOUT_NOT_CALLABLE_ERROR, SET_TIMEOUT_STRING_HANDLER_ERROR,
};
use crate::js::{DomHost, DocumentHostState, TimerId, Url, UrlLimits, UrlSearchParams, WindowRealmHost};
use crate::web::events as web_events;
use std::any::TypeId;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;
use vm_js::{
  GcObject, HostSlots, NativeFunctionId, PropertyDescriptor, PropertyKey, PropertyKind, RootId,
  Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};
use crate::web::dom::DomException;
use webidl_vm_js::bindings_runtime::BindingValue;
use webidl_vm_js::bindings_runtime::to_uint32_f64;
use webidl_vm_js::{IterableKind, VmJsHostHooksPayload, WebIdlBindingsHost};

const URL_INVALID_ERROR: &str = "Invalid URL";
const URLSP_ITER_VALUES_SLOT: &str = "__fastrender_urlsp_iter_values";
const URLSP_ITER_INDEX_SLOT: &str = "__fastrender_urlsp_iter_index";
const URLSP_ITER_LEN_SLOT: &str = "__fastrender_urlsp_iter_len";
const URL_SEARCH_PARAMS_SLOT: &str = "__fastrender_url_searchParams";
const ELEMENT_CLASS_LIST_PLACEHOLDER_SLOT: &str = "__fastrender_element_class_list_placeholder";
const DOM_TOKEN_LIST_HOST_TAG: u64 = u64::from_be_bytes(*b"FRDOMDTL");
const RANGE_HOST_TAG: u64 = u64::from_be_bytes(*b"FRDOMRNG");
// Must match `window_realm::HOST_OBJECT_ATTR`.
const ATTR_HOST_TAG: u64 = u64::from_be_bytes(*b"FRDOMATR");
// Must match `window_realm::STATIC_RANGE_BRAND_KEY`.
const STATIC_RANGE_BRAND_KEY: &str = "__fastrender_static_range";
// Must match `window_realm::STATIC_RANGE_START_CONTAINER_KEY`.
const STATIC_RANGE_START_CONTAINER_KEY: &str = "__fastrender_static_range_start_container";
// Must match `window_realm::STATIC_RANGE_START_OFFSET_KEY`.
const STATIC_RANGE_START_OFFSET_KEY: &str = "__fastrender_static_range_start_offset";
// Must match `window_realm::STATIC_RANGE_END_CONTAINER_KEY`.
const STATIC_RANGE_END_CONTAINER_KEY: &str = "__fastrender_static_range_end_container";
// Must match `window_realm::STATIC_RANGE_END_OFFSET_KEY`.
const STATIC_RANGE_END_OFFSET_KEY: &str = "__fastrender_static_range_end_offset";
const NODE_ITERATOR_HOST_TAG: u64 = u64::from_be_bytes(*b"FRDOMNIT");
const TREE_WALKER_HOST_TAG: u64 = u64::from_be_bytes(*b"FRDOMTWK");
const DOM_HOST_NOT_AVAILABLE_ERROR: &str = "DOM host not available";
const CSS_STYLE_DECL_HOST_TAG: u64 = u64::from_be_bytes(*b"FRDOMCSS");
// Must match `window_realm::NODE_LIST_ROOT_KEY`.
//
// Note: `dom_internal_keys` intentionally centralises most `__fastrender_*` keys, but the NodeList
// root back-reference is currently only needed by the vm-js DOM shims and the WebIDL host dispatch.
const NODE_LIST_ROOT_KEY: &str = "__fastrender_node_list_root";
const NODE_FILTER_ACCEPT: u16 = 1;
const NODE_FILTER_REJECT: u16 = 2;
const NODE_FILTER_SKIP: u16 = 3;
const TRAVERSAL_ACTIVE_SLOT: &str = "__fastrender_traversal_active";
const TRAVERSAL_WHAT_TO_SHOW_SLOT: &str = "__fastrender_traversal_what_to_show";
const TRAVERSAL_FILTER_SLOT: &str = "__fastrender_traversal_filter";
const TREE_WALKER_ROOT_SLOT: &str = "__fastrender_tree_walker_root";
const TREE_WALKER_CURRENT_SLOT: &str = "__fastrender_tree_walker_current";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UrlSearchParamsIteratorKind {
  Entries,
  Keys,
  Values,
}

fn url_search_params_iterator_kind(
  operation: &str,
) -> Result<UrlSearchParamsIteratorKind, VmError> {
  match operation {
    "entries" => Ok(UrlSearchParamsIteratorKind::Entries),
    "keys" => Ok(UrlSearchParamsIteratorKind::Keys),
    "values" => Ok(UrlSearchParamsIteratorKind::Values),
    _ => Err(VmError::TypeError("URLSearchParams iterator kind mismatch")),
  }
}

#[cfg(test)]
mod url_search_params_iterator_kind_tests {
  use super::*;

  #[test]
  fn url_search_params_iterator_kind_returns_error_for_unknown_operation() {
    let err = url_search_params_iterator_kind("bogus").unwrap_err();
    assert!(matches!(err, VmError::TypeError(_)));
  }
}

fn should_delegate_dom_interface(interface: &'static str) -> bool {
  matches!(
    interface,
    "Node"
      | "Element"
      | "Document"
      | "DocumentFragment"
      | "Attr"
      | "NamedNodeMap"
      | "NodeList"
      | "HTMLCollection"
      | "DOMTokenList"
  )
}

#[derive(Debug, Clone, Copy)]
struct RootedCallback {
  value: Value,
  root: RootId,
}

#[derive(Debug, Clone, Copy)]
struct RootedValue {
  value: Value,
  root: RootId,
}

#[derive(Debug)]
struct TimerEntry {
  callback: RootedCallback,
  args: Vec<RootId>,
}

#[derive(Debug, Clone)]
struct EventListenerEntry {
  event_type: String,
  callback: RootedCallback,
  capture: bool,
  once: bool,
}

#[derive(Debug, Default)]
struct EventTargetState {
  parent: Option<RootedValue>,
  listeners: Vec<EventListenerEntry>,
}

#[derive(Debug, Clone)]
enum LiveHtmlCollectionKind {
  ChildrenElements,
  TagName { qualified_name: String },
  TagNameNS {
    namespace: Option<String>,
    local_name: String,
  },
  ClassName { class_names: String },
  Name { name: String },
}

#[derive(Debug, Clone)]
struct LiveHtmlCollection {
  weak_obj: WeakGcObject,
  document_id: DocumentId,
  root: NodeId,
  kind: LiveHtmlCollectionKind,
}

#[derive(Debug, Clone, Copy)]
struct RangeState {
  document_id: DocumentId,
  range_id: RangeId,
}

enum DomHostAdapter<'a, Host: DomHost + 'static> {
  Embedder(&'a mut Host),
  DocumentHost(&'a mut DocumentHostState),
  BrowserDocument(&'a mut BrowserDocumentDom2),
}

impl<Host: DomHost + 'static> DomHost for DomHostAdapter<'_, Host> {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&crate::dom2::Document) -> R,
  {
    match self {
      Self::Embedder(host) => host.with_dom(f),
      Self::DocumentHost(host) => host.with_dom(f),
      Self::BrowserDocument(host) => <BrowserDocumentDom2 as DomHost>::with_dom(*host, f),
    }
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut crate::dom2::Document) -> (R, bool),
  {
    match self {
      Self::Embedder(host) => host.mutate_dom(f),
      Self::DocumentHost(host) => host.mutate_dom(f),
      Self::BrowserDocument(host) => <BrowserDocumentDom2 as DomHost>::mutate_dom(host, f),
    }
  }
}

fn is_callable(scope: &Scope<'_>, value: Value) -> bool {
  scope.heap().is_callable(value).unwrap_or(false)
}

fn with_active_vm_host_and_hooks<R>(
  vm: &mut Vm,
  f: impl FnOnce(&mut Vm, &mut dyn VmHost, &mut dyn VmHostHooks) -> Result<R, VmError>,
) -> Result<Option<R>, VmError> {
  let Some(hooks_ptr) = vm.active_host_hooks_ptr() else {
    return Ok(None);
  };
  // SAFETY: the returned pointer is only exposed by `vm-js` while an embedder-owned `VmHostHooks`
  // value is mutably borrowed for a single JS execution boundary.
  let hooks = unsafe { &mut *hooks_ptr };
  let host_ptr = {
    let Some(any) = hooks.as_any_mut() else {
      return Ok(None);
    };
    let any_ptr: *mut dyn std::any::Any = any;
    // SAFETY: `any_ptr` is derived from `hooks.as_any_mut()` and is only used within this block.
    unsafe {
      (&mut *any_ptr)
        .downcast_mut::<VmJsHostHooksPayload>()
        .and_then(|payload| payload.vm_host_ptr())
    }
  };
  let Some(mut host_ptr) = host_ptr else {
    return Ok(None);
  };
  // SAFETY: the embedder is responsible for ensuring the host pointer remains valid for the
  // duration of the JS execution boundary where it was installed.
  let host = unsafe { host_ptr.as_mut() };
  Ok(Some(f(vm, host, hooks)?))
}

fn get_with_active_vm_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  obj: GcObject,
  key: PropertyKey,
) -> Result<Value, VmError> {
  if let Some(value) = with_active_vm_host_and_hooks(vm, |vm, host, hooks| {
    vm.get_with_host_and_hooks(host, scope, hooks, obj, key)
  })? {
    Ok(value)
  } else {
    vm.get(scope, obj, key)
  }
}

fn call_with_active_vm_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: Value,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if let Some(value) = with_active_vm_host_and_hooks(vm, |vm, host, hooks| {
    vm.call_with_host_and_hooks(host, scope, hooks, callee, this, args)
  })? {
    Ok(value)
  } else {
    vm.call_without_host(scope, callee, this, args)
  }
}

fn with_active_vm_host<R>(
  vm: &mut Vm,
  f: impl FnOnce(&mut dyn VmHost) -> Result<R, VmError>,
) -> Result<R, VmError> {
  match with_active_vm_host_and_hooks(vm, |_vm, host, _hooks| f(host))? {
    Some(value) => Ok(value),
    None => Err(VmError::TypeError("DOM host not available")),
  }
}

fn dom_platform_mut(vm: &mut Vm) -> Option<&mut DomPlatform> {
  vm
    .user_data_mut::<WindowRealmUserData>()
    .and_then(|data| data.dom_platform_mut())
}

fn gc_object_id(obj: GcObject) -> u64 {
  (obj.index() as u64) | ((obj.generation() as u64) << 32)
}

fn require_element_receiver(
  vm: &mut Vm,
  scope: &Scope<'_>,
  receiver: Option<Value>,
) -> Result<(NodeId, GcObject), VmError> {
  let Some(Value::Object(obj)) = receiver else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
  let node_id = platform.require_element_id(scope.heap(), Value::Object(obj))?;
  Ok((node_id, obj))
}

fn sync_dom_node_collection_object(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  collection_obj: GcObject,
  document_id: DocumentId,
  nodes: &[(NodeId, DomInterface)],
) -> Result<(), VmError> {
  scope.push_root(Value::Object(collection_obj))?;

  let internal_length_key = key_from_str(scope, COLLECTION_LENGTH_KEY)?;
  let length_key = key_from_str(scope, "length")?;

  let internal_length_value = scope
    .heap()
    .object_get_own_data_property_value(collection_obj, &internal_length_key)?;
  let (internal_length_present, internal_len) = match internal_length_value {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => (true, Some(n as usize)),
    Some(_) => (true, None),
    None => (false, None),
  };

  let length_value = match scope
    .heap()
    .object_get_own_data_property_value(collection_obj, &length_key)
  {
    Ok(value) => value,
    Err(VmError::PropertyNotData) => None,
    Err(err) => return Err(err),
  };
  let (length_present, length_len) = match length_value {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => (true, Some(n as usize)),
    Some(_) => (true, None),
    None => (false, None),
  };

  let old_len = internal_len.or(length_len).unwrap_or(0);

  for (idx, (child_id, primary)) in nodes.iter().copied().enumerate() {
    let child_wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
      scope,
      document_id,
      child_id,
      primary,
    )?;
    scope.push_root(Value::Object(child_wrapper))?;

    let idx_key = key_from_str(scope, &idx.to_string())?;
    scope.define_property(
      collection_obj,
      idx_key,
      data_property(Value::Object(child_wrapper), true, true, true),
    )?;
  }

  for idx in nodes.len()..old_len {
    let idx_key = key_from_str(scope, &idx.to_string())?;
    scope.heap_mut().delete_property_or_throw(collection_obj, idx_key)?;
  }

  if internal_length_present {
    scope.define_property(
      collection_obj,
      internal_length_key,
      data_property(Value::Number(nodes.len() as f64), true, false, false),
    )?;
  }
  if length_present {
    scope.define_property(
      collection_obj,
      length_key,
      data_property(Value::Number(nodes.len() as f64), true, false, false),
    )?;
  }

  Ok(())
}

fn sync_cached_child_nodes_for_wrapper(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  wrapper_obj: GcObject,
  document_id: DocumentId,
  node_id: NodeId,
  dom: &crate::dom2::Document,
) -> Result<(), VmError> {
  if node_id.index() >= dom.nodes_len() {
    return Ok(());
  }

  let mut children: Vec<(NodeId, DomInterface)> = Vec::new();
  children
    .try_reserve(dom.node(node_id).children.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for &child_id in dom.node(node_id).children.iter() {
    if child_id.index() >= dom.nodes_len() {
      continue;
    }
    let child = dom.node(child_id);
    if child.parent != Some(node_id) {
      continue;
    }
    if matches!(child.kind, NodeKind::ShadowRoot { .. }) {
      continue;
    }
    let primary = DomInterface::primary_for_node_kind(&child.kind);
    children.push((child_id, primary));
  }

  sync_cached_child_nodes_for_wrapper_with_nodes(vm, scope, wrapper_obj, document_id, &children)
}

fn sync_cached_child_nodes_for_wrapper_with_nodes(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  wrapper_obj: GcObject,
  document_id: DocumentId,
  children: &[(NodeId, DomInterface)],
) -> Result<(), VmError> {
  let child_nodes_key = key_from_str(scope, NODE_CHILD_NODES_KEY)?;
  let Some(Value::Object(list_obj)) = scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &child_nodes_key)?
  else {
    return Ok(());
  };

  sync_dom_node_collection_object(vm, scope, list_obj, document_id, &children)
}
fn require_dom_token_list_receiver(
  scope: &Scope<'_>,
  receiver: Option<Value>,
) -> Result<(NodeId, GcObject), VmError> {
  let Some(Value::Object(obj)) = receiver else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let slots = match scope.heap().object_host_slots(obj) {
    Ok(slots) => slots,
    Err(VmError::InvalidHandle { .. }) if scope.heap().is_valid_object(obj) => None,
    Err(err) => return Err(err),
  };
  if !matches!(slots, Some(slots) if slots.b == DOM_TOKEN_LIST_HOST_TAG) {
    return Err(VmError::TypeError("Illegal invocation"));
  }
  let node_index = usize::try_from(slots.unwrap().a) // fastrender-allow-unwrap
    .map_err(|_| VmError::TypeError("Illegal invocation"))?;
  Ok((NodeId::from_index(node_index), obj))
}

fn require_node_iterator_receiver(
  scope: &Scope<'_>,
  receiver: Option<Value>,
) -> Result<(NodeIteratorId, GcObject), VmError> {
  let Some(Value::Object(obj)) = receiver else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let slots = match scope.heap().object_host_slots(obj) {
    Ok(slots) => slots,
    Err(VmError::InvalidHandle { .. }) if scope.heap().is_valid_object(obj) => None,
    Err(err) => return Err(err),
  };
  if !matches!(slots, Some(slots) if slots.b == NODE_ITERATOR_HOST_TAG) {
    return Err(VmError::TypeError("Illegal invocation"));
  }
  Ok((NodeIteratorId::from_u64(slots.unwrap().a), obj)) // fastrender-allow-unwrap
}

fn require_tree_walker_receiver(
  scope: &Scope<'_>,
  receiver: Option<Value>,
) -> Result<GcObject, VmError> {
  let Some(Value::Object(obj)) = receiver else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let slots = match scope.heap().object_host_slots(obj) {
    Ok(slots) => slots,
    Err(VmError::InvalidHandle { .. }) if scope.heap().is_valid_object(obj) => None,
    Err(err) => return Err(err),
  };
  if !matches!(slots, Some(slots) if slots.b == TREE_WALKER_HOST_TAG) {
    return Err(VmError::TypeError("Illegal invocation"));
  }
  Ok(obj)
}

fn require_range_receiver(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  receiver: Option<Value>,
) -> Result<(RangeId, DocumentId), VmError> {
  let Some(Value::Object(obj)) = receiver else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let slots = match scope.heap().object_host_slots(obj) {
    Ok(slots) => slots,
    Err(VmError::InvalidHandle { .. }) if scope.heap().is_valid_object(obj) => None,
    Err(err) => return Err(err),
  };
  if !matches!(slots, Some(slots) if slots.b == RANGE_HOST_TAG) {
    return Err(VmError::TypeError("Illegal invocation"));
  }
  let range_id = RangeId::from_u64(slots.unwrap().a); // fastrender-allow-unwrap

  // Range wrappers store their owning document wrapper so host dispatch can validate cross-document
  // operations and route to owned documents when present.
  let wrapper_document_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
  let Some(Value::Object(document_obj)) =
    scope.heap().object_get_own_data_property_value(obj, &wrapper_document_key)?
  else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
  let document_handle = platform.require_document_handle(scope.heap(), Value::Object(document_obj))?;

  Ok((range_id, document_handle.document_id))
}

fn read_internal_node_id_slot(
  scope: &Scope<'_>,
  obj: GcObject,
  key: &PropertyKey,
) -> Result<NodeId, VmError> {
  let Some(Value::Number(n)) = scope.heap().object_get_own_data_property_value(obj, key)? else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  if !n.is_finite() || n < 0.0 || n > (usize::MAX as f64) {
    return Err(VmError::TypeError("Illegal invocation"));
  }
  Ok(NodeId::from_index(n.trunc() as usize))
}

fn node_kind_to_node_type(kind: &NodeKind) -> u32 {
  match kind {
    NodeKind::Document { .. } => 9,
    NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => 11,
    NodeKind::Text { .. } => 3,
    NodeKind::Comment { .. } => 8,
    NodeKind::ProcessingInstruction { .. } => 7,
    NodeKind::Doctype { .. } => 10,
    NodeKind::Element { .. } | NodeKind::Slot { .. } => 1,
  }
}

fn tree_parent_node(dom: &crate::dom2::Document, node: NodeId) -> Option<NodeId> {
  dom.tree_parent_node(node)
}

fn tree_first_child(dom: &crate::dom2::Document, node: NodeId) -> Option<NodeId> {
  dom.first_tree_child(node)
}

fn tree_last_child(dom: &crate::dom2::Document, node: NodeId) -> Option<NodeId> {
  dom.last_tree_child(node)
}

fn tree_next_sibling(dom: &crate::dom2::Document, node: NodeId) -> Option<NodeId> {
  dom.tree_next_sibling(node)
}

fn tree_previous_sibling(dom: &crate::dom2::Document, node: NodeId) -> Option<NodeId> {
  dom.tree_previous_sibling(node)
}

fn tree_following_in_subtree(dom: &crate::dom2::Document, root: NodeId, node: NodeId) -> Option<NodeId> {
  dom.tree_following_in_subtree(root, node)
}

fn tree_preceding_in_subtree(dom: &crate::dom2::Document, root: NodeId, node: NodeId) -> Option<NodeId> {
  dom.tree_preceding_in_subtree(root, node)
}

fn to_uint16_f64(n: f64) -> u16 {
  if !n.is_finite() || n == 0.0 {
    return 0;
  }
  let n = n.trunc();
  let modulo = 65536.0;
  let mut int = n % modulo;
  if int < 0.0 {
    int += modulo;
  }
  int as u16
}

fn traversal_filter_node<Host: WindowRealmHost + DomHost + 'static>(
  dispatch: &mut VmJsWebIdlBindingsHostDispatch<Host>,
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  dom_exception: DomExceptionClassVmJs,
  traverser_obj: GcObject,
  node_id: NodeId,
  document_id: DocumentId,
  what_to_show_key: PropertyKey,
  filter_key: PropertyKey,
  active_key: PropertyKey,
) -> Result<u16, VmError> {
  // Step 1: re-entrancy guard.
  let is_active = match scope
    .heap()
    .object_get_own_data_property_value(traverser_obj, &active_key)?
  {
    Some(Value::Bool(b)) => b,
    Some(v) => scope.heap().to_boolean(v)?,
    None => false,
  };
  if is_active {
    return Err(throw_dom_exception(scope, dom_exception, "InvalidStateError", ""));
  }

  // Step 2: nodeType.
  let node_type = dispatch.with_dom_host(vm, |host| {
    Ok(host.with_dom(|dom| {
      if node_id.index() >= dom.nodes_len() {
        return None;
      }
      Some(node_kind_to_node_type(&dom.node(node_id).kind))
    }))
  })?;
  let Some(node_type) = node_type else {
    return Ok(NODE_FILTER_SKIP);
  };

  // Step 3: whatToShow bit check.
  let what_to_show = match scope
    .heap()
    .object_get_own_data_property_value(traverser_obj, &what_to_show_key)?
  {
    Some(Value::Number(n)) if n.is_finite() => {
      let n = n.trunc();
      if n <= 0.0 {
        0u32
      } else if n >= u32::MAX as f64 {
        u32::MAX
      } else {
        n as u32
      }
    }
    Some(v) => {
      let n = scope.heap_mut().to_number(v)?;
      if !n.is_finite() {
        0u32
      } else {
        let n = n.trunc();
        if n <= 0.0 {
          0u32
        } else if n >= u32::MAX as f64 {
          u32::MAX
        } else {
          n as u32
        }
      }
    }
    None => 0,
  };

  if node_type == 0 {
    return Ok(NODE_FILTER_SKIP);
  }
  let n = node_type - 1;
  if n >= 32 {
    return Ok(NODE_FILTER_SKIP);
  }
  if (what_to_show & (1u32 << n)) == 0 {
    return Ok(NODE_FILTER_SKIP);
  }

  // Step 4: if filter is null, accept.
  let filter = scope
    .heap()
    .object_get_own_data_property_value(traverser_obj, &filter_key)?
    .unwrap_or(Value::Null);
  if matches!(filter, Value::Null | Value::Undefined) {
    return Ok(NODE_FILTER_ACCEPT);
  }

  // Step 5+: call the user-provided filter callback with active flag protection.
  scope.define_property(
    traverser_obj,
    active_key,
    data_property(Value::Bool(true), true, false, false),
  )?;
  struct TraversalActiveGuard<'a> {
    scope: *mut Scope<'a>,
    traverser_obj: GcObject,
    key: PropertyKey,
  }
  impl Drop for TraversalActiveGuard<'_> {
    fn drop(&mut self) {
      // SAFETY: `scope` originates from the active native call; this guard never escapes.
      let scope = unsafe { &mut *self.scope };
      let _ = scope.define_property(
        self.traverser_obj,
        self.key,
        data_property(Value::Bool(false), true, false, false),
      );
    }
  }
  let _active_guard = TraversalActiveGuard {
    scope,
    traverser_obj,
    key: active_key,
  };

  // Resolve the node wrapper to pass to the callback.
  let primary = dispatch.with_dom_host(vm, |host| {
    Ok(host.with_dom(|dom| {
      if node_id.index() >= dom.nodes_len() {
        DomInterface::Node
      } else {
        DomInterface::primary_for_node_kind(&dom.node(node_id).kind)
      }
    }))
  })?;
  let wrapper = {
    let platform = require_dom_platform_mut(vm)?;
    platform.get_or_create_wrapper_for_document_id(scope, document_id, node_id, primary)?
  };
  scope.push_root(Value::Object(wrapper))?;

  let callback_result: Result<Value, VmError> = (|| {
    if is_callable(scope, filter) {
      return call_with_active_vm_host_and_hooks(
        vm,
        scope,
        filter,
        // WebIDL callback interfaces: callable callbacks are invoked with `this = undefined`.
        Value::Undefined,
        &[Value::Object(wrapper)],
      );
    }

    let Value::Object(filter_obj) = filter else {
      return Err(VmError::TypeError("NodeFilter is not an object"));
    };

    // WebIDL callback interfaces: call `filter.acceptNode(node)` with `this = filter`.
    let accept_node_key = key_from_str(scope, "acceptNode")?;
    let method = get_with_active_vm_host_and_hooks(vm, scope, filter_obj, accept_node_key)?;
    if !is_callable(scope, method) {
      return Err(VmError::TypeError(
        "NodeFilter callback has no callable acceptNode",
      ));
    }
    scope.push_root(method)?;
    call_with_active_vm_host_and_hooks(vm, scope, method, filter, &[Value::Object(wrapper)])
  })();

  let callback_value = callback_result?;
  // Root the callback return value: numeric conversion can trigger user code + GC.
  scope.push_root(callback_value)?;
  let n = scope.heap_mut().to_number(callback_value)?;
  let result = to_uint16_f64(n);
  // DOM Standard: values other than 1/2/3 are treated as FILTER_SKIP.
  Ok(match result {
    NODE_FILTER_ACCEPT | NODE_FILTER_REJECT | NODE_FILTER_SKIP => result,
    _ => NODE_FILTER_SKIP,
  })
}

fn mutate_dom_detached<R>(
  host: &mut dyn VmHost,
  f: impl FnOnce(&mut crate::dom2::Document) -> R,
) -> Result<R, VmError> {
  // `Document.createElement` / `createTextNode` / `createDocumentFragment` allocate detached nodes.
  // They grow the `dom2` node arena but do not change the live document tree until insertion, so we
  // report `changed=false` to avoid triggering renderer invalidation.
  let any = host.as_any_mut();
  if let Some(host) = any.downcast_mut::<DocumentHostState>() {
    return Ok(DomHost::mutate_dom(host, |dom| (f(dom), false)));
  }
  if let Some(host) = any.downcast_mut::<BrowserDocumentDom2>() {
    return Ok(DomHost::mutate_dom(host, |dom| (f(dom), false)));
  }
  Err(VmError::TypeError("DOM host not available"))
}

fn urlsp_iterator_next_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(iter_obj) = this else {
    return Err(VmError::TypeError(
      "URLSearchParams iterator.next: illegal invocation",
    ));
  };

  let intr = vm
    .intrinsics()
    .ok_or(VmError::InvariantViolation("missing intrinsics"))?;

  let values_key = key_from_str(scope, URLSP_ITER_VALUES_SLOT)?;
  let Some(Value::Object(values_obj)) = scope
    .heap()
    .object_get_own_data_property_value(iter_obj, &values_key)?
  else {
    return Err(VmError::TypeError(
      "URLSearchParams iterator.next: missing values",
    ));
  };

  let index_key = key_from_str(scope, URLSP_ITER_INDEX_SLOT)?;
  let Some(Value::Number(index)) = scope
    .heap()
    .object_get_own_data_property_value(iter_obj, &index_key)?
  else {
    return Err(VmError::TypeError(
      "URLSearchParams iterator.next: missing index",
    ));
  };
  if !index.is_finite() || index < 0.0 || index > u32::MAX as f64 {
    return Err(VmError::TypeError(
      "URLSearchParams iterator.next: invalid index",
    ));
  }
  let idx_u32 = index as u32;
  let idx_usize = idx_u32 as usize;

  let len_key = key_from_str(scope, URLSP_ITER_LEN_SLOT)?;
  let Some(Value::Number(len)) = scope
    .heap()
    .object_get_own_data_property_value(iter_obj, &len_key)?
  else {
    return Err(VmError::TypeError(
      "URLSearchParams iterator.next: missing length",
    ));
  };
  if !len.is_finite() || len < 0.0 || len > u32::MAX as f64 {
    return Err(VmError::TypeError(
      "URLSearchParams iterator.next: invalid length",
    ));
  }
  let len_u32 = len as u32;
  let len_usize = len_u32 as usize;

  let (done, value) = if idx_usize >= len_usize {
    (true, Value::Undefined)
  } else {
    let idx_key = key_from_str(scope, &idx_u32.to_string())?;
    let value = scope
      .heap()
      .object_get_own_data_property_value(values_obj, &idx_key)?
      .unwrap_or(Value::Undefined);

    // Update iterator index.
    scope.define_property(
      iter_obj,
      index_key,
      data_property(Value::Number((idx_usize + 1) as f64), true, false, true),
    )?;

    (false, value)
  };

  let result_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(result_obj))?;
  let value_key = key_from_str(scope, "value")?;
  let done_key = key_from_str(scope, "done")?;
  scope.define_property(
    result_obj,
    value_key,
    data_property(value, true, true, true),
  )?;
  scope.define_property(
    result_obj,
    done_key,
    data_property(Value::Bool(done), true, true, true),
  )?;
  Ok(Value::Object(result_obj))
}

fn iterator_return_self_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(this)
}

fn data_property(
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

fn key_from_str(scope: &mut Scope<'_>, s: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(s)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}
fn require_dom_platform_mut(vm: &mut Vm) -> Result<&mut DomPlatform, VmError> {
  dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))
}

fn js_string_to_rust_string(scope: &Scope<'_>, value: Value) -> Result<String, VmError> {
  let Value::String(s) = value else {
    return Err(VmError::TypeError("expected string"));
  };
  Ok(scope.heap().get_string(s)?.to_utf8_lossy())
}

fn is_valid_create_element_local_name(name: &str) -> bool {
  let mut chars = name.chars();
  let Some(first) = chars.next() else {
    return false;
  };

  // Fast path: ASCII-alpha first char uses the DOM "valid element local name" byte blacklist.
  if first.is_ascii_alphabetic() {
    return !name.bytes().any(|b| {
      matches!(b, b'\t' | b'\n' | 0x0C | b'\r' | b' ' | b'\0' | b'/' | b'>')
    });
  }

  // Full path: match `dom2`'s `is_valid_element_local_name` rules (non-ASCII and certain punct).
  if !(first == ':' || first == '_' || (first as u32) >= 0x80) {
    return false;
  }
  for ch in chars {
    if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | ':' | '_') || (ch as u32) >= 0x80 {
      continue;
    }
    return false;
  }
  true
}

fn array_length(vm: &mut Vm, scope: &mut Scope<'_>, array: GcObject) -> Result<usize, VmError> {
  let length_key = key_from_str(scope, "length")?;
  let len = get_with_active_vm_host_and_hooks(vm, scope, array, length_key)?;
  match len {
    Value::Number(n) if n.is_finite() && n >= 0.0 => Ok(n as usize),
    _ => Err(VmError::TypeError(
      "URLSearchParams init array length is not a number",
    )),
  }
}

fn array_get(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  array: GcObject,
  idx: usize,
) -> Result<Value, VmError> {
  let key = key_from_str(scope, &idx.to_string())?;
  get_with_active_vm_host_and_hooks(vm, scope, array, key)
}

fn url_parse_result_to_vm_error(err: crate::js::UrlError) -> VmError {
  match err {
    crate::js::UrlError::OutOfMemory => VmError::OutOfMemory,
    _ => VmError::TypeError(URL_INVALID_ERROR),
  }
}

fn url_search_params_error_to_vm_error(err: crate::js::UrlError) -> VmError {
  match err {
    crate::js::UrlError::OutOfMemory => VmError::OutOfMemory,
    _ => VmError::TypeError("URLSearchParams error"),
  }
}

fn dom_exception_class(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  global: GcObject,
) -> Result<DomExceptionClassVmJs, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::InvariantViolation("missing intrinsics"))?;
  DomExceptionClassVmJs::install_for_global(vm, scope, global, intr)
}

fn throw_dom_exception(
  scope: &mut Scope<'_>,
  class: DomExceptionClassVmJs,
  name: &str,
  message: &str,
) -> VmError {
  match class.new_instance(scope, name, message) {
    Ok(value) => VmError::Throw(value),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

fn throw_dom_error(
  scope: &mut Scope<'_>,
  class: DomExceptionClassVmJs,
  err: crate::dom2::DomError,
) -> VmError {
  throw_dom_exception(scope, class, err.code(), "")
}

fn normalize_delay_ms(value: Value) -> u64 {
  let Value::Number(n) = value else {
    return 0;
  };
  if !n.is_finite() {
    return 0;
  }
  let n = n.trunc();
  if n <= 0.0 {
    0
  } else if n >= u64::MAX as f64 {
    u64::MAX
  } else {
    n as u64
  }
}

fn normalize_timer_id(value: Value) -> TimerId {
  let Value::Number(n) = value else {
    return 0;
  };
  if !n.is_finite() {
    return 0;
  }
  let n = n.trunc();
  if n >= i32::MAX as f64 {
    i32::MAX
  } else if n <= i32::MIN as f64 {
    i32::MIN
  } else {
    n as i32
  }
}

fn finite_f64_to_f32_or_zero(n: f64) -> f32 {
  if !n.is_finite() {
    return 0.0;
  }
  let min = f32::MIN as f64;
  let max = f32::MAX as f64;
  n.clamp(min, max) as f32
}

fn layout_metric_f32_to_f64_or_zero(n: f32) -> f64 {
  if n.is_finite() { n as f64 } else { 0.0 }
}

fn layout_metric_nonneg_f32_to_f64_or_zero(n: f32) -> f64 {
  if n.is_finite() { n.max(0.0) as f64 } else { 0.0 }
}

fn get_capture_option(scope: &mut Scope<'_>, value: Value) -> Result<bool, VmError> {
  match value {
    Value::Bool(b) => Ok(b),
    Value::Object(obj) => {
      // Minimal interpretation: read an *own data property* named "capture" if present.
      let key = key_from_str(scope, "capture")?;
      let Some(v) = scope.heap().object_get_own_data_property_value(obj, &key)? else {
        return Ok(false);
      };
      Ok(scope.heap().to_boolean(v)?)
    }
    _ => Ok(false),
  }
}

fn get_once_option(scope: &mut Scope<'_>, value: Value) -> Result<bool, VmError> {
  let Value::Object(obj) = value else {
    return Ok(false);
  };
  let key = key_from_str(scope, "once")?;
  let Some(v) = scope.heap().object_get_own_data_property_value(obj, &key)? else {
    return Ok(false);
  };
  Ok(scope.heap().to_boolean(v)?)
}

pub struct VmJsWebIdlBindingsHostDispatch<Host: WindowRealmHost + 'static> {
  global: Option<GcObject>,
  limits: UrlLimits,
  urls: HashMap<WeakGcObject, Url>,
  params: HashMap<WeakGcObject, UrlSearchParams>,
  ranges: HashMap<WeakGcObject, RangeState>,
  event_targets: HashMap<WeakGcObject, EventTargetState>,
  live_html_collections: Vec<LiveHtmlCollection>,
  abort_signal_listener_cleanup_call: Option<NativeFunctionId>,
  timer_registry: Rc<RefCell<HashMap<TimerId, TimerEntry>>>,
  urlsp_iterator_next_call: Option<NativeFunctionId>,
  urlsp_iterator_iterator_call: Option<NativeFunctionId>,
  last_gc_runs: u64,
  _marker: PhantomData<fn() -> Host>,
}

impl<Host: WindowRealmHost + 'static> VmJsWebIdlBindingsHostDispatch<Host> {
  pub fn new(global: GcObject) -> Self {
    Self {
      global: Some(global),
      limits: UrlLimits::default(),
      urls: HashMap::new(),
      params: HashMap::new(),
      ranges: HashMap::new(),
      event_targets: HashMap::new(),
      live_html_collections: Vec::new(),
      abort_signal_listener_cleanup_call: None,
      timer_registry: Rc::new(RefCell::new(HashMap::new())),
      urlsp_iterator_next_call: None,
      urlsp_iterator_iterator_call: None,
      last_gc_runs: 0,
      _marker: PhantomData,
    }
  }

  pub fn new_without_global() -> Self {
    Self {
      global: None,
      limits: UrlLimits::default(),
      urls: HashMap::new(),
      params: HashMap::new(),
      ranges: HashMap::new(),
      event_targets: HashMap::new(),
      live_html_collections: Vec::new(),
      abort_signal_listener_cleanup_call: None,
      timer_registry: Rc::new(RefCell::new(HashMap::new())),
      urlsp_iterator_next_call: None,
      urlsp_iterator_iterator_call: None,
      last_gc_runs: 0,
      _marker: PhantomData,
    }
  }

  pub fn reset_for_new_realm(&mut self, global: GcObject) {
    // `WeakGcObject` / `RootId` values are heap-specific; discard all prior state on navigation.
    self.global = Some(global);
    self.urls.clear();
    self.params.clear();
    self.ranges.clear();
    self.event_targets.clear();
    self.live_html_collections.clear();
    self.timer_registry.borrow_mut().clear();
    self.urlsp_iterator_next_call = None;
    self.urlsp_iterator_iterator_call = None;
    self.abort_signal_listener_cleanup_call = None;
    self.last_gc_runs = 0;
  }

  fn abort_signal_listener_cleanup_call_id(
    &mut self,
    vm: &mut Vm,
  ) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.abort_signal_listener_cleanup_call {
      return Ok(id);
    }
    let id = vm.register_native_call(abort_signal_listener_cleanup_native)?;
    self.abort_signal_listener_cleanup_call = Some(id);
    Ok(id)
  }

  fn dom_exception_class_for_realm(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
  ) -> Result<DomExceptionClassVmJs, VmError> {
    let global = if let Some(global) = self.global {
      global
    } else {
      vm
        .user_data_mut::<WindowRealmUserData>()
        .and_then(|data| data.window_obj())
        .ok_or(VmError::TypeError("DOMException global not available"))?
    };
    dom_exception_class(vm, scope, global)
  }

  fn is_dom_backed_event_target(
    vm: &mut Vm,
    heap: &vm_js::Heap,
    receiver_obj: GcObject,
  ) -> Result<bool, VmError> {
    let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
      return Ok(false);
    };

    let window_obj = data.window_obj();
    let document_obj = data.document_obj();

    if window_obj == Some(receiver_obj) || document_obj == Some(receiver_obj) {
      return Ok(true);
    }

    let Some(platform) = data.dom_platform_mut() else {
      return Ok(false);
    };

    match platform.event_target_id_for_value(heap, Value::Object(receiver_obj)) {
      Ok(_) => Ok(true),
      Err(VmError::OutOfMemory) => Err(VmError::OutOfMemory),
      Err(_) => Ok(false),
    }
  }

  fn maybe_sweep(&mut self, vm: &mut Vm, heap: &mut vm_js::Heap)
  where
    Host: DomHost,
  {
    let gc_runs = heap.gc_runs();
    if gc_runs == self.last_gc_runs {
      return;
    }
    self.last_gc_runs = gc_runs;

    self.urls.retain(|k, _| k.upgrade(heap).is_some());
    self.params.retain(|k, _| k.upgrade(heap).is_some());
    self.ranges.retain(|k, _| k.upgrade(heap).is_some());
    self
      .live_html_collections
      .retain(|coll| coll.weak_obj.upgrade(heap).is_some());

    // When an EventTarget wrapper dies, drop its listener roots.
    self.event_targets.retain(|k, state| {
      if k.upgrade(heap).is_some() {
        true
      } else {
        if let Some(parent) = state.parent {
          heap.remove_root(parent.root);
        }
        for listener in &state.listeners {
          heap.remove_root(listener.callback.root);
        }
        false
      }
    });

    // Prune per-document live traversal state (e.g. NodeIterator) associated with collected JS
    // wrappers. This must run on GC boundaries so dead traversal state does not accumulate
    // indefinitely when JS drops NodeIterator/Range/TreeWalker wrappers without creating new ones.
    //
    // Best effort: this host may not have a DOM (`with_dom_host` will fail); the realm fallback
    // document is always available when WindowRealm user data is present.
    let _ = self.with_dom_host(vm, |host| {
      host.mutate_dom(|dom| {
        dom.sweep_dead_live_traversals_if_needed(heap);
        ((), false)
      });
      Ok(())
    });
    if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
      data
        .events_dom_fallback_mut()
        .sweep_dead_live_traversals_if_needed(heap);
    }

    // Also sweep per-document `EventListenerRegistry` tables that track opaque EventTargets via weak
    // handles. This must run on GC boundaries so the registry doesn't retain dead opaque ids
    // indefinitely.
    //
    // Best effort: this host may not have a DOM (`with_dom_host` will fail); the realm fallback
    // document is always available when WindowRealm user data is present.
    let _ = self.with_dom_host(vm, |host| {
      host.with_dom(|dom| dom.events().sweep_dead_opaque_targets(heap));
      Ok(())
    });
    if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
      data
        .events_dom_fallback()
        .events()
        .sweep_dead_opaque_targets(heap);
    }
  }

  fn require_receiver_object(receiver: Option<Value>) -> Result<GcObject, VmError> {
    let Some(Value::Object(obj)) = receiver else {
      return Err(VmError::TypeError("Illegal invocation"));
    };
    Ok(obj)
  }

  fn require_event_target_receiver(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    receiver: Option<Value>,
  ) -> Result<GcObject, VmError> {
    let obj = Self::require_receiver_object(receiver)?;

    if let Some(global) = self.global {
      if obj == global {
        return Ok(obj);
      }
    }

    // WindowRealm installs `document` (and `dom2` node wrappers) via `DomPlatform` metadata tracked
    // in `WindowRealmUserData`. Accept any registered node wrapper (including the document node).
    if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
      if data.window_obj() == Some(obj) {
        return Ok(obj);
      }
      if data.document_obj() == Some(obj) {
        return Ok(obj);
      }

      if let Some(platform) = data.dom_platform_mut() {
        match platform.event_target_id_for_value(scope.heap(), Value::Object(obj)) {
          Ok(_) => return Ok(obj),
          Err(VmError::TypeError("Illegal invocation")) => {}
          Err(err) => return Err(err),
        }
      }
    }

    // AbortSignal and `new EventTarget()` instances are branded via host slots.
    //
    // Some host objects use `slots.a` for their own kind tag (e.g. AbortSignal), so we accept the
    // EventTarget tag in either slot.
    let slots = match scope.heap().object_host_slots(obj) {
      Ok(slots) => slots,
      Err(VmError::InvalidHandle { .. }) if scope.heap().is_valid_object(obj) => None,
      Err(err) => return Err(err),
    };
    if matches!(
      slots,
      Some(slots) if slots.a == EVENT_TARGET_HOST_TAG || slots.b == EVENT_TARGET_HOST_TAG
    ) {
      return Ok(obj);
    }

    Err(VmError::TypeError("Illegal invocation"))
  }

  fn require_url(&self, receiver: Option<Value>) -> Result<Url, VmError> {
    let obj = Self::require_receiver_object(receiver)?;
    self
      .urls
      .get(&WeakGcObject::from(obj))
      .cloned()
      .ok_or(VmError::TypeError("Illegal invocation"))
  }

  fn with_dom_host<R>(
    &mut self,
    vm: &mut Vm,
    f: impl FnOnce(&mut DomHostAdapter<'_, Host>) -> Result<R, VmError>,
  ) -> Result<R, VmError>
  where
    Host: DomHost,
  {
    enum DomHostSource<Host: DomHost + 'static> {
      Embedder(NonNull<Host>),
      DocumentHost(NonNull<DocumentHostState>),
      BrowserDocument(NonNull<BrowserDocumentDom2>),
    }

    let Some(hooks_ptr) = vm.active_host_hooks_ptr() else {
      return Err(VmError::TypeError(DOM_HOST_NOT_AVAILABLE_ERROR));
    };
    // SAFETY: the returned pointer is only exposed by `vm-js` while an embedder-owned `VmHostHooks`
    // value is mutably borrowed for a single JS execution boundary.
    let hooks = unsafe { &mut *hooks_ptr };
    let Some(any) = hooks.as_any_mut() else {
      return Err(VmError::TypeError(DOM_HOST_NOT_AVAILABLE_ERROR));
    };

    let payload_ptr: *mut VmJsHostHooksPayload = {
      let any_ptr: *mut dyn std::any::Any = any;
      // SAFETY: `any_ptr` is derived from `hooks.as_any_mut()` and is only used within this method.
      unsafe {
        (&mut *any_ptr)
          .downcast_mut::<VmJsHostHooksPayload>()
          .map(|payload| payload as *mut VmJsHostHooksPayload)
      }
    }
    .ok_or(VmError::TypeError(DOM_HOST_NOT_AVAILABLE_ERROR))?;

    let source = unsafe {
      let payload = &mut *payload_ptr;
      if let Some(host) = payload.embedder_state_mut::<Host>() {
        Some(DomHostSource::Embedder(NonNull::from(host)))
      } else if let Some(mut vm_host_ptr) = payload.vm_host_ptr() {
        let vm_host = vm_host_ptr.as_mut();
        let any = vm_host.as_any_mut();
        let ty = any.type_id();
        if ty == TypeId::of::<Host>() {
          let host = any.downcast_mut::<Host>().expect("checked type id"); // fastrender-allow-unwrap
          Some(DomHostSource::Embedder(NonNull::from(host)))
        } else if ty == TypeId::of::<DocumentHostState>() {
          let host = any
            .downcast_mut::<DocumentHostState>()
            .expect("checked type id"); // fastrender-allow-unwrap
          Some(DomHostSource::DocumentHost(NonNull::from(host)))
        } else if ty == TypeId::of::<BrowserDocumentDom2>() {
          let host = any
            .downcast_mut::<BrowserDocumentDom2>()
            .expect("checked type id"); // fastrender-allow-unwrap
          Some(DomHostSource::BrowserDocument(NonNull::from(host)))
        } else {
          None
        }
      } else {
        None
      }
    }
    .ok_or(VmError::TypeError(DOM_HOST_NOT_AVAILABLE_ERROR))?;

    match source {
      DomHostSource::Embedder(mut host_ptr) => {
        let host = unsafe { host_ptr.as_mut() };
        let mut adapter = DomHostAdapter::Embedder(host);
        f(&mut adapter)
      }
      DomHostSource::DocumentHost(mut host_ptr) => {
        let host = unsafe { host_ptr.as_mut() };
        let mut adapter = DomHostAdapter::DocumentHost(host);
        f(&mut adapter)
      }
      DomHostSource::BrowserDocument(mut host_ptr) => {
        let host = unsafe { host_ptr.as_mut() };
        let mut adapter = DomHostAdapter::BrowserDocument(host);
        f(&mut adapter)
      }
    }
  }

  fn create_live_html_collection(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    document_obj: GcObject,
    root_wrapper_obj: GcObject,
    document_id: DocumentId,
    root: NodeId,
    kind: LiveHtmlCollectionKind,
  ) -> Result<GcObject, VmError>
  where
    Host: DomHost,
  {
    scope.push_root(Value::Object(document_obj))?;
    scope.push_root(Value::Object(root_wrapper_obj))?;

    let collection = scope.alloc_object()?;
    scope.push_root(Value::Object(collection))?;

    let proto_key = key_from_str(scope, HTML_COLLECTION_PROTOTYPE_KEY)?;
    let proto = match scope
      .heap()
      .object_get_own_data_property_value(document_obj, &proto_key)?
    {
      Some(Value::Object(obj)) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "missing HTMLCollection prototype for DOM collection getter",
        ))
      }
    };
    scope
      .heap_mut()
      .object_set_prototype(collection, Some(proto))?;

    // Keep the root wrapper alive even if the caller only holds the collection object.
    let root_key = key_from_str(scope, HTML_COLLECTION_ROOT_KEY)?;
    scope.define_property(
      collection,
      root_key,
      data_property(Value::Object(root_wrapper_obj), false, false, false),
    )?;

    let coll = LiveHtmlCollection {
      weak_obj: WeakGcObject::from(collection),
      document_id,
      root,
      kind,
    };
    self.sync_one_html_collection(vm, scope, collection, &coll)?;
    self.live_html_collections.push(coll);
    Ok(collection)
  }

  fn sync_live_html_collections(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
  ) -> Result<(), VmError>
  where
    Host: DomHost,
  {
    if self.live_html_collections.is_empty() {
      return Ok(());
    }

    let mut collections = std::mem::take(&mut self.live_html_collections);
    let mut out: Vec<LiveHtmlCollection> = Vec::with_capacity(collections.len());

    for coll in collections.drain(..) {
      let Some(obj) = coll.weak_obj.upgrade(scope.heap()) else {
        continue;
      };
      let mut scope = scope.reborrow();
      scope.push_root(Value::Object(obj))?;
      self.sync_one_html_collection(vm, &mut scope, obj, &coll)?;
      out.push(coll);
    }

    self.live_html_collections = out;
    Ok(())
  }

  fn sync_one_html_collection(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    collection_obj: GcObject,
    coll: &LiveHtmlCollection,
  ) -> Result<(), VmError>
  where
    Host: DomHost,
  {
    let items: Vec<(NodeId, DomInterface)> = self.with_dom_host(vm, |host| {
      Ok(host.with_dom(|dom| {
        let ids: Vec<NodeId> = match &coll.kind {
          LiveHtmlCollectionKind::ChildrenElements => dom.children_elements(coll.root),
          LiveHtmlCollectionKind::TagName { qualified_name } => {
            dom.get_elements_by_tag_name_from(coll.root, qualified_name)
          }
          LiveHtmlCollectionKind::TagNameNS {
            namespace,
            local_name,
          } => dom.get_elements_by_tag_name_ns_from(
            coll.root,
            namespace.as_deref(),
            local_name,
          ),
          LiveHtmlCollectionKind::ClassName { class_names } => {
            dom.get_elements_by_class_name_from(coll.root, class_names)
          }
          LiveHtmlCollectionKind::Name { name } => dom.get_elements_by_name_from(coll.root, name),
        };

        ids
          .into_iter()
          .map(|node_id| {
            let primary = if node_id.index() >= dom.nodes_len() {
              DomInterface::Node
            } else {
              DomInterface::primary_for_node_kind(&dom.node(node_id).kind)
            };
            (node_id, primary)
          })
          .collect()
      }))
    })?;

    let length_key = key_from_str(scope, COLLECTION_LENGTH_KEY)?;
    let old_len = match scope
      .heap()
      .object_get_own_data_property_value(collection_obj, &length_key)?
    {
      Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
      _ => 0,
    };

    for (idx, (node_id, primary)) in items.iter().copied().enumerate() {
      let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
        scope,
        coll.document_id,
        node_id,
        primary,
      )?;
      scope.push_root(Value::Object(wrapper))?;

      let idx_key = key_from_str(scope, &idx.to_string())?;
      scope.define_property(
        collection_obj,
        idx_key,
        data_property(Value::Object(wrapper), true, true, true),
      )?;
    }

    for idx in items.len()..old_len {
      let idx_key = key_from_str(scope, &idx.to_string())?;
      scope.heap_mut().delete_property_or_throw(collection_obj, idx_key)?;
    }

    // Update internal length storage. Public `length` is exposed as a readonly accessor on
    // `HTMLCollection.prototype`.
    scope.define_property(
      collection_obj,
      length_key,
      data_property(Value::Number(items.len() as f64), true, false, false),
    )?;

    Ok(())
  }

  fn require_params(&self, receiver: Option<Value>) -> Result<UrlSearchParams, VmError> {
    let obj = Self::require_receiver_object(receiver)?;
    self
      .params
      .get(&WeakGcObject::from(obj))
      .cloned()
      .ok_or(VmError::TypeError("Illegal invocation"))
  }

  fn require_range_state(&self, receiver: Option<Value>) -> Result<RangeState, VmError> {
    let obj = Self::require_receiver_object(receiver)?;
    self
      .ranges
      .get(&WeakGcObject::from(obj))
      .copied()
      .ok_or(VmError::TypeError("Illegal invocation"))
  }

  fn dom_error_to_vm_error(
    &self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    err: DomError,
  ) -> VmError {
    let name = err.code();
    let message = "";

    if let Some(intr) = vm.intrinsics() {
      if let Some(global) = self.global {
        if let Ok(dom_exception) = DomExceptionClassVmJs::install_for_global(vm, scope, global, intr) {
          return crate::js::bindings::throw_dom_exception(scope, dom_exception, name, message);
        }
      }
      // Fall back to a plain `Error` object if DOMException isn't available.
      return crate::js::bindings::dom_exception_vmjs::throw_dom_exception_like_error(
        scope, intr, name, message,
      );
    }

    // No realm intrinsics; best-effort throw.
    VmError::Throw(Value::Undefined)
  }

  fn dom_exception_to_vm_error(
    &self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    err: DomException,
  ) -> VmError {
    let (name, message) = match &err {
      DomException::SyntaxError { message } => ("SyntaxError", message.as_str()),
      DomException::NotSupportedError { message } => ("NotSupportedError", message.as_str()),
      DomException::InvalidStateError { message } => ("InvalidStateError", message.as_str()),
      DomException::NoModificationAllowedError { message } => {
        ("NoModificationAllowedError", message.as_str())
      }
    };

    if let Some(intr) = vm.intrinsics() {
      let global = self.global.or_else(|| {
        vm.user_data_mut::<WindowRealmUserData>()
          .and_then(|data| data.window_obj())
      });

      if let Some(global) = global {
        if let Ok(dom_exception) =
          DomExceptionClassVmJs::install_for_global(vm, scope, global, intr)
        {
          return crate::js::bindings::throw_dom_exception(scope, dom_exception, name, message);
        }
      }

      // Fall back to a plain `Error` object if DOMException isn't available.
      return crate::js::bindings::dom_exception_vmjs::throw_dom_exception_like_error(
        scope, intr, name, message,
      );
    }

    // No realm intrinsics; best-effort throw.
    VmError::Throw(Value::Undefined)
  }

  fn url_proto_from_global(&self, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<GcObject, VmError> {
    let global = self
      .global
      .ok_or(VmError::Unimplemented("WebIDL host missing global object"))?;

    let ctor_key = key_from_str(scope, "URL")?;
    let ctor = get_with_active_vm_host_and_hooks(vm, scope, global, ctor_key)?;
    scope.push_root(ctor)?;
    let Value::Object(ctor_obj) = ctor else {
      return Err(VmError::TypeError("globalThis.URL is not an object"));
    };

    let proto_key = key_from_str(scope, "prototype")?;
    let proto = get_with_active_vm_host_and_hooks(vm, scope, ctor_obj, proto_key)?;
    scope.push_root(proto)?;
    let Value::Object(proto_obj) = proto else {
      return Err(VmError::TypeError("URL.prototype is not an object"));
    };
    Ok(proto_obj)
  }

  fn url_search_params_proto_from_global(
    &self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
  ) -> Result<GcObject, VmError> {
    let global = self
      .global
      .ok_or(VmError::Unimplemented("WebIDL host missing global object"))?;

    let ctor_key = key_from_str(scope, "URLSearchParams")?;
    let ctor = get_with_active_vm_host_and_hooks(vm, scope, global, ctor_key)?;
    scope.push_root(ctor)?;
    let Value::Object(ctor_obj) = ctor else {
      return Err(VmError::TypeError(
        "globalThis.URLSearchParams is not an object",
      ));
    };

    let proto_key = key_from_str(scope, "prototype")?;
    let proto = get_with_active_vm_host_and_hooks(vm, scope, ctor_obj, proto_key)?;
    scope.push_root(proto)?;
    let Value::Object(proto_obj) = proto else {
      return Err(VmError::TypeError(
        "URLSearchParams.prototype is not an object",
      ));
    };
    Ok(proto_obj)
  }

  fn node_iterator_proto_from_global(
    &self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
  ) -> Result<GcObject, VmError> {
    let global = self
      .global
      .ok_or(VmError::Unimplemented("WebIDL host missing global object"))?;
    let ctor_key = key_from_str(scope, "NodeIterator")?;
    let ctor = get_with_active_vm_host_and_hooks(vm, scope, global, ctor_key)?;
    scope.push_root(ctor)?;
    let Value::Object(ctor_obj) = ctor else {
      return Err(VmError::TypeError("globalThis.NodeIterator is not an object"));
    };

    let proto_key = key_from_str(scope, "prototype")?;
    let proto = get_with_active_vm_host_and_hooks(vm, scope, ctor_obj, proto_key)?;
    scope.push_root(proto)?;
    let Value::Object(proto_obj) = proto else {
      return Err(VmError::TypeError("NodeIterator.prototype is not an object"));
    };
    Ok(proto_obj)
  }

  fn tree_walker_proto_from_global(
    &self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
  ) -> Result<GcObject, VmError> {
    let global = self
      .global
      .ok_or(VmError::Unimplemented("WebIDL host missing global object"))?;
    let ctor_key = key_from_str(scope, "TreeWalker")?;
    let ctor = get_with_active_vm_host_and_hooks(vm, scope, global, ctor_key)?;
    scope.push_root(ctor)?;
    let Value::Object(ctor_obj) = ctor else {
      return Err(VmError::TypeError("globalThis.TreeWalker is not an object"));
    };

    let proto_key = key_from_str(scope, "prototype")?;
    let proto = get_with_active_vm_host_and_hooks(vm, scope, ctor_obj, proto_key)?;
    scope.push_root(proto)?;
    let Value::Object(proto_obj) = proto else {
      return Err(VmError::TypeError("TreeWalker.prototype is not an object"));
    };
    Ok(proto_obj)
  }

  fn range_proto_from_global(
    &self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
  ) -> Result<GcObject, VmError> {
    let global = self
      .global
      .ok_or(VmError::Unimplemented("WebIDL host missing global object"))?;

    let ctor_key = key_from_str(scope, "Range")?;
    let ctor = get_with_active_vm_host_and_hooks(vm, scope, global, ctor_key)?;
    scope.push_root(ctor)?;
    let Value::Object(ctor_obj) = ctor else {
      return Err(VmError::TypeError("globalThis.Range is not an object"));
    };

    let proto_key = key_from_str(scope, "prototype")?;
    let proto = get_with_active_vm_host_and_hooks(vm, scope, ctor_obj, proto_key)?;
    scope.push_root(proto)?;
    let Value::Object(proto_obj) = proto else {
      return Err(VmError::TypeError("Range.prototype is not an object"));
    };
    Ok(proto_obj)
  }

  fn urlsp_iterator_next_call_id(&mut self, vm: &mut Vm) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.urlsp_iterator_next_call {
      return Ok(id);
    }
    let id = vm.register_native_call(urlsp_iterator_next_native)?;
    self.urlsp_iterator_next_call = Some(id);
    Ok(id)
  }

  fn urlsp_iterator_iterator_call_id(&mut self, vm: &mut Vm) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.urlsp_iterator_iterator_call {
      return Ok(id);
    }
    let id = vm.register_native_call(iterator_return_self_native)?;
    self.urlsp_iterator_iterator_call = Some(id);
    Ok(id)
  }

  fn set_timeout_impl(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let handler = args.get(0).copied().unwrap_or(Value::Undefined);
    if matches!(handler, Value::String(_)) {
      return Err(VmError::TypeError(SET_TIMEOUT_STRING_HANDLER_ERROR));
    }
    if !is_callable(scope, handler) {
      return Err(VmError::TypeError(SET_TIMEOUT_NOT_CALLABLE_ERROR));
    }
    let delay_ms = normalize_delay_ms(args.get(1).copied().unwrap_or(Value::Number(0.0)));

    let Some(event_loop) = vm
      .active_host_hooks_mut()
      .and_then(|hooks| event_loop_mut_from_hooks::<Host>(hooks))
    else {
      return Err(VmError::TypeError(
        "setTimeout called without an active EventLoop",
      ));
    };

    // Keep the callback + extra args alive until the timer fires (or is cleared). Ensure roots are
    // cleaned up on any early-return error so we don't leak persistent roots when the EventLoop
    // rejects new timers.
    let callback_root = scope.heap_mut().add_root(handler)?;
    let mut arg_roots: Vec<RootId> = Vec::new();
    for arg in args.iter().copied().skip(2) {
      match scope.heap_mut().add_root(arg) {
        Ok(root) => arg_roots.push(root),
        Err(err) => {
          scope.heap_mut().remove_root(callback_root);
          for root in arg_roots {
            scope.heap_mut().remove_root(root);
          }
          return Err(err);
        }
      }
    }

    let entry = TimerEntry {
      callback: RootedCallback {
        value: handler,
        root: callback_root,
      },
      args: arg_roots,
    };

    let registry = Rc::clone(&self.timer_registry);
    let id_cell: Rc<Cell<TimerId>> = Rc::new(Cell::new(0));
    let id_cell_for_cb = Rc::clone(&id_cell);

    let id = event_loop
      .set_timeout(Duration::from_millis(delay_ms), move |host, event_loop| {
        let id = id_cell_for_cb.get();

        // Take the registry entry first so `clearTimeout` during callback is a no-op.
        let Some(entry) = registry.borrow_mut().remove(&id) else {
          return Ok(());
        };

        let RootedCallback {
          value: callback,
          root: cb_root,
        } = entry.callback;
        let arg_roots = entry.args;

        let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
        hooks.set_event_loop(event_loop);
        let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
        window_realm.reset_interrupt();
        let budget = window_realm.vm_budget_now();
        let global = window_realm.global_object();

        let (vm, heap) = window_realm.vm_and_heap_mut();
        let mut args: Vec<Value> = Vec::new();
        args.try_reserve(arg_roots.len()).map_err(|_| {
          crate::error::Error::Other("timer callback args allocation failed".to_string())
        })?;
        for root in &arg_roots {
          if let Some(v) = heap.get_root(*root) {
            args.push(v);
          } else {
            args.push(Value::Undefined);
          }
        }

        let mut vm = vm.push_budget(budget);
        let tick_result = vm.tick();
        let call_result = tick_result.and_then(|_| {
          let mut scope = heap.scope();
          vm.call_with_host_and_hooks(
            vm_host,
            &mut scope,
            &mut hooks,
            callback,
            Value::Object(global),
            &args,
          )
          .map(|_| ())
        });
        let result: crate::error::Result<()> = call_result
          .map_err(|err| vm_error_to_event_loop_error(heap, err))
          .map(|_| ());

        let finish_err = hooks.finish(&mut *heap);

        // Always release roots for one-shot timeouts.
        heap.remove_root(cb_root);
        for root in arg_roots {
          heap.remove_root(root);
        }

        if let Some(err) = finish_err {
          return Err(err);
        }
        result
      })
      .map_err(|_| {
        // If queueing fails, ensure we don't leak persistent roots.
        scope.heap_mut().remove_root(entry.callback.root);
        for root in &entry.args {
          scope.heap_mut().remove_root(*root);
        }
        VmError::TypeError("setTimeout failed to schedule timer")
      })?;

    id_cell.set(id);
    self.timer_registry.borrow_mut().insert(id, entry);
    Ok(Value::Number(id as f64))
  }

  fn set_interval_impl(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let handler = args.get(0).copied().unwrap_or(Value::Undefined);
    if matches!(handler, Value::String(_)) {
      return Err(VmError::TypeError(SET_INTERVAL_STRING_HANDLER_ERROR));
    }
    if !is_callable(scope, handler) {
      return Err(VmError::TypeError(SET_INTERVAL_NOT_CALLABLE_ERROR));
    }
    let interval_ms = normalize_delay_ms(args.get(1).copied().unwrap_or(Value::Number(0.0)));

    let Some(event_loop) = vm
      .active_host_hooks_mut()
      .and_then(|hooks| event_loop_mut_from_hooks::<Host>(hooks))
    else {
      return Err(VmError::TypeError(
        "setInterval called without an active EventLoop",
      ));
    };

    let callback_root = scope.heap_mut().add_root(handler)?;
    let mut arg_roots: Vec<RootId> = Vec::new();
    for arg in args.iter().copied().skip(2) {
      match scope.heap_mut().add_root(arg) {
        Ok(root) => arg_roots.push(root),
        Err(err) => {
          scope.heap_mut().remove_root(callback_root);
          for root in arg_roots {
            scope.heap_mut().remove_root(root);
          }
          return Err(err);
        }
      }
    }

    let entry = TimerEntry {
      callback: RootedCallback {
        value: handler,
        root: callback_root,
      },
      args: arg_roots,
    };

    let registry = Rc::clone(&self.timer_registry);
    let id_cell: Rc<Cell<TimerId>> = Rc::new(Cell::new(0));
    let id_cell_for_cb = Rc::clone(&id_cell);

    let id = event_loop
      .set_interval(
        Duration::from_millis(interval_ms),
        move |host, event_loop| {
          let id = id_cell_for_cb.get();

          let (callback, arg_roots) = {
            let map = registry.borrow();
            let Some(entry) = map.get(&id) else {
              return Ok(());
            };
            (entry.callback.value, entry.args.clone())
          };

          let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
          hooks.set_event_loop(event_loop);
          let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
          window_realm.reset_interrupt();
          let budget = window_realm.vm_budget_now();
          let global = window_realm.global_object();

          let (vm, heap) = window_realm.vm_and_heap_mut();
          let mut args: Vec<Value> = Vec::new();
          args.try_reserve(arg_roots.len()).map_err(|_| {
            crate::error::Error::Other("timer callback args allocation failed".to_string())
          })?;
          for root in &arg_roots {
            if let Some(v) = heap.get_root(*root) {
              args.push(v);
            } else {
              args.push(Value::Undefined);
            }
          }

          let mut vm = vm.push_budget(budget);
          let tick_result = vm.tick();

          let call_result = tick_result.and_then(|_| {
            let mut scope = heap.scope();
            vm.call_with_host_and_hooks(
              vm_host,
              &mut scope,
              &mut hooks,
              callback,
              Value::Object(global),
              &args,
            )
            .map(|_| ())
          });
          let result: crate::error::Result<()> = call_result
            .map_err(|err| vm_error_to_event_loop_error(heap, err))
            .map(|_| ());

          let finish_err = hooks.finish(&mut *heap);
          if let Some(err) = finish_err {
            // Cancel on hook failure and release roots.
            event_loop.clear_interval(id);
            if let Some(entry) = registry.borrow_mut().remove(&id) {
              heap.remove_root(entry.callback.root);
              for root in entry.args {
                heap.remove_root(root);
              }
            }
            return Err(err);
          }

          if let Err(err) = result {
            // Cancel the interval on error for determinism and to avoid repeated failures.
            event_loop.clear_interval(id);
            if let Some(entry) = registry.borrow_mut().remove(&id) {
              heap.remove_root(entry.callback.root);
              for root in entry.args {
                heap.remove_root(root);
              }
            }
            return Err(err);
          }

          Ok(())
        },
      )
      .map_err(|_| {
        scope.heap_mut().remove_root(entry.callback.root);
        for root in &entry.args {
          scope.heap_mut().remove_root(*root);
        }
        VmError::TypeError("setInterval failed to schedule timer")
      })?;

    id_cell.set(id);
    self.timer_registry.borrow_mut().insert(id, entry);
    Ok(Value::Number(id as f64))
  }

  fn clear_timer_impl(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    id: TimerId,
    is_interval: bool,
  ) -> Result<Value, VmError> {
    let Some(event_loop) = vm
      .active_host_hooks_mut()
      .and_then(|hooks| event_loop_mut_from_hooks::<Host>(hooks))
    else {
      return Err(VmError::TypeError(if is_interval {
        "clearInterval called without an active EventLoop"
      } else {
        "clearTimeout called without an active EventLoop"
      }));
    };

    if is_interval {
      event_loop.clear_interval(id);
    } else {
      event_loop.clear_timeout(id);
    }

    if let Some(entry) = self.timer_registry.borrow_mut().remove(&id) {
      scope.heap_mut().remove_root(entry.callback.root);
      for root in entry.args {
        scope.heap_mut().remove_root(root);
      }
    }

    Ok(Value::Undefined)
  }

  fn queue_microtask_impl(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    callback: Value,
  ) -> Result<Value, VmError> {
    if matches!(callback, Value::String(_)) {
      return Err(VmError::TypeError(QUEUE_MICROTASK_STRING_HANDLER_ERROR));
    }
    if !is_callable(scope, callback) {
      return Err(VmError::TypeError(QUEUE_MICROTASK_NOT_CALLABLE_ERROR));
    }

    let Some(event_loop) = vm
      .active_host_hooks_mut()
      .and_then(|hooks| event_loop_mut_from_hooks::<Host>(hooks))
    else {
      return Err(VmError::TypeError(
        "queueMicrotask called without an active EventLoop",
      ));
    };

    let root = scope.heap_mut().add_root(callback)?;
    event_loop
      .queue_microtask(move |host, event_loop| {
        let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
        hooks.set_event_loop(event_loop);
        let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
        window_realm.reset_interrupt();
        let budget = window_realm.vm_budget_now();

        let global = window_realm.global_object();

        let (vm, heap) = window_realm.vm_and_heap_mut();
        let value = heap.get_root(root).unwrap_or(Value::Undefined);

        let mut vm = vm.push_budget(budget);
        let tick_result = vm.tick();
        let call_result = tick_result.and_then(|_| {
          let mut scope = heap.scope();
          vm.call_with_host_and_hooks(
            vm_host,
            &mut scope,
            &mut hooks,
            value,
            Value::Object(global),
            &[],
          )
          .map(|_| ())
        });
        let result: crate::error::Result<()> = call_result
          .map_err(|err| vm_error_to_event_loop_error(heap, err))
          .map(|_| ());

        let finish_err = hooks.finish(&mut *heap);
        heap.remove_root(root);

        if let Some(err) = finish_err {
          return Err(err);
        }
        result
      })
      .map_err(|_| {
        // If queueing fails, ensure we don't leak the persistent root.
        scope.heap_mut().remove_root(root);
        VmError::TypeError("queueMicrotask failed to enqueue microtask")
      })?;

    Ok(Value::Undefined)
  }

  fn sync_cached_child_nodes_for_wrapper(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    wrapper_obj: GcObject,
    node_id: NodeId,
    document_id: DocumentId,
  ) -> Result<(), VmError>
  where
    Host: DomHost,
  {
    let child_nodes_key = key_from_str(scope, NODE_CHILD_NODES_KEY)?;
    let Some(Value::Object(list_obj)) = scope
      .heap()
      .object_get_own_data_property_value(wrapper_obj, &child_nodes_key)?
    else {
      return Ok(());
    };

    #[derive(Debug)]
    enum SyncChildNodesError {
      Dom(DomError),
      OutOfMemory,
    }

    let children: Result<Vec<(NodeId, DomInterface)>, SyncChildNodesError> =
      self.with_dom_host(vm, |host| {
        Ok(host.with_dom(|dom| {
        if node_id.index() >= dom.nodes_len() {
          return Err(SyncChildNodesError::Dom(DomError::NotFoundError));
        }

        let parent_node = dom.node(node_id);
        // `dom2` stores `ShadowRoot` as a child of its host element; light-DOM traversal via
        // `childNodes` must never expose those shadow root nodes.
        let filter_shadow_roots = !matches!(parent_node.kind, NodeKind::ShadowRoot { .. });

        let mut out: Vec<(NodeId, DomInterface)> = Vec::new();
        out
          .try_reserve(parent_node.children.len())
          .map_err(|_| SyncChildNodesError::OutOfMemory)?;

        for &child_id in parent_node.children.iter() {
          if child_id.index() >= dom.nodes_len() {
            continue;
          }
          let child_node = dom.node(child_id);
          if child_node.parent != Some(node_id) {
            continue;
          }
          if filter_shadow_roots && matches!(child_node.kind, NodeKind::ShadowRoot { .. }) {
            continue;
          }
          let primary = DomInterface::primary_for_node_kind(&child_node.kind);
          out.push((child_id, primary));
        }

        Ok(out)
        }))
      })?;

    let children = match children {
      Ok(v) => v,
      Err(SyncChildNodesError::Dom(err)) => return Err(self.dom_error_to_vm_error(vm, scope, err)),
      Err(SyncChildNodesError::OutOfMemory) => return Err(VmError::OutOfMemory),
    };

    // Root the list object while allocating keys and wrappers.
    scope.push_root(Value::Object(list_obj))?;

    let length_key = key_from_str(scope, COLLECTION_LENGTH_KEY)?;
    let old_len = match scope
      .heap()
      .object_get_own_data_property_value(list_obj, &length_key)?
    {
      Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
      _ => 0,
    };

    for (idx, (child_id, primary)) in children.iter().copied().enumerate() {
      let child_wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
        scope,
        document_id,
        child_id,
        primary,
      )?;
      scope.push_root(Value::Object(child_wrapper))?;

      let idx_key = key_from_str(scope, &idx.to_string())?;
      scope.define_property(
        list_obj,
        idx_key,
        data_property(Value::Object(child_wrapper), true, true, true),
      )?;
    }

    for idx in children.len()..old_len {
      let idx_key = key_from_str(scope, &idx.to_string())?;
      scope.heap_mut().delete_property_or_throw(list_obj, idx_key)?;
    }

    // Update internal length storage. Public `length` is exposed as a readonly accessor on
    // `NodeList.prototype`.
    scope.define_property(
      list_obj,
      length_key,
      data_property(Value::Number(children.len() as f64), true, false, false),
    )?;

    Ok(())
  }

  fn try_delegate_dom_call_operation(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    overload: usize,
    args: &[Value],
  ) -> Result<Option<Value>, VmError> {
    if !should_delegate_dom_interface(interface) {
      return Ok(None);
    }

    let self_ptr = std::ptr::from_mut(self).cast::<()>();
    let delegated = with_active_vm_host_and_hooks(vm, |vm, host, _hooks| {
      let host_ptr = (host as *mut dyn VmHost).cast::<()>();
      if host_ptr == self_ptr {
        return Ok(None);
      }

      let any = host.as_any_mut();
      if let Some(dom) = any.downcast_mut::<crate::js::host_document::HostDocumentState>() {
        return Ok(Some(dom.call_operation(
          vm, scope, receiver, interface, operation, overload, args,
        )?));
      }
      #[cfg(test)]
      if let Some(dom) = any.downcast_mut::<tests::RecordingDomWebIdlHost>() {
        return Ok(Some(dom.call_operation(
          vm, scope, receiver, interface, operation, overload, args,
        )?));
      }
      Ok(None)
    })?;

    Ok(delegated.flatten())
  }

  fn try_delegate_dom_iterable_snapshot(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    receiver: Option<Value>,
    interface: &'static str,
    kind: IterableKind,
  ) -> Result<Option<Vec<BindingValue>>, VmError> {
    if !should_delegate_dom_interface(interface) {
      return Ok(None);
    }

    let self_ptr = std::ptr::from_mut(self).cast::<()>();
    let delegated = with_active_vm_host_and_hooks(vm, |vm, host, _hooks| {
      let host_ptr = (host as *mut dyn VmHost).cast::<()>();
      if host_ptr == self_ptr {
        return Ok(None);
      }

      let any = host.as_any_mut();
      if let Some(dom) = any.downcast_mut::<crate::js::host_document::HostDocumentState>() {
        return Ok(Some(
          dom.iterable_snapshot(vm, scope, receiver, interface, kind)?,
        ));
      }
      #[cfg(test)]
      if let Some(dom) = any.downcast_mut::<tests::RecordingDomWebIdlHost>() {
        return Ok(Some(
          dom.iterable_snapshot(vm, scope, receiver, interface, kind)?,
        ));
      }
      Ok(None)
    })?;

    Ok(delegated.flatten())
  }
}

impl<Host: WindowRealmHost + DomHost + 'static> WebIdlBindingsHost for VmJsWebIdlBindingsHostDispatch<Host> {
  fn call_operation(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    overload: usize,
    args: &[Value],
  ) -> Result<Value, VmError> {
    self.maybe_sweep(vm, scope.heap_mut());

    match (interface, operation, overload) {
      ("Document", "getElementById", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;
        let id_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let id_value = scope.heap_mut().to_string(id_value)?;
        let id = scope
          .heap()
          .get_string(id_value)
          .map(|s| s.to_utf8_lossy())
          .unwrap_or_default();

        let found = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            dom.get_element_by_id(&id).map(|node_id| {
              let primary = DomInterface::primary_for_node_kind(&dom.node(node_id).kind);
              (node_id, primary)
            })
          }))
        })?;
        let Some((node_id, primary_interface)) = found else {
          return Ok(Value::Null);
        };

        let wrapper = if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
          if let Some(platform) = data.dom_platform_mut() {
            platform.get_or_create_wrapper(
              scope,
              WeakGcObject::from(document_obj),
              node_id,
              primary_interface,
            )?
          } else {
            scope.alloc_object()?
          }
        } else {
          scope.alloc_object()?
        };
        Ok(Value::Object(wrapper))
      }
      ("Document", "getElementsByTagName", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;
        scope.push_root(Value::Object(document_obj))?;

        let handle =
          require_dom_platform_mut(vm)?.require_document_handle(scope.heap(), Value::Object(document_obj))?;
        let document_id = handle.document_id;
        let root = handle.node_id;

        let qualified_name =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let collection = self.create_live_html_collection(
          vm,
          scope,
          document_obj,
          document_obj,
          document_id,
          root,
          LiveHtmlCollectionKind::TagName { qualified_name },
        )?;
        Ok(Value::Object(collection))
      }
      ("Document", "getElementsByTagNameNS", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;
        scope.push_root(Value::Object(document_obj))?;

        let handle =
          require_dom_platform_mut(vm)?.require_document_handle(scope.heap(), Value::Object(document_obj))?;
        let document_id = handle.document_id;
        let root = handle.node_id;

        let namespace_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let namespace = match namespace_value {
          Value::Null | Value::Undefined => None,
          Value::String(_) => Some(js_string_to_rust_string(scope, namespace_value)?),
          _ => return Err(VmError::TypeError("expected namespace to be a string or null")),
        };
        let local_name =
          js_string_to_rust_string(scope, args.get(1).copied().unwrap_or(Value::Undefined))?;

        let collection = self.create_live_html_collection(
          vm,
          scope,
          document_obj,
          document_obj,
          document_id,
          root,
          LiveHtmlCollectionKind::TagNameNS {
            namespace,
            local_name,
          },
        )?;
        Ok(Value::Object(collection))
      }
      ("Document", "getElementsByClassName", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;
        scope.push_root(Value::Object(document_obj))?;

        let handle =
          require_dom_platform_mut(vm)?.require_document_handle(scope.heap(), Value::Object(document_obj))?;
        let document_id = handle.document_id;
        let root = handle.node_id;

        let class_names =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let collection = self.create_live_html_collection(
          vm,
          scope,
          document_obj,
          document_obj,
          document_id,
          root,
          LiveHtmlCollectionKind::ClassName { class_names },
        )?;
        Ok(Value::Object(collection))
      }
      ("Document", "getElementsByName", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;
        scope.push_root(Value::Object(document_obj))?;

        let handle =
          require_dom_platform_mut(vm)?.require_document_handle(scope.heap(), Value::Object(document_obj))?;
        let document_id = handle.document_id;
        let root = handle.node_id;

        let name = js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let collection = self.create_live_html_collection(
          vm,
          scope,
          document_obj,
          document_obj,
          document_id,
          root,
          LiveHtmlCollectionKind::Name { name },
        )?;
        Ok(Value::Object(collection))
      }
      ("Document", "querySelector", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;

        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let selectors =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let result: Result<Option<(NodeId, DomInterface)>, DomException> = self.with_dom_host(vm, |host| {
          Ok(dom2_bindings::query_selector(host, &selectors, None).map(|found| {
            found.map(|node_id| {
              let primary = host.with_dom(|dom| {
                if node_id.index() >= dom.nodes_len() {
                  DomInterface::Node
                } else {
                  DomInterface::primary_for_node_kind(&dom.node(node_id).kind)
                }
              });
              (node_id, primary)
            })
          }))
        })?;

        match result {
          Ok(Some((node_id, primary_interface))) => {
            let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
              scope,
              document_id,
              node_id,
              primary_interface,
            )?;
            scope.push_root(Value::Object(wrapper))?;
            Ok(Value::Object(wrapper))
          }
          Ok(None) => Ok(Value::Null),
          Err(err) => Err(self.dom_exception_to_vm_error(vm, scope, err)),
        }
      }
      ("Document", "querySelectorAll", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;
        scope.push_root(Value::Object(document_obj))?;

        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let selectors =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let node_list_proto_key = key_from_str(scope, NODE_LIST_PROTOTYPE_KEY)?;
        let node_list_proto = match scope
          .heap()
          .object_get_own_data_property_value(document_obj, &node_list_proto_key)?
        {
          Some(Value::Object(obj)) => obj,
          _ => {
            return Err(VmError::InvariantViolation(
              "missing NodeList prototype for Document.querySelectorAll",
            ))
          }
        };

        let result: Result<Vec<(NodeId, DomInterface)>, DomException> = self.with_dom_host(vm, |host| {
          Ok(dom2_bindings::query_selector_all(host, &selectors, None).map(|nodes| {
            host.with_dom(|dom| {
              nodes
                .into_iter()
                .map(|node_id| {
                  let primary = if node_id.index() >= dom.nodes_len() {
                    DomInterface::Node
                  } else {
                    DomInterface::primary_for_node_kind(&dom.node(node_id).kind)
                  };
                  (node_id, primary)
                })
                .collect()
            })
          }))
        })?;

        let nodes = match result {
          Ok(nodes) => nodes,
          Err(err) => return Err(self.dom_exception_to_vm_error(vm, scope, err)),
        };

        let list_obj = scope.alloc_object()?;
        scope.push_root(Value::Object(list_obj))?;
        scope
          .heap_mut()
          .object_set_prototype(list_obj, Some(node_list_proto))?;

        for (idx, (node_id, primary)) in nodes.iter().copied().enumerate() {
          let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
            scope,
            document_id,
            node_id,
            primary,
          )?;
          scope.push_root(Value::Object(wrapper))?;

          let idx_key = key_from_str(scope, &idx.to_string())?;
          scope.define_property(
            list_obj,
            idx_key,
            data_property(Value::Object(wrapper), true, true, true),
          )?;
        }

        let length_key = key_from_str(scope, COLLECTION_LENGTH_KEY)?;
        scope.define_property(
          list_obj,
          length_key,
          data_property(Value::Number(nodes.len() as f64), true, false, false),
        )?;

        Ok(Value::Object(list_obj))
      }
      ("DocumentFragment", "querySelector", 0) => {
        let fragment_obj = Self::require_receiver_object(receiver)?;
        let (document_id, fragment_id) = {
          let platform = require_dom_platform_mut(vm)?;
          let handle = platform.require_node_handle(scope.heap(), Value::Object(fragment_obj))?;
          (handle.document_id, handle.node_id)
        };

        let selectors =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let result: Result<Option<(NodeId, DomInterface)>, DomException> =
          self.with_dom_host(vm, |host| {
            Ok(
              dom2_bindings::query_selector(host, &selectors, Some(fragment_id)).map(|found| {
                found.map(|node_id| {
                  let primary = host.with_dom(|dom| {
                    if node_id.index() >= dom.nodes_len() {
                      DomInterface::Node
                    } else {
                      DomInterface::primary_for_node_kind(&dom.node(node_id).kind)
                    }
                  });
                  (node_id, primary)
                })
              }),
            )
          })?;

        match result {
          Ok(Some((node_id, primary_interface))) => {
            let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
              scope,
              document_id,
              node_id,
              primary_interface,
            )?;
            scope.push_root(Value::Object(wrapper))?;
            Ok(Value::Object(wrapper))
          }
          Ok(None) => Ok(Value::Null),
          Err(err) => Err(self.dom_exception_to_vm_error(vm, scope, err)),
        }
      }
      ("DocumentFragment", "querySelectorAll", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let Value::Object(fragment_obj) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };

        let (document_id, fragment_id) = {
          let platform = require_dom_platform_mut(vm)?;
          let handle = platform.require_node_handle(scope.heap(), Value::Object(fragment_obj))?;
          (handle.document_id, handle.node_id)
        };

        // WebIDL wrapper objects store a back-reference to their owning `Document` wrapper; use the
        // realm's per-document NodeList prototype so `instanceof NodeList` works.
        let wrapper_document_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let document_obj = match scope
          .heap()
          .object_get_own_data_property_value(fragment_obj, &wrapper_document_key)?
        {
          Some(Value::Object(obj)) => obj,
          _ => return Err(VmError::TypeError("Illegal invocation")),
        };
        scope.push_root(Value::Object(document_obj))?;

        let node_list_proto_key = key_from_str(scope, NODE_LIST_PROTOTYPE_KEY)?;
        let node_list_proto = match scope
          .heap()
          .object_get_own_data_property_value(document_obj, &node_list_proto_key)?
        {
          Some(Value::Object(obj)) => obj,
          _ => {
            return Err(VmError::InvariantViolation(
              "missing NodeList prototype for DocumentFragment.querySelectorAll",
            ))
          }
        };

        let selectors =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let result: Result<Vec<(NodeId, DomInterface)>, DomException> =
          self.with_dom_host(vm, |host| {
            Ok(
              dom2_bindings::query_selector_all(host, &selectors, Some(fragment_id)).map(|nodes| {
                host.with_dom(|dom| {
                  nodes
                    .into_iter()
                    .map(|node_id| {
                      let primary = if node_id.index() >= dom.nodes_len() {
                        DomInterface::Node
                      } else {
                        DomInterface::primary_for_node_kind(&dom.node(node_id).kind)
                      };
                      (node_id, primary)
                    })
                    .collect()
                })
              }),
            )
          })?;

        let nodes = match result {
          Ok(nodes) => nodes,
          Err(err) => return Err(self.dom_exception_to_vm_error(vm, scope, err)),
        };

        let list_obj = scope.alloc_object()?;
        scope.push_root(Value::Object(list_obj))?;
        scope
          .heap_mut()
          .object_set_prototype(list_obj, Some(node_list_proto))?;

        for (idx, (node_id, primary)) in nodes.iter().copied().enumerate() {
          let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
            scope,
            document_id,
            node_id,
            primary,
          )?;
          scope.push_root(Value::Object(wrapper))?;

          let idx_key = key_from_str(scope, &idx.to_string())?;
          scope.define_property(
            list_obj,
            idx_key,
            data_property(Value::Object(wrapper), true, true, true),
          )?;
        }

        let length_key = key_from_str(scope, COLLECTION_LENGTH_KEY)?;
        scope.define_property(
          list_obj,
          length_key,
          data_property(Value::Number(nodes.len() as f64), true, false, false),
        )?;

        Ok(Value::Object(list_obj))
      }
      ("Document", "documentElement", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;

        // Brand check: `Document.prototype.documentElement` must only be callable on a DOM-backed
        // Document wrapper.
        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let found = self.with_dom_host(vm, |host| {
          Ok(dom2_bindings::document_element(host).map(|node_id| {
            let primary = host.with_dom(|dom| {
              if node_id.index() >= dom.nodes_len() {
                DomInterface::Element
              } else {
                DomInterface::primary_for_node_kind(&dom.node(node_id).kind)
              }
            });
            (node_id, primary)
          }))
        })?;
        let Some((node_id, primary_interface)) = found else {
          return Ok(Value::Null);
        };

        let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
          scope,
          document_id,
          node_id,
          primary_interface,
        )?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("Document", "head", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;

        // Brand check: `Document.prototype.head` must only be callable on a DOM-backed Document
        // wrapper.
        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let found = self.with_dom_host(vm, |host| {
          Ok(dom2_bindings::head(host).map(|node_id| {
            let primary = host.with_dom(|dom| {
              if node_id.index() >= dom.nodes_len() {
                DomInterface::Element
              } else {
                DomInterface::primary_for_node_kind(&dom.node(node_id).kind)
              }
            });
            (node_id, primary)
          }))
        })?;
        let Some((node_id, primary_interface)) = found else {
          return Ok(Value::Null);
        };

        let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
          scope,
          document_id,
          node_id,
          primary_interface,
        )?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("Document", "body", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;

        let (document_id, document_node_id) = {
          let platform = require_dom_platform_mut(vm)?;
          let handle = platform.require_document_handle(scope.heap(), Value::Object(document_obj))?;
          (handle.document_id, handle.node_id)
        };

        if args.is_empty() {
          let found = self.with_dom_host(vm, |host| {
            Ok(dom2_bindings::body(host).map(|node_id| {
              let primary = host.with_dom(|dom| {
                if node_id.index() >= dom.nodes_len() {
                  DomInterface::Element
                } else {
                  DomInterface::primary_for_node_kind(&dom.node(node_id).kind)
                }
              });
              (node_id, primary)
            }))
          })?;
          let Some((node_id, primary_interface)) = found else {
            return Ok(Value::Null);
          };

          let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
            scope,
            document_id,
            node_id,
            primary_interface,
          )?;
          scope.push_root(Value::Object(wrapper))?;
          Ok(Value::Object(wrapper))
        } else {
          let value = args.get(0).copied().unwrap_or(Value::Undefined);
          if matches!(value, Value::Null | Value::Undefined) {
            // WebIDL normalizes `undefined` to `null` for `any` setters, but accept either.
            return Ok(Value::Undefined);
          }

          let element_id = require_dom_platform_mut(vm)?.require_element_id(scope.heap(), value)?;

          // Spec note (minimal): we only accept HTML <body> or <frameset> elements in the HTML
          // namespace, otherwise throw HierarchyRequestError (DOMException).
          let result: Result<(NodeId, Option<NodeId>), DomError> = self.with_dom_host(vm, |host| {
            Ok(host.mutate_dom(|dom| {
              if element_id.index() >= dom.nodes_len() {
                return (Err(DomError::NotFoundError), false);
              }
              let is_valid_body_like = match &dom.node(element_id).kind {
                NodeKind::Element {
                  tag_name,
                  namespace,
                  ..
                } => {
                  let is_html_ns = namespace.is_empty() || namespace == HTML_NAMESPACE;
                  is_html_ns
                    && (tag_name.eq_ignore_ascii_case("body")
                      || tag_name.eq_ignore_ascii_case("frameset"))
                }
                _ => false,
              };
              if !is_valid_body_like {
                return (Err(DomError::HierarchyRequestError), false);
              }

              let old_parent = match dom.parent(element_id) {
                Ok(v) => v,
                Err(err) => return (Err(err), false),
              };

              let Some(document_element) = dom.document_element_for(document_node_id) else {
                return (Err(DomError::HierarchyRequestError), false);
              };

              let existing = dom.body_for(document_node_id);
              let changed = match existing {
                Some(old_body) => dom.replace_child(document_element, element_id, old_body),
                None => dom.append_child(document_element, element_id),
              };
              match changed {
                Ok(changed) => (Ok((document_element, old_parent)), changed),
                Err(err) => (Err(err), false),
              }
            }))
          })?;

          match result {
            Ok((document_element_id, old_parent_id)) => {
              // Keep cached `childNodes` NodeLists updated on the document element.
              let wrapper = {
                let platform = require_dom_platform_mut(vm)?;
                platform.get_existing_wrapper_for_document_id(
                  scope.heap(),
                  document_id,
                  document_element_id,
                )
              };
              if let Some(wrapper) = wrapper {
                self.sync_cached_child_nodes_for_wrapper(
                  vm,
                  scope,
                  wrapper,
                  document_element_id,
                  document_id,
                )?;
              }

              // If the new body element was moved from another parent, sync that parent's cached
              // NodeList too.
              if let Some(old_parent_id) = old_parent_id {
                if old_parent_id != document_element_id {
                  let wrapper = {
                    let platform = require_dom_platform_mut(vm)?;
                    platform.get_existing_wrapper_for_document_id(
                      scope.heap(),
                      document_id,
                      old_parent_id,
                    )
                  };
                  if let Some(wrapper) = wrapper {
                    self.sync_cached_child_nodes_for_wrapper(
                      vm,
                      scope,
                      wrapper,
                      old_parent_id,
                      document_id,
                    )?;
                  }
                }
              }

              self.sync_live_html_collections(vm, scope)?;
              Ok(Value::Undefined)
            }
            Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
          }
        }
      }
      ("Event", "constructor", 0) => {
        let obj = Self::require_receiver_object(receiver)?;
        scope.push_root(Value::Object(obj))?;

        let type_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let init_value = args.get(1).copied().unwrap_or(Value::Undefined);

        let mut bubbles = false;
        let mut cancelable = false;
        let mut composed = false;
        if let Value::Object(init_obj) = init_value {
          scope.push_root(Value::Object(init_obj))?;
          let bubbles_key = key_from_str(scope, "bubbles")?;
          if let Some(v) = scope
            .heap()
            .object_get_own_data_property_value(init_obj, &bubbles_key)?
          {
            bubbles = scope.heap().to_boolean(v)?;
          }

          let cancelable_key = key_from_str(scope, "cancelable")?;
          if let Some(v) = scope
            .heap()
            .object_get_own_data_property_value(init_obj, &cancelable_key)?
          {
            cancelable = scope.heap().to_boolean(v)?;
          }

          let composed_key = key_from_str(scope, "composed")?;
          if let Some(v) = scope
            .heap()
            .object_get_own_data_property_value(init_obj, &composed_key)?
          {
            composed = scope.heap().to_boolean(v)?;
          }
        }

        let type_key = key_from_str(scope, "type")?;
        scope.define_property(
          obj,
          type_key,
          data_property(type_value, false, false, true),
        )?;

        let bubbles_key = key_from_str(scope, "bubbles")?;
        scope.define_property(
          obj,
          bubbles_key,
          data_property(Value::Bool(bubbles), false, false, true),
        )?;

        let cancelable_key = key_from_str(scope, "cancelable")?;
        scope.define_property(
          obj,
          cancelable_key,
          data_property(Value::Bool(cancelable), false, false, true),
        )?;

        let composed_key = key_from_str(scope, "composed")?;
        scope.define_property(
          obj,
          composed_key,
          data_property(Value::Bool(composed), false, false, true),
        )?;

        // Default Event fields required by the dom2 dispatch bridge.
        let target_key = key_from_str(scope, "target")?;
        scope.define_property(
          obj,
          target_key,
          data_property(Value::Null, false, false, true),
        )?;
        let src_element_key = key_from_str(scope, "srcElement")?;
        scope.define_property(
          obj,
          src_element_key,
          data_property(Value::Null, false, false, true),
        )?;
        let current_target_key = key_from_str(scope, "currentTarget")?;
        scope.define_property(
          obj,
          current_target_key,
          data_property(Value::Null, false, false, true),
        )?;
        let event_phase_key = key_from_str(scope, "eventPhase")?;
        scope.define_property(
          obj,
          event_phase_key,
          data_property(Value::Number(0.0), false, false, true),
        )?;
        let time_stamp_key = key_from_str(scope, "timeStamp")?;
        scope.define_property(
          obj,
          time_stamp_key,
          data_property(Value::Number(0.0), false, false, true),
        )?;

        // LegacyUnforgeable `isTrusted`: must be an own, non-configurable property.
        let is_trusted_key = key_from_str(scope, "isTrusted")?;
        scope.define_property(
          obj,
          is_trusted_key,
          data_property(Value::Bool(false), false, true, false),
        )?;

        let default_prevented_key = key_from_str(scope, "defaultPrevented")?;
        scope.define_property(
          obj,
          default_prevented_key,
          data_property(Value::Bool(false), false, false, true),
        )?;
        let cancel_bubble_key = key_from_str(scope, "cancelBubble")?;
        scope.define_property(
          obj,
          cancel_bubble_key,
          data_property(Value::Bool(false), true, false, true),
        )?;
        let immediate_stop_key = key_from_str(scope, EVENT_IMMEDIATE_STOP_KEY)?;
        scope.define_property(
          obj,
          immediate_stop_key,
          data_property(Value::Bool(false), true, false, true),
        )?;

        let initialized_key = key_from_str(scope, EVENT_INITIALIZED_KEY)?;
        scope.define_property(
          obj,
          initialized_key,
          data_property(Value::Bool(true), true, false, true),
        )?;

        // Brand-check for EventTarget.dispatchEvent().
        let brand_key = key_from_str(scope, EVENT_BRAND_KEY)?;
        scope.define_property(
          obj,
          brand_key,
          data_property(Value::Bool(true), false, false, false),
        )?;
        let kind_key = key_from_str(scope, EVENT_KIND_KEY)?;
        scope.define_property(
          obj,
          kind_key,
          data_property(Value::Number(0.0), false, false, false),
        )?;

        Ok(Value::Undefined)
      }
      ("CustomEvent", "constructor", 0) => {
        let obj = Self::require_receiver_object(receiver)?;
        scope.push_root(Value::Object(obj))?;

        let type_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let init_value = args.get(1).copied().unwrap_or(Value::Undefined);

        let mut bubbles = false;
        let mut cancelable = false;
        let mut composed = false;
        let mut detail = Value::Null;
        if let Value::Object(init_obj) = init_value {
          scope.push_root(Value::Object(init_obj))?;
          let bubbles_key = key_from_str(scope, "bubbles")?;
          if let Some(v) = scope
            .heap()
            .object_get_own_data_property_value(init_obj, &bubbles_key)?
          {
            bubbles = scope.heap().to_boolean(v)?;
          }

          let cancelable_key = key_from_str(scope, "cancelable")?;
          if let Some(v) = scope
            .heap()
            .object_get_own_data_property_value(init_obj, &cancelable_key)?
          {
            cancelable = scope.heap().to_boolean(v)?;
          }

          let composed_key = key_from_str(scope, "composed")?;
          if let Some(v) = scope
            .heap()
            .object_get_own_data_property_value(init_obj, &composed_key)?
          {
            composed = scope.heap().to_boolean(v)?;
          }

          let detail_key = key_from_str(scope, "detail")?;
          if let Some(v) = scope
            .heap()
            .object_get_own_data_property_value(init_obj, &detail_key)?
          {
            if !matches!(v, Value::Undefined) {
              detail = v;
            }
          }
        }

        let type_key = key_from_str(scope, "type")?;
        scope.define_property(
          obj,
          type_key,
          data_property(type_value, false, false, true),
        )?;
        let bubbles_key = key_from_str(scope, "bubbles")?;
        scope.define_property(
          obj,
          bubbles_key,
          data_property(Value::Bool(bubbles), false, false, true),
        )?;
        let cancelable_key = key_from_str(scope, "cancelable")?;
        scope.define_property(
          obj,
          cancelable_key,
          data_property(Value::Bool(cancelable), false, false, true),
        )?;
        let composed_key = key_from_str(scope, "composed")?;
        scope.define_property(
          obj,
          composed_key,
          data_property(Value::Bool(composed), false, false, true),
        )?;

        let detail_key = key_from_str(scope, "detail")?;
        scope.define_property(
          obj,
          detail_key,
          data_property(detail, false, false, true),
        )?;

        // Default Event fields required by the dom2 dispatch bridge.
        let target_key = key_from_str(scope, "target")?;
        scope.define_property(
          obj,
          target_key,
          data_property(Value::Null, false, false, true),
        )?;
        let src_element_key = key_from_str(scope, "srcElement")?;
        scope.define_property(
          obj,
          src_element_key,
          data_property(Value::Null, false, false, true),
        )?;
        let current_target_key = key_from_str(scope, "currentTarget")?;
        scope.define_property(
          obj,
          current_target_key,
          data_property(Value::Null, false, false, true),
        )?;
        let event_phase_key = key_from_str(scope, "eventPhase")?;
        scope.define_property(
          obj,
          event_phase_key,
          data_property(Value::Number(0.0), false, false, true),
        )?;
        let time_stamp_key = key_from_str(scope, "timeStamp")?;
        scope.define_property(
          obj,
          time_stamp_key,
          data_property(Value::Number(0.0), false, false, true),
        )?;

        // LegacyUnforgeable `isTrusted`: must be an own, non-configurable property.
        let is_trusted_key = key_from_str(scope, "isTrusted")?;
        scope.define_property(
          obj,
          is_trusted_key,
          data_property(Value::Bool(false), false, true, false),
        )?;

        let default_prevented_key = key_from_str(scope, "defaultPrevented")?;
        scope.define_property(
          obj,
          default_prevented_key,
          data_property(Value::Bool(false), false, false, true),
        )?;
        let cancel_bubble_key = key_from_str(scope, "cancelBubble")?;
        scope.define_property(
          obj,
          cancel_bubble_key,
          data_property(Value::Bool(false), true, false, true),
        )?;
        let immediate_stop_key = key_from_str(scope, EVENT_IMMEDIATE_STOP_KEY)?;
        scope.define_property(
          obj,
          immediate_stop_key,
          data_property(Value::Bool(false), true, false, true),
        )?;

        let initialized_key = key_from_str(scope, EVENT_INITIALIZED_KEY)?;
        scope.define_property(
          obj,
          initialized_key,
          data_property(Value::Bool(true), true, false, true),
        )?;

        // Brand-check for EventTarget.dispatchEvent().
        let brand_key = key_from_str(scope, EVENT_BRAND_KEY)?;
        scope.define_property(
          obj,
          brand_key,
          data_property(Value::Bool(true), false, false, false),
        )?;
        let kind_key = key_from_str(scope, EVENT_KIND_KEY)?;
        scope.define_property(
          obj,
          kind_key,
          data_property(Value::Number(1.0), false, false, false),
        )?;

        Ok(Value::Undefined)
      }
      ("EventTarget", "constructor", 0) => {
        let obj = Self::require_receiver_object(receiver)?;
        scope.heap_mut().object_set_host_slots(
          obj,
          HostSlots {
            a: EVENT_TARGET_HOST_TAG,
            b: 0,
          },
        )?;
        let child_id = gc_object_id(obj);
        // Integrate `new EventTarget()` with the shared DOM event registry so `dispatchEvent`
        // participates in capture/bubble semantics (and so other opaque targets can reference this
        // target as a parent).
        match self.with_dom_host(vm, |host| {
          host.mutate_dom(|dom| {
            let registry = dom.events();
            registry.register_opaque_target(child_id, WeakGcObject::new(obj));
            // Defensive: ensure any stale parent mapping is cleared if this object is initialized
            // multiple times.
            registry.set_opaque_parent(child_id, None);
            ((), false)
          });
          Ok(())
        }) {
          Ok(()) => {}
          // Fallback: when called without an active DOM host (e.g. standalone realms), keep the
          // legacy per-target listener list behavior.
          Err(VmError::TypeError(msg)) if msg == DOM_HOST_NOT_AVAILABLE_ERROR => {
            self
              .event_targets
              .entry(WeakGcObject::from(obj))
              .or_default();
          }
          Err(err) => return Err(err),
        }
        Ok(Value::Undefined)
      }
      ("EventTarget", "constructor", 1) => {
        // FastRender-only extension: `new EventTarget(parent)` (used by curated WPT tests).
        //
        // The overload is by argument count so we avoid expensive platform object conversions here.
        // The parent value is forwarded by the bindings generator and can be inspected via `args`.
        let obj = Self::require_receiver_object(receiver)?;
        scope.heap_mut().object_set_host_slots(
          obj,
          HostSlots {
            a: EVENT_TARGET_HOST_TAG,
            b: 0,
          },
        )?;
        let parent_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let mut parent_target: Option<web_events::EventTargetId> = None;
        let mut parent_obj: Option<GcObject> = None;
        if !matches!(parent_value, Value::Undefined | Value::Null) {
          let Value::Object(obj) = parent_value else {
            return Err(VmError::TypeError(
              "EventTarget parent must be null, undefined, or an EventTarget",
            ));
          };
          parent_obj = Some(obj);

          // Accept DOM-backed EventTargets (window/document/node wrappers) and opaque EventTargets
          // (AbortSignal / `new EventTarget()`).
          if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
            if data.window_obj() == Some(obj) {
              parent_target = Some(web_events::EventTargetId::Window);
            } else if data.document_obj() == Some(obj) {
              parent_target = Some(web_events::EventTargetId::Document);
            } else if let Some(platform) = data.dom_platform_mut() {
              if let Ok(t) = platform.event_target_id_for_value(scope.heap(), Value::Object(obj)) {
                parent_target = Some(t);
              }
            }
          }
          if parent_target.is_none() {
            let slots = match scope.heap().object_host_slots(obj) {
              Ok(slots) => slots,
              Err(VmError::InvalidHandle { .. }) if scope.heap().is_valid_object(obj) => None,
              Err(err) => return Err(err),
            };
            if matches!(
              slots,
              Some(slots) if slots.a == EVENT_TARGET_HOST_TAG || slots.b == EVENT_TARGET_HOST_TAG
            ) {
              parent_target = Some(web_events::EventTargetId::Opaque(gc_object_id(obj)));
            }
          }

          if parent_target.is_none() {
            return Err(VmError::TypeError(
              "EventTarget parent must be null, undefined, or an EventTarget",
            ));
          }
        }

        // Store a strong reference to the parent so it stays alive as long as the child EventTarget
        // wrapper is reachable (matching the behavior of an internal parent slot).
        {
          let state = self.event_targets.entry(WeakGcObject::from(obj)).or_default();

          // If the object is somehow initialized twice, ensure we do not leak persistent roots.
          if let Some(parent) = state.parent.take() {
            scope.heap_mut().remove_root(parent.root);
          }

          if parent_obj.is_some() {
            let root = scope.heap_mut().add_root(parent_value)?;
            state.parent = Some(RootedValue {
              value: parent_value,
              root,
            });
          }
        }

        let child_id = gc_object_id(obj);
        // Integrate with the shared DOM event registry so `dispatchEvent` uses the spec-shaped
        // capture/target/bubble algorithm even for opaque targets.
        match self.with_dom_host(vm, |host| {
          host.mutate_dom(|dom| {
            let registry = dom.events();
            registry.register_opaque_target(child_id, WeakGcObject::new(obj));
            registry.set_opaque_parent(child_id, parent_target);
            if let Some(parent_target) = parent_target {
              if let (web_events::EventTargetId::Opaque(parent_id), Some(parent_obj)) =
                (parent_target, parent_obj)
              {
                registry.register_opaque_target(parent_id, WeakGcObject::new(parent_obj));
              }
            }
            ((), false)
          });
          Ok(())
        }) {
          Ok(()) => {}
          Err(VmError::TypeError(msg)) if msg == DOM_HOST_NOT_AVAILABLE_ERROR => {}
          Err(err) => return Err(err),
        }

        Ok(Value::Undefined)
      }
      ("EventTarget", "addEventListener", 0) => {
        let obj = self.require_event_target_receiver(vm, scope, receiver)?;

        // When possible, route all EventTargets through the shared `dom2` + `web::events` listener
        // registry so opaque targets (`new EventTarget()`, `AbortSignal`, etc) participate in
        // capture/bubble semantics.
        let abort_cleanup_call_id = self.abort_signal_listener_cleanup_call_id(vm)?;
        if let Some(result) = with_active_vm_host_and_hooks(vm, |vm, host, hooks| {
          event_target_add_event_listener_dom2(
            vm,
            scope,
            host,
            hooks,
            abort_cleanup_call_id,
            obj,
            args,
          )
        })? {
          return Ok(result);
        }

        let Some(Value::String(_)) = args.get(0).copied() else {
          return Err(VmError::TypeError(
            "EventTarget.addEventListener: missing type",
          ));
        };
        let event_type = js_string_to_rust_string(scope, args[0])?;

        let callback = args.get(1).copied().unwrap_or(Value::Undefined);
        if matches!(callback, Value::Null | Value::Undefined) {
          return Ok(Value::Undefined);
        }
        // `EventListener` is a callback interface: the WebIDL binding layer validates that this is
        // either callable or an object with a callable `handleEvent` method. We treat any object
        // here as a valid listener and invoke it per callback-interface rules during dispatch.
        let Value::Object(_) = callback else {
          return Err(VmError::TypeError("EventTarget listener is not callable"));
        };

        let capture = get_capture_option(scope, args.get(2).copied().unwrap_or(Value::Undefined))?;
        let once = get_once_option(scope, args.get(2).copied().unwrap_or(Value::Undefined))?;

        let state = self
          .event_targets
          .entry(WeakGcObject::from(obj))
          .or_default();
        if state.listeners.iter().any(|l| {
          l.event_type == event_type && l.callback.value == callback && l.capture == capture
        }) {
          return Ok(Value::Undefined);
        }

        let root = scope.heap_mut().add_root(callback)?;
        state.listeners.push(EventListenerEntry {
          event_type,
          callback: RootedCallback {
            value: callback,
            root,
          },
          capture,
          once,
        });
        Ok(Value::Undefined)
      }
      ("EventTarget", "removeEventListener", 0) => {
        let obj = self.require_event_target_receiver(vm, scope, receiver)?;

        if let Some(result) = with_active_vm_host_and_hooks(vm, |vm, host, hooks| {
          event_target_remove_event_listener_dom2(vm, scope, host, hooks, obj, args)
        })? {
          return Ok(result);
        }

        let Some(Value::String(_)) = args.get(0).copied() else {
          return Ok(Value::Undefined);
        };
        let event_type = js_string_to_rust_string(scope, args[0])?;

        let callback = args.get(1).copied().unwrap_or(Value::Undefined);
        if matches!(callback, Value::Null | Value::Undefined) {
          return Ok(Value::Undefined);
        }
        let Value::Object(_) = callback else {
          return Ok(Value::Undefined);
        };

        let capture = get_capture_option(scope, args.get(2).copied().unwrap_or(Value::Undefined))?;

        let Some(state) = self.event_targets.get_mut(&WeakGcObject::from(obj)) else {
          return Ok(Value::Undefined);
        };

        let heap = scope.heap_mut();
        state.listeners.retain(|listener| {
          if listener.event_type == event_type
            && listener.callback.value == callback
            && listener.capture == capture
          {
            heap.remove_root(listener.callback.root);
            false
          } else {
            true
          }
        });
        Ok(Value::Undefined)
      }
      ("EventTarget", "dispatchEvent", 0) => {
        let obj = self.require_event_target_receiver(vm, scope, receiver)?;

        if let Some(result) = with_active_vm_host_and_hooks(vm, |vm, host, hooks| {
          event_target_dispatch_event_dom2(vm, scope, host, hooks, obj, args)
        })? {
          return Ok(result);
        }

        let event_val = args.get(0).copied().unwrap_or(Value::Undefined);

        // Snapshot listeners before touching JS to avoid re-entrancy hazards.
        let listeners_snapshot: Vec<EventListenerEntry> = self
          .event_targets
          .get(&WeakGcObject::from(obj))
          .map(|state| state.listeners.clone())
          .unwrap_or_default();

        // Keep callbacks alive for the duration of dispatch. Without this, a listener can remove
        // another listener (dropping its persistent root) and trigger a GC before we reach it in
        // `listeners_snapshot`, leaving us with a stale handle.
        if !listeners_snapshot.is_empty() {
          let mut callback_values: Vec<Value> = Vec::new();
          callback_values
            .try_reserve(listeners_snapshot.len())
            .map_err(|_| VmError::OutOfMemory)?;
          for listener in &listeners_snapshot {
            callback_values.push(listener.callback.value);
          }
          scope.push_roots(&callback_values)?;
        }

        // Resolve `event.type` (best-effort).
        //
        // We first attempt to read an *own data property* named "type" so we can implement
        // `{ once: true }` without risking re-entrancy: reading a data property cannot invoke user
        // code, while `vm.get` can trigger getters/Proxy traps.
        let (event_type, type_is_own_data_property) = match event_val {
          Value::Object(ev_obj) => {
            let key = key_from_str(scope, "type")?;
            let own_type = match scope
              .heap()
              .object_get_own_data_property_value(ev_obj, &key)
            {
              Ok(value) => value,
              // Accessor `type` (or non-data) is not safe to read without invoking user code; fall
              // back to `Get` below.
              Err(VmError::PropertyNotData) => None,
              Err(err) => return Err(err),
            };
            match own_type {
              Some(value @ Value::String(_)) => (js_string_to_rust_string(scope, value)?, true),
              Some(_value) => {
                return Err(VmError::TypeError(
                  "EventTarget.dispatchEvent: event.type is not a string",
                ))
              }
              None => {
                let value = get_with_active_vm_host_and_hooks(vm, scope, ev_obj, key)?;
                if let Value::String(_) = value {
                  (js_string_to_rust_string(scope, value)?, false)
                } else {
                  return Err(VmError::TypeError(
                    "EventTarget.dispatchEvent: event.type is not a string",
                  ));
                }
              }
            }
          }
          _ => {
            return Err(VmError::TypeError(
              "EventTarget.dispatchEvent: expected event object",
            ))
          }
        };

        // Implement `{ once: true }` by removing matching listeners *before* invoking callbacks.
        //
        // Safety note: `dispatchEvent` can call into user JS while invoking callbacks, and that JS
        // can re-enter WebIDL dispatch. For soundness we must not touch `self` after any operation
        // that could invoke user code. We therefore only perform the `{ once: true }` removal when
        // we resolved `event.type` via an own data property lookup (no user code).
        if type_is_own_data_property {
          if let Some(state) = self.event_targets.get_mut(&WeakGcObject::from(obj)) {
            let heap = scope.heap_mut();
            state.listeners.retain(|listener| {
              if listener.once && listener.event_type == event_type {
                heap.remove_root(listener.callback.root);
                false
              } else {
                true
              }
            });
          }
        }

        // Invoke listeners synchronously in registration order.
        //
        // NOTE: Calling into JS here can re-enter WebIDL dispatch through `host_from_hooks()`. For
        // soundness we must not touch `self` after any JS call, so all state mutations must happen
        // before entering this loop.
        let handle_event_key = key_from_str(scope, "handleEvent")?;
        for listener in listeners_snapshot.into_iter() {
          if listener.event_type != event_type {
            continue;
          }
          let callback = listener.callback.value;

          // Callback-interface invocation:
          // - If callable, call it with `this = event target`.
          // - Otherwise, call `callback.handleEvent(event)` with `this = callback`.
          if is_callable(scope, callback) {
            let _ = call_with_active_vm_host_and_hooks(
              vm,
              scope,
              callback,
              Value::Object(obj),
              &[event_val],
            )?;
            continue;
          }

          let Value::Object(callback_obj) = callback else {
            return Err(VmError::TypeError(
              "EventTarget.dispatchEvent: listener is not an object",
            ));
          };

          // `GetMethod(callback, "handleEvent")`
          let handle_event =
            get_with_active_vm_host_and_hooks(vm, scope, callback_obj, handle_event_key)?;
          if matches!(handle_event, Value::Undefined | Value::Null) {
            return Err(VmError::TypeError(
              "Callback interface object is missing a callable handleEvent method",
            ));
          }
          if !is_callable(scope, handle_event) {
            return Err(VmError::TypeError("GetMethod: target is not callable"));
          }
          scope.push_root(handle_event)?;

          let _ =
            call_with_active_vm_host_and_hooks(vm, scope, handle_event, callback, &[event_val])?;
        }

        Ok(Value::Bool(true))
      }

      ("Node", "nodeType", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let node_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), receiver)?;
        let node_type = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if node_id.index() >= dom.nodes_len() {
              return Err(DomError::NotFoundError);
            }
            Ok(match &dom.node(node_id).kind {
              NodeKind::Document { .. } => 9,
              NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => 11,
              NodeKind::Text { .. } => 3,
              NodeKind::Comment { .. } => 8,
              NodeKind::ProcessingInstruction { .. } => 7,
              NodeKind::Doctype { .. } => 10,
              NodeKind::Element { .. } | NodeKind::Slot { .. } => 1,
            })
          }))
        })?;
        match node_type {
          Ok(code) => Ok(Value::Number(code as f64)),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }
      ("Node", "nodeName", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let node_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), receiver)?;
        let name = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if node_id.index() >= dom.nodes_len() {
              return Err(DomError::NotFoundError);
            }
            Ok(match &dom.node(node_id).kind {
              NodeKind::Document { .. } => "#document".to_string(),
              NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => "#document-fragment".to_string(),
              NodeKind::Text { .. } => "#text".to_string(),
              NodeKind::Comment { .. } => "#comment".to_string(),
              NodeKind::ProcessingInstruction { target, .. } => target.clone(),
              NodeKind::Doctype { name, .. } => name.clone(),
              NodeKind::Element { tag_name, .. } => tag_name.to_ascii_uppercase(),
              NodeKind::Slot { .. } => "SLOT".to_string(),
            })
          }))
        })?;
        match name {
          Ok(name) => {
            let s = scope.alloc_string(&name)?;
            scope.push_root(Value::String(s))?;
            Ok(Value::String(s))
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }
      ("Node", "parentNode", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let handle = require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), receiver)?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;
        let parent = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if node_id.index() >= dom.nodes_len() {
              return Err(DomError::NotFoundError);
            }
            // ShadowRoot wrapper tree-facing semantics: `ShadowRoot.parentNode` is always null.
            if matches!(dom.node(node_id).kind, NodeKind::ShadowRoot { .. }) {
              return Ok(None);
            }
            dom.parent(node_id)
          }))
        })?;
        let parent = match parent {
          Ok(v) => v,
          Err(err) => return Err(self.dom_error_to_vm_error(vm, scope, err)),
        };
        let Some(parent_id) = parent else {
          return Ok(Value::Null);
        };
        let primary = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if parent_id.index() >= dom.nodes_len() {
              DomInterface::Node
            } else {
              DomInterface::primary_for_node_kind(&dom.node(parent_id).kind)
            }
          }))
        })?;
        let wrapper =
          require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(scope, document_id, parent_id, primary)?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("Node", "parentElement", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let handle = require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), receiver)?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;

        let parent = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            // ShadowRoot tree-facing semantics: `ShadowRoot.parentElement` is always null.
            //
            // dom2 stores shadow roots as children of their host element, so `parent_node(..)` would
            // otherwise return the host here.
            if node_id.index() < dom.nodes_len()
              && matches!(dom.node(node_id).kind, NodeKind::ShadowRoot { .. })
            {
              return None;
            }
            let Some(parent_id) = dom.parent_node(node_id) else {
              return None;
            };

            let primary = if parent_id.index() >= dom.nodes_len() {
              DomInterface::Node
            } else {
              let kind = &dom.node(parent_id).kind;
              if !matches!(kind, NodeKind::Element { .. } | NodeKind::Slot { .. }) {
                return None;
              }
              DomInterface::primary_for_node_kind(kind)
            };

            Some((parent_id, primary))
          }))
        })?;

        let Some((parent_id, primary)) = parent else {
          return Ok(Value::Null);
        };

        let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
          scope,
          document_id,
          parent_id,
          primary,
        )?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("Node", "childNodes", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let Value::Object(wrapper_obj) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };

        let handle =
          require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), Value::Object(wrapper_obj))?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;

        // WebIDL wrapper objects store a back-reference to their owning `Document` wrapper via an
        // internal `__fastrender_*` property; use the same scheme as the handwritten `childNodes`
        // shim so we can adopt the realm's NodeList prototype (`instanceof NodeList`).
        let wrapper_document_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let document_obj = match scope
          .heap()
          .object_get_own_data_property_value(wrapper_obj, &wrapper_document_key)?
        {
          Some(Value::Object(obj)) => obj,
          _ => return Err(VmError::TypeError("Illegal invocation")),
        };
        scope.push_root(Value::Object(document_obj))?;

        let child_nodes_key = key_from_str(scope, NODE_CHILD_NODES_KEY)?;
        let list_obj = match scope
          .heap()
          .object_get_own_data_property_value(wrapper_obj, &child_nodes_key)?
        {
          Some(Value::Object(obj)) => obj,
          _ => {
            let list_obj = scope.alloc_object()?;
            scope.push_root(Value::Object(list_obj))?;

            let proto_key = key_from_str(scope, NODE_LIST_PROTOTYPE_KEY)?;
            let proto = match scope
              .heap()
              .object_get_own_data_property_value(document_obj, &proto_key)?
            {
              Some(Value::Object(obj)) => obj,
              _ => {
                return Err(VmError::InvariantViolation(
                  "missing NodeList prototype for Node.childNodes",
                ))
              }
            };
            scope.heap_mut().object_set_prototype(list_obj, Some(proto))?;

            // Keep the root wrapper alive even if the caller only holds the NodeList object.
            let root_key = key_from_str(scope, NODE_LIST_ROOT_KEY)?;
            scope.define_property(
              list_obj,
              root_key,
              data_property(Value::Object(wrapper_obj), false, false, false),
            )?;

            scope.define_property(
              wrapper_obj,
              child_nodes_key,
              data_property(Value::Object(list_obj), false, false, false),
            )?;
            list_obj
          }
        };

        // Keep the cached NodeList live.
        self.sync_cached_child_nodes_for_wrapper(vm, scope, wrapper_obj, node_id, document_id)?;
        Ok(Value::Object(list_obj))
      }
      ("Node", "firstChild", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let handle = require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), receiver)?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;
        let first = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            let nodes = dom.nodes();
            let Some(node) = nodes.get(node_id.index()) else {
              return None;
            };
            if matches!(node.kind, NodeKind::Element { .. } | NodeKind::Slot { .. }) {
              // Skip ShadowRoot children for element-like nodes (light DOM traversal).
              for &child_id in &node.children {
                let Some(child_node) = nodes.get(child_id.index()) else {
                  // Match `dom2::Document::first_child` behaviour: ignore out-of-bounds IDs.
                  continue;
                };
                if matches!(child_node.kind, NodeKind::ShadowRoot { .. }) {
                  continue;
                }
                return Some(child_id);
              }
              return None;
            }
            dom.first_child(node_id)
          }))
        })?;
        let Some(first_id) = first else {
          return Ok(Value::Null);
        };
        let primary = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if first_id.index() >= dom.nodes_len() {
              DomInterface::Node
            } else {
              DomInterface::primary_for_node_kind(&dom.node(first_id).kind)
            }
          }))
        })?;
        let wrapper =
          require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(scope, document_id, first_id, primary)?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("Node", "lastChild", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let handle = require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), receiver)?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;
        let last = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            let nodes = dom.nodes();
            let Some(node) = nodes.get(node_id.index()) else {
              return None;
            };
            if matches!(node.kind, NodeKind::Element { .. } | NodeKind::Slot { .. }) {
              // Skip ShadowRoot children for element-like nodes (light DOM traversal).
              for &child_id in node.children.iter().rev() {
                let Some(child_node) = nodes.get(child_id.index()) else {
                  // Match `dom2::Document::last_child` behaviour: ignore out-of-bounds IDs.
                  continue;
                };
                if matches!(child_node.kind, NodeKind::ShadowRoot { .. }) {
                  continue;
                }
                return Some(child_id);
              }
              return None;
            }
            dom.last_child(node_id)
          }))
        })?;
        let Some(last_id) = last else {
          return Ok(Value::Null);
        };
        let primary = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if last_id.index() >= dom.nodes_len() {
              DomInterface::Node
            } else {
              DomInterface::primary_for_node_kind(&dom.node(last_id).kind)
            }
          }))
        })?;
        let wrapper =
          require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(scope, document_id, last_id, primary)?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("Node", "nextSibling", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let handle = require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), receiver)?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;
        let sib = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            let nodes = dom.nodes();
            let Some(node) = nodes.get(node_id.index()) else {
              return None;
            };
            // ShadowRoot wrapper tree-facing semantics: `ShadowRoot.nextSibling` is always null.
            if matches!(node.kind, NodeKind::ShadowRoot { .. }) {
              return None;
            }

            let Some(parent_id) = node.parent else {
              return None;
            };
            let Some(parent_node) = nodes.get(parent_id.index()) else {
              return None;
            };

            if !matches!(
              parent_node.kind,
              NodeKind::Element { .. } | NodeKind::Slot { .. }
            ) {
              return dom.next_sibling(node_id);
            }

            // Light DOM sibling traversal must not step onto ShadowRoot siblings.
            let pos = parent_node.children.iter().position(|&c| c == node_id)?;
            for &sib_id in parent_node.children.iter().skip(pos + 1) {
              let Some(sib_node) = nodes.get(sib_id.index()) else {
                continue;
              };
              if matches!(sib_node.kind, NodeKind::ShadowRoot { .. }) {
                continue;
              }
              return Some(sib_id);
            }
            None
          }))
        })?;
        let Some(sib_id) = sib else {
          return Ok(Value::Null);
        };
        let primary = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if sib_id.index() >= dom.nodes_len() {
              DomInterface::Node
            } else {
              DomInterface::primary_for_node_kind(&dom.node(sib_id).kind)
            }
          }))
        })?;
        let wrapper =
          require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(scope, document_id, sib_id, primary)?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("Node", "previousSibling", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let handle = require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), receiver)?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;
        let sib = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            let nodes = dom.nodes();
            let Some(node) = nodes.get(node_id.index()) else {
              return None;
            };
            // ShadowRoot wrapper tree-facing semantics: `ShadowRoot.previousSibling` is always null.
            if matches!(node.kind, NodeKind::ShadowRoot { .. }) {
              return None;
            }

            let Some(parent_id) = node.parent else {
              return None;
            };
            let Some(parent_node) = nodes.get(parent_id.index()) else {
              return None;
            };

            if !matches!(
              parent_node.kind,
              NodeKind::Element { .. } | NodeKind::Slot { .. }
            ) {
              return dom.previous_sibling(node_id);
            }

            // Light DOM sibling traversal must not step onto ShadowRoot siblings.
            let pos = parent_node.children.iter().position(|&c| c == node_id)?;
            for &sib_id in parent_node.children.iter().take(pos).rev() {
              let Some(sib_node) = nodes.get(sib_id.index()) else {
                continue;
              };
              if matches!(sib_node.kind, NodeKind::ShadowRoot { .. }) {
                continue;
              }
              return Some(sib_id);
            }
            None
          }))
        })?;
        let Some(sib_id) = sib else {
          return Ok(Value::Null);
        };
        let primary = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if sib_id.index() >= dom.nodes_len() {
              DomInterface::Node
            } else {
              DomInterface::primary_for_node_kind(&dom.node(sib_id).kind)
            }
          }))
        })?;
        let wrapper =
          require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(scope, document_id, sib_id, primary)?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("Node", "hasChildNodes", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let node_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), receiver)?;
        let has = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if node_id.index() >= dom.nodes_len() {
              return Err(DomError::NotFoundError);
            }
            let node = dom.node(node_id);

            // Light DOM traversal must not treat the ShadowRoot child as a visible child node.
            if matches!(node.kind, NodeKind::Element { .. } | NodeKind::Slot { .. }) {
              for &child_id in &node.children {
                if child_id.index() >= dom.nodes_len() {
                  continue;
                }
                if matches!(dom.node(child_id).kind, NodeKind::ShadowRoot { .. }) {
                  continue;
                }
                return Ok(true);
              }
              return Ok(false);
            }

            Ok(dom.first_child(node_id).is_some())
          }))
        })?;
        match has {
          Ok(value) => Ok(Value::Bool(value)),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }
      ("Node", "contains", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let node_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), receiver)?;

        let other_value = args.get(0).copied().unwrap_or(Value::Undefined);
        if matches!(other_value, Value::Null | Value::Undefined) {
          return Ok(Value::Bool(false));
        }
        let other_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), other_value)?;

        let contains = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if node_id.index() >= dom.nodes_len() || other_id.index() >= dom.nodes_len() {
              return Err(DomError::NotFoundError);
            }

            // ShadowRoot-safe inclusive descendant check:
            // - `ShadowRoot` nodes are tree boundaries: traversal stops at the shadow root.
            // - This prevents `host.contains(shadowRoot)` and `host.contains(nodeInShadow)` from
            //   returning true (ShadowRoot is not part of the light DOM tree).
            let mut current = Some(other_id);
            let mut remaining = dom.nodes_len() + 1;
            while let Some(id) = current {
              if remaining == 0 {
                break;
              }
              remaining -= 1;

              if id == node_id {
                return Ok(true);
              }
              if id.index() >= dom.nodes_len() {
                break;
              }
              if matches!(dom.node(id).kind, NodeKind::ShadowRoot { .. }) {
                current = None;
                continue;
              }
              current = dom.parent(id)?;
            }
            Ok(false)
          }))
        })?;
        match contains {
          Ok(value) => Ok(Value::Bool(value)),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }
      ("Node", "compareDocumentPosition", 0) => {
        const DOCUMENT_POSITION_DISCONNECTED: u16 = 0x01;
        const DOCUMENT_POSITION_PRECEDING: u16 = 0x02;
        const DOCUMENT_POSITION_FOLLOWING: u16 = 0x04;
        const DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC: u16 = 0x20;

        let receiver = receiver.unwrap_or(Value::Undefined);
        let this_handle = require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), receiver)?;
        let other_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let other_handle = require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), other_value)?;

        if this_handle.document_id == other_handle.document_id && this_handle.node_id == other_handle.node_id {
          return Ok(Value::Number(0.0));
        }

        let Some(mask) = with_active_vm_host_and_hooks(vm, |vm, host, _hooks| {
          let this_dom_ptr = dom_ptr_for_document_id_read(vm, host, this_handle.document_id)
            .ok_or(VmError::TypeError("Illegal invocation"))?;
          let other_dom_ptr = dom_ptr_for_document_id_read(vm, host, other_handle.document_id)
            .ok_or(VmError::TypeError("Illegal invocation"))?;

          let mask = if this_dom_ptr == other_dom_ptr {
            // SAFETY: `dom_ptr_for_document_id_read` returns a valid pointer for the duration of this
            // JS execution boundary.
            let dom = unsafe { this_dom_ptr.as_ref() };
            dom.compare_document_position(this_handle.node_id, other_handle.node_id)
          } else {
            let mut out = DOCUMENT_POSITION_DISCONNECTED | DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC;
            out |= if this_handle.document_id < other_handle.document_id {
              DOCUMENT_POSITION_FOLLOWING
            } else {
              DOCUMENT_POSITION_PRECEDING
            };
            out
          };
          Ok(mask)
        })?
        else {
          return Err(VmError::TypeError(DOM_HOST_NOT_AVAILABLE_ERROR));
        };

        Ok(Value::Number(mask as f64))
      }
      ("Node", "isEqualNode", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let this_handle = require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), receiver)?;

        let other_value = args.get(0).copied().unwrap_or(Value::Undefined);
        if matches!(other_value, Value::Null | Value::Undefined) {
          return Ok(Value::Bool(false));
        }
        let other_handle = match require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), other_value) {
          Ok(handle) => handle,
          Err(_) => return Ok(Value::Bool(false)),
        };

        let result = with_active_vm_host_and_hooks(vm, |vm, host, _hooks| {
          let dom_a_ptr = dom_ptr_for_document_id_read(vm, host, this_handle.document_id)
            .ok_or(VmError::TypeError(DOM_HOST_NOT_AVAILABLE_ERROR))?;
          let dom_b_ptr = dom_ptr_for_document_id_read(vm, host, other_handle.document_id)
            .ok_or(VmError::TypeError(DOM_HOST_NOT_AVAILABLE_ERROR))?;
          // SAFETY: pointers returned by `dom_ptr_for_document_id_read` are valid for the duration of
          // this host call.
          let dom_a = unsafe { dom_a_ptr.as_ref() };
          let dom_b = unsafe { dom_b_ptr.as_ref() };
          Ok(dom2_bindings::is_equal_node_from_dom(
            dom_a,
            this_handle.node_id,
            dom_b,
            other_handle.node_id,
          ))
        })?;

        match result {
          Some(value) => Ok(Value::Bool(value)),
          None => Err(VmError::TypeError(DOM_HOST_NOT_AVAILABLE_ERROR)),
        }
      }
      ("Node", "textContent", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let Value::Object(wrapper_obj) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let handle =
          require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), Value::Object(wrapper_obj))?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;
        if args.is_empty() {
          let text: Result<Option<String>, DomError> = self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              if node_id.index() >= dom.nodes_len() {
                return Err(DomError::NotFoundError);
              }
              Ok(dom2_bindings::text_content_get_from_dom(dom, node_id))
            }))
          })?;
          match text {
            Ok(Some(text)) => {
              let s = scope.alloc_string(&text)?;
              scope.push_root(Value::String(s))?;
              Ok(Value::String(s))
            }
            Ok(None) => Ok(Value::Null),
            Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
          }
        } else {
          let value = args.get(0).copied().unwrap_or(Value::Undefined);
          let value = match value {
            // `textContent` is `DOMString?`; `null` and `undefined` act as the empty string.
            Value::Undefined | Value::Null => String::new(),
            Value::String(_) => js_string_to_rust_string(scope, value)?,
            other => {
              let s = scope.heap_mut().to_string(other)?;
              scope.heap().get_string(s)?.to_utf8_lossy()
            }
          };

          let result: Result<(), DomError> = self.with_dom_host(vm, |host| {
            Ok(host.mutate_dom(|dom| {
              match dom2_bindings::text_content_set_from_dom(dom, node_id, &value) {
                Ok(result) => (Ok(()), result.render_affecting),
                Err(err) => (Err(err), false),
              }
            }))
          })?;
          match result {
            Ok(()) => {
              // Keep cached `childNodes` live NodeLists updated: `textContent` can replace/remove
              // all children.
              self.sync_cached_child_nodes_for_wrapper(
                vm,
                scope,
                wrapper_obj,
                node_id,
                document_id,
              )?;
              self.sync_live_html_collections(vm, scope)?;
              Ok(Value::Undefined)
            }
            Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
          }
        }
      }
      ("CharacterData", "data", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let node_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), receiver)?;

        if args.is_empty() {
          let data = self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              let Some(node) = dom.nodes().get(node_id.index()) else {
                return Err(DomError::NotFoundError);
              };
              match &node.kind {
                NodeKind::Text { content } => Ok(content.clone()),
                NodeKind::Comment { content } => Ok(content.clone()),
                NodeKind::ProcessingInstruction { data, .. } => Ok(data.clone()),
                _ => Err(DomError::InvalidNodeTypeError),
              }
            }))
          })?;
          match data {
            Ok(data) => Ok(Value::String(scope.alloc_string(&data)?)),
            Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
          }
        } else {
          let value = args.get(0).copied().unwrap_or(Value::Undefined);
          let value = {
            let s = scope.heap_mut().to_string(value)?;
            scope
              .heap()
              .get_string(s)
              .map(|s| s.to_utf8_lossy())
              .unwrap_or_default()
          };

          let result = self.with_dom_host(vm, |host| {
            Ok(host.mutate_dom(|dom| {
              let is_text_node = match dom.nodes().get(node_id.index()) {
                Some(node) => matches!(&node.kind, NodeKind::Text { .. }),
                None => return (Err(DomError::NotFoundError), false),
              };
              match dom.replace_data(node_id, 0, usize::MAX, &value) {
                Ok(changed) => (Ok(()), changed && is_text_node),
                Err(err) => (Err(err), false),
              }
            }))
          })?;
          match result {
            Ok(()) => Ok(Value::Undefined),
            Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
          }
        }
      }

      ("CharacterData", "length", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let node_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), receiver)?;
        let len = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            let Some(node) = dom.nodes().get(node_id.index()) else {
              return Err(DomError::NotFoundError);
            };
            let data = match &node.kind {
              NodeKind::Text { content } => content.as_str(),
              NodeKind::Comment { content } => content.as_str(),
              NodeKind::ProcessingInstruction { data, .. } => data.as_str(),
              _ => return Err(DomError::InvalidNodeTypeError),
            };
            Ok(data.encode_utf16().count())
          }))
        })?;
        match len {
          Ok(len) => Ok(Value::Number(len as f64)),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("CharacterData", "substringData", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let node_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), receiver)?;

        let offset_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let count_value = args.get(1).copied().unwrap_or(Value::Undefined);
        let offset = to_uint32_f64(scope.heap_mut().to_number(offset_value)?) as usize;
        let count = to_uint32_f64(scope.heap_mut().to_number(count_value)?) as usize;

        let units = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            let Some(node) = dom.nodes().get(node_id.index()) else {
              return Err(DomError::NotFoundError);
            };
            let data = match &node.kind {
              NodeKind::Text { content } => content.as_str(),
              NodeKind::Comment { content } => content.as_str(),
              NodeKind::ProcessingInstruction { data, .. } => data.as_str(),
              _ => return Err(DomError::InvalidNodeTypeError),
            };

            let units: Vec<u16> = data.encode_utf16().collect();
            if offset > units.len() {
              return Err(DomError::IndexSizeError);
            }
            let end = offset.saturating_add(count).min(units.len());
            Ok(units[offset..end].to_vec())
          }))
        })?;

        match units {
          Ok(units) => Ok(Value::String(scope.alloc_string_from_u16_vec(units)?)),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("CharacterData", "appendData", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let node_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), receiver)?;

        let data_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let data = {
          let s = scope.heap_mut().to_string(data_value)?;
          scope
            .heap()
            .get_string(s)
            .map(|s| s.to_utf8_lossy())
            .unwrap_or_default()
        };

        let result = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            let Some(node) = dom.nodes().get(node_id.index()) else {
              return (Err(DomError::NotFoundError), false);
            };
            let (offset, is_text_node) = match &node.kind {
              NodeKind::Text { content } => (content.encode_utf16().count(), true),
              NodeKind::Comment { content } => (content.encode_utf16().count(), false),
              NodeKind::ProcessingInstruction { data, .. } => (data.encode_utf16().count(), false),
              _ => return (Err(DomError::InvalidNodeTypeError), false),
            };

            match dom.replace_data(node_id, offset, 0, &data) {
              Ok(changed) => (Ok(()), changed && is_text_node),
              Err(err) => (Err(err), false),
            }
          }))
        })?;

        match result {
          Ok(()) => Ok(Value::Undefined),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("CharacterData", "insertData", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let node_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), receiver)?;

        let offset_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let data_value = args.get(1).copied().unwrap_or(Value::Undefined);

        let offset = to_uint32_f64(scope.heap_mut().to_number(offset_value)?) as usize;
        let data = {
          let s = scope.heap_mut().to_string(data_value)?;
          scope
            .heap()
            .get_string(s)
            .map(|s| s.to_utf8_lossy())
            .unwrap_or_default()
        };

        let result = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            let is_text_node = match dom.nodes().get(node_id.index()) {
              Some(node) => matches!(&node.kind, NodeKind::Text { .. }),
              None => return (Err(DomError::NotFoundError), false),
            };
            match dom.replace_data(node_id, offset, 0, &data) {
              Ok(changed) => (Ok(()), changed && is_text_node),
              Err(err) => (Err(err), false),
            }
          }))
        })?;

        match result {
          Ok(()) => Ok(Value::Undefined),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("CharacterData", "deleteData", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let node_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), receiver)?;

        let offset_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let count_value = args.get(1).copied().unwrap_or(Value::Undefined);
        let offset = to_uint32_f64(scope.heap_mut().to_number(offset_value)?) as usize;
        let count = to_uint32_f64(scope.heap_mut().to_number(count_value)?) as usize;

        let result = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            let is_text_node = match dom.nodes().get(node_id.index()) {
              Some(node) => matches!(&node.kind, NodeKind::Text { .. }),
              None => return (Err(DomError::NotFoundError), false),
            };
            match dom.replace_data(node_id, offset, count, "") {
              Ok(changed) => (Ok(()), changed && is_text_node),
              Err(err) => (Err(err), false),
            }
          }))
        })?;
        match result {
          Ok(()) => Ok(Value::Undefined),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("CharacterData", "replaceData", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let node_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), receiver)?;

        let offset_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let count_value = args.get(1).copied().unwrap_or(Value::Undefined);
        let data_value = args.get(2).copied().unwrap_or(Value::Undefined);

        let offset = to_uint32_f64(scope.heap_mut().to_number(offset_value)?) as usize;
        let count = to_uint32_f64(scope.heap_mut().to_number(count_value)?) as usize;
        let data = {
          let s = scope.heap_mut().to_string(data_value)?;
          scope
            .heap()
            .get_string(s)
            .map(|s| s.to_utf8_lossy())
            .unwrap_or_default()
        };

        let result = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            let is_text_node = match dom.nodes().get(node_id.index()) {
              Some(node) => matches!(&node.kind, NodeKind::Text { .. }),
              None => return (Err(DomError::NotFoundError), false),
            };
            match dom.replace_data(node_id, offset, count, &data) {
              Ok(changed) => (Ok(()), changed && is_text_node),
              Err(err) => (Err(err), false),
            }
          }))
        })?;
        match result {
          Ok(()) => Ok(Value::Undefined),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("Text", "splitText", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let handle = require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), receiver)?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;

        let offset_value = args.get(0).copied().unwrap_or(Value::Undefined);
        // DOM `Text.splitText(offset)` measures offsets in UTF-16 code units (WebIDL unsigned long).
        let offset_utf16 = to_uint32_f64(scope.heap_mut().to_number(offset_value)?) as usize;

        let result = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            let parent_id = match dom.parent(node_id) {
              Ok(v) => v,
              Err(err) => return (Err(err), false),
            };
            match dom.split_text(node_id, offset_utf16) {
              Ok(new_id) => (Ok((new_id, parent_id)), parent_id.is_some()),
              Err(err) => (Err(err), false),
            }
          }))
        })?;

        match result {
          Ok((new_id, parent_id)) => {
            if let Some(parent_id) = parent_id {
              let parent_wrapper = {
                let platform = require_dom_platform_mut(vm)?;
                platform.get_existing_wrapper_for_document_id(scope.heap(), document_id, parent_id)
              };
              if let Some(parent_wrapper) = parent_wrapper {
                self.sync_cached_child_nodes_for_wrapper(
                  vm,
                  scope,
                  parent_wrapper,
                  parent_id,
                  document_id,
                )?;
              }
            }

            let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
              scope,
              document_id,
              new_id,
              DomInterface::Text,
            )?;
            scope.push_root(Value::Object(wrapper))?;
            Ok(Value::Object(wrapper))
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("Node", "appendChild", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let Value::Object(parent_wrapper_obj) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let platform = require_dom_platform_mut(vm)?;
        let parent_handle =
          platform.require_node_handle(scope.heap(), Value::Object(parent_wrapper_obj))?;
        let parent_id = parent_handle.node_id;
        let document_id = parent_handle.document_id;
        let child_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let Value::Object(child_wrapper_obj) = child_value else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let child_handle = platform.require_node_handle(scope.heap(), child_value)?;
        let child_id = child_handle.node_id;
        let child_document_id = child_handle.document_id;

        // Determine whether the child is fragment-like (DocumentFragment/ShadowRoot) and snapshot
        // adoption mappings for cross-document moves.
        let (child_is_fragment_like, adopt_roots): (bool, Vec<DomNodeKey>) = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if child_id.index() >= dom.nodes_len() {
              return (false, Vec::new());
            }
            let kind = &dom.node(child_id).kind;
            let child_is_fragment_like =
              matches!(kind, NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. });
            let mut adopt_roots: Vec<DomNodeKey> = Vec::new();
            if child_document_id != document_id {
              match kind {
                NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => {
                  // Fragment insertion is transparent: adopt children, not the fragment itself.
                  for &child in dom.node(child_id).children.iter() {
                    if child.index() >= dom.nodes_len() {
                      continue;
                    }
                    if dom.node(child).parent != Some(child_id) {
                      continue;
                    }
                    adopt_roots.push(DomNodeKey::new(child_document_id, child));
                  }
                }
                NodeKind::Document { .. } => {}
                _ => adopt_roots.push(child_handle),
              }
            }
            (child_is_fragment_like, adopt_roots)
          }))
        })?;

        let mut adopt_mappings: Vec<(DocumentId, HashMap<NodeId, NodeId>)> = Vec::new();
        if !adopt_roots.is_empty() {
          adopt_mappings.reserve(adopt_roots.len());
          for handle in adopt_roots.iter().copied() {
            let root_id = handle.node_id;
            let mapping: HashMap<NodeId, NodeId> = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| {
                let mut mapping: HashMap<NodeId, NodeId> = HashMap::new();
                let mut stack: Vec<NodeId> = vec![root_id];
                let mut remaining = dom.nodes_len() + 1;
                while let Some(id) = stack.pop() {
                  if remaining == 0 {
                    break;
                  }
                  remaining -= 1;

                  if id.index() >= dom.nodes_len() {
                    continue;
                  }
                  mapping.insert(id, id);
                  let n = dom.node(id);
                  for &child in n.children.iter().rev() {
                    if child.index() >= dom.nodes_len() {
                      continue;
                    }
                    if dom.node(child).parent != Some(id) {
                      continue;
                    }
                    stack.push(child);
                  }
                }
                mapping
              }))
            })?;
            adopt_mappings.push((handle.document_id, mapping));
          }
        }

        let result: Result<Option<NodeId>, DomError> = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            let old_parent = match dom.parent(child_id) {
              Ok(v) => v,
              Err(err) => return (Err(err), false),
            };
            let res = if child_id.index() < dom.nodes_len()
              && matches!(dom.node(child_id).kind, NodeKind::ShadowRoot { .. })
            {
              dom.with_shadow_root_as_document_fragment(child_id, |dom| {
                dom.append_child(parent_id, child_id)
              })
            } else {
              dom.append_child(parent_id, child_id)
            };
            match res {
              Ok(changed) => (Ok(old_parent), changed),
              Err(err) => (Err(err), false),
            }
          }))
        })?;
        match result {
          Ok(old_parent_id) => {
            // Remap wrapper identity + ownerDocument for adopted subtrees.
            for (old_document_id, mapping) in adopt_mappings {
              require_dom_platform_mut(vm)?.remap_node_ids_between_documents(
                scope.heap_mut(),
                old_document_id,
                document_id,
                &mapping,
              )?;
            }

            // Keep cached `childNodes` live NodeLists updated for both the target parent and the
            // inserted node (DocumentFragment insertion mutates the fragment's children too).
            self.sync_cached_child_nodes_for_wrapper(
              vm,
              scope,
              parent_wrapper_obj,
              parent_id,
              document_id,
            )?;
            if child_is_fragment_like {
              self.sync_cached_child_nodes_for_wrapper(
                vm,
                scope,
                child_wrapper_obj,
                child_id,
                child_document_id,
              )?;
            }
            if let Some(old_parent_id) = old_parent_id {
              // `NodeId` values are only unique within a document, so only skip when the old parent
              // is *actually* the same node as the insertion parent.
              if !(child_document_id == document_id && old_parent_id == parent_id) {
                let old_parent_wrapper = {
                  let platform = require_dom_platform_mut(vm)?;
                  platform.get_existing_wrapper_for_document_id(scope.heap(), child_document_id, old_parent_id)
                };
                if let Some(old_parent_wrapper) = old_parent_wrapper {
                  self.sync_cached_child_nodes_for_wrapper(
                    vm,
                    scope,
                    old_parent_wrapper,
                    old_parent_id,
                    child_document_id,
                  )?;
                }
              }
            }

            self.sync_live_html_collections(vm, scope)?;
            // Per DOM, `appendChild` returns the inserted node (the same object identity passed in).
            Ok(Value::Object(child_wrapper_obj))
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }
      ("Node", "insertBefore", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let Value::Object(parent_wrapper_obj) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let platform = require_dom_platform_mut(vm)?;
        let parent_handle =
          platform.require_node_handle(scope.heap(), Value::Object(parent_wrapper_obj))?;
        let parent_id = parent_handle.node_id;
        let document_id = parent_handle.document_id;
        let child_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let Value::Object(child_wrapper_obj) = child_value else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let child_handle = platform.require_node_handle(scope.heap(), child_value)?;
        let child_id = child_handle.node_id;
        let child_document_id = child_handle.document_id;
        let reference = match args.get(1).copied() {
          None | Some(Value::Undefined) | Some(Value::Null) => None,
          Some(v) => Some(platform.require_node_id(scope.heap(), v)?),
        };

        let (child_is_fragment_like, adopt_roots): (bool, Vec<DomNodeKey>) = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if child_id.index() >= dom.nodes_len() {
              return (false, Vec::new());
            }
            let kind = &dom.node(child_id).kind;
            let child_is_fragment_like =
              matches!(kind, NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. });
            let mut adopt_roots: Vec<DomNodeKey> = Vec::new();
            if child_document_id != document_id {
              match kind {
                NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => {
                  for &child in dom.node(child_id).children.iter() {
                    if child.index() >= dom.nodes_len() {
                      continue;
                    }
                    if dom.node(child).parent != Some(child_id) {
                      continue;
                    }
                    adopt_roots.push(DomNodeKey::new(child_document_id, child));
                  }
                }
                NodeKind::Document { .. } => {}
                _ => adopt_roots.push(child_handle),
              }
            }
            (child_is_fragment_like, adopt_roots)
          }))
        })?;

        let mut adopt_mappings: Vec<(DocumentId, HashMap<NodeId, NodeId>)> = Vec::new();
        if !adopt_roots.is_empty() {
          adopt_mappings.reserve(adopt_roots.len());
          for handle in adopt_roots.iter().copied() {
            let root_id = handle.node_id;
            let mapping: HashMap<NodeId, NodeId> = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| {
                let mut mapping: HashMap<NodeId, NodeId> = HashMap::new();
                let mut stack: Vec<NodeId> = vec![root_id];
                let mut remaining = dom.nodes_len() + 1;
                while let Some(id) = stack.pop() {
                  if remaining == 0 {
                    break;
                  }
                  remaining -= 1;

                  if id.index() >= dom.nodes_len() {
                    continue;
                  }
                  mapping.insert(id, id);
                  let n = dom.node(id);
                  for &child in n.children.iter().rev() {
                    if child.index() >= dom.nodes_len() {
                      continue;
                    }
                    if dom.node(child).parent != Some(id) {
                      continue;
                    }
                    stack.push(child);
                  }
                }
                mapping
              }))
            })?;
            adopt_mappings.push((handle.document_id, mapping));
          }
        }

        let result: Result<Option<NodeId>, DomError> = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            let old_parent = match dom.parent(child_id) {
              Ok(v) => v,
              Err(err) => return (Err(err), false),
            };
            if reference.is_some_and(|reference| {
              reference.index() < dom.nodes_len()
                && matches!(dom.node(reference).kind, NodeKind::ShadowRoot { .. })
            }) {
              return (Err(DomError::NotFoundError), false);
            }

            let res = if child_id.index() < dom.nodes_len()
              && matches!(dom.node(child_id).kind, NodeKind::ShadowRoot { .. })
            {
              dom.with_shadow_root_as_document_fragment(child_id, |dom| {
                dom.insert_before(parent_id, child_id, reference)
              })
            } else {
              dom.insert_before(parent_id, child_id, reference)
            };
            match res {
              Ok(changed) => (Ok(old_parent), changed),
              Err(err) => (Err(err), false),
            }
          }))
        })?;
        match result {
          Ok(old_parent_id) => {
            for (old_document_id, mapping) in adopt_mappings {
              require_dom_platform_mut(vm)?.remap_node_ids_between_documents(
                scope.heap_mut(),
                old_document_id,
                document_id,
                &mapping,
              )?;
            }

            self.sync_cached_child_nodes_for_wrapper(
              vm,
              scope,
              parent_wrapper_obj,
              parent_id,
              document_id,
            )?;
            if child_is_fragment_like {
              self.sync_cached_child_nodes_for_wrapper(
                vm,
                scope,
                child_wrapper_obj,
                child_id,
                child_document_id,
              )?;
            }
            if let Some(old_parent_id) = old_parent_id {
              if !(child_document_id == document_id && old_parent_id == parent_id) {
                let old_parent_wrapper = {
                  let platform = require_dom_platform_mut(vm)?;
                  platform.get_existing_wrapper_for_document_id(scope.heap(), child_document_id, old_parent_id)
                };
                if let Some(old_parent_wrapper) = old_parent_wrapper {
                  self.sync_cached_child_nodes_for_wrapper(
                    vm,
                    scope,
                    old_parent_wrapper,
                    old_parent_id,
                    child_document_id,
                  )?;
                }
              }
            }

            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Object(child_wrapper_obj))
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }
      ("Node", "removeChild", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let Value::Object(parent_wrapper_obj) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let platform = require_dom_platform_mut(vm)?;
        let parent_handle =
          platform.require_node_handle(scope.heap(), Value::Object(parent_wrapper_obj))?;
        let parent_id = parent_handle.node_id;
        let document_id = parent_handle.document_id;
        let child_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let Value::Object(child_wrapper_obj) = child_value else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let child_id = platform.require_node_id(scope.heap(), child_value)?;

        let result: Result<(), DomError> = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            // ShadowRoot is never a tree child in the DOM Standard, so it cannot be removed (even
            // though dom2 stores it as a child of its host element).
            if child_id.index() < dom.nodes_len()
              && matches!(dom.node(child_id).kind, NodeKind::ShadowRoot { .. })
            {
              return (Err(DomError::NotFoundError), false);
            }
            match dom.remove_child(parent_id, child_id) {
              Ok(changed) => (Ok(()), changed),
              Err(err) => (Err(err), false),
            }
          }))
        })?;
        match result {
          Ok(()) => {
            self.sync_cached_child_nodes_for_wrapper(
              vm,
              scope,
              parent_wrapper_obj,
              parent_id,
              document_id,
            )?;
            self.sync_live_html_collections(vm, scope)?;
            // Per DOM, `removeChild` returns the removed child (preserving object identity).
            Ok(Value::Object(child_wrapper_obj))
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }
      ("Node", "replaceChild", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let Value::Object(parent_wrapper_obj) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let platform = require_dom_platform_mut(vm)?;
        let parent_handle =
          platform.require_node_handle(scope.heap(), Value::Object(parent_wrapper_obj))?;
        let parent_id = parent_handle.node_id;
        let document_id = parent_handle.document_id;
        let new_child_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let Value::Object(new_child_wrapper_obj) = new_child_value else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let new_child_handle = platform.require_node_handle(scope.heap(), new_child_value)?;
        let new_child_id = new_child_handle.node_id;
        let new_child_document_id = new_child_handle.document_id;
        let old_child_value = args.get(1).copied().unwrap_or(Value::Undefined);
        let Value::Object(old_child_wrapper_obj) = old_child_value else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let old_child_id = platform.require_node_id(scope.heap(), old_child_value)?;

        let (new_child_is_fragment_like, adopt_roots): (bool, Vec<DomNodeKey>) =
          self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              if new_child_id.index() >= dom.nodes_len() {
                return (false, Vec::new());
              }
              let kind = &dom.node(new_child_id).kind;
              let new_child_is_fragment_like =
                matches!(kind, NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. });
              let mut adopt_roots: Vec<DomNodeKey> = Vec::new();
              if new_child_document_id != document_id {
                match kind {
                  NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => {
                    for &child in dom.node(new_child_id).children.iter() {
                      if child.index() >= dom.nodes_len() {
                        continue;
                      }
                      if dom.node(child).parent != Some(new_child_id) {
                        continue;
                      }
                      adopt_roots.push(DomNodeKey::new(new_child_document_id, child));
                    }
                  }
                  NodeKind::Document { .. } => {}
                  _ => adopt_roots.push(new_child_handle),
                }
              }
              (new_child_is_fragment_like, adopt_roots)
            }))
          })?;

        let mut adopt_mappings: Vec<(DocumentId, HashMap<NodeId, NodeId>)> = Vec::new();
        if !adopt_roots.is_empty() {
          adopt_mappings.reserve(adopt_roots.len());
          for handle in adopt_roots.iter().copied() {
            let root_id = handle.node_id;
            let mapping: HashMap<NodeId, NodeId> = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| {
                let mut mapping: HashMap<NodeId, NodeId> = HashMap::new();
                let mut stack: Vec<NodeId> = vec![root_id];
                let mut remaining = dom.nodes_len() + 1;
                while let Some(id) = stack.pop() {
                  if remaining == 0 {
                    break;
                  }
                  remaining -= 1;

                  if id.index() >= dom.nodes_len() {
                    continue;
                  }
                  mapping.insert(id, id);
                  let n = dom.node(id);
                  for &child in n.children.iter().rev() {
                    if child.index() >= dom.nodes_len() {
                      continue;
                    }
                    if dom.node(child).parent != Some(id) {
                      continue;
                    }
                    stack.push(child);
                  }
                }
                mapping
              }))
            })?;
            adopt_mappings.push((handle.document_id, mapping));
          }
        }

        let result: Result<Option<NodeId>, DomError> = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            let old_parent = match dom.parent(new_child_id) {
              Ok(v) => v,
              Err(err) => return (Err(err), false),
            };
            // ShadowRoot is never a tree child in the DOM Standard, so it cannot be replaced (even
            // though dom2 stores it as a child of its host element).
            if old_child_id.index() < dom.nodes_len()
              && matches!(dom.node(old_child_id).kind, NodeKind::ShadowRoot { .. })
            {
              return (Err(DomError::NotFoundError), false);
            }

            let new_child_is_fragment = new_child_id.index() < dom.nodes_len()
              && matches!(
                dom.node(new_child_id).kind,
                NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. }
              );
            let old_parent = if new_child_is_fragment {
              None
            } else {
              match dom.parent(new_child_id) {
                Ok(v) => v,
                Err(err) => return (Err(err), false),
              }
            };

            let res = if new_child_id.index() < dom.nodes_len()
              && matches!(dom.node(new_child_id).kind, NodeKind::ShadowRoot { .. })
            {
              dom.with_shadow_root_as_document_fragment(new_child_id, |dom| {
                dom.replace_child(parent_id, new_child_id, old_child_id)
              })
            } else {
              dom.replace_child(parent_id, new_child_id, old_child_id)
            };
            match res {
              Ok(changed) => (Ok(old_parent), changed),
              Err(err) => (Err(err), false),
            }
          }))
        })?;
        match result {
          Ok(old_parent_id) => {
            for (old_document_id, mapping) in adopt_mappings {
              require_dom_platform_mut(vm)?.remap_node_ids_between_documents(
                scope.heap_mut(),
                old_document_id,
                document_id,
                &mapping,
              )?;
            }

            self.sync_cached_child_nodes_for_wrapper(
              vm,
              scope,
              parent_wrapper_obj,
              parent_id,
              document_id,
            )?;
            if new_child_is_fragment_like {
              self.sync_cached_child_nodes_for_wrapper(
                vm,
                scope,
                new_child_wrapper_obj,
                new_child_id,
                new_child_document_id,
              )?;
            }
            if let Some(old_parent_id) = old_parent_id {
              if !(new_child_document_id == document_id && old_parent_id == parent_id) {
                let old_parent_wrapper = {
                  let platform = require_dom_platform_mut(vm)?;
                  platform.get_existing_wrapper_for_document_id(
                    scope.heap(),
                    new_child_document_id,
                    old_parent_id,
                  )
                };
                if let Some(old_parent_wrapper) = old_parent_wrapper {
                  self.sync_cached_child_nodes_for_wrapper(
                    vm,
                    scope,
                    old_parent_wrapper,
                    old_parent_id,
                    new_child_document_id,
                  )?;
                }
              }
            }

            self.sync_live_html_collections(vm, scope)?;
            // `replaceChild` returns the replaced node (the old child), preserving object identity.
            Ok(Value::Object(old_child_wrapper_obj))
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }
      ("Node", "cloneNode", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let handle = require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), receiver)?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;
        let deep = args.get(0).copied().unwrap_or(Value::Bool(false));
        let deep = scope.heap().to_boolean(deep)?;

        let result: Result<NodeId, DomError> = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| match dom.clone_node(node_id, deep) {
            Ok(cloned) => (Ok(cloned), false),
            Err(err) => (Err(err), false),
          }))
        })?;
        match result {
          Ok(cloned_id) => {
            let primary = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| {
                if cloned_id.index() >= dom.nodes_len() {
                  DomInterface::Node
                } else {
                  DomInterface::primary_for_node_kind(&dom.node(cloned_id).kind)
                }
              }))
            })?;
            let wrapper =
              require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(scope, document_id, cloned_id, primary)?;
            scope.push_root(Value::Object(wrapper))?;
            Ok(Value::Object(wrapper))
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }
      ("Node", "remove", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let Value::Object(wrapper_obj) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let handle =
          require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), Value::Object(wrapper_obj))?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;

        let result: Result<Option<NodeId>, DomError> = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            if node_id.index() < dom.nodes_len()
              && matches!(dom.node(node_id).kind, NodeKind::ShadowRoot { .. })
            {
              // ShadowRoot is not a tree child per the DOM Standard. It must not be removable via
              // `Node.remove()` even though `dom2` stores it as a child of its host element.
              return (Ok(None), false);
            }
            let parent = match dom.parent(node_id) {
              Ok(Some(p)) => p,
              Ok(None) => return (Ok(None), false),
              Err(err) => return (Err(err), false),
            };
            match dom.remove_child(parent, node_id) {
              Ok(changed) => (Ok(Some(parent)), changed),
              Err(err) => (Err(err), false),
            }
          }))
        })?;
        match result {
          Ok(Some(parent_id)) => {
            let parent_wrapper = {
              let platform = require_dom_platform_mut(vm)?;
              platform.get_existing_wrapper_for_document_id(scope.heap(), document_id, parent_id)
            };
            if let Some(parent_wrapper) = parent_wrapper {
              self.sync_cached_child_nodes_for_wrapper(
                vm,
                scope,
                parent_wrapper,
                parent_id,
                document_id,
              )?;
            }
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Ok(None) => Ok(Value::Undefined),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("NodeIterator", "detach", 0) => {
        let _ = args;
        let _ = require_node_iterator_receiver(scope, receiver)?;
        Ok(Value::Undefined)
      }
      ("NodeIterator", "root", 0) => {
        let _ = args;
        let (iter_id, iter_obj) = require_node_iterator_receiver(scope, receiver)?;

        let wrapper_doc_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let Some(Value::Object(document_obj)) =
          scope.heap().object_get_own_data_property_value(iter_obj, &wrapper_doc_key)?
        else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let root_id = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| dom.node_iterator_root(iter_id)))
        })?;
        let Some(root_id) = root_id else {
          return Err(VmError::TypeError("Illegal invocation"));
        };

        let primary = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if root_id.index() >= dom.nodes_len() {
              DomInterface::Node
            } else {
              DomInterface::primary_for_node_kind(&dom.node(root_id).kind)
            }
          }))
        })?;
        let wrapper = require_dom_platform_mut(vm)?
          .get_or_create_wrapper_for_document_id(scope, document_id, root_id, primary)?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("NodeIterator", "referenceNode", 0) => {
        let _ = args;
        let (iter_id, iter_obj) = require_node_iterator_receiver(scope, receiver)?;

        let wrapper_doc_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let Some(Value::Object(document_obj)) =
          scope.heap().object_get_own_data_property_value(iter_obj, &wrapper_doc_key)?
        else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let reference_id = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| dom.node_iterator_reference(iter_id)))
        })?;
        let Some(reference_id) = reference_id else {
          return Err(VmError::TypeError("Illegal invocation"));
        };

        let primary = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if reference_id.index() >= dom.nodes_len() {
              DomInterface::Node
            } else {
              DomInterface::primary_for_node_kind(&dom.node(reference_id).kind)
            }
          }))
        })?;
        let wrapper = require_dom_platform_mut(vm)?
          .get_or_create_wrapper_for_document_id(scope, document_id, reference_id, primary)?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("NodeIterator", "pointerBeforeReferenceNode", 0) => {
        let _ = args;
        let (iter_id, _iter_obj) = require_node_iterator_receiver(scope, receiver)?;
        let before = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| dom.node_iterator_pointer_before_reference(iter_id)))
        })?;
        let Some(before) = before else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        Ok(Value::Bool(before))
      }
      ("NodeIterator", "whatToShow", 0) => {
        let _ = args;
        let (_iter_id, iter_obj) = require_node_iterator_receiver(scope, receiver)?;
        let key = key_from_str(scope, TRAVERSAL_WHAT_TO_SHOW_SLOT)?;
        Ok(scope
          .heap()
          .object_get_own_data_property_value(iter_obj, &key)?
          .unwrap_or(Value::Number(0.0)))
      }
      ("NodeIterator", "filter", 0) => {
        let _ = args;
        let (_iter_id, iter_obj) = require_node_iterator_receiver(scope, receiver)?;
        let key = key_from_str(scope, TRAVERSAL_FILTER_SLOT)?;
        Ok(scope
          .heap()
          .object_get_own_data_property_value(iter_obj, &key)?
          .unwrap_or(Value::Null))
      }
      ("NodeIterator", "nextNode", 0) => {
        let _ = args;
        let (iter_id, iter_obj) = require_node_iterator_receiver(scope, receiver)?;
        let dom_exception = self.dom_exception_class_for_realm(vm, scope)?;

        let what_key = key_from_str(scope, TRAVERSAL_WHAT_TO_SHOW_SLOT)?;
        let filter_key = key_from_str(scope, TRAVERSAL_FILTER_SLOT)?;
        let active_key = key_from_str(scope, TRAVERSAL_ACTIVE_SLOT)?;

        let wrapper_doc_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let Some(Value::Object(document_obj)) =
          scope.heap().object_get_own_data_property_value(iter_obj, &wrapper_doc_key)?
        else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let state = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            let root = dom.node_iterator_root(iter_id)?;
            let reference = dom.node_iterator_reference(iter_id)?;
            let before = dom.node_iterator_pointer_before_reference(iter_id)?;
            Some((root, reference, before))
          }))
        })?;
        let Some((root, mut node, mut before_node)) = state else {
          return Err(VmError::TypeError("Illegal invocation"));
        };

        loop {
          if !before_node {
            let next = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| tree_following_in_subtree(dom, root, node)))
            })?;
            let Some(next) = next else {
              return Ok(Value::Null);
            };
            node = next;
          } else {
            before_node = false;
          }

          let result = traversal_filter_node::<Host>(
            self,
            vm,
            scope,
            dom_exception,
            iter_obj,
            node,
            document_id,
            what_key,
            filter_key,
            active_key,
          )?;
          if result == NODE_FILTER_ACCEPT {
            break;
          }
        }

        // Persist the new iterator state.
        self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            dom.set_node_iterator_reference_and_pointer(iter_id, node, before_node);
            ((), false)
          }))
        })?;

        let primary = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if node.index() >= dom.nodes_len() {
              DomInterface::Node
            } else {
              DomInterface::primary_for_node_kind(&dom.node(node).kind)
            }
          }))
        })?;
        let wrapper =
          require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(scope, document_id, node, primary)?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("NodeIterator", "previousNode", 0) => {
        let _ = args;
        let (iter_id, iter_obj) = require_node_iterator_receiver(scope, receiver)?;
        let dom_exception = self.dom_exception_class_for_realm(vm, scope)?;

        let what_key = key_from_str(scope, TRAVERSAL_WHAT_TO_SHOW_SLOT)?;
        let filter_key = key_from_str(scope, TRAVERSAL_FILTER_SLOT)?;
        let active_key = key_from_str(scope, TRAVERSAL_ACTIVE_SLOT)?;

        let wrapper_doc_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let Some(Value::Object(document_obj)) =
          scope.heap().object_get_own_data_property_value(iter_obj, &wrapper_doc_key)?
        else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let state = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            let root = dom.node_iterator_root(iter_id)?;
            let reference = dom.node_iterator_reference(iter_id)?;
            let before = dom.node_iterator_pointer_before_reference(iter_id)?;
            Some((root, reference, before))
          }))
        })?;
        let Some((root, mut node, mut before_node)) = state else {
          return Err(VmError::TypeError("Illegal invocation"));
        };

        loop {
          if before_node {
            let prev = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| tree_preceding_in_subtree(dom, root, node)))
            })?;
            let Some(prev) = prev else {
              return Ok(Value::Null);
            };
            node = prev;
          } else {
            before_node = true;
          }

          let result = traversal_filter_node::<Host>(
            self,
            vm,
            scope,
            dom_exception,
            iter_obj,
            node,
            document_id,
            what_key,
            filter_key,
            active_key,
          )?;
          if result == NODE_FILTER_ACCEPT {
            break;
          }
        }

        // Persist the new iterator state.
        self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            dom.set_node_iterator_reference_and_pointer(iter_id, node, before_node);
            ((), false)
          }))
        })?;

        let primary = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if node.index() >= dom.nodes_len() {
              DomInterface::Node
            } else {
              DomInterface::primary_for_node_kind(&dom.node(node).kind)
            }
          }))
        })?;
        let wrapper =
          require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(scope, document_id, node, primary)?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }

      ("TreeWalker", "root", 0) => {
        let _ = args;
        let walker_obj = require_tree_walker_receiver(scope, receiver)?;

        let wrapper_doc_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let Some(Value::Object(document_obj)) =
          scope.heap().object_get_own_data_property_value(walker_obj, &wrapper_doc_key)?
        else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let root_key = key_from_str(scope, TREE_WALKER_ROOT_SLOT)?;
        let root_id = read_internal_node_id_slot(scope, walker_obj, &root_key)?;
        let primary = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if root_id.index() >= dom.nodes_len() {
              DomInterface::Node
            } else {
              DomInterface::primary_for_node_kind(&dom.node(root_id).kind)
            }
          }))
        })?;
        let wrapper = require_dom_platform_mut(vm)?
          .get_or_create_wrapper_for_document_id(scope, document_id, root_id, primary)?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("TreeWalker", "whatToShow", 0) => {
        let _ = args;
        let walker_obj = require_tree_walker_receiver(scope, receiver)?;
        let key = key_from_str(scope, TRAVERSAL_WHAT_TO_SHOW_SLOT)?;
        Ok(scope
          .heap()
          .object_get_own_data_property_value(walker_obj, &key)?
          .unwrap_or(Value::Number(0.0)))
      }
      ("TreeWalker", "filter", 0) => {
        let _ = args;
        let walker_obj = require_tree_walker_receiver(scope, receiver)?;
        let key = key_from_str(scope, TRAVERSAL_FILTER_SLOT)?;
        Ok(scope
          .heap()
          .object_get_own_data_property_value(walker_obj, &key)?
          .unwrap_or(Value::Null))
      }
      ("TreeWalker", "currentNode", 0) => {
        let walker_obj = require_tree_walker_receiver(scope, receiver)?;
        let current_key = key_from_str(scope, TREE_WALKER_CURRENT_SLOT)?;
        if args.is_empty() {
          let wrapper_doc_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
          let Some(Value::Object(document_obj)) =
            scope.heap().object_get_own_data_property_value(walker_obj, &wrapper_doc_key)?
          else {
            return Err(VmError::TypeError("Illegal invocation"));
          };
          let document_id = {
            let platform = require_dom_platform_mut(vm)?;
            platform
              .require_document_handle(scope.heap(), Value::Object(document_obj))?
              .document_id
          };

          let current_id = read_internal_node_id_slot(scope, walker_obj, &current_key)?;
          let primary = self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              if current_id.index() >= dom.nodes_len() {
                DomInterface::Node
              } else {
                DomInterface::primary_for_node_kind(&dom.node(current_id).kind)
              }
            }))
          })?;
          let wrapper = require_dom_platform_mut(vm)?
            .get_or_create_wrapper_for_document_id(scope, document_id, current_id, primary)?;
          scope.push_root(Value::Object(wrapper))?;
          Ok(Value::Object(wrapper))
        } else {
          let value = args.get(0).copied().unwrap_or(Value::Undefined);
          let node_id = {
            let platform = require_dom_platform_mut(vm)?;
            platform
              .require_node_handle(scope.heap(), value)?
              .node_id
          };
          scope.define_property(
            walker_obj,
            current_key,
            data_property(Value::Number(node_id.index() as f64), true, false, false),
          )?;
          Ok(Value::Undefined)
        }
      }

      ("TreeWalker", "parentNode", 0) => {
        let _ = args;
        let dom_exception = self.dom_exception_class_for_realm(vm, scope)?;
        let walker_obj = require_tree_walker_receiver(scope, receiver)?;

        let root_key = key_from_str(scope, TREE_WALKER_ROOT_SLOT)?;
        let current_key = key_from_str(scope, TREE_WALKER_CURRENT_SLOT)?;
        let what_key = key_from_str(scope, TRAVERSAL_WHAT_TO_SHOW_SLOT)?;
        let filter_key = key_from_str(scope, TRAVERSAL_FILTER_SLOT)?;
        let active_key = key_from_str(scope, TRAVERSAL_ACTIVE_SLOT)?;

        let wrapper_doc_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let Some(Value::Object(document_obj)) =
          scope.heap().object_get_own_data_property_value(walker_obj, &wrapper_doc_key)?
        else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let root_id = read_internal_node_id_slot(scope, walker_obj, &root_key)?;
        let mut node = read_internal_node_id_slot(scope, walker_obj, &current_key)?;

        while node != root_id {
          let parent = self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| tree_parent_node(dom, node)))
          })?;
          let Some(parent) = parent else {
            break;
          };
          node = parent;

          let result = traversal_filter_node::<Host>(
            self,
            vm,
            scope,
            dom_exception,
            walker_obj,
            node,
            document_id,
            what_key,
            filter_key,
            active_key,
          )?;
          if result == NODE_FILTER_ACCEPT {
            scope.define_property(
              walker_obj,
              current_key,
              data_property(Value::Number(node.index() as f64), true, false, false),
            )?;
            let primary = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| {
                if node.index() >= dom.nodes_len() {
                  DomInterface::Node
                } else {
                  DomInterface::primary_for_node_kind(&dom.node(node).kind)
                }
              }))
            })?;
            let wrapper = require_dom_platform_mut(vm)?
              .get_or_create_wrapper_for_document_id(scope, document_id, node, primary)?;
            scope.push_root(Value::Object(wrapper))?;
            return Ok(Value::Object(wrapper));
          }
        }

        Ok(Value::Null)
      }

      ("TreeWalker", "firstChild", 0) | ("TreeWalker", "lastChild", 0) => {
        let _ = args;
        let dom_exception = self.dom_exception_class_for_realm(vm, scope)?;
        let walker_obj = require_tree_walker_receiver(scope, receiver)?;
        let is_first = operation == "firstChild";

        let root_key = key_from_str(scope, TREE_WALKER_ROOT_SLOT)?;
        let current_key = key_from_str(scope, TREE_WALKER_CURRENT_SLOT)?;
        let what_key = key_from_str(scope, TRAVERSAL_WHAT_TO_SHOW_SLOT)?;
        let filter_key = key_from_str(scope, TRAVERSAL_FILTER_SLOT)?;
        let active_key = key_from_str(scope, TRAVERSAL_ACTIVE_SLOT)?;

        let wrapper_doc_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let Some(Value::Object(document_obj)) =
          scope.heap().object_get_own_data_property_value(walker_obj, &wrapper_doc_key)?
        else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let root_id = read_internal_node_id_slot(scope, walker_obj, &root_key)?;
        let start_current = read_internal_node_id_slot(scope, walker_obj, &current_key)?;

        let mut node = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            if is_first {
              tree_first_child(dom, start_current)
            } else {
              tree_last_child(dom, start_current)
            }
          }))
        })?;

        while let Some(current) = node {
          let result = traversal_filter_node::<Host>(
            self,
            vm,
            scope,
            dom_exception,
            walker_obj,
            current,
            document_id,
            what_key,
            filter_key,
            active_key,
          )?;
          if result == NODE_FILTER_ACCEPT {
            scope.define_property(
              walker_obj,
              current_key,
              data_property(Value::Number(current.index() as f64), true, false, false),
            )?;
            let primary = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| {
                if current.index() >= dom.nodes_len() {
                  DomInterface::Node
                } else {
                  DomInterface::primary_for_node_kind(&dom.node(current).kind)
                }
              }))
            })?;
            let wrapper = require_dom_platform_mut(vm)?
              .get_or_create_wrapper_for_document_id(scope, document_id, current, primary)?;
            scope.push_root(Value::Object(wrapper))?;
            return Ok(Value::Object(wrapper));
          }

          if result == NODE_FILTER_SKIP {
            let child = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| {
                if is_first {
                  tree_first_child(dom, current)
                } else {
                  tree_last_child(dom, current)
                }
              }))
            })?;
            if child.is_some() {
              node = child;
              continue;
            }
          }

          // Walk to next sibling/ancestor sibling.
          let mut n = current;
          loop {
            let sibling = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| {
                if is_first {
                  tree_next_sibling(dom, n)
                } else {
                  tree_previous_sibling(dom, n)
                }
              }))
            })?;
            if let Some(sibling) = sibling {
              node = Some(sibling);
              break;
            }

            let parent = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| tree_parent_node(dom, n)))
            })?;
            if parent.is_none()
              || parent == Some(root_id)
              || parent == Some(start_current)
            {
              return Ok(Value::Null);
            }
            n = parent.unwrap(); // fastrender-allow-unwrap
          }
        }

        Ok(Value::Null)
      }

      ("TreeWalker", "nextSibling", 0) | ("TreeWalker", "previousSibling", 0) => {
        let _ = args;
        let dom_exception = self.dom_exception_class_for_realm(vm, scope)?;
        let walker_obj = require_tree_walker_receiver(scope, receiver)?;
        let is_next = operation == "nextSibling";

        let root_key = key_from_str(scope, TREE_WALKER_ROOT_SLOT)?;
        let current_key = key_from_str(scope, TREE_WALKER_CURRENT_SLOT)?;
        let what_key = key_from_str(scope, TRAVERSAL_WHAT_TO_SHOW_SLOT)?;
        let filter_key = key_from_str(scope, TRAVERSAL_FILTER_SLOT)?;
        let active_key = key_from_str(scope, TRAVERSAL_ACTIVE_SLOT)?;

        let wrapper_doc_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let Some(Value::Object(document_obj)) =
          scope.heap().object_get_own_data_property_value(walker_obj, &wrapper_doc_key)?
        else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let root_id = read_internal_node_id_slot(scope, walker_obj, &root_key)?;
        let mut node = read_internal_node_id_slot(scope, walker_obj, &current_key)?;
        if node == root_id {
          return Ok(Value::Null);
        }

        loop {
          let mut sibling = self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              if is_next {
                tree_next_sibling(dom, node)
              } else {
                tree_previous_sibling(dom, node)
              }
            }))
          })?;

          while let Some(sib) = sibling {
            node = sib;
            let result = traversal_filter_node::<Host>(
              self,
              vm,
              scope,
              dom_exception,
              walker_obj,
              node,
              document_id,
              what_key,
              filter_key,
              active_key,
            )?;
            if result == NODE_FILTER_ACCEPT {
              scope.define_property(
                walker_obj,
                current_key,
                data_property(Value::Number(node.index() as f64), true, false, false),
              )?;
              let primary = self.with_dom_host(vm, |host| {
                Ok(host.with_dom(|dom| {
                  if node.index() >= dom.nodes_len() {
                    DomInterface::Node
                  } else {
                    DomInterface::primary_for_node_kind(&dom.node(node).kind)
                  }
                }))
              })?;
              let wrapper = require_dom_platform_mut(vm)?
                .get_or_create_wrapper_for_document_id(scope, document_id, node, primary)?;
              scope.push_root(Value::Object(wrapper))?;
              return Ok(Value::Object(wrapper));
            }

            sibling = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| {
                if is_next {
                  tree_first_child(dom, node)
                } else {
                  tree_last_child(dom, node)
                }
              }))
            })?;

            if result == NODE_FILTER_REJECT || sibling.is_none() {
              sibling = self.with_dom_host(vm, |host| {
                Ok(host.with_dom(|dom| {
                  if is_next {
                    tree_next_sibling(dom, node)
                  } else {
                    tree_previous_sibling(dom, node)
                  }
                }))
              })?;
            }
          }

          // Ascend.
          let parent = self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| tree_parent_node(dom, node)))
          })?;
          let Some(parent) = parent else {
            return Ok(Value::Null);
          };
          node = parent;
          if node == root_id {
            return Ok(Value::Null);
          }
          if traversal_filter_node::<Host>(
            self,
            vm,
            scope,
            dom_exception,
            walker_obj,
            node,
            document_id,
            what_key,
            filter_key,
            active_key,
          )? == NODE_FILTER_ACCEPT
          {
            return Ok(Value::Null);
          }
        }
      }

      ("TreeWalker", "previousNode", 0) => {
        let _ = args;
        let dom_exception = self.dom_exception_class_for_realm(vm, scope)?;
        let walker_obj = require_tree_walker_receiver(scope, receiver)?;

        let root_key = key_from_str(scope, TREE_WALKER_ROOT_SLOT)?;
        let current_key = key_from_str(scope, TREE_WALKER_CURRENT_SLOT)?;
        let what_key = key_from_str(scope, TRAVERSAL_WHAT_TO_SHOW_SLOT)?;
        let filter_key = key_from_str(scope, TRAVERSAL_FILTER_SLOT)?;
        let active_key = key_from_str(scope, TRAVERSAL_ACTIVE_SLOT)?;

        let wrapper_doc_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let Some(Value::Object(document_obj)) =
          scope.heap().object_get_own_data_property_value(walker_obj, &wrapper_doc_key)?
        else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let root_id = read_internal_node_id_slot(scope, walker_obj, &root_key)?;
        let mut node = read_internal_node_id_slot(scope, walker_obj, &current_key)?;

        while node != root_id {
          let mut sibling = self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| tree_previous_sibling(dom, node)))
          })?;

          while let Some(sib) = sibling {
            node = sib;
            let mut result = traversal_filter_node::<Host>(
              self,
              vm,
              scope,
              dom_exception,
              walker_obj,
              node,
              document_id,
              what_key,
              filter_key,
              active_key,
            )?;

            while result != NODE_FILTER_REJECT {
              let child = self.with_dom_host(vm, |host| {
                Ok(host.with_dom(|dom| tree_last_child(dom, node)))
              })?;
              let Some(child) = child else {
                break;
              };
              node = child;
              result = traversal_filter_node::<Host>(
                self,
                vm,
                scope,
                dom_exception,
                walker_obj,
                node,
                document_id,
                what_key,
                filter_key,
                active_key,
              )?;
            }

            if result == NODE_FILTER_ACCEPT {
              scope.define_property(
                walker_obj,
                current_key,
                data_property(Value::Number(node.index() as f64), true, false, false),
              )?;
              let primary = self.with_dom_host(vm, |host| {
                Ok(host.with_dom(|dom| {
                  if node.index() >= dom.nodes_len() {
                    DomInterface::Node
                  } else {
                    DomInterface::primary_for_node_kind(&dom.node(node).kind)
                  }
                }))
              })?;
              let wrapper = require_dom_platform_mut(vm)?
                .get_or_create_wrapper_for_document_id(scope, document_id, node, primary)?;
              scope.push_root(Value::Object(wrapper))?;
              return Ok(Value::Object(wrapper));
            }

            sibling = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| tree_previous_sibling(dom, node)))
            })?;
          }

          let parent = self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| tree_parent_node(dom, node)))
          })?;
          let Some(parent) = parent else {
            return Ok(Value::Null);
          };
          if node == root_id {
            return Ok(Value::Null);
          }
          node = parent;

          let result = traversal_filter_node::<Host>(
            self,
            vm,
            scope,
            dom_exception,
            walker_obj,
            node,
            document_id,
            what_key,
            filter_key,
            active_key,
          )?;
          if result == NODE_FILTER_ACCEPT {
            scope.define_property(
              walker_obj,
              current_key,
              data_property(Value::Number(node.index() as f64), true, false, false),
            )?;
            let primary = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| {
                if node.index() >= dom.nodes_len() {
                  DomInterface::Node
                } else {
                  DomInterface::primary_for_node_kind(&dom.node(node).kind)
                }
              }))
            })?;
            let wrapper = require_dom_platform_mut(vm)?
              .get_or_create_wrapper_for_document_id(scope, document_id, node, primary)?;
            scope.push_root(Value::Object(wrapper))?;
            return Ok(Value::Object(wrapper));
          }
        }

        Ok(Value::Null)
      }

      ("TreeWalker", "nextNode", 0) => {
        let _ = args;
        let dom_exception = self.dom_exception_class_for_realm(vm, scope)?;
        let walker_obj = require_tree_walker_receiver(scope, receiver)?;

        let root_key = key_from_str(scope, TREE_WALKER_ROOT_SLOT)?;
        let current_key = key_from_str(scope, TREE_WALKER_CURRENT_SLOT)?;
        let what_key = key_from_str(scope, TRAVERSAL_WHAT_TO_SHOW_SLOT)?;
        let filter_key = key_from_str(scope, TRAVERSAL_FILTER_SLOT)?;
        let active_key = key_from_str(scope, TRAVERSAL_ACTIVE_SLOT)?;

        let wrapper_doc_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let Some(Value::Object(document_obj)) =
          scope.heap().object_get_own_data_property_value(walker_obj, &wrapper_doc_key)?
        else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let root_id = read_internal_node_id_slot(scope, walker_obj, &root_key)?;
        let mut node = read_internal_node_id_slot(scope, walker_obj, &current_key)?;
        let mut result: u16 = NODE_FILTER_ACCEPT;

        loop {
          // Descend.
          loop {
            if result == NODE_FILTER_REJECT {
              break;
            }
            let child = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| tree_first_child(dom, node)))
            })?;
            let Some(child) = child else {
              break;
            };
            node = child;
            result = traversal_filter_node::<Host>(
              self,
              vm,
              scope,
              dom_exception,
              walker_obj,
              node,
              document_id,
              what_key,
              filter_key,
              active_key,
            )?;
            if result == NODE_FILTER_ACCEPT {
              scope.define_property(
                walker_obj,
                current_key,
                data_property(Value::Number(node.index() as f64), true, false, false),
              )?;
              let primary = self.with_dom_host(vm, |host| {
                Ok(host.with_dom(|dom| {
                  if node.index() >= dom.nodes_len() {
                    DomInterface::Node
                  } else {
                    DomInterface::primary_for_node_kind(&dom.node(node).kind)
                  }
                }))
              })?;
              let wrapper = require_dom_platform_mut(vm)?
                .get_or_create_wrapper_for_document_id(scope, document_id, node, primary)?;
              scope.push_root(Value::Object(wrapper))?;
              return Ok(Value::Object(wrapper));
            }
          }

          // Find next sibling of an ancestor.
          let mut temporary = node;
          loop {
            if temporary == root_id {
              return Ok(Value::Null);
            }
            let sibling = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| tree_next_sibling(dom, temporary)))
            })?;
            if let Some(sibling) = sibling {
              node = sibling;
              break;
            }
            let parent = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| tree_parent_node(dom, temporary)))
            })?;
            let Some(parent) = parent else {
              return Ok(Value::Null);
            };
            temporary = parent;
          }

          result = traversal_filter_node::<Host>(
            self,
            vm,
            scope,
            dom_exception,
            walker_obj,
            node,
            document_id,
            what_key,
            filter_key,
            active_key,
          )?;
          if result == NODE_FILTER_ACCEPT {
            scope.define_property(
              walker_obj,
              current_key,
              data_property(Value::Number(node.index() as f64), true, false, false),
            )?;
            let primary = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| {
                if node.index() >= dom.nodes_len() {
                  DomInterface::Node
                } else {
                  DomInterface::primary_for_node_kind(&dom.node(node).kind)
                }
              }))
            })?;
            let wrapper = require_dom_platform_mut(vm)?
              .get_or_create_wrapper_for_document_id(scope, document_id, node, primary)?;
            scope.push_root(Value::Object(wrapper))?;
            return Ok(Value::Object(wrapper));
          }
        }
      }

      ("URL", "constructor", 0) => {
        let obj = Self::require_receiver_object(receiver)?;
        let input =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
        let base = match args.get(1).copied() {
          None | Some(Value::Undefined) => None,
          Some(v) => Some(js_string_to_rust_string(scope, v)?),
        };

        let url = Url::parse_without_diagnostics(&input, base.as_deref(), &self.limits)
          .map_err(url_parse_result_to_vm_error)?;
        self.urls.insert(WeakGcObject::from(obj), url);
        Ok(Value::Undefined)
      }
      ("URL", "href", 0) => {
        let url = self.require_url(receiver)?;
        if args.is_empty() {
          let href = url.href().map_err(url_parse_result_to_vm_error)?;
          let s = scope.alloc_string(&href)?;
          scope.push_root(Value::String(s))?;
          Ok(Value::String(s))
        } else {
          let value = js_string_to_rust_string(scope, args[0])?;
          url.set_href(&value).map_err(url_parse_result_to_vm_error)?;
          Ok(Value::Undefined)
        }
      }
      ("URL", "origin", 0) => {
        let url = self.require_url(receiver)?;
        let origin = url.origin();
        let s = scope.alloc_string(&origin)?;
        scope.push_root(Value::String(s))?;
        Ok(Value::String(s))
      }
      ("URL", "searchParams", 0) => {
        let url_obj = Self::require_receiver_object(receiver)?;

        // Internal cache slot used to preserve `[SameObject]` semantics for `URL.searchParams`:
        // repeated reads should return the same wrapper object for as long as the URL object is
        // alive.
        //
        // We store the wrapper as a non-enumerable, non-writable, non-configurable own data
        // property so the vm-js GC traces it naturally.
        let slot_key = key_from_str(scope, URL_SEARCH_PARAMS_SLOT)?;
        if let Some(cached) = scope
          .heap()
          .object_get_own_data_property_value(url_obj, &slot_key)?
        {
          if cached != Value::Undefined {
            let Value::Object(_) = cached else {
              return Err(VmError::TypeError(
                "URL.searchParams cache slot value is not an object",
              ));
            };
            return Ok(cached);
          }
        }

        let url = self
          .urls
          .get(&WeakGcObject::from(url_obj))
          .cloned()
          .ok_or(VmError::TypeError("Illegal invocation"))?;
        let params = url.search_params();

        let proto = self.url_search_params_proto_from_global(vm, scope)?;
        scope.push_root(Value::Object(proto))?;
        let params_obj = scope.alloc_object_with_prototype(Some(proto))?;
        scope.push_root(Value::Object(params_obj))?;
        self.params.insert(WeakGcObject::from(params_obj), params);

        // Note: allocate a fresh key for the define_property call (instead of reusing `slot_key`)
        // so we never hold a non-rooted string handle across operations that may allocate/GC.
        let slot_key = key_from_str(scope, URL_SEARCH_PARAMS_SLOT)?;
        scope.define_property(
          url_obj,
          slot_key,
          PropertyDescriptor {
            enumerable: false,
            configurable: false,
            kind: PropertyKind::Data {
              value: Value::Object(params_obj),
              writable: false,
            },
          },
        )?;

        Ok(Value::Object(params_obj))
      }
      ("URL", "toJSON", 0) => {
        let url = self.require_url(receiver)?;
        let json = url.to_json().map_err(url_parse_result_to_vm_error)?;
        let s = scope.alloc_string(&json)?;
        scope.push_root(Value::String(s))?;
        Ok(Value::String(s))
      }
      ("URL", "canParse", 0) => {
        let input =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
        let base = match args.get(1).copied() {
          None | Some(Value::Undefined) => None,
          Some(v) => Some(js_string_to_rust_string(scope, v)?),
        };
        Ok(Value::Bool(Url::can_parse(
          &input,
          base.as_deref(),
          &self.limits,
        )))
      }
      ("URL", "parse", 0) => {
        let input =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
        let base = match args.get(1).copied() {
          None | Some(Value::Undefined) => None,
          Some(v) => Some(js_string_to_rust_string(scope, v)?),
        };

        let Ok(url) = Url::parse_without_diagnostics(&input, base.as_deref(), &self.limits) else {
          return Ok(Value::Null);
        };

        let proto = self.url_proto_from_global(vm, scope)?;
        scope.push_root(Value::Object(proto))?;
        let obj = scope.alloc_object_with_prototype(Some(proto))?;
        scope.push_root(Value::Object(obj))?;
        self.urls.insert(WeakGcObject::from(obj), url);
        Ok(Value::Object(obj))
      }

      ("URLSearchParams", "constructor", 0) => {
        let obj = Self::require_receiver_object(receiver)?;
        let init = args.get(0).copied().unwrap_or(Value::Undefined);
        let params = match init {
          Value::Undefined => UrlSearchParams::new(&self.limits),
          Value::Object(init_obj) => {
            // Allow directly passing an existing URLSearchParams wrapper (outside of the generated
            // bindings constructor conversions).
            if let Some(existing) = self.params.get(&WeakGcObject::from(init_obj)).cloned() {
              existing
            } else if scope.heap().object_is_array(init_obj)? {
              // Treat arrays as the URLSearchParams "sequence of pairs" initializer.
              let params = UrlSearchParams::new(&self.limits);
              let len = array_length(vm, scope, init_obj)?;
              for idx in 0..len {
                let pair = array_get(vm, scope, init_obj, idx)?;
                let Value::Object(pair_obj) = pair else {
                  return Err(VmError::TypeError(
                    "URLSearchParams init sequence contains a non-object element",
                  ));
                };
                if !scope.heap().object_is_array(pair_obj)? {
                  return Err(VmError::TypeError(
                    "URLSearchParams init sequence contains a non-array pair",
                  ));
                }
                let pair_len = array_length(vm, scope, pair_obj)?;
                if pair_len != 2 {
                  return Err(VmError::TypeError(
                    "URLSearchParams init pair must contain exactly two elements",
                  ));
                }
                let name = array_get(vm, scope, pair_obj, 0)?;
                let value = array_get(vm, scope, pair_obj, 1)?;
                let name = js_string_to_rust_string(scope, name)?;
                let value = js_string_to_rust_string(scope, value)?;
                params
                  .append(&name, &value)
                  .map_err(url_search_params_error_to_vm_error)?;
              }
              params
            } else {
              // Treat non-array objects as the URLSearchParams "record" initializer.
              let params = UrlSearchParams::new(&self.limits);
              let keys = scope.ordinary_own_property_keys(init_obj)?;
              for key in keys {
                let PropertyKey::String(key_s) = key else {
                  continue;
                };
                let key = PropertyKey::String(key_s);
                let Some(desc) = scope.heap().object_get_own_property(init_obj, &key)? else {
                  continue;
                };
                if !desc.enumerable {
                  continue;
                }
                let name = scope.heap().get_string(key_s)?.to_utf8_lossy();
                let value = get_with_active_vm_host_and_hooks(vm, scope, init_obj, key)?;
                let value = js_string_to_rust_string(scope, value)?;
                params
                  .append(&name, &value)
                  .map_err(url_search_params_error_to_vm_error)?;
              }
              params
            }
          }
          other => {
            // This path is unlikely for generated bindings (they convert to string first), but
            // accept primitives defensively. `vm-js` heap `ToString` only supports primitives, so
            // objects still require a proper binding-side conversion.
            let s = scope.heap_mut().to_string(other)?;
            let init = scope.heap().get_string(s)?.to_utf8_lossy();
            UrlSearchParams::parse(&init, &self.limits)
              .map_err(url_search_params_error_to_vm_error)?
          }
        };
        self.params.insert(WeakGcObject::from(obj), params);
        Ok(Value::Undefined)
      }
      ("URLSearchParams", "size", 0) => {
        let params = self.require_params(receiver)?;
        let len = params.size().map_err(url_search_params_error_to_vm_error)?;
        Ok(Value::Number(len as f64))
      }
      ("URLSearchParams", "append", 0) => {
        let params = self.require_params(receiver)?;
        let name = js_string_to_rust_string(scope, args[0])?;
        let value = js_string_to_rust_string(scope, args[1])?;
        params
          .append(&name, &value)
          .map_err(url_search_params_error_to_vm_error)?;
        Ok(Value::Undefined)
      }
      ("URLSearchParams", "delete", 0) => {
        let params = self.require_params(receiver)?;
        let name = js_string_to_rust_string(scope, args[0])?;
        let value = match args.get(1).copied() {
          None | Some(Value::Undefined) => None,
          Some(v) => Some(js_string_to_rust_string(scope, v)?),
        };
        params
          .delete(&name, value.as_deref())
          .map_err(url_search_params_error_to_vm_error)?;
        Ok(Value::Undefined)
      }
      ("URLSearchParams", "get", 0) => {
        let params = self.require_params(receiver)?;
        let name = js_string_to_rust_string(scope, args[0])?;
        let result = params
          .get(&name)
          .map_err(url_search_params_error_to_vm_error)?;
        match result {
          Some(s) => {
            let js = scope.alloc_string(&s)?;
            scope.push_root(Value::String(js))?;
            Ok(Value::String(js))
          }
          None => Ok(Value::Null),
        }
      }
      ("URLSearchParams", "getAll", 0) => {
        let params = self.require_params(receiver)?;
        let name = js_string_to_rust_string(scope, args[0])?;
        let values = params
          .get_all(&name)
          .map_err(url_search_params_error_to_vm_error)?;

        let intr = vm
          .intrinsics()
          .ok_or(VmError::InvariantViolation("missing intrinsics"))?;

        let arr = scope.alloc_array(values.len())?;
        scope.push_root(Value::Object(arr))?;
        scope
          .heap_mut()
          .object_set_prototype(arr, Some(intr.array_prototype()))?;

        for (idx, item) in values.iter().enumerate() {
          let idx_key = key_from_str(scope, &idx.to_string())?;
          let s = scope.alloc_string(item)?;
          scope.push_root(Value::String(s))?;
          scope.define_property(
            arr,
            idx_key,
            data_property(Value::String(s), true, true, true),
          )?;
        }

        Ok(Value::Object(arr))
      }
      ("URLSearchParams", "entries", 0)
      | ("URLSearchParams", "keys", 0)
      | ("URLSearchParams", "values", 0) => {
        let params_obj = Self::require_receiver_object(receiver)?;
        let params = self
          .params
          .get(&WeakGcObject::from(params_obj))
          .cloned()
          .ok_or(VmError::TypeError("Illegal invocation"))?;
        let pairs = params
          .pairs()
          .map_err(url_search_params_error_to_vm_error)?;

        let intr = vm
          .intrinsics()
          .ok_or(VmError::InvariantViolation("missing intrinsics"))?;

        let values_arr = scope.alloc_array(pairs.len())?;
        scope.push_root(Value::Object(values_arr))?;
        scope
          .heap_mut()
          .object_set_prototype(values_arr, Some(intr.array_prototype()))?;

        match url_search_params_iterator_kind(operation)? {
          UrlSearchParamsIteratorKind::Entries => {
            for (idx, (name, value)) in pairs.iter().enumerate() {
              let entry = scope.alloc_array(2)?;
              scope.push_root(Value::Object(entry))?;
              scope
                .heap_mut()
                .object_set_prototype(entry, Some(intr.array_prototype()))?;

              let name_s = scope.alloc_string(name)?;
              scope.push_root(Value::String(name_s))?;
              let value_s = scope.alloc_string(value)?;
              scope.push_root(Value::String(value_s))?;

              let k0 = key_from_str(scope, "0")?;
              let k1 = key_from_str(scope, "1")?;
              scope.define_property(
                entry,
                k0,
                data_property(Value::String(name_s), true, true, true),
              )?;
              scope.define_property(
                entry,
                k1,
                data_property(Value::String(value_s), true, true, true),
              )?;

              let idx_key = key_from_str(scope, &idx.to_string())?;
              scope.define_property(
                values_arr,
                idx_key,
                data_property(Value::Object(entry), true, true, true),
              )?;
            }
          }
          UrlSearchParamsIteratorKind::Keys => {
            for (idx, (name, _value)) in pairs.iter().enumerate() {
              let s = scope.alloc_string(name)?;
              scope.push_root(Value::String(s))?;
              let idx_key = key_from_str(scope, &idx.to_string())?;
              scope.define_property(
                values_arr,
                idx_key,
                data_property(Value::String(s), true, true, true),
              )?;
            }
          }
          UrlSearchParamsIteratorKind::Values => {
            for (idx, (_name, value)) in pairs.iter().enumerate() {
              let s = scope.alloc_string(value)?;
              scope.push_root(Value::String(s))?;
              let idx_key = key_from_str(scope, &idx.to_string())?;
              scope.define_property(
                values_arr,
                idx_key,
                data_property(Value::String(s), true, true, true),
              )?;
            }
          }
        }

        let iter_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
        scope.push_root(Value::Object(iter_obj))?;

        let values_key = key_from_str(scope, URLSP_ITER_VALUES_SLOT)?;
        scope.define_property(
          iter_obj,
          values_key,
          data_property(Value::Object(values_arr), true, false, true),
        )?;
        let index_key = key_from_str(scope, URLSP_ITER_INDEX_SLOT)?;
        scope.define_property(
          iter_obj,
          index_key,
          data_property(Value::Number(0.0), true, false, true),
        )?;
        let len_key = key_from_str(scope, URLSP_ITER_LEN_SLOT)?;
        scope.define_property(
          iter_obj,
          len_key,
          data_property(Value::Number(pairs.len() as f64), true, false, true),
        )?;

        let next_id = self.urlsp_iterator_next_call_id(vm)?;
        let next_name = scope.alloc_string("next")?;
        scope.push_root(Value::String(next_name))?;
        let next_func = scope.alloc_native_function(next_id, None, next_name, 0)?;
        scope
          .heap_mut()
          .object_set_prototype(next_func, Some(intr.function_prototype()))?;
        scope.push_root(Value::Object(next_func))?;
        let next_key = key_from_str(scope, "next")?;
        scope.define_property(
          iter_obj,
          next_key,
          data_property(Value::Object(next_func), true, false, true),
        )?;

        // Make the iterator object itself iterable.
        let iter_id = self.urlsp_iterator_iterator_call_id(vm)?;
        let iter_name = scope.alloc_string("[Symbol.iterator]")?;
        scope.push_root(Value::String(iter_name))?;
        let iter_func = scope.alloc_native_function(iter_id, None, iter_name, 0)?;
        scope
          .heap_mut()
          .object_set_prototype(iter_func, Some(intr.function_prototype()))?;
        scope.push_root(Value::Object(iter_func))?;
        let sym = intr.well_known_symbols().iterator;
        scope.define_property(
          iter_obj,
          PropertyKey::from_symbol(sym),
          data_property(Value::Object(iter_func), true, false, true),
        )?;

        Ok(Value::Object(iter_obj))
      }
      ("URLSearchParams", "forEach", 0) => {
        let params_obj = Self::require_receiver_object(receiver)?;
        let params = self
          .params
          .get(&WeakGcObject::from(params_obj))
          .cloned()
          .ok_or(VmError::TypeError("Illegal invocation"))?;

        let callback = args.get(0).copied().unwrap_or(Value::Undefined);
        if !is_callable(scope, callback) {
          return Err(VmError::TypeError(
            "URLSearchParams.forEach callback is not callable",
          ));
        }
        let this_arg = args.get(1).copied().unwrap_or(Value::Undefined);

        let pairs = params
          .pairs()
          .map_err(url_search_params_error_to_vm_error)?;

        for (name, value) in pairs {
          let value_s = scope.alloc_string(&value)?;
          scope.push_root(Value::String(value_s))?;
          let name_s = scope.alloc_string(&name)?;
          scope.push_root(Value::String(name_s))?;
          let _ = vm.call_without_host(
            scope,
            callback,
            this_arg,
            &[
              Value::String(value_s),
              Value::String(name_s),
              Value::Object(params_obj),
            ],
          )?;
        }

        Ok(Value::Undefined)
      }
      ("URLSearchParams", "has", 0) => {
        let params = self.require_params(receiver)?;
        let name = js_string_to_rust_string(scope, args[0])?;
        let value = match args.get(1).copied() {
          None | Some(Value::Undefined) => None,
          Some(v) => Some(js_string_to_rust_string(scope, v)?),
        };
        let result = params
          .has(&name, value.as_deref())
          .map_err(url_search_params_error_to_vm_error)?;
        Ok(Value::Bool(result))
      }
      ("URLSearchParams", "set", 0) => {
        let params = self.require_params(receiver)?;
        let name = js_string_to_rust_string(scope, args[0])?;
        let value = js_string_to_rust_string(scope, args[1])?;
        params
          .set(&name, &value)
          .map_err(url_search_params_error_to_vm_error)?;
        Ok(Value::Undefined)
      }

      // Global attribute getter (receiver is `None` for global interfaces in vm-js bindings).
      ("Window", "document", 0) => {
        let _ = receiver;
        let _ = args;

        let Some(data) = vm.user_data_mut::<crate::js::window_realm::WindowRealmUserData>() else {
          return Err(VmError::TypeError(
            "WebIDL Window.document called without a document object",
          ));
        };
        let Some(document_obj) = data.document_obj() else {
          return Err(VmError::TypeError(
            "WebIDL Window.document called without a document object",
          ));
        };
        Ok(Value::Object(document_obj))
      }

      ("Document", "createElement", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;
        let document_key = WeakGcObject::from(document_obj);

        {
          let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
          platform.maybe_register_document_alias_wrapper(scope, document_obj)?;
          let _ = platform.require_document_id(scope.heap(), Value::Object(document_obj))?;
        }

        let local_name =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
        if !is_valid_create_element_local_name(&local_name) {
          let global = self
            .global
            .ok_or(VmError::InvariantViolation("DOMException requires a global object"))?;
          let class = dom_exception_class(vm, scope, global)?;
          return Err(throw_dom_exception(
            scope,
            class,
            "InvalidCharacterError",
            "The tag name provided is not a valid name.",
          ));
        }

        let (node_id, primary_interface) = with_active_vm_host(vm, |host| {
          mutate_dom_detached(host, |dom| {
            let node_id = dom.create_element(&local_name, HTML_NAMESPACE);
            let primary_interface = DomInterface::primary_for_node_kind(&dom.node(node_id).kind);
            (node_id, primary_interface)
          })
        })?;

        let wrapper = {
          let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
          platform.get_or_create_wrapper(scope, document_key, node_id, primary_interface)?
        };
        Ok(Value::Object(wrapper))
      }

      ("Document", "createTextNode", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;
        let document_key = WeakGcObject::from(document_obj);

        {
          let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
          platform.maybe_register_document_alias_wrapper(scope, document_obj)?;
          let _ = platform.require_document_id(scope.heap(), Value::Object(document_obj))?;
        }

        let data =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let node_id =
          with_active_vm_host(vm, |host| mutate_dom_detached(host, |dom| dom.create_text(&data)))?;

        let wrapper = {
          let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
          platform.get_or_create_wrapper(scope, document_key, node_id, DomInterface::Text)?
        };
        Ok(Value::Object(wrapper))
      }

      ("Document", "createDocumentFragment", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;
        let document_key = WeakGcObject::from(document_obj);

        {
          let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
          platform.maybe_register_document_alias_wrapper(scope, document_obj)?;
          let _ = platform.require_document_id(scope.heap(), Value::Object(document_obj))?;
        }

        let node_id = with_active_vm_host(vm, |host| {
          mutate_dom_detached(host, |dom| dom.create_document_fragment())
        })?;

        let wrapper = {
          let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
          platform.get_or_create_wrapper(
            scope,
            document_key,
            node_id,
            DomInterface::DocumentFragment,
          )?
        };
        Ok(Value::Object(wrapper))
      }

      ("Document", "createNodeIterator", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;

        // Brand check: `Document.prototype.createNodeIterator` must only be callable on a DOM-backed
        // Document wrapper.
        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        let root_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let root_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_node_handle(scope.heap(), root_value)?
            .node_id
        };

        let what_to_show = match args.get(1).copied().unwrap_or(Value::Number(0.0)) {
          Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u32::MAX as f64 => n.trunc() as u32,
          other => {
            let n = scope.heap_mut().to_number(other)?;
            if n.is_finite() && n >= 0.0 && n <= u32::MAX as f64 {
              n.trunc() as u32
            } else {
              0
            }
          }
        };

        let filter = args.get(2).copied().unwrap_or(Value::Null);

        let proto = self.node_iterator_proto_from_global(vm, scope)?;
        scope.push_root(Value::Object(proto))?;
        let iter_obj = scope.alloc_object_with_prototype(Some(proto))?;
        scope.push_root(Value::Object(iter_obj))?;

        let id = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            let id = dom.create_node_iterator(root_id);
            dom.register_node_iterator_wrapper(scope.heap(), id, iter_obj);
            (id, false)
          }))
        })?;

        scope.heap_mut().object_set_host_slots(
          iter_obj,
          HostSlots {
            a: id.as_u64(),
            b: NODE_ITERATOR_HOST_TAG,
          },
        )?;

        // Store traversal state on the wrapper as non-enumerable data properties.
        let what_key = key_from_str(scope, TRAVERSAL_WHAT_TO_SHOW_SLOT)?;
        let filter_key = key_from_str(scope, TRAVERSAL_FILTER_SLOT)?;
        let active_key = key_from_str(scope, TRAVERSAL_ACTIVE_SLOT)?;
        let wrapper_doc_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;

        scope.define_property(
          iter_obj,
          what_key,
          PropertyDescriptor {
            enumerable: false,
            configurable: false,
            kind: PropertyKind::Data {
              value: Value::Number(what_to_show as f64),
              writable: true,
            },
          },
        )?;
        scope.define_property(
          iter_obj,
          filter_key,
          PropertyDescriptor {
            enumerable: false,
            configurable: false,
            kind: PropertyKind::Data {
              value: filter,
              writable: true,
            },
          },
        )?;
        scope.define_property(
          iter_obj,
          active_key,
          PropertyDescriptor {
            enumerable: false,
            configurable: false,
            kind: PropertyKind::Data {
              value: Value::Bool(false),
              writable: true,
            },
          },
        )?;
        scope.define_property(
          iter_obj,
          wrapper_doc_key,
          PropertyDescriptor {
            enumerable: false,
            configurable: false,
            kind: PropertyKind::Data {
              value: Value::Object(document_obj),
              writable: true,
            },
          },
        )?;

        // Root return value while constructing it.
        scope.push_root(Value::Object(iter_obj))?;

        // Ensure root/reference pointers remain stable by registering the iterator against the same
        // document ID used by node wrappers.
        let _ = document_id;

        Ok(Value::Object(iter_obj))
      }

      ("Document", "createTreeWalker", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;

        // Brand check: `Document.prototype.createTreeWalker` must only be callable on a DOM-backed
        // Document wrapper.
        {
          let platform = require_dom_platform_mut(vm)?;
          let _ = platform.require_document_handle(scope.heap(), Value::Object(document_obj))?;
        }

        let root_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let root_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_node_handle(scope.heap(), root_value)?
            .node_id
        };

        let what_to_show = match args.get(1).copied().unwrap_or(Value::Number(0.0)) {
          Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u32::MAX as f64 => n.trunc() as u32,
          other => {
            let n = scope.heap_mut().to_number(other)?;
            if n.is_finite() && n >= 0.0 && n <= u32::MAX as f64 {
              n.trunc() as u32
            } else {
              0
            }
          }
        };
        let filter = args.get(2).copied().unwrap_or(Value::Null);

        let proto = self.tree_walker_proto_from_global(vm, scope)?;
        scope.push_root(Value::Object(proto))?;
        let walker_obj = scope.alloc_object_with_prototype(Some(proto))?;
        scope.push_root(Value::Object(walker_obj))?;

        scope.heap_mut().object_set_host_slots(
          walker_obj,
          HostSlots {
            a: 0,
            b: TREE_WALKER_HOST_TAG,
          },
        )?;

        // Store traversal state on the wrapper as non-enumerable data properties.
        let root_key = key_from_str(scope, TREE_WALKER_ROOT_SLOT)?;
        let current_key = key_from_str(scope, TREE_WALKER_CURRENT_SLOT)?;
        let what_key = key_from_str(scope, TRAVERSAL_WHAT_TO_SHOW_SLOT)?;
        let filter_key = key_from_str(scope, TRAVERSAL_FILTER_SLOT)?;
        let active_key = key_from_str(scope, TRAVERSAL_ACTIVE_SLOT)?;
        let wrapper_doc_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;

        scope.define_property(
          walker_obj,
          root_key,
          PropertyDescriptor {
            enumerable: false,
            configurable: false,
            kind: PropertyKind::Data {
              value: Value::Number(root_id.index() as f64),
              writable: true,
            },
          },
        )?;
        scope.define_property(
          walker_obj,
          current_key,
          PropertyDescriptor {
            enumerable: false,
            configurable: false,
            kind: PropertyKind::Data {
              value: Value::Number(root_id.index() as f64),
              writable: true,
            },
          },
        )?;
        scope.define_property(
          walker_obj,
          what_key,
          PropertyDescriptor {
            enumerable: false,
            configurable: false,
            kind: PropertyKind::Data {
              value: Value::Number(what_to_show as f64),
              writable: true,
            },
          },
        )?;
        scope.define_property(
          walker_obj,
          filter_key,
          PropertyDescriptor {
            enumerable: false,
            configurable: false,
            kind: PropertyKind::Data {
              value: filter,
              writable: true,
            },
          },
        )?;
        scope.define_property(
          walker_obj,
          active_key,
          PropertyDescriptor {
            enumerable: false,
            configurable: false,
            kind: PropertyKind::Data {
              value: Value::Bool(false),
              writable: true,
            },
          },
        )?;
        scope.define_property(
          walker_obj,
          wrapper_doc_key,
          PropertyDescriptor {
            enumerable: false,
            configurable: false,
            kind: PropertyKind::Data {
              value: Value::Object(document_obj),
              writable: true,
            },
          },
        )?;

        scope.push_root(Value::Object(walker_obj))?;
        Ok(Value::Object(walker_obj))
      },
      ("Document", "createRange", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;
        scope.push_root(Value::Object(document_obj))?;

        {
          let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
          let _ = platform.require_document_id(scope.heap(), Value::Object(document_obj))?;
        }

        // Create a JS wrapper object whose prototype is `Range.prototype`.
        let global = self
          .global
          .or_else(|| vm.user_data_mut::<WindowRealmUserData>().and_then(|data| data.window_obj()))
          .ok_or(VmError::TypeError("Illegal invocation"))?;
        scope.push_root(Value::Object(global))?;
        let ctor_key = key_from_str(scope, "Range")?;
        let Some(Value::Object(ctor_obj)) =
          scope.heap().object_get_own_data_property_value(global, &ctor_key)?
        else {
          return Err(VmError::TypeError("Range constructor not available"));
        };
        scope.push_root(Value::Object(ctor_obj))?;
        let proto_key = key_from_str(scope, "prototype")?;
        let Some(Value::Object(proto_obj)) =
          scope.heap().object_get_own_data_property_value(ctor_obj, &proto_key)?
        else {
          return Err(VmError::TypeError("Range.prototype not available"));
        };
        scope.push_root(Value::Object(proto_obj))?;

        let range_obj = scope.alloc_object_with_prototype(Some(proto_obj))?;
        scope.push_root(Value::Object(range_obj))?;

        // Keep a reference to the wrapper document for receiver branding.
        let wrapper_document_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        scope.define_property(
          range_obj,
          wrapper_document_key,
          data_property(Value::Object(document_obj), false, false, false),
        )?;

        // Allocate and register a live range in the associated dom2 document. This does not mutate the
        // live DOM tree (only internal range state), so report `changed=false`.
        //
        // Register the wrapper in the document's weak live-traversal registry so range state can be
        // swept when the JS object is collected.
        let range_id = {
          let heap = scope.heap();
          with_active_vm_host(vm, |host| {
            mutate_dom_detached(host, |dom| dom.register_live_range(heap, range_obj))
          })?
        };

        // Set wrapper host slots after registration so the weak registry can observe wrapper GC and prune
        // stale `dom2::Document` range state.
        scope.heap_mut().object_set_host_slots(
          range_obj,
          HostSlots {
            a: range_id.as_u64(),
            b: RANGE_HOST_TAG,
          },
        )?;

        Ok(Value::Object(range_obj))
      },

      ("Range", "constructor", 0) => {
        let obj = Self::require_receiver_object(receiver)?;
        scope.push_root(Value::Object(obj))?;

        let document_obj = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| data.document_obj())
          .ok_or(VmError::TypeError("Range constructor missing document object"))?;
        scope.push_root(Value::Object(document_obj))?;

        let wrapper_document_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        scope.define_property(
          obj,
          wrapper_document_key,
          data_property(Value::Object(document_obj), false, false, false),
        )?;

        let range_id = {
          let heap = scope.heap();
          with_active_vm_host(vm, |host| {
            mutate_dom_detached(host, |dom| dom.register_live_range(heap, obj))
          })?
        };
        scope.heap_mut().object_set_host_slots(
          obj,
          HostSlots {
            a: range_id.as_u64(),
            b: RANGE_HOST_TAG,
          },
        )?;

        Ok(Value::Undefined)
      },

      ("Range", "setStart", 0) => {
        let (range_id, _document_id) = require_range_receiver(vm, scope, receiver)?;

        let node_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let offset = match args.get(1).copied().unwrap_or(Value::Number(0.0)) {
          Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u32::MAX as f64 => n as u32,
          _ => 0,
        } as usize;

        let node_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), node_value)?;

        let result: Result<(), DomError> = with_active_vm_host(vm, |host| {
          mutate_dom_detached(host, |dom| dom.range_set_start(range_id, node_id, offset))
        })?;
        match result {
          Ok(()) => Ok(Value::Undefined),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      },

      ("Range", "setEnd", 0) => {
        let (range_id, _document_id) = require_range_receiver(vm, scope, receiver)?;

        let node_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let offset = match args.get(1).copied().unwrap_or(Value::Number(0.0)) {
          Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u32::MAX as f64 => n as u32,
          _ => 0,
        } as usize;

        let node_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), node_value)?;

        let result: Result<(), DomError> = with_active_vm_host(vm, |host| {
          mutate_dom_detached(host, |dom| dom.range_set_end(range_id, node_id, offset))
        })?;
        match result {
          Ok(()) => Ok(Value::Undefined),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      },

      ("Range", "selectNodeContents", 0) => {
        let (range_id, _document_id) = require_range_receiver(vm, scope, receiver)?;

        let node_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let node_id = require_dom_platform_mut(vm)?.require_node_id(scope.heap(), node_value)?;

        let result: Result<(), DomError> = with_active_vm_host(vm, |host| {
          mutate_dom_detached(host, |dom| dom.range_select_node_contents(range_id, node_id))
        })?;
        match result {
          Ok(()) => Ok(Value::Undefined),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      },

      ("Range", "toString", 0) => {
        let (range_id, _document_id) = require_range_receiver(vm, scope, receiver)?;

        let result: Result<String, DomError> =
          self.with_dom_host(vm, |host| Ok(host.with_dom(|dom| dom.range_to_string(range_id))))?;
        match result {
          Ok(s) => Ok(Value::String(scope.alloc_string(&s)?)),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      },

      ("AbstractRange", "collapsed", 0) => {
        let (range_id, _document_id) = require_range_receiver(vm, scope, receiver)?;

        let result: Result<bool, DomError> = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            let start = dom.range_start(range_id)?;
            let end = dom.range_end(range_id)?;
            Ok(start == end)
          }))
        })?;
        match result {
          Ok(b) => Ok(Value::Bool(b)),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      },

      ("Document", "createRange", 0) => {
        let document_obj = Self::require_receiver_object(receiver)?;

        let document_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_document_handle(scope.heap(), Value::Object(document_obj))?
            .document_id
        };

        // Ranges are document-owned state and should not trigger renderer invalidation.
        let owned_range_id: Option<RangeId> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| data.with_owned_dom2_document_mut(document_id, |dom| dom.create_range()));

        let range_id = if let Some(range_id) = owned_range_id {
          range_id
        } else {
          self.with_dom_host(vm, |host| Ok(host.mutate_dom(|dom| (dom.create_range(), false))))?
        };

        let proto = self.range_proto_from_global(vm, scope)?;
        scope.push_root(Value::Object(proto))?;
        let range_obj = scope.alloc_object_with_prototype(Some(proto))?;
        scope.push_root(Value::Object(range_obj))?;

        self.ranges.insert(
          WeakGcObject::from(range_obj),
          RangeState {
            document_id,
            range_id,
          },
        );
        Ok(Value::Object(range_obj))
      }

      ("Element", "matches", 0) => {
        let element_obj = Self::require_receiver_object(receiver)?;
        let element_id = {
          let platform = require_dom_platform_mut(vm)?;
          platform
            .require_element_handle(scope.heap(), Value::Object(element_obj))?
            .node_id
        };

        let selectors =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let result: Result<bool, DomException> = self.with_dom_host(vm, |host| {
          Ok(dom2_bindings::matches_selector(host, element_id, &selectors))
        })?;
        match result {
          Ok(found) => Ok(Value::Bool(found)),
          Err(err) => Err(self.dom_exception_to_vm_error(vm, scope, err)),
        }
      }
      ("Element", "closest", 0) => {
        let element_obj = Self::require_receiver_object(receiver)?;
        let (document_id, element_id) = {
          let platform = require_dom_platform_mut(vm)?;
          let handle = platform.require_element_handle(scope.heap(), Value::Object(element_obj))?;
          (handle.document_id, handle.node_id)
        };

        let selectors =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let result: Result<Option<(NodeId, DomInterface)>, DomException> = self.with_dom_host(vm, |host| {
          Ok(dom2_bindings::closest(host, element_id, &selectors).map(|found| {
            found.map(|node_id| {
              let primary = host.with_dom(|dom| {
                if node_id.index() >= dom.nodes_len() {
                  DomInterface::Node
                } else {
                  DomInterface::primary_for_node_kind(&dom.node(node_id).kind)
                }
              });
              (node_id, primary)
            })
          }))
        })?;

        match result {
          Ok(Some((node_id, primary_interface))) => {
            let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
              scope,
              document_id,
              node_id,
              primary_interface,
            )?;
            scope.push_root(Value::Object(wrapper))?;
            Ok(Value::Object(wrapper))
          }
          Ok(None) => Ok(Value::Null),
          Err(err) => Err(self.dom_exception_to_vm_error(vm, scope, err)),
        }
      }
      ("Element", "querySelector", 0) => {
        let element_obj = Self::require_receiver_object(receiver)?;
        let (document_id, element_id) = {
          let platform = require_dom_platform_mut(vm)?;
          let handle = platform.require_element_handle(scope.heap(), Value::Object(element_obj))?;
          (handle.document_id, handle.node_id)
        };

        let selectors =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let result: Result<Option<(NodeId, DomInterface)>, DomException> = self.with_dom_host(vm, |host| {
          Ok(dom2_bindings::query_selector(host, &selectors, Some(element_id)).map(|found| {
            found.map(|node_id| {
              let primary = host.with_dom(|dom| {
                if node_id.index() >= dom.nodes_len() {
                  DomInterface::Node
                } else {
                  DomInterface::primary_for_node_kind(&dom.node(node_id).kind)
                }
              });
              (node_id, primary)
            })
          }))
        })?;

        match result {
          Ok(Some((node_id, primary_interface))) => {
            let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
              scope,
              document_id,
              node_id,
              primary_interface,
            )?;
            scope.push_root(Value::Object(wrapper))?;
            Ok(Value::Object(wrapper))
          }
          Ok(None) => Ok(Value::Null),
          Err(err) => Err(self.dom_exception_to_vm_error(vm, scope, err)),
        }
      }
      ("Element", "querySelectorAll", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let Value::Object(element_obj) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
 
        let (document_id, element_id) = {
          let platform = require_dom_platform_mut(vm)?;
          let handle = platform.require_element_handle(scope.heap(), Value::Object(element_obj))?;
          (handle.document_id, handle.node_id)
        };

        // WebIDL wrapper objects store a back-reference to their owning `Document` wrapper; use the
        // realm's per-document NodeList prototype so `instanceof NodeList` works.
        let wrapper_document_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let document_obj = match scope
          .heap()
          .object_get_own_data_property_value(element_obj, &wrapper_document_key)?
        {
          Some(Value::Object(obj)) => obj,
          _ => return Err(VmError::TypeError("Illegal invocation")),
        };
        scope.push_root(Value::Object(document_obj))?;

        let node_list_proto_key = key_from_str(scope, NODE_LIST_PROTOTYPE_KEY)?;
        let node_list_proto = match scope
          .heap()
          .object_get_own_data_property_value(document_obj, &node_list_proto_key)?
        {
          Some(Value::Object(obj)) => obj,
          _ => {
            return Err(VmError::InvariantViolation(
              "missing NodeList prototype for Element.querySelectorAll",
            ))
          }
        };

        let selectors =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let result: Result<Vec<(NodeId, DomInterface)>, DomException> = self.with_dom_host(vm, |host| {
          Ok(dom2_bindings::query_selector_all(host, &selectors, Some(element_id)).map(|nodes| {
            host.with_dom(|dom| {
              nodes
                .into_iter()
                .map(|node_id| {
                  let primary = if node_id.index() >= dom.nodes_len() {
                    DomInterface::Node
                  } else {
                    DomInterface::primary_for_node_kind(&dom.node(node_id).kind)
                  };
                  (node_id, primary)
                })
                .collect()
            })
          }))
        })?;

        let nodes = match result {
          Ok(nodes) => nodes,
          Err(err) => return Err(self.dom_exception_to_vm_error(vm, scope, err)),
        };

        let list_obj = scope.alloc_object()?;
        scope.push_root(Value::Object(list_obj))?;
        scope
          .heap_mut()
          .object_set_prototype(list_obj, Some(node_list_proto))?;

        for (idx, (node_id, primary)) in nodes.iter().copied().enumerate() {
          let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
            scope,
            document_id,
            node_id,
            primary,
          )?;
          scope.push_root(Value::Object(wrapper))?;

          let idx_key = key_from_str(scope, &idx.to_string())?;
          scope.define_property(
            list_obj,
            idx_key,
            data_property(Value::Object(wrapper), true, true, true),
          )?;
        }

        let length_key = key_from_str(scope, COLLECTION_LENGTH_KEY)?;
        scope.define_property(
          list_obj,
          length_key,
          data_property(Value::Number(nodes.len() as f64), true, false, false),
        )?;

        Ok(Value::Object(list_obj))
      }
      ("Element", "getBoundingClientRect", 0) => {
        let (node_id, _obj) = require_element_receiver(vm, scope, receiver)?;

        let rect = with_active_vm_host_and_hooks(vm, |_vm, host, _hooks| {
          let any = host.as_any_mut();
          if let Some(document) = any.downcast_mut::<BrowserDocumentDom2>() {
            Ok(
              document
                .geometry_context()
                .ok()
                .and_then(|ctx| ctx.border_box_in_viewport(node_id))
                .unwrap_or(Rect::ZERO),
            )
          } else {
            Ok(Rect::ZERO)
          }
        })?
        .unwrap_or(Rect::ZERO);

        let global = if let Some(global) = self.global {
          global
        } else {
          vm
            .user_data_mut::<WindowRealmUserData>()
            .and_then(|data| data.window_obj())
            .ok_or(VmError::TypeError("Illegal invocation"))?
        };

        let rect_obj = crate::js::window_dom_rect::alloc_dom_rect_from_global(
          scope,
          global,
          rect.x() as f64,
          rect.y() as f64,
          rect.width() as f64,
          rect.height() as f64,
        )?;
        Ok(Value::Object(rect_obj))
      }
      ("Element", "offsetWidth", 0) => {
        let (node_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        let width = with_active_vm_host_and_hooks(vm, |_vm, host, _hooks| {
          let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() else {
            return Ok(0.0);
          };
          let rect = match document.border_box_rect_page(node_id) {
            Ok(Some(rect)) => rect,
            _ => return Ok(0.0),
          };
          Ok(layout_metric_nonneg_f32_to_f64_or_zero(rect.width()))
        })?
        .unwrap_or(0.0);
        Ok(Value::Number(width))
      }
      ("Element", "offsetHeight", 0) => {
        let (node_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        let height = with_active_vm_host_and_hooks(vm, |_vm, host, _hooks| {
          let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() else {
            return Ok(0.0);
          };
          let rect = match document.border_box_rect_page(node_id) {
            Ok(Some(rect)) => rect,
            _ => return Ok(0.0),
          };
          Ok(layout_metric_nonneg_f32_to_f64_or_zero(rect.height()))
        })?
        .unwrap_or(0.0);
        Ok(Value::Number(height))
      }
      ("Element", "offsetLeft", 0) => {
        let (node_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        let left = with_active_vm_host_and_hooks(vm, |_vm, host, _hooks| {
          let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() else {
            return Ok(0.0);
          };
          let rect = match document.border_box_rect_page(node_id) {
            Ok(Some(rect)) => rect,
            _ => return Ok(0.0),
          };
          Ok(layout_metric_f32_to_f64_or_zero(rect.x()))
        })?
        .unwrap_or(0.0);
        Ok(Value::Number(left))
      }
      ("Element", "offsetTop", 0) => {
        let (node_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        let top = with_active_vm_host_and_hooks(vm, |_vm, host, _hooks| {
          let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() else {
            return Ok(0.0);
          };
          let rect = match document.border_box_rect_page(node_id) {
            Ok(Some(rect)) => rect,
            _ => return Ok(0.0),
          };
          Ok(layout_metric_f32_to_f64_or_zero(rect.y()))
        })?
        .unwrap_or(0.0);
        Ok(Value::Number(top))
      }
      ("Element", "clientWidth", 0) => {
        let (node_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        let width = with_active_vm_host_and_hooks(vm, |_vm, host, _hooks| {
          let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() else {
            return Ok(0.0);
          };
          let Some(size) = document.client_size(node_id) else {
            return Ok(0.0);
          };
          Ok(layout_metric_nonneg_f32_to_f64_or_zero(size.width))
        })?
        .unwrap_or(0.0);
        Ok(Value::Number(width))
      }
      ("Element", "clientHeight", 0) => {
        let (node_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        let height = with_active_vm_host_and_hooks(vm, |_vm, host, _hooks| {
          let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() else {
            return Ok(0.0);
          };
          let Some(size) = document.client_size(node_id) else {
            return Ok(0.0);
          };
          Ok(layout_metric_nonneg_f32_to_f64_or_zero(size.height))
        })?
        .unwrap_or(0.0);
        Ok(Value::Number(height))
      }
      ("Element", "scrollWidth", 0) => {
        let (node_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        let width = with_active_vm_host_and_hooks(vm, |_vm, host, _hooks| {
          let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() else {
            return Ok(0.0);
          };
          let Some(size) = document.scroll_size(node_id) else {
            return Ok(0.0);
          };
          Ok(layout_metric_nonneg_f32_to_f64_or_zero(size.width))
        })?
        .unwrap_or(0.0);
        Ok(Value::Number(width))
      }
      ("Element", "scrollHeight", 0) => {
        let (node_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        let height = with_active_vm_host_and_hooks(vm, |_vm, host, _hooks| {
          let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() else {
            return Ok(0.0);
          };
          let Some(size) = document.scroll_size(node_id) else {
            return Ok(0.0);
          };
          Ok(layout_metric_nonneg_f32_to_f64_or_zero(size.height))
        })?
        .unwrap_or(0.0);
        Ok(Value::Number(height))
      }
      ("Element", "scrollTop", 0) => {
        let (node_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        if args.is_empty() {
          let y = with_active_vm_host_and_hooks(vm, |_vm, host, _hooks| {
            let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() else {
              return Ok(0.0);
            };
            Ok(layout_metric_f32_to_f64_or_zero(document.scroll_offset(node_id).y))
          })?
          .unwrap_or(0.0);
          Ok(Value::Number(y))
        } else {
          let mut y = match args.get(0).copied().unwrap_or(Value::Number(0.0)) {
            Value::Number(n) => n,
            _ => 0.0,
          };
          if !y.is_finite() {
            y = 0.0;
          }
          let y = finite_f64_to_f32_or_zero(y);

          let _ = with_active_vm_host_and_hooks(vm, |_vm, host, _hooks| {
            if let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() {
              let old = document.scroll_offset(node_id);
              let _ = document.set_scroll_offset(node_id, Point::new(old.x, y));
            }
            Ok(())
          })?;
          Ok(Value::Undefined)
        }
      }
      ("Element", "scrollLeft", 0) => {
        let (node_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        if args.is_empty() {
          let x = with_active_vm_host_and_hooks(vm, |_vm, host, _hooks| {
            let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() else {
              return Ok(0.0);
            };
            Ok(layout_metric_f32_to_f64_or_zero(document.scroll_offset(node_id).x))
          })?
          .unwrap_or(0.0);
          Ok(Value::Number(x))
        } else {
          let mut x = match args.get(0).copied().unwrap_or(Value::Number(0.0)) {
            Value::Number(n) => n,
            _ => 0.0,
          };
          if !x.is_finite() {
            x = 0.0;
          }
          let x = finite_f64_to_f32_or_zero(x);

          let _ = with_active_vm_host_and_hooks(vm, |_vm, host, _hooks| {
            if let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() {
              let old = document.scroll_offset(node_id);
              let _ = document.set_scroll_offset(node_id, Point::new(x, old.y));
            }
            Ok(())
          })?;
          Ok(Value::Undefined)
        }
      }

      ("Element", "id", 0) => {
        let (element_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        if args.is_empty() {
          let id = with_active_vm_host(vm, |host| {
            let any = host.as_any_mut();
            if let Some(host) = any.downcast_mut::<DocumentHostState>() {
              Ok(host.with_dom(|dom| dom.element_id(element_id).to_string()))
            } else if let Some(host) = any.downcast_mut::<BrowserDocumentDom2>() {
              Ok(host.with_dom(|dom| dom.element_id(element_id).to_string()))
            } else {
              Err(VmError::TypeError("DOM host not available"))
            }
          })?;
          let js = scope.alloc_string(&id)?;
          scope.push_root(Value::String(js))?;
          Ok(Value::String(js))
        } else {
          let value =
            js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
          let result: Result<(), DomError> = with_active_vm_host(vm, |host| {
            let any = host.as_any_mut();
            if let Some(host) = any.downcast_mut::<DocumentHostState>() {
              Ok(host.mutate_dom(|dom| match dom.set_element_id(element_id, &value) {
                Ok(changed) => (Ok(()), changed),
                Err(err) => (Err(err), false),
              }))
            } else if let Some(host) = any.downcast_mut::<BrowserDocumentDom2>() {
              Ok(DomHost::mutate_dom(host, |dom| match dom.set_element_id(element_id, &value) {
                Ok(changed) => (Ok(()), changed),
                Err(err) => (Err(err), false),
              }))
            } else {
              Err(VmError::TypeError("DOM host not available"))
            }
          })?;
          match result {
            Ok(()) => {
              self.sync_live_html_collections(vm, scope)?;
              Ok(Value::Undefined)
            }
            Err(err) => {
              let class = self.dom_exception_class_for_realm(vm, scope)?;
              Err(throw_dom_error(scope, class, err))
            }
          }
        }
      }
      ("Element", "className", 0) => {
        let (element_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        if args.is_empty() {
          let class_name = with_active_vm_host(vm, |host| {
            let any = host.as_any_mut();
            if let Some(host) = any.downcast_mut::<DocumentHostState>() {
              Ok(host.with_dom(|dom| dom.element_class_name(element_id).to_string()))
            } else if let Some(host) = any.downcast_mut::<BrowserDocumentDom2>() {
              Ok(host.with_dom(|dom| dom.element_class_name(element_id).to_string()))
            } else {
              Err(VmError::TypeError("DOM host not available"))
            }
          })?;
          let js = scope.alloc_string(&class_name)?;
          scope.push_root(Value::String(js))?;
          Ok(Value::String(js))
        } else {
          let value =
            js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
          let result: Result<(), DomError> = with_active_vm_host(vm, |host| {
            let any = host.as_any_mut();
            if let Some(host) = any.downcast_mut::<DocumentHostState>() {
              Ok(host.mutate_dom(|dom| match dom.set_element_class_name(element_id, &value) {
                Ok(changed) => (Ok(()), changed),
                Err(err) => (Err(err), false),
              }))
            } else if let Some(host) = any.downcast_mut::<BrowserDocumentDom2>() {
              Ok(DomHost::mutate_dom(host, |dom| match dom.set_element_class_name(element_id, &value) {
                Ok(changed) => (Ok(()), changed),
                Err(err) => (Err(err), false),
              }))
            } else {
              Err(VmError::TypeError("DOM host not available"))
            }
          })?;
          match result {
            Ok(()) => {
              self.sync_live_html_collections(vm, scope)?;
              Ok(Value::Undefined)
            }
            Err(err) => {
              let class = self.dom_exception_class_for_realm(vm, scope)?;
              Err(throw_dom_error(scope, class, err))
            }
          }
        }
      }
      ("Element", "tagName", 0) => {
        let (element_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        let tag_name = with_active_vm_host(vm, |host| {
          let any = host.as_any_mut();
          let compute = |dom: &crate::dom2::Document| match &dom.node(element_id).kind {
            NodeKind::Element {
              tag_name,
              namespace,
              ..
            } => {
              if namespace.is_empty() || namespace == HTML_NAMESPACE {
                tag_name.to_ascii_uppercase()
              } else {
                tag_name.clone()
              }
            }
            NodeKind::Slot { namespace, .. } => {
              if namespace.is_empty() || namespace == HTML_NAMESPACE {
                "SLOT".to_string()
              } else {
                "slot".to_string()
              }
            }
            _ => String::new(),
          };
          if let Some(host) = any.downcast_mut::<DocumentHostState>() {
            Ok(host.with_dom(compute))
          } else if let Some(host) = any.downcast_mut::<BrowserDocumentDom2>() {
            Ok(host.with_dom(compute))
          } else {
            Err(VmError::TypeError("DOM host not available"))
          }
        })?;
        let js = scope.alloc_string(&tag_name)?;
        scope.push_root(Value::String(js))?;
        Ok(Value::String(js))
      }
      ("Element", "shadowRoot", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let handle = require_dom_platform_mut(vm)?.require_element_handle(scope.heap(), receiver)?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;

        let shadow_root = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            let shadow_root = dom.shadow_root_for_host(node_id)?;
            match &dom.node(shadow_root).kind {
              NodeKind::ShadowRoot { mode, .. } if *mode == crate::dom::ShadowRootMode::Open => {
                Some(shadow_root)
              }
              _ => None,
            }
          }))
        })?;
        let Some(shadow_root_id) = shadow_root else {
          return Ok(Value::Null);
        };

        let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
          scope,
          document_id,
          shadow_root_id,
          DomInterface::ShadowRoot,
        )?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("Element", "getElementsByTagName", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let Value::Object(wrapper_obj) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        scope.push_root(Value::Object(wrapper_obj))?;

        let handle =
          require_dom_platform_mut(vm)?.require_element_handle(scope.heap(), Value::Object(wrapper_obj))?;
        let element_id = handle.node_id;
        let document_id = handle.document_id;

        let wrapper_document_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let document_obj = match scope
          .heap()
          .object_get_own_data_property_value(wrapper_obj, &wrapper_document_key)?
        {
          Some(Value::Object(obj)) => obj,
          _ => return Err(VmError::TypeError("Illegal invocation")),
        };
        scope.push_root(Value::Object(document_obj))?;

        let qualified_name =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let collection = self.create_live_html_collection(
          vm,
          scope,
          document_obj,
          wrapper_obj,
          document_id,
          element_id,
          LiveHtmlCollectionKind::TagName { qualified_name },
        )?;
        Ok(Value::Object(collection))
      }
      ("Element", "getElementsByTagNameNS", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let Value::Object(wrapper_obj) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        scope.push_root(Value::Object(wrapper_obj))?;

        let handle =
          require_dom_platform_mut(vm)?.require_element_handle(scope.heap(), Value::Object(wrapper_obj))?;
        let element_id = handle.node_id;
        let document_id = handle.document_id;

        let wrapper_document_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let document_obj = match scope
          .heap()
          .object_get_own_data_property_value(wrapper_obj, &wrapper_document_key)?
        {
          Some(Value::Object(obj)) => obj,
          _ => return Err(VmError::TypeError("Illegal invocation")),
        };
        scope.push_root(Value::Object(document_obj))?;

        let namespace_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let namespace = match namespace_value {
          Value::Null | Value::Undefined => None,
          Value::String(_) => Some(js_string_to_rust_string(scope, namespace_value)?),
          _ => return Err(VmError::TypeError("expected namespace to be a string or null")),
        };
        let local_name =
          js_string_to_rust_string(scope, args.get(1).copied().unwrap_or(Value::Undefined))?;

        let collection = self.create_live_html_collection(
          vm,
          scope,
          document_obj,
          wrapper_obj,
          document_id,
          element_id,
          LiveHtmlCollectionKind::TagNameNS {
            namespace,
            local_name,
          },
        )?;
        Ok(Value::Object(collection))
      }
      ("Element", "getElementsByClassName", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let Value::Object(wrapper_obj) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        scope.push_root(Value::Object(wrapper_obj))?;

        let handle =
          require_dom_platform_mut(vm)?.require_element_handle(scope.heap(), Value::Object(wrapper_obj))?;
        let element_id = handle.node_id;
        let document_id = handle.document_id;

        let wrapper_document_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let document_obj = match scope
          .heap()
          .object_get_own_data_property_value(wrapper_obj, &wrapper_document_key)?
        {
          Some(Value::Object(obj)) => obj,
          _ => return Err(VmError::TypeError("Illegal invocation")),
        };
        scope.push_root(Value::Object(document_obj))?;

        let class_names =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let collection = self.create_live_html_collection(
          vm,
          scope,
          document_obj,
          wrapper_obj,
          document_id,
          element_id,
          LiveHtmlCollectionKind::ClassName { class_names },
        )?;
        Ok(Value::Object(collection))
      }
      ("Element", "children", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let Value::Object(wrapper_obj) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        let handle =
          require_dom_platform_mut(vm)?.require_element_handle(scope.heap(), Value::Object(wrapper_obj))?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;

        // WebIDL wrapper objects store a back-reference to their owning `Document` wrapper via an
        // internal `__fastrender_*` property; reuse the same storage scheme as the handwritten
        // `ParentNode.children` shim so `instanceof HTMLCollection` works with the realm's
        // per-document prototype.
        let wrapper_document_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        let document_obj = match scope
          .heap()
          .object_get_own_data_property_value(wrapper_obj, &wrapper_document_key)?
        {
          Some(Value::Object(obj)) => obj,
          _ => return Err(VmError::TypeError("Illegal invocation")),
        };
        scope.push_root(Value::Object(document_obj))?;

        let children_key = key_from_str(scope, NODE_CHILDREN_KEY)?;
        let collection_obj = match scope
          .heap()
          .object_get_own_data_property_value(wrapper_obj, &children_key)?
        {
          Some(Value::Object(obj)) => obj,
          _ => {
            let collection = scope.alloc_object()?;
            scope.push_root(Value::Object(collection))?;

            let proto_key = key_from_str(scope, HTML_COLLECTION_PROTOTYPE_KEY)?;
            let proto = match scope
              .heap()
              .object_get_own_data_property_value(document_obj, &proto_key)?
            {
              Some(Value::Object(obj)) => obj,
              _ => {
                return Err(VmError::InvariantViolation(
                  "missing HTMLCollection prototype for Element.children",
                ))
              }
            };
            scope
              .heap_mut()
              .object_set_prototype(collection, Some(proto))?;

            // Keep the root wrapper alive even if the caller only holds the collection object.
            let root_key = key_from_str(scope, HTML_COLLECTION_ROOT_KEY)?;
            scope.define_property(
              collection,
              root_key,
              data_property(Value::Object(wrapper_obj), false, false, false),
            )?;

            scope.define_property(
              wrapper_obj,
              children_key,
              data_property(Value::Object(collection), false, false, false),
            )?;

            // Register as a live HTMLCollection so mutations (appendChild/removeChild/etc) update the
            // cached object even when callers retain the collection reference.
            self.live_html_collections.push(LiveHtmlCollection {
              weak_obj: WeakGcObject::from(collection),
              document_id,
              root: node_id,
              kind: LiveHtmlCollectionKind::ChildrenElements,
            });

            collection
          }
        };
        scope.push_root(Value::Object(collection_obj))?;

        let children: Vec<(NodeId, DomInterface)> = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            dom
              .children_elements(node_id)
              .into_iter()
              .map(|child_id| {
                let primary = DomInterface::primary_for_node_kind(&dom.node(child_id).kind);
                (child_id, primary)
              })
              .collect()
          }))
        })?;

        let length_key = key_from_str(scope, COLLECTION_LENGTH_KEY)?;
        let old_len = match scope
          .heap()
          .object_get_own_data_property_value(collection_obj, &length_key)?
        {
          Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
          _ => 0,
        };

        for (idx, (child_id, primary)) in children.iter().copied().enumerate() {
          let child_wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
            scope,
            document_id,
            child_id,
            primary,
          )?;
          scope.push_root(Value::Object(child_wrapper))?;

          let idx_key = key_from_str(scope, &idx.to_string())?;
          scope.define_property(
            collection_obj,
            idx_key,
            data_property(Value::Object(child_wrapper), true, true, true),
          )?;
        }

        for idx in children.len()..old_len {
          let idx_key = key_from_str(scope, &idx.to_string())?;
          scope.heap_mut().delete_property_or_throw(collection_obj, idx_key)?;
        }

        // Update internal length storage. Public `length` is exposed as a readonly accessor on
        // `HTMLCollection.prototype`.
        scope.define_property(
          collection_obj,
          length_key,
          data_property(Value::Number(children.len() as f64), true, false, false),
        )?;

        Ok(Value::Object(collection_obj))
      }
      ("Element", "firstElementChild", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let handle = require_dom_platform_mut(vm)?.require_element_handle(scope.heap(), receiver)?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;

        let found = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            dom.first_element_child(node_id).map(|child_id| {
              let primary = if child_id.index() >= dom.nodes_len() {
                DomInterface::Node
              } else {
                DomInterface::primary_for_node_kind(&dom.node(child_id).kind)
              };
              (child_id, primary)
            })
          }))
        })?;
        let Some((child_id, primary_interface)) = found else {
          return Ok(Value::Null);
        };

        let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
          scope,
          document_id,
          child_id,
          primary_interface,
        )?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("Element", "lastElementChild", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let handle = require_dom_platform_mut(vm)?.require_element_handle(scope.heap(), receiver)?;
        let node_id = handle.node_id;
        let document_id = handle.document_id;

        let found = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            dom.last_element_child(node_id).map(|child_id| {
              let primary = if child_id.index() >= dom.nodes_len() {
                DomInterface::Node
              } else {
                DomInterface::primary_for_node_kind(&dom.node(child_id).kind)
              };
              (child_id, primary)
            })
          }))
        })?;
        let Some((child_id, primary_interface)) = found else {
          return Ok(Value::Null);
        };

        let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
          scope,
          document_id,
          child_id,
          primary_interface,
        )?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("Element", "childElementCount", 0) => {
        let receiver = receiver.unwrap_or(Value::Undefined);
        let handle = require_dom_platform_mut(vm)?.require_element_handle(scope.heap(), receiver)?;
        let node_id = handle.node_id;
        let count =
          self.with_dom_host(vm, |host| Ok(host.with_dom(|dom| dom.child_element_count(node_id))))?;
        Ok(Value::Number(count as f64))
      }
      ("Element", "nextElementSibling", 0) => {
        let (element_id, obj) = require_element_receiver(vm, scope, receiver)?;
        let document_id = require_dom_platform_mut(vm)?
          .require_element_handle(scope.heap(), Value::Object(obj))?
          .document_id;

        let sib = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            let Some(sib_id) = dom.next_element_sibling(element_id) else {
              return None;
            };
            let primary = if sib_id.index() >= dom.nodes_len() {
              DomInterface::Node
            } else {
              DomInterface::primary_for_node_kind(&dom.node(sib_id).kind)
            };
            Some((sib_id, primary))
          }))
        })?;

        let Some((sib_id, primary)) = sib else {
          return Ok(Value::Null);
        };
        let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
          scope,
          document_id,
          sib_id,
          primary,
        )?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("Element", "previousElementSibling", 0) => {
        let (element_id, obj) = require_element_receiver(vm, scope, receiver)?;
        let document_id = require_dom_platform_mut(vm)?
          .require_element_handle(scope.heap(), Value::Object(obj))?
          .document_id;

        let sib = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            let Some(sib_id) = dom.previous_element_sibling(element_id) else {
              return None;
            };
            let primary = if sib_id.index() >= dom.nodes_len() {
              DomInterface::Node
            } else {
              DomInterface::primary_for_node_kind(&dom.node(sib_id).kind)
            };
            Some((sib_id, primary))
          }))
        })?;

        let Some((sib_id, primary)) = sib else {
          return Ok(Value::Null);
        };
        let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
          scope,
          document_id,
          sib_id,
          primary,
        )?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }
      ("NodeList", "item", 0) => {
        // NodeList.item(index): return own numeric property if present and not undefined; else null.
        let Some(Value::Object(list_obj)) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        // Root the receiver across key allocations.
        scope.push_root(Value::Object(list_obj))?;

        // Matches the handwritten vm-js shim: ToNumber + truncation toward zero, with negative
        // indices treated as out-of-range.
        let idx_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let mut n = match idx_value {
          Value::Number(n) => n,
          other => scope.heap_mut().to_number(other)?,
        };
        if !n.is_finite() || n.is_nan() {
          n = 0.0;
        }
        let n = n.trunc();
        if n < 0.0 {
          return Ok(Value::Null);
        }
        let idx = if n >= u32::MAX as f64 {
          u32::MAX
        } else {
          n as u32
        };

        let key = key_from_str(scope, &idx.to_string())?;
        Ok(
          scope
            .heap()
            .object_get_own_data_property_value(list_obj, &key)?
            .filter(|v| !matches!(v, Value::Undefined))
            .unwrap_or(Value::Null),
        )
      }
      ("NodeList", "length", 0) => {
        // NodeList.length: return internal length slot if present; else 0.
        let Some(Value::Object(list_obj)) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        scope.push_root(Value::Object(list_obj))?;

        let length_key = key_from_str(scope, COLLECTION_LENGTH_KEY)?;
        let len = match scope
          .heap()
          .object_get_own_data_property_value(list_obj, &length_key)?
        {
          Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n.trunc(),
          _ => 0.0,
        };
        Ok(Value::Number(len))
      }
      ("HTMLCollection", "item", 0) => {
        // HTMLCollection.item(index): return own numeric property if present and not undefined; else null.
        let Some(Value::Object(collection_obj)) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        scope.push_root(Value::Object(collection_obj))?;

        let idx_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let mut n = match idx_value {
          Value::Number(n) => n,
          other => scope.heap_mut().to_number(other)?,
        };
        if !n.is_finite() || n.is_nan() {
          n = 0.0;
        }
        let n = n.trunc();
        if n < 0.0 {
          return Ok(Value::Null);
        }
        let idx = if n >= u32::MAX as f64 {
          u32::MAX
        } else {
          n as u32
        };

        let key = key_from_str(scope, &idx.to_string())?;
        Ok(
          scope
            .heap()
            .object_get_own_data_property_value(collection_obj, &key)?
            .filter(|v| !matches!(v, Value::Undefined))
            .unwrap_or(Value::Null),
        )
      }
      ("HTMLCollection", "length", 0) => {
        let Some(Value::Object(collection_obj)) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        scope.push_root(Value::Object(collection_obj))?;

        let length_key = key_from_str(scope, COLLECTION_LENGTH_KEY)?;
        let len = match scope
          .heap()
          .object_get_own_data_property_value(collection_obj, &length_key)?
        {
          Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n.trunc(),
          _ => 0.0,
        };
        Ok(Value::Number(len))
      }
      ("HTMLCollection", "namedItem", 0) => {
        let Some(Value::Object(collection_obj)) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };
        scope.push_root(Value::Object(collection_obj))?;

        let query =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let length_key = key_from_str(scope, COLLECTION_LENGTH_KEY)?;
        let len = match scope
          .heap()
          .object_get_own_data_property_value(collection_obj, &length_key)?
        {
          Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
          _ => 0,
        };

        // Snapshot element IDs first so we can borrow the DOM once.
        let mut candidates: Vec<(GcObject, NodeId)> = Vec::new();
        candidates
          .try_reserve(len)
          .map_err(|_| VmError::OutOfMemory)?;
        for idx in 0..len {
          let idx_key = key_from_str(scope, &idx.to_string())?;
          let value = scope
            .heap()
            .object_get_own_data_property_value(collection_obj, &idx_key)?
            .unwrap_or(Value::Undefined);
          let Value::Object(wrapper_obj) = value else {
            continue;
          };
          scope.push_root(Value::Object(wrapper_obj))?;
          let element_id =
            match require_dom_platform_mut(vm)?.require_element_id(scope.heap(), value) {
              Ok(id) => id,
              Err(VmError::TypeError(_)) => continue,
              Err(err) => return Err(err),
            };
          candidates.push((wrapper_obj, element_id));
        }

        let found: Option<GcObject> = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            for (wrapper_obj, element_id) in &candidates {
              let id_attr = dom.get_attribute(*element_id, "id").ok().flatten();
              let name_attr = dom.get_attribute(*element_id, "name").ok().flatten();
              if id_attr == Some(query.as_str()) || name_attr == Some(query.as_str()) {
                return Some(*wrapper_obj);
              }
            }
            None
          }))
        })?;

        Ok(found.map(Value::Object).unwrap_or(Value::Null))
      }
      ("DOMTokenList", "add", 0) => {
        let (element_id, _obj) = require_dom_token_list_receiver(scope, receiver)?;

        let mut tokens: Vec<String> = Vec::new();
        tokens
          .try_reserve(args.len())
          .map_err(|_| VmError::OutOfMemory)?;
        for &arg in args {
          tokens.push(js_string_to_rust_string(scope, arg)?);
        }
        let token_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();

        let result: Result<bool, DomError> =
          self.with_dom_host(vm, |host| Ok(host.class_list_add(element_id, &token_refs)))?;
        match result {
          Ok(_) => {
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            Err(throw_dom_error(scope, class, err))
          }
        }
      }
      ("DOMTokenList", "remove", 0) => {
        let (element_id, _obj) = require_dom_token_list_receiver(scope, receiver)?;

        let mut tokens: Vec<String> = Vec::new();
        tokens
          .try_reserve(args.len())
          .map_err(|_| VmError::OutOfMemory)?;
        for &arg in args {
          tokens.push(js_string_to_rust_string(scope, arg)?);
        }
        let token_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();

        let result: Result<bool, DomError> =
          self.with_dom_host(vm, |host| Ok(host.class_list_remove(element_id, &token_refs)))?;
        match result {
          Ok(_) => {
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            Err(throw_dom_error(scope, class, err))
          }
        }
      }
      ("DOMTokenList", "contains", 0) => {
        let (element_id, _obj) = require_dom_token_list_receiver(scope, receiver)?;
        let token = js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let result: Result<bool, DomError> = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| dom.class_list_contains(element_id, &token)))
        })?;
        match result {
          Ok(b) => Ok(Value::Bool(b)),
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            Err(throw_dom_error(scope, class, err))
          }
        }
      }
      ("DOMTokenList", "toggle", 0) => {
        let (element_id, _obj) = require_dom_token_list_receiver(scope, receiver)?;
        let token = js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
        let force = match args.get(1).copied().unwrap_or(Value::Undefined) {
          Value::Undefined => None,
          Value::Bool(b) => Some(b),
          _ => None,
        };

        let result: Result<bool, DomError> =
          self.with_dom_host(vm, |host| Ok(host.class_list_toggle(element_id, &token, force)))?;
        match result {
          Ok(b) => {
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Bool(b))
          }
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            Err(throw_dom_error(scope, class, err))
          }
        }
      }
      ("DOMTokenList", "replace", 0) => {
        let (element_id, _obj) = require_dom_token_list_receiver(scope, receiver)?;
        let token = js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
        let new_token =
          js_string_to_rust_string(scope, args.get(1).copied().unwrap_or(Value::Undefined))?;

        let result: Result<bool, DomError> = self.with_dom_host(vm, |host| {
          Ok(host.class_list_replace(element_id, &token, &new_token))
        })?;
        match result {
          Ok(b) => {
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Bool(b))
          }
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            Err(throw_dom_error(scope, class, err))
          }
        }
      }
      ("DOMTokenList", "item", 0) => {
        let (element_id, _obj) = require_dom_token_list_receiver(scope, receiver)?;
        let idx = match args.get(0).copied().unwrap_or(Value::Number(0.0)) {
          Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u32::MAX as f64 => n as u32,
          _ => 0,
        } as usize;

        let result: Result<Vec<String>, DomError> =
          self.with_dom_host(vm, |host| Ok(host.class_list_tokens(element_id)))?;
        match result {
          Ok(tokens) => match tokens.get(idx) {
            Some(token) => Ok(Value::String(scope.alloc_string(token)?)),
            None => Ok(Value::Null),
          },
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            Err(throw_dom_error(scope, class, err))
          }
        }
      }
      ("DOMTokenList", "length", 0) => {
        let (element_id, _obj) = require_dom_token_list_receiver(scope, receiver)?;
        let result: Result<Vec<String>, DomError> =
          self.with_dom_host(vm, |host| Ok(host.class_list_tokens(element_id)))?;
        match result {
          Ok(tokens) => Ok(Value::Number(tokens.len() as f64)),
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            Err(throw_dom_error(scope, class, err))
          }
        }
      }
      ("DOMTokenList", "value", 0) => {
        let (element_id, _obj) = require_dom_token_list_receiver(scope, receiver)?;
        if args.is_empty() {
          let value: String = self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| dom.element_class_name(element_id).to_string()))
          })?;
          Ok(Value::String(scope.alloc_string(&value)?))
        } else {
          let value =
            js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
          let result: Result<bool, DomError> =
            self.with_dom_host(vm, |host| Ok(host.set_element_class_name(element_id, &value)))?;
          match result {
            Ok(_) => {
              self.sync_live_html_collections(vm, scope)?;
              Ok(Value::Undefined)
            }
            Err(err) => {
              let class = self.dom_exception_class_for_realm(vm, scope)?;
              Err(throw_dom_error(scope, class, err))
            }
          }
        }
      }
      ("DOMTokenList", "supports", 0) => {
        let _ = require_dom_token_list_receiver(scope, receiver)?;
        Err(VmError::TypeError(
          "DOMTokenList.supports: supported tokens not available",
        ))
      }
      ("Element", "classList", 0) => {
        let (element_id, obj) = require_element_receiver(vm, scope, receiver)?;

        let key = key_from_str(scope, ELEMENT_CLASS_LIST_PLACEHOLDER_SLOT)?;
        let global = self
          .global
          .or_else(|| vm.user_data_mut::<WindowRealmUserData>().and_then(|data| data.window_obj()))
          .ok_or(VmError::TypeError("Illegal invocation"))?;
        scope.push_root(Value::Object(global))?;
        let ctor_key = key_from_str(scope, "DOMTokenList")?;
        let Some(Value::Object(ctor_obj)) =
          scope.heap().object_get_own_data_property_value(global, &ctor_key)?
        else {
          return Err(VmError::TypeError("DOMTokenList constructor not available"));
        };
        scope.push_root(Value::Object(ctor_obj))?;
        let proto_key = key_from_str(scope, "prototype")?;
        let Some(Value::Object(proto_obj)) =
          scope.heap().object_get_own_data_property_value(ctor_obj, &proto_key)?
        else {
          return Err(VmError::TypeError("DOMTokenList.prototype not available"));
        };
        scope.push_root(Value::Object(proto_obj))?;

        if let Some(Value::Object(existing)) =
          scope.heap().object_get_own_data_property_value(obj, &key)?
        {
          scope.push_root(Value::Object(existing))?;
          scope.heap_mut().object_set_host_slots(
            existing,
            HostSlots {
              a: element_id.index() as u64,
              b: DOM_TOKEN_LIST_HOST_TAG,
            },
          )?;
          scope
            .heap_mut()
            .object_set_prototype(existing, Some(proto_obj))?;
          return Ok(Value::Object(existing));
        }

        let class_list = scope.alloc_object_with_prototype(Some(proto_obj))?;
        scope.push_root(Value::Object(class_list))?;
        scope.heap_mut().object_set_host_slots(
          class_list,
          HostSlots {
            a: element_id.index() as u64,
            b: DOM_TOKEN_LIST_HOST_TAG,
          },
        )?;
        scope.define_property(
          obj,
          key,
          data_property(Value::Object(class_list), false, false, false),
        )?;
        Ok(Value::Object(class_list))
      }
      ("Element", "style", 0) => {
        let Some(Value::Object(obj)) = receiver else {
          return Err(VmError::TypeError("Illegal invocation"));
        };

        // Receiver validation: require an Element wrapper.
        let (document_id, element_id, document_wrapper_from_platform) = {
          let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
          let handle = platform.require_element_handle(scope.heap(), Value::Object(obj))?;
          let document_wrapper = platform.get_existing_wrapper_for_document_id(
            scope.heap(),
            handle.document_id,
            NodeId::from_index(0),
          );
          (handle.document_id, handle.node_id, document_wrapper)
        };

        // Stable identity: return cached own data property if present.
        let style_key = key_from_str(scope, "style")?;
        match scope.heap().object_get_own_data_property_value(obj, &style_key) {
          Ok(Some(Value::Object(existing))) => return Ok(Value::Object(existing)),
          Ok(_) => {}
          Err(VmError::PropertyNotData) => {}
          Err(err) => return Err(err),
        }

        let document_obj = if let Some(doc) = document_wrapper_from_platform {
          doc
        } else {
          // Fallback: if this element belongs to the main document, use the cached `window.document`.
          let Some(doc) = vm
            .user_data_mut::<WindowRealmUserData>()
            .and_then(|data| data.document_obj())
          else {
            return Err(VmError::InvariantViolation(
              "Element.style: missing document object",
            ));
          };
          let key = WeakGcObject::from(doc);
          let main_document_id = (key.index() as u64) | ((key.generation() as u64) << 32);
          if main_document_id != document_id {
            return Err(VmError::InvariantViolation(
              "Element.style: missing document wrapper for element",
            ));
          }
          doc
        };

        // Allocate style object and keep it alive across property definitions.
        let style_obj = scope.alloc_object()?;
        scope.push_root(Value::Object(style_obj))?;
        scope.push_root(Value::Object(document_obj))?;
        scope.heap_mut().object_set_host_slots(
          style_obj,
          HostSlots {
            a: element_id.index() as u64,
            b: CSS_STYLE_DECL_HOST_TAG,
          },
        )?;

        // Optionally set prototype so `el.style instanceof CSSStyleDeclaration` works.
        let proto_key = key_from_str(scope, CSS_STYLE_DECL_PROTOTYPE_KEY)?;
        if let Some(Value::Object(proto)) =
          scope
            .heap()
            .object_get_own_data_property_value(document_obj, &proto_key)?
        {
          scope
            .heap_mut()
            .object_set_prototype(style_obj, Some(proto))
            .map_err(|_| VmError::TypeError("failed to set CSSStyleDeclaration prototype"))?;
        }

        // Hidden bookkeeping properties expected by the shared style native functions.
        let node_id_key = key_from_str(scope, NODE_ID_KEY)?;
        scope.define_property(
          style_obj,
          node_id_key,
          data_property(Value::Number(element_id.index() as f64), true, false, true),
        )?;

        let wrapper_document_key = key_from_str(scope, WRAPPER_DOCUMENT_KEY)?;
        scope.define_property(
          style_obj,
          wrapper_document_key,
          PropertyDescriptor {
            enumerable: false,
            configurable: false,
            kind: PropertyKind::Data {
              value: Value::Object(document_obj),
              writable: false,
            },
          },
        )?;

        // Reuse shared method/accessor functions stored on the document wrapper.
        let get_property_value = {
          let key = key_from_str(scope, STYLE_GET_PROPERTY_VALUE_KEY)?;
          match scope
            .heap()
            .object_get_own_data_property_value(document_obj, &key)?
          {
            Some(Value::Object(obj)) => obj,
            _ => {
              return Err(VmError::InvariantViolation(
                "Element.style: missing __fastrender_style_get_property_value",
              ))
            }
          }
        };
        let set_property = {
          let key = key_from_str(scope, STYLE_SET_PROPERTY_KEY)?;
          match scope
            .heap()
            .object_get_own_data_property_value(document_obj, &key)?
          {
            Some(Value::Object(obj)) => obj,
            _ => {
              return Err(VmError::InvariantViolation(
                "Element.style: missing __fastrender_style_set_property",
              ))
            }
          }
        };
        let remove_property = {
          let key = key_from_str(scope, STYLE_REMOVE_PROPERTY_KEY)?;
          match scope
            .heap()
            .object_get_own_data_property_value(document_obj, &key)?
          {
            Some(Value::Object(obj)) => obj,
            _ => {
              return Err(VmError::InvariantViolation(
                "Element.style: missing __fastrender_style_remove_property",
              ))
            }
          }
        };

        let get_property_value_key = key_from_str(scope, "getPropertyValue")?;
        scope.define_property(
          style_obj,
          get_property_value_key,
          data_property(Value::Object(get_property_value), true, false, true),
        )?;

        let set_property_key = key_from_str(scope, "setProperty")?;
        scope.define_property(
          style_obj,
          set_property_key,
          data_property(Value::Object(set_property), true, false, true),
        )?;

        let remove_property_key = key_from_str(scope, "removeProperty")?;
        scope.define_property(
          style_obj,
          remove_property_key,
          data_property(Value::Object(remove_property), true, false, true),
        )?;

        for (prop, hidden_get, hidden_set) in [
          ("cssText", STYLE_CSS_TEXT_GET_KEY, STYLE_CSS_TEXT_SET_KEY),
          ("display", STYLE_DISPLAY_GET_KEY, STYLE_DISPLAY_SET_KEY),
          ("cursor", STYLE_CURSOR_GET_KEY, STYLE_CURSOR_SET_KEY),
          ("height", STYLE_HEIGHT_GET_KEY, STYLE_HEIGHT_SET_KEY),
          ("width", STYLE_WIDTH_GET_KEY, STYLE_WIDTH_SET_KEY),
        ] {
          let get_key = key_from_str(scope, hidden_get)?;
          let get = match scope
            .heap()
            .object_get_own_data_property_value(document_obj, &get_key)?
          {
            Some(Value::Object(obj)) => obj,
            _ => {
              return Err(VmError::InvariantViolation(
                "Element.style: missing style property getter",
              ))
            }
          };
          let set_key = key_from_str(scope, hidden_set)?;
          let set = match scope
            .heap()
            .object_get_own_data_property_value(document_obj, &set_key)?
          {
            Some(Value::Object(obj)) => obj,
            _ => {
              return Err(VmError::InvariantViolation(
                "Element.style: missing style property setter",
              ))
            }
          };
          let prop_key = key_from_str(scope, prop)?;
          scope.define_property(
            style_obj,
            prop_key,
            PropertyDescriptor {
              enumerable: false,
              configurable: true,
              kind: PropertyKind::Accessor {
                get: Value::Object(get),
                set: Value::Object(set),
              },
            },
          )?;
        }

        // Cache on element wrapper so subsequent accesses are fast and stable.
        scope.define_property(
          obj,
          style_key,
          data_property(Value::Object(style_obj), true, false, true),
        )?;
        Ok(Value::Object(style_obj))
      }
      ("Element", "innerHTML", 0) => {
        let (element_id, obj) = require_element_receiver(vm, scope, receiver)?;
        let document_id = require_dom_platform_mut(vm)?
          .require_element_handle(scope.heap(), Value::Object(obj))?
          .document_id;
        if args.is_empty() {
          let result: Result<String, DomError> =
            self.with_dom_host(vm, |host| Ok(host.with_dom(|dom| dom.inner_html(element_id))))?;
          match result {
            Ok(html) => {
              let js = scope.alloc_string(&html)?;
              scope.push_root(Value::String(js))?;
              Ok(Value::String(js))
            }
            Err(err) => {
              let class = self.dom_exception_class_for_realm(vm, scope)?;
              Err(throw_dom_error(scope, class, err))
            }
          }
        } else {
          // `[LegacyNullToEmptyString]`: treat null/undefined as the empty string.
          let html_value = args.get(0).copied().unwrap_or(Value::Undefined);
          let html = match html_value {
            Value::Null | Value::Undefined => String::new(),
            other => {
              // Use vm-js's minimal `ToString` (avoids invoking user code).
              let html_s = scope.heap_mut().to_string(other)?;
              scope.heap().get_string(html_s)?.to_utf8_lossy()
            }
          };

          let result: Result<(), DomError> = self.with_dom_host(vm, |host| {
            Ok(host.mutate_dom(|dom| {
              let before = dom.mutation_generation();
              let res = dom.set_inner_html(element_id, &html);
              let changed = dom.mutation_generation() != before;
              (res, changed)
            }))
          })?;
          match result {
            Ok(()) => {
              // `innerHTML` replaces children; keep cached `childNodes` live NodeLists updated.
              self.sync_cached_child_nodes_for_wrapper(vm, scope, obj, element_id, document_id)?;
              self.sync_live_html_collections(vm, scope)?;
              Ok(Value::Undefined)
            }
            Err(err) => {
              let class = self.dom_exception_class_for_realm(vm, scope)?;
              Err(throw_dom_error(scope, class, err))
            }
          }
        }
      }
      ("Element", "outerHTML", 0) => {
        let (element_id, obj) = require_element_receiver(vm, scope, receiver)?;
        let document_id = require_dom_platform_mut(vm)?
          .require_element_handle(scope.heap(), Value::Object(obj))?
          .document_id;
        if args.is_empty() {
          let result: Result<String, DomError> =
            self.with_dom_host(vm, |host| Ok(host.with_dom(|dom| dom.outer_html(element_id))))?;
          match result {
            Ok(html) => {
              let js = scope.alloc_string(&html)?;
              scope.push_root(Value::String(js))?;
              Ok(Value::String(js))
            }
            Err(err) => {
              let class = self.dom_exception_class_for_realm(vm, scope)?;
              Err(throw_dom_error(scope, class, err))
            }
          }
        } else {
          // `[LegacyNullToEmptyString]`: treat null/undefined as the empty string.
          let html_value = args.get(0).copied().unwrap_or(Value::Undefined);
          let html = match html_value {
            Value::Null | Value::Undefined => String::new(),
            other => {
              // Use vm-js's minimal `ToString` (avoids invoking user code).
              let html_s = scope.heap_mut().to_string(other)?;
              scope.heap().get_string(html_s)?.to_utf8_lossy()
            }
          };

          let result: Result<Option<NodeId>, DomError> = self.with_dom_host(vm, |host| {
            Ok(host.mutate_dom(|dom| {
              let before = dom.mutation_generation();
              let parent = match dom.parent(element_id) {
                Ok(v) => v,
                Err(err) => return (Err(err), false),
              };
              let res = dom.set_outer_html(element_id, &html);
              let changed = dom.mutation_generation() != before;
              match res {
                Ok(()) => (Ok(parent), changed),
                Err(err) => (Err(err), false),
              }
            }))
          })?;
          match result {
            Ok(parent_id) => {
              // `outerHTML` replaces this element in its parent; keep cached `childNodes` live
              // NodeLists updated for the parent if it is wrapped.
              if let Some(parent_id) = parent_id {
                let parent_wrapper = {
                  let platform = require_dom_platform_mut(vm)?;
                  platform.get_existing_wrapper_for_document_id(scope.heap(), document_id, parent_id)
                };
                if let Some(parent_wrapper) = parent_wrapper {
                  self.sync_cached_child_nodes_for_wrapper(
                    vm,
                    scope,
                    parent_wrapper,
                    parent_id,
                    document_id,
                  )?;
                }
              }
              self.sync_live_html_collections(vm, scope)?;
              Ok(Value::Undefined)
            }
            Err(err) => {
              let class = self.dom_exception_class_for_realm(vm, scope)?;
              Err(throw_dom_error(scope, class, err))
            }
          }
        }
      }
      ("Element", "getAttribute", 0) => {
        let (element_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        let name =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;

        let value: Result<Option<String>, DomError> = with_active_vm_host(vm, |host| {
          let any = host.as_any_mut();
          let get = |dom: &crate::dom2::Document| {
            dom
              .get_attribute(element_id, &name)
              .map(|v| v.map(str::to_string))
          };
          if let Some(host) = any.downcast_mut::<DocumentHostState>() {
            Ok(host.with_dom(get))
          } else if let Some(host) = any.downcast_mut::<BrowserDocumentDom2>() {
            Ok(host.with_dom(get))
          } else {
            Err(VmError::TypeError("DOM host not available"))
          }
        })?;

        match value {
          Ok(Some(value)) => {
            let js = scope.alloc_string(&value)?;
            scope.push_root(Value::String(js))?;
            Ok(Value::String(js))
          }
          Ok(None) => Ok(Value::Null),
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            Err(throw_dom_error(scope, class, err))
          }
        }
      }
      ("Element", "setAttribute", 0) => {
        let (element_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        let name =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
        let value =
          js_string_to_rust_string(scope, args.get(1).copied().unwrap_or(Value::Undefined))?;

        let result: Result<bool, DomError> = with_active_vm_host(vm, |host| {
          let any = host.as_any_mut();
          if let Some(host) = any.downcast_mut::<DocumentHostState>() {
            Ok(dom2_bindings::set_attribute(host, element_id, &name, &value))
          } else if let Some(host) = any.downcast_mut::<BrowserDocumentDom2>() {
            Ok(dom2_bindings::set_attribute(host, element_id, &name, &value))
          } else {
            Err(VmError::TypeError("DOM host not available"))
          }
        })?;

        match result {
          Ok(_) => {
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            Err(throw_dom_error(scope, class, err))
          }
        }
      }
      ("Element", "removeAttribute", 0) => {
        let (element_id, _obj) = require_element_receiver(vm, scope, receiver)?;
        let name =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
        let result: Result<bool, DomError> = with_active_vm_host(vm, |host| {
          let any = host.as_any_mut();
          if let Some(host) = any.downcast_mut::<DocumentHostState>() {
            Ok(dom2_bindings::remove_attribute(host, element_id, &name))
          } else if let Some(host) = any.downcast_mut::<BrowserDocumentDom2>() {
            Ok(dom2_bindings::remove_attribute(host, element_id, &name))
          } else {
            Err(VmError::TypeError("DOM host not available"))
          }
        })?;

        match result {
          Ok(_) => {
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            Err(throw_dom_error(scope, class, err))
          }
        }
      }
      ("Element", "replaceWith", 0) => {
        let (node_id, obj) = require_element_receiver(vm, scope, receiver)?;
        let document_id = require_dom_platform_mut(vm)?
          .require_element_handle(scope.heap(), Value::Object(obj))?
          .document_id;

        enum ReplaceWithItem {
          Node(DomNodeKey),
          Text(String),
        }

        // Convert args: (Node or DOMString)...
        //
        // Spec note: WebIDL argument conversion happens before the `replaceWith` algorithm runs.
        // Since ToString can invoke user code, we must not read `parent`/`nextSibling` until after
        // conversion.
        let mut items: Vec<ReplaceWithItem> = Vec::with_capacity(args.len());
        for &arg in args {
          if matches!(arg, Value::Object(_)) {
            match require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), arg) {
              Ok(handle) => {
                items.push(ReplaceWithItem::Node(handle));
                continue;
              }
              Err(VmError::TypeError("Illegal invocation")) => {}
              Err(err) => return Err(err),
            }
          }

          let text = match arg {
            Value::String(_) => js_string_to_rust_string(scope, arg)?,
            other => {
              let s = with_active_vm_host_and_hooks(vm, |vm, host, hooks| {
                scope.to_string(vm, host, hooks, other)
              })?
              .ok_or(VmError::TypeError(DOM_HOST_NOT_AVAILABLE_ERROR))?;
              scope.heap().get_string(s)?.to_utf8_lossy()
            }
          };
          items.push(ReplaceWithItem::Text(text));
        }

        // Snapshot insertion side effects that need post-mutation cache syncing + adoption.
        //
        // Note: use `DomNodeKey` for all node identities so cross-document moves don't collide on
        // `NodeId` indices.
        let (old_parents, fragments, adopt_roots): (Vec<DomNodeKey>, Vec<DomNodeKey>, Vec<DomNodeKey>) =
          self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              let mut old_parents: Vec<DomNodeKey> = Vec::new();
              let mut fragments: Vec<DomNodeKey> = Vec::new();
              let mut adopt_roots: Vec<DomNodeKey> = Vec::new();

              for item in &items {
                let ReplaceWithItem::Node(handle) = item else {
                  continue;
                };
                let node_id = handle.node_id;
                if node_id.index() >= dom.nodes_len() {
                  continue;
                }

                let kind = &dom.node(node_id).kind;
                let is_fragment_like =
                  matches!(kind, NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. });

                if is_fragment_like {
                  fragments.push(*handle);
                  if handle.document_id != document_id {
                    // Fragment insertion is transparent: adopt children, not the fragment itself.
                    for &child in dom.node(node_id).children.iter() {
                      if child.index() >= dom.nodes_len() {
                        continue;
                      }
                      if dom.node(child).parent != Some(node_id) {
                        continue;
                      }
                      adopt_roots.push(DomNodeKey::new(handle.document_id, child));
                    }
                  }
                } else {
                  if let Some(p) = dom.parent_node(node_id) {
                    old_parents.push(DomNodeKey::new(handle.document_id, p));
                  }
                  if handle.document_id != document_id && !matches!(kind, NodeKind::Document { .. }) {
                    adopt_roots.push(*handle);
                  }
                }
              }

              old_parents.sort_by_key(|h| (h.document_id, h.node_id.index()));
              old_parents.dedup_by_key(|h| (h.document_id, h.node_id.index()));
              fragments.sort_by_key(|h| (h.document_id, h.node_id.index()));
              fragments.dedup_by_key(|h| (h.document_id, h.node_id.index()));
              adopt_roots.sort_by_key(|h| (h.document_id, h.node_id.index()));
              adopt_roots.dedup_by_key(|h| (h.document_id, h.node_id.index()));

              (old_parents, fragments, adopt_roots)
            }))
        })?;
        })?;

        // Snapshot subtree mappings for adoption. Apply wrapper remaps only after the DOM mutation
        // succeeds so failed `replaceWith` calls don't corrupt wrapper identity / `ownerDocument`.
        let mut adopt_mappings: Vec<(DocumentId, HashMap<NodeId, NodeId>)> = Vec::new();
        if !adopt_roots.is_empty() {
          adopt_mappings.reserve(adopt_roots.len());
          for handle in adopt_roots.iter().copied() {
            let root_id = handle.node_id;
            let mapping: HashMap<NodeId, NodeId> = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| {
                let mut mapping: HashMap<NodeId, NodeId> = HashMap::new();
                let mut stack: Vec<NodeId> = vec![root_id];
                let mut remaining = dom.nodes_len() + 1;
                while let Some(id) = stack.pop() {
                  if remaining == 0 {
                    break;
                  }
                  remaining -= 1;

                  if id.index() >= dom.nodes_len() {
                    continue;
                  }
                  mapping.insert(id, id);
                  let n = dom.node(id);
                  for &child in n.children.iter().rev() {
                    if child.index() >= dom.nodes_len() {
                      continue;
                    }
                    if dom.node(child).parent != Some(id) {
                      continue;
                    }
                    stack.push(child);
                  }
                }
                mapping
              }))
            })?;
            adopt_mappings.push((handle.document_id, mapping));
          }
        }

        // Spec: if the node is detached after argument conversion, return undefined.
        //
        // Otherwise:
        // - If args are empty, remove receiver from its parent.
        // - Else, convert args to a DocumentFragment, then:
        //   - If receiver is still a child of `parent`, replace it.
        //   - Otherwise, insert before `viableNextSibling` (handles cases where receiver was moved
        //     during conversion, e.g. `el.replaceWith(el)`).
        let result: Result<Option<NodeId>, DomError> = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            let generation = dom.mutation_generation();
            let result: Result<Option<NodeId>, DomError> = (|| {
              let Some(parent_id) = dom.parent(node_id)? else {
                return Ok(None);
              };

              // `viableNextSibling` is captured before conversion can move nodes out of the parent.
              let mut viable_next_sibling = dom.next_sibling(node_id);
              for item in &items {
                if let ReplaceWithItem::Node(handle) = item {
                  if viable_next_sibling == Some(handle.node_id) {
                    viable_next_sibling = dom.next_sibling(handle.node_id);
                  }
                }
              }

              if items.is_empty() {
                dom.remove_child(parent_id, node_id)?;
                return Ok(Some(parent_id));
              }

              let fragment = dom.create_document_fragment();
              for item in &items {
                let child_id = match item {
                  ReplaceWithItem::Node(handle) => handle.node_id,
                  ReplaceWithItem::Text(text) => dom.create_text(text),
                };
                let child_is_shadow_root = child_id.index() < dom.nodes_len()
                  && matches!(dom.node(child_id).kind, NodeKind::ShadowRoot { .. });
                if child_is_shadow_root {
                  dom.with_shadow_root_as_document_fragment(child_id, |dom| dom.append_child(fragment, child_id))?;
                } else {
                  dom.append_child(fragment, child_id)?;
                }
              }

              if dom.parent(node_id)? == Some(parent_id) {
                dom.replace_child(parent_id, fragment, node_id)?;
              } else {
                dom.insert_before(parent_id, fragment, viable_next_sibling)?;
              }
              Ok(Some(parent_id))
            })();

            let changed = dom.mutation_generation() != generation;
            (result, changed)
          }))
        })?;

        match result {
          Ok(Some(parent_id)) => {
            for (old_document_id, mapping) in adopt_mappings {
              require_dom_platform_mut(vm)?.remap_node_ids_between_documents(
                scope.heap_mut(),
                old_document_id,
                document_id,
                &mapping,
              )?;
            }

            let parent_wrapper = {
              let platform = require_dom_platform_mut(vm)?;
              platform.get_existing_wrapper_for_document_id(scope.heap(), document_id, parent_id)
            };
            if let Some(parent_wrapper) = parent_wrapper {
              self.sync_cached_child_nodes_for_wrapper(vm, scope, parent_wrapper, parent_id, document_id)?;
            }
            for old_parent in old_parents {
              // Only skip syncing when the old parent is *actually* the insertion parent (same
              // document + same NodeId).
              if old_parent.document_id == document_id && old_parent.node_id == parent_id {
                continue;
              }
              let old_parent_wrapper = {
                let platform = require_dom_platform_mut(vm)?;
                platform.get_existing_wrapper_for_document_id(
                  scope.heap(),
                  old_parent.document_id,
                  old_parent.node_id,
                )
              };
              if let Some(old_parent_wrapper) = old_parent_wrapper {
                self.sync_cached_child_nodes_for_wrapper(
                  vm,
                  scope,
                  old_parent_wrapper,
                  old_parent.node_id,
                  old_parent.document_id,
                )?;
              }
            }
            for fragment in fragments {
              let fragment_wrapper = {
                let platform = require_dom_platform_mut(vm)?;
                platform.get_existing_wrapper_for_document_id(
                  scope.heap(),
                  fragment.document_id,
                  fragment.node_id,
                )
              };
              if let Some(fragment_wrapper) = fragment_wrapper {
                self.sync_cached_child_nodes_for_wrapper(
                  vm,
                  scope,
                  fragment_wrapper,
                  fragment.node_id,
                  fragment.document_id,
                )?;
              }
            }
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Ok(None) => {
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            Err(throw_dom_error(scope, class, err))
          }
        }
      }
      ("Element", "insertAdjacentHTML", 0) => {
        let (element_id, obj) = require_element_receiver(vm, scope, receiver)?;
        let document_id = require_dom_platform_mut(vm)?
          .require_element_handle(scope.heap(), Value::Object(obj))?
          .document_id;
        let position =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
        let html_value = args.get(1).copied().unwrap_or(Value::Undefined);
        let html = match html_value {
          Value::String(_) => js_string_to_rust_string(scope, html_value)?,
          other => {
            let s = with_active_vm_host_and_hooks(vm, |vm, host, hooks| {
              scope.to_string(vm, host, hooks, other)
            })?
            .ok_or(VmError::TypeError(DOM_HOST_NOT_AVAILABLE_ERROR))?;
            scope.heap().get_string(s)?.to_utf8_lossy()
          }
        };

        let result: Result<(), DomError> = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            let before = dom.mutation_generation();
            let result = dom.insert_adjacent_html(element_id, &position, &html);
            let changed = dom.mutation_generation() != before;
            (result, changed)
          }))
        })?;

        match result {
          Ok(()) => {
            // Keep cached `childNodes` live NodeLists updated.
            match position.to_ascii_lowercase().as_str() {
              "afterbegin" | "beforeend" => {
                // Insertion inside the element mutates its child list.
                self.sync_cached_child_nodes_for_wrapper(vm, scope, obj, element_id, document_id)?;
              }
              "beforebegin" | "afterend" => {
                // Insertion as a sibling mutates the parent's child list.
                let parent_id: Result<Option<NodeId>, DomError> = self.with_dom_host(vm, |host| {
                  Ok(host.with_dom(|dom| dom.parent(element_id)))
                })?;
                if let Ok(Some(parent_id)) = parent_id {
                  let parent_wrapper = {
                    let platform = require_dom_platform_mut(vm)?;
                    platform.get_existing_wrapper_for_document_id(scope.heap(), document_id, parent_id)
                  };
                  if let Some(parent_wrapper) = parent_wrapper {
                    self.sync_cached_child_nodes_for_wrapper(
                      vm,
                      scope,
                      parent_wrapper,
                      parent_id,
                      document_id,
                    )?;
                  }
                }
              }
              _ => {}
            }
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            Err(throw_dom_error(scope, class, err))
          }
        }
      }
      ("Element", "insertAdjacentElement", 0) => {
        let (element_id, obj) = require_element_receiver(vm, scope, receiver)?;
        let document_id = require_dom_platform_mut(vm)?
          .require_element_handle(scope.heap(), Value::Object(obj))?
          .document_id;
        let where_ =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
        let new_element_val = args.get(1).copied().unwrap_or(Value::Undefined);
        let new_element_handle =
          require_dom_platform_mut(vm)?.require_element_handle(scope.heap(), new_element_val)?;
        let new_element_id = new_element_handle.node_id;
        let new_element_document_id = new_element_handle.document_id;

        let where_lower = where_.to_ascii_lowercase();
        let (target_parent, old_parent): (Option<DomNodeKey>, Option<DomNodeKey>) = self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            let target_parent = match where_lower.as_str() {
              "afterbegin" | "beforeend" => Some(DomNodeKey::new(document_id, element_id)),
              "beforebegin" | "afterend" => dom
                .parent_node(element_id)
                .map(|parent| DomNodeKey::new(document_id, parent)),
              _ => None,
            };
            let old_parent = if new_element_id.index() >= dom.nodes_len() {
              None
            } else {
              dom
                .parent_node(new_element_id)
                .map(|parent| DomNodeKey::new(new_element_document_id, parent))
            };
            (target_parent, old_parent)
          }))
        })?;

        // Snapshot subtree mappings for adoption. Apply wrapper remaps only after the DOM mutation
        // succeeds so failed insertions don't corrupt wrapper identity / `ownerDocument`.
        let mut adopt_mappings: Vec<(DocumentId, HashMap<NodeId, NodeId>)> = Vec::new();
        if new_element_document_id != document_id {
          let root_id = new_element_id;
          let mapping: HashMap<NodeId, NodeId> = self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              let mut mapping: HashMap<NodeId, NodeId> = HashMap::new();
              let mut stack: Vec<NodeId> = vec![root_id];
              let mut remaining = dom.nodes_len() + 1;
              while let Some(id) = stack.pop() {
                if remaining == 0 {
                  break;
                }
                remaining -= 1;

                if id.index() >= dom.nodes_len() {
                  continue;
                }
                mapping.insert(id, id);
                let n = dom.node(id);
                for &child in n.children.iter().rev() {
                  if child.index() >= dom.nodes_len() {
                    continue;
                  }
                  if dom.node(child).parent != Some(id) {
                    continue;
                  }
                  stack.push(child);
                }
              }
              mapping
            }))
          })?;
          adopt_mappings.push((new_element_document_id, mapping));
        }

        let result: Result<Option<NodeId>, DomError> = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            let before = dom.mutation_generation();
            let result = dom.insert_adjacent_element(element_id, &where_, new_element_id);
            let changed = dom.mutation_generation() != before;
            (result, changed)
          }))
        })?;

        match result {
          Ok(Some(_)) => {
            for (old_document_id, mapping) in adopt_mappings {
              require_dom_platform_mut(vm)?.remap_node_ids_between_documents(
                scope.heap_mut(),
                old_document_id,
                document_id,
                &mapping,
              )?;
            }

            // Keep cached `childNodes` live NodeLists updated.
            if let Some(parent_id) = target_parent {
              if parent_id.document_id == document_id && parent_id.node_id == element_id {
                self.sync_cached_child_nodes_for_wrapper(vm, scope, obj, element_id, document_id)?;
              } else {
                let parent_wrapper = {
                  let platform = require_dom_platform_mut(vm)?;
                  platform.get_existing_wrapper_for_document_id(
                    scope.heap(),
                    parent_id.document_id,
                    parent_id.node_id,
                  )
                };
                if let Some(parent_wrapper) = parent_wrapper {
                  self.sync_cached_child_nodes_for_wrapper(
                    vm,
                    scope,
                    parent_wrapper,
                    parent_id.node_id,
                    parent_id.document_id,
                  )?;
                }
              }
            }
            if let Some(old_parent) = old_parent {
              if Some(old_parent) != target_parent {
                let old_parent_wrapper = {
                  let platform = require_dom_platform_mut(vm)?;
                  platform.get_existing_wrapper_for_document_id(
                    scope.heap(),
                    old_parent.document_id,
                    old_parent.node_id,
                  )
                };
                if let Some(old_parent_wrapper) = old_parent_wrapper {
                  self.sync_cached_child_nodes_for_wrapper(
                    vm,
                    scope,
                    old_parent_wrapper,
                    old_parent.node_id,
                    old_parent.document_id,
                  )?;
                }
              }
            }
            self.sync_live_html_collections(vm, scope)?;
            Ok(new_element_val)
          }
          Ok(None) => Ok(Value::Null),
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            Err(throw_dom_error(scope, class, err))
          }
        }
      }
      ("Element", "insertAdjacentText", 0) => {
        let (element_id, obj) = require_element_receiver(vm, scope, receiver)?;
        let document_id = require_dom_platform_mut(vm)?
          .require_element_handle(scope.heap(), Value::Object(obj))?
          .document_id;
        let where_ =
          js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
        let data = js_string_to_rust_string(scope, args.get(1).copied().unwrap_or(Value::Undefined))?;

        let result: Result<(), DomError> = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            let before = dom.mutation_generation();
            let result = dom.insert_adjacent_text(element_id, &where_, &data);
            let changed = dom.mutation_generation() != before;
            (result, changed)
          }))
        })?;

        match result {
          Ok(()) => {
            // Keep cached `childNodes` live NodeLists updated.
            match where_.to_ascii_lowercase().as_str() {
              "afterbegin" | "beforeend" => {
                self.sync_cached_child_nodes_for_wrapper(vm, scope, obj, element_id, document_id)?;
              }
              "beforebegin" | "afterend" => {
                let parent_id: Result<Option<NodeId>, DomError> = self.with_dom_host(vm, |host| {
                  Ok(host.with_dom(|dom| dom.parent(element_id)))
                })?;
                if let Ok(Some(parent_id)) = parent_id {
                  let parent_wrapper = {
                    let platform = require_dom_platform_mut(vm)?;
                    platform.get_existing_wrapper_for_document_id(scope.heap(), document_id, parent_id)
                  };
                  if let Some(parent_wrapper) = parent_wrapper {
                    self.sync_cached_child_nodes_for_wrapper(
                      vm,
                      scope,
                      parent_wrapper,
                      parent_id,
                      document_id,
                    )?;
                  }
                }
              }
              _ => {}
            }
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            Err(throw_dom_error(scope, class, err))
          }
        }
      }

      (interface @ ("Element" | "Document" | "DocumentFragment"), op @ ("append" | "prepend"), 0) => {
        let prepend = op == "prepend";
        let (parent_id, wrapper_obj, document_id) = match interface {
          "Element" => {
            let (element_id, wrapper_obj) = require_element_receiver(vm, scope, receiver)?;
            scope.push_root(Value::Object(wrapper_obj))?;
            let document_id = require_dom_platform_mut(vm)?
              .require_element_handle(scope.heap(), Value::Object(wrapper_obj))?
              .document_id;
            (element_id, wrapper_obj, document_id)
          }
          "Document" => {
            let receiver = receiver.unwrap_or(Value::Undefined);
            let Value::Object(wrapper_obj) = receiver else {
              return Err(VmError::TypeError("Illegal invocation"));
            };
            scope.push_root(Value::Object(wrapper_obj))?;
            let handle = require_dom_platform_mut(vm)?
              .require_document_handle(scope.heap(), Value::Object(wrapper_obj))?;
            (handle.node_id, wrapper_obj, handle.document_id)
          }
          "DocumentFragment" => {
            let receiver = receiver.unwrap_or(Value::Undefined);
            let Value::Object(wrapper_obj) = receiver else {
              return Err(VmError::TypeError("Illegal invocation"));
            };
            scope.push_root(Value::Object(wrapper_obj))?;
            let handle = require_dom_platform_mut(vm)?
              .require_document_fragment_handle(scope.heap(), Value::Object(wrapper_obj))?;
            (handle.node_id, wrapper_obj, handle.document_id)
          }
          other => {
            debug_assert!(
              false,
              "unexpected WebIDL interface for append/prepend dispatch: {other}"
            );
            return Err(VmError::InvariantViolation(
              "unexpected WebIDL interface for append/prepend dispatch",
            ));
          }
        };

        #[derive(Debug)]
        enum NodeOrDomString {
          Node(DomNodeKey),
          Text(String),
        }

        let nodes: Vec<NodeOrDomString> =
          with_active_vm_host_and_hooks(vm, |vm, host, hooks| -> Result<_, VmError> {
            let mut nodes = Vec::with_capacity(args.len());
            for &value in args {
              // Per WebIDL `(Node or DOMString)`: try Node wrapper conversion first, then fall back
              // to stringification (full ECMAScript `ToString`, which can invoke user code).
              if matches!(value, Value::Object(_)) {
                match require_dom_platform_mut(vm)?.require_node_handle(scope.heap(), value) {
                  Ok(handle) => {
                    nodes.push(NodeOrDomString::Node(handle));
                    continue;
                  }
                  Err(VmError::TypeError("Illegal invocation")) => {}
                  Err(err) => return Err(err),
                }
              }

              let s = match value {
                Value::String(_) => js_string_to_rust_string(scope, value)?,
                other => {
                  let s = scope.to_string(vm, host, hooks, other)?;
                  scope.heap().get_string(s)?.to_utf8_lossy()
                }
              };
              nodes.push(NodeOrDomString::Text(s));
            }
            Ok(nodes)
          })?
          .ok_or(VmError::TypeError(DOM_HOST_NOT_AVAILABLE_ERROR))?;

        // Fast path: no args.
        if nodes.is_empty() {
          return Ok(Value::Undefined);
        }

        // Snapshot insertion side effects that need post-mutation cache syncing:
        // - Old parents of moved nodes (so cached `childNodes` NodeLists can be updated).
        // - Fragment-like nodes (DocumentFragment/ShadowRoot): insertion empties them, so cached
        //   `childNodes` NodeLists on the fragment itself must be updated.
        // - Roots of foreign subtrees that must be adopted into the destination document. Adoption
        //   updates wrapper identity + `ownerDocument`.
        //
        // Note: `dom2::NodeId` values are only unique within a document, so all cached-node sync keys
        // use `DomNodeKey` (document_id + node_id).
        let (old_parents, fragment_nodes, adopt_roots): (Vec<DomNodeKey>, Vec<DomNodeKey>, Vec<DomNodeKey>) =
          self.with_dom_host(vm, |host| {
          Ok(host.with_dom(|dom| {
            let mut old_parents: Vec<DomNodeKey> = Vec::new();
            let mut fragment_nodes: Vec<DomNodeKey> = Vec::new();
            let mut adopt_roots: Vec<DomNodeKey> = Vec::new();
            for item in &nodes {
              let NodeOrDomString::Node(handle) = item else {
                continue;
              };
              let node_id = handle.node_id;
              if node_id.index() >= dom.nodes_len() {
                continue;
              }

              let kind = &dom.node(node_id).kind;
              if let Some(parent) = dom.parent_node(node_id) {
                old_parents.push(DomNodeKey::new(handle.document_id, parent));
              }

              let is_fragment_like = matches!(kind, NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. });
              if is_fragment_like {
                fragment_nodes.push(DomNodeKey::new(handle.document_id, node_id));
              }

              // Adoption roots for cross-document nodes:
              // - Normal nodes adopt as a whole (root + descendants).
              // - Fragment-like nodes are transparent: their children are adopted, but the fragment
              //   itself stays in its original document.
              //
              // Mirror the semantics used by the handwritten vm-js shims (`window_realm.rs`):
              // cross-document fragment insertion adopts children but keeps the fragment itself in
              // its source document.
              if handle.document_id != document_id {
                match kind {
                  NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => {
                    for &child in dom.node(node_id).children.iter() {
                      if child.index() >= dom.nodes_len() {
                        continue;
                      }
                      if dom.node(child).parent != Some(node_id) {
                        continue;
                      }
                      adopt_roots.push(DomNodeKey::new(handle.document_id, child));
                    }
                  }
                  // `Document` nodes cannot be inserted anywhere; avoid remapping wrappers for them.
                  NodeKind::Document { .. } => {}
                  _ => {
                    adopt_roots.push(*handle);
                  }
                }
              }
            }

            // Avoid redundant sync work.
            old_parents.sort_by_key(|handle| (handle.document_id, handle.node_id.index()));
            old_parents.dedup_by_key(|handle| (handle.document_id, handle.node_id.index()));
            fragment_nodes.sort_by_key(|handle| (handle.document_id, handle.node_id.index()));
            fragment_nodes.dedup_by_key(|handle| (handle.document_id, handle.node_id.index()));
            adopt_roots.sort_by_key(|handle| (handle.document_id, handle.node_id.index()));
            adopt_roots.dedup_by_key(|handle| (handle.document_id, handle.node_id.index()));

            (old_parents, fragment_nodes, adopt_roots)
          }))
        })?;

        // Snapshot subtree mappings for adoption. DOMParser-created documents are modeled as
        // detached document roots inside the same host `dom2::Document` arena, so adoption is a
        // wrapper remap within a single node arena (no cloning needed).
        //
        // Important: apply wrapper remaps only after the DOM mutation succeeds, so failed insertions
        // don't corrupt `ownerDocument` / wrapper identity.
        let mut adopt_mappings: Vec<(DocumentId, HashMap<NodeId, NodeId>)> = Vec::new();
        if !adopt_roots.is_empty() {
          adopt_mappings.reserve(adopt_roots.len());
          for handle in adopt_roots.iter().copied() {
            let root_id = handle.node_id;
            let mapping: HashMap<NodeId, NodeId> = self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| {
                let mut mapping: HashMap<NodeId, NodeId> = HashMap::new();
                let mut stack: Vec<NodeId> = vec![root_id];
                let mut remaining = dom.nodes_len() + 1;
                while let Some(id) = stack.pop() {
                  if remaining == 0 {
                    break;
                  }
                  remaining -= 1;

                  if id.index() >= dom.nodes_len() {
                    continue;
                  }
                  mapping.insert(id, id);
                  let n = dom.node(id);
                  for &child in n.children.iter().rev() {
                    if child.index() >= dom.nodes_len() {
                      continue;
                    }
                    if dom.node(child).parent != Some(id) {
                      continue;
                    }
                    stack.push(child);
                  }
                }
                mapping
              }))
            })?;
            adopt_mappings.push((handle.document_id, mapping));
          }
        }

        let result: Result<(), DomError> = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            // Minimal WHATWG "convert nodes into a node" algorithm:
            // - If 1 item, use it directly (string => Text).
            // - If >1 items, build a DocumentFragment and insert once.
            let node_to_insert = if nodes.len() == 1 {
              match &nodes[0] {
                NodeOrDomString::Node(handle) => handle.node_id,
                NodeOrDomString::Text(s) => dom.create_text(s),
              }
            } else {
              let fragment = dom.create_document_fragment();
              for item in &nodes {
                let child = match item {
                  NodeOrDomString::Node(handle) => handle.node_id,
                  NodeOrDomString::Text(s) => dom.create_text(s),
                };
                let child_is_shadow_root =
                  child.index() < dom.nodes_len() && matches!(dom.node(child).kind, NodeKind::ShadowRoot { .. });
                let inserted = if child_is_shadow_root {
                  dom.with_shadow_root_as_document_fragment(child, |dom| dom.append_child(fragment, child))
                } else {
                  dom.append_child(fragment, child)
                };
                if let Err(err) = inserted {
                  return (Err(err), false);
                };
              }
              fragment
            };

            // `prepend` inserts before the (current) first child. For the multi-arg path, this is
            // computed after moving nodes into the temporary fragment, matching the DOM Standard.
            let reference = if prepend {
              let children = match dom.children(parent_id) {
                Ok(children) => children,
                Err(err) => return (Err(err), false),
              };
              children.iter().copied().find(|&child_id| {
                if child_id.index() >= dom.nodes_len() {
                  return false;
                }
                let child = dom.node(child_id);
                child.parent == Some(parent_id)
                  && !matches!(child.kind, NodeKind::ShadowRoot { .. })
              })
            } else {
              None
            };

            let node_is_shadow_root = node_to_insert.index() < dom.nodes_len()
              && matches!(dom.node(node_to_insert).kind, NodeKind::ShadowRoot { .. });
            let inserted = if node_is_shadow_root {
              if prepend {
                dom.with_shadow_root_as_document_fragment(node_to_insert, |dom| {
                  dom.insert_before(parent_id, node_to_insert, reference)
                })
              } else {
                dom.with_shadow_root_as_document_fragment(node_to_insert, |dom| dom.append_child(parent_id, node_to_insert))
              }
            } else if prepend {
              dom.insert_before(parent_id, node_to_insert, reference)
            } else {
              dom.append_child(parent_id, node_to_insert)
            };
            match inserted {
              Ok(changed) => (Ok(()), changed),
              Err(err) => (Err(err), false),
            }
          }))
        })?;

        match result {
          Ok(()) => {
            // Remap wrapper identity + ownerDocument for adopted subtrees.
            for (old_document_id, mapping) in adopt_mappings {
              require_dom_platform_mut(vm)?.remap_node_ids_between_documents(
                scope.heap_mut(),
                old_document_id,
                document_id,
                &mapping,
              )?;
            }

            // Keep cached `childNodes` live NodeLists updated for:
            // - the target parent;
            // - any old parents of moved nodes (e.g. `a.append(b)` moves `b` out of its old parent);
            // - fragment-like nodes that are emptied by insertion (e.g. DocumentFragment).
            self.sync_cached_child_nodes_for_wrapper(vm, scope, wrapper_obj, parent_id, document_id)?;
            // Sync old parents for moved nodes (if wrappers exist / had `childNodes` cached).
            for old_parent in old_parents {
              // `NodeId` values are only unique within a document, so only skip when the old parent
              // is *actually* the same node as the insertion parent (same document + same id).
              if old_parent.document_id == document_id && old_parent.node_id == parent_id {
                continue;
              }
              let wrapper = {
                require_dom_platform_mut(vm)?
                  .get_existing_wrapper_for_document_id(scope.heap(), old_parent.document_id, old_parent.node_id)
              };
              if let Some(wrapper) = wrapper {
                self.sync_cached_child_nodes_for_wrapper(
                  vm,
                  scope,
                  wrapper,
                  old_parent.node_id,
                  old_parent.document_id,
                )?;
              }
            }
            // Sync fragment nodes that were emptied by insertion.
            for fragment in fragment_nodes {
              let wrapper = {
                require_dom_platform_mut(vm)?
                  .get_existing_wrapper_for_document_id(scope.heap(), fragment.document_id, fragment.node_id)
              };
              if let Some(wrapper) = wrapper {
                self.sync_cached_child_nodes_for_wrapper(
                  vm,
                  scope,
                  wrapper,
                  fragment.node_id,
                  fragment.document_id,
                )?;
              }
            }
            // `children` is a live HTMLCollection and is kept up to date via `sync_live_html_collections`.
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("Element", "remove", 0) => {
        let (element_id, obj) = require_element_receiver(vm, scope, receiver)?;
        let document_id = require_dom_platform_mut(vm)?
          .require_element_handle(scope.heap(), Value::Object(obj))?
          .document_id;

        let result: Result<Option<NodeId>, DomError> = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| {
            let parent = match dom.parent(element_id) {
              Ok(v) => v,
              Err(err) => return (Err(err), false),
            };
            let Some(parent_id) = parent else {
              return (Ok(None), false);
            };
            match dom.remove_child(parent_id, element_id) {
              Ok(changed) => (Ok(Some(parent_id)), changed),
              Err(err) => (Err(err), false),
            }
          }))
        })?;

        match result {
          Ok(Some(parent_id)) => {
            let parent_wrapper = {
              let platform = require_dom_platform_mut(vm)?;
              platform.get_existing_wrapper_for_document_id(scope.heap(), document_id, parent_id)
            };
            if let Some(parent_wrapper) = parent_wrapper {
              self.sync_cached_child_nodes_for_wrapper(
                vm,
                scope,
                parent_wrapper,
                parent_id,
                document_id,
              )?;
            }
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Ok(None) => {
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("Range", "commonAncestorContainer", 0) => {
        let state = self.require_range_state(receiver)?;

        let owned_result: Option<Result<(NodeId, DomInterface), DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document(state.document_id, |dom| {
              let node_id = dom.range_common_ancestor_container(state.range_id)?;
              let primary = DomInterface::primary_for_node_kind(&dom.node(node_id).kind);
              Ok((node_id, primary))
            })
          });

        let result = if let Some(result) = owned_result {
          result
        } else {
          self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              let node_id = dom.range_common_ancestor_container(state.range_id)?;
              let primary = DomInterface::primary_for_node_kind(&dom.node(node_id).kind);
              Ok((node_id, primary))
            }))
          })?
        };

        match result {
          Ok((node_id, primary_interface)) => {
            let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
              scope,
              state.document_id,
              node_id,
              primary_interface,
            )?;
            scope.push_root(Value::Object(wrapper))?;
            Ok(Value::Object(wrapper))
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("Range", "constructor", 0) => {
        let range_obj = Self::require_receiver_object(receiver)?;

        let document_obj = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| data.document_obj())
          .ok_or(VmError::TypeError("Illegal invocation"))?;
        let document_id = gc_object_id(document_obj);

        let range_id = self.with_dom_host(vm, |host| {
          Ok(host.mutate_dom(|dom| (dom.create_range(), false)))
        })?;

        self.ranges.insert(
          WeakGcObject::from(range_obj),
          RangeState {
            document_id,
            range_id,
          },
        );
        Ok(Value::Undefined)
      }

      ("StaticRange", "constructor", 0) => {
        let range_obj = Self::require_receiver_object(receiver)?;
        scope.push_root(Value::Object(range_obj))?;

        let init = args.get(0).copied().unwrap_or(Value::Undefined);
        let Value::Object(init_obj) = init else {
          return Err(VmError::TypeError("StaticRange constructor requires an init object"));
        };
        scope.push_root(Value::Object(init_obj))?;

        let mut get_required = |name: &str, err: &'static str| -> Result<Value, VmError> {
          let key = key_from_str(scope, name)?;
          let value = vm.get(scope, init_obj, key)?;
          if matches!(value, Value::Undefined) {
            return Err(VmError::TypeError(err));
          }
          Ok(value)
        };

        let start_container =
          get_required("startContainer", "StaticRangeInit.startContainer is required")?;
        let start_offset = get_required("startOffset", "StaticRangeInit.startOffset is required")?;
        let end_container = get_required("endContainer", "StaticRangeInit.endContainer is required")?;
        let end_offset = get_required("endOffset", "StaticRangeInit.endOffset is required")?;

        // Root boundary containers across property definition: allocating property keys can GC.
        scope.push_roots(&[start_container, end_container])?;

        let mut validate_container = |v: Value| -> Result<(), VmError> {
          if matches!(v, Value::Null | Value::Undefined) {
            return Err(VmError::TypeError("StaticRangeInit container must be a Node"));
          }
          let Value::Object(obj) = v else {
            return Err(VmError::TypeError("StaticRangeInit container must be a Node"));
          };

          // Reject Attr nodes (WPT uses `Element.getAttributeNode`).
          let slots = match scope.heap().object_host_slots(obj) {
            Ok(slots) => slots,
            Err(VmError::InvalidHandle { .. }) if scope.heap().is_valid_object(obj) => None,
            Err(err) => return Err(err),
          };
          if matches!(slots, Some(slots) if slots.b == ATTR_HOST_TAG) {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            return Err(throw_dom_exception(
              scope,
              class,
              "InvalidNodeTypeError",
              "StaticRangeInit containers must not be Attr nodes",
            ));
          }

          // Reject DocumentType nodes.
          let is_document_type = {
            let platform = require_dom_platform_mut(vm)?;
            platform
              .require_document_type_handle(scope.heap(), Value::Object(obj))
              .is_ok()
          };
          if is_document_type {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            return Err(throw_dom_exception(
              scope,
              class,
              "InvalidNodeTypeError",
              "StaticRangeInit containers must not be DocumentType nodes",
            ));
          }

          {
            let platform = require_dom_platform_mut(vm)?;
            platform
              .require_node_handle(scope.heap(), v)
              .map_err(|_| VmError::TypeError("StaticRangeInit container must be a Node"))?;
          }
          Ok(())
        };

        validate_container(start_container)?;
        validate_container(end_container)?;

        let start_offset = match start_offset {
          Value::Number(n) => to_uint32_f64(n) as u32,
          _ => 0,
        };
        let end_offset = match end_offset {
          Value::Number(n) => to_uint32_f64(n) as u32,
          _ => 0,
        };

        let brand_key = key_from_str(scope, STATIC_RANGE_BRAND_KEY)?;
        scope.define_property(
          range_obj,
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

        let start_container_key = key_from_str(scope, STATIC_RANGE_START_CONTAINER_KEY)?;
        scope.define_property(
          range_obj,
          start_container_key,
          data_property(start_container, false, false, false),
        )?;
        let start_offset_key = key_from_str(scope, STATIC_RANGE_START_OFFSET_KEY)?;
        scope.define_property(
          range_obj,
          start_offset_key,
          data_property(Value::Number(start_offset as f64), false, false, false),
        )?;
        let end_container_key = key_from_str(scope, STATIC_RANGE_END_CONTAINER_KEY)?;
        scope.define_property(
          range_obj,
          end_container_key,
          data_property(end_container, false, false, false),
        )?;
        let end_offset_key = key_from_str(scope, STATIC_RANGE_END_OFFSET_KEY)?;
        scope.define_property(
          range_obj,
          end_offset_key,
          data_property(Value::Number(end_offset as f64), false, false, false),
        )?;

        Ok(Value::Undefined)
      }

      ("Range", "setStart", 0) | ("Range", "setEnd", 0) => {
        let range_obj = Self::require_receiver_object(receiver)?;
        let weak = WeakGcObject::from(range_obj);
        let state = self
          .ranges
          .get(&weak)
          .copied()
          .ok_or(VmError::TypeError("Illegal invocation"))?;

        let node_val = args.get(0).copied().unwrap_or(Value::Undefined);
        let node_handle = {
          let platform = require_dom_platform_mut(vm)?;
          platform.require_node_handle(scope.heap(), node_val)?
        };

        let offset_val = args.get(1).copied().unwrap_or(Value::Number(0.0));
        let offset = match offset_val {
          Value::Number(n) if n.is_finite() && n >= 0.0 => (n as u32) as usize,
          _ => 0,
        };

        let is_start = operation == "setStart";

        let (next_state, result): (RangeState, Result<(), DomError>) = if node_handle.document_id == state.document_id {
          // Same document: update in-place.
          let owned_result: Option<Result<(), DomError>> = vm
            .user_data_mut::<WindowRealmUserData>()
            .and_then(|data| {
              data.with_owned_dom2_document_mut(state.document_id, |dom| {
                if is_start {
                  dom.range_set_start(state.range_id, node_handle.node_id, offset)
                } else {
                  dom.range_set_end(state.range_id, node_handle.node_id, offset)
                }
              })
            });

          let result = if let Some(result) = owned_result {
            result
          } else {
            self.with_dom_host(vm, |host| {
              Ok(host.mutate_dom(|dom| {
                let result = if is_start {
                  dom.range_set_start(state.range_id, node_handle.node_id, offset)
                } else {
                  dom.range_set_end(state.range_id, node_handle.node_id, offset)
                };
                (result, false)
              }))
            })?
          };

          (state, result)
        } else {
          // Cross-document: create a new range in the target document and only migrate state if the
          // operation succeeds. (We may leak an unused range on error, but avoid mutating the
          // existing range in that case.)
          let owned_new_range_id: Option<RangeId> = vm
            .user_data_mut::<WindowRealmUserData>()
            .and_then(|data| {
              data.with_owned_dom2_document_mut(node_handle.document_id, |dom| dom.create_range())
            });

          let new_range_id = if let Some(id) = owned_new_range_id {
            id
          } else {
            self.with_dom_host(vm, |host| {
              Ok(host.mutate_dom(|dom| (dom.create_range(), false)))
            })?
          };

          let owned_result: Option<Result<(), DomError>> = vm
            .user_data_mut::<WindowRealmUserData>()
            .and_then(|data| {
              data.with_owned_dom2_document_mut(node_handle.document_id, |dom| {
                // Per spec, cross-root setStart/setEnd collapses the range to the boundary point.
                dom.range_set_start(new_range_id, node_handle.node_id, offset)?;
                dom.range_set_end(new_range_id, node_handle.node_id, offset)
              })
            });

          let result = if let Some(result) = owned_result {
            result
          } else {
            self.with_dom_host(vm, |host| {
              Ok(host.mutate_dom(|dom| {
                // Per spec, cross-root setStart/setEnd collapses the range to the boundary point.
                let result = dom
                  .range_set_start(new_range_id, node_handle.node_id, offset)
                  .and_then(|_| dom.range_set_end(new_range_id, node_handle.node_id, offset));
                (result, false)
              }))
            })?
          };

          (
            RangeState {
              document_id: node_handle.document_id,
              range_id: new_range_id,
            },
            result,
          )
        };

        match result {
          Ok(()) => {
            self.ranges.insert(weak, next_state);
            Ok(Value::Undefined)
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("Range", "isPointInRange", 0) | ("Range", "comparePoint", 0) => {
        let range_obj = Self::require_receiver_object(receiver)?;
        let state = self
          .ranges
          .get(&WeakGcObject::from(range_obj))
          .copied()
          .ok_or(VmError::TypeError("Illegal invocation"))?;

        let node_val = args.get(0).copied().unwrap_or(Value::Undefined);
        let node_handle = {
          let platform = require_dom_platform_mut(vm)?;
          platform.require_node_handle(scope.heap(), node_val)?
        };

        // Cross-document: comparePoint throws, isPointInRange returns false.
        if node_handle.document_id != state.document_id {
          if operation == "comparePoint" {
            return Err(self.dom_error_to_vm_error(vm, scope, DomError::WrongDocumentError));
          }
          return Ok(Value::Bool(false));
        }

        let offset_val = args.get(1).copied().unwrap_or(Value::Number(0.0));
        let offset = match offset_val {
          Value::Number(n) if n.is_finite() && n >= 0.0 => (n as u32) as usize,
          _ => 0,
        };

        let owned_result: Option<Result<i16, DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document(state.document_id, |dom| {
              if operation == "comparePoint" {
                dom.range_compare_point(state.range_id, node_handle.node_id, offset)
              } else {
                dom
                  .range_is_point_in_range(state.range_id, node_handle.node_id, offset)
                  .map(|b| if b { 1 } else { 0 })
              }
            })
          });

        if let Some(result) = owned_result {
          match (operation, result) {
            ("comparePoint", Ok(v)) => Ok(Value::Number(v as f64)),
            ("isPointInRange", Ok(v)) => Ok(Value::Bool(v != 0)),
            (_, Err(err)) => Err(self.dom_error_to_vm_error(vm, scope, err)),
            _ => Err(VmError::InvariantViolation("Range operation mismatch")),
          }
        } else {
          let result = self.with_dom_host(vm, |host| {
            Ok(host.mutate_dom(|dom| {
              let out = if operation == "comparePoint" {
                dom.range_compare_point(state.range_id, node_handle.node_id, offset)
              } else {
                dom
                  .range_is_point_in_range(state.range_id, node_handle.node_id, offset)
                  .map(|b| if b { 1 } else { 0 })
              };
              (out, false)
            }))
          })?;

          match (operation, result) {
            ("comparePoint", Ok(v)) => Ok(Value::Number(v as f64)),
            ("isPointInRange", Ok(v)) => Ok(Value::Bool(v != 0)),
            (_, Err(err)) => Err(self.dom_error_to_vm_error(vm, scope, err)),
            _ => Err(VmError::InvariantViolation("Range operation mismatch")),
          }
        }
      }

      ("Range", "intersectsNode", 0) => {
        let range_obj = Self::require_receiver_object(receiver)?;
        let state = self
          .ranges
          .get(&WeakGcObject::from(range_obj))
          .copied()
          .ok_or(VmError::TypeError("Illegal invocation"))?;

        let node_val = args.get(0).copied().unwrap_or(Value::Undefined);
        let node_handle = {
          let platform = require_dom_platform_mut(vm)?;
          platform.require_node_handle(scope.heap(), node_val)?
        };

        if node_handle.document_id != state.document_id {
          return Ok(Value::Bool(false));
        }

        let owned_result: Option<Result<bool, DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document(state.document_id, |dom| {
              dom.range_intersects_node(state.range_id, node_handle.node_id)
            })
          });

        let result = if let Some(result) = owned_result {
          result
        } else {
          self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              dom.range_intersects_node(state.range_id, node_handle.node_id)
            }))
          })?
        };

        match result {
          Ok(v) => Ok(Value::Bool(v)),
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("Range", "deleteContents", 0) => {
        let state = self.require_range_state(receiver)?;

        // Gather a conservative set of ancestor nodes whose `childNodes` live NodeLists may need to
        // be synced after the mutation.
        let owned_ancestors: Option<Result<Vec<NodeId>, DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document(state.document_id, |dom| {
              let start = dom.range_start_container(state.range_id)?;
              let end = dom.range_end_container(state.range_id)?;
              let common = dom.range_common_ancestor_container(state.range_id)?;

              let mut out: HashSet<NodeId> = HashSet::new();
              let mut n = start;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              let mut n = end;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              Ok(out.into_iter().collect())
            })
          });

        let ancestors: Result<Vec<NodeId>, DomError> = if let Some(result) = owned_ancestors {
          result
        } else {
          self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              let start = dom.range_start_container(state.range_id)?;
              let end = dom.range_end_container(state.range_id)?;
              let common = dom.range_common_ancestor_container(state.range_id)?;

              let mut out: HashSet<NodeId> = HashSet::new();
              let mut n = start;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              let mut n = end;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              Ok(out.into_iter().collect())
            }))
          })?
        };

        let ancestors = match ancestors {
          Ok(v) => v,
          Err(err) => return Err(self.dom_error_to_vm_error(vm, scope, err)),
        };

        let owned_result: Option<Result<(), DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document_mut(state.document_id, |dom| {
              dom.range_delete_contents(state.range_id)
            })
          });

        let result = if let Some(result) = owned_result {
          result
        } else {
          self.with_dom_host(vm, |host| {
            Ok(host.mutate_dom(|dom| match dom.range_delete_contents(state.range_id) {
              Ok(()) => (Ok(()), true),
              Err(err) => (Err(err), false),
            }))
          })?
        };

        match result {
          Ok(()) => {
            for node_id in ancestors {
              let wrapper = {
                let platform = require_dom_platform_mut(vm)?;
                platform.get_existing_wrapper_for_document_id(scope.heap(), state.document_id, node_id)
              };
              if let Some(wrapper) = wrapper {
                self.sync_cached_child_nodes_for_wrapper(vm, scope, wrapper, node_id, state.document_id)?;
              }
            }
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("Range", "extractContents", 0) => {
        let state = self.require_range_state(receiver)?;

        let owned_ancestors: Option<Result<Vec<NodeId>, DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document(state.document_id, |dom| {
              let start = dom.range_start_container(state.range_id)?;
              let end = dom.range_end_container(state.range_id)?;
              let common = dom.range_common_ancestor_container(state.range_id)?;

              let mut out: HashSet<NodeId> = HashSet::new();
              let mut n = start;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              let mut n = end;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              Ok(out.into_iter().collect())
            })
          });

        let ancestors: Result<Vec<NodeId>, DomError> = if let Some(result) = owned_ancestors {
          result
        } else {
          self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              let start = dom.range_start_container(state.range_id)?;
              let end = dom.range_end_container(state.range_id)?;
              let common = dom.range_common_ancestor_container(state.range_id)?;

              let mut out: HashSet<NodeId> = HashSet::new();
              let mut n = start;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              let mut n = end;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              Ok(out.into_iter().collect())
            }))
          })?
        };

        let ancestors = match ancestors {
          Ok(v) => v,
          Err(err) => return Err(self.dom_error_to_vm_error(vm, scope, err)),
        };

        let owned_result: Option<Result<NodeId, DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document_mut(state.document_id, |dom| {
              dom.range_extract_contents(state.range_id)
            })
          });

        let result = if let Some(result) = owned_result {
          result
        } else {
          self.with_dom_host(vm, |host| {
            Ok(host.mutate_dom(|dom| match dom.range_extract_contents(state.range_id) {
              Ok(fragment) => (Ok(fragment), true),
              Err(err) => (Err(err), false),
            }))
          })?
        };

        match result {
          Ok(fragment_id) => {
            for node_id in ancestors {
              let wrapper = {
                let platform = require_dom_platform_mut(vm)?;
                platform.get_existing_wrapper_for_document_id(scope.heap(), state.document_id, node_id)
              };
              if let Some(wrapper) = wrapper {
                self.sync_cached_child_nodes_for_wrapper(vm, scope, wrapper, node_id, state.document_id)?;
              }
            }
            self.sync_live_html_collections(vm, scope)?;

            let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
              scope,
              state.document_id,
              fragment_id,
              DomInterface::DocumentFragment,
            )?;
            scope.push_root(Value::Object(wrapper))?;
            Ok(Value::Object(wrapper))
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("Range", "cloneContents", 0) => {
        let state = self.require_range_state(receiver)?;

        let owned_result: Option<Result<NodeId, DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document_mut(state.document_id, |dom| dom.range_clone_contents(state.range_id))
          });

        let result = if let Some(result) = owned_result {
          result
        } else {
          self.with_dom_host(vm, |host| {
            Ok(host.mutate_dom(|dom| match dom.range_clone_contents(state.range_id) {
              Ok(fragment) => (Ok(fragment), /* changed */ false),
              Err(err) => (Err(err), false),
            }))
          })?
        };

        match result {
          Ok(fragment_id) => {
            let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
              scope,
              state.document_id,
              fragment_id,
              DomInterface::DocumentFragment,
            )?;
            scope.push_root(Value::Object(wrapper))?;
            Ok(Value::Object(wrapper))
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("Range", "insertNode", 0) => {
        let state = self.require_range_state(receiver)?;

        let node_val = args.get(0).copied().unwrap_or(Value::Undefined);
        let node_handle = {
          let platform = require_dom_platform_mut(vm)?;
          platform.require_node_handle(scope.heap(), node_val)?
        };
        // Cross-document insertion adopts the inserted node into the range's document (see
        // `maybe_register_document_alias_wrapper` tests in this module). We only support adoption
        // between wrappers that share the same underlying host `dom2::Document` allocation: if
        // either document id corresponds to an owned `dom2::Document`, require them to match to
        // avoid mixing distinct arenas.
        if node_handle.document_id != state.document_id {
          let (range_is_owned, node_is_owned) = {
            let mut range_is_owned = false;
            let mut node_is_owned = false;
            if let Some(data) = vm.user_data::<WindowRealmUserData>() {
              range_is_owned = data
                .with_owned_dom2_document(state.document_id, |_| ())
                .is_some();
              node_is_owned = data
                .with_owned_dom2_document(node_handle.document_id, |_| ())
                .is_some();
            }
            (range_is_owned, node_is_owned)
          };
          if range_is_owned || node_is_owned {
            return Err(self.dom_error_to_vm_error(vm, scope, DomError::WrongDocumentError));
          }
        }

        // Range endpoint ancestors likely to have their cached NodeLists invalidated.
        let owned_ancestors: Option<Result<Vec<NodeId>, DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document(state.document_id, |dom| {
              let start = dom.range_start_container(state.range_id)?;
              let end = dom.range_end_container(state.range_id)?;
              let common = dom.range_common_ancestor_container(state.range_id)?;

              let mut out: HashSet<NodeId> = HashSet::new();
              // Range.insertNode() can split Text nodes, which mutates the parent even when the
              // common ancestor is the Text node itself. Include the tree parents to keep cached
              // NodeLists live.
              if let Some(parent) = dom.tree_parent_node(start) {
                out.insert(parent);
              }
              if let Some(parent) = dom.tree_parent_node(end) {
                out.insert(parent);
              }
              let mut n = start;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              let mut n = end;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              Ok(out.into_iter().collect())
            })
          });

        let ancestors: Result<Vec<NodeId>, DomError> = if let Some(result) = owned_ancestors {
          result
        } else {
          self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              let start = dom.range_start_container(state.range_id)?;
              let end = dom.range_end_container(state.range_id)?;
              let common = dom.range_common_ancestor_container(state.range_id)?;

              let mut out: HashSet<NodeId> = HashSet::new();
              if let Some(parent) = dom.tree_parent_node(start) {
                out.insert(parent);
              }
              if let Some(parent) = dom.tree_parent_node(end) {
                out.insert(parent);
              }
              let mut n = start;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              let mut n = end;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              Ok(out.into_iter().collect())
            }))
          })?
        };

        let ancestors = match ancestors {
          Ok(v) => v,
          Err(err) => return Err(self.dom_error_to_vm_error(vm, scope, err)),
        };

        // Snapshot adoption mappings for inserted nodes created by an alias Document wrapper.
        let mut adopt_mappings: Vec<(DocumentId, HashMap<NodeId, NodeId>)> = Vec::new();
        if node_handle.document_id != state.document_id {
          let adopt_roots: Vec<DomNodeKey> = self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              if node_handle.node_id.index() >= dom.nodes_len() {
                return Vec::new();
              }
              let kind = &dom.node(node_handle.node_id).kind;
              match kind {
                NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => dom
                  .node(node_handle.node_id)
                  .children
                  .iter()
                  .copied()
                  .filter(|&child| {
                    child.index() < dom.nodes_len() && dom.node(child).parent == Some(node_handle.node_id)
                  })
                  .map(|child| DomNodeKey::new(node_handle.document_id, child))
                  .collect(),
                NodeKind::Document { .. } => Vec::new(),
                _ => vec![node_handle],
              }
            }))
          })?;
          if !adopt_roots.is_empty() {
            adopt_mappings.reserve(adopt_roots.len());
            for handle in adopt_roots.iter().copied() {
              let root_id = handle.node_id;
              let mapping: HashMap<NodeId, NodeId> = self.with_dom_host(vm, |host| {
                Ok(host.with_dom(|dom| {
                  let mut mapping: HashMap<NodeId, NodeId> = HashMap::new();
                  let mut stack: Vec<NodeId> = vec![root_id];
                  let mut remaining = dom.nodes_len() + 1;
                  while let Some(id) = stack.pop() {
                    if remaining == 0 {
                      break;
                    }
                    remaining -= 1;
 
                    if id.index() >= dom.nodes_len() {
                      continue;
                    }
                    mapping.insert(id, id);
                    let n = dom.node(id);
                    for &child in n.children.iter().rev() {
                      if child.index() >= dom.nodes_len() {
                        continue;
                      }
                      if dom.node(child).parent != Some(id) {
                        continue;
                      }
                      stack.push(child);
                    }
                  }
                  mapping
                }))
              })?;
              adopt_mappings.push((handle.document_id, mapping));
            }
          }
        }
 
        // Track old parent + fragment-like insertion semantics so we can keep cached NodeLists live.
        let owned_parent_info: Option<Result<(Option<DomNodeKey>, bool), DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document(state.document_id, |dom| {
              let old_parent = dom.parent(node_handle.node_id)?;
              let node_is_fragment_like = node_handle.node_id.index() < dom.nodes_len()
                && matches!(
                  dom.node(node_handle.node_id).kind,
                  NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. }
                );
              let old_parent = old_parent.map(|id| DomNodeKey::new(node_handle.document_id, id));
              Ok((old_parent, node_is_fragment_like))
            })
          });

        let parent_info: Result<(Option<DomNodeKey>, bool), DomError> = if let Some(result) = owned_parent_info {
          result
        } else {
          self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              let old_parent = dom.parent(node_handle.node_id)?;
              let node_is_fragment_like = node_handle.node_id.index() < dom.nodes_len()
                && matches!(
                  dom.node(node_handle.node_id).kind,
                  NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. }
                );
              let old_parent = old_parent.map(|id| DomNodeKey::new(node_handle.document_id, id));
              Ok((old_parent, node_is_fragment_like))
            }))
          })?
        };

        let (old_parent, node_is_fragment_like) = match parent_info {
          Ok(v) => v,
          Err(err) => return Err(self.dom_error_to_vm_error(vm, scope, err)),
        };

        let owned_result: Option<Result<(), DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document_mut(state.document_id, |dom| {
              dom.range_insert_node(state.range_id, node_handle.node_id)
            })
          });

        let result = if let Some(result) = owned_result {
          result
        } else {
          self.with_dom_host(vm, |host| {
            Ok(host.mutate_dom(|dom| match dom.range_insert_node(state.range_id, node_handle.node_id) {
              Ok(()) => (Ok(()), true),
              Err(err) => (Err(err), false),
            }))
          })?
        };

        match result {
          Ok(()) => {
            // Remap wrapper identity + ownerDocument for adopted subtrees.
            for (old_document_id, mapping) in adopt_mappings {
              require_dom_platform_mut(vm)?.remap_node_ids_between_documents(
                scope.heap_mut(),
                old_document_id,
                state.document_id,
                &mapping,
              )?;
            }
 
            let owned_new_parent: Option<Result<Option<NodeId>, DomError>> = vm
              .user_data_mut::<WindowRealmUserData>()
              .and_then(|data| {
                data.with_owned_dom2_document(state.document_id, |dom| dom.parent(node_handle.node_id))
              });
            let new_parent: Result<Option<NodeId>, DomError> = if let Some(result) = owned_new_parent {
              result
            } else {
              self.with_dom_host(vm, |host| {
                Ok(host.with_dom(|dom| dom.parent(node_handle.node_id)))
              })?
            };
            let new_parent = match new_parent {
              Ok(v) => v,
              Err(err) => return Err(self.dom_error_to_vm_error(vm, scope, err)),
            };

            if let Some(parent_id) = new_parent {
              let parent_wrapper = {
                let platform = require_dom_platform_mut(vm)?;
                platform.get_existing_wrapper_for_document_id(scope.heap(), state.document_id, parent_id)
              };
              if let Some(parent_wrapper) = parent_wrapper {
                self.sync_cached_child_nodes_for_wrapper(vm, scope, parent_wrapper, parent_id, state.document_id)?;
              }
            }
            if let Some(old_parent_id) = old_parent {
              if !(old_parent_id.document_id == state.document_id && Some(old_parent_id.node_id) == new_parent) {
                let wrapper = {
                  let platform = require_dom_platform_mut(vm)?;
                  platform.get_existing_wrapper_for_document_id(
                    scope.heap(),
                    old_parent_id.document_id,
                    old_parent_id.node_id,
                  )
                };
                if let Some(wrapper) = wrapper {
                  self.sync_cached_child_nodes_for_wrapper(
                    vm,
                    scope,
                    wrapper,
                    old_parent_id.node_id,
                    old_parent_id.document_id,
                  )?;
                }
              }
            }
            if node_is_fragment_like {
              let wrapper_obj = match node_val {
                Value::Object(obj) => Some(obj),
                _ => None,
              };
              let wrapper = wrapper_obj.or_else(|| {
                let platform = require_dom_platform_mut(vm).ok()?;
                platform.get_existing_wrapper_for_document_id(scope.heap(), node_handle.document_id, node_handle.node_id)
              });
              if let Some(wrapper) = wrapper {
                self.sync_cached_child_nodes_for_wrapper(
                  vm,
                  scope,
                  wrapper,
                  node_handle.node_id,
                  node_handle.document_id,
                )?;
              }
            }
            for node_id in ancestors {
              let wrapper = {
                let platform = require_dom_platform_mut(vm)?;
                platform.get_existing_wrapper_for_document_id(scope.heap(), state.document_id, node_id)
              };
              if let Some(wrapper) = wrapper {
                self.sync_cached_child_nodes_for_wrapper(vm, scope, wrapper, node_id, state.document_id)?;
              }
            }
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("Range", "surroundContents", 0) => {
        let state = self.require_range_state(receiver)?;

        let new_parent_val = args.get(0).copied().unwrap_or(Value::Undefined);
        let new_parent_handle = {
          let platform = require_dom_platform_mut(vm)?;
          platform.require_node_handle(scope.heap(), new_parent_val)?
        };
        if new_parent_handle.document_id != state.document_id {
          let (range_is_owned, node_is_owned) = {
            let mut range_is_owned = false;
            let mut node_is_owned = false;
            if let Some(data) = vm.user_data::<WindowRealmUserData>() {
              range_is_owned = data
                .with_owned_dom2_document(state.document_id, |_| ())
                .is_some();
              node_is_owned = data
                .with_owned_dom2_document(new_parent_handle.document_id, |_| ())
                .is_some();
            }
            (range_is_owned, node_is_owned)
          };
          if range_is_owned || node_is_owned {
            return Err(self.dom_error_to_vm_error(vm, scope, DomError::WrongDocumentError));
          }
        }

        let owned_ancestors: Option<Result<Vec<NodeId>, DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document(state.document_id, |dom| {
              let start = dom.range_start_container(state.range_id)?;
              let end = dom.range_end_container(state.range_id)?;
              let common = dom.range_common_ancestor_container(state.range_id)?;

              let mut out: HashSet<NodeId> = HashSet::new();
              let mut n = start;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              let mut n = end;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              Ok(out.into_iter().collect())
            })
          });

        let ancestors: Result<Vec<NodeId>, DomError> = if let Some(result) = owned_ancestors {
          result
        } else {
          self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              let start = dom.range_start_container(state.range_id)?;
              let end = dom.range_end_container(state.range_id)?;
              let common = dom.range_common_ancestor_container(state.range_id)?;

              let mut out: HashSet<NodeId> = HashSet::new();
              let mut n = start;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              let mut n = end;
              let mut remaining = dom.nodes_len() + 1;
              loop {
                out.insert(n);
                if n == common || remaining == 0 {
                  break;
                }
                remaining -= 1;
                let Some(parent) = dom.tree_parent_node(n) else {
                  break;
                };
                n = parent;
              }
              Ok(out.into_iter().collect())
            }))
          })?
        };

        let ancestors = match ancestors {
          Ok(v) => v,
          Err(err) => return Err(self.dom_error_to_vm_error(vm, scope, err)),
        };

        // Snapshot the wrapper's old parent (before insertion) so we can keep its `childNodes` live
        // NodeList in sync even when the wrapper is adopted into a different document id.
        let owned_old_parent: Option<Result<Option<DomNodeKey>, DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document(state.document_id, |dom| {
              let old_parent = dom.parent(new_parent_handle.node_id)?;
              Ok(old_parent.map(|id| DomNodeKey::new(new_parent_handle.document_id, id)))
            })
          });
        let old_parent: Result<Option<DomNodeKey>, DomError> = if let Some(result) = owned_old_parent {
          result
        } else {
          self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              let old_parent = dom.parent(new_parent_handle.node_id)?;
              Ok(old_parent.map(|id| DomNodeKey::new(new_parent_handle.document_id, id)))
            }))
          })?
        };
        let old_parent = match old_parent {
          Ok(v) => v,
          Err(err) => return Err(self.dom_error_to_vm_error(vm, scope, err)),
        };

        let adopt_mapping: Option<(DocumentId, HashMap<NodeId, NodeId>)> =
          if new_parent_handle.document_id == state.document_id {
            None
          } else {
            let mut mapping: HashMap<NodeId, NodeId> = HashMap::new();
            mapping.insert(new_parent_handle.node_id, new_parent_handle.node_id);
            Some((new_parent_handle.document_id, mapping))
          };

        let owned_result: Option<Result<(), DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document_mut(state.document_id, |dom| {
              dom.range_surround_contents(state.range_id, new_parent_handle.node_id)
            })
          });

        let result = if let Some(result) = owned_result {
          result
        } else {
          self.with_dom_host(vm, |host| {
            Ok(host.mutate_dom(|dom| match dom.range_surround_contents(state.range_id, new_parent_handle.node_id) {
              Ok(()) => (Ok(()), true),
              Err(err) => (Err(err), false),
            }))
          })?
        };

        match result {
          Ok(()) => {
            if let Some((old_document_id, mapping)) = adopt_mapping {
              require_dom_platform_mut(vm)?.remap_node_ids_between_documents(
                scope.heap_mut(),
                old_document_id,
                state.document_id,
                &mapping,
              )?;
            }

            // Sync the wrapper's own cached childNodes (its children were replaced by the extracted fragment).
            let wrapper_obj = match new_parent_val {
              Value::Object(obj) => Some(obj),
              _ => None,
            };
            if let Some(wrapper_obj) = wrapper_obj {
              self.sync_cached_child_nodes_for_wrapper(
                vm,
                scope,
                wrapper_obj,
                new_parent_handle.node_id,
                state.document_id,
              )?;
            }

            let owned_new_parent_parent: Option<Result<Option<NodeId>, DomError>> = vm
              .user_data_mut::<WindowRealmUserData>()
              .and_then(|data| {
                data.with_owned_dom2_document(state.document_id, |dom| dom.parent(new_parent_handle.node_id))
              });
            let new_parent_parent: Result<Option<NodeId>, DomError> = if let Some(result) = owned_new_parent_parent {
              result
            } else {
              self.with_dom_host(vm, |host| {
                Ok(host.with_dom(|dom| dom.parent(new_parent_handle.node_id)))
              })?
            };
            let new_parent_parent = match new_parent_parent {
              Ok(v) => v,
              Err(err) => return Err(self.dom_error_to_vm_error(vm, scope, err)),
            };

            if let Some(parent_id) = new_parent_parent {
              let parent_wrapper = {
                let platform = require_dom_platform_mut(vm)?;
                platform.get_existing_wrapper_for_document_id(scope.heap(), state.document_id, parent_id)
              };
              if let Some(parent_wrapper) = parent_wrapper {
                self.sync_cached_child_nodes_for_wrapper(vm, scope, parent_wrapper, parent_id, state.document_id)?;
              }
            }

            if let Some(old_parent_id) = old_parent {
              if !(old_parent_id.document_id == state.document_id && Some(old_parent_id.node_id) == new_parent_parent) {
                let wrapper = {
                  let platform = require_dom_platform_mut(vm)?;
                  platform.get_existing_wrapper_for_document_id(
                    scope.heap(),
                    old_parent_id.document_id,
                    old_parent_id.node_id,
                  )
                };
                if let Some(wrapper) = wrapper {
                  self.sync_cached_child_nodes_for_wrapper(
                    vm,
                    scope,
                    wrapper,
                    old_parent_id.node_id,
                    old_parent_id.document_id,
                  )?;
                }
              }
            }

            for node_id in ancestors {
              let wrapper = {
                let platform = require_dom_platform_mut(vm)?;
                platform.get_existing_wrapper_for_document_id(scope.heap(), state.document_id, node_id)
              };
              if let Some(wrapper) = wrapper {
                self.sync_cached_child_nodes_for_wrapper(vm, scope, wrapper, node_id, state.document_id)?;
              }
            }
            self.sync_live_html_collections(vm, scope)?;
            Ok(Value::Undefined)
          }
          Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
        }
      }

      ("Range", "detach", 0) => Ok(Value::Undefined),

      (
        "AbstractRange",
        op @ ("startContainer" | "startOffset" | "endContainer" | "endOffset" | "collapsed"),
        0,
      ) => {
        let range_obj = Self::require_receiver_object(receiver)?;
        scope.push_root(Value::Object(range_obj))?;

        // Support `StaticRange` instances created by the handwritten constructor (used by the
        // handwritten DOM backend and as a fallback in WebIDL mode until bindings are generated).
        //
        // Those objects store their boundary points on own non-configurable data properties and are
        // branded via `STATIC_RANGE_BRAND_KEY`.
        let brand_key = key_from_str(scope, STATIC_RANGE_BRAND_KEY)?;
        let is_static_range = matches!(
          scope
            .heap()
            .object_get_own_data_property_value(range_obj, &brand_key)?,
          Some(Value::Bool(true))
        );
        if is_static_range {
          let start_container_key = key_from_str(scope, STATIC_RANGE_START_CONTAINER_KEY)?;
          let start_container = scope
            .heap()
            .object_get_own_data_property_value(range_obj, &start_container_key)?
            .ok_or(VmError::TypeError("Illegal invocation"))?;
          let start_offset_key = key_from_str(scope, STATIC_RANGE_START_OFFSET_KEY)?;
          let start_offset = scope
            .heap()
            .object_get_own_data_property_value(range_obj, &start_offset_key)?
            .ok_or(VmError::TypeError("Illegal invocation"))?;
          let end_container_key = key_from_str(scope, STATIC_RANGE_END_CONTAINER_KEY)?;
          let end_container = scope
            .heap()
            .object_get_own_data_property_value(range_obj, &end_container_key)?
            .ok_or(VmError::TypeError("Illegal invocation"))?;
          let end_offset_key = key_from_str(scope, STATIC_RANGE_END_OFFSET_KEY)?;
          let end_offset = scope
            .heap()
            .object_get_own_data_property_value(range_obj, &end_offset_key)?
            .ok_or(VmError::TypeError("Illegal invocation"))?;

          return match op {
            "startContainer" => Ok(start_container),
            "startOffset" => Ok(start_offset),
            "endContainer" => Ok(end_container),
            "endOffset" => Ok(end_offset),
            "collapsed" => Ok(Value::Bool(
              start_container == end_container && start_offset == end_offset,
            )),
            _ => Err(VmError::TypeError("AbstractRange operation mismatch")),
          };
        }

        let state = self
          .ranges
          .get(&WeakGcObject::from(range_obj))
          .copied()
          .ok_or(VmError::TypeError("Illegal invocation"))?;

        // Fast path: return offsets/collapsed without allocating wrappers.
        if op == "startOffset" || op == "endOffset" || op == "collapsed" {
          let owned_result: Option<Result<Value, DomError>> = vm
            .user_data_mut::<WindowRealmUserData>()
            .and_then(|data| {
              data.with_owned_dom2_document(state.document_id, |dom| match op {
                "startOffset" => dom
                  .range_start_offset(state.range_id)
                  .map(|v| Value::Number(v as f64)),
                "endOffset" => dom
                  .range_end_offset(state.range_id)
                  .map(|v| Value::Number(v as f64)),
                "collapsed" => {
                  let start = dom.range_start(state.range_id)?;
                  let end = dom.range_end(state.range_id)?;
                  Ok(Value::Bool(start == end))
                }
                _ => Err(DomError::NotFoundError),
              })
            });

          let value = if let Some(result) = owned_result {
            result
          } else {
            self.with_dom_host(vm, |host| {
              Ok(host.with_dom(|dom| match op {
                "startOffset" => dom.range_start_offset(state.range_id).map(|v| Value::Number(v as f64)),
                "endOffset" => dom.range_end_offset(state.range_id).map(|v| Value::Number(v as f64)),
                "collapsed" => {
                  let start = dom.range_start(state.range_id)?;
                  let end = dom.range_end(state.range_id)?;
                  Ok(Value::Bool(start == end))
                }
                _ => Err(DomError::NotFoundError),
              }))
            })?
          };

          return match value {
            Ok(v) => Ok(v),
            Err(err) => Err(self.dom_error_to_vm_error(vm, scope, err)),
          };
        }

        // Container getters: resolve node + primary interface, then allocate/create wrapper.
        let owned_result: Option<Result<(NodeId, DomInterface), DomError>> = vm
          .user_data_mut::<WindowRealmUserData>()
          .and_then(|data| {
            data.with_owned_dom2_document(state.document_id, |dom| {
              let node_id = if op == "startContainer" {
                dom.range_start_container(state.range_id)?
              } else {
                dom.range_end_container(state.range_id)?
              };
              let primary = DomInterface::primary_for_node_kind(&dom.node(node_id).kind);
              Ok((node_id, primary))
            })
          });

        let (node_id, primary_interface) = if let Some(result) = owned_result {
          match result {
            Ok(v) => v,
            Err(err) => return Err(self.dom_error_to_vm_error(vm, scope, err)),
          }
        } else {
          self.with_dom_host(vm, |host| {
            Ok(host.with_dom(|dom| {
              let node_id = if op == "startContainer" {
                dom.range_start_container(state.range_id)?
              } else {
                dom.range_end_container(state.range_id)?
              };
              let primary = DomInterface::primary_for_node_kind(&dom.node(node_id).kind);
              Ok((node_id, primary))
            }))
          })?
          .map_err(|err: DomError| self.dom_error_to_vm_error(vm, scope, err))?
        };

        let wrapper = require_dom_platform_mut(vm)?.get_or_create_wrapper_for_document_id(
          scope,
          state.document_id,
          node_id,
          primary_interface,
        )?;
        scope.push_root(Value::Object(wrapper))?;
        Ok(Value::Object(wrapper))
      }

      ("Window", "alert", _) => Ok(Value::Undefined),
      ("Window", "queueMicrotask", 0) => {
        let callback = args.get(0).copied().unwrap_or(Value::Undefined);
        self.queue_microtask_impl(vm, scope, callback)
      }
      ("Window", "setTimeout", 0) => self.set_timeout_impl(vm, scope, args),
      ("Window", "setInterval", 0) => self.set_interval_impl(vm, scope, args),
      ("Window", "clearTimeout", 0) => {
        let id = normalize_timer_id(args.get(0).copied().unwrap_or(Value::Number(0.0)));
        self.clear_timer_impl(vm, scope, id, false)
      }
      ("Window", "clearInterval", 0) => {
        let id = normalize_timer_id(args.get(0).copied().unwrap_or(Value::Number(0.0)));
        self.clear_timer_impl(vm, scope, id, true)
      }

      // WHATWG DOM: Range.prototype.detach() is legacy and specified as a no-op.
      // WPT uses it for "detached" range setup.
      ("Range", "detach", 0) => Ok(Value::Undefined),

      _ => {
        if let Some(value) = self.try_delegate_dom_call_operation(
          vm, scope, receiver, interface, operation, overload, args,
        )? {
          Ok(value)
        } else {
          let has_receiver = receiver.is_some();
          Err(VmError::Unimplemented(Box::leak(
            format!(
              "WebIDL binding dispatch not implemented: {interface}.{operation} (overload {overload}, receiver={has_receiver})"
            )
            .into_boxed_str(),
          )))
        }
      }
    }
  }

  fn call_constructor(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    interface: &'static str,
    overload: usize,
    args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    // Compatibility shim: generated vm-js WebIDL bindings historically dispatched constructors via
    // `call_operation(interface, "constructor", ...)`. Forward `call_constructor` to that path so
    // newer bindings can call into the same host implementation.
    //
    // Note: `new_target` is currently ignored because the existing host dispatch does not use it
    // (and older generated bindings never supplied it).
    self.call_operation(vm, scope, None, interface, "constructor", overload, args)
  }

  fn iterable_snapshot(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    receiver: Option<Value>,
    interface: &'static str,
    kind: IterableKind,
  ) -> Result<Vec<BindingValue>, VmError> {
    self.maybe_sweep(vm, scope.heap_mut());

    match interface {
      "URLSearchParams" => {
        let params = self.require_params(receiver)?;
        let pairs = params
          .pairs()
          .map_err(url_search_params_error_to_vm_error)?;
        let mut out: Vec<BindingValue> = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
          match kind {
            IterableKind::Entries => out.push(BindingValue::Sequence(vec![
              BindingValue::RustString(k),
              BindingValue::RustString(v),
            ])),
            IterableKind::Keys => out.push(BindingValue::RustString(k)),
            IterableKind::Values => out.push(BindingValue::RustString(v)),
          }
        }
        Ok(out)
      }
      "DOMTokenList" => {
        let (element_id, _obj) = require_dom_token_list_receiver(scope, receiver)?;
        let tokens: Result<Vec<String>, DomError> =
          self.with_dom_host(vm, |host| Ok(host.class_list_tokens(element_id)))?;
        let tokens = match tokens {
          Ok(tokens) => tokens,
          Err(err) => {
            let class = self.dom_exception_class_for_realm(vm, scope)?;
            return Err(throw_dom_error(scope, class, err));
          }
        };

        let mut out: Vec<BindingValue> = Vec::with_capacity(tokens.len());
        for (idx, token) in tokens.into_iter().enumerate() {
          match kind {
            IterableKind::Values => out.push(BindingValue::RustString(token)),
            IterableKind::Keys => out.push(BindingValue::Number(idx as f64)),
            IterableKind::Entries => out.push(BindingValue::Sequence(vec![
              BindingValue::Number(idx as f64),
              BindingValue::RustString(token),
            ])),
          }
        }
        Ok(out)
      }
      "NodeList" | "HTMLCollection" => {
        let Some(Value::Object(obj)) = receiver else {
          // Preserve existing delegation behavior for call sites that request a snapshot without an
          // explicit receiver (primarily tests that validate delegation mechanics).
          if let Some(values) =
            self.try_delegate_dom_iterable_snapshot(vm, scope, receiver, interface, kind)?
          {
            return Ok(values);
          }
          return Err(VmError::TypeError("Illegal invocation"));
        };
        scope.push_root(Value::Object(obj))?;

        let length_key = key_from_str(scope, COLLECTION_LENGTH_KEY)?;
        let len = match scope
          .heap()
          .object_get_own_data_property_value(obj, &length_key)?
        {
          Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n.trunc() as usize,
          _ => 0,
        };

        let mut out: Vec<BindingValue> = Vec::with_capacity(len);
        for idx in 0..len {
          match kind {
            IterableKind::Keys => out.push(BindingValue::Number(idx as f64)),
            IterableKind::Values | IterableKind::Entries => {
              let idx_key = key_from_str(scope, &idx.to_string())?;
              let value = scope
                .heap()
                .object_get_own_data_property_value(obj, &idx_key)?
                .filter(|v| !matches!(v, Value::Undefined))
                .unwrap_or(Value::Null);
              let value = match value {
                Value::Null => BindingValue::Null,
                other => BindingValue::Object(other),
              };
              if matches!(kind, IterableKind::Values) {
                out.push(value);
              } else {
                // `kind` is either Values or Entries in this branch. Avoid `unreachable!()` so this
                // helper cannot panic even if future refactors widen the match above.
                out.push(BindingValue::Sequence(vec![
                  BindingValue::Number(idx as f64),
                  value,
                ]));
              }
            }
          }
        }
        Ok(out)
      }
      _ => {
        if let Some(values) =
          self.try_delegate_dom_iterable_snapshot(vm, scope, receiver, interface, kind)?
        {
          Ok(values)
        } else {
          Err(VmError::TypeError(Box::leak(
            format!("unimplemented host iterable snapshot: {interface} ({kind:?})")
              .into_boxed_str(),
          )))
        }
      }
    }
  }
}

#[cfg(test)]
mod element_replace_with_tests {
  use super::*;
  use crate::js::window_realm::{DomBindingsBackend, WindowRealm, WindowRealmConfig};
  use crate::js::window_timers::VmJsEventLoopHooks;
  use crate::js::{DocumentHostState, WindowHostState};
  use vm_js::Value;

  #[test]
  fn element_replace_with_inserts_text_and_node_in_order() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);

    let mut window = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = window.global_object();

    let mut webidl_host = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      r#"
        (() => {
          document.body.innerHTML = '<div id="root"><span id="a"></span></div>';
          document.getElementById('a').replaceWith('x', document.createElement('b'));
          return document.getElementById('a') === null
            && document.getElementById('root').innerHTML === 'x<b></b>';
        })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_replace_with_converts_non_node_object_via_to_string() -> Result<(), VmError> {
    let dom =
      crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);

    let mut window = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = window.global_object();

    let mut webidl_host = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      r#"
        (() => {
          document.body.innerHTML = '<div id="root"><span id="a"></span></div>';
          document.getElementById('a').replaceWith({ toString() { return 'x' } });
          return document.getElementById('a') === null
            && document.getElementById('root').innerHTML === 'x';
        })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_replace_with_self_is_noop() -> Result<(), VmError> {
    let dom =
      crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);

    let mut window = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = window.global_object();

    let mut webidl_host = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      r#"
        (() => {
          document.body.innerHTML = '<div id="root"><span id="a"></span><span id="b"></span></div>';
          const a = document.getElementById('a');
          a.replaceWith(a);
          return document.getElementById('root').innerHTML === '<span id="a"></span><span id="b"></span>';
        })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_replace_with_updates_viable_next_sibling_when_it_is_moved() -> Result<(), VmError> {
    let dom =
      crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);

    let mut window = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = window.global_object();

    let mut webidl_host = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      r#"
        (() => {
          document.body.innerHTML = '<div id="root"><span id="a"></span><span id="b"></span><span id="c"></span></div>';
          const a = document.getElementById('a');
          const b = document.getElementById('b');
          a.replaceWith(b, a);
          return document.getElementById('root').innerHTML === '<span id="b"></span><span id="a"></span><span id="c"></span>';
        })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_replace_with_reads_parent_after_argument_to_string_side_effects() -> Result<(), VmError> {
    let dom =
      crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);

    let mut window = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = window.global_object();

    let mut webidl_host = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      r#"
        (() => {
          document.body.innerHTML = '<div id="root"><span id="a"></span></div>';
          const root = document.getElementById('root');
          const a = document.getElementById('a');
          a.replaceWith({
            toString() {
              // If `replaceWith` reads `parent` before argument conversion, this removal will cause a
              // later DOM mutation to fail (NotFoundError). Spec: argument conversion happens first.
              root.innerHTML = '';
              return 'x';
            }
          });
          return document.getElementById('a') === null && root.innerHTML === '';
        })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_replace_with_adopts_foreign_nodes_in_webidl_dom_backend() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut window = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = window.global_object();

    let mut webidl_host = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      r#"
        (() => {
          document.body.innerHTML = '<div id="root"><span id="a"></span></div>';
          const root = document.getElementById('root');
          const a = document.getElementById('a');
          const doc2 = new DOMParser().parseFromString('<!doctype html><p>hi</p>', 'text/html');
          const foreign = doc2.createElement('p');
          foreign.appendChild(doc2.createTextNode('hello'));
          if (foreign.ownerDocument !== doc2) return false;
          a.replaceWith(foreign);
          return document.getElementById('a') === null
            && foreign.parentNode === root
            && foreign.ownerDocument === document
            && foreign.firstChild.ownerDocument === document;
        })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_replace_with_adopts_foreign_fragment_children_but_not_fragment_itself_in_webidl_dom_backend(
  ) -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut window = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = window.global_object();

    let mut webidl_host = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      r#"
        (() => {
          document.body.innerHTML = '<div id="root"><span id="a"></span></div>';
          const root = document.getElementById('root');
          const a = document.getElementById('a');
          const doc2 = new DOMParser().parseFromString('<!doctype html><p>hi</p>', 'text/html');
          const frag = doc2.createDocumentFragment();
          const list = frag.childNodes;
          const foreign = doc2.createElement('p');
          foreign.appendChild(doc2.createTextNode('hello'));
          frag.appendChild(foreign);
          if (frag.ownerDocument !== doc2) return false;
          if (list.length !== 1 || list.item(0) !== foreign) return false;
          a.replaceWith(frag);
          return document.getElementById('a') === null
            && foreign.parentNode === root
            && foreign.ownerDocument === document
            && foreign.firstChild.ownerDocument === document
            && frag.ownerDocument === doc2
            && frag.childNodes.length === 0
            && list.length === 0;
        })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }
}

#[cfg(test)]
mod cross_document_insertion_tests {
  use super::*;
  use crate::js::window_realm::{DomBindingsBackend, WindowRealm, WindowRealmConfig};
  use crate::js::window_timers::VmJsEventLoopHooks;
  use crate::js::{DocumentHostState, WindowHostState};
  use vm_js::Value;

  fn run_and_get_string(script: &str) -> Result<String, VmError> {
    let dom =
      crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);

    let mut window = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = window.global_object();

    let mut webidl_host = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(&mut doc_host, &mut hooks, script)?;
    match out {
      Value::String(s) => Ok(window.heap().get_string(s)?.to_utf8_lossy()),
      _ => Err(VmError::TypeError("expected string")),
    }
  }

  #[test]
  fn node_append_child_adopts_cross_document_subtree_and_preserves_wrapper_identity(
  ) -> Result<(), VmError> {
    let out = run_and_get_string(
      r#"
      (() => {
        const doc2 = Object.create(document);
        const foreign = doc2.createElement('b');
        const text = doc2.createTextNode('x');
        if (foreign.ownerDocument !== doc2) return 'foreign_owner_document_before';
        if (text.ownerDocument !== doc2) return 'text_owner_document_before';
        foreign.appendChild(text);

        const returned = document.body.appendChild(foreign);
        if (returned !== foreign) return 'returned_identity';
        if (foreign.ownerDocument !== document) return 'foreign_owner_document_after';
        if (text.ownerDocument !== document) return 'text_owner_document_after';
        if (foreign.firstChild !== text) return 'text_identity';
        return 'ok';
      })()
      "#,
    )?;
    assert_eq!(out, "ok");
    Ok(())
  }

  #[test]
  fn node_insert_before_adopts_cross_document_subtree_and_preserves_wrapper_identity(
  ) -> Result<(), VmError> {
    let out = run_and_get_string(
      r#"
      (() => {
        const doc2 = Object.create(document);
        const foreign = doc2.createElement('b');
        const text = doc2.createTextNode('x');
        if (foreign.ownerDocument !== doc2) return 'foreign_owner_document_before';
        if (text.ownerDocument !== doc2) return 'text_owner_document_before';
        foreign.appendChild(text);

        const ref = document.createElement('i');
        document.body.appendChild(ref);

        const returned = document.body.insertBefore(foreign, ref);
        if (returned !== foreign) return 'returned_identity';
        if (document.body.firstChild !== foreign) return 'inserted_position';
        if (foreign.ownerDocument !== document) return 'foreign_owner_document_after';
        if (text.ownerDocument !== document) return 'text_owner_document_after';
        if (foreign.firstChild !== text) return 'text_identity';
        return 'ok';
      })()
      "#,
    )?;
    assert_eq!(out, "ok");
    Ok(())
  }

  #[test]
  fn node_replace_child_adopts_cross_document_subtree_and_preserves_wrapper_identity(
  ) -> Result<(), VmError> {
    let out = run_and_get_string(
      r#"
      (() => {
        const doc2 = Object.create(document);
        const foreign = doc2.createElement('b');
        const text = doc2.createTextNode('x');
        foreign.appendChild(text);

        const old = document.createElement('i');
        document.body.appendChild(old);

        const returned = document.body.replaceChild(foreign, old);
        if (returned !== old) return 'returned_old_identity';
        if (document.body.firstChild !== foreign) return 'inserted_position';
        if (old.parentNode !== null) return 'old_still_has_parent';
        if (foreign.ownerDocument !== document) return 'foreign_owner_document_after';
        if (text.ownerDocument !== document) return 'text_owner_document_after';
        if (foreign.firstChild !== text) return 'text_identity';
        return 'ok';
      })()
      "#,
    )?;
    assert_eq!(out, "ok");
    Ok(())
  }

  #[test]
  fn node_append_child_document_fragment_preserves_fragment_identity_and_adopts_children(
  ) -> Result<(), VmError> {
    let out = run_and_get_string(
      r#"
      (() => {
        const doc2 = Object.create(document);
        const frag = doc2.createDocumentFragment();
        const child = doc2.createElement('b');
        const text = doc2.createTextNode('x');
        child.appendChild(text);
        frag.appendChild(child);

        if (frag.ownerDocument !== doc2) return 'frag_owner_document_before';
        if (child.ownerDocument !== doc2) return 'child_owner_document_before';

        const returned = document.body.appendChild(frag);
        if (returned !== frag) return 'returned_fragment_identity';
        if (frag.ownerDocument !== doc2) return 'frag_owner_document_after';
        if (frag.firstChild !== null) return 'fragment_not_empty';

        if (document.body.firstChild !== child) return 'child_not_inserted';
        if (child.ownerDocument !== document) return 'child_owner_document_after';
        if (text.ownerDocument !== document) return 'text_owner_document_after';
        if (child.firstChild !== text) return 'text_identity';
        return 'ok';
      })()
      "#,
    )?;
    assert_eq!(out, "ok");
    Ok(())
  }

  #[test]
  fn element_insert_adjacent_element_adopts_cross_document_node_and_preserves_wrapper_identity(
  ) -> Result<(), VmError> {
    let out = run_and_get_string(
      r#"
      (() => {
        const doc2 = Object.create(document);
        const el = doc2.createElement('b');
        const text = doc2.createTextNode('x');
        el.appendChild(text);

        const inserted = document.body.insertAdjacentElement('beforeend', el);
        if (inserted !== el) return 'returned_identity';
        if (document.body.lastChild !== el) return 'not_inserted';
        if (el.ownerDocument !== document) return 'el_owner_document_after';
        if (text.ownerDocument !== document) return 'text_owner_document_after';
        if (el.firstChild !== text) return 'text_identity';
        return 'ok';
      })()
      "#,
    )?;
    assert_eq!(out, "ok");
    Ok(())
  }
}

#[cfg(test)]
mod url_search_params_init_pair_length_tests {
  use super::*;
  use crate::js::window_realm::{DomBindingsBackend, WindowRealm, WindowRealmConfig};
  use crate::js::window_timers::VmJsEventLoopHooks;
  use crate::js::{DocumentHostState, WindowHostState};
  use vm_js::Value;

  #[test]
  fn url_search_params_init_pair_length_must_be_exactly_two() -> Result<(), VmError> {
    let dom =
      crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);

    let mut window = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = window.global_object();

    let mut webidl_host = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      r#"
        (() => {
          const isTypeError = (fn) => {
            try {
              fn();
              return false;
            } catch (e) {
              return e && e.name === 'TypeError';
            }
          };
          return isTypeError(() => new URLSearchParams([['a']]))
            && isTypeError(() => new URLSearchParams([['a', 'b', 'c']]));
        })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }
}

#[cfg(test)]
mod window_document_tests {
  use super::*;
  use crate::dom2;
  use crate::dom2::DomError;
  use crate::js::window_realm::DomBindingsBackend;
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use crate::js::window_timers::VmJsEventLoopHooks;
  use crate::js::{DocumentHostState, WindowHostState};
  use selectors::context::QuirksMode;
  use vm_js::{
    GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value,
    Vm, VmError, VmHost, VmHostHooks, VmOptions,
  };
  use webidl_vm_js::{host_from_hooks, VmJsHostHooksPayload};

  fn window_document_getter_native(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let bindings_host = host_from_hooks(hooks)?;
    bindings_host.call_operation(vm, scope, None, "Window", "document", 0, &[])
  }

  #[test]
  fn vmjs_webidl_window_document_global_getter_returns_realm_document() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let mut dummy_vm_host = ();

    let mut webidl_host =
      VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(window.global_object());

    // WindowRealm eagerly installs `document` as a data property. Delete it so the WebIDL bindings
    // installer (or our test fallback) installs an accessor that routes through host dispatch.
    {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      let Some(document_obj) = vm
        .user_data_mut::<crate::js::window_realm::WindowRealmUserData>()
        .and_then(|data| data.document_obj())
      else {
        return Err(VmError::TypeError(
          "expected WindowRealm to cache a document object",
        ));
      };

      let mut scope = heap.scope();
      let global = realm.global_object();
      scope.push_root(Value::Object(global))?;
      scope.push_root(Value::Object(document_obj))?;

      // Keep the document object alive across the `document` delete + bindings installation (both
      // can allocate and therefore GC).
      let keepalive_key = key_from_str(&mut scope, "__fastrender_document_keepalive")?;
      scope.define_property(
        global,
        keepalive_key,
        data_property(Value::Object(document_obj), true, false, true),
      )?;

      let document_key = key_from_str(&mut scope, "document")?;
      scope.delete_property_or_throw(global, document_key)?;
    }

    // Install the generated vm-js bindings. (As of some revisions, Window.document is not yet
    // generated; the fallback below installs a minimal getter.)
    {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      crate::js::bindings::install_window_bindings_vm_js(vm, heap, realm)?;
    }

    // If the generated bindings didn't install Window.document yet, install a minimal accessor that
    // matches the vm-js codegen calling convention (receiver = None for global interface members).
    {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      scope.push_root(Value::Object(global))?;

      let document_key = key_from_str(&mut scope, "document")?;
      let has_document = scope
        .heap()
        .object_get_own_property(global, &document_key)?
        .is_some();

      if !has_document {
        let get_id = vm.register_native_call(window_document_getter_native)?;
        let name = scope.alloc_string("get document")?;
        scope.push_root(Value::String(name))?;
        let get_func = scope.alloc_native_function(get_id, None, name, 0)?;
        scope
          .heap_mut()
          .object_set_prototype(get_func, Some(realm.intrinsics().function_prototype()))?;
        scope.push_root(Value::Object(get_func))?;

        scope.define_property(
          global,
          document_key,
          PropertyDescriptor {
            enumerable: true,
            configurable: true,
            kind: PropertyKind::Accessor {
              get: Value::Object(get_func),
              set: Value::Undefined,
            },
          },
        )?;
      }
    }

    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut dummy_vm_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut dummy_vm_host,
      &mut hooks,
      "typeof document === 'object' && document === globalThis.__fastrender_document_keepalive",
    )?;
    assert_eq!(out, Value::Bool(true));

    Ok(())
  }

  #[test]
  fn webidl_dom_token_list_supports_throws_for_class_list() -> Result<(), VmError> {
    let config = WindowRealmConfig::new("https://example.invalid/")
      .with_dom_bindings_backend(DomBindingsBackend::WebIdl);
    let mut window = WindowRealm::new(config)?;
    let mut dom_host = DocumentHostState::new(dom2::Document::new(QuirksMode::NoQuirks));
    let mut webidl_host =
      VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(window.global_object());

    // Ensure DOMTokenList bindings (and `DOMTokenList.prototype.supports`) exist in the realm.
    {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      crate::js::bindings::install_window_bindings_vm_js(vm, heap, realm)?;
    }

    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut dom_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      "typeof document.createElement('div').classList.supports === 'function'",
    )?;
    assert_eq!(out, Value::Bool(true));

    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      "(() => { try { document.createElement('div').classList.supports('x'); return false; } catch (e) { return e instanceof TypeError; } })()",
    )?;
    assert_eq!(out, Value::Bool(true));

    Ok(())
  }

  fn make_webidl_window_dom_host_and_dispatch(
  ) -> Result<
    (
      WindowRealm,
      DocumentHostState,
      VmJsWebIdlBindingsHostDispatch<WindowHostState>,
    ),
    VmError,
  > {
    let config = WindowRealmConfig::new("https://example.invalid/")
      .with_dom_bindings_backend(DomBindingsBackend::WebIdl);
    let mut window = WindowRealm::new(config)?;
    let dom_host = DocumentHostState::new(dom2::Document::new(QuirksMode::NoQuirks));
    // Ensure WebIDL-generated DOM collection constructors/prototypes are present.
    {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      crate::js::bindings::install_window_bindings_vm_js(vm, heap, realm)?;
    }
    let dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(window.global_object());
    Ok((window, dom_host, dispatch))
  }

  #[test]
  fn webidl_document_alias_wrapper_create_element_uses_alias_owner_document() -> Result<(), VmError> {
    let (mut window, mut dom_host, mut webidl_host) = make_webidl_window_dom_host_and_dispatch()?;
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut dom_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      "(() => { const doc2 = Object.create(document); const el = doc2.createElement('b'); return el.ownerDocument === doc2; })()",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn webidl_document_alias_wrapper_create_document_fragment_uses_alias_owner_document(
  ) -> Result<(), VmError> {
    let (mut window, mut dom_host, mut webidl_host) = make_webidl_window_dom_host_and_dispatch()?;
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut dom_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      "(() => { const doc2 = Object.create(document); const frag = doc2.createDocumentFragment(); return frag.ownerDocument === doc2; })()",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn webidl_document_alias_wrapper_create_text_node_uses_alias_owner_document() -> Result<(), VmError> {
    let (mut window, mut dom_host, mut webidl_host) = make_webidl_window_dom_host_and_dispatch()?;
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut dom_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      "(() => { const doc2 = Object.create(document); const text = doc2.createTextNode('x'); return text.ownerDocument === doc2; })()",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn webidl_node_child_nodes_returns_live_nodelist() -> Result<(), VmError> {
    let (mut window, mut dom_host, mut webidl_host) = make_webidl_window_dom_host_and_dispatch()?;
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut dom_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      r#"
      (() => {
        try {
          const node = document.createElement('div');
          const list1 = node.childNodes;
          const list2 = node.childNodes;
          if (list1 !== list2) return false;
          if (!(list1 instanceof NodeList)) return false;
          if (Array.isArray(list1)) return false;
          if (list1.length !== 0) return false;
          if (list1.item(0) !== null) return false;

          const a = document.createElement('span');
          const b = document.createElement('span');
          node.appendChild(a);
           node.appendChild(b);
           if (list1.length !== 2) return false;
           if (list1.item(0) !== a) return false;
           if (list1.item(1) !== b) return false;

           if (node.removeChild(a) !== a) return false;
           if (list1.length !== 1) return false;
           if (list1.item(0) !== b) return false;
           if (node.childNodes !== list1) return false;
           return true;
         } catch (e) {
          return false;
        }
      })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn webidl_query_selector_all_returns_static_nodelist() -> Result<(), VmError> {
    let (mut window, mut dom_host, mut webidl_host) = make_webidl_window_dom_host_and_dispatch()?;
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut dom_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      r#"
      (() => {
        try {
          const root = document.createElement('div');
          root.appendChild(document.createElement('span'));
          root.appendChild(document.createElement('span'));
          const snapshot = root.querySelectorAll('span');
          if (!(snapshot instanceof NodeList)) return false;
          if (snapshot.length !== 2) return false;

          // `querySelectorAll` is static: mutations after the call should not affect the snapshot.
          root.appendChild(document.createElement('span'));
          if (snapshot.length !== 2) return false;

          // A new call observes the mutation and returns a distinct NodeList object.
          const fresh = root.querySelectorAll('span');
          if (fresh === snapshot) return false;
          if (fresh.length !== 3) return false;
          return true;
        } catch (e) {
          return false;
        }
      })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn webidl_document_fragment_query_selector_all_works() -> Result<(), VmError> {
    let (mut window, mut dom_host, mut webidl_host) = make_webidl_window_dom_host_and_dispatch()?;
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut dom_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      r#"
      (() => {
        try {
          const frag = document.createDocumentFragment();
          const a = document.createElement('span');
          a.id = "a";
          a.className = "x";
          frag.appendChild(a);
          const b = document.createElement('div');
          b.className = "x";
          frag.appendChild(b);

          if (frag.querySelector('#a') !== a) return false;

          const snapshot = frag.querySelectorAll('.x');
          if (!(snapshot instanceof NodeList)) return false;
          if (snapshot.length !== 2) return false;
          if (snapshot[0] !== a) return false;
          if (snapshot[1] !== b) return false;

          // querySelectorAll returns a static NodeList.
          const c = document.createElement('span');
          c.className = "x";
          frag.appendChild(c);
          if (snapshot.length !== 2) return false;

          let threw = false;
          try { frag.querySelector('div['); } catch (e) { threw = e && e.name === 'SyntaxError'; }
          if (!threw) return false;

          return true;
        } catch (e) {
          return false;
        }
      })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn webidl_element_children_returns_live_html_collection() -> Result<(), VmError> {
    let (mut window, mut dom_host, mut webidl_host) = make_webidl_window_dom_host_and_dispatch()?;
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut dom_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      r#"
      (() => {
        try {
          const node = document.createElement('div');
          const coll1 = node.children;
          const coll2 = node.children;
          if (coll1 !== coll2) return false;
          if (!(coll1 instanceof HTMLCollection)) return false;
          if (Array.isArray(coll1)) return false;
          if (coll1.length !== 0) return false;

          const a = document.createElement('a');
          const b = document.createElement('b');
          node.appendChild(a);
          node.appendChild(b);
          if (coll1.length !== 2) return false;
          if (coll1[0] !== a) return false;
          if (coll1.item(1) !== b) return false;

          node.removeChild(a);
          if (coll1.length !== 1) return false;
          if (coll1[0] !== b) return false;
          if (node.children !== coll1) return false;
          return true;
        } catch (e) {
          return false;
        }
      })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn webidl_live_collections_sync_after_other_dom_mutations() -> Result<(), VmError> {
    let (mut window, mut dom_host, mut webidl_host) = make_webidl_window_dom_host_and_dispatch()?;
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut dom_host,
      &mut window,
      Some(&mut webidl_host),
    );
    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      r#"
      (() => {
        try {
          const root = document.createElement('div');
          const a = document.createElement('a');
          const b = document.createElement('b');
          root.appendChild(a);
          root.appendChild(b);

           const kids = root.children;
           const nodes = root.childNodes;

           if (kids.length !== 2 || nodes.length !== 2) return false;
           if (kids[0] !== a || kids[1] !== b) return false;
           if (nodes[0] !== a || nodes[1] !== b) return false;

           const c = document.createElement('c');
           root.insertBefore(c, b);
           if (kids.length !== 3 || nodes.length !== 3) return false;
           if (kids[1] !== c || kids[2] !== b) return false;
           if (nodes[1] !== c || nodes[2] !== b) return false;

           const d = document.createElement('d');
           root.replaceChild(d, c);
           if (kids.length !== 3 || nodes.length !== 3) return false;
           if (kids[1] !== d) return false;
           if (nodes[1] !== d) return false;

           b.remove();
           if (kids.length !== 2 || nodes.length !== 2) return false;
           if (kids[0] !== a || kids[1] !== d) return false;
           if (nodes[0] !== a || nodes[1] !== d) return false;
           if (kids[2] !== undefined) return false;
           if (nodes[2] !== undefined) return false;
           if (nodes.item(2) !== null) return false;

           root.innerHTML = 't<span id="x"></span>';
           if (kids.length !== 1) return false;
           if (kids[0].id !== 'x') return false;
           if (nodes.length !== 2) return false;
           if (nodes.item(1).id !== 'x') return false;

          // Element.append / ParentNode.append can also move nodes across parents; ensure cached
          // `childNodes` NodeLists on the *old* parent are kept live.
          const p1 = document.createElement('div');
          const p2 = document.createElement('div');
          const x = document.createElement('x');
          p1.appendChild(x);
           const p1Nodes = p1.childNodes;
           const p2Nodes = p2.childNodes;
           p2.append(x);
           if (p1Nodes.length !== 0) return false;
           if (p2Nodes.length !== 1) return false;
           if (p2Nodes[0] !== x) return false;

          // `document.body` setter replaces/appends under `documentElement`. Ensure cached collections
          // on the document element stay live.
          const html = document.createElement('html');
          document.appendChild(html);
          const head = document.createElement('head');
          const body1 = document.createElement('body');
          html.appendChild(head);
          html.appendChild(body1);
           const htmlKids = html.children;
           const htmlNodes = html.childNodes;
           if (htmlKids.length !== 2 || htmlNodes.length !== 2) return false;
           if (htmlKids[1] !== body1 || htmlNodes[1] !== body1) return false;

           const body2 = document.createElement('body');
           document.body = body2;
           if (body2.parentNode !== html) return false;
           if (body1.parentNode !== null) return false;
           const gotBody = document.body;
           if (gotBody === body1) return false;
           if (gotBody === null) return false;
           if (gotBody !== body2) return false;
           if (htmlKids.length !== 2 || htmlNodes.length !== 2) return false;
           if (htmlKids[1] === body1) return false;
           if (htmlKids[1] !== body2) return false;
           if (htmlNodes[1] !== body2) return false;

           return true;
         } catch (e) {
           return false;
         }
       })()
       "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn webidl_get_elements_by_tag_name_returns_live_html_collection() -> Result<(), VmError> {
    let (mut window, mut dom_host, mut webidl_host) = make_webidl_window_dom_host_and_dispatch()?;
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut dom_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      r#"
      (() => {
        try {
          const root = document.createElement('div');
          const span = document.createElement('span');
          const coll = root.getElementsByTagName('span');
          if (!(coll instanceof HTMLCollection)) return false;
          if (Array.isArray(coll)) return false;
          if (coll.length !== 0) return false;
          if (coll.item(0) !== null) return false;

          root.appendChild(span);
          if (coll.length !== 1) return false;
          if (coll.item(0) !== span) return false;
          if (coll[0] !== span) return false;

          root.removeChild(span);
          if (coll.length !== 0) return false;
          if (coll.item(0) !== null) return false;
          return true;
        } catch (e) {
          return false;
        }
      })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn webidl_dom_token_list_class_list_methods_and_errors() -> Result<(), VmError> {
    let (mut window, mut dom_host, mut webidl_host) = make_webidl_window_dom_host_and_dispatch()?;
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut dom_host,
      &mut window,
      Some(&mut webidl_host),
    );

    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      r#"
      (() => {
        try {
          const el = document.createElement('div');
          const cl = el.classList;
          if (!(cl instanceof DOMTokenList)) return false;
          if (cl.length !== 0) return false;
          if (cl.contains('a')) return false;

          cl.add('a');
          if (!cl.contains('a')) return false;
          if (cl.length !== 1) return false;
          if (cl[0] !== 'a') return false;

          cl.add('b');
          if (cl.length !== 2) return false;
          if (cl.item(1) !== 'b') return false;

          if (cl.toggle('a') !== false) return false;
          if (cl.contains('a')) return false;
          if (cl.toggle('a') !== true) return false;
          if (!cl.contains('a')) return false;
          cl.remove('a');
          if (cl.contains('a')) return false;

          const emptyTokenThrows = (() => {
            try {
              cl.add('');
              return false;
            } catch (e) {
              return e instanceof DOMException && e.name === 'SyntaxError';
            }
          })();
          if (!emptyTokenThrows) return false;

          const whitespaceTokenThrows = (() => {
            try {
              cl.add('a b');
              return false;
            } catch (e) {
              return e instanceof DOMException && e.name === 'InvalidCharacterError';
            }
          })();
          if (!whitespaceTokenThrows) return false;

          return true;
        } catch (e) {
          return false;
        }
      })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  fn get_own_string_property(
    scope: &mut Scope<'_>,
    obj: GcObject,
    name: &str,
  ) -> Result<String, VmError> {
    // Root `obj` across string/key allocations.
    scope.push_root(Value::Object(obj))?;
    let key_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    let value = scope
      .heap()
      .object_get_own_data_property_value(obj, &key)?
      .unwrap_or(Value::Undefined);
    let Value::String(s) = value else {
      return Err(VmError::TypeError(
        "expected DOMException property to be a string",
      ));
    };
    Ok(scope.heap().get_string(s)?.to_utf8_lossy())
  }

  fn assert_dom_exception_name(
    scope: &mut Scope<'_>,
    thrown: Value,
    expected: &str,
  ) -> Result<(), VmError> {
    scope.push_root(thrown)?;
    let Value::Object(obj) = thrown else {
      return Err(VmError::TypeError(
        "expected thrown DOMException to be an object",
      ));
    };
    assert_eq!(get_own_string_property(scope, obj, "name")?, expected);
    Ok(())
  }

  #[test]
  fn vmjs_host_dispatch_throw_dom_exception_produces_object_with_name() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let global = realm.global_object();

    let mut scope = heap.scope();
    let class = dom_exception_class(&mut vm, &mut scope, global)?;

    let err = throw_dom_exception(&mut scope, class, "SyntaxError", "m");
    let VmError::Throw(thrown) = err else {
      return Err(VmError::TypeError(
        "expected throw_dom_exception to return VmError::Throw",
      ));
    };
    assert_dom_exception_name(&mut scope, thrown, "SyntaxError")?;

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn vmjs_host_dispatch_throw_dom_error_maps_code_to_dom_exception_name() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let global = realm.global_object();

    let mut scope = heap.scope();
    let class = dom_exception_class(&mut vm, &mut scope, global)?;

    let err = throw_dom_error(&mut scope, class, DomError::NotFoundError);
    let VmError::Throw(thrown) = err else {
      return Err(VmError::TypeError(
        "expected throw_dom_error to return VmError::Throw",
      ));
    };
    assert_dom_exception_name(&mut scope, thrown, "NotFoundError")?;

    let err = throw_dom_error(&mut scope, class, DomError::InvalidStateError);
    let VmError::Throw(thrown) = err else {
      return Err(VmError::TypeError(
        "expected throw_dom_error to return VmError::Throw",
      ));
    };
    assert_dom_exception_name(&mut scope, thrown, "InvalidStateError")?;

    let err = throw_dom_error(&mut scope, class, DomError::InvalidNodeTypeError);
    let VmError::Throw(thrown) = err else {
      return Err(VmError::TypeError(
        "expected throw_dom_error to return VmError::Throw",
      ));
    };
    assert_dom_exception_name(&mut scope, thrown, "InvalidNodeTypeError")?;

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }
}

#[cfg(test)]
mod document_element_accessors_tests {
  use super::*;
  use crate::js::window_realm::{DomBindingsBackend, WindowRealm, WindowRealmConfig};
  use crate::js::window_timers::VmJsEventLoopHooks;
  use crate::js::{DocumentHostState, WindowHostState};
  use vm_js::Value;
 
  fn make_window_and_dom_host() -> Result<(WindowRealm, DocumentHostState), VmError> {
    let config = WindowRealmConfig::new("https://example.invalid/")
      .with_dom_bindings_backend(DomBindingsBackend::WebIdl);
    let window = WindowRealm::new(config)?;
 
    let root = crate::dom::parse_html("<!doctype html><html><head></head><body></body></html>")
      .map_err(|_| VmError::TypeError("failed to parse HTML fixture"))?;
    let dom_host = DocumentHostState::from_renderer_dom(&root);
    Ok((window, dom_host))
  }
 
  #[test]
  fn document_element_head_body_accessors_expose_expected_elements() -> Result<(), VmError> {
    let (mut window, mut dom_host) = make_window_and_dom_host()?;
    let mut webidl_host =
      VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(window.global_object());
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut dom_host,
      &mut window,
      Some(&mut webidl_host),
    );
 
    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      r#"
      (() => {
        const de = Object.getOwnPropertyDescriptor(Document.prototype, 'documentElement').get.call(document);
        const head = Object.getOwnPropertyDescriptor(Document.prototype, 'head').get.call(document);
        const body = Object.getOwnPropertyDescriptor(Document.prototype, 'body').get.call(document);
        return de !== null && de.tagName === 'HTML'
          && head !== null && head.tagName === 'HEAD'
          && body !== null && body.tagName === 'BODY';
      })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }
 
  #[test]
  fn document_body_setter_replaces_body_element() -> Result<(), VmError> {
    let (mut window, mut dom_host) = make_window_and_dom_host()?;
    let mut webidl_host =
      VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(window.global_object());
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut dom_host,
      &mut window,
      Some(&mut webidl_host),
    );
 
    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      r#"
      (() => {
        const bodyDesc = Object.getOwnPropertyDescriptor(Document.prototype, 'body');
        const get = bodyDesc.get;
        const set = bodyDesc.set;
        const newBody = Document.prototype.createElement.call(document, 'body');
        set.call(document, newBody);
        return get.call(document) === newBody;
      })()
      "#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }
}

#[cfg(test)]
mod document_node_creation_tests {
  use super::*;
  use crate::dom2::NodeKind;
  use crate::js::window_realm::{DomBindingsBackend, WindowRealm, WindowRealmConfig};
  use selectors::context::QuirksMode;
  use std::any::Any;
  use vm_js::{Job, Scope, Value, VmError, VmHostHooks};

  #[derive(Default)]
  struct TestHooks {
    payload: VmJsHostHooksPayload,
  }

  impl VmHostHooks for TestHooks {
    fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<vm_js::RealmId>) {}

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
      Some(&mut self.payload)
    }
  }

  fn get_own_string_property(
    scope: &mut Scope<'_>,
    obj: GcObject,
    name: &str,
  ) -> Result<String, VmError> {
    scope.push_root(Value::Object(obj))?;
    let key_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    let value = scope
      .heap()
      .object_get_own_data_property_value(obj, &key)?
      .unwrap_or(Value::Undefined);
    let Value::String(s) = value else {
      return Err(VmError::TypeError("expected DOMException property to be a string"));
    };
    Ok(scope.heap().get_string(s)?.to_utf8_lossy())
  }

  #[test]
  fn element_style_webidl_backend_returns_stable_css_style_declaration_like_object(
  ) -> Result<(), VmError> {
    let mut window = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let mut dom_host = DocumentHostState::new(crate::dom2::Document::new(QuirksMode::NoQuirks));
    let mut dispatch =
      VmJsWebIdlBindingsHostDispatch::<crate::js::WindowHostState>::new(window.global_object());

    let mut hooks = TestHooks::default();
    hooks.payload.set_vm_host(&mut dom_host);
    // Expose the dispatch host to WebIDL binding shims.
    hooks
      .payload
      .webidl_bindings_host_slot_mut()
      .set(&mut dispatch);

    let ok = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      "(() => {\n\
        const el = document.createElement('div');\n\
        const s = el.style;\n\
        if (!(s && s === el.style)) return false;\n\
        if (!(s instanceof CSSStyleDeclaration)) return false;\n\
        s.setProperty('cursor', 'pointer');\n\
        let attr = el.getAttribute('style');\n\
        if (attr === null || !attr.includes('cursor: pointer')) return false;\n\
        el.style.display = 'none';\n\
        attr = el.getAttribute('style');\n\
        if (attr === null || !attr.includes('display: none')) return false;\n\
        const removed = el.style.removeProperty('cursor');\n\
        if (removed !== 'pointer') return false;\n\
        if (el.style.getPropertyValue('cursor') !== '') return false;\n\
        el.style.cssText = 'width: 10px; height: 5px;';\n\
        if (el.style.width !== '10px') return false;\n\
        if (el.style.height !== '5px') return false;\n\
        return true;\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    Ok(())
  }

  #[test]
  fn document_create_node_allocators_return_detached_wrappers() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let mut dom_host = DocumentHostState::new(crate::dom2::Document::new(QuirksMode::NoQuirks));
    let mut dispatch =
      VmJsWebIdlBindingsHostDispatch::<crate::js::WindowHostState>::new(window.global_object());

    let (vm, _realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();

    let mut hooks = TestHooks::default();
    hooks.payload.set_vm_host(&mut dom_host);

    let (element_id, text_id, fragment_id) = vm.with_host_hooks_override(
      &mut hooks,
      |vm| -> Result<(NodeId, NodeId, NodeId), VmError> {
        let document = dispatch.call_operation(vm, &mut scope, None, "Window", "document", 0, &[])?;
        let Value::Object(document_obj) = document else {
          return Err(VmError::TypeError("expected Window.document to return an object"));
        };
        scope.push_root(document)?;

        let div_s = scope.alloc_string("div")?;
        scope.push_root(Value::String(div_s))?;
        let element = dispatch.call_operation(
          vm,
          &mut scope,
          Some(Value::Object(document_obj)),
          "Document",
          "createElement",
          0,
          &[Value::String(div_s)],
        )?;
        scope.push_root(element)?;
        let element_id = {
          let data = vm
            .user_data_mut::<WindowRealmUserData>()
            .ok_or(VmError::TypeError("expected WindowRealmUserData"))?;
          let platform = data
            .dom_platform_mut()
            .ok_or(VmError::TypeError("expected DomPlatform"))?;
          platform.require_element_id(scope.heap(), element)?
        };

        let hi_s = scope.alloc_string("hi")?;
        scope.push_root(Value::String(hi_s))?;
        let text = dispatch.call_operation(
          vm,
          &mut scope,
          Some(Value::Object(document_obj)),
          "Document",
          "createTextNode",
          0,
          &[Value::String(hi_s)],
        )?;
        scope.push_root(text)?;
        let text_id = {
          let data = vm
            .user_data_mut::<WindowRealmUserData>()
            .ok_or(VmError::TypeError("expected WindowRealmUserData"))?;
          let platform = data
            .dom_platform_mut()
            .ok_or(VmError::TypeError("expected DomPlatform"))?;
          platform.require_text_id(scope.heap(), text)?
        };

        let fragment = dispatch.call_operation(
          vm,
          &mut scope,
          Some(Value::Object(document_obj)),
          "Document",
          "createDocumentFragment",
          0,
          &[],
        )?;
        scope.push_root(fragment)?;
        let fragment_id = {
          let data = vm
            .user_data_mut::<WindowRealmUserData>()
            .ok_or(VmError::TypeError("expected WindowRealmUserData"))?;
          let platform = data
            .dom_platform_mut()
            .ok_or(VmError::TypeError("expected DomPlatform"))?;
          platform.require_document_fragment_id(scope.heap(), fragment)?
        };

        Ok((element_id, text_id, fragment_id))
      },
    )?;

    dom_host.with_dom(|dom| {
      assert!(dom.node(element_id).parent.is_none());
      match &dom.node(element_id).kind {
        NodeKind::Element { tag_name, .. } => assert_eq!(tag_name, "div"),
        other => panic!("expected Element node kind, got {other:?}"),
      }

      assert!(dom.node(text_id).parent.is_none());
      match &dom.node(text_id).kind {
        NodeKind::Text { content } => assert_eq!(content, "hi"),
        other => panic!("expected Text node kind, got {other:?}"),
      }

      assert!(dom.node(fragment_id).parent.is_none());
      assert!(matches!(dom.node(fragment_id).kind, NodeKind::DocumentFragment));
    });

    Ok(())
  }

  #[test]
  fn document_create_element_throws_invalid_character_error() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let mut dom_host = DocumentHostState::new(crate::dom2::Document::new(QuirksMode::NoQuirks));
    let mut dispatch =
      VmJsWebIdlBindingsHostDispatch::<crate::js::WindowHostState>::new(window.global_object());

    let (vm, _realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();

    let mut hooks = TestHooks::default();
    hooks.payload.set_vm_host(&mut dom_host);

    let err = vm
      .with_host_hooks_override(&mut hooks, |vm| {
        let document = dispatch.call_operation(vm, &mut scope, None, "Window", "document", 0, &[])?;
        let Value::Object(document_obj) = document else {
          return Err(VmError::TypeError("expected Window.document to return an object"));
        };
        scope.push_root(document)?;

        let invalid_s = scope.alloc_string("")?;
        scope.push_root(Value::String(invalid_s))?;
        dispatch.call_operation(
          vm,
          &mut scope,
          Some(Value::Object(document_obj)),
          "Document",
          "createElement",
          0,
          &[Value::String(invalid_s)],
        )
      })
      .unwrap_err();

    let VmError::Throw(thrown) = err else {
      return Err(VmError::TypeError("expected InvalidCharacterError to throw"));
    };
    scope.push_root(thrown)?;
    let Value::Object(obj) = thrown else {
      return Err(VmError::TypeError("expected thrown DOMException to be an object"));
    };
    assert_eq!(
      get_own_string_property(&mut scope, obj, "name")?,
      "InvalidCharacterError"
    );

    Ok(())
  }

  #[test]
  fn document_create_element_prototype_method_returns_html_element_wrappers_in_webidl_backend(
  ) -> Result<(), VmError> {
    let mut window = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let mut dom_host = DocumentHostState::new(crate::dom2::Document::new(QuirksMode::NoQuirks));
    let mut dispatch =
      VmJsWebIdlBindingsHostDispatch::<crate::js::WindowHostState>::new(window.global_object());

    let mut hooks = TestHooks::default();
    hooks.payload.set_vm_host(&mut dom_host);
    hooks
      .payload
      .webidl_bindings_host_slot_mut()
      .set(&mut dispatch);

    let out = window.exec_script_with_host_and_hooks(
      &mut dom_host,
      &mut hooks,
      r#"(() => {
        const el = Document.prototype.createElement.call(document, 'div');
        if (!(el instanceof HTMLElement)) throw new Error('expected instanceof HTMLElement');
        if (!(el instanceof HTMLDivElement)) throw new Error('expected instanceof HTMLDivElement');
        if (Object.getPrototypeOf(el) !== HTMLDivElement.prototype) throw new Error('wrong prototype');
        return true;
      })()"#,
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }
}

#[cfg(test)]
mod selector_api_tests {
  use super::*;
  use crate::js::window_realm::{DomBindingsBackend, WindowRealm, WindowRealmConfig};
  use std::any::Any;
  use vm_js::{Job, Value, VmError, VmHostHooks};

  #[derive(Default)]
  struct TestHooks {
    payload: VmJsHostHooksPayload,
  }

  impl VmHostHooks for TestHooks {
    fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<vm_js::RealmId>) {
      panic!("unexpected promise job in selector_api_tests");
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
      Some(&mut self.payload)
    }
  }

  fn assert_script(
    window: &mut WindowRealm,
    host: &mut DocumentHostState,
    hooks: &mut TestHooks,
    script: &str,
  ) -> Result<(), VmError> {
    let out = window.exec_script_with_host_and_hooks(host, hooks, script)?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn webidl_selector_apis_are_wired_through_host_dispatch() -> Result<(), VmError> {
    let mut window = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;

    let root = crate::dom::parse_html(
      "<!doctype html><div id=a><span id=b></span></div>",
    )
    .expect("parse_html");
    let mut dom_host = DocumentHostState::from_renderer_dom(&root);

    let mut dispatch =
      VmJsWebIdlBindingsHostDispatch::<crate::js::WindowHostState>::new(window.global_object());

    let mut hooks = TestHooks::default();
    hooks.payload.set_vm_host(&mut dom_host);
    hooks
      .payload
      .webidl_bindings_host_slot_mut()
      .set(&mut dispatch);

    assert_script(
      &mut window,
      &mut dom_host,
      &mut hooks,
      r#"
        (function () {
          var el = document.getElementById('a');
          return Element.prototype.matches.call(el, '#a') === true;
        })()
      "#,
    )?;

    assert_script(
      &mut window,
      &mut dom_host,
      &mut hooks,
      r#"
        (function () {
          var el = document.getElementById('a');
          var b = Element.prototype.querySelector.call(el, '#b');
          return b !== null && b.id === 'b';
        })()
      "#,
    )?;

    assert_script(
      &mut window,
      &mut dom_host,
      &mut hooks,
      r#"
        (function () {
          var b = document.getElementById('b');
          var a = Element.prototype.closest.call(b, '#a');
          return a !== null && a.id === 'a';
        })()
      "#,
    )?;

    assert_script(
      &mut window,
      &mut dom_host,
      &mut hooks,
      r#"
        (function () {
          var a = Document.prototype.querySelector.call(document, '#a');
          return a !== null && a.id === 'a';
        })()
      "#,
    )?;

    assert_script(
      &mut window,
      &mut dom_host,
      &mut hooks,
      r#"
        (function () {
          var el = document.getElementById('a');
          try {
            Element.prototype.matches.call(el, '???');
            return false;
          } catch (e) {
            return e && e.name === 'SyntaxError';
          }
        })()
      "#,
    )?;

    Ok(())
  }
}

#[cfg(test)]
mod webidl_event_target_dom2_tests {
  use super::*;
  use crate::js::window_realm::{DomBindingsBackend, WindowRealm, WindowRealmConfig};
  use crate::js::WindowHostState;
  use vm_js::Value;

  fn exec_webidl_event_target_script_to_string(script: &str) -> Result<String, VmError> {
    let mut window = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;

    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(window.global_object());
    let value = window.with_webidl_bindings_host(&mut dispatch, |realm| realm.exec_script(script))?;

    let (_vm, _realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let Value::String(s) = value else {
      return Err(VmError::TypeError("expected script to return a string"));
    };
    Ok(scope.heap().get_string(s)?.to_utf8_lossy())
  }

  #[test]
  fn webidl_event_target_capture_ordering_at_target_uses_dom2_dispatch() -> Result<(), VmError> {
    let out = exec_webidl_event_target_script_to_string(
      r#"
(() => {
  const t = new EventTarget();
  const log = [];
  function bubble() { log.push('bubble'); }
  function capture() { log.push('capture'); }
  t.addEventListener('x', bubble);
  t.addEventListener('x', capture, Object.create({ capture: true }));
  t.dispatchEvent(new Event('x', { bubbles: true }));
  return log.join(',');
})()
"#,
    )?;
    assert_eq!(out, "capture,bubble");
    Ok(())
  }

  #[test]
  fn webidl_event_target_parent_chain_propagates_and_sets_current_target() -> Result<(), VmError> {
    let out = exec_webidl_event_target_script_to_string(
      r#"
(() => {
  const parent = new EventTarget();
  const child = new EventTarget(parent);
  const log = [];

  parent.addEventListener('x', (e) => log.push('pc:' + (e.currentTarget === parent)), { capture: true });
  child.addEventListener('x', (e) => log.push('cc:' + (e.currentTarget === child)), { capture: true });
  child.addEventListener('x', (e) => log.push('cb:' + (e.currentTarget === child)));
  parent.addEventListener('x', (e) => log.push('pb:' + (e.currentTarget === parent)));

  child.dispatchEvent(new Event('x', { bubbles: true }));
  return log.join(',');
})()
"#,
    )?;
    assert_eq!(out, "pc:true,cc:true,cb:true,pb:true");
    Ok(())
  }

  #[test]
  fn webidl_event_target_passive_listener_prevent_default_is_ignored() -> Result<(), VmError> {
    let out = exec_webidl_event_target_script_to_string(
      r#"
(() => {
  const t = new EventTarget();
  let inside = null;
  t.addEventListener('x', (e) => { e.preventDefault(); inside = e.defaultPrevented; }, { passive: true });
  const ev = new Event('x', { cancelable: true });
  const ret = t.dispatchEvent(ev);
  return [inside, ev.defaultPrevented, ret].join(',');
})()
"#,
    )?;
    assert_eq!(out, "false,false,true");
    Ok(())
  }

  #[test]
  fn webidl_event_target_once_listener_runs_once() -> Result<(), VmError> {
    let out = exec_webidl_event_target_script_to_string(
      r#"
(() => {
  const t = new EventTarget();
  let n = 0;
  t.addEventListener('x', () => { n++; }, { once: true });
  t.dispatchEvent(new Event('x'));
  t.dispatchEvent(new Event('x'));
  return String(n);
})()
"#,
    )?;
    assert_eq!(out, "1");
    Ok(())
  }

  #[test]
  fn webidl_event_target_stop_propagation_prevents_reaching_target() -> Result<(), VmError> {
    let out = exec_webidl_event_target_script_to_string(
      r#"
(() => {
  const parent = new EventTarget();
  const child = new EventTarget(parent);
  const log = [];
  parent.addEventListener('x', (e) => { log.push('pc'); e.stopPropagation(); }, { capture: true });
  child.addEventListener('x', () => log.push('cc'), { capture: true });
  child.addEventListener('x', () => log.push('cb'));
  parent.addEventListener('x', () => log.push('pb'));
  child.dispatchEvent(new Event('x', { bubbles: true }));
  return log.join(',');
})()
"#,
    )?;
    assert_eq!(out, "pc");
    Ok(())
  }

  #[test]
  fn webidl_event_target_stop_propagation_does_not_stop_other_listeners_on_same_target(
  ) -> Result<(), VmError> {
    let out = exec_webidl_event_target_script_to_string(
      r#"
(() => {
  const parent = new EventTarget();
  const child = new EventTarget(parent);
  const log = [];
  parent.addEventListener('x', () => { log.push('a'); }, { capture: true });
  parent.addEventListener('x', (e) => { log.push('b'); e.stopPropagation(); }, { capture: true });
  parent.addEventListener('x', () => { log.push('c'); }, { capture: true });
  child.addEventListener('x', () => log.push('child'), { capture: true });
  child.dispatchEvent(new Event('x', { bubbles: true }));
  return log.join(',');
})()
"#,
    )?;
    // stopPropagation prevents reaching the target, but does not stop other listeners on the same
    // currentTarget/phase.
    assert_eq!(out, "a,b,c");
    Ok(())
  }

  #[test]
  fn webidl_event_target_stop_immediate_propagation_stops_other_listeners_on_same_target(
  ) -> Result<(), VmError> {
    let out = exec_webidl_event_target_script_to_string(
      r#"
(() => {
  const parent = new EventTarget();
  const child = new EventTarget(parent);
  const log = [];
  parent.addEventListener('x', () => { log.push('a'); }, { capture: true });
  parent.addEventListener('x', (e) => { log.push('b'); e.stopImmediatePropagation(); }, { capture: true });
  parent.addEventListener('x', () => { log.push('c'); }, { capture: true });
  child.addEventListener('x', () => log.push('child'), { capture: true });
  child.dispatchEvent(new Event('x', { bubbles: true }));
  return log.join(',');
})()
"#,
    )?;
    assert_eq!(out, "a,b");
    Ok(())
  }

  #[test]
  fn webidl_event_target_prevent_default_sets_default_prevented_and_returns_false(
  ) -> Result<(), VmError> {
    let out = exec_webidl_event_target_script_to_string(
      r#"
(() => {
  const t = new EventTarget();
  t.addEventListener('x', (e) => e.preventDefault());
  const ev = new Event('x', { cancelable: true });
  const ret = t.dispatchEvent(ev);
  return [ev.defaultPrevented, ret].join(',');
})()
"#,
    )?;
    assert_eq!(out, "true,false");
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::api::RenderOptions;
  use std::any::Any;
  use vm_js::{
    Heap, HeapLimits, Job, Realm, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
  };
  use webidl_vm_js::host_from_hooks;

  #[derive(Debug, Default)]
  pub(super) struct RecordingDomWebIdlHost {
    last_call: Option<RecordingCall>,
    last_iterable: Option<RecordingIterableSnapshot>,
  }

  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  struct RecordingCall {
    interface: &'static str,
    operation: &'static str,
    overload: usize,
  }

  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  struct RecordingIterableSnapshot {
    interface: &'static str,
    kind: IterableKind,
  }

  impl WebIdlBindingsHost for RecordingDomWebIdlHost {
    fn call_operation(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _receiver: Option<Value>,
      interface: &'static str,
      operation: &'static str,
      overload: usize,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      self.last_call = Some(RecordingCall {
        interface,
        operation,
        overload,
      });
      Ok(Value::Bool(true))
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

    fn iterable_snapshot(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _receiver: Option<Value>,
      interface: &'static str,
      kind: IterableKind,
    ) -> Result<Vec<BindingValue>, VmError> {
      self.last_iterable = Some(RecordingIterableSnapshot { interface, kind });
      Ok(vec![BindingValue::Number(123.0)])
    }
  }

  #[derive(Default)]
  struct TestHooks {
    payload: VmJsHostHooksPayload,
  }

  impl VmHostHooks for TestHooks {
    fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<vm_js::RealmId>) {}

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
      Some(&mut self.payload)
    }
  }

  struct DummyWindowRealmHost;

  impl WindowRealmHost for DummyWindowRealmHost {
    fn vm_host_and_window_realm(
      &mut self,
    ) -> crate::error::Result<(&mut dyn VmHost, &mut crate::js::WindowRealm)> {
      unreachable!("DummyWindowRealmHost is only used as a type parameter in tests")
    }
  }

  impl crate::js::DomHost for DummyWindowRealmHost {
    fn with_dom<R, F>(&self, _f: F) -> R
    where
      F: FnOnce(&crate::dom2::Document) -> R,
    {
      unreachable!("DummyWindowRealmHost does not provide a DOM")
    }

    fn mutate_dom<R, F>(&mut self, _f: F) -> R
    where
      F: FnOnce(&mut crate::dom2::Document) -> (R, bool),
    {
      unreachable!("DummyWindowRealmHost does not provide a DOM")
    }
  }

  fn call_dom_operation_native(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let bindings_host = host_from_hooks(hooks)?;
    bindings_host.call_operation(vm, scope, None, "Document", "testOperation", 0, &[])
  }

  fn call_dom_iterable_native(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let bindings_host = host_from_hooks(hooks)?;
    let values =
      bindings_host.iterable_snapshot(vm, scope, None, "NodeList", IterableKind::Values)?;
    Ok(Value::Number(values.len() as f64))
  }

  fn call_non_dom_operation_native(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let bindings_host = host_from_hooks(hooks)?;
    bindings_host.call_operation(vm, scope, None, "Window", "alert", 0, &[])
  }

  fn make_native_fn(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    name: &str,
    native: vm_js::NativeCall,
  ) -> Result<GcObject, VmError> {
    let id = vm.register_native_call(native)?;
    let name_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(name_s))?;
    let func = scope.alloc_native_function(id, None, name_s, 0)?;
    scope.push_root(Value::Object(func))?;
    Ok(func)
  }

  #[test]
  fn webidl_nodelist_item_and_length_dispatch_read_own_properties() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let global = realm.global_object();
    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<DummyWindowRealmHost>::new(global);
    let mut scope = heap.scope();

    let list_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(list_obj))?;

    let key0 = key_from_str(&mut scope, "0")?;
    scope.define_property(list_obj, key0, data_property(Value::Number(10.0), true, true, true))?;
    let key1 = key_from_str(&mut scope, "1")?;
    scope.define_property(
      list_obj,
      key1,
      data_property(Value::Undefined, true, true, true),
    )?;
    let length_key = key_from_str(&mut scope, COLLECTION_LENGTH_KEY)?;
    scope.define_property(
      list_obj,
      length_key,
      data_property(Value::Number(2.0), true, true, true),
    )?;

    let len = dispatch.call_operation(
      &mut vm,
      &mut scope,
      Some(Value::Object(list_obj)),
      "NodeList",
      "length",
      0,
      &[],
    )?;
    assert_eq!(len, Value::Number(2.0));

    let item0 = dispatch.call_operation(
      &mut vm,
      &mut scope,
      Some(Value::Object(list_obj)),
      "NodeList",
      "item",
      0,
      &[Value::Number(0.0)],
    )?;
    assert_eq!(item0, Value::Number(10.0));

    // Own property exists but is undefined => null.
    let item1 = dispatch.call_operation(
      &mut vm,
      &mut scope,
      Some(Value::Object(list_obj)),
      "NodeList",
      "item",
      0,
      &[Value::Number(1.0)],
    )?;
    assert_eq!(item1, Value::Null);

    // Missing numeric property => null.
    let item2 = dispatch.call_operation(
      &mut vm,
      &mut scope,
      Some(Value::Object(list_obj)),
      "NodeList",
      "item",
      0,
      &[Value::Number(2.0)],
    )?;
    assert_eq!(item2, Value::Null);

    // Missing length property => 0.
    let empty_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(empty_obj))?;
    let len0 = dispatch.call_operation(
      &mut vm,
      &mut scope,
      Some(Value::Object(empty_obj)),
      "NodeList",
      "length",
      0,
      &[],
    )?;
    assert_eq!(len0, Value::Number(0.0));

    // Brand check: receiver must be an object.
    let err = dispatch
      .call_operation(&mut vm, &mut scope, None, "NodeList", "length", 0, &[])
      .unwrap_err();
    assert!(matches!(err, VmError::TypeError("Illegal invocation")));

    // Avoid `Realm dropped without calling teardown()` panics in vm-js.
    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn webidl_nodelist_iterable_snapshot_reads_length_and_indices() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let global = realm.global_object();
    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<DummyWindowRealmHost>::new(global);
    let mut scope = heap.scope();

    let list_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(list_obj))?;

    let v0_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(v0_obj))?;
    let v1_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(v1_obj))?;

    let idx0 = key_from_str(&mut scope, "0")?;
    scope.define_property(
      list_obj,
      idx0,
      data_property(Value::Object(v0_obj), true, true, true),
    )?;
    let idx1 = key_from_str(&mut scope, "1")?;
    scope.define_property(
      list_obj,
      idx1,
      data_property(Value::Object(v1_obj), true, true, true),
    )?;
    let length_key = key_from_str(&mut scope, COLLECTION_LENGTH_KEY)?;
    scope.define_property(
      list_obj,
      length_key,
      data_property(Value::Number(2.0), true, false, false),
    )?;

    let values = dispatch.iterable_snapshot(
      &mut vm,
      &mut scope,
      Some(Value::Object(list_obj)),
      "NodeList",
      IterableKind::Values,
    )?;
    assert_eq!(
      values,
      vec![
        BindingValue::Object(Value::Object(v0_obj)),
        BindingValue::Object(Value::Object(v1_obj)),
      ]
    );

    let keys = dispatch.iterable_snapshot(
      &mut vm,
      &mut scope,
      Some(Value::Object(list_obj)),
      "NodeList",
      IterableKind::Keys,
    )?;
    assert_eq!(keys, vec![BindingValue::Number(0.0), BindingValue::Number(1.0)]);

    let entries = dispatch.iterable_snapshot(
      &mut vm,
      &mut scope,
      Some(Value::Object(list_obj)),
      "NodeList",
      IterableKind::Entries,
    )?;
    assert_eq!(
      entries,
      vec![
        BindingValue::Sequence(vec![
          BindingValue::Number(0.0),
          BindingValue::Object(Value::Object(v0_obj)),
        ]),
        BindingValue::Sequence(vec![
          BindingValue::Number(1.0),
          BindingValue::Object(Value::Object(v1_obj)),
        ]),
      ]
    );

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn webidl_dispatch_delegates_dom_operation_to_active_vm_host() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    // Install bindings so global names exist (future allowlist expansions will install more DOM).
    crate::js::bindings::install_node_bindings_vm_js(&mut vm, &mut heap, &realm)?;

    let global = realm.global_object();
    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<DummyWindowRealmHost>::new(global);
    let mut dom_host = RecordingDomWebIdlHost::default();

    let mut hooks = TestHooks::default();
    hooks.payload.set_vm_host(&mut dom_host);
    hooks
      .payload
      .webidl_bindings_host_slot_mut()
      .set(&mut dispatch);

    let mut dummy_vm_host = ();

    let result = {
      let mut scope = heap.scope();
      let func = make_native_fn(
        &mut vm,
        &mut scope,
        "callDomOperation",
        call_dom_operation_native,
      )?;
      vm.call_with_host_and_hooks(
        &mut dummy_vm_host,
        &mut scope,
        &mut hooks,
        Value::Object(func),
        Value::Undefined,
        &[],
      )?
    };

    assert_eq!(result, Value::Bool(true));
    assert_eq!(
      dom_host.last_call,
      Some(RecordingCall {
        interface: "Document",
        operation: "testOperation",
        overload: 0
      })
    );

    // Avoid `Realm dropped without calling teardown()` panics in vm-js.
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn webidl_dispatch_delegates_dom_iterable_snapshot_to_active_vm_host() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let global = realm.global_object();
    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<DummyWindowRealmHost>::new(global);
    let mut dom_host = RecordingDomWebIdlHost::default();

    let mut hooks = TestHooks::default();
    hooks.payload.set_vm_host(&mut dom_host);
    hooks
      .payload
      .webidl_bindings_host_slot_mut()
      .set(&mut dispatch);

    let mut dummy_vm_host = ();

    let result = {
      let mut scope = heap.scope();
      let func = make_native_fn(
        &mut vm,
        &mut scope,
        "callDomIterable",
        call_dom_iterable_native,
      )?;
      vm.call_with_host_and_hooks(
        &mut dummy_vm_host,
        &mut scope,
        &mut hooks,
        Value::Object(func),
        Value::Undefined,
        &[],
      )?
    };

    assert_eq!(result, Value::Number(1.0));
    assert_eq!(
      dom_host.last_iterable,
      Some(RecordingIterableSnapshot {
        interface: "NodeList",
        kind: IterableKind::Values
      })
    );

    // Avoid `Realm dropped without calling teardown()` panics in vm-js.
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn webidl_dispatch_does_not_delegate_non_dom_interfaces() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let global = realm.global_object();
    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<DummyWindowRealmHost>::new(global);
    let mut dom_host = RecordingDomWebIdlHost::default();

    let mut hooks = TestHooks::default();
    hooks.payload.set_vm_host(&mut dom_host);
    hooks
      .payload
      .webidl_bindings_host_slot_mut()
      .set(&mut dispatch);

    let mut dummy_vm_host = ();

    let result = {
      let mut scope = heap.scope();
      let func = make_native_fn(
        &mut vm,
        &mut scope,
        "callNonDomOperation",
        call_non_dom_operation_native,
      )?;
      vm.call_with_host_and_hooks(
        &mut dummy_vm_host,
        &mut scope,
        &mut hooks,
        Value::Object(func),
        Value::Undefined,
        &[],
      )?
    };

    assert_eq!(result, Value::Undefined);
    assert_eq!(dom_host.last_call, None);

    // Avoid `Realm dropped without calling teardown()` panics in vm-js.
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn webidl_dispatch_helpers_do_not_delegate_to_browser_document_dom2() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());

    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<DummyWindowRealmHost>::new_without_global();

    let mut dom = BrowserDocumentDom2::from_html(
      "<!doctype html><html><body></body></html>",
      RenderOptions::new().with_viewport(1, 1),
    )
    .expect("BrowserDocumentDom2");

    let mut hooks = TestHooks::default();
    hooks.payload.set_vm_host(&mut dom);

    let mut scope = heap.scope();

    let delegated = vm.with_host_hooks_override(&mut hooks, |vm| {
      dispatch.try_delegate_dom_call_operation(
        vm,
        &mut scope,
        None,
        "Document",
        "testOperation",
        0,
        &[],
      )
    })?;
    assert_eq!(delegated, None);

    let delegated = vm.with_host_hooks_override(&mut hooks, |vm| {
      dispatch.try_delegate_dom_iterable_snapshot(
        vm,
        &mut scope,
        None,
        "NodeList",
        IterableKind::Values,
      )
    })?;
    assert_eq!(delegated, None);

    Ok(())
  }

  #[test]
  fn webidl_dispatch_call_constructor_is_shim_for_call_operation() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());
    let mut scope = heap.scope();

    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<DummyWindowRealmHost>::new_without_global();

    let err = dispatch
      .call_constructor(
        &mut vm,
        &mut scope,
        "BogusInterface",
        2,
        &[],
        Value::Undefined,
      )
      .expect_err("expected unimplemented constructor dispatch to error");

    match err {
      VmError::Unimplemented(msg) => {
        assert!(msg.contains("BogusInterface.constructor"));
        assert!(msg.contains("overload 2"));
        assert!(msg.contains("receiver=false"));
      }
      other => panic!("expected VmError::Unimplemented, got {other:?}"),
    }

    Ok(())
  }

  #[test]
  fn webidl_dispatch_unimplemented_fallback_error_includes_context() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());
    let mut scope = heap.scope();

    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<DummyWindowRealmHost>::new_without_global();

    let err = dispatch
      .call_operation(
        &mut vm,
        &mut scope,
        Some(Value::Undefined),
        "BogusInterface",
        "bogusOperation",
        3,
        &[],
      )
      .expect_err("expected unimplemented operation dispatch to error");

    match err {
      VmError::Unimplemented(msg) => {
        assert!(msg.contains("BogusInterface.bogusOperation"));
        assert!(msg.contains("overload 3"));
        assert!(msg.contains("receiver=true"));
      }
      other => panic!("expected VmError::Unimplemented, got {other:?}"),
    }

    Ok(())
  }
}

#[cfg(test)]
mod element_dispatch_tests {
  use super::*;
  use crate::dom2;
  use crate::js::dom_platform::DomInterface;
  use crate::js::realm_module_loader::{ModuleLoader, ModuleLoaderHandle};
  use crate::js::window_realm::DomBindingsBackend;
  use crate::js::{WindowHostState, WindowRealm, WindowRealmConfig};
  use selectors::context::QuirksMode;
  use std::any::Any;
  use webidl_vm_js::host_from_hooks;

  #[derive(Default)]
  struct TestHooks {
    payload: VmJsHostHooksPayload,
  }

  impl VmHostHooks for TestHooks {
    fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {}

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
      Some(&mut self.payload)
    }
  }

  #[test]
  fn element_dispatch_id_class_name_tag_name_and_attributes() -> Result<(), VmError> {
    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = vm_js::Heap::new(vm_js::HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024));
    let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

    // `Element.classList` returns a real DOMTokenList instance, which requires the `DOMTokenList`
    // bindings to exist on the realm global. This test uses host dispatch directly (without
    // `WindowRealm`), so install just the DOMTokenList bindings to provide the prototype.
    crate::js::bindings::install_dom_token_list_bindings_vm_js(&mut vm, &mut heap, &realm)?;

    let document_url = "https://example.invalid/".to_string();
    let module_loader: ModuleLoaderHandle =
      std::rc::Rc::new(std::cell::RefCell::new(ModuleLoader::new(Some(document_url.clone()))));
    vm.set_user_data(WindowRealmUserData::new(
      document_url,
      std::rc::Rc::clone(&module_loader),
      Some(1),
      None,
      5 * 1024 * 1024,
      std::sync::Arc::new(crate::clock::RealClock::default()),
    ));

    let mut scope = heap.scope();
    let mut platform = DomPlatform::new(&mut scope, &realm)?;

    // `DomPlatform` wrapper caching is keyed by a (weak) JS document wrapper identity. WebIDL
    // dispatch tests don't need a full Document implementation, but they must supply a stable
    // document key so wrapper identity remains consistent.
    let document_obj = scope.alloc_object()?;
    let document_key = WeakGcObject::from(document_obj);
    let document_root = scope.heap_mut().add_root(Value::Object(document_obj))?;
    platform.register_wrapper(
      scope.heap(),
      document_obj,
      document_key,
      dom2::NodeId::from_index(0),
      DomInterface::Document,
    );
    vm
      .user_data_mut::<WindowRealmUserData>()
      .expect("user data")
      .set_dom_platform(platform);

    let mut dom = crate::dom2::Document::new(QuirksMode::NoQuirks);
    let div = dom.create_element("div", "");
    dom.set_attribute(div, "id", "a").expect("set id");
    dom.set_attribute(div, "class", "x").expect("set class");
    dom
      .append_child(dom.root(), div)
      .expect("append div to document");
    let mut host = DocumentHostState::new(dom);

    let wrapper = {
      let data = vm.user_data_mut::<WindowRealmUserData>().expect("user data");
      let platform = data.dom_platform_mut().expect("platform");
      platform.get_or_create_wrapper(&mut scope, document_key, div, DomInterface::Element)?
    };
    scope.push_root(Value::Object(wrapper))?;

    let global = realm.global_object();
    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<crate::js::WindowHostState>::new(global);

    let mut hooks = TestHooks::default();
    hooks.payload.set_vm_host(&mut host);

    let result = vm.with_host_hooks_override(&mut hooks, |vm| {
      // Getter attributes.
      let got = dispatch.call_operation(vm, &mut scope, Some(Value::Object(wrapper)), "Element", "id", 0, &[])?;
      assert_eq!(js_string_to_rust_string(&scope, got)?, "a");

      let got =
        dispatch.call_operation(vm, &mut scope, Some(Value::Object(wrapper)), "Element", "className", 0, &[])?;
      assert_eq!(js_string_to_rust_string(&scope, got)?, "x");

      let got =
        dispatch.call_operation(vm, &mut scope, Some(Value::Object(wrapper)), "Element", "tagName", 0, &[])?;
      assert_eq!(js_string_to_rust_string(&scope, got)?, "DIV");

      // classList object should be stable and not crash.
      let got1 =
        dispatch.call_operation(vm, &mut scope, Some(Value::Object(wrapper)), "Element", "classList", 0, &[])?;
      let got2 =
        dispatch.call_operation(vm, &mut scope, Some(Value::Object(wrapper)), "Element", "classList", 0, &[])?;
      match (got1, got2) {
        (Value::Object(o1), Value::Object(o2)) => assert_eq!(o1, o2),
        other => panic!("expected object classList, got {other:?}"),
      }

      // getAttribute returns string/null.
      let id_key = scope.alloc_string("id")?;
      scope.push_root(Value::String(id_key))?;
      let got = dispatch.call_operation(
        vm,
        &mut scope,
        Some(Value::Object(wrapper)),
        "Element",
        "getAttribute",
        0,
        &[Value::String(id_key)],
      )?;
      assert_eq!(js_string_to_rust_string(&scope, got)?, "a");

      let missing_key = scope.alloc_string("missing")?;
      scope.push_root(Value::String(missing_key))?;
      let got = dispatch.call_operation(
        vm,
        &mut scope,
        Some(Value::Object(wrapper)),
        "Element",
        "getAttribute",
        0,
        &[Value::String(missing_key)],
      )?;
      assert_eq!(got, Value::Null);

      // setAttribute + removeAttribute.
      let data_x_key = scope.alloc_string("data-x")?;
      scope.push_root(Value::String(data_x_key))?;
      let one = scope.alloc_string("1")?;
      scope.push_root(Value::String(one))?;
      let got = dispatch.call_operation(
        vm,
        &mut scope,
        Some(Value::Object(wrapper)),
        "Element",
        "setAttribute",
        0,
        &[Value::String(data_x_key), Value::String(one)],
      )?;
      assert_eq!(got, Value::Undefined);

      let got = dispatch.call_operation(
        vm,
        &mut scope,
        Some(Value::Object(wrapper)),
        "Element",
        "getAttribute",
        0,
        &[Value::String(data_x_key)],
      )?;
      assert_eq!(js_string_to_rust_string(&scope, got)?, "1");

      let class_key = scope.alloc_string("class")?;
      scope.push_root(Value::String(class_key))?;
      let got = dispatch.call_operation(
        vm,
        &mut scope,
        Some(Value::Object(wrapper)),
        "Element",
        "removeAttribute",
        0,
        &[Value::String(class_key)],
      )?;
      assert_eq!(got, Value::Undefined);

      let got =
        dispatch.call_operation(vm, &mut scope, Some(Value::Object(wrapper)), "Element", "className", 0, &[])?;
      assert_eq!(js_string_to_rust_string(&scope, got)?, "");

      // className setter reflects into `class`.
      let new_class = scope.alloc_string("y")?;
      scope.push_root(Value::String(new_class))?;
      let got = dispatch.call_operation(
        vm,
        &mut scope,
        Some(Value::Object(wrapper)),
        "Element",
        "className",
        0,
        &[Value::String(new_class)],
      )?;
      assert_eq!(got, Value::Undefined);

      let got =
        dispatch.call_operation(vm, &mut scope, Some(Value::Object(wrapper)), "Element", "className", 0, &[])?;
      assert_eq!(js_string_to_rust_string(&scope, got)?, "y");

      // Brand check: wrong receiver throws TypeError.
      let bad = scope.alloc_object()?;
      scope.push_root(Value::Object(bad))?;
      let err = dispatch.call_operation(vm, &mut scope, Some(Value::Object(bad)), "Element", "id", 0, &[]);
      assert!(matches!(err, Err(VmError::TypeError("Illegal invocation"))));

      Ok(())
    });

    // Ensure we unregister persistent roots (Realm::drop debug-asserts on missing teardown).
    drop(scope);
    if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
      if let Some(platform) = data.dom_platform_mut() {
        platform.teardown(&mut heap);
      }
    }
    heap.remove_root(document_root);
    realm.teardown(&mut heap);
    result
  }

  fn call_document_get_element_by_id_native(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let Value::Object(global_obj) = this else {
      return Err(VmError::TypeError("expected global object"));
    };
    scope.push_root(Value::Object(global_obj))?;

    // Resolve `globalThis.document`.
    let document_key_s = scope.alloc_string("document")?;
    scope.push_root(Value::String(document_key_s))?;
    let document_key = PropertyKey::from_string(document_key_s);
    let document = vm.get_with_host_and_hooks(host, scope, hooks, global_obj, document_key)?;
    scope.push_root(document)?;

    let id_s = scope.alloc_string("target")?;
    scope.push_root(Value::String(id_s))?;
    let id_value = Value::String(id_s);

    let host_dispatch = webidl_vm_js::host_from_hooks(hooks)?;
    host_dispatch.call_operation(
      vm,
      scope,
      Some(document),
      "Document",
      "getElementById",
      0,
      &[id_value],
    )
  }

  #[test]
  fn dom_dispatch_can_recover_dom_host_without_embedder_state() -> Result<(), VmError> {
    let dom =
      crate::dom2::parse_html("<!doctype html><html><body><div id=\"target\"></div></body></html>")
      .expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut realm = crate::js::window_realm::WindowRealm::new(
      crate::js::window_realm::WindowRealmConfig::new("https://example.com/"),
    )?;
    let global = realm.global_object();

    let mut host_dispatch =
      VmJsWebIdlBindingsHostDispatch::<crate::js::WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<crate::js::WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut realm,
      Some(&mut host_dispatch),
    );

    let payload = VmHostHooks::as_any_mut(&mut hooks)
      .and_then(|any| any.downcast_mut::<VmJsHostHooksPayload>())
      .expect("hooks should expose VmJsHostHooksPayload");
    assert!(
      payload.embedder_state_any_mut().is_none(),
      "expected embedder_state to be unset for borrow-split hook construction"
    );

    let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
    let call_id = vm.register_native_call(call_document_get_element_by_id_native)?;
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global))?;

    let name = scope.alloc_string("test_dom_dispatch")?;
    scope.push_root(Value::String(name))?;
    let func = scope.alloc_native_function(call_id, None, name, 0)?;
    scope.heap_mut().object_set_prototype(
      func,
      Some(realm_ref.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(func))?;

    let result = vm.call_with_host_and_hooks(
      &mut doc_host,
      &mut scope,
      &mut hooks,
      Value::Object(func),
      Value::Object(global),
      &[],
    )?;
    assert!(matches!(result, Value::Object(_)));

    Ok(())
  }

  #[test]
  fn webidl_traversal_props_parent_element_and_siblings_work_without_embedder_state() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html(
      "<!doctype html><html><body><div id='root'><span id='a'></span><b id='b'></b></div></body></html>",
    )
    .expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.com/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;

    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(realm.global_object());
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut realm,
      Some(&mut dispatch),
    );

    let out = realm.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      "(() => {\
       const root=document.getElementById('root');\
       const a=document.getElementById('a');\
       const b=document.getElementById('b');\
       return a.parentElement===root && a.nextElementSibling===b && b.previousElementSibling===a;\
       })()",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_append_inserts_text_node_in_webidl_dom_backend() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = realm.global_object();

    let mut host_dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut realm,
      Some(&mut host_dispatch),
    );

    let payload = VmHostHooks::as_any_mut(&mut hooks)
      .and_then(|any| any.downcast_mut::<VmJsHostHooksPayload>())
      .expect("hooks should expose VmJsHostHooksPayload");
    assert!(
      payload.embedder_state_any_mut().is_none(),
      "expected borrow-split hooks to not set embedder_state"
    );

    let out = realm.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      "(() => { document.body.append({ toString: function() { return 'x'; } }); return true; })()",
    )?;
    assert_eq!(out, Value::Bool(true));

    doc_host.with_dom(|dom| {
      let body = dom.body().expect("body");
      let children = dom.children(body).expect("body children");
      assert_eq!(children.len(), 1);
      match &dom.node(children[0]).kind {
        NodeKind::Text { content } => assert_eq!(content, "x"),
        other => panic!("expected Text child, got {other:?}"),
      }
    });

    Ok(())
  }

  #[test]
  fn element_prepend_append_and_remove_work_in_webidl_dom_backend() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body><div id=\"a\"></div></body></html>")
      .expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = realm.global_object();

    let mut host_dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut realm,
      Some(&mut host_dispatch),
    );

    let out = realm.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      "(() => {\n\
         const a = document.getElementById('a');\n\
         a.prepend('1');\n\
         a.append('2');\n\
         return true;\n\
       })()",
    )?;
    assert_eq!(out, Value::Bool(true));

    doc_host.with_dom(|dom| {
      let a = dom.get_element_by_id("a").expect("expected #a to exist");
      let children = dom.children(a).expect("#a children");
      let mut s = String::new();
      for &child in children {
        match &dom.node(child).kind {
          NodeKind::Text { content } => s.push_str(content),
          other => panic!("expected only Text children, got {other:?}"),
        }
      }
      assert_eq!(s, "12");
    });

    let out = realm.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      "(() => {\n\
         const el = document.getElementById('a');\n\
         el.remove();\n\
         return true;\n\
       })()",
    )?;
    assert_eq!(out, Value::Bool(true));

    doc_host.with_dom(|dom| {
      assert!(dom.get_element_by_id("a").is_none());
    });

    Ok(())
  }

  #[test]
  fn document_fragment_append_and_prepend_work_in_webidl_dom_backend() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = realm.global_object();

    let mut host_dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut realm,
      Some(&mut host_dispatch),
    );

    let out = realm.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      "(() => {\
        const frag = document.createDocumentFragment();\
        const list = frag.childNodes;\
        const a = document.createElement('a');\
        frag.append('x', a);\
        if (list.length !== 2) return false;\
        if (list.item(1) !== a) return false;\
        const b = document.createElement('b');\
        frag.prepend('y', b);\
        if (list.length !== 4) return false;\
        if (list.item(1) !== b) return false;\
        if (list.item(2) !== a) return false;\
        return true;\
      })()",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn document_append_inserts_node_in_webidl_dom_backend() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = realm.global_object();

    let mut host_dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut realm,
      Some(&mut host_dispatch),
    );

    let out = realm.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      "(() => {\
        const d = new Document();\
        const c = d.createComment('x');\
        d.append(c);\
        return d.childNodes.length === 1 && d.childNodes[0] === c;\
      })()",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_append_adopts_foreign_nodes_in_webidl_dom_backend() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = realm.global_object();

    let mut host_dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut realm,
      Some(&mut host_dispatch),
    );

    let out = realm.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      "(() => {\
        const parent = document.createElement('div');\
        const doc2 = new DOMParser().parseFromString('<!doctype html><p>hi</p>', 'text/html');\
        const foreign = doc2.createElement('p');\
        foreign.appendChild(doc2.createTextNode('hello'));\
        if (foreign.ownerDocument !== doc2) return false;\
        parent.append(foreign);\
        return foreign.parentNode === parent\
          && foreign.ownerDocument === document\
          && foreign.firstChild.ownerDocument === document;\
      })()",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_append_child_adopts_foreign_nodes_in_webidl_dom_backend() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = realm.global_object();

    let mut host_dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut realm,
      Some(&mut host_dispatch),
    );

    let out = realm.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      "(() => {\
        const parent = document.createElement('div');\
        const doc2 = new DOMParser().parseFromString('<!doctype html><p>hi</p>', 'text/html');\
        const foreign = doc2.createElement('p');\
        foreign.appendChild(doc2.createTextNode('hello'));\
        if (foreign.ownerDocument !== doc2) return false;\
        if (parent.appendChild(foreign) !== foreign) return false;\
        return foreign.parentNode === parent\
          && foreign.ownerDocument === document\
          && foreign.firstChild.ownerDocument === document;\
      })()",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_append_child_adopts_foreign_fragment_children_but_not_fragment_itself_in_webidl_dom_backend(
  ) -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = realm.global_object();

    let mut host_dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut realm,
      Some(&mut host_dispatch),
    );

    let out = realm.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      "(() => {\
        const parent = document.createElement('div');\
        const doc2 = new DOMParser().parseFromString('<!doctype html><p>hi</p>', 'text/html');\
        const frag = doc2.createDocumentFragment();\
        const list = frag.childNodes;\
        const foreign = doc2.createElement('p');\
        foreign.appendChild(doc2.createTextNode('hello'));\
        frag.appendChild(foreign);\
        if (frag.ownerDocument !== doc2) return false;\
        if (list.length !== 1 || list.item(0) !== foreign) return false;\
        if (parent.appendChild(frag) !== frag) return false;\
        return foreign.parentNode === parent\
          && foreign.ownerDocument === document\
          && foreign.firstChild.ownerDocument === document\
          && frag.ownerDocument === doc2\
          && frag.childNodes.length === 0\
          && list.length === 0;\
      })()",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_insert_adjacent_element_adopts_foreign_nodes_in_webidl_dom_backend() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = realm.global_object();

    let mut host_dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut realm,
      Some(&mut host_dispatch),
    );

    let out = realm.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      "(() => {\
        const host = document.createElement('div');\
        const doc2 = new DOMParser().parseFromString('<!doctype html><p>hi</p>', 'text/html');\
        const foreign = doc2.createElement('p');\
        foreign.appendChild(doc2.createTextNode('hello'));\
        if (foreign.ownerDocument !== doc2) return false;\
        const out = host.insertAdjacentElement('beforeend', foreign);\
        if (out !== foreign) return false;\
        return foreign.parentNode === host\
          && foreign.ownerDocument === document\
          && foreign.firstChild.ownerDocument === document;\
      })()",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_append_adopts_foreign_fragment_children_but_not_fragment_itself_in_webidl_dom_backend(
  ) -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = realm.global_object();

    let mut host_dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut realm,
      Some(&mut host_dispatch),
    );

    let out = realm.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      "(() => {\
        const parent = document.createElement('div');\
        const doc2 = new DOMParser().parseFromString('<!doctype html><p>hi</p>', 'text/html');\
        const frag = doc2.createDocumentFragment();\
        const list = frag.childNodes;\
        const foreign = doc2.createElement('p');\
        foreign.appendChild(doc2.createTextNode('hello'));\
        frag.appendChild(foreign);\
        if (frag.ownerDocument !== doc2) return false;\
        if (list.length !== 1 || list.item(0) !== foreign) return false;\
        parent.append(frag);\
        return foreign.parentNode === parent\
          && foreign.ownerDocument === document\
          && foreign.firstChild.ownerDocument === document\
          && frag.ownerDocument === doc2\
          && frag.childNodes.length === 0\
          && list.length === 0;\
      })()",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_append_inserts_text_and_element_in_order() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = realm.global_object();

    let mut host_dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut realm,
      Some(&mut host_dispatch),
    );

    let out = realm.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      "(() => {\
        const el = document.createElement('div');\
        el.append('x', document.createElement('b'));\
        return el.innerHTML === 'x<b></b>';\
      })()",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_prepend_inserts_text_and_element_in_order() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = realm.global_object();

    let mut host_dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut realm,
      Some(&mut host_dispatch),
    );

    let out = realm.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      "(() => {\
        const el = document.createElement('div');\
        el.append(document.createElement('i'));\
        el.prepend('x', document.createElement('b'));\
        return el.innerHTML === 'x<b></b><i></i>';\
      })()",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_append_and_prepend_sync_cached_collections() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = realm.global_object();

    let mut host_dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(global);
    let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_vm_host_and_window_realm(
      &mut doc_host,
      &mut realm,
      Some(&mut host_dispatch),
    );

    let out = realm.exec_script_with_host_and_hooks(
      &mut doc_host,
      &mut hooks,
      "(() => {\
        const el = document.createElement('div');\
        const list = el.childNodes;\
        const coll = el.children;\
        const b = document.createElement('b');\
        el.append('x', b);\
        if (list.length !== 2) return false;\
        if (coll.length !== 1) return false;\
        if (list.item(1) !== b) return false;\
        if (coll.item(0) !== b) return false;\
        const i = document.createElement('i');\
        el.prepend('y', i);\
        if (list.length !== 4) return false;\
        if (coll.length !== 2) return false;\
        if (coll.item(0) !== i) return false;\
        if (coll.item(1) !== b) return false;\
        return true;\
      })()",
    )?;
    assert_eq!(out, Value::Bool(true));
    Ok(())
  }
}

#[cfg(test)]
mod element_html_accessors_webidl_tests {
  use super::*;
  use crate::js::window_realm::{DomBindingsBackend, WindowRealm, WindowRealmConfig};
  use crate::js::window_timers::VmJsEventLoopHooks;
  use vm_js::{Scope, Value, VmError};

  #[test]
  fn element_inner_html_and_outer_html_work_via_webidl_dispatch() -> Result<(), VmError> {
    let dom =
      crate::dom2::parse_html("<!doctype html><html><body></body></html>").expect("parse_html");
    let mut doc_host = DocumentHostState::new(dom);

    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.com/")
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl),
    )?;
    let global = realm.global_object();

    let mut host_dispatch =
      VmJsWebIdlBindingsHostDispatch::<crate::js::WindowHostState>::new(global);
    let mut hooks =
      VmJsEventLoopHooks::<crate::js::WindowHostState>::new_with_vm_host_and_window_realm(
        &mut doc_host,
        &mut realm,
        Some(&mut host_dispatch),
      );

    let script = r#"(() => {
      document.body.innerHTML = '<div id="a"></div>';
      const el = document.getElementById('a');
      if (!el) return 'missing';
      if (el.tagName !== 'DIV') return 'tagName:' + el.tagName;
      if (document.body.innerHTML !== '<div id="a"></div>') return 'inner:' + document.body.innerHTML;
      if (el.outerHTML !== '<div id="a"></div>') return 'outer:' + el.outerHTML;
      if (Object.prototype.hasOwnProperty.call(el, 'innerHTML')) return 'own-innerHTML';
      if (Object.prototype.hasOwnProperty.call(el, 'outerHTML')) return 'own-outerHTML';
      document.body.innerHTML = null;
      if (document.body.innerHTML !== '') return 'null:' + document.body.innerHTML;
      return 'ok';
    })()"#;

    let out = realm.exec_script_with_host_and_hooks(&mut doc_host, &mut hooks, script)?;
    let Value::String(s) = out else {
      panic!("expected string result, got {out:?}");
    };

    let mut scope: Scope<'_> = realm.heap_mut().scope();
    let got = scope.heap().get_string(s)?.to_utf8_lossy();
    assert_eq!(got, "ok");
    Ok(())
  }
}

#[cfg(test)]
mod dom_dispatch_tests {
  use super::*;

  use crate::js::window::WindowHostState;
  use crate::resource::{FetchedResource, ResourceFetcher};
  use std::any::Any;
  use std::sync::Arc;
  use vm_js::{Job, RealmId, VmHostHooks};

  #[derive(Debug, Default)]
  struct NoFetchResourceFetcher;

  impl ResourceFetcher for NoFetchResourceFetcher {
    fn fetch(&self, url: &str) -> crate::error::Result<FetchedResource> {
      Err(crate::Error::Other(format!(
        "NoFetchResourceFetcher does not support fetch: {url}"
      )))
    }
  }

  fn default_test_fetcher() -> Arc<dyn ResourceFetcher> {
    Arc::new(NoFetchResourceFetcher)
  }

  struct TestHooks {
    payload: VmJsHostHooksPayload,
  }

  impl TestHooks {
    fn new(host: &mut WindowHostState) -> Self {
      let mut payload = VmJsHostHooksPayload::default();
      payload.set_embedder_state(host);
      Self { payload }
    }
  }

  impl VmHostHooks for TestHooks {
    fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
      Some(&mut self.payload)
    }
  }

  fn child_list_len(scope: &mut Scope<'_>, array: GcObject) -> Result<usize, VmError> {
    scope.push_root(Value::Object(array))?;
    let length_key = key_from_str(scope, COLLECTION_LENGTH_KEY)?;
    match scope
      .heap()
      .object_get_own_data_property_value(array, &length_key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Number(n) if n.is_finite() && n >= 0.0 => Ok(n as usize),
      _ => Err(VmError::TypeError(
        "expected collection length slot to be a number",
      )),
    }
  }

  fn child_at(scope: &mut Scope<'_>, array: GcObject, idx: usize) -> Result<Value, VmError> {
    scope.push_root(Value::Object(array))?;
    let key = key_from_str(scope, &idx.to_string())?;
    Ok(
      scope
        .heap()
        .object_get_own_data_property_value(array, &key)?
        .unwrap_or(Value::Undefined),
    )
  }

  fn set_wrapper_document(
    scope: &mut Scope<'_>,
    wrapper: GcObject,
    document_obj: GcObject,
  ) -> Result<(), VmError> {
    // Mirror `window_realm`'s DOM wrappers: attach the originating document object so native DOM
    // operations can derive the correct wrapper cache key.
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(wrapper))?;
    scope.push_root(Value::Object(document_obj))?;
    let key = key_from_str(&mut scope, WRAPPER_DOCUMENT_KEY)?;
    match scope
      .heap_mut()
      .object_set_existing_data_property_value(wrapper, &key, Value::Object(document_obj))
    {
      Ok(()) => return Ok(()),
      Err(VmError::PropertyNotFound | VmError::PropertyNotData) => {}
      Err(err) => return Err(err),
    }
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Object(document_obj),
          writable: true,
        },
      },
    )?;
    Ok(())
  }

  #[test]
  fn delegate_helpers_do_not_delegate_to_browser_document_dom2() -> Result<(), VmError> {
    let mut dom_host = BrowserDocumentDom2::from_html(
      "<!doctype html><html></html>",
      crate::api::RenderOptions::default(),
    )
    .expect("BrowserDocumentDom2::from_html");

    struct Hooks {
      payload: VmJsHostHooksPayload,
    }

    impl VmHostHooks for Hooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}

      fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(&mut self.payload)
      }
    }

    let mut payload = VmJsHostHooksPayload::default();
    payload.set_vm_host(&mut dom_host);
    let mut hooks = Hooks { payload };

    let mut vm = Vm::new(vm_js::VmOptions::default());
    let mut heap = vm_js::Heap::new(vm_js::HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new_without_global();

    let delegated = vm.with_host_hooks_override(&mut hooks, |vm| {
      dispatch.try_delegate_dom_call_operation(vm, &mut scope, None, "Node", "parentNode", 0, &[])
    })?;
    assert!(delegated.is_none());

    let delegated = vm.with_host_hooks_override(&mut hooks, |vm| {
      dispatch.try_delegate_dom_iterable_snapshot(vm, &mut scope, None, "Node", IterableKind::Keys)
    })?;
    assert!(delegated.is_none());

    Ok(())
  }

  #[test]
  fn node_append_child_attaches_and_traversal_reflects_it() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<div id=a></div>").unwrap();
    let mut host =
      WindowHostState::new_with_fetcher(dom, "https://example.invalid/", default_test_fetcher())
        .unwrap();

    let parent_id = host
      .with_dom(|dom| dom.get_element_by_id("a"))
      .ok_or(VmError::TypeError("missing #a"))?;
    let child_id = host.mutate_dom(|dom| (dom.create_element("span", ""), false));
    let parent_primary =
      host.with_dom(|dom| DomInterface::primary_for_node_kind(&dom.node(parent_id).kind));
    let child_primary =
      host.with_dom(|dom| DomInterface::primary_for_node_kind(&dom.node(child_id).kind));

    // `VmJsHostHooksPayload::set_embedder_state` stores a raw pointer; create hooks before borrowing
    // the window realm to avoid aliasing `&mut host`.
    let mut hooks = TestHooks::new(&mut host);

    let window = host.window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(realm.global_object());
    let document_obj = vm
      .user_data::<WindowRealmUserData>()
      .and_then(|data| data.document_obj())
      .ok_or(VmError::TypeError("missing window.document"))?;
    scope.push_root(Value::Object(document_obj))?;
    let document_key = WeakGcObject::from(document_obj);
    let parent_wrapper = {
      let wrapper =
        require_dom_platform_mut(vm)?.get_or_create_wrapper(&mut scope, document_key, parent_id, parent_primary)?;
      set_wrapper_document(&mut scope, wrapper, document_obj)?;
      scope.push_root(Value::Object(wrapper))?;
      wrapper
    };
    let child_wrapper = {
      let wrapper =
        require_dom_platform_mut(vm)?.get_or_create_wrapper(&mut scope, document_key, child_id, child_primary)?;
      set_wrapper_document(&mut scope, wrapper, document_obj)?;
      scope.push_root(Value::Object(wrapper))?;
      wrapper
    };

    let appended = vm.with_host_hooks_override(&mut hooks, |vm| {
      dispatch.call_operation(
        vm,
        &mut scope,
        Some(Value::Object(parent_wrapper)),
        "Node",
        "appendChild",
        0,
        &[Value::Object(child_wrapper)],
      )
    })?;
    assert_eq!(appended, Value::Object(child_wrapper));

    let parent_node = vm.with_host_hooks_override(&mut hooks, |vm| {
      dispatch.call_operation(
        vm,
        &mut scope,
        Some(Value::Object(child_wrapper)),
        "Node",
        "parentNode",
        0,
        &[],
      )
    })?;
    assert_eq!(parent_node, Value::Object(parent_wrapper));

    let first_child = vm.with_host_hooks_override(&mut hooks, |vm| {
      dispatch.call_operation(
        vm,
        &mut scope,
        Some(Value::Object(parent_wrapper)),
        "Node",
        "firstChild",
        0,
        &[],
      )
    })?;
    assert_eq!(first_child, Value::Object(child_wrapper));

    let child_nodes = vm.with_host_hooks_override(&mut hooks, |vm| {
      dispatch.call_operation(
        vm,
        &mut scope,
        Some(Value::Object(parent_wrapper)),
        "Node",
        "childNodes",
        0,
        &[],
      )
    })?;
    let Value::Object(child_nodes_list) = child_nodes else {
      return Err(VmError::TypeError("expected childNodes to return an object"));
    };
    assert_eq!(child_list_len(&mut scope, child_nodes_list)?, 1);
    assert_eq!(child_at(&mut scope, child_nodes_list, 0)?, Value::Object(child_wrapper));

    Ok(())
  }

  #[test]
  fn node_append_child_brand_check_rejects_plain_object() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<div id=a></div>").unwrap();
    let mut host =
      WindowHostState::new_with_fetcher(dom, "https://example.invalid/", default_test_fetcher())
        .unwrap();

    let parent_id = host
      .with_dom(|dom| dom.get_element_by_id("a"))
      .ok_or(VmError::TypeError("missing #a"))?;
    let parent_primary =
      host.with_dom(|dom| DomInterface::primary_for_node_kind(&dom.node(parent_id).kind));

    let mut hooks = TestHooks::new(&mut host);

    let window = host.window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(realm.global_object());
    let document_obj = vm
      .user_data::<WindowRealmUserData>()
      .and_then(|data| data.document_obj())
      .ok_or(VmError::TypeError("missing window.document"))?;
    scope.push_root(Value::Object(document_obj))?;
    let document_key = WeakGcObject::from(document_obj);
    let parent_wrapper = {
      let wrapper =
        require_dom_platform_mut(vm)?.get_or_create_wrapper(&mut scope, document_key, parent_id, parent_primary)?;
      set_wrapper_document(&mut scope, wrapper, document_obj)?;
      scope.push_root(Value::Object(wrapper))?;
      wrapper
    };

    let plain_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(plain_obj))?;
    let err = vm
      .with_host_hooks_override(&mut hooks, |vm| {
        dispatch.call_operation(
          vm,
          &mut scope,
          Some(Value::Object(parent_wrapper)),
          "Node",
          "appendChild",
          0,
          &[Value::Object(plain_obj)],
        )
      })
      .expect_err("expected brand check to throw");
    assert!(matches!(err, VmError::TypeError("Illegal invocation")));
    Ok(())
  }

  #[test]
  fn node_mutation_errors_throw_dom_exception_like() -> Result<(), VmError> {
    let dom = crate::dom2::parse_html("<div id=a></div>").unwrap();
    let mut host =
      WindowHostState::new_with_fetcher(dom, "https://example.invalid/", default_test_fetcher())
        .unwrap();

    let parent_text_id = host.mutate_dom(|dom| (dom.create_text("hi"), false));
    let child_id = host.mutate_dom(|dom| (dom.create_element("span", ""), false));
    let parent_primary =
      host.with_dom(|dom| DomInterface::primary_for_node_kind(&dom.node(parent_text_id).kind));
    let child_primary =
      host.with_dom(|dom| DomInterface::primary_for_node_kind(&dom.node(child_id).kind));

    let mut hooks = TestHooks::new(&mut host);

    let window = host.window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let mut dispatch = VmJsWebIdlBindingsHostDispatch::<WindowHostState>::new(realm.global_object());
    let document_obj = vm
      .user_data::<WindowRealmUserData>()
      .and_then(|data| data.document_obj())
      .ok_or(VmError::TypeError("missing window.document"))?;
    scope.push_root(Value::Object(document_obj))?;
    let document_key = WeakGcObject::from(document_obj);
    let text_wrapper = {
      let wrapper =
        require_dom_platform_mut(vm)?.get_or_create_wrapper(&mut scope, document_key, parent_text_id, parent_primary)?;
      set_wrapper_document(&mut scope, wrapper, document_obj)?;
      scope.push_root(Value::Object(wrapper))?;
      wrapper
    };
    let child_wrapper = {
      let wrapper =
        require_dom_platform_mut(vm)?.get_or_create_wrapper(&mut scope, document_key, child_id, child_primary)?;
      set_wrapper_document(&mut scope, wrapper, document_obj)?;
      scope.push_root(Value::Object(wrapper))?;
      wrapper
    };

    let err = vm
      .with_host_hooks_override(&mut hooks, |vm| {
        dispatch.call_operation(
          vm,
          &mut scope,
          Some(Value::Object(text_wrapper)),
          "Node",
          "appendChild",
          0,
          &[Value::Object(child_wrapper)],
        )
      })
      .expect_err("expected appendChild on Text to throw");
    assert!(matches!(err, VmError::Throw(_) | VmError::ThrowWithStack { .. }));
    Ok(())
  }
}
