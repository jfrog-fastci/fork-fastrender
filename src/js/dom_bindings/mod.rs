//! DOM core WebIDL-shaped JS bindings (MVP).
//!
//! This module builds a small DOM bindings layer on top of:
//! - [`crate::dom2::Document`] for the mutable DOM tree
//! - [`webidl_js_runtime::VmJsRuntime`] for a minimal JS value + object model
//!
//! The design is intentionally "spec-shaped": prototypes + constructors exist, methods are defined
//! on prototypes, and the host keeps platform object state in Rust-side tables (not JS-visible
//! internal slots).

use crate::dom::HTML_NAMESPACE;
use crate::dom2::{self, NodeId, NodeKind};
use crate::js::cookie_jar::{CookieJar, MAX_COOKIE_STRING_BYTES};
use crate::js::bindings::DomExceptionClass;
use crate::js::orchestrator::CurrentScriptState;
use crate::resource::ResourceFetcher;
use crate::web::events;
use rustc_hash::FxHashMap;
use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;
use vm_js::{PropertyDescriptorPatch, PropertyKey, RootId, Value, VmError, WeakGcObject};
use webidl_js_runtime::{JsRuntime as _, VmJsRuntime, WebIdlJsRuntime as _};

const CHILD_NODES_CACHE_PROP: &str = "__fastrender_childNodes";

#[derive(Debug, Clone)]
enum PlatformObjectKind {
  Window,
  Document { node_id: NodeId },
  Node { node_id: NodeId },
  Event { event_id: u64 },
}

#[derive(Clone, Copy)]
struct Prototypes {
  object: Value,
  event_target: Value,
  node: Value,
  element: Value,
  document: Value,
  event: Value,
  custom_event: Value,
  dom_exception: Value,
}

#[derive(Debug, Clone, Copy)]
struct ListenerEntry {
  callback: Value,
  callback_root: RootId,
}

type ActiveEventMap = Rc<RefCell<FxHashMap<u64, NonNull<events::Event>>>>;

/// A JS realm containing DOM core bindings backed by a mutable [`dom2::Document`].
pub struct DomJsRealm {
  rt: VmJsRuntime,
  window: Value,
  document: Value,

  document_node_id: NodeId,

  dom: Rc<RefCell<dom2::Document>>,
  current_script_state: Rc<RefCell<CurrentScriptState>>,
  cookie_jar: Rc<RefCell<CookieJar>>,
  document_url: Rc<RefCell<Option<String>>>,
  cookie_fetcher: Rc<RefCell<Option<Arc<dyn ResourceFetcher>>>>,

  platform_objects: Rc<RefCell<FxHashMap<WeakGcObject, PlatformObjectKind>>>,
  node_wrapper_cache: Rc<RefCell<FxHashMap<NodeId, WeakGcObject>>>,

  event_listeners: Rc<events::EventListenerRegistry>,
  listener_callbacks: Rc<RefCell<FxHashMap<events::ListenerId, ListenerEntry>>>,

  next_event_id: Rc<Cell<u64>>,
  events: Rc<RefCell<FxHashMap<u64, events::Event>>>,
  event_detail_roots: Rc<RefCell<FxHashMap<u64, RootId>>>,
  active_events: ActiveEventMap,

  prototypes: Prototypes,
}

impl DomJsRealm {
  pub fn new(dom: dom2::Document) -> Result<Self, VmError> {
    let dom = Rc::new(RefCell::new(dom));
    let document_node_id = dom.borrow().root();

    let current_script_state = Rc::new(RefCell::new(CurrentScriptState::default()));
    let cookie_jar: Rc<RefCell<CookieJar>> = Rc::new(RefCell::new(CookieJar::new()));
    let document_url: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let cookie_fetcher: Rc<RefCell<Option<Arc<dyn ResourceFetcher>>>> = Rc::new(RefCell::new(None));
    let platform_objects: Rc<RefCell<FxHashMap<WeakGcObject, PlatformObjectKind>>> =
      Rc::new(RefCell::new(FxHashMap::default()));
    let node_wrapper_cache: Rc<RefCell<FxHashMap<NodeId, WeakGcObject>>> =
      Rc::new(RefCell::new(FxHashMap::default()));

    let event_listeners = Rc::new(events::EventListenerRegistry::new());
    let listener_callbacks: Rc<RefCell<FxHashMap<events::ListenerId, ListenerEntry>>> =
      Rc::new(RefCell::new(FxHashMap::default()));
    let next_event_id: Rc<Cell<u64>> = Rc::new(Cell::new(1));
    let events_map: Rc<RefCell<FxHashMap<u64, events::Event>>> =
      Rc::new(RefCell::new(FxHashMap::default()));
    let event_detail_roots: Rc<RefCell<FxHashMap<u64, RootId>>> =
      Rc::new(RefCell::new(FxHashMap::default()));
    let active_events: ActiveEventMap = Rc::new(RefCell::new(FxHashMap::default()));

    let mut rt = VmJsRuntime::new();

    // Global object: we treat this as Window for MVP.
    let window = rt.alloc_object_value()?;
    let _ = rt.heap_mut().add_root(window)?;
    let Value::Object(window_obj) = window else {
      return Err(VmError::InvariantViolation(
        "alloc_object_value must return an object",
      ));
    };

    // Prototypes.
    let object_proto = rt.alloc_object_value()?;
    let _ = rt.heap_mut().add_root(object_proto)?;
    let event_target_proto = rt.alloc_object_value()?;
    let _ = rt.heap_mut().add_root(event_target_proto)?;
    let node_proto = rt.alloc_object_value()?;
    let _ = rt.heap_mut().add_root(node_proto)?;
    let element_proto = rt.alloc_object_value()?;
    let _ = rt.heap_mut().add_root(element_proto)?;
    let document_proto = rt.alloc_object_value()?;
    let _ = rt.heap_mut().add_root(document_proto)?;
    let event_proto = rt.alloc_object_value()?;
    let _ = rt.heap_mut().add_root(event_proto)?;
    let custom_event_proto = rt.alloc_object_value()?;
    let _ = rt.heap_mut().add_root(custom_event_proto)?;
    let dom_exception_proto = rt.alloc_object_value()?;
    let _ = rt.heap_mut().add_root(dom_exception_proto)?;

    rt.set_prototype(object_proto, None)?;
    rt.set_prototype(event_target_proto, Some(object_proto))?;
    rt.set_prototype(node_proto, Some(event_target_proto))?;
    rt.set_prototype(element_proto, Some(node_proto))?;
    rt.set_prototype(document_proto, Some(node_proto))?;
    rt.set_prototype(event_proto, Some(object_proto))?;
    rt.set_prototype(custom_event_proto, Some(event_proto))?;
    rt.set_prototype(dom_exception_proto, Some(object_proto))?;

    let prototypes = Prototypes {
      object: object_proto,
      event_target: event_target_proto,
      node: node_proto,
      element: element_proto,
      document: document_proto,
      event: event_proto,
      custom_event: custom_event_proto,
      dom_exception: dom_exception_proto,
    };

    // Window is an EventTarget.
    rt.set_prototype(window, Some(event_target_proto))?;

    // Document instance.
    let document = rt.alloc_object_value()?;
    let _ = rt.heap_mut().add_root(document)?;
    rt.set_prototype(document, Some(document_proto))?;
    let Value::Object(document_obj) = document else {
      return Err(VmError::InvariantViolation(
        "alloc_object_value must return an object",
      ));
    };

    // Platform-object bookkeeping (brand checks + identity).
    platform_objects
      .borrow_mut()
      .insert(WeakGcObject::from(window_obj), PlatformObjectKind::Window);
    platform_objects.borrow_mut().insert(
      WeakGcObject::from(document_obj),
      PlatformObjectKind::Document {
        node_id: document_node_id,
      },
    );
    node_wrapper_cache
      .borrow_mut()
      .insert(document_node_id, WeakGcObject::from(document_obj));

    // Attach `document` on the global object.
    define_data_property_str(&mut rt, window, "document", document, /* enumerable */ false)?;

    // Constructors (MVP: mostly non-constructable stubs).
    install_constructors(
      &mut rt,
      window,
      prototypes,
      platform_objects.clone(),
      event_listeners.clone(),
      listener_callbacks.clone(),
      next_event_id.clone(),
      events_map.clone(),
      event_detail_roots.clone(),
      active_events.clone(),
      dom.clone(),
      node_wrapper_cache.clone(),
      current_script_state.clone(),
      cookie_jar.clone(),
      document_url.clone(),
      cookie_fetcher.clone(),
    )?;

    Ok(Self {
      rt,
      window,
      document,
      document_node_id,
      dom,
      current_script_state,
      cookie_jar,
      document_url,
      cookie_fetcher,
      platform_objects,
      node_wrapper_cache,
      event_listeners,
      listener_callbacks,
      next_event_id,
      events: events_map,
      event_detail_roots,
      active_events,
      prototypes,
    })
  }

  pub fn runtime(&self) -> &VmJsRuntime {
    &self.rt
  }

  pub fn runtime_mut(&mut self) -> &mut VmJsRuntime {
    &mut self.rt
  }

  pub fn window(&self) -> Value {
    self.window
  }

  pub fn document(&self) -> Value {
    self.document
  }

  pub fn dom_exception_prototype(&self) -> Value {
    self.prototypes.dom_exception
  }

  pub fn dom(&self) -> Rc<RefCell<dom2::Document>> {
    self.dom.clone()
  }

  pub fn cookie_jar(&self) -> Rc<RefCell<CookieJar>> {
    self.cookie_jar.clone()
  }

  pub fn set_document_url(&mut self, url: Option<String>) {
    *self.document_url.borrow_mut() = url;
  }

  pub fn set_cookie_fetcher(&mut self, fetcher: Option<Arc<dyn ResourceFetcher>>) {
    *self.cookie_fetcher.borrow_mut() = fetcher;
  }

  pub fn set_cookie_fetcher_for_document(
    &mut self,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
  ) {
    *self.document_url.borrow_mut() = Some(document_url.into());
    *self.cookie_fetcher.borrow_mut() = Some(fetcher);
  }

  pub fn current_script_state(&self) -> Rc<RefCell<CurrentScriptState>> {
    self.current_script_state.clone()
  }

  pub fn wrap_node(&mut self, node_id: NodeId) -> Result<Value, VmError> {
    wrap_node(
      &mut self.rt,
      &self.dom,
      &self.platform_objects,
      &self.node_wrapper_cache,
      self.document_node_id,
      self.document,
      self.prototypes,
      node_id,
    )
  }

  /// Dispatch a Rust-owned DOM event into the realm, invoking JS `addEventListener` callbacks.
  ///
  /// This is intended for host-driven platform events (e.g. HTML lifecycle events like
  /// `DOMContentLoaded`/`load`) that are not triggered via `EventTarget.dispatchEvent(...)` from JS.
  ///
  /// Note: the internal JS `Event` wrapper objects are currently rooted permanently (matching the
  /// behavior of the JS `Event` constructor in this module). Since browsers fire only a handful of
  /// lifecycle events per document, this is acceptable for the MVP.
  pub fn dispatch_event_to_js(
    &mut self,
    target: events::EventTargetId,
    event: events::Event,
  ) -> Result<bool, VmError> {
    let dom = self.dom.clone();
    let platform_objects = self.platform_objects.clone();
    let node_wrapper_cache = self.node_wrapper_cache.clone();
    let event_listeners = self.event_listeners.clone();
    let listener_callbacks = self.listener_callbacks.clone();
    let events_map = self.events.clone();
    let event_detail_roots = self.event_detail_roots.clone();
    let active_events = self.active_events.clone();
    let prototypes = self.prototypes;
    let window = self.window;
    let document = self.document;
    let document_node_id = self.document_node_id;

    let rt = &mut self.rt;

    // Allocate a fresh Event wrapper identity and store the Rust `Event` in our per-realm table.
    let event_id = self.next_event_id.get();
    self.next_event_id.set(event_id.wrapping_add(1));
    let is_custom_event = event.detail.is_some();
    set_event_detail_root(rt, &event_detail_roots, event_id, event.detail)?;
    events_map.borrow_mut().insert(event_id, event);

    let event_value = rt.alloc_object_value()?;
    // The Rust event table is not traced by the GC, so keep wrapper objects rooted.
    let _ = rt.heap_mut().add_root(event_value)?;
    rt.set_prototype(
      event_value,
      Some(if is_custom_event {
        prototypes.custom_event
      } else {
        prototypes.event
      }),
    )?;
    let Value::Object(event_obj) = event_value else {
      return Err(VmError::InvariantViolation(
        "alloc_object_value must return an object",
      ));
    };
    platform_objects.borrow_mut().insert(
      WeakGcObject::from(event_obj),
      PlatformObjectKind::Event { event_id },
    );

    // Temporarily move the event out of the events table so we can hold an `&mut Event` for the
    // duration of dispatch.
    let mut event = events_map
      .borrow_mut()
      .remove(&event_id)
      .ok_or_else(|| VmError::InvariantViolation("missing event id in events table"))?;

    struct ActiveEventGuard {
      active: ActiveEventMap,
      id: u64,
    }
    impl Drop for ActiveEventGuard {
      fn drop(&mut self) {
        self.active.borrow_mut().remove(&self.id);
      }
    }

    struct JsInvoker {
      rt: *mut VmJsRuntime,
      listener_callbacks: Rc<RefCell<FxHashMap<events::ListenerId, ListenerEntry>>>,
      dom: Rc<RefCell<dom2::Document>>,
      platform_objects: Rc<RefCell<FxHashMap<WeakGcObject, PlatformObjectKind>>>,
      node_wrapper_cache: Rc<RefCell<FxHashMap<NodeId, WeakGcObject>>>,
      document_node_id: NodeId,
      window: Value,
      document: Value,
      prototypes: Prototypes,
      event_value: Value,
    }

    impl events::EventListenerInvoker for JsInvoker {
      fn invoke(
        &mut self,
        listener_id: events::ListenerId,
        event: &mut events::Event,
      ) -> std::result::Result<(), events::DomError> {
        let callback = self
          .listener_callbacks
          .borrow()
          .get(&listener_id)
          .map(|e| e.callback)
          .ok_or_else(|| events::DomError::new("unknown listener id"))?;

        let current_target = event
          .current_target
          .ok_or_else(|| events::DomError::new("missing currentTarget during dispatch"))?;

        // SAFETY: `rt` is borrowed mutably by the caller while this invoker is alive.
        let rt = unsafe { &mut *self.rt };
        let current_target_wrapper = wrap_event_target(
          rt,
          &self.dom,
          &self.platform_objects,
          &self.node_wrapper_cache,
          self.document_node_id,
          self.window,
          self.document,
          self.prototypes,
          current_target,
        )
        .map_err(|e| events::DomError::new(format!("{e:?}")))?;

        if rt.is_callable(callback) {
          rt
            .call_function(callback, current_target_wrapper, &[self.event_value])
            .map_err(|e| events::DomError::new(format!("{e:?}")))?;
        } else {
          // Support callback objects with a `handleEvent` method.
          let handle_event_key = prop_key_str(rt, "handleEvent")
            .map_err(|e| events::DomError::new(format!("{e:?}")))?;
          let handle_event = rt
            .get(callback, handle_event_key)
            .map_err(|e| events::DomError::new(format!("{e:?}")))?;
          if !rt.is_callable(handle_event) {
            return Err(events::DomError::new(
              "EventTarget listener callback has no callable handleEvent",
            ));
          }
          rt
            .call_function(handle_event, callback, &[self.event_value])
            .map_err(|e| events::DomError::new(format!("{e:?}")))?;
        }
        Ok(())
      }
    }

    let mut invoker = JsInvoker {
      rt: rt as *mut VmJsRuntime,
      listener_callbacks: listener_callbacks.clone(),
      dom: dom.clone(),
      platform_objects: platform_objects.clone(),
      node_wrapper_cache: node_wrapper_cache.clone(),
      document_node_id,
      window,
      document,
      prototypes,
      event_value,
    };

    let result = {
      // Register the Event as active so Event.prototype methods can mutate it during callback
      // invocation without tripping RefCell reentrancy.
      {
        let ptr = NonNull::from(&mut event);
        active_events.borrow_mut().insert(event_id, ptr);
      }
      let _active_guard = ActiveEventGuard {
        active: active_events.clone(),
        id: event_id,
      };

      let dom_ref = dom.borrow();
      events::dispatch_event(target, &mut event, &dom_ref, &event_listeners, &mut invoker)
    };

    // Persist updated event state after dispatch.
    events_map.borrow_mut().insert(event_id, event);

    // `events::dispatch_event` may remove listeners during dispatch (e.g. `{ once: true }`). Clean
    // up any callback roots for listener IDs that are now unreferenced.
    {
      let stale_ids: Vec<events::ListenerId> = listener_callbacks
        .borrow()
        .keys()
        .copied()
        .filter(|id| !event_listeners.contains_listener_id(*id))
        .collect();
      if !stale_ids.is_empty() {
        let mut callbacks = listener_callbacks.borrow_mut();
        for id in stale_ids {
          if let Some(entry) = callbacks.remove(&id) {
            rt.heap_mut().remove_root(entry.callback_root);
          }
        }
      }
    }

    match result {
      Ok(not_canceled) => Ok(not_canceled),
      Err(err) => Err(rt.throw_type_error(&err.to_string())),
    }
  }
}

fn prop_key_str(rt: &mut VmJsRuntime, name: &str) -> Result<PropertyKey, VmError> {
  let Value::String(s) = rt.alloc_string_value(name)? else {
    return Err(VmError::InvariantViolation(
      "alloc_string_value must return a string",
    ));
  };
  Ok(PropertyKey::String(s))
}

fn define_data_property_str(
  rt: &mut VmJsRuntime,
  obj: Value,
  name: &str,
  value: Value,
  enumerable: bool,
) -> Result<(), VmError> {
  let key = prop_key_str(rt, name)?;
  rt.define_data_property(obj, key, value, enumerable)
}

fn define_method(
  rt: &mut VmJsRuntime,
  proto: Value,
  name: &str,
  f: Value,
) -> Result<(), VmError> {
  define_data_property_str(rt, proto, name, f, /* enumerable */ false)
}

fn define_accessor(
  rt: &mut VmJsRuntime,
  proto: Value,
  name: &str,
  get: Value,
  set: Value,
) -> Result<(), VmError> {
  let key = prop_key_str(rt, name)?;
  rt.define_accessor_property(proto, key, get, set, /* enumerable */ false)
}

fn to_rust_string(rt: &mut VmJsRuntime, v: Value) -> Result<String, VmError> {
  let v = rt.to_string(v)?;
  rt.string_to_utf8_lossy(v)
}

fn selector_mentions_scope(selectors: &str) -> bool {
  selectors
    .as_bytes()
    .windows(6)
    .any(|w| w.eq_ignore_ascii_case(b":scope"))
}

fn is_html_namespace(namespace: &str) -> bool {
  namespace.is_empty() || namespace == HTML_NAMESPACE
}

fn get_text_content(dom: &dom2::Document, root: NodeId) -> String {
  match &dom.node(root).kind {
    NodeKind::Text { content } => return content.clone(),
    NodeKind::Comment { content } => return content.clone(),
    NodeKind::ProcessingInstruction { data, .. } => return data.clone(),
    _ => {}
  }

  let mut out = String::new();
  for id in dom.subtree_preorder(root) {
    if let NodeKind::Text { content } = &dom.node(id).kind {
      out.push_str(content);
    }
  }
  out
}

fn direct_child_nodes(dom: &dom2::Document, parent: NodeId) -> Result<Vec<NodeId>, dom2::DomError> {
  let Some(parent_node) = dom.nodes().get(parent.index()) else {
    return Err(dom2::DomError::NotFoundError);
  };
  // In `dom2`, `<template>` contents are represented as regular descendants but are marked inert on
  // the `<template>` element itself. DOM tree navigation APIs (childNodes/firstChild/etc.) must not
  // traverse into these inert subtrees.
  if parent_node.inert_subtree {
    return Ok(Vec::new());
  }
  Ok(
    dom
      .children(parent)?
      .iter()
      .copied()
      .filter(|&child| {
        child.index() < dom.nodes_len() && dom.node(child).parent == Some(parent)
      })
      .collect(),
  )
}

fn direct_element_children(
  dom: &dom2::Document,
  parent: NodeId,
) -> Result<Vec<NodeId>, dom2::DomError> {
  Ok(
    direct_child_nodes(dom, parent)?
      .into_iter()
      .filter(|&child| {
        matches!(
          dom.node(child).kind,
          NodeKind::Element { .. } | NodeKind::Slot { .. }
        )
      })
      .collect(),
  )
}

fn maybe_refresh_cached_child_nodes(
  rt: &mut VmJsRuntime,
  dom: &Rc<RefCell<dom2::Document>>,
  platform_objects: &Rc<RefCell<FxHashMap<WeakGcObject, PlatformObjectKind>>>,
  node_wrapper_cache: &Rc<RefCell<FxHashMap<NodeId, WeakGcObject>>>,
  document_node_id: NodeId,
  document: Value,
  prototypes: Prototypes,
  node_id: NodeId,
) -> Result<(), VmError> {
  let Some(wrapper_obj) = node_wrapper_cache.borrow().get(&node_id).copied() else {
    return Ok(());
  };
  let Some(wrapper_obj) = wrapper_obj.upgrade(rt.heap()) else {
    return Ok(());
  };
  let wrapper = Value::Object(wrapper_obj);

  let cache_key = prop_key_str(rt, CHILD_NODES_CACHE_PROP)?;
  let cached = rt.get(wrapper, cache_key)?;
  let Value::Object(array_obj) = cached else {
    return Ok(());
  };

  // Snapshot child wrappers first; the array update logic below borrows the heap mutably via
  // `Scope`, so we must not call into `wrap_node` while the scope is alive.
  let child_ids = {
    let dom_ref = dom.borrow();
    direct_child_nodes(&dom_ref, node_id)
      .map_err(|e| rt.throw_type_error(&format!("childNodes refresh: {e}")))?
  };
  let mut child_wrappers: Vec<Value> = Vec::new();
  for child_id in child_ids {
    child_wrappers.push(wrap_node(
      rt,
      dom,
      platform_objects,
      node_wrapper_cache,
      document_node_id,
      document,
      prototypes,
      child_id,
    )?);
  }

  // Mutate the cached array in-place so stored references behave like a live NodeList.
  let mut scope = rt.heap_mut().scope();
  let length_key = PropertyKey::String(scope.alloc_string("length")?);
  let _ = scope.define_own_property(
    array_obj,
    length_key,
    PropertyDescriptorPatch {
      value: Some(Value::Number(0.0)),
      writable: Some(true),
      ..Default::default()
    },
  )?;
  for (idx, value) in child_wrappers.into_iter().enumerate() {
    let key = PropertyKey::String(scope.alloc_string(&idx.to_string())?);
    scope.create_data_property_or_throw(array_obj, key, value)?;
  }

  Ok(())
}

fn set_text_content(dom: &mut dom2::Document, node: NodeId, value: &str) -> Result<(), dom2::DomError> {
  match &mut dom.node_mut(node).kind {
    NodeKind::Text { content } | NodeKind::Comment { content } => {
      content.clear();
      content.push_str(value);
      return Ok(());
    }
    NodeKind::ProcessingInstruction { data, .. } => {
      data.clear();
      data.push_str(value);
      return Ok(());
    }
    NodeKind::Doctype { .. } => {
      // `DocumentType.textContent` is `null` in the DOM spec; setting it is a no-op.
      return Ok(());
    }
    NodeKind::Document { .. }
    | NodeKind::DocumentFragment
    | NodeKind::Element { .. }
    | NodeKind::Slot { .. }
    | NodeKind::ShadowRoot { .. } => {
      // Replace children.
    }
  }

  let children: Vec<NodeId> = dom.children(node)?.to_vec();
  for child in children {
    dom.remove_child(node, child)?;
  }

  if !value.is_empty() {
    let text = dom.create_text(value);
    dom.append_child(node, text)?;
  }

  Ok(())
}

fn create_dom_exception_object(
  rt: &mut VmJsRuntime,
  dom_exception_proto: Value,
  name: &str,
  message: &str,
) -> Value {
  let obj = match rt.alloc_object_value() {
    Ok(v) => v,
    Err(_) => return Value::Undefined,
  };

  let _ = rt.set_prototype(obj, Some(dom_exception_proto));

  if let Ok(name_value) = rt.alloc_string_value(name) {
    let _ = define_data_property_str(rt, obj, "name", name_value, false);
  }
  if let Ok(message_value) = rt.alloc_string_value(message) {
    let _ = define_data_property_str(rt, obj, "message", message_value, false);
  }

  obj
}

pub(crate) fn throw_dom_exception(
  rt: &mut VmJsRuntime,
  dom_exception_proto: Value,
  name: &str,
  message: &str,
) -> VmError {
  VmError::Throw(create_dom_exception_object(
    rt,
    dom_exception_proto,
    name,
    message,
  ))
}

pub(crate) fn throw_dom_error(
  rt: &mut VmJsRuntime,
  dom_exception_proto: Value,
  err: dom2::DomError,
) -> VmError {
  let name = err.code();
  throw_dom_exception(rt, dom_exception_proto, name, name)
}

