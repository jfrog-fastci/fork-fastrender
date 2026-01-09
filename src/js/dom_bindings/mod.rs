//! DOM core WebIDL-shaped JS bindings (MVP).
//!
//! This module builds a small DOM bindings layer on top of:
//! - [`crate::dom2::Document`] for the mutable DOM tree
//! - [`webidl_js_runtime::VmJsRuntime`] for a minimal JS value + object model
//!
//! The design is intentionally "spec-shaped": prototypes + constructors exist, methods are defined
//! on prototypes, and the host keeps platform object state in Rust-side tables (not JS-visible
//! internal slots).

use crate::dom2::{self, NodeId, NodeKind};
use crate::js::orchestrator::CurrentScriptState;
use crate::web::events;
use rustc_hash::FxHashMap;
use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;
use vm_js::{GcObject, PropertyKey, RootId, Value, VmError};
use webidl_js_runtime::{JsRuntime as _, VmJsRuntime, WebIdlJsRuntime as _};

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

  platform_objects: Rc<RefCell<FxHashMap<GcObject, PlatformObjectKind>>>,
  node_wrapper_cache: Rc<RefCell<FxHashMap<NodeId, GcObject>>>,

  event_listeners: Rc<events::EventListenerRegistry>,
  listener_callbacks: Rc<RefCell<FxHashMap<events::ListenerId, ListenerEntry>>>,

  next_event_id: Rc<Cell<u64>>,
  events: Rc<RefCell<FxHashMap<u64, events::Event>>>,
  active_events: ActiveEventMap,

  prototypes: Prototypes,
}

