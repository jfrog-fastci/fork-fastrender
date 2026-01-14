use crate::dom2;
use crate::web::events::{
  dispatch_event, AddEventListenerOptions, DomError, Event, EventInit, EventListenerInvoker,
  EventPhase, EventTargetId, ListenerId,
};
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::rc::Rc;
use vm_js::{GcObject, PropertyKey, RootId, Value, VmError, WeakGcObject};
use webidl_js_runtime::{JsRuntime as _, VmJsRuntime, WebIdlJsRuntime as _};

/// Fallible `Box::new` that returns `VmError::OutOfMemory` instead of aborting the process.
///
/// Event creation/dispatch is reachable from untrusted JS and uses fallible `Result<_, VmError>`
/// APIs. Rust's default `Box::new` aborts the process on allocator OOM, so use a manual allocation.
#[inline]
fn box_try_new_vm<T>(value: T) -> Result<Box<T>, VmError> {
  // `Box::new` does not allocate for ZSTs, so it cannot fail with OOM.
  if std::mem::size_of::<T>() == 0 {
    return Ok(Box::new(value));
  }

  let layout = std::alloc::Layout::new::<T>();
  // SAFETY: `alloc` returns either a suitably aligned block of memory for `T` or null on OOM. We
  // write `value` into it and transfer ownership to `Box`.
  unsafe {
    let ptr = std::alloc::alloc(layout) as *mut T;
    if ptr.is_null() {
      return Err(VmError::OutOfMemory);
    }
    ptr.write(value);
    Ok(Box::from_raw(ptr))
  }
}

#[derive(Debug, Clone, Copy)]
struct ListenerEntry {
  callback: Value,
  callback_root: RootId,
}

pub struct DomEventsRealm {
  ctx: Rc<DomEventsContext>,
  pub window: Value,
  pub document: Value,
  pub event_target_prototype: Value,
  pub event_prototype: Value,
  pub event_constructor: Value,
}

impl DomEventsRealm {
  pub fn new(rt: &mut VmJsRuntime, dom: dom2::Document) -> Result<Self, VmError> {
    let event_target_prototype = rt.alloc_object_value()?;
    rt.heap_mut().add_root(event_target_prototype)?;
    let event_prototype = rt.alloc_object_value()?;
    rt.heap_mut().add_root(event_prototype)?;
    let ctx = Rc::new(DomEventsContext {
      dom,
      event_target_prototype,
      node_wrappers: RefCell::new(FxHashMap::default()),
      listeners: RefCell::new(FxHashMap::default()),
      events: RefCell::new(FxHashMap::default()),
      event_target_by_obj: RefCell::new(FxHashMap::default()),
      obj_by_event_target: RefCell::new(FxHashMap::default()),
    });
    install_event_target_prototype(rt, ctx.clone(), event_target_prototype)?;
    install_event_prototype(rt, ctx.clone(), event_prototype)?;
    let event_constructor = install_event_constructor(rt, ctx.clone(), event_prototype)?;

    // Create window/document EventTargets (MVP).
    let window = rt.alloc_object_value()?;
    rt.heap_mut().add_root(window)?;
    rt.set_prototype(window, Some(event_target_prototype))?;
    ctx.register_event_target(EventTargetId::Window, window)?;

    let document = rt.alloc_object_value()?;
    rt.heap_mut().add_root(document)?;
    rt.set_prototype(document, Some(event_target_prototype))?;
    ctx.register_event_target(EventTargetId::Document, document)?;

    // Expose constructors on the window object.
    let event_key = prop_key_str(rt, "Event")?;
    rt.define_data_property(window, event_key, event_constructor, false)?;

    Ok(Self {
      ctx,
      window,
      document,
      event_target_prototype,
      event_prototype,
      event_constructor,
    })
  }

  pub fn create_node_wrapper(
    &self,
    rt: &mut VmJsRuntime,
    node_id: dom2::NodeId,
  ) -> Result<Value, VmError> {
    self.ctx.get_or_create_node_wrapper(rt, node_id)
  }

  pub fn ctx(&self) -> &DomEventsContext {
    &self.ctx
  }
}