fn parse_add_event_listener_options(rt: &mut VmJsRuntime, options: Value) -> Result<events::AddEventListenerOptions, VmError> {
  if matches!(options, Value::Undefined) {
    return Ok(events::AddEventListenerOptions::default());
  }

  // The IDL signature is `optional (AddEventListenerOptions or boolean)`.
  match options {
    Value::Bool(capture) => Ok(events::AddEventListenerOptions {
      capture,
      ..Default::default()
    }),
    Value::Object(_) => {
      let capture_key = prop_key_str(rt, "capture")?;
      let capture_value = rt.get(options, capture_key)?;
      let capture = rt.to_boolean(capture_value)?;

      let once_key = prop_key_str(rt, "once")?;
      let once_value = rt.get(options, once_key)?;
      let once = rt.to_boolean(once_value)?;

      let passive_key = prop_key_str(rt, "passive")?;
      let passive_value = rt.get(options, passive_key)?;
      let passive = rt.to_boolean(passive_value)?;
      Ok(events::AddEventListenerOptions {
        capture,
        once,
        passive,
      })
    }
    other => Ok(events::AddEventListenerOptions {
      capture: rt.to_boolean(other)?,
      ..Default::default()
    }),
  }
}

fn parse_event_listener_capture(rt: &mut VmJsRuntime, options: Value) -> Result<bool, VmError> {
  if matches!(options, Value::Undefined) {
    return Ok(false);
  }
  match options {
    Value::Bool(capture) => Ok(capture),
    Value::Object(_) => {
      let capture_key = prop_key_str(rt, "capture")?;
      let capture_value = rt.get(options, capture_key)?;
      rt.to_boolean(capture_value)
    }
    other => rt.to_boolean(other),
  }
}

fn listener_id_from_callback(
  rt: &mut VmJsRuntime,
  callback: Value,
) -> Result<Option<events::ListenerId>, VmError> {
  match callback {
    Value::Undefined | Value::Null => Ok(None),
    Value::Object(obj) => {
      let id = (obj.id().index() as u64) | ((obj.id().generation() as u64) << 32);
      Ok(Some(events::ListenerId::new(id)))
    }
    _ => Err(rt.throw_type_error(
      "EventTarget listener callback must be a callable or an object with handleEvent",
    )),
  }
}

fn validate_event_listener_callback(rt: &mut VmJsRuntime, callback: Value) -> Result<(), VmError> {
  if matches!(callback, Value::Undefined | Value::Null) {
    return Ok(());
  }

  if rt.is_callable(callback) {
    return Ok(());
  }

  let Value::Object(_) = callback else {
    return Err(rt.throw_type_error(
      "EventTarget listener callback must be a callable or an object with handleEvent",
    ));
  };

  let handle_event_key = prop_key_str(rt, "handleEvent")?;
  let handle_event = rt.get(callback, handle_event_key)?;
  if rt.is_callable(handle_event) {
    return Ok(());
  }

  Err(rt.throw_type_error(
    "EventTarget listener callback is not callable and has no callable handleEvent",
  ))
}

fn extract_event_target_id(
  rt: &mut VmJsRuntime,
  platform_objects: &Rc<RefCell<FxHashMap<WeakGcObject, PlatformObjectKind>>>,
  this: Value,
) -> Result<events::EventTargetId, VmError> {
  let Value::Object(obj) = this else {
    return Err(rt.throw_type_error("Illegal invocation"));
  };
  let key = WeakGcObject::from(obj);
  let map = platform_objects.borrow();
  match map.get(&key) {
    Some(PlatformObjectKind::Window) => Ok(events::EventTargetId::Window),
    Some(PlatformObjectKind::Document { .. }) => Ok(events::EventTargetId::Document),
    Some(PlatformObjectKind::Node { node_id }) => Ok(events::EventTargetId::Node(*node_id)),
    _ => Err(rt.throw_type_error("Illegal invocation")),
  }
}

fn extract_node_id(
  rt: &mut VmJsRuntime,
  platform_objects: &Rc<RefCell<FxHashMap<WeakGcObject, PlatformObjectKind>>>,
  this: Value,
) -> Result<NodeId, VmError> {
  let Value::Object(obj) = this else {
    return Err(rt.throw_type_error("Illegal invocation"));
  };
  let key = WeakGcObject::from(obj);
  let map = platform_objects.borrow();
  match map.get(&key) {
    Some(PlatformObjectKind::Document { node_id }) => Ok(*node_id),
    Some(PlatformObjectKind::Node { node_id }) => Ok(*node_id),
    _ => Err(rt.throw_type_error("Illegal invocation")),
  }
}

fn extract_document_id(
  rt: &mut VmJsRuntime,
  platform_objects: &Rc<RefCell<FxHashMap<WeakGcObject, PlatformObjectKind>>>,
  this: Value,
) -> Result<NodeId, VmError> {
  let Value::Object(obj) = this else {
    return Err(rt.throw_type_error("Illegal invocation"));
  };
  let key = WeakGcObject::from(obj);
  let map = platform_objects.borrow();
  match map.get(&key) {
    Some(PlatformObjectKind::Document { node_id }) => Ok(*node_id),
    _ => Err(rt.throw_type_error("Illegal invocation")),
  }
}

fn extract_event_id(
  rt: &mut VmJsRuntime,
  platform_objects: &Rc<RefCell<FxHashMap<WeakGcObject, PlatformObjectKind>>>,
  this: Value,
) -> Result<u64, VmError> {
  let Value::Object(obj) = this else {
    return Err(rt.throw_type_error("Illegal invocation"));
  };
  let key = WeakGcObject::from(obj);
  let map = platform_objects.borrow();
  match map.get(&key) {
    Some(PlatformObjectKind::Event { event_id }) => Ok(*event_id),
    _ => Err(rt.throw_type_error("Illegal invocation")),
  }
}

fn with_event<R>(
  rt: &mut VmJsRuntime,
  active_events: &ActiveEventMap,
  events_map: &Rc<RefCell<FxHashMap<u64, events::Event>>>,
  event_id: u64,
  f: impl FnOnce(&mut events::Event) -> R,
) -> Result<R, VmError> {
  let active = { active_events.borrow().get(&event_id).copied() };
  if let Some(ptr) = active {
    // SAFETY: entries in `active_events` are only created by `dispatchEvent` and removed once
    // dispatch completes. While present, the pointer is valid.
    let event = unsafe { &mut *ptr.as_ptr() };
    return Ok(f(event));
  }

  let mut map = events_map.borrow_mut();
  let event = map
    .get_mut(&event_id)
    .ok_or_else(|| rt.throw_type_error("Event is no longer active"))?;
  Ok(f(event))
}

fn value_needs_gc_root(value: &Value) -> bool {
  matches!(*value, Value::String(_) | Value::Symbol(_) | Value::Object(_))
}

fn set_event_detail_root(
  rt: &mut VmJsRuntime,
  event_detail_roots: &Rc<RefCell<FxHashMap<u64, RootId>>>,
  event_id: u64,
  detail: Option<Value>,
) -> Result<(), VmError> {
  let old_root = event_detail_roots.borrow_mut().remove(&event_id);
  if let Some(old_root) = old_root {
    rt.heap_mut().remove_root(old_root);
  }

  let Some(detail) = detail.filter(value_needs_gc_root) else {
    return Ok(());
  };
  let root = rt.heap_mut().add_root(detail)?;
  event_detail_roots.borrow_mut().insert(event_id, root);
  Ok(())
}

fn wrap_event_target(
  rt: &mut VmJsRuntime,
  dom: &Rc<RefCell<dom2::Document>>,
  platform_objects: &Rc<RefCell<FxHashMap<WeakGcObject, PlatformObjectKind>>>,
  node_wrapper_cache: &Rc<RefCell<FxHashMap<NodeId, WeakGcObject>>>,
  document_node_id: NodeId,
  window: Value,
  document: Value,
  prototypes: Prototypes,
  target: events::EventTargetId,
) -> Result<Value, VmError> {
  match target {
    events::EventTargetId::Window => Ok(window),
    events::EventTargetId::Document => Ok(document),
    events::EventTargetId::Node(node_id) => wrap_node(
      rt,
      dom,
      platform_objects,
      node_wrapper_cache,
      document_node_id,
      document,
      prototypes,
      node_id,
    ),
  }
}

fn wrap_node(
  rt: &mut VmJsRuntime,
  dom: &Rc<RefCell<dom2::Document>>,
  platform_objects: &Rc<RefCell<FxHashMap<WeakGcObject, PlatformObjectKind>>>,
  node_wrapper_cache: &Rc<RefCell<FxHashMap<NodeId, WeakGcObject>>>,
  document_node_id: NodeId,
  document: Value,
  prototypes: Prototypes,
  node_id: NodeId,
) -> Result<Value, VmError> {
  if node_id == document_node_id {
    return Ok(document);
  }

  if node_id.index() >= dom.borrow().nodes_len() {
    return Err(rt.throw_type_error("NotFoundError"));
  }

  let existing = { node_wrapper_cache.borrow().get(&node_id).copied() };
  if let Some(existing) = existing {
    if let Some(obj) = existing.upgrade(rt.heap()) {
      return Ok(Value::Object(obj));
    }
    // Stale wrapper: keep brand-check tables from growing without bound across GC cycles.
    platform_objects.borrow_mut().remove(&existing);
  }

  let proto = {
    let dom = dom.borrow();
    match &dom.node(node_id).kind {
      NodeKind::Element { .. } | NodeKind::Slot { .. } => prototypes.element,
      _ => prototypes.node,
    }
  };

  let obj = rt.alloc_object_value()?;
  rt.set_prototype(obj, Some(proto))?;
  let Value::Object(obj_handle) = obj else {
    return Err(VmError::InvariantViolation(
      "alloc_object_value must return an object",
    ));
  };

  node_wrapper_cache
    .borrow_mut()
    .insert(node_id, WeakGcObject::from(obj_handle));
  platform_objects.borrow_mut().insert(
    WeakGcObject::from(obj_handle),
    PlatformObjectKind::Node { node_id },
  );

  Ok(obj)
}

