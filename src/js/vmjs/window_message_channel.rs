//! Minimal `window.postMessage`, `MessageChannel`, and `MessagePort` implementation for `vm-js`
//! Window realms.
//!
//! ## Scope / MVP semantics
//! - Single-window environment: `window.postMessage` dispatches a `message` event to the same
//!   window when `targetOrigin` matches the current origin or is `"*"`.
//! - Messages are delivered asynchronously as an [`EventLoop`] task, never synchronously.
//! - `MessageChannel` creates an entangled `{ port1, port2 }` pair.
//! - `MessagePort.postMessage` enqueues a `message` event on the entangled port.
//! - `MessagePort` supports both `onmessage` and `addEventListener('message', ...)`.
//! - Transfer list semantics are implemented for `MessagePort` objects (detaching the sender-side
//!   port and attaching a fresh `MessagePort` wrapper to the receiver).
//!
//! This module intentionally keeps the implementation small and self-contained; it is expected to
//! evolve as structured clone / multi-context support is added.

use crate::js::event_loop::{EventLoop, TaskSource};
use crate::js::dom_internal_keys::{EVENT_BRAND_KEY, EVENT_KIND_KEY};
use crate::js::window_realm::{WindowRealmHost, WindowRealmUserData, EVENT_TARGET_HOST_TAG};
use crate::js::window_timers::{vm_error_to_event_loop_error, VmJsEventLoopHooks};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use url::Url;
use vm_js::iterator;
use vm_js::{
  GcObject, Heap, HostSlots, NativeConstructId, PropertyDescriptor, PropertyKey, PropertyKind, Realm,
  RealmId, RootId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};
use webidl_vm_js::VmJsHostHooksPayload;

const EVENT_KIND_EVENT: u8 = 0;

const DETACHED_MESSAGE_PORT_ID: u64 = 0;

/// Upper bound on how many values will be consumed from a transfer list iterable.
///
/// This is a hostile-input guardrail: the transfer list is specified as a WebIDL `sequence<object>`,
/// meaning callers control an iterable that could be extremely large.
const MAX_TRANSFER_LIST_PORTS: u32 = 1_024;
const TRANSFER_LIST_TOO_LARGE_ERROR: &str = "transfer list is too large";

/// Upper bound on how many messages can be queued for a single receiving port.
///
/// This is a DoS resistance measure (untrusted JS can otherwise enqueue unbounded work).
const MAX_PENDING_MESSAGES_PER_PORT: usize = 10_000;
const MESSAGE_PORT_QUEUE_FULL_ERROR: &str = "MessagePort message queue is full";

const POST_MESSAGE_NO_EVENT_LOOP_ERROR: &str = "postMessage called without an active EventLoop";
const PORT_POST_MESSAGE_NO_EVENT_LOOP_ERROR: &str = "MessagePort.postMessage called without an active EventLoop";

fn data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data { value, writable },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn get_own_data_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Result<Value, VmError> {
  let key = alloc_key(scope, name)?;
  Ok(
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)?
      .unwrap_or(Value::Undefined),
  )
}

fn set_own_data_prop(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
  writable: bool,
) -> Result<(), VmError> {
  // Root `obj` + `value` while allocating the property key (which can GC).
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, data_desc(value, writable))
}

fn realm_id_from_slot(value: Value) -> Option<RealmId> {
  let Value::Number(n) = value else {
    return None;
  };
  if !n.is_finite() || n < 0.0 {
    return None;
  }
  let raw = n as u64;
  if raw as f64 != n {
    return None;
  }
  Some(RealmId::from_raw(raw))
}

fn realm_id_for_binding_call(vm: &Vm, scope: &Scope<'_>, callee: GcObject) -> Result<RealmId, VmError> {
  if let Some(realm_id) = vm.current_realm() {
    return Ok(realm_id);
  }

  let slots = scope.heap().get_function_native_slots(callee)?;
  let realm_id = slots
    .get(REALM_ID_SLOT)
    .copied()
    .and_then(realm_id_from_slot)
    .ok_or(VmError::InvariantViolation(
      "MessageChannel bindings invoked without an active realm",
    ))?;
  Ok(realm_id)
}

#[derive(Default)]
struct Registry {
  realms: HashMap<RealmId, RealmState>,
}

struct RealmState {
  message_port_proto: GcObject,
  message_channel_proto: GcObject,
  next_port_id: u64,
  ports: HashMap<u64, PortState>,
  last_gc_runs: u64,
}

