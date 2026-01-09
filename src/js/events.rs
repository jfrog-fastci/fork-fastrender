use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::rc::Rc;

use vm_js::{PropertyKey, RootId, Value, VmError};

use crate::dom2;
use crate::error::{Error, Result};
use crate::js::webidl::{JsRuntime as WebIdlJsRuntime, VmJsRuntime, WebIdlJsRuntime as WebIdlHooks};
use crate::web::events::{
  dispatch_event, AddEventListenerOptions, DomError as EventsDomError, Event, EventListenerInvoker,
  EventListenerRegistry, EventPhase, EventTargetId, ListenerId,
};

/// A JS function value that can be registered as a DOM event listener.
///
/// For now this is the `vm-js` value type. Higher-level Web IDL bindings can treat this as an
/// opaque handle.
pub type JsFunctionHandle = Value;

#[derive(Debug, Clone, Copy)]
struct ListenerEntry {
  callback: Value,
  callback_root: RootId,
}

type ActiveEventMap = Rc<RefCell<HashMap<u64, NonNull<Event>>>>;

#[derive(Debug, Clone, Copy)]
struct EventKeys {
  event_id: PropertyKey,
  type_: PropertyKey,
  target: PropertyKey,
  current_target: PropertyKey,
  event_phase: PropertyKey,
  default_prevented: PropertyKey,
  stop_propagation: PropertyKey,
  stop_immediate_propagation: PropertyKey,
  prevent_default: PropertyKey,
}

struct EventWrapper {
  prototype: Value,
  keys: EventKeys,
  active_events: ActiveEventMap,
  next_event_id: u64,
}

impl EventWrapper {
  fn intern_key(rt: &mut VmJsRuntime, name: &str) -> std::result::Result<PropertyKey, VmError> {
    rt.property_key_from_str(name)
  }

  fn new(rt: &mut VmJsRuntime) -> std::result::Result<Self, VmError> {
    let active_events: ActiveEventMap = Rc::new(RefCell::new(HashMap::new()));

    let keys = EventKeys {
      // Intentionally internal-ish; not a web-exposed slot. Used to find the Rust `Event` during
      // dispatch.
      event_id: Self::intern_key(rt, "__fastrender_event_id")?,
      type_: Self::intern_key(rt, "type")?,
      target: Self::intern_key(rt, "target")?,
      current_target: Self::intern_key(rt, "currentTarget")?,
      event_phase: Self::intern_key(rt, "eventPhase")?,
      default_prevented: Self::intern_key(rt, "defaultPrevented")?,
      stop_propagation: Self::intern_key(rt, "stopPropagation")?,
      stop_immediate_propagation: Self::intern_key(rt, "stopImmediatePropagation")?,
      prevent_default: Self::intern_key(rt, "preventDefault")?,
    };

    // Prototype with methods/getters that mutate the active Rust `Event`.
    let prototype = rt.alloc_object_value()?;

    let stop_propagation = {
      let active = active_events.clone();
      let key_event_id = keys.event_id;
      rt.alloc_function_value(move |rt, this, _args| {
        let id = get_event_id(rt, this, key_event_id)?;
        with_active_event(rt, &active, id, |event| event.stop_propagation())?;
        Ok(Value::Undefined)
      })?
    };
    rt.define_data_property(prototype, keys.stop_propagation, stop_propagation, false)?;

    let stop_immediate = {
      let active = active_events.clone();
      let key_event_id = keys.event_id;
      rt.alloc_function_value(move |rt, this, _args| {
        let id = get_event_id(rt, this, key_event_id)?;
        with_active_event(rt, &active, id, |event| event.stop_immediate_propagation())?;
        Ok(Value::Undefined)
      })?
    };
    rt.define_data_property(
      prototype,
      keys.stop_immediate_propagation,
      stop_immediate,
      false,
    )?;

    let prevent_default = {
      let active = active_events.clone();
      let key_event_id = keys.event_id;
      rt.alloc_function_value(move |rt, this, _args| {
        let id = get_event_id(rt, this, key_event_id)?;
        with_active_event(rt, &active, id, |event| event.prevent_default())?;
        Ok(Value::Undefined)
      })?
    };
    rt.define_data_property(prototype, keys.prevent_default, prevent_default, false)?;

    let default_prevented_getter = {
      let active = active_events.clone();
      let key_event_id = keys.event_id;
      rt.alloc_function_value(move |rt, this, _args| {
        let id = get_event_id(rt, this, key_event_id)?;
        let flag = with_active_event_ret(rt, &active, id, |event| event.default_prevented)?;
        Ok(Value::Bool(flag))
      })?
    };
    rt.define_accessor_property(
      prototype,
      keys.default_prevented,
      default_prevented_getter,
      Value::Undefined,
      false,
    )?;

    Ok(Self {
      prototype,
      keys,
      active_events,
      next_event_id: 1,
    })
  }