impl DomJsRealm {
  pub fn new(dom: dom2::Document) -> Result<Self, VmError> {
    let dom = Rc::new(RefCell::new(dom));
    let document_node_id = dom.borrow().root();

    let current_script_state = Rc::new(RefCell::new(CurrentScriptState::default()));
    let platform_objects: Rc<RefCell<FxHashMap<GcObject, PlatformObjectKind>>> =
      Rc::new(RefCell::new(FxHashMap::default()));
    let node_wrapper_cache: Rc<RefCell<FxHashMap<NodeId, GcObject>>> =
      Rc::new(RefCell::new(FxHashMap::default()));

    let event_listeners = Rc::new(events::EventListenerRegistry::new());
    let listener_callbacks: Rc<RefCell<FxHashMap<events::ListenerId, ListenerEntry>>> =
      Rc::new(RefCell::new(FxHashMap::default()));
    let next_event_id: Rc<Cell<u64>> = Rc::new(Cell::new(1));
    let events_map: Rc<RefCell<FxHashMap<u64, events::Event>>> =
      Rc::new(RefCell::new(FxHashMap::default()));
    let active_events: ActiveEventMap = Rc::new(RefCell::new(FxHashMap::default()));

    let mut rt = VmJsRuntime::new();

    // Global object: we treat this as Window for MVP.
    let window = rt.alloc_object_value()?;
    let _ = rt.heap_mut().add_root(window)?;
    let Value::Object(window_obj) = window else {
      unreachable!("alloc_object_value must return an object");
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

    rt.set_prototype(object_proto, None)?;
    rt.set_prototype(event_target_proto, Some(object_proto))?;
    rt.set_prototype(node_proto, Some(event_target_proto))?;
    rt.set_prototype(element_proto, Some(node_proto))?;
    rt.set_prototype(document_proto, Some(node_proto))?;
    rt.set_prototype(event_proto, Some(object_proto))?;

    let prototypes = Prototypes {
      object: object_proto,
      event_target: event_target_proto,
      node: node_proto,
      element: element_proto,
      document: document_proto,
      event: event_proto,
    };

    // Window is an EventTarget.
    rt.set_prototype(window, Some(event_target_proto))?;

    // Document instance.
    let document = rt.alloc_object_value()?;
    let _ = rt.heap_mut().add_root(document)?;
    rt.set_prototype(document, Some(document_proto))?;
    let Value::Object(document_obj) = document else {
      unreachable!("alloc_object_value must return an object");
    };

    // Platform-object bookkeeping (brand checks + identity).
    platform_objects
      .borrow_mut()
      .insert(window_obj, PlatformObjectKind::Window);
    platform_objects.borrow_mut().insert(
      document_obj,
      PlatformObjectKind::Document {
        node_id: document_node_id,
      },
    );
    node_wrapper_cache
      .borrow_mut()
      .insert(document_node_id, document_obj);

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
      active_events.clone(),
      dom.clone(),
      node_wrapper_cache.clone(),
      current_script_state.clone(),
    )?;

    Ok(Self {
      rt,
      window,
      document,
      document_node_id,
      dom,
      current_script_state,
      platform_objects,
      node_wrapper_cache,
      event_listeners,
      listener_callbacks,
      next_event_id,
      events: events_map,
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

  pub fn dom(&self) -> Rc<RefCell<dom2::Document>> {
    self.dom.clone()
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
}

fn prop_key_str(rt: &mut VmJsRuntime, name: &str) -> Result<PropertyKey, VmError> {
  let Value::String(s) = rt.alloc_string_value(name)? else {
    unreachable!("alloc_string_value must return a string");
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
  let Value::String(s) = v else {
    unreachable!("ToString must return a string");
  };
  Ok(rt.heap().get_string(s)?.to_utf8_lossy())
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

fn listener_id_from_callback(rt: &mut VmJsRuntime, callback: Value) -> Result<Option<events::ListenerId>, VmError> {
  if matches!(callback, Value::Undefined | Value::Null) {
    return Ok(None);
  }
  if !rt.is_callable(callback) {
    return Err(rt.throw_type_error("EventTarget listener callback is not callable"));
  }
  let Value::Object(obj) = callback else {
    return Err(rt.throw_type_error("EventTarget listener callback is not an object"));
  };

  let id = (obj.id().index() as u64) | ((obj.id().generation() as u64) << 32);
  Ok(Some(events::ListenerId::new(id)))
}

fn extract_event_target_id(
  rt: &mut VmJsRuntime,
  platform_objects: &Rc<RefCell<FxHashMap<GcObject, PlatformObjectKind>>>,
  this: Value,
) -> Result<events::EventTargetId, VmError> {
  let Value::Object(obj) = this else {
    return Err(rt.throw_type_error("Illegal invocation"));
  };
  let map = platform_objects.borrow();
  match map.get(&obj) {
    Some(PlatformObjectKind::Window) => Ok(events::EventTargetId::Window),
    Some(PlatformObjectKind::Document { .. }) => Ok(events::EventTargetId::Document),
    Some(PlatformObjectKind::Node { node_id }) => Ok(events::EventTargetId::Node(*node_id)),
    _ => Err(rt.throw_type_error("Illegal invocation")),
  }
}

fn extract_node_id(
  rt: &mut VmJsRuntime,
  platform_objects: &Rc<RefCell<FxHashMap<GcObject, PlatformObjectKind>>>,
  this: Value,
) -> Result<NodeId, VmError> {
  let Value::Object(obj) = this else {
    return Err(rt.throw_type_error("Illegal invocation"));
  };
  let map = platform_objects.borrow();
  match map.get(&obj) {
    Some(PlatformObjectKind::Document { node_id }) => Ok(*node_id),
    Some(PlatformObjectKind::Node { node_id }) => Ok(*node_id),
    _ => Err(rt.throw_type_error("Illegal invocation")),
  }
}

fn extract_document_id(
  rt: &mut VmJsRuntime,
  platform_objects: &Rc<RefCell<FxHashMap<GcObject, PlatformObjectKind>>>,
  this: Value,
) -> Result<NodeId, VmError> {
  let Value::Object(obj) = this else {
    return Err(rt.throw_type_error("Illegal invocation"));
  };
  let map = platform_objects.borrow();
  match map.get(&obj) {
    Some(PlatformObjectKind::Document { node_id }) => Ok(*node_id),
    _ => Err(rt.throw_type_error("Illegal invocation")),
  }
}

fn extract_event_id(
  rt: &mut VmJsRuntime,
  platform_objects: &Rc<RefCell<FxHashMap<GcObject, PlatformObjectKind>>>,
  this: Value,
) -> Result<u64, VmError> {
  let Value::Object(obj) = this else {
    return Err(rt.throw_type_error("Illegal invocation"));
  };
  let map = platform_objects.borrow();
  match map.get(&obj) {
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
    let event = unsafe { ptr.as_ptr().as_mut().expect("NonNull is never null") };
    return Ok(f(event));
  }

  let mut map = events_map.borrow_mut();
  let event = map
    .get_mut(&event_id)
    .ok_or_else(|| rt.throw_type_error("Event is no longer active"))?;
  Ok(f(event))
}

fn wrap_event_target(
  rt: &mut VmJsRuntime,
  dom: &Rc<RefCell<dom2::Document>>,
  platform_objects: &Rc<RefCell<FxHashMap<GcObject, PlatformObjectKind>>>,
  node_wrapper_cache: &Rc<RefCell<FxHashMap<NodeId, GcObject>>>,
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
  platform_objects: &Rc<RefCell<FxHashMap<GcObject, PlatformObjectKind>>>,
  node_wrapper_cache: &Rc<RefCell<FxHashMap<NodeId, GcObject>>>,
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

  if let Some(existing) = node_wrapper_cache.borrow().get(&node_id).copied() {
    return Ok(Value::Object(existing));
  }

  let proto = {
    let dom = dom.borrow();
    match &dom.node(node_id).kind {
      NodeKind::Element { .. } | NodeKind::Slot { .. } => prototypes.element,
      _ => prototypes.node,
    }
  };

  let obj = rt.alloc_object_value()?;
  // Node wrappers are cached for stable identity and must remain valid across GC cycles.
  let _ = rt.heap_mut().add_root(obj)?;
  rt.set_prototype(obj, Some(proto))?;
  let Value::Object(obj_handle) = obj else {
    unreachable!("alloc_object_value must return an object");
  };

  node_wrapper_cache.borrow_mut().insert(node_id, obj_handle);
  platform_objects.borrow_mut().insert(
    obj_handle,
    PlatformObjectKind::Node { node_id },
  );

  Ok(obj)
}

fn install_constructors(
  rt: &mut VmJsRuntime,
  global: Value,
  prototypes: Prototypes,
  platform_objects: Rc<RefCell<FxHashMap<GcObject, PlatformObjectKind>>>,
  event_listeners: Rc<events::EventListenerRegistry>,
  listener_callbacks: Rc<RefCell<FxHashMap<events::ListenerId, ListenerEntry>>>,
  next_event_id: Rc<Cell<u64>>,
  events_map: Rc<RefCell<FxHashMap<u64, events::Event>>>,
  active_events: ActiveEventMap,
  dom: Rc<RefCell<dom2::Document>>,
  node_wrapper_cache: Rc<RefCell<FxHashMap<NodeId, GcObject>>>,
  current_script_state: Rc<RefCell<CurrentScriptState>>,
) -> Result<(), VmError> {
  fn illegal_constructor(rt: &mut VmJsRuntime, name: &'static str) -> Result<Value, VmError> {
    Err(rt.throw_type_error(&format!("{name} is not a constructor")))
  }

  let window = global;
  let document_key = prop_key_str(rt, "document")?;
  let document = rt.get(global, document_key)?;
  let document_node_id = dom.borrow().root();

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
        unreachable!("alloc_object_value must return an object");
      };
      platform_objects
        .borrow_mut()
        .insert(obj_handle, PlatformObjectKind::Event { event_id });
      Ok(obj)
    })?
  };
  define_data_property_str(rt, event_ctor, "prototype", event_proto, false)?;
  define_data_property_str(rt, global, "Event", event_ctor, false)?;

  // EventTarget.prototype
  {
    let platform_objects_for_add = platform_objects.clone();
    let event_listeners_for_add = event_listeners.clone();
    let listener_callbacks_for_add = listener_callbacks.clone();
    let add = rt.alloc_function_value(move |rt, this, args| {
      let target = extract_event_target_id(rt, &platform_objects_for_add, this)?;
      let type_ = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;

      let callback = args.get(1).copied().unwrap_or(Value::Undefined);
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
        platform_objects: Rc<RefCell<FxHashMap<GcObject, PlatformObjectKind>>>,
        node_wrapper_cache: Rc<RefCell<FxHashMap<NodeId, GcObject>>>,
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

          rt
            .call_function(callback, current_target_wrapper, &[self.event_value])
            .map_err(|e| events::DomError::new(format!("{e:?}")))?;
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

  // Node.prototype
  {
    // appendChild
    let dom_for_append = dom.clone();
    let platform_objects_for_append = platform_objects.clone();
    let append_child = rt.alloc_function_value(move |rt, this, args| {
      let parent_id = extract_node_id(rt, &platform_objects_for_append, this)?;
      let child = args
        .get(0)
        .copied()
        .ok_or_else(|| rt.throw_type_error("appendChild: missing child"))?;
      let child_id = extract_node_id(rt, &platform_objects_for_append, child)?;
      dom_for_append
        .borrow_mut()
        .append_child(parent_id, child_id)
        .map_err(|e| rt.throw_type_error(&format!("appendChild: {e}")))?;
      Ok(child)
    })?;
    define_method(rt, prototypes.node, "appendChild", append_child)?;

    // removeChild
    let dom_for_remove = dom.clone();
    let platform_objects_for_remove = platform_objects.clone();
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
        .map_err(|e| rt.throw_type_error(&format!("removeChild: {e}")))?;
      Ok(child)
    })?;
    define_method(rt, prototypes.node, "removeChild", remove_child)?;

    // parentNode
    let dom_for_parent = dom.clone();
    let platform_objects_for_parent = platform_objects.clone();
    let node_wrapper_cache_for_parent = node_wrapper_cache.clone();
    let parent_node_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_parent, this)?;
      let parent = dom_for_parent.borrow().parent_node(node_id);
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

    // firstChild
    let dom_for_first = dom.clone();
    let platform_objects_for_first = platform_objects.clone();
    let node_wrapper_cache_for_first = node_wrapper_cache.clone();
    let first_child_get = rt.alloc_function_value(move |rt, this, _args| {
      let node_id = extract_node_id(rt, &platform_objects_for_first, this)?;
      match dom_for_first.borrow().first_child(node_id) {
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
      match dom_for_last.borrow().last_child(node_id) {
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
      match dom_for_prev.borrow().previous_sibling(node_id) {
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
      match dom_for_next.borrow().next_sibling(node_id) {
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
        NodeKind::ShadowRoot { .. } => 11,
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
        NodeKind::Element { tag_name, .. } => tag_name.to_ascii_uppercase(),
        NodeKind::Slot { .. } => "SLOT".to_string(),
        NodeKind::Text { .. } => "#text".to_string(),
        NodeKind::Comment { .. } => "#comment".to_string(),
        NodeKind::ProcessingInstruction { target, .. } => target.clone(),
        NodeKind::ShadowRoot { .. } => "#document-fragment".to_string(),
      };
      rt.alloc_string_value(&name)
    })?;
    define_accessor(rt, prototypes.node, "nodeName", node_name_get, Value::Undefined)?;

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
    let text_content_set = rt.alloc_function_value(move |rt, this, args| {
      let node_id = extract_node_id(rt, &platform_objects_for_text_content_set, this)?;
      let v = args.get(0).copied().unwrap_or(Value::Undefined);
      let text = if matches!(v, Value::Null) {
        String::new()
      } else {
        to_rust_string(rt, v)?
      };
      set_text_content(&mut dom_for_text_content_set.borrow_mut(), node_id, &text)
        .map_err(|e| rt.throw_type_error(&format!("textContent: {e}")))?;
      Ok(Value::Undefined)
    })?;

    define_accessor(rt, prototypes.node, "textContent", text_content_get, text_content_set)?;
  }

  // Element.prototype
  {
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
        .map_err(|e| rt.throw_type_error(&format!("getAttribute: {e}")))? {
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
        .map_err(|e| rt.throw_type_error(&format!("setAttribute: {e}")))?;
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
        .map_err(|e| rt.throw_type_error(&format!("removeAttribute: {e}")))?;
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
        .map_err(|e| rt.throw_type_error(&format!("id: {e}")))?
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
        .map_err(|e| rt.throw_type_error(&format!("id: {e}")))?;
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
        .map_err(|e| rt.throw_type_error(&format!("className: {e}")))?
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
        .map_err(|e| rt.throw_type_error(&format!("className: {e}")))?;
      Ok(Value::Undefined)
    })?;
    define_accessor(rt, prototypes.element, "className", class_get, class_set)?;
  }

  // Document.prototype
  {
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
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use selectors::context::QuirksMode;
  use std::cell::Cell;
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

    let append_child_key = pk(&mut realm.rt, "appendChild");
    let append_child = realm.rt.get(document, append_child_key).unwrap();
    realm
      .rt
      .call_function(append_child, document, &[target])
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
    assert!(matches!(err, VmError::Throw(_)));
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
  }
}