pub struct DomEventsContext {
  dom: dom2::Document,
  event_target_prototype: Value,
  node_wrappers: RefCell<FxHashMap<dom2::NodeId, WeakGcObject>>,
  listeners: RefCell<FxHashMap<ListenerId, ListenerEntry>>,
  events: RefCell<FxHashMap<GcObject, Box<Event>>>,
  event_target_by_obj: RefCell<FxHashMap<WeakGcObject, EventTargetId>>,
  obj_by_event_target: RefCell<FxHashMap<EventTargetId, WeakGcObject>>,
}

impl DomEventsContext {
  fn register_event_target(&self, target: EventTargetId, obj: Value) -> Result<(), VmError> {
    let Value::Object(handle) = obj else {
      return Err(VmError::Unimplemented(
        "register_event_target: value is not an object",
      ));
    };
    let weak = WeakGcObject::from(handle);
    self.event_target_by_obj.borrow_mut().insert(weak, target);
    self.obj_by_event_target.borrow_mut().insert(target, weak);
    Ok(())
  }

  fn event_target_id_for_value(&self, value: Value) -> Option<EventTargetId> {
    let Value::Object(obj) = value else {
      return None;
    };
    self
      .event_target_by_obj
      .borrow()
      .get(&WeakGcObject::from(obj))
      .copied()
  }

  fn get_or_create_node_wrapper(
    &self,
    rt: &mut VmJsRuntime,
    node_id: dom2::NodeId,
  ) -> Result<Value, VmError> {
    let existing = { self.node_wrappers.borrow().get(&node_id).copied() };
    if let Some(existing) = existing {
      if let Some(obj) = existing.upgrade(rt.heap()) {
        return Ok(Value::Object(obj));
      }
      // Stale wrapper: keep `event_target_by_obj` bounded across GC cycles.
      self.event_target_by_obj.borrow_mut().remove(&existing);
    }

    let obj = rt.alloc_object_value()?;
    rt.set_prototype(obj, Some(self.event_target_prototype))?;
    self.register_event_target(EventTargetId::Node(node_id), obj)?;

    let Value::Object(handle) = obj else {
      return Err(VmError::InvariantViolation(
        "alloc_object_value returned non-object value",
      ));
    };
    self
      .node_wrappers
      .borrow_mut()
      .insert(node_id, WeakGcObject::from(handle));
    Ok(obj)
  }

  fn object_for_event_target(
    &self,
    rt: &mut VmJsRuntime,
    target: Option<EventTargetId>,
  ) -> Result<Value, VmError> {
    match target {
      None => Ok(Value::Null),
      Some(EventTargetId::Node(node_id)) => self.get_or_create_node_wrapper(rt, node_id),
      Some(id) => {
        let Some(weak) = self.obj_by_event_target.borrow().get(&id).copied() else {
          return Ok(Value::Null);
        };
        Ok(match weak.upgrade(rt.heap()) {
          Some(obj) => Value::Object(obj),
          None => Value::Null,
        })
      }
    }
  }

  fn listener_id_for_callback(&self, callback: Value) -> Option<ListenerId> {
    let Value::Object(obj) = callback else {
      // Per DOM, `addEventListener(null, ...)` is a no-op. We only accept objects as callback
      // identities.
      return None;
    };
    let id = obj.id();
    Some(ListenerId::new(
      (id.index() as u64) | ((id.generation() as u64) << 32),
    ))
  }

  fn ensure_listener_entry(
    &self,
    rt: &mut VmJsRuntime,
    listener_id: ListenerId,
    callback: Value,
  ) -> Result<(), VmError> {
    if self.listeners.borrow().contains_key(&listener_id) {
      return Ok(());
    }
    let callback_root = rt.heap_mut().add_root(callback)?;
    self.listeners.borrow_mut().insert(
      listener_id,
      ListenerEntry {
        callback,
        callback_root,
      },
    );
    Ok(())
  }

  fn remove_listener_if_unused(&self, rt: &mut VmJsRuntime, listener_id: ListenerId) {
    if self.dom.events().contains_listener_id(listener_id) {
      return;
    }
    if let Some(entry) = self.listeners.borrow_mut().remove(&listener_id) {
      rt.heap_mut().remove_root(entry.callback_root);
    }
  }

  fn register_event_object(&self, obj: Value, event: Event) -> Result<(), VmError> {
    let Value::Object(handle) = obj else {
      return Err(VmError::Unimplemented(
        "register_event_object: value is not an object",
      ));
    };
    let mut events = self.events.borrow_mut();
    events.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    events.insert(handle, box_try_new_vm(event)?);
    Ok(())
  }