  fn alloc_event_id(&mut self) -> u64 {
    let id = self.next_event_id;
    self.next_event_id = self.next_event_id.wrapping_add(1);
    id
  }

  fn wrap_event(
    &mut self,
    rt: &mut VmJsRuntime,
    event_id: u64,
    event: &Event,
  ) -> std::result::Result<Value, VmError> {
    let obj = rt.alloc_object_value()?;
    rt.set_prototype(obj, Some(self.prototype))?;

    rt.define_data_property(obj, self.keys.event_id, Value::Number(event_id as f64), false)?;

    let type_ = rt.alloc_string_value(&event.type_)?;
    rt.define_data_property(obj, self.keys.type_, type_, true)?;

    let target = js_value_for_target(rt, event.target)?;
    rt.define_data_property(obj, self.keys.target, target, true)?;

    let current_target = js_value_for_target(rt, event.current_target)?;
    rt.define_data_property(obj, self.keys.current_target, current_target, true)?;

    rt.define_data_property(
      obj,
      self.keys.event_phase,
      js_value_for_phase(event.event_phase),
      true,
    )?;

    Ok(obj)
  }
}

fn js_value_for_phase(phase: EventPhase) -> Value {
  // Mirror the DOM `Event.eventPhase` numeric values.
  // https://dom.spec.whatwg.org/#dom-event-eventphase
  Value::Number(match phase {
    EventPhase::None => 0.0,
    EventPhase::Capturing => 1.0,
    EventPhase::AtTarget => 2.0,
    EventPhase::Bubbling => 3.0,
  })
}

fn js_value_for_target(
  rt: &mut VmJsRuntime,
  target: Option<EventTargetId>,
) -> std::result::Result<Value, VmError> {
  Ok(match target {
    None => Value::Null,
    Some(EventTargetId::Window) => rt.alloc_string_value("window")?,
    Some(EventTargetId::Document) => rt.alloc_string_value("document")?,
    Some(EventTargetId::Node(node_id)) => Value::Number(node_id.index() as f64),
  })
}

fn get_event_id(
  rt: &mut VmJsRuntime,
  this: Value,
  key_event_id: PropertyKey,
) -> std::result::Result<u64, VmError> {
  if !rt.is_object(this) {
    return Err(rt.throw_type_error("Event method called on non-object receiver"));
  }
  let id_val = rt.get(this, key_event_id)?;
  match id_val {
    Value::Number(n) if n.is_finite() => Ok(n as u64),
    _ => Err(rt.throw_type_error("Event object is missing internal event id")),
  }
}

fn with_active_event(
  rt: &mut VmJsRuntime,
  active: &ActiveEventMap,
  id: u64,
  f: impl FnOnce(&mut Event),
) -> std::result::Result<(), VmError> {
  let ptr = { active.borrow().get(&id).copied() };
  let Some(mut ptr) = ptr else {
    return Err(rt.throw_type_error("Event is no longer active"));
  };
  // Safety: the pointer is installed by the dispatch invoker for the duration of a listener call.
  unsafe {
    f(ptr.as_mut());
  }
  Ok(())
}

fn with_active_event_ret<R: Copy>(
  rt: &mut VmJsRuntime,
  active: &ActiveEventMap,
  id: u64,
  f: impl FnOnce(&mut Event) -> R,
) -> std::result::Result<R, VmError> {
  let mut out: Option<R> = None;
  with_active_event(rt, active, id, |event| out = Some(f(event)))?;
  out.ok_or_else(|| rt.throw_type_error("Event is no longer active"))
}

struct ActiveEventGuard {
  active: ActiveEventMap,
  id: u64,
}

impl Drop for ActiveEventGuard {
  fn drop(&mut self) {
    self.active.borrow_mut().remove(&self.id);
  }
}

/// JS-facing DOM Events registry + invoker.
///
/// This is a thin adapter:
/// - `web::events::EventListenerRegistry` stores the spec-shaped listener list keyed by [`ListenerId`].
/// - `JsDomEvents` stores the associated JS callback functions and can invoke them.
pub struct JsDomEvents {
  runtime: VmJsRuntime,
  registry: Rc<EventListenerRegistry>,
  next_listener_id: u64,
  listeners: HashMap<ListenerId, ListenerEntry>,
  event_wrapper: EventWrapper,
}

