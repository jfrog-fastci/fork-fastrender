use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::rc::Rc;

use vm_js::{PropertyKey, RootId, Value, VmError};

use crate::dom2;
use crate::error::{Error, Result};
use crate::js::vm_error_format;
use crate::web::events::{
  dispatch_event, AddEventListenerOptions, DomError as EventsDomError, Event, EventListenerInvoker,
  EventListenerRegistry, EventPhase, EventTargetId, ListenerId,
};
use webidl_js_runtime::{JsRuntime as _, VmJsRuntime, WebIdlJsRuntime as _};

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
  bubbles: PropertyKey,
  cancelable: PropertyKey,
  composed: PropertyKey,
  time_stamp: PropertyKey,
  target: PropertyKey,
  src_element: PropertyKey,
  current_target: PropertyKey,
  event_phase: PropertyKey,
  is_trusted: PropertyKey,
  default_prevented: PropertyKey,
  cancel_bubble: PropertyKey,
  return_value: PropertyKey,
  stop_propagation: PropertyKey,
  stop_immediate_propagation: PropertyKey,
  composed_path: PropertyKey,
  prevent_default: PropertyKey,
}

struct EventWrapper {
  prototype: Value,
  keys: EventKeys,
  active_events: ActiveEventMap,
  next_event_id: u64,
  window_target: Value,
  document_target: Value,
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
      bubbles: Self::intern_key(rt, "bubbles")?,
      cancelable: Self::intern_key(rt, "cancelable")?,
      composed: Self::intern_key(rt, "composed")?,
      time_stamp: Self::intern_key(rt, "timeStamp")?,
      target: Self::intern_key(rt, "target")?,
      src_element: Self::intern_key(rt, "srcElement")?,
      current_target: Self::intern_key(rt, "currentTarget")?,
      event_phase: Self::intern_key(rt, "eventPhase")?,
      is_trusted: Self::intern_key(rt, "isTrusted")?,
      default_prevented: Self::intern_key(rt, "defaultPrevented")?,
      stop_propagation: Self::intern_key(rt, "stopPropagation")?,
      cancel_bubble: Self::intern_key(rt, "cancelBubble")?,
      stop_immediate_propagation: Self::intern_key(rt, "stopImmediatePropagation")?,
      return_value: Self::intern_key(rt, "returnValue")?,
      composed_path: Self::intern_key(rt, "composedPath")?,
      prevent_default: Self::intern_key(rt, "preventDefault")?,
    };

    // Keep key strings alive across GC.
    //
    // These `PropertyKey` handles are stored in Rust and are not traced by the `vm-js` heap.
    // They must be explicitly rooted (or referenced by rooted JS objects) to avoid becoming
    // dangling handles after a GC cycle.
    for key in [
      keys.event_id,
      keys.type_,
      keys.bubbles,
      keys.cancelable,
      keys.composed,
      keys.time_stamp,
      keys.target,
      keys.src_element,
      keys.current_target,
      keys.event_phase,
      keys.is_trusted,
      keys.default_prevented,
      keys.stop_propagation,
      keys.cancel_bubble,
      keys.stop_immediate_propagation,
      keys.return_value,
      keys.composed_path,
      keys.prevent_default,
    ] {
      match key {
        PropertyKey::String(s) => {
          let _ = rt.heap_mut().add_root(Value::String(s))?;
        }
        PropertyKey::Symbol(sym) => {
          let _ = rt.heap_mut().add_root(Value::Symbol(sym))?;
        }
      }
    }

    // Stable JS-visible sentinels for non-node event targets.
    //
    // `vm-js` string values are not automatically interned. `webidl_js_runtime::VmJsRuntime`
    // special-cases `"window"`/`"document"` in `alloc_string_value` to return stable, rooted string
    // handles.
    let window_target = rt.alloc_string_value("window")?;
    let document_target = rt.alloc_string_value("document")?;

    // Prototype with methods/getters that mutate the active Rust `Event`.
    let prototype = rt.alloc_object_value()?;
    let _ = rt.heap_mut().add_root(prototype)?;

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

    let cancel_bubble_getter = {
      let active = active_events.clone();
      let key_event_id = keys.event_id;
      rt.alloc_function_value(move |rt, this, _args| {
        let id = get_event_id(rt, this, key_event_id)?;
        let flag = with_active_event_ret(rt, &active, id, |event| event.propagation_stopped)?;
        Ok(Value::Bool(flag))
      })?
    };
    let cancel_bubble_setter = {
      let active = active_events.clone();
      let key_event_id = keys.event_id;
      rt.alloc_function_value(move |rt, this, args| {
        let id = get_event_id(rt, this, key_event_id)?;
        let value = args.get(0).copied().unwrap_or(Value::Undefined);
        if rt.to_boolean(value)? {
          with_active_event(rt, &active, id, |event| event.stop_propagation())?;
        }
        Ok(Value::Undefined)
      })?
    };
    rt.define_accessor_property(
      prototype,
      keys.cancel_bubble,
      cancel_bubble_getter,
      cancel_bubble_setter,
      false,
    )?;

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

    let return_value_getter = {
      let active = active_events.clone();
      let key_event_id = keys.event_id;
      rt.alloc_function_value(move |rt, this, _args| {
        let id = get_event_id(rt, this, key_event_id)?;
        let flag = with_active_event_ret(rt, &active, id, |event| !event.default_prevented)?;
        Ok(Value::Bool(flag))
      })?
    };
    let return_value_setter = {
      let active = active_events.clone();
      let key_event_id = keys.event_id;
      rt.alloc_function_value(move |rt, this, args| {
        let id = get_event_id(rt, this, key_event_id)?;
        let value = args.get(0).copied().unwrap_or(Value::Undefined);
        if !rt.to_boolean(value)? {
          with_active_event(rt, &active, id, |event| event.prevent_default())?;
        }
        Ok(Value::Undefined)
      })?
    };
    rt.define_accessor_property(
      prototype,
      keys.return_value,
      return_value_getter,
      return_value_setter,
      false,
    )?;

    let composed_path = {
      let active = active_events.clone();
      let key_event_id = keys.event_id;
      let window_target = window_target;
      let document_target = document_target;
      rt.alloc_function_value(move |rt, this, _args| {
        let id = get_event_id(rt, this, key_event_id)?;
        with_active_event_result(rt, &active, id, |rt, event| {
          let arr = rt.alloc_array()?;
          let arr_root = rt.heap_mut().add_root(arr)?;
          let res = (|| {
            let path = event.composed_path();
            for (idx, target) in path.iter().copied().enumerate() {
              let key = rt.property_key_from_u32(idx as u32)?;
              let value = match target {
                EventTargetId::Window => window_target,
                EventTargetId::Document => document_target,
                EventTargetId::Node(node_id) => Value::Number(node_id.index() as f64),
                EventTargetId::Opaque(_) => Value::Null,
              };
              rt.define_data_property(arr, key, value, true)?;
            }
            Ok(arr)
          })();
          rt.heap_mut().remove_root(arr_root);
          res
        })
      })?
    };
    rt.define_data_property(prototype, keys.composed_path, composed_path, false)?;

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
      window_target,
      document_target,
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

    rt.define_data_property(
      obj,
      self.keys.event_id,
      Value::Number(event_id as f64),
      false,
    )?;

    let type_ = rt.alloc_string_value(&event.type_)?;
    rt.define_data_property(obj, self.keys.type_, type_, true)?;

    rt.define_data_property(obj, self.keys.bubbles, Value::Bool(event.bubbles), true)?;
    rt.define_data_property(
      obj,
      self.keys.cancelable,
      Value::Bool(event.cancelable),
      true,
    )?;
    rt.define_data_property(obj, self.keys.composed, Value::Bool(event.composed), true)?;
    rt.define_data_property(
      obj,
      self.keys.time_stamp,
      Value::Number(event.time_stamp),
      true,
    )?;

    let target = self.js_value_for_target(event.target);
    rt.define_data_property(obj, self.keys.target, target, true)?;
    rt.define_data_property(obj, self.keys.src_element, target, true)?;

    let current_target = self.js_value_for_target(event.current_target);
    rt.define_data_property(obj, self.keys.current_target, current_target, true)?;

    rt.define_data_property(
      obj,
      self.keys.event_phase,
      js_value_for_phase(event.event_phase),
      true,
    )?;

    rt.define_data_property(
      obj,
      self.keys.is_trusted,
      Value::Bool(event.is_trusted),
      true,
    )?;

    Ok(obj)
  }

  fn js_value_for_target(&self, target: Option<EventTargetId>) -> Value {
    match target {
      None => Value::Null,
      Some(EventTargetId::Window) => self.window_target,
      Some(EventTargetId::Document) => self.document_target,
      Some(EventTargetId::Node(node_id)) => Value::Number(node_id.index() as f64),
      Some(EventTargetId::Opaque(_)) => Value::Null,
    }
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

fn with_active_event_result<R>(
  rt: &mut VmJsRuntime,
  active: &ActiveEventMap,
  id: u64,
  f: impl FnOnce(&mut VmJsRuntime, &mut Event) -> std::result::Result<R, VmError>,
) -> std::result::Result<R, VmError> {
  let ptr = { active.borrow().get(&id).copied() };
  let Some(mut ptr) = ptr else {
    return Err(rt.throw_type_error("Event is no longer active"));
  };
  // Safety: the pointer is installed by the dispatch invoker for the duration of a listener call.
  unsafe { f(rt, ptr.as_mut()) }
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
  ) -> Result<Option<ListenerId>> {
    let Some(id) = self.listener_id_for_callback(callback) else {
      // Per DOM, `addEventListener(null, ...)` is a no-op.
      return Ok(None);
    };
    let capture = options.capture;
    if self.registry.add_event_listener(target, type_, id, options) {
      if let Err(err) = self.ensure_listener_entry(id, callback) {
        // Roll back the registry insertion so we don't leave an uncallable listener behind when
        // rooting fails due to resource limits.
        let _ = self
          .registry
          .remove_event_listener(target, type_, id, capture);
        self.remove_listener_if_unused(id);
        return Err(err);
      }
    }

    Ok(Some(id))
  }

  pub fn remove_js_event_listener(
    &mut self,
    target: EventTargetId,
    type_: &str,
    callback: JsFunctionHandle,
    capture: bool,
  ) -> bool {
    let Some(listener_id) = self.listener_id_for_callback(callback) else {
      return false;
    };
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

  fn listener_id_for_callback(&self, callback: Value) -> Option<ListenerId> {
    let Value::Object(obj) = callback else {
      // Per DOM, `addEventListener(null, ...)` is a no-op.
      return None;
    };

    let id = obj.id();
    Some(ListenerId::new(
      (id.index() as u64) | ((id.generation() as u64) << 32),
    ))
  }

  fn ensure_listener_entry(&mut self, listener_id: ListenerId, callback: Value) -> Result<()> {
    if self.listeners.contains_key(&listener_id) {
      return Ok(());
    }
    self
      .listeners
      .try_reserve(1)
      .map_err(|_| Error::Other("out of memory while registering event listener".to_string()))?;
    let callback_root = self
      .runtime
      .heap_mut()
      .add_root(callback)
      .map_err(|err| self.vm_error_to_error(err))?;
    self.listeners.insert(
      listener_id,
      ListenerEntry {
        callback,
        callback_root,
      },
    );
    Ok(())
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
    let is_exception = err.thrown_value().is_some();
    let message = vm_error_format::vm_error_to_string(self.runtime.heap_mut(), err);
    if is_exception {
      Error::Other(format!("JS exception: {message}"))
    } else {
      Error::Other(format!("JS error: {message}"))
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

    // DOM dispatch uses WebIDL's "call a user object's operation" algorithm for `EventListener`,
    // passing `event.currentTarget` as the callback this value.
    //
    // - If the listener is callable (function), invoke it with `this = currentTarget`.
    // - Otherwise, treat it as a callback interface object and invoke `listener.handleEvent(event)`
    //   with `this = listener`.
    let call = if self.runtime.is_callable(entry.callback) {
      let this_arg = self.event_wrapper.js_value_for_target(event.current_target);
      self
        .runtime
        .call_function(entry.callback, this_arg, &[js_event])
    } else if self.runtime.is_object(entry.callback) {
      // Root the event object while we look up and call handleEvent, since `get_method` may
      // allocate and trigger a GC.
      (|| -> std::result::Result<Value, VmError> {
        let event_root = self.runtime.heap_mut().add_root(js_event)?;
        let res = (|| {
          let handle_event_key = self.runtime.property_key_from_str("handleEvent")?;
          let Some(handle_event) = self.runtime.get_method(entry.callback, handle_event_key)?
          else {
            return Err(self.runtime.throw_type_error(
              "Callback interface object is missing a callable handleEvent method",
            ));
          };
          self
            .runtime
            .call_function(handle_event, entry.callback, &[js_event])
        })();
        self.runtime.heap_mut().remove_root(event_root);
        res
      })()
    } else {
      Err(
        self
          .runtime
          .throw_type_error("Event listener is not callable and not an object"),
      )
    };

    // Drop JS roots for listeners that are no longer registered (including `once` listeners, which
    // the registry removes before invoking). This must run even if the callback throws.
    self.remove_listener_if_unused(listener_id);

    call
      .map(|_| ())
      .map_err(|e| EventsDomError::new(self.vm_error_to_error(e).to_string()))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::web::events::EventInit;
  use crate::dom::{DomNode, DomNodeType};
  use selectors::context::QuirksMode;
  use std::cell::RefCell;
  use std::rc::Rc;
  use webidl_js_runtime::{JsPropertyKind, JsRuntime as _};

  #[derive(Debug, Clone, Copy)]
  enum Action {
    None,
    StopPropagation,
    StopImmediatePropagation,
    PreventDefault,
  }

  fn key(rt: &mut VmJsRuntime, name: &str) -> PropertyKey {
    let v = rt.alloc_string_value(name).expect("alloc string");
    // `PropertyKey` stores a `GcString` handle. Root it so it stays valid across potential GC cycles
    // while the key is captured by Rust listener closures.
    let _ = rt.heap_mut().add_root(v).expect("root string key");
    let Value::String(s) = v else {
      panic!("expected string");
    };
    PropertyKey::String(s)
  }

  fn as_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
    let Value::String(s) = v else {
      panic!("expected string");
    };
    rt.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  fn assert_js_value_eq(rt: &VmJsRuntime, got: Value, expected: Value) {
    match (got, expected) {
      (Value::String(_), Value::String(_)) => {
        assert_eq!(as_utf8_lossy(rt, got), as_utf8_lossy(rt, expected));
      }
      _ => assert_eq!(got, expected),
    }
  }

  fn event_accessor_setter(
    rt: &mut VmJsRuntime,
    event: Value,
    key: PropertyKey,
  ) -> std::result::Result<Value, VmError> {
    let Value::Object(obj) = event else {
      panic!("expected Event object");
    };
    let proto = rt
      .heap()
      .object_prototype(obj)?
      .expect("Event object is missing prototype");
    let desc = rt
      .get_own_property(Value::Object(proto), key)?
      .expect("Event prototype is missing property");
    let JsPropertyKind::Accessor { set, .. } = desc.kind else {
      panic!("expected accessor property");
    };
    Ok(set)
  }

  fn make_listener(
    js: &mut JsDomEvents,
    log: Rc<RefCell<Vec<&'static str>>>,
    label: &'static str,
    action: Action,
    keys: ListenerKeys,
  ) -> Value {
    js.runtime_mut()
      .alloc_function_value(move |rt, this, args| {
        let event = args.get(0).copied().unwrap_or(Value::Undefined);
        log.borrow_mut().push(label);

        // Basic Event wrapper smoke: read a few properties.
        let got_type = rt.get(event, keys.type_)?;
        assert_eq!(as_utf8_lossy(rt, got_type), "test");

        let got_target = rt.get(event, keys.target)?;
        assert_js_value_eq(rt, got_target, keys.expected_target);

        let got_current = rt.get(event, keys.current_target)?;
        assert_js_value_eq(rt, got_current, keys.expected_current_target);
        // Callable listeners are invoked with `this = event.currentTarget`.
        assert_js_value_eq(rt, this, keys.expected_current_target);

        let got_phase = rt.get(event, keys.event_phase)?;
        assert_eq!(got_phase, Value::Number(keys.expected_phase));

        let bubbles = rt.get(event, keys.bubbles)?;
        assert_eq!(bubbles, Value::Bool(keys.expected_bubbles));
        let cancelable = rt.get(event, keys.cancelable)?;
        assert_eq!(cancelable, Value::Bool(keys.expected_cancelable));
        let composed = rt.get(event, keys.composed)?;
        assert_eq!(composed, Value::Bool(keys.expected_composed));
        let is_trusted = rt.get(event, keys.is_trusted)?;
        assert_eq!(is_trusted, Value::Bool(keys.expected_is_trusted));

        match action {
          Action::None => {}
          Action::StopPropagation => {
            let f = rt.get(event, keys.stop_propagation)?;
            rt.call_function(f, event, &[])?;
          }
          Action::StopImmediatePropagation => {
            let f = rt.get(event, keys.stop_immediate_propagation)?;
            rt.call_function(f, event, &[])?;
          }
          Action::PreventDefault => {
            let f = rt.get(event, keys.prevent_default)?;
            rt.call_function(f, event, &[])?;
          }
        }
        Ok(Value::Undefined)
      })
      .expect("alloc function")
  }

  #[derive(Clone, Copy)]
  struct ListenerKeys {
    type_: PropertyKey,
    bubbles: PropertyKey,
    cancelable: PropertyKey,
    composed: PropertyKey,
    target: PropertyKey,
    current_target: PropertyKey,
    event_phase: PropertyKey,
    is_trusted: PropertyKey,
    stop_propagation: PropertyKey,
    stop_immediate_propagation: PropertyKey,
    prevent_default: PropertyKey,
    expected_target: Value,
    expected_current_target: Value,
    expected_phase: f64,
    expected_bubbles: bool,
    expected_cancelable: bool,
    expected_composed: bool,
    expected_is_trusted: bool,
  }

  fn element(tag_name: &str, children: Vec<DomNode>) -> DomNode {
    DomNode {
      node_type: DomNodeType::Element {
        tag_name: tag_name.to_string(),
        namespace: String::new(),
        attributes: Vec::new(),
      },
      children,
    }
  }

  fn build_doc_body_target() -> (dom2::Document, dom2::NodeId, dom2::NodeId) {
    // Document → <body> → <div>
    let root = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
        scripting_enabled: true,
      },
      children: vec![element("body", vec![element("div", vec![])])],
    };
    let doc = dom2::Document::from_renderer_dom(&root);
    let root_id = doc.root();
    let body = doc.node(root_id).children[0];
    let target = doc.node(body).children[0];
    (doc, body, target)
  }

  #[test]
  fn js_listeners_capture_and_bubble_in_dom_order() -> Result<()> {
    let (doc, body, target) = build_doc_body_target();
    let mut js = JsDomEvents::new()?;

    let key_type = key(js.runtime_mut(), "type");
    let key_bubbles = key(js.runtime_mut(), "bubbles");
    let key_cancelable = key(js.runtime_mut(), "cancelable");
    let key_composed = key(js.runtime_mut(), "composed");
    let key_target = key(js.runtime_mut(), "target");
    let key_current_target = key(js.runtime_mut(), "currentTarget");
    let key_event_phase = key(js.runtime_mut(), "eventPhase");
    let key_is_trusted = key(js.runtime_mut(), "isTrusted");
    let key_stop_propagation = key(js.runtime_mut(), "stopPropagation");
    let key_stop_immediate_propagation = key(js.runtime_mut(), "stopImmediatePropagation");
    let key_prevent_default = key(js.runtime_mut(), "preventDefault");

    let target_value = Value::Number(target.index() as f64);
    let body_value = Value::Number(body.index() as f64);
    let doc_value = js
      .runtime_mut()
      .alloc_string_value("document")
      .expect("alloc string");
    let _ = js
      .runtime_mut()
      .heap_mut()
      .add_root(doc_value)
      .expect("root document sentinel");

    let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

    let doc_capture = make_listener(
      &mut js,
      log.clone(),
      "doc-capture",
      Action::None,
      ListenerKeys {
        type_: key_type,
        bubbles: key_bubbles,
        cancelable: key_cancelable,
        composed: key_composed,
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        is_trusted: key_is_trusted,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: doc_value,
        expected_phase: 1.0,
        expected_bubbles: true,
        expected_cancelable: false,
        expected_composed: false,
        expected_is_trusted: false,
      },
    );
    let body_capture = make_listener(
      &mut js,
      log.clone(),
      "body-capture",
      Action::None,
      ListenerKeys {
        expected_current_target: body_value,
        ..ListenerKeys {
          type_: key_type,
          bubbles: key_bubbles,
          cancelable: key_cancelable,
          composed: key_composed,
          target: key_target,
          current_target: key_current_target,
          event_phase: key_event_phase,
          is_trusted: key_is_trusted,
          stop_propagation: key_stop_propagation,
          stop_immediate_propagation: key_stop_immediate_propagation,
          prevent_default: key_prevent_default,
          expected_target: target_value,
          expected_current_target: body_value,
          expected_phase: 1.0,
          expected_bubbles: true,
          expected_cancelable: false,
          expected_composed: false,
          expected_is_trusted: false,
        }
      },
    );

    let target_capture = make_listener(
      &mut js,
      log.clone(),
      "target-capture",
      Action::None,
      ListenerKeys {
        expected_current_target: target_value,
        expected_phase: 2.0,
        ..ListenerKeys {
          type_: key_type,
          bubbles: key_bubbles,
          cancelable: key_cancelable,
          composed: key_composed,
          target: key_target,
          current_target: key_current_target,
          event_phase: key_event_phase,
          is_trusted: key_is_trusted,
          stop_propagation: key_stop_propagation,
          stop_immediate_propagation: key_stop_immediate_propagation,
          prevent_default: key_prevent_default,
          expected_target: target_value,
          expected_current_target: target_value,
          expected_phase: 2.0,
          expected_bubbles: true,
          expected_cancelable: false,
          expected_composed: false,
          expected_is_trusted: false,
        }
      },
    );

    let target_bubble = make_listener(
      &mut js,
      log.clone(),
      "target-bubble",
      Action::None,
      ListenerKeys {
        expected_current_target: target_value,
        expected_phase: 2.0,
        ..ListenerKeys {
          type_: key_type,
          bubbles: key_bubbles,
          cancelable: key_cancelable,
          composed: key_composed,
          target: key_target,
          current_target: key_current_target,
          event_phase: key_event_phase,
          is_trusted: key_is_trusted,
          stop_propagation: key_stop_propagation,
          stop_immediate_propagation: key_stop_immediate_propagation,
          prevent_default: key_prevent_default,
          expected_target: target_value,
          expected_current_target: target_value,
          expected_phase: 2.0,
          expected_bubbles: true,
          expected_cancelable: false,
          expected_composed: false,
          expected_is_trusted: false,
        }
      },
    );

    let body_bubble = make_listener(
      &mut js,
      log.clone(),
      "body-bubble",
      Action::None,
      ListenerKeys {
        expected_current_target: body_value,
        expected_phase: 3.0,
        ..ListenerKeys {
          type_: key_type,
          bubbles: key_bubbles,
          cancelable: key_cancelable,
          composed: key_composed,
          target: key_target,
          current_target: key_current_target,
          event_phase: key_event_phase,
          is_trusted: key_is_trusted,
          stop_propagation: key_stop_propagation,
          stop_immediate_propagation: key_stop_immediate_propagation,
          prevent_default: key_prevent_default,
          expected_target: target_value,
          expected_current_target: body_value,
          expected_phase: 3.0,
          expected_bubbles: true,
          expected_cancelable: false,
          expected_composed: false,
          expected_is_trusted: false,
        }
      },
    );

    let doc_bubble = make_listener(
      &mut js,
      log.clone(),
      "doc-bubble",
      Action::None,
      ListenerKeys {
        expected_current_target: doc_value,
        expected_phase: 3.0,
        ..ListenerKeys {
          type_: key_type,
          bubbles: key_bubbles,
          cancelable: key_cancelable,
          composed: key_composed,
          target: key_target,
          current_target: key_current_target,
          event_phase: key_event_phase,
          is_trusted: key_is_trusted,
          stop_propagation: key_stop_propagation,
          stop_immediate_propagation: key_stop_immediate_propagation,
          prevent_default: key_prevent_default,
          expected_target: target_value,
          expected_current_target: doc_value,
          expected_phase: 3.0,
          expected_bubbles: true,
          expected_cancelable: false,
          expected_composed: false,
          expected_is_trusted: false,
        }
      },
    );

    let type_ = "test";
    let _ = js.add_js_event_listener(
      EventTargetId::Document,
      type_,
      doc_capture,
      AddEventListenerOptions {
        capture: true,
        ..Default::default()
      },
    )?;
    let _ = js.add_js_event_listener(
      EventTargetId::Node(body),
      type_,
      body_capture,
      AddEventListenerOptions {
        capture: true,
        ..Default::default()
      },
    )?;
    let _ = js.add_js_event_listener(
      EventTargetId::Node(target),
      type_,
      target_capture,
      AddEventListenerOptions {
        capture: true,
        ..Default::default()
      },
    )?;

    let _ = js.add_js_event_listener(
      EventTargetId::Node(target),
      type_,
      target_bubble,
      AddEventListenerOptions::default(),
    )?;
    let _ = js.add_js_event_listener(
      EventTargetId::Node(body),
      type_,
      body_bubble,
      AddEventListenerOptions::default(),
    )?;
    let _ = js.add_js_event_listener(
      EventTargetId::Document,
      type_,
      doc_bubble,
      AddEventListenerOptions::default(),
    )?;

    let mut event = Event::new(
      type_,
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );
    js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

    assert_eq!(
      *log.borrow(),
      vec![
        "doc-capture",
        "body-capture",
        "target-capture",
        "target-bubble",
        "body-bubble",
        "doc-bubble"
      ]
    );
    Ok(())
  }

  #[test]
  fn js_stop_propagation_is_observed_by_dispatch() -> Result<()> {
    let (doc, body, target) = build_doc_body_target();
    let mut js = JsDomEvents::new()?;

    let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

    let key_type = key(js.runtime_mut(), "type");
    let key_bubbles = key(js.runtime_mut(), "bubbles");
    let key_cancelable = key(js.runtime_mut(), "cancelable");
    let key_composed = key(js.runtime_mut(), "composed");
    let key_target = key(js.runtime_mut(), "target");
    let key_current_target = key(js.runtime_mut(), "currentTarget");
    let key_event_phase = key(js.runtime_mut(), "eventPhase");
    let key_is_trusted = key(js.runtime_mut(), "isTrusted");
    let key_stop_propagation = key(js.runtime_mut(), "stopPropagation");
    let key_stop_immediate_propagation = key(js.runtime_mut(), "stopImmediatePropagation");
    let key_prevent_default = key(js.runtime_mut(), "preventDefault");

    let target_value = Value::Number(target.index() as f64);

    let stopper = make_listener(
      &mut js,
      log.clone(),
      "target-stop",
      Action::StopPropagation,
      ListenerKeys {
        type_: key_type,
        bubbles: key_bubbles,
        cancelable: key_cancelable,
        composed: key_composed,
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        is_trusted: key_is_trusted,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: target_value,
        expected_phase: 2.0,
        expected_bubbles: true,
        expected_cancelable: false,
        expected_composed: false,
        expected_is_trusted: false,
      },
    );

    let body_bubble = make_listener(
      &mut js,
      log.clone(),
      "body-bubble",
      Action::None,
      ListenerKeys {
        expected_current_target: Value::Number(body.index() as f64),
        expected_phase: 3.0,
        ..ListenerKeys {
          type_: key_type,
          bubbles: key_bubbles,
          cancelable: key_cancelable,
          composed: key_composed,
          target: key_target,
          current_target: key_current_target,
          event_phase: key_event_phase,
          is_trusted: key_is_trusted,
          stop_propagation: key_stop_propagation,
          stop_immediate_propagation: key_stop_immediate_propagation,
          prevent_default: key_prevent_default,
          expected_target: target_value,
          expected_current_target: Value::Number(body.index() as f64),
          expected_phase: 3.0,
          expected_bubbles: true,
          expected_cancelable: false,
          expected_composed: false,
          expected_is_trusted: false,
        }
      },
    );

    let _ = js.add_js_event_listener(
      EventTargetId::Node(target),
      "test",
      stopper,
      AddEventListenerOptions::default(),
    )?;
    let _ = js.add_js_event_listener(
      EventTargetId::Node(body),
      "test",
      body_bubble,
      AddEventListenerOptions::default(),
    )?;

    let mut event = Event::new(
      "test",
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );
    js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

    assert_eq!(*log.borrow(), vec!["target-stop"]);
    Ok(())
  }

  #[test]
  fn js_stop_immediate_propagation_skips_later_listeners_on_same_target() -> Result<()> {
    let (doc, _body, target) = build_doc_body_target();
    let mut js = JsDomEvents::new()?;

    let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

    let key_type = key(js.runtime_mut(), "type");
    let key_bubbles = key(js.runtime_mut(), "bubbles");
    let key_cancelable = key(js.runtime_mut(), "cancelable");
    let key_composed = key(js.runtime_mut(), "composed");
    let key_target = key(js.runtime_mut(), "target");
    let key_current_target = key(js.runtime_mut(), "currentTarget");
    let key_event_phase = key(js.runtime_mut(), "eventPhase");
    let key_is_trusted = key(js.runtime_mut(), "isTrusted");
    let key_stop_propagation = key(js.runtime_mut(), "stopPropagation");
    let key_stop_immediate_propagation = key(js.runtime_mut(), "stopImmediatePropagation");
    let key_prevent_default = key(js.runtime_mut(), "preventDefault");

    let target_value = Value::Number(target.index() as f64);

    let first = make_listener(
      &mut js,
      log.clone(),
      "first",
      Action::StopImmediatePropagation,
      ListenerKeys {
        type_: key_type,
        bubbles: key_bubbles,
        cancelable: key_cancelable,
        composed: key_composed,
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        is_trusted: key_is_trusted,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: target_value,
        expected_phase: 2.0,
        expected_bubbles: true,
        expected_cancelable: false,
        expected_composed: false,
        expected_is_trusted: false,
      },
    );

    let second = make_listener(
      &mut js,
      log.clone(),
      "second",
      Action::None,
      ListenerKeys {
        type_: key_type,
        bubbles: key_bubbles,
        cancelable: key_cancelable,
        composed: key_composed,
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        is_trusted: key_is_trusted,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: target_value,
        expected_phase: 2.0,
        expected_bubbles: true,
        expected_cancelable: false,
        expected_composed: false,
        expected_is_trusted: false,
      },
    );

    let _ = js.add_js_event_listener(
      EventTargetId::Node(target),
      "test",
      first,
      AddEventListenerOptions::default(),
    )?;
    let _ = js.add_js_event_listener(
      EventTargetId::Node(target),
      "test",
      second,
      AddEventListenerOptions::default(),
    )?;

    let mut event = Event::new(
      "test",
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );
    js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

    assert_eq!(*log.borrow(), vec!["first"]);
    Ok(())
  }

  #[test]
  fn js_once_listener_runs_only_once() -> Result<()> {
    let (doc, _body, target) = build_doc_body_target();
    let mut js = JsDomEvents::new()?;

    let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

    let key_type = key(js.runtime_mut(), "type");
    let key_bubbles = key(js.runtime_mut(), "bubbles");
    let key_cancelable = key(js.runtime_mut(), "cancelable");
    let key_composed = key(js.runtime_mut(), "composed");
    let key_target = key(js.runtime_mut(), "target");
    let key_current_target = key(js.runtime_mut(), "currentTarget");
    let key_event_phase = key(js.runtime_mut(), "eventPhase");
    let key_is_trusted = key(js.runtime_mut(), "isTrusted");
    let key_stop_propagation = key(js.runtime_mut(), "stopPropagation");
    let key_stop_immediate_propagation = key(js.runtime_mut(), "stopImmediatePropagation");
    let key_prevent_default = key(js.runtime_mut(), "preventDefault");

    let target_value = Value::Number(target.index() as f64);

    let once = make_listener(
      &mut js,
      log.clone(),
      "once",
      Action::None,
      ListenerKeys {
        type_: key_type,
        bubbles: key_bubbles,
        cancelable: key_cancelable,
        composed: key_composed,
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        is_trusted: key_is_trusted,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: target_value,
        expected_phase: 2.0,
        expected_bubbles: true,
        expected_cancelable: false,
        expected_composed: false,
        expected_is_trusted: false,
      },
    );

    let _ = js.add_js_event_listener(
      EventTargetId::Node(target),
      "test",
      once,
      AddEventListenerOptions {
        once: true,
        ..Default::default()
      },
    )?;

    let mut event = Event::new(
      "test",
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );
    js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

    let mut event2 = Event::new(
      "test",
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );
    js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event2)?;

    assert_eq!(*log.borrow(), vec!["once"]);
    Ok(())
  }

  #[test]
  fn js_passive_listener_cannot_prevent_default() -> Result<()> {
    let (doc, _body, target) = build_doc_body_target();
    let mut js = JsDomEvents::new()?;

    let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

    let key_type = key(js.runtime_mut(), "type");
    let key_bubbles = key(js.runtime_mut(), "bubbles");
    let key_cancelable = key(js.runtime_mut(), "cancelable");
    let key_composed = key(js.runtime_mut(), "composed");
    let key_target = key(js.runtime_mut(), "target");
    let key_current_target = key(js.runtime_mut(), "currentTarget");
    let key_event_phase = key(js.runtime_mut(), "eventPhase");
    let key_is_trusted = key(js.runtime_mut(), "isTrusted");
    let key_stop_propagation = key(js.runtime_mut(), "stopPropagation");
    let key_stop_immediate_propagation = key(js.runtime_mut(), "stopImmediatePropagation");
    let key_prevent_default = key(js.runtime_mut(), "preventDefault");

    let target_value = Value::Number(target.index() as f64);

    let passive = make_listener(
      &mut js,
      log.clone(),
      "passive",
      Action::PreventDefault,
      ListenerKeys {
        type_: key_type,
        bubbles: key_bubbles,
        cancelable: key_cancelable,
        composed: key_composed,
        target: key_target,
        current_target: key_current_target,
        event_phase: key_event_phase,
        is_trusted: key_is_trusted,
        stop_propagation: key_stop_propagation,
        stop_immediate_propagation: key_stop_immediate_propagation,
        prevent_default: key_prevent_default,
        expected_target: target_value,
        expected_current_target: target_value,
        expected_phase: 2.0,
        expected_bubbles: true,
        expected_cancelable: true,
        expected_composed: false,
        expected_is_trusted: false,
      },
    );

    let _ = js.add_js_event_listener(
      EventTargetId::Node(target),
      "test",
      passive,
      AddEventListenerOptions {
        passive: true,
        ..Default::default()
      },
    )?;

    let mut event = Event::new(
      "test",
      EventInit {
        bubbles: true,
        cancelable: true,
        ..Default::default()
      },
    );
    let res = js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;
    assert!(res, "dispatchEvent should return true if not canceled");
    assert!(
      !event.default_prevented,
      "passive listeners must not set defaultPrevented"
    );

    assert_eq!(*log.borrow(), vec!["passive"]);
    Ok(())
  }

  #[test]
  fn js_prevent_default_sets_default_prevented_property() -> Result<()> {
    let (doc, _body, target) = build_doc_body_target();
    let mut js = JsDomEvents::new()?;

    let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

    let key_default_prevented = key(js.runtime_mut(), "defaultPrevented");
    let key_prevent_default = key(js.runtime_mut(), "preventDefault");

    let log_for_cb = log.clone();
    let expected_this = Value::Number(target.index() as f64);
    let listener = js
      .runtime_mut()
      .alloc_function_value(move |rt, this, args| {
        assert_eq!(this, expected_this);
        let event = args.get(0).copied().unwrap_or(Value::Undefined);
        log_for_cb.borrow_mut().push("listener");

        let f = rt.get(event, key_prevent_default)?;
        rt.call_function(f, event, &[])?;

        let prevented = rt.get(event, key_default_prevented)?;
        assert_eq!(prevented, Value::Bool(true));

        Ok(Value::Undefined)
      })
      .expect("alloc function");

    let _ = js.add_js_event_listener(
      EventTargetId::Node(target),
      "test",
      listener,
      AddEventListenerOptions::default(),
    )?;

    let mut event = Event::new(
      "test",
      EventInit {
        bubbles: true,
        cancelable: true,
        ..Default::default()
      },
    );
    let dispatch_ok = js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

    assert!(
      !dispatch_ok,
      "dispatchEvent should return false when canceled"
    );
    assert!(event.default_prevented);
    assert_eq!(*log.borrow(), vec!["listener"]);
    Ok(())
  }

  #[test]
  fn js_callback_interface_listener_object_invokes_handle_event() -> Result<()> {
    let (doc, _body, target) = build_doc_body_target();
    let mut js = JsDomEvents::new()?;

    let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

    let key_type = key(js.runtime_mut(), "type");
    let key_bubbles = key(js.runtime_mut(), "bubbles");
    let key_cancelable = key(js.runtime_mut(), "cancelable");
    let key_composed = key(js.runtime_mut(), "composed");
    let key_target = key(js.runtime_mut(), "target");
    let key_current_target = key(js.runtime_mut(), "currentTarget");
    let key_event_phase = key(js.runtime_mut(), "eventPhase");
    let key_is_trusted = key(js.runtime_mut(), "isTrusted");

    let target_value = Value::Number(target.index() as f64);

    // Listener object implements the EventListener callback interface by exposing a callable
    // `handleEvent` method.
    let listener_obj = js
      .runtime_mut()
      .alloc_object_value()
      .expect("alloc listener object");
    let listener_obj_for_assert = listener_obj;
    let log_for_cb = log.clone();

    let keys = ListenerKeys {
      type_: key_type,
      bubbles: key_bubbles,
      cancelable: key_cancelable,
      composed: key_composed,
      target: key_target,
      current_target: key_current_target,
      event_phase: key_event_phase,
      is_trusted: key_is_trusted,
      // Unused for this test, but required by the struct.
      stop_propagation: key(js.runtime_mut(), "stopPropagation"),
      stop_immediate_propagation: key(js.runtime_mut(), "stopImmediatePropagation"),
      prevent_default: key(js.runtime_mut(), "preventDefault"),
      expected_target: target_value,
      expected_current_target: target_value,
      expected_phase: 2.0,
      expected_bubbles: true,
      expected_cancelable: false,
      expected_composed: false,
      expected_is_trusted: false,
    };

    let handle_event = js
      .runtime_mut()
      .alloc_function_value(move |rt, this, args| {
        // Per WebIDL "call a user object's operation", handleEvent is called with `this = listener`.
        assert_eq!(this, listener_obj_for_assert);

        let event = args.get(0).copied().unwrap_or(Value::Undefined);
        log_for_cb.borrow_mut().push("handleEvent");

        let got_type = rt.get(event, keys.type_)?;
        assert_eq!(as_utf8_lossy(rt, got_type), "test");
        let got_target = rt.get(event, keys.target)?;
        assert_eq!(got_target, keys.expected_target);
        let got_current = rt.get(event, keys.current_target)?;
        assert_eq!(got_current, keys.expected_current_target);
        let got_phase = rt.get(event, keys.event_phase)?;
        assert_eq!(got_phase, Value::Number(keys.expected_phase));

        let bubbles = rt.get(event, keys.bubbles)?;
        assert_eq!(bubbles, Value::Bool(keys.expected_bubbles));
        let cancelable = rt.get(event, keys.cancelable)?;
        assert_eq!(cancelable, Value::Bool(keys.expected_cancelable));
        let composed = rt.get(event, keys.composed)?;
        assert_eq!(composed, Value::Bool(keys.expected_composed));
        let is_trusted = rt.get(event, keys.is_trusted)?;
        assert_eq!(is_trusted, Value::Bool(keys.expected_is_trusted));

        Ok(Value::Undefined)
      })
      .expect("alloc handleEvent");

    let handle_event_key = js
      .runtime_mut()
      .property_key_from_str("handleEvent")
      .expect("intern handleEvent key");
    js.runtime_mut()
      .define_data_property(listener_obj, handle_event_key, handle_event, true)
      .expect("define handleEvent");

    let _ = js.add_js_event_listener(
      EventTargetId::Node(target),
      "test",
      listener_obj,
      AddEventListenerOptions::default(),
    )?;

    let mut event = Event::new(
      "test",
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );
    js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

    assert_eq!(*log.borrow(), vec!["handleEvent"]);
    Ok(())
  }

  #[test]
  fn js_cancel_bubble_setter_stops_propagation() -> Result<()> {
    let (doc, body, target) = build_doc_body_target();
    let mut js = JsDomEvents::new()?;

    let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

    let key_cancel_bubble = key(js.runtime_mut(), "cancelBubble");

    let log_for_target = log.clone();
    let target_listener = js
      .runtime_mut()
      .alloc_function_value(move |rt, _this, args| {
        let event = args.get(0).copied().unwrap_or(Value::Undefined);
        log_for_target.borrow_mut().push("target");

        let initial = rt.get(event, key_cancel_bubble)?;
        assert_eq!(initial, Value::Bool(false));

        let setter = event_accessor_setter(rt, event, key_cancel_bubble)?;
        rt.call_function(setter, event, &[Value::Bool(true)])?;

        let updated = rt.get(event, key_cancel_bubble)?;
        assert_eq!(updated, Value::Bool(true));

        Ok(Value::Undefined)
      })
      .expect("alloc function");

    let log_for_body = log.clone();
    let body_listener = js
      .runtime_mut()
      .alloc_function_value(move |_rt, _this, _args| {
        log_for_body.borrow_mut().push("body");
        Ok(Value::Undefined)
      })
      .expect("alloc function");

    let _ = js.add_js_event_listener(
      EventTargetId::Node(target),
      "test",
      target_listener,
      AddEventListenerOptions::default(),
    )?;
    let _ = js.add_js_event_listener(
      EventTargetId::Node(body),
      "test",
      body_listener,
      AddEventListenerOptions::default(),
    )?;

    let mut event = Event::new(
      "test",
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );
    js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

    assert_eq!(*log.borrow(), vec!["target"]);
    Ok(())
  }

  #[test]
  fn js_return_value_setter_false_calls_prevent_default() -> Result<()> {
    let (doc, _body, target) = build_doc_body_target();
    let mut js = JsDomEvents::new()?;

    let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

    let key_return_value = key(js.runtime_mut(), "returnValue");
    let key_default_prevented = key(js.runtime_mut(), "defaultPrevented");

    let log_for_cb = log.clone();
    let expected_this = Value::Number(target.index() as f64);
    let listener = js
      .runtime_mut()
      .alloc_function_value(move |rt, this, args| {
        assert_eq!(this, expected_this);
        let event = args.get(0).copied().unwrap_or(Value::Undefined);
        log_for_cb.borrow_mut().push("listener");

        let initial = rt.get(event, key_return_value)?;
        assert_eq!(initial, Value::Bool(true));
        let initial_prevented = rt.get(event, key_default_prevented)?;
        assert_eq!(initial_prevented, Value::Bool(false));

        let setter = event_accessor_setter(rt, event, key_return_value)?;
        rt.call_function(setter, event, &[Value::Bool(false)])?;

        let updated = rt.get(event, key_return_value)?;
        assert_eq!(updated, Value::Bool(false));
        let updated_prevented = rt.get(event, key_default_prevented)?;
        assert_eq!(updated_prevented, Value::Bool(true));

        Ok(Value::Undefined)
      })
      .expect("alloc function");

    let _ = js.add_js_event_listener(
      EventTargetId::Node(target),
      "test",
      listener,
      AddEventListenerOptions::default(),
    )?;

    let mut event = Event::new(
      "test",
      EventInit {
        bubbles: true,
        cancelable: true,
        ..Default::default()
      },
    );
    let dispatch_ok = js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;

    assert!(
      !dispatch_ok,
      "dispatchEvent should return false when canceled via returnValue"
    );
    assert!(event.default_prevented);
    assert_eq!(*log.borrow(), vec!["listener"]);
    Ok(())
  }

  #[test]
  fn js_composed_path_and_src_element_reflect_dispatch_path() -> Result<()> {
    let (doc, _body, target) = build_doc_body_target();
    let mut js = JsDomEvents::new()?;

    let key_composed_path = key(js.runtime_mut(), "composedPath");
    let key_length = key(js.runtime_mut(), "length");
    let key_src_element = key(js.runtime_mut(), "srcElement");

    let window_value = js
      .runtime_mut()
      .alloc_string_value("window")
      .expect("alloc window string");
    let _ = js
      .runtime_mut()
      .heap_mut()
      .add_root(window_value)
      .expect("root window sentinel");
    let document_value = js
      .runtime_mut()
      .alloc_string_value("document")
      .expect("alloc document string");
    let _ = js
      .runtime_mut()
      .heap_mut()
      .add_root(document_value)
      .expect("root document sentinel");

    let mut expected: Vec<Value> = Vec::new();
    expected.push(Value::Number(target.index() as f64));
    let mut current = target;
    loop {
      let Some(parent) = doc.node(current).parent else {
        break;
      };
      if matches!(doc.node(parent).kind, dom2::NodeKind::Document { .. }) {
        break;
      }
      expected.push(Value::Number(parent.index() as f64));
      current = parent;
    }
    expected.push(document_value);
    expected.push(window_value);

    let expected_for_cb = expected.clone();
    let listener = js
      .runtime_mut()
      .alloc_function_value(move |rt, _this, args| {
        let event = args.get(0).copied().unwrap_or(Value::Undefined);

        let src_element = rt.get(event, key_src_element)?;
        assert_eq!(src_element, expected_for_cb[0]);

        let composed_path_fn = rt.get(event, key_composed_path)?;
        let path = rt.call_function(composed_path_fn, event, &[])?;

        let len = rt.get(path, key_length)?;
        assert_eq!(len, Value::Number(expected_for_cb.len() as f64));

        for (idx, expected_value) in expected_for_cb.iter().copied().enumerate() {
          let key = rt.property_key_from_u32(idx as u32)?;
          let got = rt.get(path, key)?;
          assert_js_value_eq(rt, got, expected_value);
        }

        Ok(Value::Undefined)
      })
      .expect("alloc function");

    let _ = js.add_js_event_listener(
      EventTargetId::Node(target),
      "test",
      listener,
      AddEventListenerOptions::default(),
    )?;

    let mut event = Event::new(
      "test",
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );
    js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;
    Ok(())
  }

  #[test]
  fn js_time_stamp_is_number() -> Result<()> {
    let (doc, _body, target) = build_doc_body_target();
    let mut js = JsDomEvents::new()?;

    let key_time_stamp = key(js.runtime_mut(), "timeStamp");

    let listener = js
      .runtime_mut()
      .alloc_function_value(move |rt, _this, args| {
        let event = args.get(0).copied().unwrap_or(Value::Undefined);
        let ts = rt.get(event, key_time_stamp)?;
        let Value::Number(n) = ts else {
          panic!("expected timeStamp to be a number");
        };
        assert!(n.is_finite());
        assert!(n >= 0.0);
        Ok(Value::Undefined)
      })
      .expect("alloc function");

    let _ = js.add_js_event_listener(
      EventTargetId::Node(target),
      "test",
      listener,
      AddEventListenerOptions::default(),
    )?;

    let mut event = Event::new(
      "test",
      EventInit {
        bubbles: true,
        ..Default::default()
      },
    );
    js.dispatch_dom_event(&doc, EventTargetId::Node(target), &mut event)?;
    Ok(())
  }
}