  fn event_ptr_for_value(&self, value: Value) -> Option<*mut Event> {
    let Value::Object(obj) = value else {
      return None;
    };
    let events = self.events.borrow();
    let event = events.get(&obj)?;
    Some((&**event as *const Event) as *mut Event)
  }
}

fn with_event_ref<R>(
  rt: &mut VmJsRuntime,
  ctx: &DomEventsContext,
  this: Value,
  err: &str,
  f: impl FnOnce(&mut VmJsRuntime, &Event) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let Some(ptr) = ctx.event_ptr_for_value(this) else {
    return Err(rt.throw_type_error(err));
  };
  // Safety: events are owned by `DomEventsContext.events` for the lifetime of the realm.
  unsafe { f(rt, &*ptr) }
}

fn with_event_mut<R>(
  rt: &mut VmJsRuntime,
  ctx: &DomEventsContext,
  this: Value,
  err: &str,
  f: impl FnOnce(&mut VmJsRuntime, &mut Event) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let Some(ptr) = ctx.event_ptr_for_value(this) else {
    return Err(rt.throw_type_error(err));
  };
  // Safety:
  // - Events are heap-owned in `DomEventsContext.events` and never moved/dropped.
  // - During `dispatch_event`, the DOM dispatch algorithm holds an `&mut Event` and calls into the
  //   JS listener invoker. JS can then call methods like `preventDefault()` which re-borrow the same
  //   event mutably via this raw pointer. This matches the approach used by `JsDomEvents` and relies
  //   on the dispatch algorithm not accessing the event concurrently while user code runs.
  unsafe { f(rt, &mut *ptr) }
}