struct PortState {
  entangled: u64,
  active_obj: WeakGcObject,
  closed: bool,
  pending_messages: usize,
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
  REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

fn with_realm_state_mut<R>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  f: impl FnOnce(&mut RealmState, &mut Scope<'_>) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let realm_id = realm_id_for_binding_call(vm, scope, callee)?;
  let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
  let state = reg
    .realms
    .get_mut(&realm_id)
    .ok_or(VmError::InvariantViolation(
      "MessageChannel bindings used before install_window_message_channel_bindings",
    ))?;

  // Opportunistically sweep dead ports when GC has run.
  let gc_runs = scope.heap().gc_runs();
  if gc_runs != state.last_gc_runs {
    state.last_gc_runs = gc_runs;
    let heap = scope.heap();
    // Drop port pairs where both endpoints are dead and there are no pending messages.
    let mut to_remove: Vec<u64> = Vec::new();
    for (&id, port) in state.ports.iter() {
      if id == DETACHED_MESSAGE_PORT_ID {
        continue;
      }
      if id > port.entangled {
        continue;
      }
      let entangled = match state.ports.get(&port.entangled) {
        Some(p) => p,
        None => continue,
      };
      let alive_a = port.active_obj.upgrade(heap).is_some();
      let alive_b = entangled.active_obj.upgrade(heap).is_some();
      if !alive_a
        && !alive_b
        && port.pending_messages == 0
        && entangled.pending_messages == 0
      {
        to_remove.push(id);
        to_remove.push(port.entangled);
      }
    }
    for id in to_remove {
      state.ports.remove(&id);
    }
  }

  f(state, scope)
}

fn serialized_origin_for_document_url(url: &str) -> String {
  let Ok(url) = Url::parse(url) else {
    return "null".to_string();
  };
  match url.scheme() {
    "http" | "https" => url.origin().ascii_serialization(),
    _ => "null".to_string(),
  }
}

fn require_message_port(scope: &Scope<'_>, this: Value, method: &'static str) -> Result<(GcObject, u64), VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError(method));
  };
  let slots = scope
    .heap()
    .object_host_slots(obj)?
    .ok_or(VmError::TypeError(method))?;
  if slots.b != EVENT_TARGET_HOST_TAG {
    return Err(VmError::TypeError(method));
  }
  let id = slots.a;
  if id == DETACHED_MESSAGE_PORT_ID {
    return Err(VmError::TypeError("MessagePort is detached"));
  }
  Ok((obj, id))
}

fn message_port_id_unchecked(scope: &Scope<'_>, obj: GcObject) -> Result<u64, VmError> {
  let slots = scope
    .heap()
    .object_host_slots(obj)?
    .ok_or(VmError::TypeError("MessagePort: illegal invocation"))?;
  if slots.b != EVENT_TARGET_HOST_TAG {
    return Err(VmError::TypeError("MessagePort: illegal invocation"));
  }
  Ok(slots.a)
}

fn brand_event(scope: &mut Scope<'_>, obj: GcObject) -> Result<(), VmError> {
  let brand_key = alloc_key(scope, EVENT_BRAND_KEY)?;
  scope.define_property(
    obj,
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

  let kind_key = alloc_key(scope, EVENT_KIND_KEY)?;
  scope.define_property(
    obj,
    kind_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(EVENT_KIND_EVENT as f64),
        writable: false,
      },
    },
  )?;

  Ok(())
}

fn make_message_port_object(
  scope: &mut Scope<'_>,
  proto: GcObject,
  port_id: u64,
) -> Result<GcObject, VmError> {
  let obj = scope.alloc_object_with_prototype(Some(proto))?;
  scope.push_root(Value::Object(obj))?;

  scope
    .heap_mut()
    .object_set_host_slots(obj, HostSlots { a: port_id, b: EVENT_TARGET_HOST_TAG })?;

  // Event handler attribute.
  set_own_data_prop(scope, obj, "onmessage", Value::Null, true)?;

  Ok(obj)
}

fn clone_message_value_if_possible(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  global: GcObject,
  message: Value,
) -> Result<Value, VmError> {
  // If `structuredClone` exists, use it to avoid sharing mutable objects across tasks. If it does
  // not exist yet, fall back to passing the value through (MVP).
  let key = alloc_key(scope, "structuredClone")?;
  let func = vm.get_with_host_and_hooks(host, scope, hooks, global, key)?;
  if scope.heap().is_callable(func).unwrap_or(false) {
    vm.call_with_host_and_hooks(host, scope, hooks, func, Value::Undefined, &[message])
  } else {
    Ok(message)
  }
}

struct PreparedTransfer {
  port_id: u64,
  sender_obj: GcObject,
  new_obj: GcObject,
  new_root: RootId,
}

