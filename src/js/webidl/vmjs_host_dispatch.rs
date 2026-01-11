use crate::js::runtime::{current_event_loop_mut, with_event_loop};
use crate::js::window_timers::{
  vm_error_to_event_loop_error, VmJsEventLoopHooks, QUEUE_MICROTASK_NOT_CALLABLE_ERROR,
  QUEUE_MICROTASK_STRING_HANDLER_ERROR, SET_INTERVAL_NOT_CALLABLE_ERROR, SET_INTERVAL_STRING_HANDLER_ERROR,
  SET_TIMEOUT_NOT_CALLABLE_ERROR, SET_TIMEOUT_STRING_HANDLER_ERROR,
};
use crate::js::{TimerId, Url, UrlLimits, UrlSearchParams, WindowRealmHost};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::Duration;
use vm_js::{
  GcObject, NativeFunctionId, PropertyDescriptor, PropertyKey, PropertyKind, RootId, Scope, Value, Vm,
  VmError, VmHost, VmHostHooks, WeakGcObject,
};
use webidl_vm_js::WebIdlBindingsHost;

const URL_INVALID_ERROR: &str = "Invalid URL";
const URLSP_ITER_VALUES_SLOT: &str = "__fastrender_urlsp_iter_values";
const URLSP_ITER_INDEX_SLOT: &str = "__fastrender_urlsp_iter_index";
const URLSP_ITER_LEN_SLOT: &str = "__fastrender_urlsp_iter_len";

#[derive(Debug, Clone, Copy)]
struct RootedCallback {
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
}

#[derive(Debug, Default)]
struct EventTargetState {
  listeners: Vec<EventListenerEntry>,
}