fn install_constructors(
  rt: &mut VmJsRuntime,
  global: Value,
  prototypes: Prototypes,
  platform_objects: Rc<RefCell<FxHashMap<WeakGcObject, PlatformObjectKind>>>,
  event_listeners: Rc<events::EventListenerRegistry>,
  listener_callbacks: Rc<RefCell<FxHashMap<events::ListenerId, ListenerEntry>>>,
  next_event_id: Rc<Cell<u64>>,
  events_map: Rc<RefCell<FxHashMap<u64, events::Event>>>,
  event_detail_roots: Rc<RefCell<FxHashMap<u64, RootId>>>,
  active_events: ActiveEventMap,
  dom: Rc<RefCell<dom2::Document>>,
  node_wrapper_cache: Rc<RefCell<FxHashMap<NodeId, WeakGcObject>>>,
  current_script_state: Rc<RefCell<CurrentScriptState>>,
  cookie_jar: Rc<RefCell<CookieJar>>,
  document_url: Rc<RefCell<Option<String>>>,
  cookie_fetcher: Rc<RefCell<Option<Arc<dyn ResourceFetcher>>>>,
) -> Result<(), VmError> {
  fn illegal_constructor(rt: &mut VmJsRuntime, name: &'static str) -> Result<Value, VmError> {
    Err(rt.throw_type_error(&format!("{name} is not a constructor")))
  }

  let window = global;
  let document_key = prop_key_str(rt, "document")?;
  let document = rt.get(global, document_key)?;
  let document_node_id = dom.borrow().root();

  // Minimal DOMException implementation needed for spec-shaped selector errors.
  //
  // Reuse the realm's `DOMException.prototype` so any DOMException objects thrown from either
  // selector APIs or `dom2::DomError` mapping share the same prototype chain.
  let dom_exception = DomExceptionClass::install_with_prototype(rt, global, prototypes.dom_exception)?;

  // EventTarget / Node / Element / Document constructors: non-constructable stubs.
  let event_target_ctor = rt.alloc_function_value(|rt, _this, _args| illegal_constructor(rt, "EventTarget"))?;
  let node_ctor = rt.alloc_function_value(|rt, _this, _args| illegal_constructor(rt, "Node"))?;
  let element_ctor = rt.alloc_function_value(|rt, _this, _args| illegal_constructor(rt, "Element"))?;
  let document_ctor = rt.alloc_function_value(|rt, _this, _args| illegal_constructor(rt, "Document"))?;

  define_data_property_str(rt, event_target_ctor, "prototype", prototypes.event_target, false)?;
  define_data_property_str(rt, node_ctor, "prototype", prototypes.node, false)?;
  define_data_property_str(rt, element_ctor, "prototype", prototypes.element, false)?;
  define_data_property_str(rt, document_ctor, "prototype", prototypes.document, false)?;

  define_data_property_str(rt, global, "EventTarget", event_target_ctor, false)?;
  define_data_property_str(rt, global, "Node", node_ctor, false)?;
  define_data_property_str(rt, global, "Element", element_ctor, false)?;
  define_data_property_str(rt, global, "Document", document_ctor, false)?;

  // `Node` nodeType constants (MVP).
  //
  // Exposing these makes it possible to write simple scripts/tests that branch on `nodeType` without
  // hardcoding magic numbers.
  define_data_property_str(rt, node_ctor, "ELEMENT_NODE", Value::Number(1.0), false)?;
  define_data_property_str(rt, node_ctor, "ATTRIBUTE_NODE", Value::Number(2.0), false)?;
  define_data_property_str(rt, node_ctor, "TEXT_NODE", Value::Number(3.0), false)?;
  define_data_property_str(rt, node_ctor, "CDATA_SECTION_NODE", Value::Number(4.0), false)?;
  define_data_property_str(rt, node_ctor, "ENTITY_REFERENCE_NODE", Value::Number(5.0), false)?;
  define_data_property_str(rt, node_ctor, "ENTITY_NODE", Value::Number(6.0), false)?;
  define_data_property_str(
    rt,
    node_ctor,
    "PROCESSING_INSTRUCTION_NODE",
    Value::Number(7.0),
    false,
  )?;
  define_data_property_str(rt, node_ctor, "COMMENT_NODE", Value::Number(8.0), false)?;
  define_data_property_str(rt, node_ctor, "DOCUMENT_NODE", Value::Number(9.0), false)?;
  define_data_property_str(rt, node_ctor, "DOCUMENT_TYPE_NODE", Value::Number(10.0), false)?;
  define_data_property_str(rt, node_ctor, "DOCUMENT_FRAGMENT_NODE", Value::Number(11.0), false)?;
  define_data_property_str(rt, node_ctor, "NOTATION_NODE", Value::Number(12.0), false)?;

  // Event constructor: produces a platform-backed Event object.
  let event_proto = prototypes.event;
  let event_ctor = {
    let platform_objects = platform_objects.clone();
    let next_event_id = next_event_id.clone();
    let events_map = events_map.clone();
    rt.alloc_function_value(move |rt, _this, args| {
      let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
      let type_ = to_rust_string(rt, type_arg)?;

      let mut init = events::EventInit::default();
      if let Some(init_value) = args.get(1).copied() {
        if matches!(init_value, Value::Object(_)) {
          let bubbles_key = prop_key_str(rt, "bubbles")?;
          let bubbles_value = rt.get(init_value, bubbles_key)?;
          init.bubbles = rt.to_boolean(bubbles_value)?;

          let cancelable_key = prop_key_str(rt, "cancelable")?;
          let cancelable_value = rt.get(init_value, cancelable_key)?;
          init.cancelable = rt.to_boolean(cancelable_value)?;

          let composed_key = prop_key_str(rt, "composed")?;
          let composed_value = rt.get(init_value, composed_key)?;
          init.composed = rt.to_boolean(composed_value)?;
        }
      }

      let event_id = next_event_id.get();
      next_event_id.set(event_id.wrapping_add(1));
      events_map
        .borrow_mut()
        .insert(event_id, events::Event::new(type_, init));

      let obj = rt.alloc_object_value()?;
      // Keep Event wrapper objects alive even when only referenced from Rust-side tables.
      let _ = rt.heap_mut().add_root(obj)?;
      rt.set_prototype(obj, Some(event_proto))?;
      let Value::Object(obj_handle) = obj else {
        return Err(VmError::InvariantViolation(
          "alloc_object_value must return an object",
        ));
      };
      platform_objects
        .borrow_mut()
        .insert(WeakGcObject::from(obj_handle), PlatformObjectKind::Event { event_id });
      Ok(obj)
    })?
  };
  define_data_property_str(rt, event_ctor, "prototype", event_proto, false)?;
  define_data_property_str(rt, global, "Event", event_ctor, false)?;

  // CustomEvent constructor: produces a platform-backed CustomEvent object.
  //
  // Note: like Event above, this does not currently enforce `new` (calling as a function returns a
  // new object). The MVP binding layer prioritizes compatibility over strict WebIDL `[[Call]]`
  // semantics.
  let custom_event_proto = prototypes.custom_event;
  let custom_event_ctor = {
    let platform_objects = platform_objects.clone();
    let next_event_id = next_event_id.clone();
    let events_map = events_map.clone();
    let event_detail_roots = event_detail_roots.clone();
    rt.alloc_function_value(move |rt, _this, args| {
      let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
      let type_ = to_rust_string(rt, type_arg)?;

      let mut init = events::CustomEventInit::default();
      if let Some(init_value) = args.get(1).copied() {
        if matches!(init_value, Value::Object(_)) {
          let bubbles_key = prop_key_str(rt, "bubbles")?;
          let bubbles_value = rt.get(init_value, bubbles_key)?;
          init.bubbles = rt.to_boolean(bubbles_value)?;

          let cancelable_key = prop_key_str(rt, "cancelable")?;
          let cancelable_value = rt.get(init_value, cancelable_key)?;
          init.cancelable = rt.to_boolean(cancelable_value)?;

          let composed_key = prop_key_str(rt, "composed")?;
          let composed_value = rt.get(init_value, composed_key)?;
          init.composed = rt.to_boolean(composed_value)?;

          let detail_key = prop_key_str(rt, "detail")?;
          let detail_value = rt.get(init_value, detail_key)?;
          // WebIDL dictionary conversion treats `undefined` as "missing", so use the default.
          init.detail = if matches!(detail_value, Value::Undefined) {
            Value::Null
          } else {
            detail_value
          };
        }
      }

      let event = events::Event::new_custom_event(type_, init);
      let event_id = next_event_id.get();
      next_event_id.set(event_id.wrapping_add(1));
      set_event_detail_root(rt, &event_detail_roots, event_id, event.detail)?;
      events_map.borrow_mut().insert(event_id, event);

      let obj = rt.alloc_object_value()?;
      // Keep Event wrapper objects alive even when only referenced from Rust-side tables.
      let _ = rt.heap_mut().add_root(obj)?;
      rt.set_prototype(obj, Some(custom_event_proto))?;
      let Value::Object(obj_handle) = obj else {
        return Err(VmError::InvariantViolation(
          "alloc_object_value must return an object",
        ));
      };
      platform_objects
        .borrow_mut()
        .insert(WeakGcObject::from(obj_handle), PlatformObjectKind::Event { event_id });
      Ok(obj)
    })?
  };
  define_data_property_str(rt, custom_event_ctor, "prototype", custom_event_proto, false)?;
  define_data_property_str(rt, global, "CustomEvent", custom_event_ctor, false)?;

  // EventTarget.prototype
  {
    let platform_objects_for_add = platform_objects.clone();
    let event_listeners_for_add = event_listeners.clone();
    let listener_callbacks_for_add = listener_callbacks.clone();
    let add = rt.alloc_function_value(move |rt, this, args| {
      let target = extract_event_target_id(rt, &platform_objects_for_add, this)?;
      let type_ = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;

      let callback = args.get(1).copied().unwrap_or(Value::Undefined);
      validate_event_listener_callback(rt, callback)?;
      let Some(listener_id) = listener_id_from_callback(rt, callback)? else {
        return Ok(Value::Undefined);
      };

      let options = parse_add_event_listener_options(rt, args.get(2).copied().unwrap_or(Value::Undefined))?;

      {
        let mut callbacks = listener_callbacks_for_add.borrow_mut();
        if !callbacks.contains_key(&listener_id) {
          let root = rt.heap_mut().add_root(callback)?;
          callbacks.insert(
            listener_id,
            ListenerEntry {
              callback,
              callback_root: root,
            },
          );
        }
      }
      let _ = event_listeners_for_add.add_event_listener(target, &type_, listener_id, options);
      Ok(Value::Undefined)
    })?;
    define_method(rt, prototypes.event_target, "addEventListener", add)?;

    let platform_objects_for_remove = platform_objects.clone();
    let event_listeners_for_remove = event_listeners.clone();
    let listener_callbacks_for_remove = listener_callbacks.clone();
    let remove = rt.alloc_function_value(move |rt, this, args| {
      let target = extract_event_target_id(rt, &platform_objects_for_remove, this)?;
      let type_ = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;

      let callback = args.get(1).copied().unwrap_or(Value::Undefined);
      validate_event_listener_callback(rt, callback)?;
      let Some(listener_id) = listener_id_from_callback(rt, callback)? else {
        return Ok(Value::Undefined);
      };

      let capture = parse_event_listener_capture(rt, args.get(2).copied().unwrap_or(Value::Undefined))?;
      let removed = event_listeners_for_remove.remove_event_listener(target, &type_, listener_id, capture);
      if removed && !event_listeners_for_remove.contains_listener_id(listener_id) {
        if let Some(entry) = listener_callbacks_for_remove.borrow_mut().remove(&listener_id) {
          rt.heap_mut().remove_root(entry.callback_root);
        }
      }
      Ok(Value::Undefined)
    })?;
    define_method(rt, prototypes.event_target, "removeEventListener", remove)?;

    let platform_objects_for_dispatch = platform_objects.clone();
    let event_listeners_for_dispatch = event_listeners.clone();
    let listener_callbacks_for_dispatch = listener_callbacks.clone();
    let events_map_for_dispatch = events_map.clone();
    let active_events_for_dispatch = active_events.clone();
    let dom_for_dispatch = dom.clone();
    let node_wrapper_cache_for_dispatch = node_wrapper_cache.clone();

    let dispatch = rt.alloc_function_value(move |rt, this, args| {
      let target = extract_event_target_id(rt, &platform_objects_for_dispatch, this)?;
      let event_value = args.get(0).copied().unwrap_or(Value::Undefined);

      let event_id = extract_event_id(rt, &platform_objects_for_dispatch, event_value)?;

      let mut event = events_map_for_dispatch
        .borrow_mut()
        .remove(&event_id)
        .ok_or_else(|| rt.throw_type_error("dispatchEvent: unknown Event"))?;

      struct ActiveEventGuard {
        active: ActiveEventMap,
        id: u64,
      }
      impl Drop for ActiveEventGuard {
        fn drop(&mut self) {
          self.active.borrow_mut().remove(&self.id);
        }
      }

      struct JsInvoker {
        rt: *mut VmJsRuntime,
        listener_callbacks: Rc<RefCell<FxHashMap<events::ListenerId, ListenerEntry>>>,
        dom: Rc<RefCell<dom2::Document>>,
        platform_objects: Rc<RefCell<FxHashMap<WeakGcObject, PlatformObjectKind>>>,
        node_wrapper_cache: Rc<RefCell<FxHashMap<NodeId, WeakGcObject>>>,
        document_node_id: NodeId,
        window: Value,
        document: Value,
        prototypes: Prototypes,
        event_value: Value,
      }

      impl events::EventListenerInvoker for JsInvoker {
        fn invoke(
          &mut self,
          listener_id: events::ListenerId,
          event: &mut events::Event,
        ) -> std::result::Result<(), events::DomError> {
          let callback = self
            .listener_callbacks
            .borrow()
            .get(&listener_id)
            .map(|e| e.callback)
            .ok_or_else(|| events::DomError::new("unknown listener id"))?;

          let current_target = event
            .current_target
            .ok_or_else(|| events::DomError::new("missing currentTarget during dispatch"))?;

          // SAFETY: `rt` is borrowed mutably by the host function while this invoker is alive.
          let rt = unsafe { &mut *self.rt };
          let current_target_wrapper = wrap_event_target(
            rt,
            &self.dom,
            &self.platform_objects,
            &self.node_wrapper_cache,
            self.document_node_id,
            self.window,
            self.document,
            self.prototypes,
            current_target,
          )
          .map_err(|e| events::DomError::new(format!("{e:?}")))?;

          if rt.is_callable(callback) {
            rt
              .call_function(callback, current_target_wrapper, &[self.event_value])
              .map_err(|e| events::DomError::new(format!("{e:?}")))?;
          } else {
            // Support callback objects with a `handleEvent` method.
            let handle_event_key = prop_key_str(rt, "handleEvent")
              .map_err(|e| events::DomError::new(format!("{e:?}")))?;
            let handle_event = rt
              .get(callback, handle_event_key)
              .map_err(|e| events::DomError::new(format!("{e:?}")))?;
            if !rt.is_callable(handle_event) {
              return Err(events::DomError::new(
                "EventTarget listener callback has no callable handleEvent",
              ));
            }
            rt
              .call_function(handle_event, callback, &[self.event_value])
              .map_err(|e| events::DomError::new(format!("{e:?}")))?;
          }
          Ok(())
        }
      }

      // `events::dispatch_event` holds `&mut Event` for the duration of dispatch, which means our
      // JS Event methods cannot borrow it from a shared container. We instead expose it via
      // `active_events` above.
      let mut invoker = JsInvoker {
        rt: rt as *mut VmJsRuntime,
        listener_callbacks: listener_callbacks_for_dispatch.clone(),
        dom: dom_for_dispatch.clone(),
        platform_objects: platform_objects_for_dispatch.clone(),
        node_wrapper_cache: node_wrapper_cache_for_dispatch.clone(),
        document_node_id,
        window,
        document,
        prototypes,
        event_value,
      };

      let result = {
        // Register the Event as active so Event.prototype methods can mutate it during callback
        // invocation without tripping RefCell reentrancy.
        {
          let ptr = NonNull::from(&mut event);
          active_events_for_dispatch.borrow_mut().insert(event_id, ptr);
        }
        let _active_guard = ActiveEventGuard {
          active: active_events_for_dispatch.clone(),
          id: event_id,
        };

        // `events::dispatch_event` holds `&mut Event` for the duration of dispatch, which means our
        // JS Event methods cannot borrow it from a shared container. We instead expose it via
        // `active_events` above.
        let dom_ref = dom_for_dispatch.borrow();
        events::dispatch_event(
          target,
          &mut event,
          &dom_ref,
          &event_listeners_for_dispatch,
          &mut invoker,
        )
      };

      // Persist updated event state after dispatch.
      events_map_for_dispatch.borrow_mut().insert(event_id, event);

      // `events::dispatch_event` may remove listeners during dispatch (e.g. `{ once: true }`).
      //
      // Our callback map roots listener callbacks so they survive GC, but it must not keep callbacks
      // alive after the registry no longer references them. Clean up any callback roots for listener
      // IDs that are now unreferenced.
      //
      // This is intentionally opportunistic: it is only invoked on dispatch (and on explicit
      // `removeEventListener`). It's sufficient for the MVP and keeps long-running realms from
      // leaking callbacks when using `once`.
      {
        let stale_ids: Vec<events::ListenerId> = listener_callbacks_for_dispatch
          .borrow()
          .keys()
          .copied()
          .filter(|id| !event_listeners_for_dispatch.contains_listener_id(*id))
          .collect();
        if !stale_ids.is_empty() {
          let mut callbacks = listener_callbacks_for_dispatch.borrow_mut();
          for id in stale_ids {
            if let Some(entry) = callbacks.remove(&id) {
              rt.heap_mut().remove_root(entry.callback_root);
            }
          }
        }
      }

      match result {
        Ok(not_canceled) => Ok(Value::Bool(not_canceled)),
        Err(err) => Err(rt.throw_type_error(&err.to_string())),
      }
    })?;
    define_method(rt, prototypes.event_target, "dispatchEvent", dispatch)?;
  }

  // Window/document event handler IDL attributes (MVP).
  //
  // Real-world pages frequently use `window.onload = ...` and `document.onreadystatechange = ...`
  // rather than `addEventListener`. Provide minimal accessor properties that are wired into the
  // existing `web::events::EventListenerRegistry` via an internal wrapper listener:
  // - the wrapper is registered as a normal event listener (so it participates in dispatch),
  // - the IDL attribute setter updates the wrapper's "current callback" slot,
  // - the wrapper invokes that callback (if present) when the event fires.
  //
  // This intentionally keeps the IDL handler isolated from `addEventListener` / `removeEventListener`:
  // the wrapper has its own listener identity, so calling `removeEventListener("load", window.onload)`
  // does not remove the IDL handler (matching browser behaviour).
  {
    fn install_event_handler_idl_attribute(
      rt: &mut VmJsRuntime,
      obj: Value,
      platform_objects: Rc<RefCell<FxHashMap<WeakGcObject, PlatformObjectKind>>>,
      event_listeners: Rc<events::EventListenerRegistry>,
      listener_callbacks: Rc<RefCell<FxHashMap<events::ListenerId, ListenerEntry>>>,
      expected_target: events::EventTargetId,
      property_name: &'static str,
      event_type: &'static str,
    ) -> Result<(), VmError> {
      // Current user-provided callback stored in Rust and kept alive via an explicit heap root.
      let handler_slot: Rc<RefCell<Option<ListenerEntry>>> = Rc::new(RefCell::new(None));

      // Wrapper listener registered into the event listener registry. This is the only callback
      // exposed to the event dispatch layer; it forwards to `handler_slot` when invoked.
      let handler_slot_for_wrapper = handler_slot.clone();
      let wrapper = rt.alloc_function_value(move |rt, this, args| {
        let entry = *handler_slot_for_wrapper.borrow();
        let Some(entry) = entry else {
          return Ok(Value::Undefined);
        };

        if rt.is_callable(entry.callback) {
          let _ = rt.call_function(entry.callback, this, args)?;
          return Ok(Value::Undefined);
        }

        // Support callback objects with a `handleEvent` method (consistent with `addEventListener`).
        let handle_event_key = prop_key_str(rt, "handleEvent")?;
        let handle_event = rt.get(entry.callback, handle_event_key)?;
        if rt.is_callable(handle_event) {
          let _ = rt.call_function(handle_event, entry.callback, args)?;
        }
        Ok(Value::Undefined)
      })?;

      // Keep the wrapper function alive even before it is registered in the listener tables.
      let wrapper_root = rt.heap_mut().add_root(wrapper)?;
      let wrapper_listener_id = listener_id_from_callback(rt, wrapper)?
        .ok_or_else(|| rt.throw_type_error("internal event handler wrapper must be an object"))?;

      // on* getter.
      let handler_slot_for_get = handler_slot.clone();
      let platform_objects_for_get = platform_objects.clone();
      let get = rt.alloc_function_value(move |rt, this, _args| {
        let got = extract_event_target_id(rt, &platform_objects_for_get, this)?;
        if got != expected_target {
          return Err(rt.throw_type_error("Illegal invocation"));
        }
        let entry = *handler_slot_for_get.borrow();
        Ok(entry.map(|entry| entry.callback).unwrap_or(Value::Null))
      })?;

      // on* setter.
      let handler_slot_for_set = handler_slot.clone();
      let platform_objects_for_set = platform_objects.clone();
      let event_listeners_for_set = event_listeners.clone();
      let listener_callbacks_for_set = listener_callbacks.clone();
      let set = rt.alloc_function_value(move |rt, this, args| {
        let got = extract_event_target_id(rt, &platform_objects_for_set, this)?;
        if got != expected_target {
          return Err(rt.throw_type_error("Illegal invocation"));
        }

        let new_value = args.get(0).copied().unwrap_or(Value::Undefined);
        let new_callback = match new_value {
          Value::Undefined | Value::Null => None,
          other => {
            validate_event_listener_callback(rt, other)?;
            Some(other)
          }
        };

        // Ensure the wrapper listener is wired into dispatch once a non-null handler is set.
        if new_callback.is_some() {
          {
            let mut callbacks = listener_callbacks_for_set.borrow_mut();
            if !callbacks.contains_key(&wrapper_listener_id) {
              callbacks.insert(
                wrapper_listener_id,
                ListenerEntry {
                  callback: wrapper,
                  callback_root: wrapper_root,
                },
              );
            }
          }

          let _inserted = event_listeners_for_set.add_event_listener(
            expected_target,
            event_type,
            wrapper_listener_id,
            events::AddEventListenerOptions::default(),
          );
        }

        // Update the stored handler callback + roots.
        let mut slot = handler_slot_for_set.borrow_mut();
        if slot
          .as_ref()
          .is_some_and(|existing| Some(existing.callback) == new_callback)
        {
          return Ok(Value::Undefined);
        }

        if let Some(existing) = slot.take() {
          rt.heap_mut().remove_root(existing.callback_root);
        }

        if let Some(callback) = new_callback {
          let callback_root = rt.heap_mut().add_root(callback)?;
          *slot = Some(ListenerEntry {
            callback,
            callback_root,
          });
        }

        Ok(Value::Undefined)
      })?;

      define_accessor(rt, obj, property_name, get, set)?;
      Ok(())
    }

    // window.onload
    install_event_handler_idl_attribute(
      rt,
      window,
      platform_objects.clone(),
      event_listeners.clone(),
      listener_callbacks.clone(),
      events::EventTargetId::Window,
      "onload",
      "load",
    )?;

    // document.onreadystatechange
    install_event_handler_idl_attribute(
      rt,
      document,
      platform_objects.clone(),
      event_listeners.clone(),
      listener_callbacks.clone(),
      events::EventTargetId::Document,
      "onreadystatechange",
      "readystatechange",
    )?;
  }

  // Node.prototype
  {
    let dom_exception_proto = prototypes.dom_exception;

    // appendChild
    let dom_for_append = dom.clone();
    let platform_objects_for_append = platform_objects.clone();
    let node_wrapper_cache_for_append = node_wrapper_cache.clone();
    let append_child = rt.alloc_function_value(move |rt, this, args| {
      let parent_id = extract_node_id(rt, &platform_objects_for_append, this)?;
      let child = args
        .get(0)
        .copied()
        .ok_or_else(|| rt.throw_type_error("appendChild: missing child"))?;
      let child_id = extract_node_id(rt, &platform_objects_for_append, child)?;
      let (old_parent, is_fragment) = {
        let dom_ref = dom_for_append.borrow();
        let is_fragment = dom_ref
          .nodes()
          .get(child_id.index())
          .is_some_and(|node| matches!(node.kind, NodeKind::DocumentFragment));
        (dom_ref.parent_node(child_id), is_fragment)
      };
      dom_for_append
        .borrow_mut()
        .append_child(parent_id, child_id)
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))?;

      maybe_refresh_cached_child_nodes(
        rt,
        &dom_for_append,
        &platform_objects_for_append,
        &node_wrapper_cache_for_append,
        document_node_id,
        document,
        prototypes,
        parent_id,
      )?;
      if let Some(old_parent) = old_parent {
        if old_parent != parent_id {
          maybe_refresh_cached_child_nodes(
            rt,
            &dom_for_append,
            &platform_objects_for_append,
            &node_wrapper_cache_for_append,
            document_node_id,
            document,
            prototypes,
            old_parent,
          )?;
        }
      }
      if is_fragment {
        maybe_refresh_cached_child_nodes(
          rt,
          &dom_for_append,
          &platform_objects_for_append,
          &node_wrapper_cache_for_append,
          document_node_id,
          document,
          prototypes,
          child_id,
        )?;
      }
      Ok(child)
    })?;
    define_method(rt, prototypes.node, "appendChild", append_child)?;

    // insertBefore
    let dom_for_insert = dom.clone();
    let platform_objects_for_insert = platform_objects.clone();
    let node_wrapper_cache_for_insert = node_wrapper_cache.clone();
    let insert_before = rt.alloc_function_value(move |rt, this, args| {
      let parent_id = extract_node_id(rt, &platform_objects_for_insert, this)?;
      let child = args
        .get(0)
        .copied()
        .ok_or_else(|| rt.throw_type_error("insertBefore: missing newChild"))?;
      let child_id = extract_node_id(rt, &platform_objects_for_insert, child)?;
      let reference = args.get(1).copied().unwrap_or(Value::Null);
      let reference_id = match reference {
        Value::Undefined | Value::Null => None,
        other => Some(extract_node_id(rt, &platform_objects_for_insert, other)?),
      };
      let (old_parent, is_fragment) = {
        let dom_ref = dom_for_insert.borrow();
        let is_fragment = dom_ref
          .nodes()
          .get(child_id.index())
          .is_some_and(|node| matches!(node.kind, NodeKind::DocumentFragment));
        (dom_ref.parent_node(child_id), is_fragment)
      };
      dom_for_insert
        .borrow_mut()
        .insert_before(parent_id, child_id, reference_id)
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))?;
      maybe_refresh_cached_child_nodes(
        rt,
        &dom_for_insert,
        &platform_objects_for_insert,
        &node_wrapper_cache_for_insert,
        document_node_id,
        document,
        prototypes,
        parent_id,
      )?;
      if let Some(old_parent) = old_parent {
        if old_parent != parent_id {
          maybe_refresh_cached_child_nodes(
            rt,
            &dom_for_insert,
            &platform_objects_for_insert,
            &node_wrapper_cache_for_insert,
            document_node_id,
            document,
            prototypes,
            old_parent,
          )?;
        }
      }
      if is_fragment {
        maybe_refresh_cached_child_nodes(
          rt,
          &dom_for_insert,
          &platform_objects_for_insert,
          &node_wrapper_cache_for_insert,
          document_node_id,
          document,
          prototypes,
          child_id,
        )?;
      }
      Ok(child)
    })?;
    define_method(rt, prototypes.node, "insertBefore", insert_before)?;

    // removeChild
    let dom_for_remove = dom.clone();
    let platform_objects_for_remove = platform_objects.clone();
    let node_wrapper_cache_for_remove = node_wrapper_cache.clone();
    let remove_child = rt.alloc_function_value(move |rt, this, args| {
      let parent_id = extract_node_id(rt, &platform_objects_for_remove, this)?;
      let child = args
        .get(0)
        .copied()
        .ok_or_else(|| rt.throw_type_error("removeChild: missing child"))?;
      let child_id = extract_node_id(rt, &platform_objects_for_remove, child)?;
      dom_for_remove
        .borrow_mut()
        .remove_child(parent_id, child_id)
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))?;

      maybe_refresh_cached_child_nodes(
        rt,
        &dom_for_remove,
        &platform_objects_for_remove,
        &node_wrapper_cache_for_remove,
        document_node_id,
        document,
        prototypes,
        parent_id,
      )?;
      Ok(child)
    })?;
    define_method(rt, prototypes.node, "removeChild", remove_child)?;

    // remove
    let dom_for_remove_self = dom.clone();
    let platform_objects_for_remove_self = platform_objects.clone();
    let node_wrapper_cache_for_remove_self = node_wrapper_cache.clone();
    let remove = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_remove_self, this)?;
      let Some(parent_id) = dom_for_remove_self.borrow().parent_node(node_id) else {
        return Ok(Value::Undefined);
      };
      dom_for_remove_self
        .borrow_mut()
        .remove_child(parent_id, node_id)
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))?;

      maybe_refresh_cached_child_nodes(
        rt,
        &dom_for_remove_self,
        &platform_objects_for_remove_self,
        &node_wrapper_cache_for_remove_self,
        document_node_id,
        document,
        prototypes,
        parent_id,
      )?;
      Ok(Value::Undefined)
    })?;
    define_method(rt, prototypes.node, "remove", remove)?;

    // replaceChild
    let dom_for_replace = dom.clone();
    let platform_objects_for_replace = platform_objects.clone();
    let node_wrapper_cache_for_replace = node_wrapper_cache.clone();
    let replace_child = rt.alloc_function_value(move |rt, this, args| {
      let parent_id = extract_node_id(rt, &platform_objects_for_replace, this)?;
      let new_child = args
        .get(0)
        .copied()
        .ok_or_else(|| rt.throw_type_error("replaceChild: missing newChild"))?;
      let old_child = args
        .get(1)
        .copied()
        .ok_or_else(|| rt.throw_type_error("replaceChild: missing oldChild"))?;
      let new_child_id = extract_node_id(rt, &platform_objects_for_replace, new_child)?;
      let old_child_id = extract_node_id(rt, &platform_objects_for_replace, old_child)?;
      let (old_parent, is_fragment) = {
        let dom_ref = dom_for_replace.borrow();
        let is_fragment = dom_ref
          .nodes()
          .get(new_child_id.index())
          .is_some_and(|node| matches!(node.kind, NodeKind::DocumentFragment));
        (dom_ref.parent_node(new_child_id), is_fragment)
      };
      dom_for_replace
        .borrow_mut()
        .replace_child(parent_id, new_child_id, old_child_id)
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))?;
      maybe_refresh_cached_child_nodes(
        rt,
        &dom_for_replace,
        &platform_objects_for_replace,
        &node_wrapper_cache_for_replace,
        document_node_id,
        document,
        prototypes,
        parent_id,
      )?;
      if let Some(old_parent) = old_parent {
        if old_parent != parent_id {
          maybe_refresh_cached_child_nodes(
            rt,
            &dom_for_replace,
            &platform_objects_for_replace,
            &node_wrapper_cache_for_replace,
            document_node_id,
            document,
            prototypes,
            old_parent,
          )?;
        }
      }
      if is_fragment {
        maybe_refresh_cached_child_nodes(
          rt,
          &dom_for_replace,
          &platform_objects_for_replace,
          &node_wrapper_cache_for_replace,
          document_node_id,
          document,
          prototypes,
          new_child_id,
        )?;
      }
      Ok(old_child)
    })?;
    define_method(rt, prototypes.node, "replaceChild", replace_child)?;

    // parentNode
    let dom_for_parent = dom.clone();
    let platform_objects_for_parent = platform_objects.clone();
    let node_wrapper_cache_for_parent = node_wrapper_cache.clone();
    let parent_node_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_parent, this)?;
      let parent = dom_for_parent.borrow().dom_parent_for_event_path(node_id);
      match parent {
        Some(id) => wrap_node(
          rt,
          &dom_for_parent,
          &platform_objects_for_parent,
          &node_wrapper_cache_for_parent,
          document_node_id,
          document,
          prototypes,
          id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(rt, prototypes.node, "parentNode", parent_node_get, Value::Undefined)?;

    // parentElement
    let dom_for_parent_element = dom.clone();
    let platform_objects_for_parent_element = platform_objects.clone();
    let node_wrapper_cache_for_parent_element = node_wrapper_cache.clone();
    let parent_element_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_parent_element, this)?;
      let dom_ref = dom_for_parent_element.borrow();
      let parent = dom_ref.dom_parent_for_event_path(node_id).filter(|&id| {
        matches!(
          dom_ref.node(id).kind,
          NodeKind::Element { .. } | NodeKind::Slot { .. }
        )
      });
      match parent {
        Some(id) => wrap_node(
          rt,
          &dom_for_parent_element,
          &platform_objects_for_parent_element,
          &node_wrapper_cache_for_parent_element,
          document_node_id,
          document,
          prototypes,
          id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(
      rt,
      prototypes.node,
      "parentElement",
      parent_element_get,
      Value::Undefined,
    )?;

    // childNodes
    let dom_for_child_nodes = dom.clone();
    let platform_objects_for_child_nodes = platform_objects.clone();
    let node_wrapper_cache_for_child_nodes = node_wrapper_cache.clone();
    let child_nodes_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_child_nodes, this)?;
      let Value::Object(_) = this else {
        return Err(rt.throw_type_error("Illegal invocation"));
      };

      let cache_key = prop_key_str(rt, CHILD_NODES_CACHE_PROP)?;
      let cached = rt.get(this, cache_key)?;
      if matches!(cached, Value::Object(_)) {
        return Ok(cached);
      }

      let arr = rt.alloc_array()?;
      rt.define_data_property(this, cache_key, arr, /* enumerable */ false)?;

      // Populate and return the cached array.
      maybe_refresh_cached_child_nodes(
        rt,
        &dom_for_child_nodes,
        &platform_objects_for_child_nodes,
        &node_wrapper_cache_for_child_nodes,
        document_node_id,
        document,
        prototypes,
        node_id,
      )?;
      Ok(arr)
    })?;
    define_accessor(rt, prototypes.node, "childNodes", child_nodes_get, Value::Undefined)?;

    // hasChildNodes
    let dom_for_has_child_nodes = dom.clone();
    let platform_objects_for_has_child_nodes = platform_objects.clone();
    let has_child_nodes = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_has_child_nodes, this)?;
      let dom_ref = dom_for_has_child_nodes.borrow();
      let child_ids = direct_child_nodes(&dom_ref, node_id)
        .map_err(|e| rt.throw_type_error(&format!("hasChildNodes: {e}")))?;
      Ok(Value::Bool(!child_ids.is_empty()))
    })?;
    define_method(rt, prototypes.node, "hasChildNodes", has_child_nodes)?;

    // cloneNode
    let dom_for_clone = dom.clone();
    let platform_objects_for_clone = platform_objects.clone();
    let node_wrapper_cache_for_clone = node_wrapper_cache.clone();
    let clone_node = rt.alloc_function_value(move |rt, this, args| {
      let node_id = extract_node_id(rt, &platform_objects_for_clone, this)?;
      let deep_val = args.get(0).copied().unwrap_or(Value::Undefined);
      let deep = rt.to_boolean(deep_val)?;
      let cloned_id = dom_for_clone
        .borrow_mut()
        .clone_node(node_id, deep)
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))?;
      wrap_node(
        rt,
        &dom_for_clone,
        &platform_objects_for_clone,
        &node_wrapper_cache_for_clone,
        document_node_id,
        document,
        prototypes,
        cloned_id,
      )
    })?;
    define_method(rt, prototypes.node, "cloneNode", clone_node)?;

    // contains
    let dom_for_contains = dom.clone();
    let platform_objects_for_contains = platform_objects.clone();
    let contains = rt.alloc_function_value(move |rt, this, args| {
      let node_id = extract_node_id(rt, &platform_objects_for_contains, this)?;
      let other = args.get(0).copied().unwrap_or(Value::Undefined);

      let other_id = match other {
        Value::Undefined | Value::Null => return Ok(Value::Bool(false)),
        Value::Object(obj) => match platform_objects_for_contains
          .borrow()
          .get(&WeakGcObject::from(obj))
        {
          Some(PlatformObjectKind::Document { node_id }) => *node_id,
          Some(PlatformObjectKind::Node { node_id }) => *node_id,
          _ => {
            return Err(rt.throw_type_error(
              "contains: argument must be a Node (or null/undefined)",
            ))
          }
        },
        _ => {
          return Err(rt.throw_type_error(
            "contains: argument must be a Node (or null/undefined)",
          ))
        }
      };

      let dom_ref = dom_for_contains.borrow();
      let mut cur = Some(other_id);
      while let Some(id) = cur {
        if id == node_id {
          return Ok(Value::Bool(true));
        }
        cur = dom_ref.dom_parent_for_event_path(id);
      }
      Ok(Value::Bool(false))
    })?;
    define_method(rt, prototypes.node, "contains", contains)?;

    // children (element-only)
    let dom_for_children = dom.clone();
    let platform_objects_for_children = platform_objects.clone();
    let node_wrapper_cache_for_children = node_wrapper_cache.clone();
    let children_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_children, this)?;
      let child_ids = {
        let dom_ref = dom_for_children.borrow();
        direct_element_children(&dom_ref, node_id)
          .map_err(|e| rt.throw_type_error(&format!("children: {e}")))?
      };
      let mut wrappers: Vec<Value> = Vec::new();
      for child_id in child_ids {
        wrappers.push(wrap_node(
          rt,
          &dom_for_children,
          &platform_objects_for_children,
          &node_wrapper_cache_for_children,
          document_node_id,
          document,
          prototypes,
          child_id,
        )?);
      }

      let arr = rt.alloc_array()?;
      let Value::Object(arr_obj) = arr else {
        return Err(rt.throw_type_error("alloc_array must return an object"));
      };
      let mut scope = rt.heap_mut().scope();
      for (idx, value) in wrappers.into_iter().enumerate() {
        let key = PropertyKey::String(scope.alloc_string(&idx.to_string())?);
        scope.create_data_property_or_throw(arr_obj, key, value)?;
      }
      Ok(arr)
    })?;
    define_accessor(rt, prototypes.node, "children", children_get, Value::Undefined)?;

    // childElementCount
    let dom_for_child_element_count = dom.clone();
    let platform_objects_for_child_element_count = platform_objects.clone();
    let child_element_count_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_child_element_count, this)?;
      let dom_ref = dom_for_child_element_count.borrow();
      let count = direct_element_children(&dom_ref, node_id)
        .map_err(|e| rt.throw_type_error(&format!("childElementCount: {e}")))?
        .len();
      Ok(Value::Number(count as f64))
    })?;
    define_accessor(
      rt,
      prototypes.node,
      "childElementCount",
      child_element_count_get,
      Value::Undefined,
    )?;

    // firstElementChild / lastElementChild
    let dom_for_first_el = dom.clone();
    let platform_objects_for_first_el = platform_objects.clone();
    let node_wrapper_cache_for_first_el = node_wrapper_cache.clone();
    let first_element_child_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_first_el, this)?;
      let dom_ref = dom_for_first_el.borrow();
      let first = direct_element_children(&dom_ref, node_id)
        .map_err(|e| rt.throw_type_error(&format!("firstElementChild: {e}")))?
        .into_iter()
        .next();
      match first {
        Some(id) => wrap_node(
          rt,
          &dom_for_first_el,
          &platform_objects_for_first_el,
          &node_wrapper_cache_for_first_el,
          document_node_id,
          document,
          prototypes,
          id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(
      rt,
      prototypes.node,
      "firstElementChild",
      first_element_child_get,
      Value::Undefined,
    )?;

    let dom_for_last_el = dom.clone();
    let platform_objects_for_last_el = platform_objects.clone();
    let node_wrapper_cache_for_last_el = node_wrapper_cache.clone();
    let last_element_child_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_last_el, this)?;
      let dom_ref = dom_for_last_el.borrow();
      let last = direct_element_children(&dom_ref, node_id)
        .map_err(|e| rt.throw_type_error(&format!("lastElementChild: {e}")))?
        .into_iter()
        .last();
      match last {
        Some(id) => wrap_node(
          rt,
          &dom_for_last_el,
          &platform_objects_for_last_el,
          &node_wrapper_cache_for_last_el,
          document_node_id,
          document,
          prototypes,
          id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(
      rt,
      prototypes.node,
      "lastElementChild",
      last_element_child_get,
      Value::Undefined,
    )?;

    // previousElementSibling / nextElementSibling
    let dom_for_prev_el_sib = dom.clone();
    let platform_objects_for_prev_el_sib = platform_objects.clone();
    let node_wrapper_cache_for_prev_el_sib = node_wrapper_cache.clone();
    let previous_element_sibling_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_prev_el_sib, this)?;
      let dom_ref = dom_for_prev_el_sib.borrow();
      let Some(parent) = dom_ref.dom_parent_for_event_path(node_id) else {
        return Ok(Value::Null);
      };
      let siblings = direct_child_nodes(&dom_ref, parent)
        .map_err(|e| rt.throw_type_error(&format!("previousElementSibling: {e}")))?;
      let Some(pos) = siblings.iter().position(|&id| id == node_id) else {
        return Ok(Value::Null);
      };
      let prev = siblings
        .into_iter()
        .take(pos)
        .rev()
        .find(|&id| {
          matches!(
            dom_ref.node(id).kind,
            NodeKind::Element { .. } | NodeKind::Slot { .. }
          )
        });
      match prev {
        Some(id) => wrap_node(
          rt,
          &dom_for_prev_el_sib,
          &platform_objects_for_prev_el_sib,
          &node_wrapper_cache_for_prev_el_sib,
          document_node_id,
          document,
          prototypes,
          id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(
      rt,
      prototypes.node,
      "previousElementSibling",
      previous_element_sibling_get,
      Value::Undefined,
    )?;

    let dom_for_next_el_sib = dom.clone();
    let platform_objects_for_next_el_sib = platform_objects.clone();
    let node_wrapper_cache_for_next_el_sib = node_wrapper_cache.clone();
    let next_element_sibling_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_next_el_sib, this)?;
      let dom_ref = dom_for_next_el_sib.borrow();
      let Some(parent) = dom_ref.dom_parent_for_event_path(node_id) else {
        return Ok(Value::Null);
      };
      let siblings = direct_child_nodes(&dom_ref, parent)
        .map_err(|e| rt.throw_type_error(&format!("nextElementSibling: {e}")))?;
      let Some(pos) = siblings.iter().position(|&id| id == node_id) else {
        return Ok(Value::Null);
      };
      let next = siblings.into_iter().skip(pos + 1).find(|&id| {
        matches!(
          dom_ref.node(id).kind,
          NodeKind::Element { .. } | NodeKind::Slot { .. }
        )
      });
      match next {
        Some(id) => wrap_node(
          rt,
          &dom_for_next_el_sib,
          &platform_objects_for_next_el_sib,
          &node_wrapper_cache_for_next_el_sib,
          document_node_id,
          document,
          prototypes,
          id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(
      rt,
      prototypes.node,
      "nextElementSibling",
      next_element_sibling_get,
      Value::Undefined,
    )?;

    // firstChild
    let dom_for_first = dom.clone();
    let platform_objects_for_first = platform_objects.clone();
    let node_wrapper_cache_for_first = node_wrapper_cache.clone();
    let first_child_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_first, this)?;
      let dom_ref = dom_for_first.borrow();
      let first = direct_child_nodes(&dom_ref, node_id)
        .map_err(|e| rt.throw_type_error(&format!("firstChild: {e}")))?
        .into_iter()
        .next();
      match first {
        Some(id) => wrap_node(
          rt,
          &dom_for_first,
          &platform_objects_for_first,
          &node_wrapper_cache_for_first,
          document_node_id,
          document,
          prototypes,
          id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(rt, prototypes.node, "firstChild", first_child_get, Value::Undefined)?;

    // lastChild
    let dom_for_last = dom.clone();
    let platform_objects_for_last = platform_objects.clone();
    let node_wrapper_cache_for_last = node_wrapper_cache.clone();
    let last_child_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_last, this)?;
      let dom_ref = dom_for_last.borrow();
      let last = direct_child_nodes(&dom_ref, node_id)
        .map_err(|e| rt.throw_type_error(&format!("lastChild: {e}")))?
        .into_iter()
        .last();
      match last {
        Some(id) => wrap_node(
          rt,
          &dom_for_last,
          &platform_objects_for_last,
          &node_wrapper_cache_for_last,
          document_node_id,
          document,
          prototypes,
          id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(rt, prototypes.node, "lastChild", last_child_get, Value::Undefined)?;

    // previousSibling
    let dom_for_prev = dom.clone();
    let platform_objects_for_prev = platform_objects.clone();
    let node_wrapper_cache_for_prev = node_wrapper_cache.clone();
    let prev_sibling_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_prev, this)?;
      let dom_ref = dom_for_prev.borrow();
      let Some(parent) = dom_ref.dom_parent_for_event_path(node_id) else {
        return Ok(Value::Null);
      };
      let siblings = direct_child_nodes(&dom_ref, parent)
        .map_err(|e| rt.throw_type_error(&format!("previousSibling: {e}")))?;
      let Some(pos) = siblings.iter().position(|&id| id == node_id) else {
        return Ok(Value::Null);
      };
      let prev = pos.checked_sub(1).and_then(|idx| siblings.get(idx)).copied();
      match prev {
        Some(id) => wrap_node(
          rt,
          &dom_for_prev,
          &platform_objects_for_prev,
          &node_wrapper_cache_for_prev,
          document_node_id,
          document,
          prototypes,
          id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(rt, prototypes.node, "previousSibling", prev_sibling_get, Value::Undefined)?;

    // nextSibling
    let dom_for_next = dom.clone();
    let platform_objects_for_next = platform_objects.clone();
    let node_wrapper_cache_for_next = node_wrapper_cache.clone();
    let next_sibling_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_next, this)?;
      let dom_ref = dom_for_next.borrow();
      let Some(parent) = dom_ref.dom_parent_for_event_path(node_id) else {
        return Ok(Value::Null);
      };
      let siblings = direct_child_nodes(&dom_ref, parent)
        .map_err(|e| rt.throw_type_error(&format!("nextSibling: {e}")))?;
      let Some(pos) = siblings.iter().position(|&id| id == node_id) else {
        return Ok(Value::Null);
      };
      let next = siblings.get(pos + 1).copied();
      match next {
        Some(id) => wrap_node(
          rt,
          &dom_for_next,
          &platform_objects_for_next,
          &node_wrapper_cache_for_next,
          document_node_id,
          document,
          prototypes,
          id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(rt, prototypes.node, "nextSibling", next_sibling_get, Value::Undefined)?;

    // nodeType
    let dom_for_node_type = dom.clone();
    let platform_objects_for_node_type = platform_objects.clone();
    let node_type_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_node_type, this)?;
      let dom_ref = dom_for_node_type.borrow();
      let node_type = match &dom_ref.node(node_id).kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => 1,
        NodeKind::Text { .. } => 3,
        NodeKind::ProcessingInstruction { .. } => 7,
        NodeKind::Comment { .. } => 8,
        NodeKind::Document { .. } => 9,
        NodeKind::Doctype { .. } => 10,
        NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => 11,
      };
      Ok(Value::Number(node_type as f64))
    })?;
    define_accessor(rt, prototypes.node, "nodeType", node_type_get, Value::Undefined)?;

    // nodeName
    let dom_for_node_name = dom.clone();
    let platform_objects_for_node_name = platform_objects.clone();
    let node_name_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_node_name, this)?;
      let dom_ref = dom_for_node_name.borrow();
      let name = match &dom_ref.node(node_id).kind {
        NodeKind::Document { .. } => "#document".to_string(),
        NodeKind::Doctype { name, .. } => name.clone(),
        NodeKind::Element { tag_name, namespace, .. } => {
          if is_html_namespace(namespace) {
            tag_name.to_ascii_uppercase()
          } else {
            tag_name.clone()
          }
        }
        NodeKind::Slot { namespace, .. } => {
          if is_html_namespace(namespace) {
            "SLOT".to_string()
          } else {
            "slot".to_string()
          }
        }
        NodeKind::Text { .. } => "#text".to_string(),
        NodeKind::Comment { .. } => "#comment".to_string(),
        NodeKind::ProcessingInstruction { target, .. } => target.clone(),
        NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => "#document-fragment".to_string(),
      };
      rt.alloc_string_value(&name)
    })?;
    define_accessor(rt, prototypes.node, "nodeName", node_name_get, Value::Undefined)?;

    // nodeValue
    let dom_for_node_value_get = dom.clone();
    let platform_objects_for_node_value_get = platform_objects.clone();
    let node_value_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_node_value_get, this)?;
      let dom_ref = dom_for_node_value_get.borrow();
      match &dom_ref.node(node_id).kind {
        NodeKind::Text { content } => rt.alloc_string_value(content),
        NodeKind::Comment { content } => rt.alloc_string_value(content),
        NodeKind::ProcessingInstruction { data, .. } => rt.alloc_string_value(data),
        _ => Ok(Value::Null),
      }
    })?;

    let dom_for_node_value_set = dom.clone();
    let platform_objects_for_node_value_set = platform_objects.clone();
    let node_value_set = rt.alloc_function_value(move |rt, this, args| {
      let node_id = extract_node_id(rt, &platform_objects_for_node_value_set, this)?;
      let v = args.get(0).copied().unwrap_or(Value::Undefined);
      let text = match v {
        Value::Undefined | Value::Null => String::new(),
        other => to_rust_string(rt, other)?,
      };

      let mut dom_mut = dom_for_node_value_set.borrow_mut();
      match &mut dom_mut.node_mut(node_id).kind {
        NodeKind::Text { content } | NodeKind::Comment { content } => {
          content.clear();
          content.push_str(&text);
        }
        NodeKind::ProcessingInstruction { data, .. } => {
          data.clear();
          data.push_str(&text);
        }
        _ => {
          // Per DOM, setting nodeValue on non-character-data nodes is a no-op.
        }
      }
      Ok(Value::Undefined)
    })?;
    define_accessor(rt, prototypes.node, "nodeValue", node_value_get, node_value_set)?;

    // textContent
    let dom_for_text_content_get = dom.clone();
    let platform_objects_for_text_content_get = platform_objects.clone();
    let text_content_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_text_content_get, this)?;
      let dom_ref = dom_for_text_content_get.borrow();
      if matches!(&dom_ref.node(node_id).kind, NodeKind::Doctype { .. }) {
        return Ok(Value::Null);
      }
      let text = get_text_content(&dom_ref, node_id);
      rt.alloc_string_value(&text)
    })?;

    let dom_for_text_content_set = dom.clone();
    let platform_objects_for_text_content_set = platform_objects.clone();
    let node_wrapper_cache_for_text_content_set = node_wrapper_cache.clone();
    let text_content_set = rt.alloc_function_value(move |rt, this, args| {
      let node_id = extract_node_id(rt, &platform_objects_for_text_content_set, this)?;
      let v = args.get(0).copied().unwrap_or(Value::Undefined);
      let text = if matches!(v, Value::Null) {
        String::new()
      } else {
        to_rust_string(rt, v)?
      };
      set_text_content(&mut dom_for_text_content_set.borrow_mut(), node_id, &text)
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))?;

      maybe_refresh_cached_child_nodes(
        rt,
        &dom_for_text_content_set,
        &platform_objects_for_text_content_set,
        &node_wrapper_cache_for_text_content_set,
        document_node_id,
        document,
        prototypes,
        node_id,
      )?;
      Ok(Value::Undefined)
    })?;

    define_accessor(rt, prototypes.node, "textContent", text_content_get, text_content_set)?;

    // ownerDocument
    let platform_objects_for_owner = platform_objects.clone();
    let owner_document_get = rt.alloc_function_value(move |rt, this, _args| {
      let Value::Object(obj) = this else {
        return Err(rt.throw_type_error("Illegal invocation"));
      };
      let key = WeakGcObject::from(obj);
      match platform_objects_for_owner.borrow().get(&key) {
        Some(PlatformObjectKind::Document { .. }) => Ok(Value::Null),
        Some(PlatformObjectKind::Node { .. }) => Ok(document),
        _ => Err(rt.throw_type_error("Illegal invocation")),
      }
    })?;
    define_accessor(
      rt,
      prototypes.node,
      "ownerDocument",
      owner_document_get,
      Value::Undefined,
    )?;

    // isConnected
    let dom_for_is_connected = dom.clone();
    let platform_objects_for_is_connected = platform_objects.clone();
    let is_connected_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_is_connected, this)?;
      Ok(Value::Bool(dom_for_is_connected.borrow().is_connected_for_scripting(node_id)))
    })?;
    define_accessor(
      rt,
      prototypes.node,
      "isConnected",
      is_connected_get,
      Value::Undefined,
    )?;
  }

  // Element.prototype
  {
    let dom_exception_proto = prototypes.dom_exception;

    // tagName
    let dom_for_tag_name = dom.clone();
    let platform_objects_for_tag_name = platform_objects.clone();
    let tag_name_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_tag_name, this)?;
      let dom_ref = dom_for_tag_name.borrow();
      let name = match &dom_ref.node(node_id).kind {
        NodeKind::Element { tag_name, namespace, .. } => {
          if is_html_namespace(namespace) {
            tag_name.to_ascii_uppercase()
          } else {
            tag_name.clone()
          }
        }
        NodeKind::Slot { namespace, .. } => {
          if is_html_namespace(namespace) {
            "SLOT".to_string()
          } else {
            "slot".to_string()
          }
        }
        _ => return Err(rt.throw_type_error("tagName: receiver is not an Element")),
      };
      rt.alloc_string_value(&name)
    })?;
    define_accessor(rt, prototypes.element, "tagName", tag_name_get, Value::Undefined)?;

    // innerText (MVP: textContent-like semantics)
    let dom_for_inner_text_get = dom.clone();
    let platform_objects_for_inner_text_get = platform_objects.clone();
    let inner_text_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_inner_text_get, this)?;
      let dom_ref = dom_for_inner_text_get.borrow();
      match &dom_ref.node(node_id).kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
        _ => return Err(rt.throw_type_error("innerText: receiver is not an Element")),
      }
      let text = get_text_content(&dom_ref, node_id);
      rt.alloc_string_value(&text)
    })?;

    let dom_for_inner_text_set = dom.clone();
    let platform_objects_for_inner_text_set = platform_objects.clone();
    let node_wrapper_cache_for_inner_text_set = node_wrapper_cache.clone();
    let inner_text_set = rt.alloc_function_value(move |rt, this, args| {
      let node_id = extract_node_id(rt, &platform_objects_for_inner_text_set, this)?;
      {
        let dom_ref = dom_for_inner_text_set.borrow();
        match &dom_ref.node(node_id).kind {
          NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
          _ => return Err(rt.throw_type_error("innerText: receiver is not an Element")),
        }
      }

      let v = args.get(0).copied().unwrap_or(Value::Undefined);
      let text = if matches!(v, Value::Null) {
        String::new()
      } else {
        to_rust_string(rt, v)?
      };
      set_text_content(&mut dom_for_inner_text_set.borrow_mut(), node_id, &text)
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))?;

      maybe_refresh_cached_child_nodes(
        rt,
        &dom_for_inner_text_set,
        &platform_objects_for_inner_text_set,
        &node_wrapper_cache_for_inner_text_set,
        document_node_id,
        document,
        prototypes,
        node_id,
      )?;
      Ok(Value::Undefined)
    })?;
    define_accessor(rt, prototypes.element, "innerText", inner_text_get, inner_text_set)?;

    let dom_for_get_attribute = dom.clone();
    let platform_objects_for_get_attribute = platform_objects.clone();
    let get_attribute = rt.alloc_function_value(move |rt, this, args| {
      let node_id = extract_node_id(rt, &platform_objects_for_get_attribute, this)?;
      let name = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      let dom_ref = dom_for_get_attribute.borrow();
      match &dom_ref.node(node_id).kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
        _ => return Err(rt.throw_type_error("getAttribute: receiver is not an Element")),
      }
      match dom_ref
        .get_attribute(node_id, &name)
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))? {
        Some(v) => rt.alloc_string_value(v),
        None => Ok(Value::Null),
      }
    })?;
    define_method(rt, prototypes.element, "getAttribute", get_attribute)?;

    let dom_for_set = dom.clone();
    let platform_objects_for_set = platform_objects.clone();
    let set_attribute = rt.alloc_function_value(move |rt, this, args| {
      let node_id = extract_node_id(rt, &platform_objects_for_set, this)?;
      let name = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      let value = to_rust_string(rt, args.get(1).copied().unwrap_or(Value::Undefined))?;
      match &dom_for_set.borrow().node(node_id).kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
        _ => return Err(rt.throw_type_error("setAttribute: receiver is not an Element")),
      }
      dom_for_set
        .borrow_mut()
        .set_attribute(node_id, &name, &value)
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))?;
      Ok(Value::Undefined)
    })?;
    define_method(rt, prototypes.element, "setAttribute", set_attribute)?;

    let dom_for_remove = dom.clone();
    let platform_objects_for_remove = platform_objects.clone();
    let remove_attribute = rt.alloc_function_value(move |rt, this, args| {
      let node_id = extract_node_id(rt, &platform_objects_for_remove, this)?;
      let name = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      match &dom_for_remove.borrow().node(node_id).kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
        _ => return Err(rt.throw_type_error("removeAttribute: receiver is not an Element")),
      }
      dom_for_remove
        .borrow_mut()
        .remove_attribute(node_id, &name)
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))?;
      Ok(Value::Undefined)
    })?;
    define_method(rt, prototypes.element, "removeAttribute", remove_attribute)?;

    let dom_for_id_get = dom.clone();
    let platform_objects_for_id_get = platform_objects.clone();
    let id_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_id_get, this)?;
      let dom_ref = dom_for_id_get.borrow();
      match &dom_ref.node(node_id).kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
        _ => return Err(rt.throw_type_error("id: receiver is not an Element")),
      }
      let v = dom_ref
        .get_attribute(node_id, "id")
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))?
        .unwrap_or("");
      rt.alloc_string_value(v)
    })?;
    let dom_for_id_set = dom.clone();
    let platform_objects_for_id_set = platform_objects.clone();
    let id_set = rt.alloc_function_value(move |rt, this, args| {
      let node_id = extract_node_id(rt, &platform_objects_for_id_set, this)?;
      let value = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      match &dom_for_id_set.borrow().node(node_id).kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
        _ => return Err(rt.throw_type_error("id: receiver is not an Element")),
      }
      dom_for_id_set
        .borrow_mut()
        .set_attribute(node_id, "id", &value)
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))?;
      Ok(Value::Undefined)
    })?;
    define_accessor(rt, prototypes.element, "id", id_get, id_set)?;

    let dom_for_class_get = dom.clone();
    let platform_objects_for_class_get = platform_objects.clone();
    let class_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_class_get, this)?;
      let dom_ref = dom_for_class_get.borrow();
      match &dom_ref.node(node_id).kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
        _ => return Err(rt.throw_type_error("className: receiver is not an Element")),
      }
      let v = dom_ref
        .get_attribute(node_id, "class")
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))?
        .unwrap_or("");
      rt.alloc_string_value(v)
    })?;
    let dom_for_class_set = dom.clone();
    let platform_objects_for_class_set = platform_objects.clone();
    let class_set = rt.alloc_function_value(move |rt, this, args| {
      let node_id = extract_node_id(rt, &platform_objects_for_class_set, this)?;
      let value = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      match &dom_for_class_set.borrow().node(node_id).kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
        _ => return Err(rt.throw_type_error("className: receiver is not an Element")),
      }
      dom_for_class_set
        .borrow_mut()
        .set_attribute(node_id, "class", &value)
        .map_err(|e| throw_dom_error(rt, dom_exception_proto, e))?;
      Ok(Value::Undefined)
    })?;
    define_accessor(rt, prototypes.element, "className", class_get, class_set)?;

    // `Element.prototype.matches(selectors)`
    let dom_for_matches = dom.clone();
    let platform_objects_for_matches = platform_objects.clone();
    let dom_ex_for_matches = dom_exception;
    let matches = rt.alloc_function_value(move |rt, this, args| {
      if args.len() < 1 {
        return Err(rt.throw_type_error(&format!(
          "Element.matches: expected at least 1 arguments, got {}",
          args.len()
        )));
      }
      let node_id = extract_node_id(rt, &platform_objects_for_matches, this)?;
      {
        let dom_ref = dom_for_matches.borrow();
        match &dom_ref.node(node_id).kind {
          NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
          _ => return Err(rt.throw_type_error("matches: receiver is not an Element")),
        }
      }
      let selectors = to_rust_string(rt, args[0])?;
      let matched = match dom_for_matches.borrow_mut().matches_selector(node_id, &selectors) {
        Ok(v) => v,
        Err(err) => {
          let exc = dom_ex_for_matches.from_dom_exception(rt, &err)?;
          return Err(VmError::Throw(exc));
        }
      };
      Ok(Value::Bool(matched))
    })?;
    define_method(rt, prototypes.element, "matches", matches)?;

    // `Element.prototype.closest(selectors)`
    let dom_for_closest = dom.clone();
    let platform_objects_for_closest = platform_objects.clone();
    let node_wrapper_cache_for_closest = node_wrapper_cache.clone();
    let dom_ex_for_closest = dom_exception;
    let closest = rt.alloc_function_value(move |rt, this, args| {
      if args.len() < 1 {
        return Err(rt.throw_type_error(&format!(
          "Element.closest: expected at least 1 arguments, got {}",
          args.len()
        )));
      }
      let node_id = extract_node_id(rt, &platform_objects_for_closest, this)?;
      {
        let dom_ref = dom_for_closest.borrow();
        match &dom_ref.node(node_id).kind {
          NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
          _ => return Err(rt.throw_type_error("closest: receiver is not an Element")),
        }
      }
      let selectors = to_rust_string(rt, args[0])?;
      let found = match dom_for_closest.borrow_mut().closest(node_id, &selectors) {
        Ok(v) => v,
        Err(err) => {
          let exc = dom_ex_for_closest.from_dom_exception(rt, &err)?;
          return Err(VmError::Throw(exc));
        }
      };
      match found {
        Some(id) => wrap_node(
          rt,
          &dom_for_closest,
          &platform_objects_for_closest,
          &node_wrapper_cache_for_closest,
          document_node_id,
          document,
          prototypes,
          id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_method(rt, prototypes.element, "closest", closest)?;

    // `ParentNode.querySelector(selectors)` scoped to an element.
    let dom_for_el_query = dom.clone();
    let platform_objects_for_el_query = platform_objects.clone();
    let node_wrapper_cache_for_el_query = node_wrapper_cache.clone();
    let dom_ex_for_el_query = dom_exception;
    let query_selector = rt.alloc_function_value(move |rt, this, args| {
      if args.len() < 1 {
        return Err(rt.throw_type_error(&format!(
          "Element.querySelector: expected at least 1 arguments, got {}",
          args.len()
        )));
      }
      let node_id = extract_node_id(rt, &platform_objects_for_el_query, this)?;
      {
        let dom_ref = dom_for_el_query.borrow();
        match &dom_ref.node(node_id).kind {
          NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
          _ => return Err(rt.throw_type_error("querySelector: receiver is not an Element")),
        }
      }
      let selectors = to_rust_string(rt, args[0])?;
      let allow_scope = selector_mentions_scope(&selectors);
      let scope = Some(node_id);
      let filter_self = !allow_scope;

      let found = if filter_self {
        let ids = match dom_for_el_query
          .borrow_mut()
          .query_selector_all(&selectors, scope)
        {
          Ok(v) => v,
          Err(err) => {
            let exc = dom_ex_for_el_query.from_dom_exception(rt, &err)?;
            return Err(VmError::Throw(exc));
          }
        };
        ids.into_iter().find(|id| *id != node_id)
      } else {
        match dom_for_el_query
          .borrow_mut()
          .query_selector(&selectors, scope)
        {
          Ok(v) => v,
          Err(err) => {
            let exc = dom_ex_for_el_query.from_dom_exception(rt, &err)?;
            return Err(VmError::Throw(exc));
          }
        }
      };

      match found {
        Some(id) => wrap_node(
          rt,
          &dom_for_el_query,
          &platform_objects_for_el_query,
          &node_wrapper_cache_for_el_query,
          document_node_id,
          document,
          prototypes,
          id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_method(rt, prototypes.element, "querySelector", query_selector)?;

    let dom_for_el_query_all = dom.clone();
    let platform_objects_for_el_query_all = platform_objects.clone();
    let node_wrapper_cache_for_el_query_all = node_wrapper_cache.clone();
    let dom_ex_for_el_query_all = dom_exception;
    let query_selector_all = rt.alloc_function_value(move |rt, this, args| {
      if args.len() < 1 {
        return Err(rt.throw_type_error(&format!(
          "Element.querySelectorAll: expected at least 1 arguments, got {}",
          args.len()
        )));
      }
      let node_id = extract_node_id(rt, &platform_objects_for_el_query_all, this)?;
      {
        let dom_ref = dom_for_el_query_all.borrow();
        match &dom_ref.node(node_id).kind {
          NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
          _ => return Err(rt.throw_type_error("querySelectorAll: receiver is not an Element")),
        }
      }
      let selectors = to_rust_string(rt, args[0])?;
      let allow_scope = selector_mentions_scope(&selectors);
      let scope = Some(node_id);
      let filter_self = !allow_scope;

      let ids = match dom_for_el_query_all
        .borrow_mut()
        .query_selector_all(&selectors, scope)
      {
        Ok(v) => v,
        Err(err) => {
          let exc = dom_ex_for_el_query_all.from_dom_exception(rt, &err)?;
          return Err(VmError::Throw(exc));
        }
      };

      let arr = rt.alloc_array()?;
      let arr_root = rt.heap_mut().add_root(arr)?;
      let res = (|| {
        let mut idx: u32 = 0;
        for id in ids {
          if filter_self && id == node_id {
            continue;
          }
          let key = rt.property_key_from_u32(idx)?;
          idx = idx.wrapping_add(1);
          let value = wrap_node(
            rt,
            &dom_for_el_query_all,
            &platform_objects_for_el_query_all,
            &node_wrapper_cache_for_el_query_all,
            document_node_id,
            document,
            prototypes,
            id,
          )?;
          rt.define_data_property(arr, key, value, true)?;
        }
        Ok(arr)
      })();
      rt.heap_mut().remove_root(arr_root);
      res
    })?;
    define_method(rt, prototypes.element, "querySelectorAll", query_selector_all)?;
  }

  // Document.prototype
  {
    // `document.readyState`
    let dom_for_ready_state = dom.clone();
    let platform_objects_for_ready_state = platform_objects.clone();
    let ready_state_get = rt.alloc_function_value(move |rt, this, _args| {
      let _doc_id = extract_document_id(rt, &platform_objects_for_ready_state, this)?;
      let state = dom_for_ready_state.borrow().ready_state().as_str();
      rt.alloc_string_value(state)
    })?;
    define_accessor(
      rt,
      prototypes.document,
      "readyState",
      ready_state_get,
      Value::Undefined,
    )?;

    // document.cookie (MVP: deterministic name=value store; ignores attributes).
    //
    // When a `ResourceFetcher` is configured via `DomJsRealm::set_cookie_fetcher_for_document`, we
    // mirror cookie state from the fetcher's `Cookie` header value so JS can observe cookies set by
    // HTTP responses.
    let cookie_jar_for_get = cookie_jar.clone();
    let cookie_fetcher_for_get = cookie_fetcher.clone();
    let document_url_for_get = document_url.clone();
    let platform_objects_for_cookie_get = platform_objects.clone();
    let cookie_get = rt.alloc_function_value(move |rt, this, _args| {
      let _doc_id = extract_document_id(rt, &platform_objects_for_cookie_get, this)?;

      let fetcher = cookie_fetcher_for_get.borrow().clone();
      let url = document_url_for_get.borrow().clone();
      if let (Some(fetcher), Some(url)) = (fetcher.as_ref(), url.as_deref()) {
        if let Some(header) = fetcher.cookie_header_value(url) {
          cookie_jar_for_get
            .borrow_mut()
            .replace_from_cookie_header(&header);
        }
      }

      let cookie = cookie_jar_for_get.borrow().cookie_string();
      rt.alloc_string_value(&cookie)
    })?;
    let cookie_jar_for_set = cookie_jar.clone();
    let cookie_fetcher_for_set = cookie_fetcher.clone();
    let document_url_for_set = document_url.clone();
    let platform_objects_for_cookie_set = platform_objects.clone();
    let cookie_set = rt.alloc_function_value(move |rt, this, args| {
      let _doc_id = extract_document_id(rt, &platform_objects_for_cookie_set, this)?;
      let value = args.get(0).copied().unwrap_or(Value::Undefined);
      let s = rt.to_string(value)?;
      let Value::String(s) = s else {
        return Err(VmError::InvariantViolation("to_string must return a string value"));
      };
      let js_s = rt.heap().get_string(s)?;
      if js_s.as_code_units().len() > MAX_COOKIE_STRING_BYTES {
        return Ok(Value::Undefined);
      }
      let cookie_string = js_s.to_utf8_lossy();

      let fetcher = cookie_fetcher_for_set.borrow().clone();
      let url = document_url_for_set.borrow().clone();
      if let (Some(fetcher), Some(url)) = (fetcher.as_ref(), url.as_deref()) {
        fetcher.store_cookie_from_document(url, &cookie_string);
      }

      cookie_jar_for_set
        .borrow_mut()
        .set_cookie_string(&cookie_string);
      Ok(Value::Undefined)
    })?;
    define_accessor(rt, prototypes.document, "cookie", cookie_get, cookie_set)?;

    // Legacy DOM Events factory: `document.createEvent(interfaceName)`.
    let dom_for_create_event = dom.clone();
    let platform_objects_for_create_event = platform_objects.clone();
    let next_event_id_for_create_event = next_event_id.clone();
    let events_map_for_create_event = events_map.clone();
    let event_detail_roots_for_create_event = event_detail_roots.clone();
    let dom_ex_for_create_event = dom_exception;
    let create_event = rt.alloc_function_value(move |rt, this, args| {
      let _doc_id = extract_document_id(rt, &platform_objects_for_create_event, this)?;
      let interface_name = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      let event = match dom_for_create_event.borrow().create_event(&interface_name) {
        Ok(ev) => ev,
        Err(err) => {
          let exc = dom_ex_for_create_event.from_dom_exception(rt, &err)?;
          return Err(VmError::Throw(exc));
        }
      };

      let is_custom_event = event.detail.is_some();
      let event_id = next_event_id_for_create_event.get();
      next_event_id_for_create_event.set(event_id.wrapping_add(1));
      set_event_detail_root(
        rt,
        &event_detail_roots_for_create_event,
        event_id,
        event.detail,
      )?;
      events_map_for_create_event.borrow_mut().insert(event_id, event);

      let obj = rt.alloc_object_value()?;
      // Keep wrapper objects alive even when only referenced from Rust-side tables.
      let _ = rt.heap_mut().add_root(obj)?;
      rt.set_prototype(
        obj,
        Some(if is_custom_event {
          prototypes.custom_event
        } else {
          prototypes.event
        }),
      )?;
      let Value::Object(obj_handle) = obj else {
        return Err(VmError::InvariantViolation(
          "alloc_object_value must return an object",
        ));
      };
      platform_objects_for_create_event.borrow_mut().insert(
        WeakGcObject::from(obj_handle),
        PlatformObjectKind::Event { event_id },
      );
      Ok(obj)
    })?;
    define_method(rt, prototypes.document, "createEvent", create_event)?;

    let dom_for_create_element = dom.clone();
    let platform_objects_for_create_element = platform_objects.clone();
    let node_wrapper_cache_for_create_element = node_wrapper_cache.clone();
    let create_element = rt.alloc_function_value(move |rt, this, args| {
      let _doc_id = extract_document_id(rt, &platform_objects_for_create_element, this)?;
      let tag_name = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      let node_id = dom_for_create_element
        .borrow_mut()
        .create_element(&tag_name, "");
      wrap_node(
        rt,
        &dom_for_create_element,
        &platform_objects_for_create_element,
        &node_wrapper_cache_for_create_element,
        document_node_id,
        document,
        prototypes,
        node_id,
      )
    })?;
    define_method(rt, prototypes.document, "createElement", create_element)?;

    let dom_for_text = dom.clone();
    let platform_objects_for_text = platform_objects.clone();
    let node_wrapper_cache_for_text = node_wrapper_cache.clone();
    let create_text = rt.alloc_function_value(move |rt, this, args| {
      let _doc_id = extract_document_id(rt, &platform_objects_for_text, this)?;
      let data = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      let node_id = dom_for_text.borrow_mut().create_text(&data);
      wrap_node(
        rt,
        &dom_for_text,
        &platform_objects_for_text,
        &node_wrapper_cache_for_text,
        document_node_id,
        document,
        prototypes,
        node_id,
      )
    })?;
    define_method(rt, prototypes.document, "createTextNode", create_text)?;

    let dom_for_comment = dom.clone();
    let platform_objects_for_comment = platform_objects.clone();
    let node_wrapper_cache_for_comment = node_wrapper_cache.clone();
    let create_comment = rt.alloc_function_value(move |rt, this, args| {
      let _doc_id = extract_document_id(rt, &platform_objects_for_comment, this)?;
      let data = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      let node_id = dom_for_comment.borrow_mut().create_comment(&data);
      wrap_node(
        rt,
        &dom_for_comment,
        &platform_objects_for_comment,
        &node_wrapper_cache_for_comment,
        document_node_id,
        document,
        prototypes,
        node_id,
      )
    })?;
    define_method(rt, prototypes.document, "createComment", create_comment)?;

    let dom_for_create_fragment = dom.clone();
    let platform_objects_for_create_fragment = platform_objects.clone();
    let node_wrapper_cache_for_create_fragment = node_wrapper_cache.clone();
    let create_document_fragment = rt.alloc_function_value(move |rt, this, _args| {
      let _doc_id = extract_document_id(rt, &platform_objects_for_create_fragment, this)?;
      let node_id = dom_for_create_fragment
        .borrow_mut()
        .create_document_fragment();
      wrap_node(
        rt,
        &dom_for_create_fragment,
        &platform_objects_for_create_fragment,
        &node_wrapper_cache_for_create_fragment,
        document_node_id,
        document,
        prototypes,
        node_id,
      )
    })?;
    define_method(
      rt,
      prototypes.document,
      "createDocumentFragment",
      create_document_fragment,
    )?;

    let dom_for_get_by_id = dom.clone();
    let platform_objects_for_get_by_id = platform_objects.clone();
    let node_wrapper_cache_for_get_by_id = node_wrapper_cache.clone();
    let get_by_id = rt.alloc_function_value(move |rt, this, args| {
      let _doc_id = extract_document_id(rt, &platform_objects_for_get_by_id, this)?;
      let id = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      match dom_for_get_by_id.borrow().get_element_by_id(&id) {
        Some(node_id) => wrap_node(
          rt,
          &dom_for_get_by_id,
          &platform_objects_for_get_by_id,
          &node_wrapper_cache_for_get_by_id,
          document_node_id,
          document,
          prototypes,
          node_id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_method(rt, prototypes.document, "getElementById", get_by_id)?;

    // `ParentNode.querySelector(selectors)` for Document.
    let dom_for_doc_query = dom.clone();
    let platform_objects_for_doc_query = platform_objects.clone();
    let node_wrapper_cache_for_doc_query = node_wrapper_cache.clone();
    let dom_ex_for_doc_query = dom_exception;
    let query_selector = rt.alloc_function_value(move |rt, this, args| {
      if args.len() < 1 {
        return Err(rt.throw_type_error(&format!(
          "Document.querySelector: expected at least 1 arguments, got {}",
          args.len()
        )));
      }
      let _doc_id = extract_document_id(rt, &platform_objects_for_doc_query, this)?;
      let selectors = to_rust_string(rt, args[0])?;
      let found = match dom_for_doc_query.borrow_mut().query_selector(&selectors, None) {
        Ok(v) => v,
        Err(err) => {
          let exc = dom_ex_for_doc_query.from_dom_exception(rt, &err)?;
          return Err(VmError::Throw(exc));
        }
      };
      match found {
        Some(id) => wrap_node(
          rt,
          &dom_for_doc_query,
          &platform_objects_for_doc_query,
          &node_wrapper_cache_for_doc_query,
          document_node_id,
          document,
          prototypes,
          id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_method(rt, prototypes.document, "querySelector", query_selector)?;

    let dom_for_doc_query_all = dom.clone();
    let platform_objects_for_doc_query_all = platform_objects.clone();
    let node_wrapper_cache_for_doc_query_all = node_wrapper_cache.clone();
    let dom_ex_for_doc_query_all = dom_exception;
    let query_selector_all = rt.alloc_function_value(move |rt, this, args| {
      if args.len() < 1 {
        return Err(rt.throw_type_error(&format!(
          "Document.querySelectorAll: expected at least 1 arguments, got {}",
          args.len()
        )));
      }
      let _doc_id = extract_document_id(rt, &platform_objects_for_doc_query_all, this)?;
      let selectors = to_rust_string(rt, args[0])?;
      let ids = match dom_for_doc_query_all
        .borrow_mut()
        .query_selector_all(&selectors, None)
      {
        Ok(v) => v,
        Err(err) => {
          let exc = dom_ex_for_doc_query_all.from_dom_exception(rt, &err)?;
          return Err(VmError::Throw(exc));
        }
      };

      let arr = rt.alloc_array()?;
      let arr_root = rt.heap_mut().add_root(arr)?;
      let res = (|| {
        for (idx, id) in ids.into_iter().enumerate() {
          let key = rt.property_key_from_u32(idx as u32)?;
          let value = wrap_node(
            rt,
            &dom_for_doc_query_all,
            &platform_objects_for_doc_query_all,
            &node_wrapper_cache_for_doc_query_all,
            document_node_id,
            document,
            prototypes,
            id,
          )?;
          rt.define_data_property(arr, key, value, true)?;
        }
        Ok(arr)
      })();
      rt.heap_mut().remove_root(arr_root);
      res
    })?;
    define_method(rt, prototypes.document, "querySelectorAll", query_selector_all)?;

    let dom_for_doc_el = dom.clone();
    let platform_objects_for_doc_el = platform_objects.clone();
    let node_wrapper_cache_for_doc_el = node_wrapper_cache.clone();
    let document_element_get = rt.alloc_function_value(move |rt, this, _args| {
      let _doc_id = extract_document_id(rt, &platform_objects_for_doc_el, this)?;
      match dom_for_doc_el.borrow().document_element() {
        Some(node_id) => wrap_node(
          rt,
          &dom_for_doc_el,
          &platform_objects_for_doc_el,
          &node_wrapper_cache_for_doc_el,
          document_node_id,
          document,
          prototypes,
          node_id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(
      rt,
      prototypes.document,
      "documentElement",
      document_element_get,
      Value::Undefined,
    )?;

    let dom_for_head = dom.clone();
    let platform_objects_for_head = platform_objects.clone();
    let node_wrapper_cache_for_head = node_wrapper_cache.clone();
    let head_get = rt.alloc_function_value(move |rt, this, _args| {
      let _doc_id = extract_document_id(rt, &platform_objects_for_head, this)?;
      match dom_for_head.borrow().head() {
        Some(node_id) => wrap_node(
          rt,
          &dom_for_head,
          &platform_objects_for_head,
          &node_wrapper_cache_for_head,
          document_node_id,
          document,
          prototypes,
          node_id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(rt, prototypes.document, "head", head_get, Value::Undefined)?;

    let dom_for_body = dom.clone();
    let platform_objects_for_body = platform_objects.clone();
    let node_wrapper_cache_for_body = node_wrapper_cache.clone();
    let body_get = rt.alloc_function_value(move |rt, this, _args| {
      let _doc_id = extract_document_id(rt, &platform_objects_for_body, this)?;
      match dom_for_body.borrow().body() {
        Some(node_id) => wrap_node(
          rt,
          &dom_for_body,
          &platform_objects_for_body,
          &node_wrapper_cache_for_body,
          document_node_id,
          document,
          prototypes,
          node_id,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(rt, prototypes.document, "body", body_get, Value::Undefined)?;

    let current_script_get = {
      let current_script_state = current_script_state.clone();
      let dom = dom.clone();
      let platform_objects = platform_objects.clone();
      let node_wrapper_cache = node_wrapper_cache.clone();
      rt.alloc_function_value(move |rt, this, _args| {
        let _doc_id = extract_document_id(rt, &platform_objects, this)?;
        let current = current_script_state.borrow().current_script;
        match current {
          Some(node_id) => wrap_node(
            rt,
            &dom,
            &platform_objects,
            &node_wrapper_cache,
            document_node_id,
            document,
            prototypes,
            node_id,
          ),
          None => Ok(Value::Null),
        }
      })?
    };
    define_accessor(
      rt,
      prototypes.document,
      "currentScript",
      current_script_get,
      Value::Undefined,
    )?;
  }

  // Event.prototype
  {
    let platform_objects_for_type = platform_objects.clone();
    let events_map_for_type = events_map.clone();
    let active_events_for_type = active_events.clone();
    let type_get = rt.alloc_function_value(move |rt, this, _args| {
      let event_id = extract_event_id(rt, &platform_objects_for_type, this)?;
      let type_ = with_event(
        rt,
        &active_events_for_type,
        &events_map_for_type,
        event_id,
        |e| e.type_.clone(),
      )?;
      rt.alloc_string_value(&type_)
    })?;
    define_accessor(rt, prototypes.event, "type", type_get, Value::Undefined)?;

    let platform_objects_for_bubbles = platform_objects.clone();
    let events_map_for_bubbles = events_map.clone();
    let active_events_for_bubbles = active_events.clone();
    let bubbles_get = rt.alloc_function_value(move |rt, this, _args| {
      let event_id = extract_event_id(rt, &platform_objects_for_bubbles, this)?;
      let bubbles = with_event(
        rt,
        &active_events_for_bubbles,
        &events_map_for_bubbles,
        event_id,
        |e| e.bubbles,
      )?;
      Ok(Value::Bool(bubbles))
    })?;
    define_accessor(rt, prototypes.event, "bubbles", bubbles_get, Value::Undefined)?;

    let platform_objects_for_cancelable = platform_objects.clone();
    let events_map_for_cancelable = events_map.clone();
    let active_events_for_cancelable = active_events.clone();
    let cancelable_get = rt.alloc_function_value(move |rt, this, _args| {
      let event_id = extract_event_id(rt, &platform_objects_for_cancelable, this)?;
      let cancelable = with_event(
        rt,
        &active_events_for_cancelable,
        &events_map_for_cancelable,
        event_id,
        |e| e.cancelable,
      )?;
      Ok(Value::Bool(cancelable))
    })?;
    define_accessor(rt, prototypes.event, "cancelable", cancelable_get, Value::Undefined)?;

    let platform_objects_for_default_prevented = platform_objects.clone();
    let events_map_for_default_prevented = events_map.clone();
    let active_events_for_default_prevented = active_events.clone();
    let default_prevented_get = rt.alloc_function_value(move |rt, this, _args| {
      let event_id = extract_event_id(rt, &platform_objects_for_default_prevented, this)?;
      let default_prevented = with_event(
        rt,
        &active_events_for_default_prevented,
        &events_map_for_default_prevented,
        event_id,
        |e| e.default_prevented,
      )?;
      Ok(Value::Bool(default_prevented))
    })?;
    define_accessor(
      rt,
      prototypes.event,
      "defaultPrevented",
      default_prevented_get,
      Value::Undefined,
    )?;

    let platform_objects_for_phase = platform_objects.clone();
    let events_map_for_phase = events_map.clone();
    let active_events_for_phase = active_events.clone();
    let event_phase_get = rt.alloc_function_value(move |rt, this, _args| {
      let event_id = extract_event_id(rt, &platform_objects_for_phase, this)?;
      let phase = with_event(
        rt,
        &active_events_for_phase,
        &events_map_for_phase,
        event_id,
        |e| e.event_phase,
      )?;
      let phase = match phase {
        events::EventPhase::None => 0.0,
        events::EventPhase::Capturing => 1.0,
        events::EventPhase::AtTarget => 2.0,
        events::EventPhase::Bubbling => 3.0,
      };
      Ok(Value::Number(phase))
    })?;
    define_accessor(rt, prototypes.event, "eventPhase", event_phase_get, Value::Undefined)?;

    let platform_objects_for_target = platform_objects.clone();
    let events_map_for_target = events_map.clone();
    let active_events_for_target = active_events.clone();
    let dom_for_target = dom.clone();
    let node_wrapper_cache_for_target = node_wrapper_cache.clone();
    let target_get = rt.alloc_function_value(move |rt, this, _args| {
      let event_id = extract_event_id(rt, &platform_objects_for_target, this)?;
      let target = with_event(
        rt,
        &active_events_for_target,
        &events_map_for_target,
        event_id,
        |e| e.target,
      )?;
      match target {
        Some(t) => wrap_event_target(
          rt,
          &dom_for_target,
          &platform_objects_for_target,
          &node_wrapper_cache_for_target,
          document_node_id,
          window,
          document,
          prototypes,
          t,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(rt, prototypes.event, "target", target_get, Value::Undefined)?;

    let platform_objects_for_current_target = platform_objects.clone();
    let events_map_for_current_target = events_map.clone();
    let active_events_for_current_target = active_events.clone();
    let dom_for_current_target = dom.clone();
    let node_wrapper_cache_for_current_target = node_wrapper_cache.clone();
    let current_target_get = rt.alloc_function_value(move |rt, this, _args| {
      let event_id = extract_event_id(rt, &platform_objects_for_current_target, this)?;
      let current_target = with_event(
        rt,
        &active_events_for_current_target,
        &events_map_for_current_target,
        event_id,
        |e| e.current_target,
      )?;
      match current_target {
        Some(t) => wrap_event_target(
          rt,
          &dom_for_current_target,
          &platform_objects_for_current_target,
          &node_wrapper_cache_for_current_target,
          document_node_id,
          window,
          document,
          prototypes,
          t,
        ),
        None => Ok(Value::Null),
      }
    })?;
    define_accessor(
      rt,
      prototypes.event,
      "currentTarget",
      current_target_get,
      Value::Undefined,
    )?;

    let platform_objects_for_stop = platform_objects.clone();
    let events_map_for_stop = events_map.clone();
    let active_events_for_stop = active_events.clone();
    let stop_propagation = rt.alloc_function_value(move |rt, this, _args| {
      let event_id = extract_event_id(rt, &platform_objects_for_stop, this)?;
      with_event(rt, &active_events_for_stop, &events_map_for_stop, event_id, |event| {
        event.stop_propagation();
      })?;
      Ok(Value::Undefined)
    })?;
    define_method(rt, prototypes.event, "stopPropagation", stop_propagation)?;

    let platform_objects_for_stop_immediate = platform_objects.clone();
    let events_map_for_stop_immediate = events_map.clone();
    let active_events_for_stop_immediate = active_events.clone();
    let stop_immediate = rt.alloc_function_value(move |rt, this, _args| {
      let event_id = extract_event_id(rt, &platform_objects_for_stop_immediate, this)?;
      with_event(
        rt,
        &active_events_for_stop_immediate,
        &events_map_for_stop_immediate,
        event_id,
        |event| {
        event.stop_immediate_propagation();
        },
      )?;
      Ok(Value::Undefined)
    })?;
    define_method(rt, prototypes.event, "stopImmediatePropagation", stop_immediate)?;

    let platform_objects_for_prevent = platform_objects.clone();
    let events_map_for_prevent = events_map.clone();
    let active_events_for_prevent = active_events.clone();
    let prevent_default = rt.alloc_function_value(move |rt, this, _args| {
      let event_id = extract_event_id(rt, &platform_objects_for_prevent, this)?;
      with_event(
        rt,
        &active_events_for_prevent,
        &events_map_for_prevent,
        event_id,
        |event| {
        event.prevent_default();
        },
      )?;
      Ok(Value::Undefined)
    })?;
    define_method(rt, prototypes.event, "preventDefault", prevent_default)?;

    let platform_objects_for_init_event = platform_objects.clone();
    let events_map_for_init_event = events_map.clone();
    let active_events_for_init_event = active_events.clone();
    let init_event = rt.alloc_function_value(move |rt, this, args| {
      let event_id = extract_event_id(rt, &platform_objects_for_init_event, this)?;
      let type_ = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      let bubbles = rt.to_boolean(args.get(1).copied().unwrap_or(Value::Undefined))?;
      let cancelable = rt.to_boolean(args.get(2).copied().unwrap_or(Value::Undefined))?;
      with_event(
        rt,
        &active_events_for_init_event,
        &events_map_for_init_event,
        event_id,
        move |event| {
          event.init_event(type_, bubbles, cancelable);
        },
      )?;
      Ok(Value::Undefined)
    })?;
    define_method(rt, prototypes.event, "initEvent", init_event)?;
  }

  // CustomEvent.prototype
  {
    let platform_objects_for_detail = platform_objects.clone();
    let events_map_for_detail = events_map.clone();
    let active_events_for_detail = active_events.clone();
    let detail_get = rt.alloc_function_value(move |rt, this, _args| {
      let event_id = extract_event_id(rt, &platform_objects_for_detail, this)?;
      let detail = with_event(
        rt,
        &active_events_for_detail,
        &events_map_for_detail,
        event_id,
        |e| e.detail.unwrap_or(Value::Null),
      )?;
      Ok(detail)
    })?;
    define_accessor(rt, prototypes.custom_event, "detail", detail_get, Value::Undefined)?;

    let platform_objects_for_init_custom = platform_objects.clone();
    let events_map_for_init_custom = events_map.clone();
    let active_events_for_init_custom = active_events.clone();
    let event_detail_roots_for_init_custom = event_detail_roots.clone();
    let init_custom_event = rt.alloc_function_value(move |rt, this, args| {
      let event_id = extract_event_id(rt, &platform_objects_for_init_custom, this)?;
      let type_ = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      let bubbles = rt.to_boolean(args.get(1).copied().unwrap_or(Value::Undefined))?;
      let cancelable = rt.to_boolean(args.get(2).copied().unwrap_or(Value::Undefined))?;
      let detail = args.get(3).copied().unwrap_or(Value::Undefined);

      // Root the new detail payload so it survives GC even though it is stored only in Rust tables.
      set_event_detail_root(
        rt,
        &event_detail_roots_for_init_custom,
        event_id,
        Some(detail),
      )?;

      with_event(
        rt,
        &active_events_for_init_custom,
        &events_map_for_init_custom,
        event_id,
        move |event| {
          event.init_custom_event(type_, bubbles, cancelable, detail);
        },
      )?;
      Ok(Value::Undefined)
    })?;
    define_method(rt, prototypes.custom_event, "initCustomEvent", init_custom_event)?;
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::error::{Error, Result};
  use crate::resource::{FetchedResource, ResourceFetcher};
  use selectors::context::QuirksMode;
  use std::cell::Cell;
  use std::sync::{Arc, Mutex};
  use webidl_js_runtime::JsPropertyKind;

  fn pk(rt: &mut VmJsRuntime, name: &str) -> PropertyKey {
    prop_key_str(rt, name).unwrap()
  }

  fn as_str(rt: &VmJsRuntime, v: Value) -> String {
    let Value::String(s) = v else {
      panic!("expected string, got {v:?}");
    };
    rt.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  #[derive(Default)]
  struct CookieRecordingFetcher {
    cookies: Mutex<Vec<(String, String)>>,
  }

  impl CookieRecordingFetcher {
    fn cookie_header(&self) -> String {
      let lock = self.cookies.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      lock
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
    }
  }

  impl ResourceFetcher for CookieRecordingFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
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

      let mut lock = self.cookies.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      if let Some(existing) = lock.iter_mut().find(|(n, _)| n == name) {
        existing.1 = value.to_string();
      } else {
        lock.push((name.to_string(), value.to_string()));
      }
    }
  }

  #[test]
  fn document_cookie_get_and_set_round_trips_deterministically() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let cookie_key = pk(&mut realm.rt, "cookie");
    let initial = realm.rt.get(document, cookie_key).unwrap();
    assert_eq!(as_str(&realm.rt, initial), "");

    let desc = realm
      .rt
      .get_own_property(realm.prototypes.document, cookie_key)
      .unwrap()
      .expect("expected Document.prototype.cookie");
    let set = match desc.kind {
      JsPropertyKind::Accessor { set, .. } => set,
      other => panic!("expected accessor property, got {other:?}"),
    };

    // Attributes are ignored and output ordering is deterministic (sorted by cookie name).
    let b = realm.rt.alloc_string_value("b=c; Path=/").unwrap();
    realm.rt.call_function(set, document, &[b]).unwrap();
    let a = realm.rt.alloc_string_value("a=b").unwrap();
    realm.rt.call_function(set, document, &[a]).unwrap();

    let got = realm.rt.get(document, cookie_key).unwrap();
    assert_eq!(as_str(&realm.rt, got), "a=b; b=c");
  }

  #[test]
  fn document_cookie_syncs_with_fetcher_cookie_store() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let fetcher = Arc::new(CookieRecordingFetcher::default());
    fetcher.store_cookie_from_document("https://example.invalid/", "z=1");
    realm.set_cookie_fetcher_for_document("https://example.invalid/", fetcher.clone());

    let cookie_key = pk(&mut realm.rt, "cookie");
    let initial = realm.rt.get(document, cookie_key).unwrap();
    assert_eq!(as_str(&realm.rt, initial), "z=1");

    let desc = realm
      .rt
      .get_own_property(realm.prototypes.document, cookie_key)
      .unwrap()
      .expect("expected Document.prototype.cookie");
    let set = match desc.kind {
      JsPropertyKind::Accessor { set, .. } => set,
      other => panic!("expected accessor property, got {other:?}"),
    };

    let b = realm.rt.alloc_string_value("b=c; Path=/").unwrap();
    realm.rt.call_function(set, document, &[b]).unwrap();
    let a = realm.rt.alloc_string_value("a=b").unwrap();
    realm.rt.call_function(set, document, &[a]).unwrap();

    assert_eq!(
      fetcher
        .cookie_header_value("https://example.invalid/")
        .unwrap_or_default(),
      "z=1; b=c; a=b"
    );

    let got = realm.rt.get(document, cookie_key).unwrap();
    assert_eq!(as_str(&realm.rt, got), "a=b; b=c; z=1");
  }

  #[test]
  fn wrap_node_returns_stable_identity() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let node_id = realm.dom.borrow_mut().create_element("div", "");

    let a = realm.wrap_node(node_id).unwrap();
    let b = realm.wrap_node(node_id).unwrap();
    assert_eq!(a, b);
  }

  #[test]
  fn node_wrapper_cache_is_weak_and_allows_gc_recycling() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let node_id = realm.dom.borrow_mut().create_element("div", "");

    let a = realm.wrap_node(node_id).unwrap();
    let Value::Object(obj_a) = a else {
      panic!("expected object wrapper, got {a:?}");
    };

    // Keep the wrapper live by attaching it to a rooted global (Window).
    let tmp_key = pk(&mut realm.rt, "tmp");
    realm
      .rt
      .define_data_property(realm.window(), tmp_key, a, true)
      .unwrap();

    // Drop the last JS-visible reference and force a GC cycle.
    realm
      .rt
      .define_data_property(realm.window(), tmp_key, Value::Null, true)
      .unwrap();
    realm.rt.heap_mut().collect_garbage();

    assert!(
      !realm.rt.heap().is_valid_object(obj_a),
      "expected wrapper to be collectable after dropping JS refs",
    );

    // Looking up the node again should create a fresh wrapper.
    let b = realm.wrap_node(node_id).unwrap();
    let Value::Object(obj_b) = b else {
      panic!("expected object wrapper, got {b:?}");
    };
    assert!(realm.rt.heap().is_valid_object(obj_b));
    assert_ne!(obj_a, obj_b);
  }

  #[test]
  fn create_element_and_append_child_mutates_dom2_tree() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let document = realm.document();

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let div = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();

    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(document, append_child_key).unwrap();
    realm
      .rt
      .call_function(append_child, document, &[div])
      .unwrap();

    // Assert that the appended node is now connected under the document root.
    let dom = realm.dom.borrow();
    assert_eq!(dom.node(dom.root()).children.len(), 1);
    let child = dom.node(dom.root()).children[0];
    assert!(matches!(dom.node(child).kind, NodeKind::Element { .. } | NodeKind::Slot { .. }));
  }

  #[test]
  fn document_create_comment_exposes_comment_node() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_comment_key = pk(&mut realm.rt, "createComment");
    let create_comment = realm
      .rt
      .get(document, create_comment_key)
      .expect("Document.createComment should exist");

    let content = realm.rt.alloc_string_value("hello").unwrap();
    let comment = realm
      .rt
      .call_function(create_comment, document, &[content])
      .unwrap();

    let node_type_key = pk(&mut realm.rt, "nodeType");
    assert_eq!(realm.rt.get(comment, node_type_key).unwrap(), Value::Number(8.0));

    let node_name_key = pk(&mut realm.rt, "nodeName");
    let node_name = realm.rt.get(comment, node_name_key).unwrap();
    assert_eq!(as_str(&realm.rt, node_name), "#comment");
    let comment_id = extract_node_id(&mut realm.rt, &realm.platform_objects, comment).unwrap();
    let dom_ref = realm.dom.borrow();
    match &dom_ref.node(comment_id).kind {
      NodeKind::Comment { content } => assert_eq!(content, "hello"),
      other => panic!("expected comment node, got {other:?}"),
    }
    assert_eq!(dom_ref.parent(comment_id).unwrap(), None);
  }

  #[test]
  fn create_document_fragment_inserts_children_transparently() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let document = realm.document();

    let div_id = realm.dom.borrow_mut().create_element("div", "");
    let span_id = realm.dom.borrow_mut().create_element("span", "");
    let p_id = realm.dom.borrow_mut().create_element("p", "");

    let div = realm.wrap_node(div_id).unwrap();
    let span = realm.wrap_node(span_id).unwrap();
    let p = realm.wrap_node(p_id).unwrap();

    let create_fragment_key = pk(&mut realm.rt, "createDocumentFragment");
    let create_fragment = realm.rt.get(document, create_fragment_key).unwrap();
    let fragment = realm
      .rt
      .call_function(create_fragment, document, &[])
      .unwrap();
    let fragment_id = extract_node_id(&mut realm.rt, &realm.platform_objects, fragment).unwrap();

    let append_child_key = pk(&mut realm.rt, "appendChild");
    let fragment_append = realm.rt.get(fragment, append_child_key).unwrap();
    realm
      .rt
      .call_function(fragment_append, fragment, &[span])
      .unwrap();
    realm
      .rt
      .call_function(fragment_append, fragment, &[p])
      .unwrap();

    let div_append = realm.rt.get(div, append_child_key).unwrap();
    let returned = realm
      .rt
      .call_function(div_append, div, &[fragment])
      .unwrap();
    assert_eq!(returned, fragment);

    let dom_ref = realm.dom.borrow();
    assert_eq!(dom_ref.children(div_id).unwrap(), &[span_id, p_id]);
    assert!(dom_ref.children(fragment_id).unwrap().is_empty());
    assert_eq!(dom_ref.parent(span_id).unwrap(), Some(div_id));
    assert_eq!(dom_ref.parent(p_id).unwrap(), Some(div_id));
    assert_eq!(dom_ref.parent(fragment_id).unwrap(), None);
  }

  #[test]
  fn contains_and_has_child_nodes_reflect_dom_relationships() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let div_id = realm.dom.borrow_mut().create_element("div", "");
    let span_id = realm.dom.borrow_mut().create_element("span", "");
    let q_id = realm.dom.borrow_mut().create_element("q", "");

    let div = realm.wrap_node(div_id).unwrap();
    let span = realm.wrap_node(span_id).unwrap();
    let q = realm.wrap_node(q_id).unwrap();

    let append_child_key = pk(&mut realm.rt, "appendChild");
    let div_append = realm.rt.get(div, append_child_key).unwrap();
    realm
      .rt
      .call_function(div_append, div, &[span])
      .unwrap();

    let has_child_nodes_key = pk(&mut realm.rt, "hasChildNodes");
    let has_child_nodes = realm.rt.get(div, has_child_nodes_key).unwrap();
    assert_eq!(
      realm.rt.call_function(has_child_nodes, div, &[]).unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      realm.rt.call_function(has_child_nodes, span, &[]).unwrap(),
      Value::Bool(false)
    );

    let contains_key = pk(&mut realm.rt, "contains");
    let contains = realm.rt.get(div, contains_key).unwrap();
    assert_eq!(
      realm.rt.call_function(contains, div, &[span]).unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      realm.rt.call_function(contains, div, &[div]).unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      realm.rt.call_function(contains, span, &[div]).unwrap(),
      Value::Bool(false)
    );
    assert_eq!(
      realm.rt.call_function(contains, div, &[q]).unwrap(),
      Value::Bool(false)
    );
    assert_eq!(
      realm
        .rt
        .call_function(contains, div, &[Value::Null])
        .unwrap(),
      Value::Bool(false)
    );
    assert_eq!(
      realm
        .rt
        .call_function(contains, div, &[Value::Undefined])
        .unwrap(),
      Value::Bool(false)
    );
  }

  #[test]
  fn insert_before_inserts_at_reference_and_allows_null_reference() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let div_id = realm.dom.borrow_mut().create_element("div", "");
    let span_id = realm.dom.borrow_mut().create_element("span", "");
    let p_id = realm.dom.borrow_mut().create_element("p", "");

    realm
      .dom
      .borrow_mut()
      .append_child(div_id, span_id)
      .unwrap();

    let div = realm.wrap_node(div_id).unwrap();
    let span = realm.wrap_node(span_id).unwrap();
    let p = realm.wrap_node(p_id).unwrap();

    let insert_before_key = pk(&mut realm.rt, "insertBefore");
    let insert_before = realm.rt.get(div, insert_before_key).unwrap();
    realm
      .rt
      .call_function(insert_before, div, &[p, span])
      .unwrap();

    {
      let dom_ref = realm.dom.borrow();
      assert_eq!(dom_ref.children(div_id).unwrap(), &[p_id, span_id]);
    }

    // `null` should append.
    let q_id = realm.dom.borrow_mut().create_element("q", "");
    let q = realm.wrap_node(q_id).unwrap();
    realm
      .rt
      .call_function(insert_before, div, &[q, Value::Null])
      .unwrap();

    let dom_ref = realm.dom.borrow();
    assert_eq!(dom_ref.children(div_id).unwrap(), &[p_id, span_id, q_id]);
  }

  #[test]
  fn replace_child_replaces_and_returns_old_child() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let div_id = realm.dom.borrow_mut().create_element("div", "");
    let span_id = realm.dom.borrow_mut().create_element("span", "");
    let p_id = realm.dom.borrow_mut().create_element("p", "");

    realm
      .dom
      .borrow_mut()
      .append_child(div_id, span_id)
      .unwrap();

    let div = realm.wrap_node(div_id).unwrap();
    let span = realm.wrap_node(span_id).unwrap();
    let p = realm.wrap_node(p_id).unwrap();

    let replace_child_key = pk(&mut realm.rt, "replaceChild");
    let replace_child = realm.rt.get(div, replace_child_key).unwrap();
    let returned = realm
      .rt
      .call_function(replace_child, div, &[p, span])
      .unwrap();
    assert_eq!(returned, span);

    let dom_ref = realm.dom.borrow();
    assert_eq!(dom_ref.children(div_id).unwrap(), &[p_id]);
    assert_eq!(dom_ref.parent(span_id).unwrap(), None);
  }

  #[test]
  fn append_child_throws_hierarchy_request_error_domexception() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let marker_value = realm.rt.alloc_string_value("marker").unwrap();
    let dom_exception_ctor_key = pk(&mut realm.rt, "DOMException");
    let dom_exception_ctor = realm.rt.get(realm.window(), dom_exception_ctor_key).unwrap();
    let dom_exception_proto_key = pk(&mut realm.rt, "prototype");
    let dom_exception_proto = realm
      .rt
      .get(dom_exception_ctor, dom_exception_proto_key)
      .unwrap();
    assert_eq!(dom_exception_proto, realm.dom_exception_prototype());
    define_data_property_str(
      &mut realm.rt,
      dom_exception_proto,
      "__dom_exception_marker",
      marker_value,
      false,
    )
    .unwrap();

    let document = realm.document();

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let div = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();

    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(div, append_child_key).unwrap();
    let err = realm
      .rt
      .call_function(append_child, div, &[document])
      .expect_err("expected HierarchyRequestError");

    let Some(thrown) = err.thrown_value() else {
      panic!("expected thrown error, got {err:?}");
    };

    let name_key = pk(&mut realm.rt, "name");
    let name = realm.rt.get(thrown, name_key).unwrap();
    assert_eq!(as_str(&realm.rt, name), "HierarchyRequestError");

    let ctor_key = pk(&mut realm.rt, "constructor");
    let ctor = realm.rt.get(thrown, ctor_key).unwrap();
    assert_eq!(ctor, dom_exception_ctor);

    let marker_key = pk(&mut realm.rt, "__dom_exception_marker");
    let marker = realm.rt.get(thrown, marker_key).unwrap();
    assert_eq!(as_str(&realm.rt, marker), "marker");
  }

  #[test]
  fn set_attribute_and_get_attribute_roundtrip() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let document = realm.document();
    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let el = realm
      .rt
      .call_function(
        create_element,
        document,
        &[div_tag],
      )
      .unwrap();

    let set_attribute_key = pk(&mut realm.rt, "setAttribute");
    let set_attribute = realm.rt.get(el, set_attribute_key).unwrap();
    let id_str = realm.rt.alloc_string_value("id").unwrap();
    let x_str = realm.rt.alloc_string_value("x").unwrap();
    realm
      .rt
      .call_function(
        set_attribute,
        el,
        &[
          id_str,
          x_str,
        ],
      )
      .unwrap();

    let get_attribute_key = pk(&mut realm.rt, "getAttribute");
    let get_attribute = realm.rt.get(el, get_attribute_key).unwrap();
    let id_str2 = realm.rt.alloc_string_value("id").unwrap();
    let got = realm
      .rt
      .call_function(get_attribute, el, &[id_str2])
      .unwrap();
    assert_eq!(as_str(&realm.rt, got), "x");
  }

  #[test]
  fn get_element_by_id_finds_attached_nodes() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let document = realm.document();
    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let el = realm
      .rt
      .call_function(
        create_element,
        document,
        &[div_tag],
      )
      .unwrap();

    let set_attribute_key = pk(&mut realm.rt, "setAttribute");
    let set_attribute = realm.rt.get(el, set_attribute_key).unwrap();
    let id_str = realm.rt.alloc_string_value("id").unwrap();
    let x_str = realm.rt.alloc_string_value("x").unwrap();
    realm
      .rt
      .call_function(
        set_attribute,
        el,
        &[
          id_str,
          x_str,
        ],
      )
      .unwrap();

    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(document, append_child_key).unwrap();
    realm
      .rt
      .call_function(append_child, document, &[el])
      .unwrap();

    let get_element_by_id_key = pk(&mut realm.rt, "getElementById");
    let get_element_by_id = realm.rt.get(document, get_element_by_id_key).unwrap();
    let x_str2 = realm.rt.alloc_string_value("x").unwrap();
    let found = realm
      .rt
      .call_function(
        get_element_by_id,
        document,
        &[x_str2],
      )
      .unwrap();

    assert_eq!(found, el, "wrapper identity should be preserved");
  }

  #[test]
  fn selector_apis_respect_element_scope_and_scope_pseudo_class() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(document, append_child_key).unwrap();

    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let div = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();
    let span_tag = realm.rt.alloc_string_value("span").unwrap();
    let span = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();

    let set_attribute_key = pk(&mut realm.rt, "setAttribute");
    let set_attribute_div = realm.rt.get(div, set_attribute_key).unwrap();
    let class_str = realm.rt.alloc_string_value("class").unwrap();
    let x_str = realm.rt.alloc_string_value("x").unwrap();
    realm
      .rt
      .call_function(set_attribute_div, div, &[class_str, x_str])
      .unwrap();

    let set_attribute_span = realm.rt.get(span, set_attribute_key).unwrap();
    let class_str2 = realm.rt.alloc_string_value("class").unwrap();
    let x_str2 = realm.rt.alloc_string_value("x").unwrap();
    realm
      .rt
      .call_function(set_attribute_span, span, &[class_str2, x_str2])
      .unwrap();

    // Build: document -> div.x -> span.x
    realm
      .rt
      .call_function(append_child, div, &[span])
      .unwrap();
    realm
      .rt
      .call_function(append_child, document, &[div])
      .unwrap();

    let doc_query_selector_key = pk(&mut realm.rt, "querySelector");
    let doc_query_selector = realm.rt.get(document, doc_query_selector_key).unwrap();
    let sel_x = realm.rt.alloc_string_value(".x").unwrap();
    let doc_found = realm
      .rt
      .call_function(doc_query_selector, document, &[sel_x])
      .unwrap();
    assert_eq!(
      doc_found, div,
      "Document.querySelector should return the first match in document order"
    );

    let el_query_selector_key = pk(&mut realm.rt, "querySelector");
    let el_query_selector = realm.rt.get(div, el_query_selector_key).unwrap();

    // Element-scoped querySelector should not include the scope element unless :scope is used.
    let sel_x2 = realm.rt.alloc_string_value(".x").unwrap();
    let el_found = realm
      .rt
      .call_function(el_query_selector, div, &[sel_x2])
      .unwrap();
    assert_eq!(
      el_found, span,
      "Element.querySelector should exclude the scope element for non-:scope selectors"
    );

    let sel_scope = realm.rt.alloc_string_value(":scope").unwrap();
    let scope_found = realm
      .rt
      .call_function(el_query_selector, div, &[sel_scope])
      .unwrap();
    assert_eq!(scope_found, div, "Element.querySelector(:scope) should return self");

    let el_query_selector_all_key = pk(&mut realm.rt, "querySelectorAll");
    let el_query_selector_all = realm.rt.get(div, el_query_selector_all_key).unwrap();

    let sel_x3 = realm.rt.alloc_string_value(".x").unwrap();
    let list = realm
      .rt
      .call_function(el_query_selector_all, div, &[sel_x3])
      .unwrap();
    assert!(realm.rt.is_object(list));
    let key0 = realm.rt.property_key_from_u32(0).unwrap();
    let first = realm.rt.get(list, key0).unwrap();
    assert_eq!(first, span);
    let key1 = realm.rt.property_key_from_u32(1).unwrap();
    let second = realm.rt.get(list, key1).unwrap();
    assert!(matches!(second, Value::Undefined));

    let sel_scope2 = realm.rt.alloc_string_value(":scope").unwrap();
    let list2 = realm
      .rt
      .call_function(el_query_selector_all, div, &[sel_scope2])
      .unwrap();
    let key0 = realm.rt.property_key_from_u32(0).unwrap();
    let first2 = realm.rt.get(list2, key0).unwrap();
    assert_eq!(first2, div);
  }

  #[test]
  fn selector_apis_support_matches_closest_and_domexception_syntaxerror() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(document, append_child_key).unwrap();

    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let div = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();
    let span_tag = realm.rt.alloc_string_value("span").unwrap();
    let span = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();

    realm
      .rt
      .call_function(append_child, div, &[span])
      .unwrap();
    realm
      .rt
      .call_function(append_child, document, &[div])
      .unwrap();

    let matches_key = pk(&mut realm.rt, "matches");
    let matches_fn = realm.rt.get(span, matches_key).unwrap();
    let sel_span = realm.rt.alloc_string_value("span").unwrap();
    let matched = realm
      .rt
      .call_function(matches_fn, span, &[sel_span])
      .unwrap();
    assert_eq!(matched, Value::Bool(true));

    let closest_key = pk(&mut realm.rt, "closest");
    let closest_fn = realm.rt.get(span, closest_key).unwrap();
    let sel_div = realm.rt.alloc_string_value("div").unwrap();
    let closest_div = realm
      .rt
      .call_function(closest_fn, span, &[sel_div])
      .unwrap();
    assert_eq!(closest_div, div);

    let sel_section = realm.rt.alloc_string_value("section").unwrap();
    let closest_none = realm
      .rt
      .call_function(closest_fn, span, &[sel_section])
      .unwrap();
    assert_eq!(closest_none, Value::Null);

    // Invalid selector strings should throw DOMException(name="SyntaxError"), not a JS SyntaxError.
    let bad_sel = realm.rt.alloc_string_value("[").unwrap();
    let err = realm
      .rt
      .call_function(matches_fn, span, &[bad_sel])
      .unwrap_err();
    let Some(thrown) = err.thrown_value() else {
      panic!("expected thrown error, got {err:?}");
    };

    let name_key = pk(&mut realm.rt, "name");
    let name = realm.rt.get(thrown, name_key).unwrap();
    let name = realm.rt.to_string(name).unwrap();
    assert_eq!(as_str(&realm.rt, name), "SyntaxError");

    let ctor_key = pk(&mut realm.rt, "constructor");
    let ctor = realm.rt.get(thrown, ctor_key).unwrap();
    let domexc_key = pk(&mut realm.rt, "DOMException");
    let domexc_ctor = realm.rt.get(realm.window(), domexc_key).unwrap();
    assert_eq!(ctor, domexc_ctor);

    let syntax_key = pk(&mut realm.rt, "SyntaxError");
    let syntax_ctor = realm.rt.get(realm.window(), syntax_key).unwrap();
    if realm.rt.is_callable(syntax_ctor) {
      assert_ne!(ctor, syntax_ctor);
    }
  }

  #[test]
  fn current_script_reflects_host_state() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let script = realm.dom.borrow_mut().create_element("script", "");
    realm.current_script_state.borrow_mut().current_script = Some(script);

    let document = realm.document();
    let current_script_key = pk(&mut realm.rt, "currentScript");
    let got = realm.rt.get(document, current_script_key).unwrap();

    assert_eq!(got, realm.wrap_node(script).unwrap());
  }

  #[test]
  fn child_nodes_are_live_and_element_traversal_properties_work() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let document = realm.document();
    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();

    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let span_tag = realm.rt.alloc_string_value("span").unwrap();

    let parent = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();
    let a = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();
    let b = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();

    // parent.appendChild(a); parent.appendChild(b);
    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(parent, append_child_key).unwrap();
    realm.rt.call_function(append_child, parent, &[a]).unwrap();
    realm.rt.call_function(append_child, parent, &[b]).unwrap();

    // document.appendChild(parent);
    let append_child_doc = realm.rt.get(document, append_child_key).unwrap();
    realm
      .rt
      .call_function(append_child_doc, document, &[parent])
      .unwrap();

    // childNodes should be cached and live-ish.
    let child_nodes_key = pk(&mut realm.rt, "childNodes");
    let nodes = realm.rt.get(parent, child_nodes_key).unwrap();
    let nodes2 = realm.rt.get(parent, child_nodes_key).unwrap();
    assert_eq!(nodes, nodes2);

    let length_key = pk(&mut realm.rt, "length");
    assert_eq!(realm.rt.get(nodes, length_key).unwrap(), Value::Number(2.0));

    let idx0 = pk(&mut realm.rt, "0");
    let idx1 = pk(&mut realm.rt, "1");
    assert_eq!(realm.rt.get(nodes, idx0).unwrap(), a);
    assert_eq!(realm.rt.get(nodes, idx1).unwrap(), b);

    let parent_element_key = pk(&mut realm.rt, "parentElement");
    assert_eq!(realm.rt.get(a, parent_element_key).unwrap(), parent);
    assert_eq!(realm.rt.get(b, parent_element_key).unwrap(), parent);

    let children_key = pk(&mut realm.rt, "children");
    let kids = realm.rt.get(parent, children_key).unwrap();
    assert_eq!(realm.rt.get(kids, length_key).unwrap(), Value::Number(2.0));
    assert_eq!(realm.rt.get(kids, idx0).unwrap(), a);
    assert_eq!(realm.rt.get(kids, idx1).unwrap(), b);

    let child_element_count_key = pk(&mut realm.rt, "childElementCount");
    assert_eq!(
      realm.rt.get(parent, child_element_count_key).unwrap(),
      Value::Number(2.0)
    );

    let first_element_child_key = pk(&mut realm.rt, "firstElementChild");
    let last_element_child_key = pk(&mut realm.rt, "lastElementChild");
    assert_eq!(realm.rt.get(parent, first_element_child_key).unwrap(), a);
    assert_eq!(realm.rt.get(parent, last_element_child_key).unwrap(), b);

    let next_element_sibling_key = pk(&mut realm.rt, "nextElementSibling");
    let previous_element_sibling_key = pk(&mut realm.rt, "previousElementSibling");
    assert_eq!(realm.rt.get(a, next_element_sibling_key).unwrap(), b);
    assert_eq!(realm.rt.get(b, previous_element_sibling_key).unwrap(), a);

    // parent.removeChild(a);
    let remove_child_key = pk(&mut realm.rt, "removeChild");
    let remove_child = realm.rt.get(parent, remove_child_key).unwrap();
    realm.rt.call_function(remove_child, parent, &[a]).unwrap();

    // The cached `nodes` array should update in place.
    assert_eq!(realm.rt.get(nodes, length_key).unwrap(), Value::Number(1.0));
    assert_eq!(realm.rt.get(nodes, idx0).unwrap(), b);

    // a should be detached.
    let parent_node_key = pk(&mut realm.rt, "parentNode");
    assert_eq!(realm.rt.get(a, parent_node_key).unwrap(), Value::Null);
    assert_eq!(realm.rt.get(a, parent_element_key).unwrap(), Value::Null);

    // b should now have no element siblings.
    assert_eq!(
      realm.rt.get(b, previous_element_sibling_key).unwrap(),
      Value::Null
    );
    assert_eq!(realm.rt.get(b, next_element_sibling_key).unwrap(), Value::Null);
  }

  #[test]
  fn child_nodes_update_on_insert_before_replace_child_and_remove() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();

    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let span_tag = realm.rt.alloc_string_value("span").unwrap();

    let parent_a = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();
    let parent_b = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();
    let child = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();

    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_a = realm.rt.get(parent_a, append_child_key).unwrap();
    realm
      .rt
      .call_function(append_a, parent_a, &[child])
      .unwrap();

    // Cache childNodes for both parents so we can verify in-place updates.
    let child_nodes_key = pk(&mut realm.rt, "childNodes");
    let nodes_a = realm.rt.get(parent_a, child_nodes_key).unwrap();
    let nodes_b = realm.rt.get(parent_b, child_nodes_key).unwrap();
    assert_eq!(realm.rt.get(parent_a, child_nodes_key).unwrap(), nodes_a);
    assert_eq!(realm.rt.get(parent_b, child_nodes_key).unwrap(), nodes_b);

    let length_key = pk(&mut realm.rt, "length");
    let idx0 = pk(&mut realm.rt, "0");
    assert_eq!(realm.rt.get(nodes_a, length_key).unwrap(), Value::Number(1.0));
    assert_eq!(realm.rt.get(nodes_a, idx0).unwrap(), child);
    assert_eq!(realm.rt.get(nodes_b, length_key).unwrap(), Value::Number(0.0));

    // Move `child` from parent_a to parent_b via insertBefore(child, null).
    let insert_before_key = pk(&mut realm.rt, "insertBefore");
    let insert_before = realm.rt.get(parent_b, insert_before_key).unwrap();
    let inserted = realm
      .rt
      .call_function(insert_before, parent_b, &[child, Value::Null])
      .unwrap();
    assert_eq!(inserted, child);

    // Both cached NodeLists should update in place.
    assert_eq!(realm.rt.get(nodes_a, length_key).unwrap(), Value::Number(0.0));
    assert_eq!(realm.rt.get(nodes_b, length_key).unwrap(), Value::Number(1.0));
    assert_eq!(realm.rt.get(nodes_b, idx0).unwrap(), child);

    // Create a new node under parent_a and then move it into parent_b via replaceChild.
    let replacement = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();
    let append_a = realm.rt.get(parent_a, append_child_key).unwrap();
    realm
      .rt
      .call_function(append_a, parent_a, &[replacement])
      .unwrap();
    assert_eq!(realm.rt.get(nodes_a, length_key).unwrap(), Value::Number(1.0));
    assert_eq!(realm.rt.get(nodes_a, idx0).unwrap(), replacement);

    let replace_child_key = pk(&mut realm.rt, "replaceChild");
    let replace_child = realm.rt.get(parent_b, replace_child_key).unwrap();
    let replaced = realm
      .rt
      .call_function(replace_child, parent_b, &[replacement, child])
      .unwrap();
    assert_eq!(replaced, child);

    assert_eq!(realm.rt.get(nodes_a, length_key).unwrap(), Value::Number(0.0));
    assert_eq!(realm.rt.get(nodes_b, length_key).unwrap(), Value::Number(1.0));
    assert_eq!(realm.rt.get(nodes_b, idx0).unwrap(), replacement);

    // remove() should update the parent's cached NodeList as well.
    let remove_key = pk(&mut realm.rt, "remove");
    let remove = realm.rt.get(replacement, remove_key).unwrap();
    realm.rt.call_function(remove, replacement, &[]).unwrap();
    assert_eq!(realm.rt.get(nodes_b, length_key).unwrap(), Value::Number(0.0));
  }

  #[test]
  fn document_fragments_move_children_and_update_cached_child_nodes() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let document = realm.document();
    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let create_fragment_key = pk(&mut realm.rt, "createDocumentFragment");
    let create_fragment = realm.rt.get(document, create_fragment_key).unwrap();

    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let span_tag = realm.rt.alloc_string_value("span").unwrap();

    let parent = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();
    let a = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();
    let b = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();

    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(parent, append_child_key).unwrap();
    realm.rt.call_function(append_child, parent, &[a]).unwrap();
    realm.rt.call_function(append_child, parent, &[b]).unwrap();

    // childNodes should be cached and updated after insertBefore/replaceChild.
    let child_nodes_key = pk(&mut realm.rt, "childNodes");
    let nodes = realm.rt.get(parent, child_nodes_key).unwrap();
    let nodes2 = realm.rt.get(parent, child_nodes_key).unwrap();
    assert_eq!(nodes, nodes2);

    let length_key = pk(&mut realm.rt, "length");
    let idx0 = pk(&mut realm.rt, "0");
    let idx1 = pk(&mut realm.rt, "1");
    let idx2 = pk(&mut realm.rt, "2");
    assert_eq!(realm.rt.get(nodes, length_key).unwrap(), Value::Number(2.0));
    assert_eq!(realm.rt.get(nodes, idx0).unwrap(), a);
    assert_eq!(realm.rt.get(nodes, idx1).unwrap(), b);

    let insert_before_key = pk(&mut realm.rt, "insertBefore");
    let insert_before = realm.rt.get(parent, insert_before_key).unwrap();
    let c = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();
    realm
      .rt
      .call_function(insert_before, parent, &[c, b])
      .unwrap();

    // [a, c, b]
    assert_eq!(realm.rt.get(nodes, length_key).unwrap(), Value::Number(3.0));
    assert_eq!(realm.rt.get(nodes, idx0).unwrap(), a);
    assert_eq!(realm.rt.get(nodes, idx1).unwrap(), c);
    assert_eq!(realm.rt.get(nodes, idx2).unwrap(), b);

    let replace_child_key = pk(&mut realm.rt, "replaceChild");
    let replace_child = realm.rt.get(parent, replace_child_key).unwrap();
    let d = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();
    let replaced = realm
      .rt
      .call_function(replace_child, parent, &[d, c])
      .unwrap();
    assert_eq!(replaced, c);

    // [a, d, b]
    assert_eq!(realm.rt.get(nodes, length_key).unwrap(), Value::Number(3.0));
    assert_eq!(realm.rt.get(nodes, idx0).unwrap(), a);
    assert_eq!(realm.rt.get(nodes, idx1).unwrap(), d);
    assert_eq!(realm.rt.get(nodes, idx2).unwrap(), b);

    // DocumentFragment insertion should move its children and empty the fragment, updating cached
    // childNodes arrays for both the parent and the fragment.
    let frag = realm
      .rt
      .call_function(create_fragment, document, &[])
      .unwrap();
    let x = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();
    let y = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();
    let append_child_frag = realm.rt.get(frag, append_child_key).unwrap();
    realm.rt.call_function(append_child_frag, frag, &[x]).unwrap();
    realm.rt.call_function(append_child_frag, frag, &[y]).unwrap();

    let frag_nodes = realm.rt.get(frag, child_nodes_key).unwrap();
    assert_eq!(
      realm.rt.get(frag_nodes, length_key).unwrap(),
      Value::Number(2.0)
    );

    realm
      .rt
      .call_function(insert_before, parent, &[frag, b])
      .unwrap();

    // [a, d, x, y, b]
    let idx3 = pk(&mut realm.rt, "3");
    let idx4 = pk(&mut realm.rt, "4");
    assert_eq!(realm.rt.get(nodes, length_key).unwrap(), Value::Number(5.0));
    assert_eq!(realm.rt.get(nodes, idx0).unwrap(), a);
    assert_eq!(realm.rt.get(nodes, idx1).unwrap(), d);
    assert_eq!(realm.rt.get(nodes, idx2).unwrap(), x);
    assert_eq!(realm.rt.get(nodes, idx3).unwrap(), y);
    assert_eq!(realm.rt.get(nodes, idx4).unwrap(), b);

    let frag_nodes2 = realm.rt.get(frag, child_nodes_key).unwrap();
    assert_eq!(frag_nodes, frag_nodes2);
    assert_eq!(
      realm.rt.get(frag_nodes, length_key).unwrap(),
      Value::Number(0.0)
    );
  }

  #[test]
  fn document_create_event_and_init_event_populates_event_fields() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_event_key = pk(&mut realm.rt, "createEvent");
    let create_event = realm.rt.get(document, create_event_key).unwrap();
    let iface = realm.rt.alloc_string_value("Event").unwrap();
    let event = realm
      .rt
      .call_function(create_event, document, &[iface])
      .unwrap();

    let init_event_key = pk(&mut realm.rt, "initEvent");
    let init_event = realm.rt.get(event, init_event_key).unwrap();
    let type_x = realm.rt.alloc_string_value("x").unwrap();
    realm
      .rt
      .call_function(init_event, event, &[type_x, Value::Bool(true), Value::Bool(true)])
      .unwrap();

    let type_key = pk(&mut realm.rt, "type");
    let bubbles_key = pk(&mut realm.rt, "bubbles");
    let cancelable_key = pk(&mut realm.rt, "cancelable");

    let type_value = realm.rt.get(event, type_key).unwrap();
    assert_eq!(as_str(&realm.rt, type_value), "x");
    assert_eq!(realm.rt.get(event, bubbles_key).unwrap(), Value::Bool(true));
    assert_eq!(
      realm.rt.get(event, cancelable_key).unwrap(),
      Value::Bool(true)
    );
  }

  #[test]
  fn document_create_custom_event_and_init_custom_event_sets_detail() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_event_key = pk(&mut realm.rt, "createEvent");
    let create_event = realm.rt.get(document, create_event_key).unwrap();
    let iface = realm.rt.alloc_string_value("CustomEvent").unwrap();
    let event = realm
      .rt
      .call_function(create_event, document, &[iface])
      .unwrap();

    let init_custom_key = pk(&mut realm.rt, "initCustomEvent");
    let init_custom = realm.rt.get(event, init_custom_key).unwrap();
    let type_x = realm.rt.alloc_string_value("x").unwrap();
    realm
      .rt
      .call_function(
        init_custom,
        event,
        &[type_x, Value::Bool(true), Value::Bool(true), Value::Number(123.0)],
      )
      .unwrap();

    let detail_key = pk(&mut realm.rt, "detail");
    assert_eq!(realm.rt.get(event, detail_key).unwrap(), Value::Number(123.0));
  }

  #[test]
  fn custom_event_constructor_roots_object_detail_across_gc() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let ctor_key = pk(&mut realm.rt, "CustomEvent");
    let ctor = realm.rt.get(realm.window(), ctor_key).unwrap();
    let type_x = realm.rt.alloc_string_value("x").unwrap();

    // Basic `detail` propagation.
    let init_num = realm.rt.alloc_object_value().unwrap();
    define_data_property_str(&mut realm.rt, init_num, "detail", Value::Number(1.0), true).unwrap();
    let ev_num = realm
      .rt
      .call_function(ctor, Value::Undefined, &[type_x, init_num])
      .unwrap();
    let detail_key = pk(&mut realm.rt, "detail");
    assert_eq!(realm.rt.get(ev_num, detail_key).unwrap(), Value::Number(1.0));

    // Object `detail` should survive a GC cycle even if it is not referenced from JS, since the
    // platform-backed Event table is not traced.
    let detail_obj = realm.rt.alloc_object_value().unwrap();
    define_data_property_str(&mut realm.rt, detail_obj, "x", Value::Number(1.0), true).unwrap();
    let init_obj = realm.rt.alloc_object_value().unwrap();
    define_data_property_str(&mut realm.rt, init_obj, "detail", detail_obj, true).unwrap();

    let type_y = realm.rt.alloc_string_value("y").unwrap();
    let ev_obj = realm
      .rt
      .call_function(ctor, Value::Undefined, &[type_y, init_obj])
      .unwrap();

    realm.rt.heap_mut().collect_garbage();

    let detail_key = pk(&mut realm.rt, "detail");
    let detail = realm.rt.get(ev_obj, detail_key).unwrap();
    let Value::Object(obj) = detail else {
      panic!("expected object detail, got {detail:?}");
    };
    assert!(
      realm.rt.heap().is_valid_object(obj),
      "expected detail object to be kept alive by CustomEvent"
    );
    let x_key = pk(&mut realm.rt, "x");
    assert_eq!(realm.rt.get(detail, x_key).unwrap(), Value::Number(1.0));
  }

  #[test]
  fn document_create_event_unsupported_interface_throws_not_supported_error() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_event_key = pk(&mut realm.rt, "createEvent");
    let create_event = realm.rt.get(document, create_event_key).unwrap();
    let iface = realm.rt.alloc_string_value("Nope").unwrap();
    let err = realm
      .rt
      .call_function(create_event, document, &[iface])
      .expect_err("expected NotSupportedError");

    let Some(thrown) = err.thrown_value() else {
      panic!("expected VmError::Throw, got {err:?}");
    };
    let name_key = pk(&mut realm.rt, "name");
    let name = realm.rt.get(thrown, name_key).unwrap();
    assert_eq!(as_str(&realm.rt, name), "NotSupportedError");
  }

  #[test]
  fn event_target_dispatch_invokes_js_callback_with_current_target_this() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    // Create a target element and attach it to the document so events build a path.
    let document = realm.document();
    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let target = realm
      .rt
      .call_function(
        create_element,
        document,
        &[div_tag],
      )
      .unwrap();
    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(document, append_child_key).unwrap();
    realm
      .rt
      .call_function(append_child, document, &[target])
      .unwrap();

    // Create a cancelable, bubbling event.
    let event_key = pk(&mut realm.rt, "Event");
    let event_ctor = realm.rt.get(realm.window(), event_key).unwrap();
    let init = realm.rt.alloc_object_value().unwrap();
    define_data_property_str(&mut realm.rt, init, "bubbles", Value::Bool(true), true).unwrap();
    define_data_property_str(&mut realm.rt, init, "cancelable", Value::Bool(true), true).unwrap();
    let x_type = realm.rt.alloc_string_value("x").unwrap();
    let event = realm
      .rt
      .call_function(
        event_ctor,
        Value::Undefined,
        &[x_type, init],
      )
      .unwrap();

    // Record observed event phases for capture vs bubble listeners.
    let capture_phase_seen = Rc::new(Cell::new(0u32));
    let bubble_phase_seen = Rc::new(Cell::new(0u32));

    // Add a capturing listener on document (phase = Capturing).
    let capture_seen = capture_phase_seen.clone();
    let capture_cb = realm
      .rt
      .alloc_function_value(move |rt, this, args| {
        let event = args[0];
        let current_target_key = pk(rt, "currentTarget");
        let current_target = rt.get(event, current_target_key)?;
        assert_eq!(this, current_target, "this must equal event.currentTarget");
        let event_phase_key = pk(rt, "eventPhase");
        let phase = rt.get(event, event_phase_key)?;
        let Value::Number(n) = phase else {
          panic!("expected number eventPhase");
        };
        capture_seen.set(n as u32);
        Ok(Value::Undefined)
      })
      .unwrap();

    let add_key = pk(&mut realm.rt, "addEventListener");
    let add = realm.rt.get(document, add_key).unwrap();
    let x_type2 = realm.rt.alloc_string_value("x").unwrap();
    realm
      .rt
      .call_function(
        add,
        document,
        &[
          x_type2,
          capture_cb,
          Value::Bool(true),
        ],
      )
      .unwrap();

    // Add a bubbling listener on document that calls preventDefault (phase = Bubbling).
    let bubble_seen = bubble_phase_seen.clone();
    let bubble_cb = realm
      .rt
      .alloc_function_value(move |rt, this, args| {
        let event = args[0];
        let current_target_key = pk(rt, "currentTarget");
        let current_target = rt.get(event, current_target_key)?;
        assert_eq!(this, current_target, "this must equal event.currentTarget");
        let event_phase_key = pk(rt, "eventPhase");
        let phase = rt.get(event, event_phase_key)?;
        let Value::Number(n) = phase else {
          panic!("expected number eventPhase");
        };
        bubble_seen.set(n as u32);

        let prevent_default_key = pk(rt, "preventDefault");
        let prevent = rt.get(event, prevent_default_key)?;
        rt.call_function(prevent, event, &[])?;
        Ok(Value::Undefined)
      })
      .unwrap();
    let x_type3 = realm.rt.alloc_string_value("x").unwrap();
    realm
      .rt
      .call_function(
        add,
        document,
        &[
          x_type3,
          bubble_cb,
          Value::Undefined,
        ],
      )
      .unwrap();

    // Dispatch at the target.
    let dispatch_key = pk(&mut realm.rt, "dispatchEvent");
    let dispatch = realm.rt.get(target, dispatch_key).unwrap();
    let dispatched = realm
      .rt
      .call_function(dispatch, target, &[event])
      .unwrap();
    assert_eq!(
      dispatched,
      Value::Bool(false),
      "dispatchEvent should return false when canceled"
    );

    assert_eq!(capture_phase_seen.get(), 1, "capturing listener should observe phase=1");
    assert_eq!(bubble_phase_seen.get(), 3, "bubbling listener should observe phase=3");
  }

  #[test]
  fn event_target_dispatch_invokes_handle_event_object_with_object_this() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    // Create a target element and attach it to the document so events build a path.
    let document = realm.document();
    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let target = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();
    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(document, append_child_key).unwrap();
    realm
      .rt
      .call_function(append_child, document, &[target])
      .unwrap();

    // Create a callback object with a callable `handleEvent` method.
    let callback_obj = realm.rt.alloc_object_value().unwrap();
    let called = Rc::new(Cell::new(0u32));
    let called_for_cb = called.clone();
    let expected_this = callback_obj;
    let expected_target = target;
    let handle_event = realm
      .rt
      .alloc_function_value(move |rt, this, args| {
        assert_eq!(this, expected_this, "handleEvent must be invoked with this=callback object");
        assert_eq!(args.len(), 1, "handleEvent must receive the Event argument");
        let event = args[0];
        let current_target_key = pk(rt, "currentTarget");
        let current_target = rt.get(event, current_target_key)?;
        assert_eq!(
          current_target, expected_target,
          "event.currentTarget must be the dispatch currentTarget, not the listener object"
        );
        called_for_cb.set(called_for_cb.get() + 1);
        Ok(Value::Undefined)
      })
      .unwrap();
    define_data_property_str(&mut realm.rt, callback_obj, "handleEvent", handle_event, true).unwrap();

    let add_key = pk(&mut realm.rt, "addEventListener");
    let add = realm.rt.get(target, add_key).unwrap();
    let x_type = realm.rt.alloc_string_value("x").unwrap();
    realm
      .rt
      .call_function(add, target, &[x_type, callback_obj, Value::Undefined])
      .unwrap();

    let event_key = pk(&mut realm.rt, "Event");
    let event_ctor = realm.rt.get(realm.window(), event_key).unwrap();
    let x_type2 = realm.rt.alloc_string_value("x").unwrap();
    let event = realm
      .rt
      .call_function(event_ctor, Value::Undefined, &[x_type2])
      .unwrap();

    let dispatch_key = pk(&mut realm.rt, "dispatchEvent");
    let dispatch = realm.rt.get(target, dispatch_key).unwrap();
    let dispatched = realm
      .rt
      .call_function(dispatch, target, &[event])
      .unwrap();
    assert_eq!(dispatched, Value::Bool(true));
    assert_eq!(called.get(), 1);
  }

  #[test]
  fn once_listener_is_removed_and_callback_is_unrooted_after_dispatch() {
    use vm_js::WeakGcObject;

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    // Create a target node and attach it.
    let document = realm.document();
    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let target = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();
    // Keep the target wrapper alive across `collect_garbage()` below.
    let target_root = realm.rt.heap_mut().add_root(target).unwrap();

    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(document, append_child_key).unwrap();
    realm
      .rt
      .call_function(append_child, document, &[target])
      .unwrap();

    // Keep the target wrapper alive across the GC cycle below.
    //
    // Node wrappers are weakly cached, so they can be collected when there are no JS-visible
    // references even if the underlying `dom2` node is still connected to the document.
    let keep_alive_key = pk(&mut realm.rt, "__fastrender_test_keep_alive");
    realm
      .rt
      .define_data_property(realm.window(), keep_alive_key, target, /* enumerable */ true)
      .unwrap();

    let calls = Rc::new(Cell::new(0u32));
    let calls_for_cb = calls.clone();
    let cb = realm
      .rt
      .alloc_function_value(move |_rt, _this, _args| {
        calls_for_cb.set(calls_for_cb.get() + 1);
        Ok(Value::Undefined)
      })
      .unwrap();

    let Value::Object(cb_obj) = cb else {
      panic!("expected callback to be an object");
    };
    let cb_weak = WeakGcObject::from(cb_obj);

    // Add a once listener.
    let opts = realm.rt.alloc_object_value().unwrap();
    define_data_property_str(&mut realm.rt, opts, "once", Value::Bool(true), true).unwrap();
    let add_key = pk(&mut realm.rt, "addEventListener");
    let add = realm.rt.get(target, add_key).unwrap();
    let type_x = realm.rt.alloc_string_value("x").unwrap();
    realm
      .rt
      .call_function(add, target, &[type_x, cb, opts])
      .unwrap();
    assert_eq!(realm.listener_callbacks.borrow().len(), 1);

    // Create an Event and dispatch twice.
    let event_key = pk(&mut realm.rt, "Event");
    let event_ctor = realm.rt.get(realm.window(), event_key).unwrap();
    let type_x2 = realm.rt.alloc_string_value("x").unwrap();
    let event = realm
      .rt
      .call_function(event_ctor, Value::Undefined, &[type_x2])
      .unwrap();
    let event_root = realm.rt.heap_mut().add_root(event).unwrap();

    let dispatch_key = pk(&mut realm.rt, "dispatchEvent");
    let dispatch = realm.rt.get(target, dispatch_key).unwrap();
    let dispatched = realm
      .rt
      .call_function(dispatch, target, &[event])
      .unwrap();
    assert_eq!(dispatched, Value::Bool(true));
    assert_eq!(calls.get(), 1);

    // `once` should remove the listener registration, and the bindings should drop the rooted
    // callback entry.
    assert!(realm.listener_callbacks.borrow().is_empty());

    // The callback object should now be collectable since nothing else references it.
    realm.rt.heap_mut().collect_garbage();
    assert!(cb_weak.upgrade(realm.rt.heap()).is_none());

    // Dispatching again should not invoke the callback.
    let dispatched_again = realm
      .rt
      .call_function(dispatch, target, &[event])
      .unwrap();
    assert_eq!(dispatched_again, Value::Bool(true));
    assert_eq!(calls.get(), 1);
    realm.rt.heap_mut().remove_root(event_root);
    realm.rt.heap_mut().remove_root(target_root);
  }

  #[test]
  fn document_head_and_body_reflect_html_children() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(document, append_child_key).unwrap();

    let html_tag = realm.rt.alloc_string_value("html").unwrap();
    let html = realm
      .rt
      .call_function(create_element, document, &[html_tag])
      .unwrap();
    realm
      .rt
      .call_function(append_child, document, &[html])
      .unwrap();

    let head_tag = realm.rt.alloc_string_value("head").unwrap();
    let head = realm
      .rt
      .call_function(create_element, document, &[head_tag])
      .unwrap();
    let body_tag = realm.rt.alloc_string_value("body").unwrap();
    let body = realm
      .rt
      .call_function(create_element, document, &[body_tag])
      .unwrap();

    let html_append_child = realm.rt.get(html, append_child_key).unwrap();
    realm
      .rt
      .call_function(html_append_child, html, &[head])
      .unwrap();
    realm
      .rt
      .call_function(html_append_child, html, &[body])
      .unwrap();

    let head_key = pk(&mut realm.rt, "head");
    let got_head = realm.rt.get(document, head_key).unwrap();
    assert_eq!(got_head, head);

    let body_key = pk(&mut realm.rt, "body");
    let got_body = realm.rt.get(document, body_key).unwrap();
    assert_eq!(got_body, body);
  }

  #[test]
  fn node_text_content_get_and_set_mutates_dom2_tree() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let div_id = realm.dom.borrow_mut().create_element("div", "");
    let text_id = realm.dom.borrow_mut().create_text("hello");
    realm
      .dom
      .borrow_mut()
      .append_child(div_id, text_id)
      .unwrap();
    let div = realm.wrap_node(div_id).unwrap();

    let text_content_key = pk(&mut realm.rt, "textContent");
    let got = realm.rt.get(div, text_content_key).unwrap();
    assert_eq!(as_str(&realm.rt, got), "hello");

    let desc = realm
      .rt
      .get_own_property(realm.prototypes.node, text_content_key)
      .unwrap()
      .expect("expected Node.prototype.textContent");
    let set = match desc.kind {
      JsPropertyKind::Accessor { set, .. } => set,
      other => panic!("expected accessor property, got {other:?}"),
    };

    // Null clears children.
    realm.rt.call_function(set, div, &[Value::Null]).unwrap();
    assert_eq!(realm.dom.borrow().children(div_id).unwrap().len(), 0);
    let got = realm.rt.get(div, text_content_key).unwrap();
    assert_eq!(as_str(&realm.rt, got), "");

    // String replaces children with a single Text node.
    let x = realm.rt.alloc_string_value("x").unwrap();
    realm.rt.call_function(set, div, &[x]).unwrap();
    let dom_ref = realm.dom.borrow();
    let children = dom_ref.children(div_id).unwrap();
    assert_eq!(children.len(), 1);
    let child = children[0];
    assert_eq!(dom_ref.text_data(child).unwrap(), "x");
    let got = realm.rt.get(div, text_content_key).unwrap();
    assert_eq!(as_str(&realm.rt, got), "x");
  }

  #[test]
  fn node_clone_node_deep_clones_detached_subtree() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let create_text_key = pk(&mut realm.rt, "createTextNode");
    let create_text = realm.rt.get(document, create_text_key).unwrap();
    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(document, append_child_key).unwrap();

    // document.appendChild(<div id=src><span>hello</span></div>)
    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let div = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();

    let set_attribute_key = pk(&mut realm.rt, "setAttribute");
    let set_attribute = realm.rt.get(div, set_attribute_key).unwrap();
    let attr_id = realm.rt.alloc_string_value("id").unwrap();
    let id_src = realm.rt.alloc_string_value("src").unwrap();
    realm
      .rt
      .call_function(set_attribute, div, &[attr_id, id_src])
      .unwrap();

    realm.rt.call_function(append_child, document, &[div]).unwrap();

    let span_tag = realm.rt.alloc_string_value("span").unwrap();
    let span = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();
    let div_append = realm.rt.get(div, append_child_key).unwrap();
    realm.rt.call_function(div_append, div, &[span]).unwrap();

    let hello = realm.rt.alloc_string_value("hello").unwrap();
    let text = realm
      .rt
      .call_function(create_text, document, &[hello])
      .unwrap();
    let span_append = realm.rt.get(span, append_child_key).unwrap();
    realm.rt.call_function(span_append, span, &[text]).unwrap();

    // div.cloneNode(true) should deep clone but remain detached.
    let clone_node_key = pk(&mut realm.rt, "cloneNode");
    let clone_node = realm.rt.get(div, clone_node_key).unwrap();
    let clone = realm
      .rt
      .call_function(clone_node, div, &[Value::Bool(true)])
      .unwrap();
    assert_ne!(clone, div);

    let parent_node_key = pk(&mut realm.rt, "parentNode");
    assert_eq!(realm.rt.get(clone, parent_node_key).unwrap(), Value::Null);
    let is_connected_key = pk(&mut realm.rt, "isConnected");
    assert_eq!(
      realm.rt.get(clone, is_connected_key).unwrap(),
      Value::Bool(false)
    );

    let id_key = pk(&mut realm.rt, "id");
    let clone_id = realm.rt.get(clone, id_key).unwrap();
    assert_eq!(as_str(&realm.rt, clone_id), "src");

    let first_child_key = pk(&mut realm.rt, "firstChild");
    let clone_span = realm.rt.get(clone, first_child_key).unwrap();
    assert_ne!(clone_span, Value::Null);
    assert_ne!(clone_span, span);

    let clone_text = realm.rt.get(clone_span, first_child_key).unwrap();
    let node_value_key = pk(&mut realm.rt, "nodeValue");
    let clone_text_value = realm.rt.get(clone_text, node_value_key).unwrap();
    assert_eq!(as_str(&realm.rt, clone_text_value), "hello");

    // Shallow clone omits children.
    let shallow = realm.rt.call_function(clone_node, div, &[]).unwrap();
    assert_eq!(realm.rt.get(shallow, first_child_key).unwrap(), Value::Null);

    // Document.cloneNode is currently unsupported by dom2; surface NotSupportedError.
    let document_clone = realm.rt.get(document, clone_node_key).unwrap();
    let err = realm
      .rt
      .call_function(document_clone, document, &[])
      .expect_err("expected document.cloneNode to throw");
    let thrown = err.thrown_value().expect("expected thrown value");
    let name_key = pk(&mut realm.rt, "name");
    let name = realm.rt.get(thrown, name_key).unwrap();
    assert_eq!(as_str(&realm.rt, name), "NotSupportedError");
  }

  #[test]
  fn wrap_node_rejects_node_id_from_other_document() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let small_len = realm.dom.borrow().nodes_len();

    let mut other = dom2::Document::new(QuirksMode::NoQuirks);
    let mut node_id = other.root();
    while node_id.index() <= small_len + 4 {
      node_id = other.create_element("div", "");
    }

    let err = realm.wrap_node(node_id).unwrap_err();
    assert!(err.thrown_value().is_some());
  }

  #[test]
  fn node_type_and_name_reflect_dom2_node_kind() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let node_type_key = pk(&mut realm.rt, "nodeType");
    let node_name_key = pk(&mut realm.rt, "nodeName");

    assert_eq!(realm.rt.get(document, node_type_key).unwrap(), Value::Number(9.0));
    let doc_name = realm.rt.get(document, node_name_key).unwrap();
    assert_eq!(as_str(&realm.rt, doc_name), "#document");

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let div = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();
    assert_eq!(realm.rt.get(div, node_type_key).unwrap(), Value::Number(1.0));
    let div_name = realm.rt.get(div, node_name_key).unwrap();
    assert_eq!(as_str(&realm.rt, div_name), "DIV");

    let create_text_key = pk(&mut realm.rt, "createTextNode");
    let create_text = realm.rt.get(document, create_text_key).unwrap();
    let hello = realm.rt.alloc_string_value("hello").unwrap();
    let text = realm
      .rt
      .call_function(create_text, document, &[hello])
      .unwrap();
    assert_eq!(realm.rt.get(text, node_type_key).unwrap(), Value::Number(3.0));
    let text_name = realm.rt.get(text, node_name_key).unwrap();
    assert_eq!(as_str(&realm.rt, text_name), "#text");

    let create_fragment_key = pk(&mut realm.rt, "createDocumentFragment");
    let create_fragment = realm.rt.get(document, create_fragment_key).unwrap();
    let fragment = realm.rt.call_function(create_fragment, document, &[]).unwrap();
    assert_eq!(
      realm.rt.get(fragment, node_type_key).unwrap(),
      Value::Number(11.0)
    );
    let fragment_name = realm.rt.get(fragment, node_name_key).unwrap();
    assert_eq!(as_str(&realm.rt, fragment_name), "#document-fragment");
  }

  #[test]
  fn node_value_get_and_set_mutates_character_data() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_text_key = pk(&mut realm.rt, "createTextNode");
    let create_text = realm.rt.get(document, create_text_key).unwrap();
    let hello = realm.rt.alloc_string_value("hello").unwrap();
    let text = realm
      .rt
      .call_function(create_text, document, &[hello])
      .unwrap();

    let node_value_key = pk(&mut realm.rt, "nodeValue");
    let got = realm.rt.get(text, node_value_key).unwrap();
    assert_eq!(as_str(&realm.rt, got), "hello");

    let desc = realm
      .rt
      .get_own_property(realm.prototypes.node, node_value_key)
      .unwrap()
      .expect("expected Node.prototype.nodeValue");
    let set = match desc.kind {
      JsPropertyKind::Accessor { set, .. } => set,
      other => panic!("expected accessor property, got {other:?}"),
    };

    let world = realm.rt.alloc_string_value("world").unwrap();
    realm.rt.call_function(set, text, &[world]).unwrap();
    let got = realm.rt.get(text, node_value_key).unwrap();
    assert_eq!(as_str(&realm.rt, got), "world");

    let text_id = extract_node_id(&mut realm.rt, &realm.platform_objects, text).unwrap();
    assert_eq!(realm.dom.borrow().text_data(text_id).unwrap(), "world");

    // nodeValue on elements is null; setting it is a no-op.
    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let div = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();
    assert_eq!(realm.rt.get(div, node_value_key).unwrap(), Value::Null);
    realm.rt.call_function(set, div, &[world]).unwrap();
    assert_eq!(realm.rt.get(div, node_value_key).unwrap(), Value::Null);
  }

  #[test]
  fn element_tag_name_respects_namespace() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let div = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();

    let tag_name_key = pk(&mut realm.rt, "tagName");
    let got_tag = realm.rt.get(div, tag_name_key).unwrap();
    assert_eq!(as_str(&realm.rt, got_tag), "DIV");

    // Non-HTML namespaces should not be uppercased.
    let svg_id = realm.dom.borrow_mut().create_element("svg", crate::dom::SVG_NAMESPACE);
    let svg = realm.wrap_node(svg_id).unwrap();
    let got_tag = realm.rt.get(svg, tag_name_key).unwrap();
    assert_eq!(as_str(&realm.rt, got_tag), "svg");

    let node_name_key = pk(&mut realm.rt, "nodeName");
    let got_name = realm.rt.get(svg, node_name_key).unwrap();
    assert_eq!(as_str(&realm.rt, got_name), "svg");
  }

  #[test]
  fn element_inner_text_get_and_set_is_text_content_like_mvp() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let div_id = realm.dom.borrow_mut().create_element("div", "");
    let text_id = realm.dom.borrow_mut().create_text("hello");
    realm
      .dom
      .borrow_mut()
      .append_child(div_id, text_id)
      .unwrap();
    let div = realm.wrap_node(div_id).unwrap();

    let inner_text_key = pk(&mut realm.rt, "innerText");
    let got = realm.rt.get(div, inner_text_key).unwrap();
    assert_eq!(as_str(&realm.rt, got), "hello");

    let desc = realm
      .rt
      .get_own_property(realm.prototypes.element, inner_text_key)
      .unwrap()
      .expect("expected Element.prototype.innerText");
    let set = match desc.kind {
      JsPropertyKind::Accessor { set, .. } => set,
      other => panic!("expected accessor property, got {other:?}"),
    };

    // Null clears children.
    realm.rt.call_function(set, div, &[Value::Null]).unwrap();
    assert_eq!(realm.dom.borrow().children(div_id).unwrap().len(), 0);
    let got = realm.rt.get(div, inner_text_key).unwrap();
    assert_eq!(as_str(&realm.rt, got), "");

    // String replaces children with a single Text node.
    let x = realm.rt.alloc_string_value("x").unwrap();
    realm.rt.call_function(set, div, &[x]).unwrap();
    let dom_ref = realm.dom.borrow();
    let children = dom_ref.children(div_id).unwrap();
    assert_eq!(children.len(), 1);
    let child = children[0];
    assert_eq!(dom_ref.text_data(child).unwrap(), "x");
    let got = realm.rt.get(div, inner_text_key).unwrap();
    assert_eq!(as_str(&realm.rt, got), "x");
  }

  #[test]
  fn node_navigation_skips_inert_template_contents() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();

    let template_tag = realm.rt.alloc_string_value("template").unwrap();
    let template = realm
      .rt
      .call_function(create_element, document, &[template_tag])
      .unwrap();
    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let child = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();

    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(template, append_child_key).unwrap();
    realm
      .rt
      .call_function(append_child, template, &[child])
      .unwrap();

    // `<template>` behaves like it has no children (its contents subtree is inert).
    let first_child_key = pk(&mut realm.rt, "firstChild");
    assert_eq!(realm.rt.get(template, first_child_key).unwrap(), Value::Null);

    let child_nodes_key = pk(&mut realm.rt, "childNodes");
    let nodes = realm.rt.get(template, child_nodes_key).unwrap();
    let length_key = pk(&mut realm.rt, "length");
    assert_eq!(realm.rt.get(nodes, length_key).unwrap(), Value::Number(0.0));

    // Nodes inside the inert template subtree observe a null parent.
    let parent_node_key = pk(&mut realm.rt, "parentNode");
    assert_eq!(realm.rt.get(child, parent_node_key).unwrap(), Value::Null);

    // `contains()` must not walk across the inert template boundary.
    let contains_key = pk(&mut realm.rt, "contains");
    let contains = realm.rt.get(template, contains_key).unwrap();
    assert_eq!(
      realm.rt.call_function(contains, template, &[child]).unwrap(),
      Value::Bool(false)
    );
    let contains_doc = realm.rt.get(document, contains_key).unwrap();
    assert_eq!(
      realm.rt.call_function(contains_doc, document, &[child]).unwrap(),
      Value::Bool(false)
    );
  }

  #[test]
  fn node_owner_document_and_is_connected_reflect_dom2_tree() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let owner_document_key = pk(&mut realm.rt, "ownerDocument");
    let is_connected_key = pk(&mut realm.rt, "isConnected");

    // Document.ownerDocument is null, but the document itself is always connected.
    assert_eq!(realm.rt.get(document, owner_document_key).unwrap(), Value::Null);
    assert_eq!(realm.rt.get(document, is_connected_key).unwrap(), Value::Bool(true));

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let div = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();

    // Newly-created nodes are detached.
    assert_eq!(realm.rt.get(div, owner_document_key).unwrap(), document);
    assert_eq!(realm.rt.get(div, is_connected_key).unwrap(), Value::Bool(false));

    // Appending connects the subtree.
    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(document, append_child_key).unwrap();
    realm
      .rt
      .call_function(append_child, document, &[div])
      .unwrap();
    assert_eq!(realm.rt.get(div, is_connected_key).unwrap(), Value::Bool(true));
  }

  #[test]
  fn node_contains_reflects_dom2_tree() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let append_child_key = pk(&mut realm.rt, "appendChild");
    let contains_key = pk(&mut realm.rt, "contains");

    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let div = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();
    let span_tag = realm.rt.alloc_string_value("span").unwrap();
    let span = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();

    let doc_append_child = realm.rt.get(document, append_child_key).unwrap();
    realm
      .rt
      .call_function(doc_append_child, document, &[div])
      .unwrap();

    let div_append_child = realm.rt.get(div, append_child_key).unwrap();
    realm.rt.call_function(div_append_child, div, &[span]).unwrap();

    let div_contains = realm.rt.get(div, contains_key).unwrap();
    assert_eq!(
      realm.rt.call_function(div_contains, div, &[span]).unwrap(),
      Value::Bool(true)
    );
    // Inclusive.
    assert_eq!(
      realm.rt.call_function(div_contains, div, &[div]).unwrap(),
      Value::Bool(true)
    );
    // Missing arg is treated as null.
    assert_eq!(
      realm.rt.call_function(div_contains, div, &[]).unwrap(),
      Value::Bool(false)
    );
    assert_eq!(
      realm
        .rt
        .call_function(div_contains, div, &[Value::Null])
        .unwrap(),
      Value::Bool(false)
    );

    let span_contains = realm.rt.get(span, contains_key).unwrap();
    assert_eq!(
      realm.rt.call_function(span_contains, span, &[div]).unwrap(),
      Value::Bool(false)
    );

    let document_contains = realm.rt.get(document, contains_key).unwrap();
    assert_eq!(
      realm
        .rt
        .call_function(document_contains, document, &[span])
        .unwrap(),
      Value::Bool(true)
    );

    let err = realm
      .rt
      .call_function(div_contains, div, &[Value::Number(1.0)])
      .unwrap_err();
    assert!(err.thrown_value().is_some());
  }

  #[test]
  fn node_has_child_nodes_reflects_dom2_tree() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let append_child_key = pk(&mut realm.rt, "appendChild");
    let remove_child_key = pk(&mut realm.rt, "removeChild");
    let has_child_nodes_key = pk(&mut realm.rt, "hasChildNodes");

    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let div = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();
    let span_tag = realm.rt.alloc_string_value("span").unwrap();
    let span = realm
      .rt
      .call_function(create_element, document, &[span_tag])
      .unwrap();

    let div_has_child_nodes = realm.rt.get(div, has_child_nodes_key).unwrap();
    assert_eq!(
      realm.rt.call_function(div_has_child_nodes, div, &[]).unwrap(),
      Value::Bool(false)
    );

    let div_append_child = realm.rt.get(div, append_child_key).unwrap();
    realm
      .rt
      .call_function(div_append_child, div, &[span])
      .unwrap();

    assert_eq!(
      realm.rt.call_function(div_has_child_nodes, div, &[]).unwrap(),
      Value::Bool(true)
    );

    let div_remove_child = realm.rt.get(div, remove_child_key).unwrap();
    realm
      .rt
      .call_function(div_remove_child, div, &[span])
      .unwrap();
    assert_eq!(
      realm.rt.call_function(div_has_child_nodes, div, &[]).unwrap(),
      Value::Bool(false)
    );
  }

  #[test]
  fn append_child_with_document_fragment_inserts_children_and_empties_fragment() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let create_fragment_key = pk(&mut realm.rt, "createDocumentFragment");
    let create_fragment = realm.rt.get(document, create_fragment_key).unwrap();
    let append_child_key = pk(&mut realm.rt, "appendChild");

    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let parent = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();

    let fragment = realm.rt.call_function(create_fragment, document, &[]).unwrap();

    let a_tag = realm.rt.alloc_string_value("a").unwrap();
    let a = realm
      .rt
      .call_function(create_element, document, &[a_tag])
      .unwrap();
    let b_tag = realm.rt.alloc_string_value("b").unwrap();
    let b = realm
      .rt
      .call_function(create_element, document, &[b_tag])
      .unwrap();

    let fragment_append_child = realm.rt.get(fragment, append_child_key).unwrap();
    realm
      .rt
      .call_function(fragment_append_child, fragment, &[a])
      .unwrap();
    realm
      .rt
      .call_function(fragment_append_child, fragment, &[b])
      .unwrap();

    let parent_append_child = realm.rt.get(parent, append_child_key).unwrap();
    realm
      .rt
      .call_function(parent_append_child, parent, &[fragment])
      .unwrap();

    let parent_id = extract_node_id(&mut realm.rt, &realm.platform_objects, parent).unwrap();
    let fragment_id =
      extract_node_id(&mut realm.rt, &realm.platform_objects, fragment).unwrap();
    let a_id = extract_node_id(&mut realm.rt, &realm.platform_objects, a).unwrap();
    let b_id = extract_node_id(&mut realm.rt, &realm.platform_objects, b).unwrap();

    let dom_ref = realm.dom.borrow();
    assert_eq!(dom_ref.children(parent_id).unwrap(), &[a_id, b_id]);
    assert_eq!(dom_ref.children(fragment_id).unwrap(), &[]);
  }

  #[test]
  fn append_child_document_fragment_into_document_is_atomic() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let marker_value = realm.rt.alloc_string_value("marker").unwrap();
    let dom_exception_proto = realm.dom_exception_prototype();
    define_data_property_str(
      &mut realm.rt,
      dom_exception_proto,
      "__dom_exception_marker",
      marker_value,
      false,
    )
    .unwrap();

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();
    let create_fragment_key = pk(&mut realm.rt, "createDocumentFragment");
    let create_fragment = realm.rt.get(document, create_fragment_key).unwrap();
    let append_child_key = pk(&mut realm.rt, "appendChild");

    let fragment = realm.rt.call_function(create_fragment, document, &[]).unwrap();

    let a_tag = realm.rt.alloc_string_value("a").unwrap();
    let a = realm
      .rt
      .call_function(create_element, document, &[a_tag])
      .unwrap();
    let b_tag = realm.rt.alloc_string_value("b").unwrap();
    let b = realm
      .rt
      .call_function(create_element, document, &[b_tag])
      .unwrap();

    let fragment_append_child = realm.rt.get(fragment, append_child_key).unwrap();
    realm
      .rt
      .call_function(fragment_append_child, fragment, &[a])
      .unwrap();
    realm
      .rt
      .call_function(fragment_append_child, fragment, &[b])
      .unwrap();

    let document_append_child = realm.rt.get(document, append_child_key).unwrap();
    let err = realm
      .rt
      .call_function(document_append_child, document, &[fragment])
      .expect_err("expected HierarchyRequestError");
    let Some(thrown) = err.thrown_value() else {
      panic!("expected thrown error, got {err:?}");
    };

    let name_key = pk(&mut realm.rt, "name");
    let name = realm.rt.get(thrown, name_key).unwrap();
    assert_eq!(as_str(&realm.rt, name), "HierarchyRequestError");

    let marker_key = pk(&mut realm.rt, "__dom_exception_marker");
    let marker = realm.rt.get(thrown, marker_key).unwrap();
    assert_eq!(as_str(&realm.rt, marker), "marker");

    let document_id = extract_node_id(&mut realm.rt, &realm.platform_objects, document).unwrap();
    let fragment_id =
      extract_node_id(&mut realm.rt, &realm.platform_objects, fragment).unwrap();
    let a_id = extract_node_id(&mut realm.rt, &realm.platform_objects, a).unwrap();
    let b_id = extract_node_id(&mut realm.rt, &realm.platform_objects, b).unwrap();

    let dom_ref = realm.dom.borrow();
    assert_eq!(dom_ref.children(document_id).unwrap(), &[]);
    assert_eq!(dom_ref.children(fragment_id).unwrap(), &[a_id, b_id]);
    assert_eq!(dom_ref.parent(a_id).unwrap(), Some(fragment_id));
    assert_eq!(dom_ref.parent(b_id).unwrap(), Some(fragment_id));
  }

  #[test]
  fn node_is_connected_is_false_for_inert_template_contents() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();
    let document = realm.document();

    let create_element_key = pk(&mut realm.rt, "createElement");
    let create_element = realm.rt.get(document, create_element_key).unwrap();

    let template_tag = realm.rt.alloc_string_value("template").unwrap();
    let template = realm
      .rt
      .call_function(create_element, document, &[template_tag])
      .unwrap();
    let div_tag = realm.rt.alloc_string_value("div").unwrap();
    let child = realm
      .rt
      .call_function(create_element, document, &[div_tag])
      .unwrap();

    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(document, append_child_key).unwrap();
    realm
      .rt
      .call_function(append_child, template, &[child])
      .unwrap();

    let is_connected_key = pk(&mut realm.rt, "isConnected");
    assert_eq!(
      realm.rt.get(template, is_connected_key).unwrap(),
      Value::Bool(false)
    );
    assert_eq!(
      realm.rt.get(child, is_connected_key).unwrap(),
      Value::Bool(false)
    );

    // Connecting a `<template>` should not connect its inert contents subtree.
    realm
      .rt
      .call_function(append_child, document, &[template])
      .unwrap();
    assert_eq!(
      realm.rt.get(template, is_connected_key).unwrap(),
      Value::Bool(true)
    );
    assert_eq!(
      realm.rt.get(child, is_connected_key).unwrap(),
      Value::Bool(false)
    );
  }

  #[test]
  fn node_constructor_exposes_node_type_constants() {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut realm = DomJsRealm::new(dom).unwrap();

    let node_key = pk(&mut realm.rt, "Node");
    let node_ctor = realm.rt.get(realm.window(), node_key).unwrap();
    let element_node_key = pk(&mut realm.rt, "ELEMENT_NODE");
    let document_node_key = pk(&mut realm.rt, "DOCUMENT_NODE");
    assert_eq!(
      realm.rt.get(node_ctor, element_node_key).unwrap(),
      Value::Number(1.0)
    );
    assert_eq!(
      realm.rt.get(node_ctor, document_node_key).unwrap(),
      Value::Number(9.0)
    );
  }
}