fn collect_transfer_ports(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<Vec<GcObject>, VmError> {
  if matches!(value, Value::Undefined | Value::Null) {
    return Ok(Vec::new());
  }

  let mut record = iterator::get_iterator(vm, host, hooks, scope, value)?;
  let mut ports: Vec<GcObject> = Vec::new();
  let mut seen: HashSet<u64> = HashSet::new();
  let mut count: u32 = 0;

  let collect_result: Result<(), VmError> = (|| {
    loop {
      // Ensure budgets apply even if the iterator is "pure native".
      vm.tick()?;

      let Some(item) = iterator::iterator_step_value(vm, host, hooks, scope, &mut record)? else {
        break;
      };
      count += 1;
      if count > MAX_TRANSFER_LIST_PORTS {
        return Err(VmError::TypeError(TRANSFER_LIST_TOO_LARGE_ERROR));
      }

      let port_obj = {
        let mut iter_scope = scope.reborrow();
        iter_scope.push_root(item)?;
        let Value::Object(obj) = item else {
          return Err(VmError::TypeError("transfer list contains a non-MessagePort value"));
        };
        let id = message_port_id_unchecked(&iter_scope, obj)?;
        if id == DETACHED_MESSAGE_PORT_ID {
          return Err(VmError::TypeError("transfer list contains a detached MessagePort"));
        }
        if !seen.insert(id) {
          return Err(VmError::TypeError("transfer list contains duplicate MessagePorts"));
        }
        obj
      };

      // Root each port object so it can't be GC'd while we prepare transfers.
      scope.push_root(Value::Object(port_obj))?;
      ports.push(port_obj);
    }
    Ok(())
  })();

  if let Err(err) = collect_result {
    let _ = iterator::iterator_close(
      vm,
      host,
      hooks,
      scope,
      &record,
      iterator::CloseCompletionKind::Throw,
    );
    return Err(err);
  }

  Ok(ports)
}

fn prepare_port_transfers(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  transfer_ports: &[GcObject],
) -> Result<Vec<PreparedTransfer>, VmError> {
  if transfer_ports.is_empty() {
    return Ok(Vec::new());
  }

  let proto = with_realm_state_mut(vm, scope, callee, |state, _scope| Ok(state.message_port_proto))?;
  let mut prepared: Vec<PreparedTransfer> = Vec::new();
  for &sender_obj in transfer_ports {
    let port_id = message_port_id_unchecked(scope, sender_obj)?;
    if port_id == DETACHED_MESSAGE_PORT_ID {
      return Err(VmError::TypeError("transfer list contains a detached MessagePort"));
    }

    let new_obj = make_message_port_object(scope, proto, port_id)?;
    let new_root = scope.heap_mut().add_root(Value::Object(new_obj))?;

    prepared.push(PreparedTransfer {
      port_id,
      sender_obj,
      new_obj,
      new_root,
    });
  }
  Ok(prepared)
}

fn commit_port_transfers(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  transfers: &[PreparedTransfer],
) -> Result<(), VmError> {
  if transfers.is_empty() {
    return Ok(());
  }

  with_realm_state_mut(vm, scope, callee, |state, scope| {
    for tr in transfers {
      // Detach sender-side port wrapper.
      scope.push_root(Value::Object(tr.sender_obj))?;
      scope.heap_mut().object_set_host_slots(
        tr.sender_obj,
        HostSlots {
          a: DETACHED_MESSAGE_PORT_ID,
          b: EVENT_TARGET_HOST_TAG,
        },
      )?;

      // Point the underlying port endpoint at the receiver-side wrapper.
      if let Some(port_state) = state.ports.get_mut(&tr.port_id) {
        port_state.active_obj = WeakGcObject::new(tr.new_obj);
      }
    }
    Ok(())
  })?;

  Ok(())
}

fn remove_transfer_roots(heap: &mut Heap, transfers: Vec<PreparedTransfer>) {
  for tr in transfers {
    heap.remove_root(tr.new_root);
  }
}

enum ActiveEventLoop<'a> {
  WindowHost(&'a mut EventLoop<crate::js::WindowHostState>),
  BrowserTab(&'a mut EventLoop<crate::api::BrowserTabHost>),
}

fn hooks_payload_mut<'a>(hooks: &'a mut dyn VmHostHooks) -> Option<&'a mut VmJsHostHooksPayload> {
  let any = hooks.as_any_mut()?;
  any.downcast_mut::<VmJsHostHooksPayload>()
}

fn active_event_loop_from_hooks(hooks: &mut dyn VmHostHooks) -> Option<ActiveEventLoop<'_>> {
  let payload = hooks_payload_mut(hooks)?;
  // Use a raw pointer so we can attempt multiple typed downcasts without borrow checker conflicts.
  let payload_ptr: *mut VmJsHostHooksPayload = payload;
  // SAFETY: `payload_ptr` is derived from the `hooks` argument and is only used within this
  // function's dynamic extent.
  unsafe {
    if let Some(el) =
      (&mut *payload_ptr).event_loop_mut::<EventLoop<crate::js::WindowHostState>>()
    {
      return Some(ActiveEventLoop::WindowHost(el));
    }
    if let Some(el) =
      (&mut *payload_ptr).event_loop_mut::<EventLoop<crate::api::BrowserTabHost>>()
    {
      return Some(ActiveEventLoop::BrowserTab(el));
    }
  }
  None
}