fn install_event_target_prototype(
  rt: &mut VmJsRuntime,
  ctx: Rc<DomEventsContext>,
  proto: Value,
) -> Result<(), VmError> {
  let add = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, args| {
      let Some(target_id) = ctx.event_target_id_for_value(this) else {
        return Err(
          rt.throw_type_error("EventTarget.addEventListener: receiver is not an EventTarget"),
        );
      };

      let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
      let type_ = value_to_dom_string(rt, type_arg)?;

      let callback = args.get(1).copied().unwrap_or(Value::Undefined);
      if matches!(callback, Value::Undefined | Value::Null) {
        return Ok(Value::Undefined);
      }
      if !rt.is_callable(callback) {
        return Err(rt.throw_type_error("EventTarget.addEventListener: callback is not callable"));
      }

      let Some(listener_id) = ctx.listener_id_for_callback(callback) else {
        return Ok(Value::Undefined);
      };

      let options_arg = args.get(2).copied().unwrap_or(Value::Undefined);
      let options = parse_add_event_listener_options(rt, options_arg)?;
      let capture = options.capture;
      let inserted = ctx
        .dom
        .events()
        .add_event_listener(target_id, &type_, listener_id, options);
      if inserted {
        if let Err(err) = ctx.ensure_listener_entry(rt, listener_id, callback) {
          let _ = ctx
            .dom
            .events()
            .remove_event_listener(target_id, &type_, listener_id, capture);
          ctx.remove_listener_if_unused(rt, listener_id);
          return Err(err);
        }
      }
      Ok(Value::Undefined)
    })?
  };

  let remove = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, args| {
      let Some(target_id) = ctx.event_target_id_for_value(this) else {
        return Err(
          rt.throw_type_error("EventTarget.removeEventListener: receiver is not an EventTarget"),
        );
      };

      let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
      let type_ = value_to_dom_string(rt, type_arg)?;

      let callback = args.get(1).copied().unwrap_or(Value::Undefined);
      if matches!(callback, Value::Undefined | Value::Null) {
        return Ok(Value::Undefined);
      }
      if !rt.is_callable(callback) {
        return Err(
          rt.throw_type_error("EventTarget.removeEventListener: callback is not callable"),
        );
      }
      let Some(listener_id) = ctx.listener_id_for_callback(callback) else {
        return Ok(Value::Undefined);
      };

      let options_arg = args.get(2).copied().unwrap_or(Value::Undefined);
      let capture = parse_capture_option(rt, options_arg)?;

      let removed = ctx
        .dom
        .events()
        .remove_event_listener(target_id, &type_, listener_id, capture);
      if removed {
        ctx.remove_listener_if_unused(rt, listener_id);
      }

      Ok(Value::Undefined)
    })?
  };

  let dispatch = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, args| {
      let Some(target_id) = ctx.event_target_id_for_value(this) else {
        return Err(
          rt.throw_type_error("EventTarget.dispatchEvent: receiver is not an EventTarget"),
        );
      };

      let event_obj = args.get(0).copied().unwrap_or(Value::Undefined);
      let Some(event_ptr) = ctx.event_ptr_for_value(event_obj) else {
        return Err(rt.throw_type_error("EventTarget.dispatchEvent: event is not an Event"));
      };

      struct JsInvoker<'a> {
        rt: &'a mut VmJsRuntime,
        ctx: &'a DomEventsContext,
        event_obj: Value,
      }

      impl EventListenerInvoker for JsInvoker<'_> {
        fn invoke(
          &mut self,
          listener_id: ListenerId,
          event: &mut Event,
        ) -> std::result::Result<(), DomError> {
          let entry = self
            .ctx
            .listeners
            .borrow()
            .get(&listener_id)
            .copied()
            .ok_or_else(|| {
              DomError::new(format!("missing JS callback for listener {listener_id:?}"))
            })?;
          let this_arg = self
            .ctx
            .object_for_event_target(self.rt, event.current_target)
            .map_err(|e| DomError::new(e.to_string()))?;
          let result = self
            .rt
            .call_function(entry.callback, this_arg, &[self.event_obj])
            .map(|_| ())
            .map_err(|e| DomError::new(e.to_string()));
          // Even if the callback throws, keep listener roots in sync with the registry.
          self.ctx.remove_listener_if_unused(self.rt, listener_id);
          result
        }
      }

      let mut invoker = JsInvoker {
        rt,
        ctx: ctx.as_ref(),
        event_obj,
      };

      let ok = unsafe {
        dispatch_event(
          target_id,
          &mut *event_ptr,
          &ctx.dom,
          ctx.dom.events(),
          &mut invoker,
        )
      }
      .map_err(|err| rt.throw_type_error(&format!("dispatchEvent failed: {err}")))?;
      Ok(Value::Bool(ok))
    })?
  };

  let add_key = prop_key_str(rt, "addEventListener")?;
  let remove_key = prop_key_str(rt, "removeEventListener")?;
  let dispatch_key = prop_key_str(rt, "dispatchEvent")?;
  rt.define_data_property(proto, add_key, add, false)?;
  rt.define_data_property(proto, remove_key, remove, false)?;
  rt.define_data_property(proto, dispatch_key, dispatch, false)?;
  Ok(())
}