fn is_callable(scope: &Scope<'_>, value: Value) -> bool {
  scope.heap().is_callable(value).unwrap_or(false)
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
  scope.define_property(result_obj, value_key, data_property(value, true, true, true))?;
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

fn data_property(value: Value, writable: bool, enumerable: bool, configurable: bool) -> PropertyDescriptor {
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

fn js_string_to_rust_string(scope: &Scope<'_>, value: Value) -> Result<String, VmError> {
  let Value::String(s) = value else {
    return Err(VmError::TypeError("expected string"));
  };
  Ok(scope.heap().get_string(s)?.to_utf8_lossy())
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

pub struct VmJsWebIdlBindingsHostDispatch<Host: WindowRealmHost + 'static> {
  global: Option<GcObject>,
  limits: UrlLimits,
  urls: HashMap<WeakGcObject, Url>,
  params: HashMap<WeakGcObject, UrlSearchParams>,
  event_targets: HashMap<WeakGcObject, EventTargetState>,
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
      event_targets: HashMap::new(),
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
      event_targets: HashMap::new(),
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
    self.event_targets.clear();
    self.timer_registry.borrow_mut().clear();
    self.urlsp_iterator_next_call = None;
    self.urlsp_iterator_iterator_call = None;
    self.last_gc_runs = 0;
  }

  fn maybe_sweep(&mut self, heap: &mut vm_js::Heap) {
    let gc_runs = heap.gc_runs();
    if gc_runs == self.last_gc_runs {
      return;
    }
    self.last_gc_runs = gc_runs;

    self.urls.retain(|k, _| k.upgrade(heap).is_some());
    self.params.retain(|k, _| k.upgrade(heap).is_some());

    // When an EventTarget wrapper dies, drop its listener roots.
    self.event_targets.retain(|k, state| {
      if k.upgrade(heap).is_some() {
        true
      } else {
        for listener in &state.listeners {
          heap.remove_root(listener.callback.root);
        }
        false
      }
    });
  }

  fn require_receiver_object(receiver: Option<Value>) -> Result<GcObject, VmError> {
    let Some(Value::Object(obj)) = receiver else {
      return Err(VmError::TypeError("Illegal invocation"));
    };
    Ok(obj)
  }

  fn require_url(&self, receiver: Option<Value>) -> Result<Url, VmError> {
    let obj = Self::require_receiver_object(receiver)?;
    self
      .urls
      .get(&WeakGcObject::from(obj))
      .cloned()
      .ok_or(VmError::TypeError("Illegal invocation"))
  }

  fn require_params(&self, receiver: Option<Value>) -> Result<UrlSearchParams, VmError> {
    let obj = Self::require_receiver_object(receiver)?;
    self
      .params
      .get(&WeakGcObject::from(obj))
      .cloned()
      .ok_or(VmError::TypeError("Illegal invocation"))
  }

  fn url_proto_from_global(&self, vm: &mut Vm, scope: &mut Scope<'_>) -> Result<GcObject, VmError> {
    let global = self
      .global
      .ok_or(VmError::Unimplemented("WebIDL host missing global object"))?;

    let ctor_key = key_from_str(scope, "URL")?;
    let ctor = vm.get(scope, global, ctor_key)?;
    scope.push_root(ctor)?;
    let Value::Object(ctor_obj) = ctor else {
      return Err(VmError::TypeError("globalThis.URL is not an object"));
    };

    let proto_key = key_from_str(scope, "prototype")?;
    let proto = vm.get(scope, ctor_obj, proto_key)?;
    scope.push_root(proto)?;
    let Value::Object(proto_obj) = proto else {
      return Err(VmError::TypeError("URL.prototype is not an object"));
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

  fn set_timeout_impl(&mut self, scope: &mut Scope<'_>, args: &[Value]) -> Result<Value, VmError> {
    let handler = args.get(0).copied().unwrap_or(Value::Undefined);
    if matches!(handler, Value::String(_)) {
      return Err(VmError::TypeError(SET_TIMEOUT_STRING_HANDLER_ERROR));
    }
    if !is_callable(scope, handler) {
      return Err(VmError::TypeError(SET_TIMEOUT_NOT_CALLABLE_ERROR));
    }
    let delay_ms = normalize_delay_ms(args.get(1).copied().unwrap_or(Value::Number(0.0)));

    let Some(event_loop) = current_event_loop_mut::<Host>() else {
      return Err(VmError::TypeError("setTimeout called without an active EventLoop"));
    };

    let callback_root = scope.heap_mut().add_root(handler)?;
    let mut arg_roots: Vec<RootId> = Vec::new();
    for arg in args.iter().copied().skip(2) {
      arg_roots.push(scope.heap_mut().add_root(arg)?);
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

        let RootedCallback { value: callback, root: cb_root } = entry.callback;
        let arg_roots = entry.args;

        let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
        let (vm_host, window_realm) = host.vm_host_and_window_realm();
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

        let result: crate::error::Result<()> = with_event_loop(event_loop, || {
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
          call_result
            .map_err(|err| vm_error_to_event_loop_error(heap, err))
            .map(|_| ())
        });

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
      .map_err(|_| VmError::TypeError("setTimeout failed to schedule timer"))?;

    id_cell.set(id);
    self.timer_registry.borrow_mut().insert(id, entry);
    Ok(Value::Number(id as f64))
  }

  fn set_interval_impl(&mut self, scope: &mut Scope<'_>, args: &[Value]) -> Result<Value, VmError> {
    let handler = args.get(0).copied().unwrap_or(Value::Undefined);
    if matches!(handler, Value::String(_)) {
      return Err(VmError::TypeError(SET_INTERVAL_STRING_HANDLER_ERROR));
    }
    if !is_callable(scope, handler) {
      return Err(VmError::TypeError(SET_INTERVAL_NOT_CALLABLE_ERROR));
    }
    let interval_ms = normalize_delay_ms(args.get(1).copied().unwrap_or(Value::Number(0.0)));

    let Some(event_loop) = current_event_loop_mut::<Host>() else {
      return Err(VmError::TypeError("setInterval called without an active EventLoop"));
    };

    let callback_root = scope.heap_mut().add_root(handler)?;
    let mut arg_roots: Vec<RootId> = Vec::new();
    for arg in args.iter().copied().skip(2) {
      arg_roots.push(scope.heap_mut().add_root(arg)?);
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
      .set_interval(Duration::from_millis(interval_ms), move |host, event_loop| {
        let id = id_cell_for_cb.get();

        let (callback, arg_roots) = {
          let map = registry.borrow();
          let Some(entry) = map.get(&id) else {
            return Ok(());
          };
          (entry.callback.value, entry.args.clone())
        };

        let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
        let (vm_host, window_realm) = host.vm_host_and_window_realm();
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

        let result: crate::error::Result<()> = with_event_loop(event_loop, || {
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
          call_result
            .map_err(|err| vm_error_to_event_loop_error(heap, err))
            .map(|_| ())
        });

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
      })
      .map_err(|_| VmError::TypeError("setInterval failed to schedule timer"))?;

    id_cell.set(id);
    self.timer_registry.borrow_mut().insert(id, entry);
    Ok(Value::Number(id as f64))
  }

  fn clear_timer_impl(&mut self, scope: &mut Scope<'_>, id: TimerId, is_interval: bool) -> Result<Value, VmError> {
    let Some(event_loop) = current_event_loop_mut::<Host>() else {
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

  fn queue_microtask_impl(&mut self, scope: &mut Scope<'_>, callback: Value) -> Result<Value, VmError> {
    if matches!(callback, Value::String(_)) {
      return Err(VmError::TypeError(QUEUE_MICROTASK_STRING_HANDLER_ERROR));
    }
    if !is_callable(scope, callback) {
      return Err(VmError::TypeError(QUEUE_MICROTASK_NOT_CALLABLE_ERROR));
    }

    let Some(event_loop) = current_event_loop_mut::<Host>() else {
      return Err(VmError::TypeError(
        "queueMicrotask called without an active EventLoop",
      ));
    };

    let root = scope.heap_mut().add_root(callback)?;
    event_loop
      .queue_microtask(move |host, event_loop| {
        let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
        let (vm_host, window_realm) = host.vm_host_and_window_realm();
        window_realm.reset_interrupt();
        let budget = window_realm.vm_budget_now();
        let global = window_realm.global_object();

        let (vm, heap) = window_realm.vm_and_heap_mut();
        let value = heap.get_root(root).unwrap_or(Value::Undefined);

        let result: crate::error::Result<()> = with_event_loop(event_loop, || {
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
          call_result
            .map_err(|err| vm_error_to_event_loop_error(heap, err))
            .map(|_| ())
        });

        let finish_err = hooks.finish(&mut *heap);
        heap.remove_root(root);

        if let Some(err) = finish_err {
          return Err(err);
        }
        result
      })
      .map_err(|_| VmError::TypeError("queueMicrotask failed to enqueue microtask"))?;

    Ok(Value::Undefined)
  }
}

impl<Host: WindowRealmHost + 'static> WebIdlBindingsHost for VmJsWebIdlBindingsHostDispatch<Host> {
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
    self.maybe_sweep(scope.heap_mut());

    match (interface, operation, overload) {
      ("EventTarget", "constructor", 0) => {
        let obj = Self::require_receiver_object(receiver)?;
        self.event_targets.entry(WeakGcObject::from(obj)).or_default();
        Ok(Value::Undefined)
      }
      ("EventTarget", "addEventListener", 0) => {
        let obj = Self::require_receiver_object(receiver)?;
        let Some(Value::String(_)) = args.get(0).copied() else {
          return Err(VmError::TypeError("EventTarget.addEventListener: missing type"));
        };
        let event_type = js_string_to_rust_string(scope, args[0])?;

        let callback = args.get(1).copied().unwrap_or(Value::Undefined);
        if matches!(callback, Value::Null | Value::Undefined) {
          return Ok(Value::Undefined);
        }
        if matches!(callback, Value::String(_)) || !is_callable(scope, callback) {
          return Err(VmError::TypeError("EventTarget listener is not callable"));
        }

        let capture = get_capture_option(scope, args.get(2).copied().unwrap_or(Value::Undefined))?;

        let state = self.event_targets.entry(WeakGcObject::from(obj)).or_default();
        if state.listeners.iter().any(|l| {
          l.event_type == event_type && l.callback.value == callback && l.capture == capture
        }) {
          return Ok(Value::Undefined);
        }

        let root = scope.heap_mut().add_root(callback)?;
        state.listeners.push(EventListenerEntry {
          event_type,
          callback: RootedCallback { value: callback, root },
          capture,
        });
        Ok(Value::Undefined)
      }
      ("EventTarget", "removeEventListener", 0) => {
        let obj = Self::require_receiver_object(receiver)?;
        let Some(Value::String(_)) = args.get(0).copied() else {
          return Ok(Value::Undefined);
        };
        let event_type = js_string_to_rust_string(scope, args[0])?;

        let callback = args.get(1).copied().unwrap_or(Value::Undefined);
        if matches!(callback, Value::Null | Value::Undefined) {
          return Ok(Value::Undefined);
        }
        if matches!(callback, Value::String(_)) || !is_callable(scope, callback) {
          return Ok(Value::Undefined);
        }

        let capture = get_capture_option(scope, args.get(2).copied().unwrap_or(Value::Undefined))?;

        let Some(state) = self.event_targets.get_mut(&WeakGcObject::from(obj)) else {
          return Ok(Value::Undefined);
        };

        let heap = scope.heap_mut();
        state.listeners.retain(|listener| {
          if listener.event_type == event_type && listener.callback.value == callback && listener.capture == capture {
            heap.remove_root(listener.callback.root);
            false
          } else {
            true
          }
        });
        Ok(Value::Undefined)
      }
      ("EventTarget", "dispatchEvent", 0) => {
        let obj = Self::require_receiver_object(receiver)?;
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

        // Resolve event.type (best-effort).
        let event_type = match event_val {
          Value::Object(ev_obj) => {
            let key = key_from_str(scope, "type")?;
            let value = vm.get(scope, ev_obj, key)?;
            if let Value::String(_) = value {
              js_string_to_rust_string(scope, value)?
            } else {
              return Err(VmError::TypeError("EventTarget.dispatchEvent: event.type is not a string"));
            }
          }
          _ => return Err(VmError::TypeError("EventTarget.dispatchEvent: expected event object")),
        };

        // Invoke listeners synchronously in registration order.
        //
        // NOTE: Calling into JS here can re-enter WebIDL dispatch through `host_from_hooks()`. This
        // implementation intentionally does not touch `self` after taking the snapshot above.
        for listener in listeners_snapshot.into_iter() {
          if listener.event_type != event_type {
            continue;
          }
          let _ = vm.call_without_host(scope, listener.callback.value, Value::Object(obj), &[event_val])?;
        }

        Ok(Value::Bool(true))
      }

      ("URL", "constructor", 0) => {
        let obj = Self::require_receiver_object(receiver)?;
        let input = js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
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
      ("URL", "toJSON", 0) => {
        let url = self.require_url(receiver)?;
        let json = url.to_json().map_err(url_parse_result_to_vm_error)?;
        let s = scope.alloc_string(&json)?;
        scope.push_root(Value::String(s))?;
        Ok(Value::String(s))
      }
      ("URL", "canParse", 0) => {
        let input = js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
        let base = match args.get(1).copied() {
          None | Some(Value::Undefined) => None,
          Some(v) => Some(js_string_to_rust_string(scope, v)?),
        };
        Ok(Value::Bool(Url::can_parse(&input, base.as_deref(), &self.limits)))
      }
      ("URL", "parse", 0) => {
        let input = js_string_to_rust_string(scope, args.get(0).copied().unwrap_or(Value::Undefined))?;
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
          Value::String(_) => {
            let s = js_string_to_rust_string(scope, init)?;
            UrlSearchParams::parse(&s, &self.limits).map_err(url_search_params_error_to_vm_error)?
          }
          Value::Object(other) => self
            .params
            .get(&WeakGcObject::from(other))
            .cloned()
            .ok_or(VmError::TypeError("Unsupported URLSearchParams init object"))?,
          _ => return Err(VmError::TypeError("Unsupported URLSearchParams init value")),
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
        let result = params.get(&name).map_err(url_search_params_error_to_vm_error)?;
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
          scope.define_property(arr, idx_key, data_property(Value::String(s), true, true, true))?;
        }

        Ok(Value::Object(arr))
      }
      ("URLSearchParams", "entries", 0) | ("URLSearchParams", "keys", 0) | ("URLSearchParams", "values", 0) => {
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

        match operation {
          "entries" => {
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
          "keys" => {
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
          "values" => {
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
          _ => unreachable!("URLSearchParams iterator kind mismatch"),
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
            &[Value::String(value_s), Value::String(name_s), Value::Object(params_obj)],
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

      ("Window", "alert", _) => Ok(Value::Undefined),
      ("Window", "queueMicrotask", 0) => {
        let callback = args.get(0).copied().unwrap_or(Value::Undefined);
        self.queue_microtask_impl(scope, callback)
      }
      ("Window", "setTimeout", 0) => self.set_timeout_impl(scope, args),
      ("Window", "setInterval", 0) => self.set_interval_impl(scope, args),
      ("Window", "clearTimeout", 0) => {
        let id = normalize_timer_id(args.get(0).copied().unwrap_or(Value::Number(0.0)));
        self.clear_timer_impl(scope, id, false)
      }
      ("Window", "clearInterval", 0) => {
        let id = normalize_timer_id(args.get(0).copied().unwrap_or(Value::Number(0.0)));
        self.clear_timer_impl(scope, id, true)
      }

      _ => Err(VmError::Unimplemented("WebIDL binding dispatch not implemented for operation")),
    }
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
    Err(VmError::Unimplemented(
      "vm-js WebIDL bindings use call_operation(\"constructor\") dispatch",
    ))
  }
}