fn dispatch_message_event(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  target: GcObject,
  data: Value,
  origin: &str,
  ports: &[GcObject],
  call_onmessage: bool,
) -> Result<(), VmError> {
  // Build a MessageEvent-like object.
  let event = scope.alloc_object()?;
  scope.push_root(Value::Object(event))?;

  // `EventTarget.dispatchEvent` requires the object to be branded as an Event.
  brand_event(scope, event)?;

  let type_s = scope.alloc_string("message")?;
  scope.push_root(Value::String(type_s))?;
  set_own_data_prop(scope, event, "type", Value::String(type_s), false)?;
  set_own_data_prop(scope, event, "data", data, false)?;

  let origin_s = scope.alloc_string(origin)?;
  scope.push_root(Value::String(origin_s))?;
  set_own_data_prop(scope, event, "origin", Value::String(origin_s), false)?;

  set_own_data_prop(scope, event, "source", Value::Null, false)?;

  let ports_arr = scope.alloc_array(ports.len())?;
  scope.push_root(Value::Object(ports_arr))?;
  scope
    .heap_mut()
    .object_set_prototype(ports_arr, Some(vm.intrinsics().ok_or(VmError::Unimplemented(
      "message event dispatch requires intrinsics",
    ))?.array_prototype()))?;
  for (idx, &port_obj) in ports.iter().enumerate() {
    let idx_u32: u32 = idx.try_into().map_err(|_| VmError::Unimplemented("ports array too large"))?;
    let key = alloc_key(scope, &idx_u32.to_string())?;
    scope.define_property(
      ports_arr,
      key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Object(port_obj),
          writable: true,
        },
      },
    )?;
  }
  set_own_data_prop(scope, event, "ports", Value::Object(ports_arr), false)?;

  // Dispatch via EventTarget so `addEventListener('message', ...)` works.
  let dispatch_key = alloc_key(scope, "dispatchEvent")?;
  let dispatch = vm.get_with_host_and_hooks(host, scope, hooks, target, dispatch_key)?;
  if scope.heap().is_callable(dispatch).unwrap_or(false) {
    let _ = vm.call_with_host_and_hooks(
      host,
      scope,
      hooks,
      dispatch,
      Value::Object(target),
      &[Value::Object(event)],
    )?;
  }

  if call_onmessage {
    let onmessage_key = alloc_key(scope, "onmessage")?;
    let onmessage = vm.get_with_host_and_hooks(host, scope, hooks, target, onmessage_key)?;
    if scope.heap().is_callable(onmessage).unwrap_or(false) {
      // `onmessage` exceptions should not abort dispatch.
      let _ = vm.call_with_host_and_hooks(
        host,
        scope,
        hooks,
        onmessage,
        Value::Object(target),
        &[Value::Object(event)],
      );
    }
  }

  Ok(())
}

fn window_post_message_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let global = global_from_callee(scope, callee)?;

  let message = args.get(0).copied().unwrap_or(Value::Undefined);
  let target_origin_arg = args.get(1).copied().ok_or(VmError::TypeError(
    "postMessage requires a targetOrigin argument",
  ))?;
  let transfer_arg = args.get(2).copied().unwrap_or(Value::Undefined);

  let target_origin_s = match target_origin_arg {
    Value::String(s) => s,
    other => scope.heap_mut().to_string(other)?,
  };
  let target_origin = scope
    .heap()
    .get_string(target_origin_s)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let origin = vm
    .user_data::<WindowRealmUserData>()
    .map(|data| serialized_origin_for_document_url(data.document_url()))
    .unwrap_or_else(|| "null".to_string());

  if target_origin != "*" && target_origin != origin {
    // MVP: mismatch => no delivery.
    return Ok(Value::Undefined);
  }

  let transfer_ports = collect_transfer_ports(vm, scope, host, hooks, transfer_arg)?;
  let transfers = prepare_port_transfers(vm, scope, callee, &transfer_ports)?;

  // Clone message (best-effort structuredClone when available).
  let data = match clone_message_value_if_possible(vm, scope, host, hooks, global, message) {
    Ok(v) => v,
    Err(err) => {
      // `prepare_port_transfers` roots receiver-side port wrappers; discard those roots when cloning
      // fails so we don't leak heap roots on `DataCloneError`.
      remove_transfer_roots(scope.heap_mut(), transfers);
      return Err(err);
    }
  };
  let data_root = scope.heap_mut().add_root(data)?;

  let port_roots: Vec<RootId> = transfers.iter().map(|t| t.new_root).collect();

  let Some(event_loop) = active_event_loop_from_hooks(hooks) else {
    // Cleanup roots and abandon transfers.
    scope.heap_mut().remove_root(data_root);
    remove_transfer_roots(scope.heap_mut(), transfers);
    return Err(VmError::TypeError(POST_MESSAGE_NO_EVENT_LOOP_ERROR));
  };

  // Schedule the dispatch task.
  let queue_result = match event_loop {
    ActiveEventLoop::WindowHost(event_loop) => event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      run_window_message_task::<crate::js::WindowHostState>(host, event_loop, data_root, port_roots)
    }),
    ActiveEventLoop::BrowserTab(event_loop) => event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      run_window_message_task::<crate::api::BrowserTabHost>(host, event_loop, data_root, port_roots)
    }),
  };

  if let Err(_err) = queue_result {
    // Do not detach ports if we couldn't enqueue.
    scope.heap_mut().remove_root(data_root);
    remove_transfer_roots(scope.heap_mut(), transfers);
    return Err(VmError::TypeError("postMessage could not enqueue a task"));
  }

  commit_port_transfers(vm, scope, callee, &transfers)?;

  Ok(Value::Undefined)
}