fn install_event_prototype(
  rt: &mut VmJsRuntime,
  ctx: Rc<DomEventsContext>,
  proto: Value,
) -> Result<(), VmError> {
  let stop = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      with_event_mut(
        rt,
        &ctx,
        this,
        "Event.stopPropagation: receiver is not an Event",
        |_rt, event| {
          event.stop_propagation();
          Ok(Value::Undefined)
        },
      )
    })?
  };

  let stop_immediate = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      with_event_mut(
        rt,
        &ctx,
        this,
        "Event.stopImmediatePropagation: receiver is not an Event",
        |_rt, event| {
          event.stop_immediate_propagation();
          Ok(Value::Undefined)
        },
      )
    })?
  };

  let prevent = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      with_event_mut(
        rt,
        &ctx,
        this,
        "Event.preventDefault: receiver is not an Event",
        |_rt, event| {
          event.prevent_default();
          Ok(Value::Undefined)
        },
      )
    })?
  };

  let composed_path = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      with_event_ref(
        rt,
        &ctx,
        this,
        "Event.composedPath: receiver is not an Event",
        |rt, event| {
          let targets = event.composed_path();
          let arr = rt.alloc_array()?;
          let arr_root = rt.heap_mut().add_root(arr)?;
          let res = (|| {
            for (idx, target) in targets.iter().copied().enumerate() {
              let key = rt.property_key_from_u32(idx as u32)?;
              let value = ctx.object_for_event_target(rt, Some(target))?;
              rt.define_data_property(arr, key, value, true)?;
            }
            Ok(arr)
          })();
          rt.heap_mut().remove_root(arr_root);
          res
        },
      )
    })?
  };

  let get_type = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      let ty = with_event_ref(
        rt,
        &ctx,
        this,
        "Event.type: receiver is not an Event",
        |_rt, event| Ok(event.type_.clone()),
      )?;
      rt.alloc_string_value(&ty)
    })?
  };

  let get_bubbles = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      with_event_ref(
        rt,
        &ctx,
        this,
        "Event.bubbles: receiver is not an Event",
        |_rt, event| Ok(Value::Bool(event.bubbles)),
      )
    })?
  };

  let get_cancelable = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      with_event_ref(
        rt,
        &ctx,
        this,
        "Event.cancelable: receiver is not an Event",
        |_rt, event| Ok(Value::Bool(event.cancelable)),
      )
    })?
  };

  let get_default_prevented = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      with_event_ref(
        rt,
        &ctx,
        this,
        "Event.defaultPrevented: receiver is not an Event",
        |_rt, event| Ok(Value::Bool(event.default_prevented)),
      )
    })?
  };

  let get_event_phase = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      with_event_ref(
        rt,
        &ctx,
        this,
        "Event.eventPhase: receiver is not an Event",
        |_rt, event| {
          let phase = match event.event_phase {
            EventPhase::None => 0,
            EventPhase::Capturing => 1,
            EventPhase::AtTarget => 2,
            EventPhase::Bubbling => 3,
          };
          Ok(Value::Number(phase as f64))
        },
      )
    })?
  };

  let get_target = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      with_event_ref(
        rt,
        &ctx,
        this,
        "Event.target: receiver is not an Event",
        |rt, event| ctx.object_for_event_target(rt, event.target),
      )
    })?
  };

  let get_current_target = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      with_event_ref(
        rt,
        &ctx,
        this,
        "Event.currentTarget: receiver is not an Event",
        |rt, event| ctx.object_for_event_target(rt, event.current_target),
      )
    })?
  };

  let stop_key = prop_key_str(rt, "stopPropagation")?;
  let stop_immediate_key = prop_key_str(rt, "stopImmediatePropagation")?;
  let prevent_key = prop_key_str(rt, "preventDefault")?;
  let composed_path_key = prop_key_str(rt, "composedPath")?;
  rt.define_data_property(proto, stop_key, stop, false)?;
  rt.define_data_property(proto, stop_immediate_key, stop_immediate, false)?;
  rt.define_data_property(proto, prevent_key, prevent, false)?;
  rt.define_data_property(proto, composed_path_key, composed_path, false)?;

  define_getter(rt, proto, "type", get_type)?;
  define_getter(rt, proto, "bubbles", get_bubbles)?;
  define_getter(rt, proto, "cancelable", get_cancelable)?;
  define_getter(rt, proto, "defaultPrevented", get_default_prevented)?;
  define_getter(rt, proto, "eventPhase", get_event_phase)?;
  define_getter(rt, proto, "target", get_target)?;
  define_getter(rt, proto, "currentTarget", get_current_target)?;

  Ok(())
}

fn install_event_constructor(
  rt: &mut VmJsRuntime,
  ctx: Rc<DomEventsContext>,
  event_proto: Value,
) -> Result<Value, VmError> {
  let ctor = rt.alloc_function_value(move |rt, _this, args| {
    let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
    let type_ = value_to_dom_string(rt, type_arg)?;

    let init_arg = args.get(1).copied().unwrap_or(Value::Undefined);
    let init = parse_event_init(rt, init_arg)?;

    let event = Event::new(type_, init);
    let obj = rt.alloc_object_value()?;
    rt.set_prototype(obj, Some(event_proto))?;
    ctx.register_event_object(obj, event)?;
    Ok(obj)
  })?;
  Ok(ctor)
}

fn define_getter(rt: &mut VmJsRuntime, obj: Value, name: &str, get: Value) -> Result<(), VmError> {
  let key = prop_key_str(rt, name)?;
  rt.define_accessor_property(obj, key, get, Value::Undefined, false)
}

fn parse_add_event_listener_options(
  rt: &mut VmJsRuntime,
  value: Value,
) -> Result<AddEventListenerOptions, VmError> {
  if matches!(value, Value::Undefined) {
    return Ok(AddEventListenerOptions::default());
  }
  if let Value::Bool(capture) = value {
    return Ok(AddEventListenerOptions {
      capture,
      ..Default::default()
    });
  }
  let Value::Object(_) = value else {
    // Spec does WebIDL conversions; for MVP treat non-object values like default options.
    return Ok(AddEventListenerOptions::default());
  };
  Ok(AddEventListenerOptions {
    capture: get_bool_prop(rt, value, "capture")?,
    once: get_bool_prop(rt, value, "once")?,
    passive: get_bool_prop(rt, value, "passive")?,
  })
}

