use crate::dom2;
use crate::js::webidl::{JsRuntime as _, VmJsRuntime, WebIdlJsRuntime as _};
use crate::web::events::{
  dispatch_event, AddEventListenerOptions, DomError, Event, EventInit, EventListenerInvoker,
  EventPhase, EventTargetId, ListenerId,
};
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::rc::Rc;
use vm_js::{GcObject, PropertyKey, RootId, Value, VmError};

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
    let event_prototype = rt.alloc_object_value()?;
    let ctx = Rc::new(DomEventsContext {
      dom,
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
    rt.set_prototype(window, Some(event_target_prototype))?;
    ctx.register_event_target(EventTargetId::Window, window)?;

    let document = rt.alloc_object_value()?;
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
    let obj = rt.alloc_object_value()?;
    rt.set_prototype(obj, Some(self.event_target_prototype))?;
    self
      .ctx
      .register_event_target(EventTargetId::Node(node_id), obj)?;
    Ok(obj)
  }

  pub fn ctx(&self) -> &DomEventsContext {
    &self.ctx
  }
}

pub struct DomEventsContext {
  dom: dom2::Document,
  listeners: RefCell<FxHashMap<ListenerId, ListenerEntry>>,
  events: RefCell<FxHashMap<GcObject, Box<Event>>>,
  event_target_by_obj: RefCell<FxHashMap<GcObject, EventTargetId>>,
  obj_by_event_target: RefCell<FxHashMap<EventTargetId, Value>>,
}

impl DomEventsContext {
  fn register_event_target(&self, target: EventTargetId, obj: Value) -> Result<(), VmError> {
    let Value::Object(handle) = obj else {
      return Err(VmError::Unimplemented(
        "register_event_target: value is not an object",
      ));
    };
    self.event_target_by_obj.borrow_mut().insert(handle, target);
    self.obj_by_event_target.borrow_mut().insert(target, obj);
    Ok(())
  }

  fn event_target_id_for_value(&self, value: Value) -> Option<EventTargetId> {
    let Value::Object(obj) = value else {
      return None;
    };
    self.event_target_by_obj.borrow().get(&obj).copied()
  }

  fn object_for_event_target(&self, target: Option<EventTargetId>) -> Value {
    match target {
      None => Value::Null,
      Some(id) => self.obj_by_event_target.borrow().get(&id).copied().unwrap_or(Value::Null),
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
    self.events.borrow_mut().insert(handle, Box::new(event));
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
  f: impl FnOnce(&Event) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let Some(ptr) = ctx.event_ptr_for_value(this) else {
    return Err(rt.throw_type_error(err));
  };
  // Safety: events are owned by `DomEventsContext.events` for the lifetime of the realm.
  unsafe { f(&*ptr) }
}

fn with_event_mut<R>(
  rt: &mut VmJsRuntime,
  ctx: &DomEventsContext,
  this: Value,
  err: &str,
  f: impl FnOnce(&mut Event) -> Result<R, VmError>,
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
  unsafe { f(&mut *ptr) }
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
        return Err(rt.throw_type_error(
          "EventTarget.addEventListener: receiver is not an EventTarget",
        ));
      };

      let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
      let type_ = value_to_dom_string(rt, type_arg)?;

      let callback = args.get(1).copied().unwrap_or(Value::Undefined);
      if matches!(callback, Value::Undefined | Value::Null) {
        return Ok(Value::Undefined);
      }
      if !rt.is_callable(callback) {
        return Err(rt.throw_type_error(
          "EventTarget.addEventListener: callback is not callable",
        ));
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
        return Err(rt.throw_type_error(
          "EventTarget.removeEventListener: receiver is not an EventTarget",
        ));
      };

      let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
      let type_ = value_to_dom_string(rt, type_arg)?;

      let callback = args.get(1).copied().unwrap_or(Value::Undefined);
      if matches!(callback, Value::Undefined | Value::Null) {
        return Ok(Value::Undefined);
      }
      if !rt.is_callable(callback) {
        return Err(rt.throw_type_error(
          "EventTarget.removeEventListener: callback is not callable",
        ));
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
        return Err(rt.throw_type_error(
          "EventTarget.dispatchEvent: receiver is not an EventTarget",
        ));
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
        fn invoke(&mut self, listener_id: ListenerId, event: &mut Event) -> std::result::Result<(), DomError> {
          let entry = self
            .ctx
            .listeners
            .borrow()
            .get(&listener_id)
            .copied()
            .ok_or_else(|| DomError::new(format!("missing JS callback for listener {listener_id:?}")))?;
          let this_arg = self.ctx.object_for_event_target(event.current_target);
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
        |event| {
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
        |event| {
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
        |event| {
          event.prevent_default();
          Ok(Value::Undefined)
        },
      )
    })?
  };

  let get_type = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      let ty = with_event_ref(rt, &ctx, this, "Event.type: receiver is not an Event", |event| {
        Ok(event.type_.clone())
      })?;
      rt.alloc_string_value(&ty)
    })?
  };

  let get_bubbles = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      with_event_ref(rt, &ctx, this, "Event.bubbles: receiver is not an Event", |event| {
        Ok(Value::Bool(event.bubbles))
      })
    })?
  };

  let get_cancelable = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      with_event_ref(rt, &ctx, this, "Event.cancelable: receiver is not an Event", |event| {
        Ok(Value::Bool(event.cancelable))
      })
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
        |event| Ok(Value::Bool(event.default_prevented)),
      )
    })?
  };

  let get_event_phase = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      with_event_ref(rt, &ctx, this, "Event.eventPhase: receiver is not an Event", |event| {
        let phase = match event.event_phase {
          EventPhase::None => 0,
          EventPhase::Capturing => 1,
          EventPhase::AtTarget => 2,
          EventPhase::Bubbling => 3,
        };
        Ok(Value::Number(phase as f64))
      })
    })?
  };

  let get_target = {
    let ctx = ctx.clone();
    rt.alloc_function_value(move |rt, this, _args| {
      with_event_ref(rt, &ctx, this, "Event.target: receiver is not an Event", |event| {
        Ok(ctx.object_for_event_target(event.target))
      })
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
        |event| Ok(ctx.object_for_event_target(event.current_target)),
      )
    })?
  };

  let stop_key = prop_key_str(rt, "stopPropagation")?;
  let stop_immediate_key = prop_key_str(rt, "stopImmediatePropagation")?;
  let prevent_key = prop_key_str(rt, "preventDefault")?;
  rt.define_data_property(proto, stop_key, stop, false)?;
  rt.define_data_property(proto, stop_immediate_key, stop_immediate, false)?;
  rt.define_data_property(proto, prevent_key, prevent, false)?;

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
  let Value::String(s) = value else {
    return Err(rt.throw_type_error("expected string"));
  };
  Ok(rt.heap().get_string(s)?.to_utf8_lossy())
}

fn prop_key_str(rt: &mut VmJsRuntime, name: &str) -> Result<PropertyKey, VmError> {
  let v = rt.alloc_string_value(name)?;
  let Value::String(s) = v else {
    return Err(rt.throw_type_error("failed to allocate string key"));
  };
  Ok(PropertyKey::String(s))
}