fn run_window_message_task<Host: WindowRealmHost + 'static>(
  host: &mut Host,
  event_loop: &mut EventLoop<Host>,
  data_root: RootId,
  port_roots: Vec<RootId>,
) -> crate::error::Result<()> {
  let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
  hooks.set_event_loop(event_loop);
  let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
  window_realm.reset_interrupt();
  let budget = window_realm.vm_budget_now();
  let global = window_realm.realm().global_object();
  let (vm, heap) = window_realm.vm_and_heap_mut();
  let mut vm = vm.push_budget(budget);

  let mut task_result: Result<(), VmError> = Ok(());

  // Tick first so budgets apply.
  if let Err(err) = vm.tick() {
    task_result = Err(err);
  }

  let data_value = heap.get_root(data_root).unwrap_or(Value::Undefined);
  let ports: Vec<GcObject> = port_roots
    .iter()
    .filter_map(|root| match heap.get_root(*root) {
      Some(Value::Object(obj)) => Some(obj),
      _ => None,
    })
    .collect();

  if task_result.is_ok() {
    let mut scope = heap.scope();
    // Compute origin from current document URL.
    let origin = vm
      .user_data::<WindowRealmUserData>()
      .map(|data| serialized_origin_for_document_url(data.document_url()))
      .unwrap_or_else(|| "null".to_string());

    if let Err(err) = dispatch_message_event(
      &mut vm,
      &mut scope,
      vm_host,
      &mut hooks,
      global,
      data_value,
      &origin,
      &ports,
      /* call_onmessage */ true,
    ) {
      task_result = Err(err);
    }
  }

  // Cleanup roots.
  heap.remove_root(data_root);
  for root in port_roots {
    heap.remove_root(root);
  }

  if let Some(err) = hooks.finish(heap) {
    return Err(err);
  }

  task_result.map_err(|e| vm_error_to_event_loop_error(heap, e))
}

fn message_channel_ctor_call_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "MessageChannel constructor cannot be invoked without 'new'",
  ))
}

fn message_channel_ctor_construct_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let (channel_proto, port_proto) = with_realm_state_mut(vm, scope, callee, |state, _scope| {
    Ok((state.message_channel_proto, state.message_port_proto))
  })?;

  let channel = scope.alloc_object_with_prototype(Some(channel_proto))?;
  scope.push_root(Value::Object(channel))?;

  let (port1_id, port2_id, port1, port2) = with_realm_state_mut(vm, scope, callee, |state, scope| {
    let port1_id = state.next_port_id;
    state.next_port_id = state.next_port_id.saturating_add(1);
    let port2_id = state.next_port_id;
    state.next_port_id = state.next_port_id.saturating_add(1);

    let port1 = make_message_port_object(scope, port_proto, port1_id)?;
    let port2 = make_message_port_object(scope, port_proto, port2_id)?;

    state.ports.insert(
      port1_id,
      PortState {
        entangled: port2_id,
        active_obj: WeakGcObject::new(port1),
        closed: false,
        pending_messages: 0,
      },
    );
    state.ports.insert(
      port2_id,
      PortState {
        entangled: port1_id,
        active_obj: WeakGcObject::new(port2),
        closed: false,
        pending_messages: 0,
      },
    );

    Ok((port1_id, port2_id, port1, port2))
  })?;

  let _ = (port1_id, port2_id);

  set_own_data_prop(scope, channel, "port1", Value::Object(port1), false)?;
  set_own_data_prop(scope, channel, "port2", Value::Object(port2), false)?;

  Ok(Value::Object(channel))
}

fn message_port_ctor_call_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("Illegal constructor"))
}

fn message_port_ctor_construct_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  Err(VmError::TypeError("Illegal constructor"))
}