impl JsDomEvents {
  pub fn new() -> Result<Self> {
    let mut runtime = VmJsRuntime::new();
    let event_wrapper = EventWrapper::new(&mut runtime).map_err(|e| Error::Other(e.to_string()))?;
    Ok(Self {
      runtime,
      registry: Rc::new(EventListenerRegistry::new()),
      next_listener_id: 1,
      listeners: HashMap::new(),
      event_wrapper,
    })
  }

  pub fn runtime_mut(&mut self) -> &mut VmJsRuntime {
    &mut self.runtime
  }

  /// Register a JS listener callback.
  pub fn add_js_event_listener(
    &mut self,
    target: EventTargetId,
    type_: &str,
    callback: JsFunctionHandle,
    options: AddEventListenerOptions,
  ) -> ListenerId {
    let id = self.listener_id_for_callback(callback);

    if self.registry.add_event_listener(target, type_, id, options) {
      self.ensure_listener_entry(id, callback);
    }

    id
  }

  pub fn remove_js_event_listener(
    &mut self,
    target: EventTargetId,
    type_: &str,
    listener_id: ListenerId,
    capture: bool,
  ) -> bool {
    let removed = self
      .registry
      .remove_event_listener(target, type_, listener_id, capture);
    if removed {
      self.remove_listener_if_unused(listener_id);
    }
    removed
  }

  pub fn dispatch_dom_event(
    &mut self,
    dom: &dom2::Document,
    target: EventTargetId,
    event: &mut Event,
  ) -> Result<bool> {
    let registry = Rc::clone(&self.registry);
    dispatch_event(target, event, dom, registry.as_ref(), self)
      .map_err(|e| Error::Other(e.to_string()))
  }

  fn listener_id_for_callback(&mut self, callback: Value) -> ListenerId {
    if let Value::Object(obj) = callback {
      let id = obj.id();
      return ListenerId::new((id.index() as u64) | ((id.generation() as u64) << 32));
    }

    let id = ListenerId::new(self.next_listener_id);
    self.next_listener_id = self.next_listener_id.wrapping_add(1);
    id
  }

  fn ensure_listener_entry(&mut self, listener_id: ListenerId, callback: Value) {
    if self.listeners.contains_key(&listener_id) {
      return;
    }
    let callback_root = self.runtime.heap_mut().add_root(callback);
    self.listeners.insert(
      listener_id,
      ListenerEntry {
        callback,
        callback_root,
      },
    );
  }

  fn remove_listener_if_unused(&mut self, listener_id: ListenerId) {
    if !self.registry.contains_listener_id(listener_id) {
      self.remove_listener_id(listener_id);
    }
  }

  fn remove_listener_id(&mut self, listener_id: ListenerId) {
    if let Some(entry) = self.listeners.remove(&listener_id) {
      self.runtime.heap_mut().remove_root(entry.callback_root);
    }
  }

  fn vm_error_to_error(&mut self, err: VmError) -> Error {
    match err {
      VmError::Throw(value) => {
        // Best-effort: stringify the thrown value.
        let message = self
          .runtime
          .to_string(value)
          .ok()
          .and_then(|v| match v {
            Value::String(s) => self.runtime.heap().get_string(s).ok().map(|s| s.to_utf8_lossy()),
            _ => None,
          })
          .unwrap_or_else(|| "uncaught exception".to_string());
        Error::Other(format!("JS exception: {message}"))
      }
      other => Error::Other(format!("JS error: {other}")),
    }
  }
}

impl EventListenerInvoker for JsDomEvents {
  fn invoke(
    &mut self,
    listener_id: ListenerId,
    event: &mut Event,
  ) -> std::result::Result<(), EventsDomError> {
    let entry = self.listeners.get(&listener_id).copied().ok_or_else(|| {
      EventsDomError::new(format!(
        "unknown event listener id during dispatch: {listener_id:?}"
      ))
    })?;

    let event_id = self.event_wrapper.alloc_event_id();
    self
      .event_wrapper
      .active_events
      .borrow_mut()
      .insert(event_id, NonNull::from(&mut *event));
    let _guard = ActiveEventGuard {
      active: self.event_wrapper.active_events.clone(),
      id: event_id,
    };

    let js_event = self
      .event_wrapper
      .wrap_event(&mut self.runtime, event_id, event)
      .map_err(|e| EventsDomError::new(self.vm_error_to_error(e).to_string()))?;

    let call = self
      .runtime
      .call_function(entry.callback, Value::Undefined, &[js_event]);

    // Drop JS roots for listeners that are no longer registered (including `once` listeners, which
    // the registry removes before invoking). This must run even if the callback throws.
    self.remove_listener_if_unused(listener_id);

    call.map(|_| ()).map_err(|e| {
      EventsDomError::new(self.vm_error_to_error(e).to_string())
    })
  }
}