fn parse_capture_option(rt: &mut VmJsRuntime, value: Value) -> Result<bool, VmError> {
  if matches!(value, Value::Undefined) {
    return Ok(false);
  }
  if let Value::Bool(capture) = value {
    return Ok(capture);
  }
  let Value::Object(_) = value else {
    return Ok(false);
  };
  get_bool_prop(rt, value, "capture")
}

fn parse_event_init(rt: &mut VmJsRuntime, value: Value) -> Result<EventInit, VmError> {
  if matches!(value, Value::Undefined) {
    return Ok(EventInit::default());
  }
  let Value::Object(_) = value else {
    return Ok(EventInit::default());
  };
  Ok(EventInit {
    bubbles: get_bool_prop(rt, value, "bubbles")?,
    cancelable: get_bool_prop(rt, value, "cancelable")?,
    composed: get_bool_prop(rt, value, "composed")?,
  })
}

fn get_bool_prop(rt: &mut VmJsRuntime, obj: Value, name: &str) -> Result<bool, VmError> {
  let key = prop_key_str(rt, name)?;
  let val = rt.get(obj, key)?;
  rt.to_boolean(val)
}

fn value_to_dom_string(rt: &mut VmJsRuntime, value: Value) -> Result<String, VmError> {
  let value = rt.to_string(value)?;
  rt.string_to_utf8_lossy(value)
}