fn message_port_post_message_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let global = global_from_callee(scope, callee)?;
  let (_this_obj, sender_id) = require_message_port(
    scope,
    this,
    "MessagePort.prototype.postMessage called on incompatible receiver",
  )?;

  let message = args.get(0).copied().unwrap_or(Value::Undefined);
  let transfer_arg = args.get(1).copied().unwrap_or(Value::Undefined);

  let (sender_closed, receiver_id, receiver_closed, receiver_pending) =
    with_realm_state_mut(vm, scope, callee, |state, _scope| {
    let Some(sender) = state.ports.get(&sender_id) else {
      return Err(VmError::TypeError("MessagePort is invalid"));
    };
    let Some(receiver) = state.ports.get(&sender.entangled) else {
      return Err(VmError::TypeError("MessagePort is entangled with an invalid port"));
    };
    Ok((sender.closed, sender.entangled, receiver.closed, receiver.pending_messages))
  })?;

  if sender_closed || receiver_closed {
    return Ok(Value::Undefined);
  }
  if receiver_pending >= MAX_PENDING_MESSAGES_PER_PORT {
    return Err(VmError::TypeError(MESSAGE_PORT_QUEUE_FULL_ERROR));
  }

  let transfer_ports = collect_transfer_ports(vm, scope, host, hooks, transfer_arg)?;
  let transfers = prepare_port_transfers(vm, scope, callee, &transfer_ports)?;

  let data = match clone_message_value_if_possible(vm, scope, host, hooks, global, message) {
    Ok(v) => v,
    Err(err) => {
      // `prepare_port_transfers` roots receiver-side port wrappers; discard those roots when cloning
      // fails so we don't leak heap roots on `DataCloneError`.
      remove_transfer_roots(scope.heap_mut(), transfers);
      return Err(err);
    }
  };
  let data_root = scope.heap_mut().add_root(data)?;
  let port_roots: Vec<RootId> = transfers.iter().map(|t| t.new_root).collect();

  let Some(event_loop) = active_event_loop_from_hooks(hooks) else {
    scope.heap_mut().remove_root(data_root);
    remove_transfer_roots(scope.heap_mut(), transfers);
    return Err(VmError::TypeError(PORT_POST_MESSAGE_NO_EVENT_LOOP_ERROR));
  };

  let queue_result = match event_loop {
    ActiveEventLoop::WindowHost(event_loop) => event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      run_port_message_task::<crate::js::WindowHostState>(host, event_loop, receiver_id, data_root, port_roots)
    }),
    ActiveEventLoop::BrowserTab(event_loop) => event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      run_port_message_task::<crate::api::BrowserTabHost>(host, event_loop, receiver_id, data_root, port_roots)
    }),
  };

  if let Err(_err) = queue_result {
    scope.heap_mut().remove_root(data_root);
    remove_transfer_roots(scope.heap_mut(), transfers);
    return Err(VmError::TypeError("MessagePort.postMessage could not enqueue a task"));
  }

  // Now that the task is enqueued, commit port transfers and update pending message counts.
  commit_port_transfers(vm, scope, callee, &transfers)?;
  with_realm_state_mut(vm, scope, callee, |state, _scope| {
    if let Some(receiver) = state.ports.get_mut(&receiver_id) {
      receiver.pending_messages = receiver.pending_messages.saturating_add(1);
    }
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn run_port_message_task<Host: WindowRealmHost + 'static>(
  host: &mut Host,
  event_loop: &mut EventLoop<Host>,
  receiver_id: u64,
  data_root: RootId,
  port_roots: Vec<RootId>,
) -> crate::error::Result<()> {
  let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
  hooks.set_event_loop(event_loop);
  let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
  window_realm.reset_interrupt();
  let budget = window_realm.vm_budget_now();
  let realm_id = window_realm.realm().id();
  let (vm, heap) = window_realm.vm_and_heap_mut();
  let mut vm = vm.push_budget(budget);

  let mut task_result: Result<(), VmError> = Ok(());

  if let Err(err) = vm.tick() {
    task_result = Err(err);
  }

  // Always decrement pending message count even if we can't deliver.
  {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = reg.realms.get_mut(&realm_id) {
      if let Some(receiver) = state.ports.get_mut(&receiver_id) {
        receiver.pending_messages = receiver.pending_messages.saturating_sub(1);
      }
    }
  }

  let data_value = heap.get_root(data_root).unwrap_or(Value::Undefined);
  let ports: Vec<GcObject> = port_roots
    .iter()
    .filter_map(|root| match heap.get_root(*root) {
      Some(Value::Object(obj)) => Some(obj),
      _ => None,
    })
    .collect();

  if task_result.is_ok() {
    let target_port_obj = {
      let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
      reg
        .realms
        .get(&realm_id)
        .and_then(|state| state.ports.get(&receiver_id))
        .and_then(|port| {
          if port.closed {
            return None;
          }
          port.active_obj.upgrade(heap)
        })
    };

    if let Some(target) = target_port_obj {
      let mut scope = heap.scope();
      if let Err(err) = dispatch_message_event(
        &mut vm,
        &mut scope,
        vm_host,
        &mut hooks,
        target,
        data_value,
        "",
        &ports,
        /* call_onmessage */ true,
      ) {
        task_result = Err(err);
      }
    }
  }

  heap.remove_root(data_root);
  for root in port_roots {
    heap.remove_root(root);
  }

  if let Some(err) = hooks.finish(heap) {
    return Err(err);
  }

  task_result.map_err(|e| vm_error_to_event_loop_error(heap, e))
}

fn message_port_start_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Ports are started by default in this MVP.
  Ok(Value::Undefined)
}

fn message_port_close_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (_obj, port_id) = require_message_port(
    scope,
    this,
    "MessagePort.prototype.close called on incompatible receiver",
  )?;
  with_realm_state_mut(vm, scope, callee, |state, _scope| {
    if let Some(port) = state.ports.get_mut(&port_id) {
      port.closed = true;
    }
    Ok(())
  })?;
  Ok(Value::Undefined)
}

const REALM_ID_SLOT: usize = 0;
const GLOBAL_SLOT: usize = 1;

fn global_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots.get(GLOBAL_SLOT).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::InvariantViolation(
      "MessageChannel binding missing global object slot",
    )),
  }
}

/// Install `MessageChannel`, `MessagePort`, and `postMessage` onto the window global object.
pub fn install_window_message_channel_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
) -> Result<(), VmError> {
  let realm_id = realm.id();

  // Avoid double-installation.
  {
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if reg.realms.contains_key(&realm_id) {
      return Ok(());
    }
  }

  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let intr = realm.intrinsics();
  let func_proto = intr.function_prototype();

  // EventTarget must exist to inherit `addEventListener` / `dispatchEvent`.
  let event_target_ctor = match get_own_data_prop(&mut scope, global, "EventTarget")? {
    Value::Object(obj) => obj,
    _ => return Err(VmError::Unimplemented("EventTarget is required for MessagePort")),
  };
  scope.push_root(Value::Object(event_target_ctor))?;
  let event_target_proto = match get_own_data_prop(&mut scope, event_target_ctor, "prototype")? {
    Value::Object(obj) => obj,
    _ => return Err(VmError::Unimplemented("EventTarget.prototype missing")),
  };

  // --- MessagePort ---
  let message_port_ctor = {
    let call_id = vm.register_native_call(message_port_ctor_call_native)?;
    let construct_id: NativeConstructId = vm.register_native_construct(message_port_ctor_construct_native)?;
    let name_s = scope.alloc_string("MessagePort")?;
    scope.push_root(Value::String(name_s))?;
    let slots = [Value::Number(realm_id.to_raw() as f64), Value::Object(global)];
    let ctor = scope.alloc_native_function_with_slots(call_id, Some(construct_id), name_s, 0, &slots)?;
    scope.heap_mut().object_set_prototype(ctor, Some(func_proto))?;
    scope.push_root(Value::Object(ctor))?;

    // Inherit from EventTarget.prototype.
    let Value::Object(proto) = get_own_data_prop(&mut scope, ctor, "prototype")? else {
      return Err(VmError::InvariantViolation("MessagePort.prototype missing"));
    };
    scope.push_root(Value::Object(proto))?;
    scope.heap_mut().object_set_prototype(proto, Some(event_target_proto))?;

    let post_message_id = vm.register_native_call(message_port_post_message_native)?;
    let post_message_name = scope.alloc_string("postMessage")?;
    scope.push_root(Value::String(post_message_name))?;
    let post_message_fn = scope.alloc_native_function_with_slots(
      post_message_id,
      None,
      post_message_name,
      1,
      &slots,
    )?;
    scope
      .heap_mut()
      .object_set_prototype(post_message_fn, Some(func_proto))?;
    set_own_data_prop(
      &mut scope,
      proto,
      "postMessage",
      Value::Object(post_message_fn),
      true,
    )?;

    let start_id = vm.register_native_call(message_port_start_native)?;
    let start_name = scope.alloc_string("start")?;
    scope.push_root(Value::String(start_name))?;
    let start_fn = scope.alloc_native_function_with_slots(start_id, None, start_name, 0, &slots)?;
    scope.heap_mut().object_set_prototype(start_fn, Some(func_proto))?;
    set_own_data_prop(&mut scope, proto, "start", Value::Object(start_fn), true)?;

    let close_id = vm.register_native_call(message_port_close_native)?;
    let close_name = scope.alloc_string("close")?;
    scope.push_root(Value::String(close_name))?;
    let close_fn = scope.alloc_native_function_with_slots(close_id, None, close_name, 0, &slots)?;
    scope.heap_mut().object_set_prototype(close_fn, Some(func_proto))?;
    set_own_data_prop(&mut scope, proto, "close", Value::Object(close_fn), true)?;

    ctor
  };

  // --- MessageChannel ---
  let message_channel_ctor = {
    let call_id = vm.register_native_call(message_channel_ctor_call_native)?;
    let construct_id = vm.register_native_construct(message_channel_ctor_construct_native)?;
    let name_s = scope.alloc_string("MessageChannel")?;
    scope.push_root(Value::String(name_s))?;
    let slots = [Value::Number(realm_id.to_raw() as f64), Value::Object(global)];
    let ctor = scope.alloc_native_function_with_slots(call_id, Some(construct_id), name_s, 0, &slots)?;
    scope.heap_mut().object_set_prototype(ctor, Some(func_proto))?;
    scope.push_root(Value::Object(ctor))?;
    ctor
  };

  // --- window.postMessage ---
  let post_message_func = {
    let call_id = vm.register_native_call(window_post_message_native)?;
    let name_s = scope.alloc_string("postMessage")?;
    scope.push_root(Value::String(name_s))?;
    let slots = [Value::Number(realm_id.to_raw() as f64), Value::Object(global)];
    let func = scope.alloc_native_function_with_slots(call_id, None, name_s, 2, &slots)?;
    scope.heap_mut().object_set_prototype(func, Some(func_proto))?;
    func
  };

  // Install globals.
  set_own_data_prop(
    &mut scope,
    global,
    "MessagePort",
    Value::Object(message_port_ctor),
    true,
  )?;
  set_own_data_prop(
    &mut scope,
    global,
    "MessageChannel",
    Value::Object(message_channel_ctor),
    true,
  )?;
  set_own_data_prop(
    &mut scope,
    global,
    "postMessage",
    Value::Object(post_message_func),
    true,
  )?;

  let message_port_proto = match get_own_data_prop(&mut scope, message_port_ctor, "prototype")? {
    Value::Object(obj) => obj,
    _ => return Err(VmError::InvariantViolation("MessagePort.prototype missing")),
  };
  let message_channel_proto = match get_own_data_prop(&mut scope, message_channel_ctor, "prototype")? {
    Value::Object(obj) => obj,
    _ => return Err(VmError::InvariantViolation("MessageChannel.prototype missing")),
  };

  // Register per-realm state.
  let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
  reg.realms.insert(
    realm_id,
    RealmState {
      message_port_proto,
      message_channel_proto,
      next_port_id: 1,
      ports: HashMap::new(),
      last_gc_runs: scope.heap().gc_runs(),
    },
  );

  Ok(())
}