fn prop_key_str(rt: &mut VmJsRuntime, name: &str) -> Result<PropertyKey, VmError> {
  let v = rt.alloc_string_value(name)?;
  let Value::String(s) = v else {
    return Err(rt.throw_type_error("failed to allocate string key"));
  };
  Ok(PropertyKey::String(s))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom::{DomNode, DomNodeType};
  use selectors::context::QuirksMode;
  use std::cell::RefCell;
  use std::rc::Rc;

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

  fn make_doc_body_target() -> (dom2::Document, dom2::NodeId, dom2::NodeId) {
    // Document → <body> → <div>
    let root = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
        scripting_enabled: true,
        is_html_document: true,
      },
      children: vec![element("body", vec![element("div", vec![])])],
    };
    let doc = dom2::Document::from_renderer_dom(&root);
    fn first_element_child(doc: &dom2::Document, parent: dom2::NodeId) -> dom2::NodeId {
      doc
        .node(parent)
        .children
        .iter()
        .copied()
        .find(|&child| matches!(doc.node(child).kind, dom2::NodeKind::Element { .. }))
        .expect("expected element child")
    }

    let root_id = doc.root();
    // Document imports may materialize a doctype node; skip to the first element.
    let body = first_element_child(&doc, root_id);
    let target = first_element_child(&doc, body);
    (doc, body, target)
  }

  fn key(rt: &mut VmJsRuntime, name: &str) -> PropertyKey {
    prop_key_str(rt, name).expect("alloc property key")
  }

  fn bool_value(v: Value) -> bool {
    match v {
      Value::Bool(b) => b,
      other => panic!("expected bool, got {other:?}"),
    }
  }

  #[test]
  fn node_wrapper_identity_is_stable_for_same_node() {
    let (dom, body_id, _target_id) = make_doc_body_target();
    let mut rt = VmJsRuntime::new();
    let realm = DomEventsRealm::new(&mut rt, dom).expect("install realm");

    let a = realm
      .create_node_wrapper(&mut rt, body_id)
      .expect("body wrapper");
    let b = realm
      .create_node_wrapper(&mut rt, body_id)
      .expect("body wrapper (second time)");

    assert_eq!(
      a, b,
      "wrapper identity should be stable for the same NodeId"
    );
  }

  #[test]
  fn capture_and_bubble_listener_order_window_document_body_target() {
    let (dom, body_id, target_id) = make_doc_body_target();
    let mut rt = VmJsRuntime::new();
    let realm = DomEventsRealm::new(&mut rt, dom).expect("install realm");

    let body = realm
      .create_node_wrapper(&mut rt, body_id)
      .expect("body wrapper");
    let target = realm
      .create_node_wrapper(&mut rt, target_id)
      .expect("target wrapper");

    let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

    let add_listener = |rt: &mut VmJsRuntime,
                        target_obj: Value,
                        label: &'static str,
                        capture: bool,
                        log: Rc<RefCell<Vec<&'static str>>>| {
      let cb = rt
        .alloc_function_value(move |_rt, _this, _args| {
          log.borrow_mut().push(label);
          Ok(Value::Undefined)
        })
        .expect("callback fn");
      let add_key = key(rt, "addEventListener");
      let add = rt.get(target_obj, add_key).expect("get addEventListener");
      let type_ = rt.alloc_string_value("x").expect("type string");
      let mut args = vec![type_, cb];
      if capture {
        args.push(Value::Bool(true));
      }
      rt.call_function(add, target_obj, &args)
        .expect("addEventListener call");
    };

    add_listener(&mut rt, realm.window, "window_capture", true, log.clone());
    add_listener(
      &mut rt,
      realm.document,
      "document_capture",
      true,
      log.clone(),
    );
    add_listener(&mut rt, body, "body_capture", true, log.clone());
    add_listener(&mut rt, target, "target_capture", true, log.clone());

    add_listener(&mut rt, target, "target_bubble", false, log.clone());
    add_listener(&mut rt, body, "body_bubble", false, log.clone());
    add_listener(
      &mut rt,
      realm.document,
      "document_bubble",
      false,
      log.clone(),
    );
    add_listener(&mut rt, realm.window, "window_bubble", false, log.clone());

    // Create a bubbling event.
    let init = rt.alloc_object_value().expect("init");
    let bubbles_key = key(&mut rt, "bubbles");
    rt.define_data_property(init, bubbles_key, Value::Bool(true), true)
      .expect("init.bubbles");
    let type_ = rt.alloc_string_value("x").unwrap();
    let event = rt
      .call_function(realm.event_constructor, Value::Undefined, &[type_, init])
      .expect("new Event");

    let dispatch_key = key(&mut rt, "dispatchEvent");
    let dispatch = rt.get(target, dispatch_key).expect("get dispatchEvent");
    let res = rt
      .call_function(dispatch, target, &[event])
      .expect("dispatchEvent");
    assert!(bool_value(res));

    assert_eq!(
      log.borrow().as_slice(),
      &[
        "window_capture",
        "document_capture",
        "body_capture",
        "target_capture",
        "target_bubble",
        "body_bubble",
        "document_bubble",
        "window_bubble",
      ]
    );
  }

  #[test]
  fn once_option_removes_after_first_dispatch() {
    let (dom, _body_id, target_id) = make_doc_body_target();
    let mut rt = VmJsRuntime::new();
    let realm = DomEventsRealm::new(&mut rt, dom).expect("install realm");

    let target = realm
      .create_node_wrapper(&mut rt, target_id)
      .expect("target wrapper");

    let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
    let cb_log = log.clone();
    let cb = rt
      .alloc_function_value(move |_rt, _this, _args| {
        cb_log.borrow_mut().push("once");
        Ok(Value::Undefined)
      })
      .expect("callback fn");

    let options = rt.alloc_object_value().expect("options");
    let once_key = key(&mut rt, "once");
    rt.define_data_property(options, once_key, Value::Bool(true), true)
      .expect("options.once");

    let add_key = key(&mut rt, "addEventListener");
    let add = rt.get(target, add_key).expect("get addEventListener");
    let type_ = rt.alloc_string_value("x").unwrap();
    rt.call_function(add, target, &[type_, cb, options])
      .expect("addEventListener");

    for _ in 0..2 {
      let type_ = rt.alloc_string_value("x").unwrap();
      let event = rt
        .call_function(realm.event_constructor, Value::Undefined, &[type_])
        .expect("new Event");
      let dispatch_key = key(&mut rt, "dispatchEvent");
      let dispatch = rt.get(target, dispatch_key).expect("dispatchEvent getter");
      rt.call_function(dispatch, target, &[event])
        .expect("dispatchEvent");
    }

    assert_eq!(log.borrow().as_slice(), &["once"]);
  }

  #[test]
  fn passive_option_ignores_prevent_default() {
    let (dom, _body_id, target_id) = make_doc_body_target();
    let mut rt = VmJsRuntime::new();
    let realm = DomEventsRealm::new(&mut rt, dom).expect("install realm");

    let target = realm
      .create_node_wrapper(&mut rt, target_id)
      .expect("target wrapper");

    let cb = rt
      .alloc_function_value(move |rt, _this, args| {
        let event = args.get(0).copied().unwrap_or(Value::Undefined);
        let prevent_key = key(rt, "preventDefault");
        let prevent = rt.get(event, prevent_key)?;
        rt.call_function(prevent, event, &[])?;
        Ok(Value::Undefined)
      })
      .expect("callback fn");

    let options = rt.alloc_object_value().expect("options");
    let passive_key = key(&mut rt, "passive");
    rt.define_data_property(options, passive_key, Value::Bool(true), true)
      .expect("options.passive");

    let add_key = key(&mut rt, "addEventListener");
    let add = rt.get(target, add_key).expect("get addEventListener");
    let type_ = rt.alloc_string_value("x").unwrap();
    rt.call_function(add, target, &[type_, cb, options])
      .expect("addEventListener");

    let init = rt.alloc_object_value().expect("init");
    let cancelable_key = key(&mut rt, "cancelable");
    rt.define_data_property(init, cancelable_key, Value::Bool(true), true)
      .expect("init.cancelable");
    let type_ = rt.alloc_string_value("x").unwrap();
    let event = rt
      .call_function(realm.event_constructor, Value::Undefined, &[type_, init])
      .expect("new Event");

    let dispatch_key = key(&mut rt, "dispatchEvent");
    let dispatch = rt.get(target, dispatch_key).expect("get dispatchEvent");
    let res = rt
      .call_function(dispatch, target, &[event])
      .expect("dispatchEvent");
    assert!(bool_value(res));

    let default_prevented_key = key(&mut rt, "defaultPrevented");
    let default_prevented = rt
      .get(event, default_prevented_key)
      .expect("get defaultPrevented");
    assert!(!bool_value(default_prevented));
  }

  #[test]
  fn stop_immediate_propagation_stops_later_listeners_on_same_target() {
    let (dom, _body_id, target_id) = make_doc_body_target();
    let mut rt = VmJsRuntime::new();
    let realm = DomEventsRealm::new(&mut rt, dom).expect("install realm");

    let target = realm
      .create_node_wrapper(&mut rt, target_id)
      .expect("target wrapper");

    let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

    let log_first = log.clone();
    let first = rt
      .alloc_function_value(move |rt, _this, args| {
        log_first.borrow_mut().push("first");
        let event = args.get(0).copied().unwrap_or(Value::Undefined);
        let stop_key = key(rt, "stopImmediatePropagation");
        let stop = rt.get(event, stop_key)?;
        rt.call_function(stop, event, &[])?;
        Ok(Value::Undefined)
      })
      .expect("first fn");

    let log_second = log.clone();
    let second = rt
      .alloc_function_value(move |_rt, _this, _args| {
        log_second.borrow_mut().push("second");
        Ok(Value::Undefined)
      })
      .expect("second fn");

    let add_key = key(&mut rt, "addEventListener");
    let add = rt.get(target, add_key).expect("get addEventListener");
    let type_ = rt.alloc_string_value("x").unwrap();
    rt.call_function(add, target, &[type_, first])
      .expect("add first");
    let add_key = key(&mut rt, "addEventListener");
    let add = rt.get(target, add_key).expect("get addEventListener");
    let type_ = rt.alloc_string_value("x").unwrap();
    rt.call_function(add, target, &[type_, second])
      .expect("add second");

    let init = rt.alloc_object_value().expect("init");
    let bubbles_key = key(&mut rt, "bubbles");
    rt.define_data_property(init, bubbles_key, Value::Bool(true), true)
      .expect("init.bubbles");
    let type_ = rt.alloc_string_value("x").unwrap();
    let event = rt
      .call_function(realm.event_constructor, Value::Undefined, &[type_, init])
      .expect("new Event");

    let dispatch_key = key(&mut rt, "dispatchEvent");
    let dispatch = rt.get(target, dispatch_key).expect("get dispatchEvent");
    rt.call_function(dispatch, target, &[event])
      .expect("dispatchEvent");

    assert_eq!(log.borrow().as_slice(), &["first"]);
  }
}