pub(crate) fn teardown_window_message_channel_bindings_for_realm(realm_id: RealmId) {
  let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
  reg.realms.remove(&realm_id);
}

#[cfg(test)]
mod tests {
  use crate::dom2;
  use crate::error::Result;
  use crate::js::{RunLimits, RunUntilIdleOutcome, WindowHost};
  use crate::resource::{FetchedResource, ResourceFetcher};
  use selectors::context::QuirksMode;
  use std::sync::Arc;
  use vm_js::{PropertyKey, Value};

  #[derive(Debug, Default)]
  struct NoFetchResourceFetcher;

  impl ResourceFetcher for NoFetchResourceFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      Err(crate::Error::Other(format!(
        "NoFetchResourceFetcher does not support fetch: {url}"
      )))
    }
  }

  fn get_global_prop(host: &mut WindowHost, name: &str) -> Value {
    let window = host.host_mut().window_mut();
    let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global)).expect("push root global");
    let key_s = scope.alloc_string(name).expect("alloc prop name");
    scope.push_root(Value::String(key_s)).expect("push root prop name");
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .expect("get prop")
      .unwrap_or(Value::Undefined)
  }

  fn get_global_prop_utf8(host: &mut WindowHost, name: &str) -> Option<String> {
    let value = get_global_prop(host, name);
    let window = host.host_mut().window_mut();
    match value {
      Value::String(s) => Some(window.heap().get_string(s).expect("get string").to_utf8_lossy()),
      _ => None,
    }
  }

  fn get_global_prop_bool(host: &mut WindowHost, name: &str) -> Option<bool> {
    match get_global_prop(host, name) {
      Value::Bool(b) => Some(b),
      _ => None,
    }
  }

  fn make_host() -> WindowHost {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    WindowHost::new_with_fetcher(dom, "https://example.invalid/", Arc::new(NoFetchResourceFetcher))
      .expect("WindowHost::new_with_fetcher")
  }

  #[test]
  fn message_channel_delivers_messages_in_order() -> Result<()> {
    let mut host = make_host();
    host.exec_script(
      r#"
        globalThis.__log = '';
        const ch = new MessageChannel();
        ch.port1.onmessage = (e) => { globalThis.__log += e.data; };
        ch.port2.postMessage('a');
        ch.port2.postMessage('b');
      "#,
    )?;
    assert_eq!(host.run_until_idle(RunLimits::unbounded())?, RunUntilIdleOutcome::Idle);
    assert_eq!(get_global_prop_utf8(&mut host, "__log"), Some("ab".to_string()));
    Ok(())
  }

  #[test]
  fn message_channel_onmessage_handler_is_called() -> Result<()> {
    let mut host = make_host();
    host.exec_script(
      r#"
        globalThis.__count = 0;
        const ch = new MessageChannel();
        ch.port1.onmessage = (_e) => { globalThis.__count++; };
        ch.port2.postMessage('hi');
      "#,
    )?;
    assert_eq!(host.run_until_idle(RunLimits::unbounded())?, RunUntilIdleOutcome::Idle);
    assert_eq!(get_global_prop(&mut host, "__count"), Value::Number(1.0));
    Ok(())
  }

  #[test]
  fn message_channel_add_event_listener_works() -> Result<()> {
    let mut host = make_host();
    host.exec_script(
      r#"
        globalThis.__msg = null;
        const ch = new MessageChannel();
        ch.port1.addEventListener('message', (e) => { globalThis.__msg = e.data; });
        ch.port2.postMessage('hi');
      "#,
    )?;
    assert_eq!(host.run_until_idle(RunLimits::unbounded())?, RunUntilIdleOutcome::Idle);
    assert_eq!(get_global_prop_utf8(&mut host, "__msg"), Some("hi".to_string()));
    Ok(())
  }

  #[test]
  fn window_post_message_can_transfer_a_port() -> Result<()> {
    let mut host = make_host();
    host.exec_script(
      r#"
        globalThis.__got = null;
        globalThis.__detached = false;
        const ch = new MessageChannel();
        globalThis.addEventListener('message', (e) => {
          const p = e.ports[0];
          p.onmessage = (ev) => { globalThis.__got = ev.data; };
          ch.port1.postMessage('hi');
        });
        globalThis.postMessage('x', '*', [ch.port2]);
        try { ch.port2.postMessage('nope'); } catch (e) { globalThis.__detached = true; }
      "#,
    )?;
    assert_eq!(host.run_until_idle(RunLimits::unbounded())?, RunUntilIdleOutcome::Idle);
    assert_eq!(get_global_prop_utf8(&mut host, "__got"), Some("hi".to_string()));
    assert_eq!(get_global_prop_bool(&mut host, "__detached"), Some(true));
    Ok(())
  }
}
