//! WebSocket bindings (`WebSocket` class) for the `vm-js` Window realm.
//!
//! This is a minimal, deterministic implementation intended to unblock real-world scripts that
//! expect `new WebSocket("ws://…")` to work.
//!
//! Design notes:
//! - Connection I/O is performed on a dedicated thread per WebSocket.
//! - Network callbacks are delivered by queueing `EventLoop` tasks via
//!   [`crate::js::event_loop::ExternalTaskQueueHandle`].
//! - The JS-visible object is a non-DOM `EventTarget` (inherits from `EventTarget.prototype`) and
//!   dispatches `open`/`message`/`error`/`close` events via `dispatchEvent`.

use crate::js::event_loop::{ExternalTaskQueueHandle, TaskSource};
use crate::js::url_resolve::{resolve_url, UrlResolveError};
use crate::js::window_blob;
use crate::js::window_realm::{WindowRealmHost, WindowRealmUserData};
use crate::js::window_timers::{event_loop_mut_from_hooks, vm_error_to_event_loop_error, VmJsEventLoopHooks};
use std::borrow::Cow;
use std::char::decode_utf16;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;
use tungstenite::protocol::{CloseFrame, Message};
use tungstenite::{client::IntoClientRequest, connect};
use vm_js::iterator::{self, CloseCompletionKind};
use vm_js::{
  GcObject, GcString, Heap, NativeConstructId, NativeFunctionId, PropertyDescriptor, PropertyKey, PropertyKind, Realm,
  Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
};

const SLOT_ENV_ID: usize = 0;

const ENV_ID_KEY: &str = "__fastrender_websocket_env_id";
const WS_ID_KEY: &str = "__fastrender_websocket_id";

// Must match the brand key used by `WindowRealm`'s `EventTarget` implementation.
const EVENT_TARGET_BRAND_KEY: &str = "__fastrender_event_target";

pub const WS_CONNECTING: u16 = 0;
pub const WS_OPEN: u16 = 1;
pub const WS_CLOSING: u16 = 2;
pub const WS_CLOSED: u16 = 3;

const MAX_WEBSOCKET_URL_BYTES: usize = 8 * 1024;
const MAX_WEBSOCKET_PROTOCOL_BYTES: usize = 1 * 1024;
const MAX_WEBSOCKET_PROTOCOLS: u32 = 32;
const MAX_WEBSOCKET_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_WEBSOCKET_CLOSE_REASON_BYTES: usize = 123;
const MAX_QUEUED_WEBSOCKET_SEND_COMMANDS: usize = 1_024;
const MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET: usize = 1_024;

#[derive(Clone)]
pub struct WindowWebSocketEnv {
  pub document_url: Option<String>,
}

impl WindowWebSocketEnv {
  pub fn for_document(document_url: Option<String>) -> Self {
    Self { document_url }
  }
}

struct EnvState {
  env: WindowWebSocketEnv,
  next_id: u64,
  sockets: HashMap<u64, WebSocketState>,
}

impl EnvState {
  fn new(env: WindowWebSocketEnv) -> Self {
    Self {
      env,
      next_id: 1,
      sockets: HashMap::new(),
    }
  }

  fn alloc_id(&mut self) -> u64 {
    let id = self.next_id;
    self.next_id = self.next_id.saturating_add(1);
    id
  }
}

struct WebSocketState {
  weak_obj: WeakGcObject,
  url: String,
  protocol: String,
  ready_state: u16,
  buffered_amount: usize,
  pending_events: usize,
  cmd_tx: mpsc::SyncSender<WsCommand>,
  thread: Option<JoinHandle<()>>,
}

enum WsCommand {
  SendText(String),
  SendBinary(Vec<u8>),
  Close {
    code: Option<u16>,
    reason: Option<String>,
  },
  Shutdown,
}

static NEXT_ENV_ID: AtomicU64 = AtomicU64::new(1);
static ENVS: OnceLock<Mutex<HashMap<u64, EnvState>>> = OnceLock::new();

fn envs() -> &'static Mutex<HashMap<u64, EnvState>> {
  ENVS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn unregister_window_websocket_env(env_id: u64) {
  // Remove first so subsequent lookups fail while we join threads.
  let env_state = {
    let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.remove(&env_id)
  };

  let Some(mut env_state) = env_state else {
    return;
  };

  for (_id, mut ws) in env_state.sockets.drain() {
    // Best-effort shutdown: if the channel is full/disconnected, dropping the sender will still
    // cause the thread to eventually exit.
    let _ = ws.cmd_tx.try_send(WsCommand::Shutdown);
    drop(ws.cmd_tx);
    if let Some(handle) = ws.thread.take() {
      let _ = handle.join();
    }
  }
}

#[derive(Debug)]
#[must_use = "websocket bindings are only valid while the returned WindowWebSocketBindings is kept alive"]
pub struct WindowWebSocketBindings {
  env_id: u64,
  active: bool,
}

impl WindowWebSocketBindings {
  fn new(env_id: u64) -> Self {
    Self { env_id, active: true }
  }

  pub fn env_id(&self) -> u64 {
    self.env_id
  }

  fn disarm(mut self) -> u64 {
    self.active = false;
    self.env_id
  }
}

impl Drop for WindowWebSocketBindings {
  fn drop(&mut self) {
    if self.active {
      unregister_window_websocket_env(self.env_id);
    }
  }
}

fn with_env_state<R>(env_id: u64, f: impl FnOnce(&EnvState) -> Result<R, VmError>) -> Result<R, VmError> {
  let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = lock
    .get(&env_id)
    .ok_or(VmError::Unimplemented("WebSocket env id not registered"))?;
  f(state)
}

fn with_env_state_mut<R>(env_id: u64, f: impl FnOnce(&mut EnvState) -> Result<R, VmError>) -> Result<R, VmError> {
  let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = lock
    .get_mut(&env_id)
    .ok_or(VmError::Unimplemented("WebSocket env id not registered"))?;
  f(state)
}

fn data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data { value, writable },
  }
}

fn accessor_desc(get: Value, set: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Accessor { get, set },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn get_data_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Result<Value, VmError> {
  let key = alloc_key(scope, name)?;
  Ok(
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)?
      .unwrap_or(Value::Undefined),
  )
}

fn set_data_prop(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
  writable: bool,
) -> Result<(), VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, data_desc(value, writable))
}

fn env_id_from_callee(scope: &mut Scope<'_>, callee: GcObject) -> Result<u64, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let value = slots.get(SLOT_ENV_ID).copied().unwrap_or(Value::Undefined);
  let Value::Number(n) = value else {
    return Err(VmError::InvariantViolation(
      "WebSocket constructor missing env id slot",
    ));
  };
  if !n.is_finite() || n < 0.0 || n > u64::MAX as f64 {
    return Err(VmError::InvariantViolation(
      "WebSocket constructor env id slot invalid",
    ));
  }
  Ok(n as u64)
}

fn parse_env_and_ws_id(scope: &mut Scope<'_>, this: Value) -> Result<(u64, u64, GcObject), VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  if !scope.heap().is_valid_object(obj) {
    return Err(VmError::TypeError("Illegal invocation"));
  }

  scope.push_root(Value::Object(obj))?;
  let env_key = alloc_key(scope, ENV_ID_KEY)?;
  let ws_key = alloc_key(scope, WS_ID_KEY)?;

  let env = scope
    .heap()
    .object_get_own_data_property_value(obj, &env_key)?
    .unwrap_or(Value::Undefined);
  let ws = scope
    .heap()
    .object_get_own_data_property_value(obj, &ws_key)?
    .unwrap_or(Value::Undefined);

  let env_id = match env {
    Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u64::MAX as f64 => n as u64,
    _ => return Err(VmError::TypeError("Illegal invocation")),
  };
  let ws_id = match ws {
    Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u64::MAX as f64 => n as u64,
    _ => return Err(VmError::TypeError("Illegal invocation")),
  };

  Ok((env_id, ws_id, obj))
}

fn js_string_to_rust_string_limited(
  heap: &Heap,
  handle: GcString,
  max_bytes: usize,
  error: &'static str,
) -> Result<String, VmError> {
  let js = heap.get_string(handle)?;
  let code_units_len = js.len_code_units();
  if code_units_len > max_bytes {
    return Err(VmError::TypeError(error));
  }

  let capacity = code_units_len.saturating_mul(3).min(max_bytes);
  let mut out = String::with_capacity(capacity);
  let mut out_len = 0usize;
  for decoded in decode_utf16(js.as_code_units().iter().copied()) {
    let ch = decoded.unwrap_or('\u{FFFD}');
    let ch_len = ch.len_utf8();
    let next_len = out_len.checked_add(ch_len).unwrap_or(usize::MAX);
    if next_len > max_bytes {
      return Err(VmError::TypeError(error));
    }
    out.push(ch);
    out_len = next_len;
  }
  Ok(out)
}

fn to_rust_string_limited(
  heap: &mut Heap,
  value: Value,
  max_bytes: usize,
  error: &'static str,
) -> Result<String, VmError> {
  let s = heap.to_string(value)?;
  js_string_to_rust_string_limited(heap, s, max_bytes, error)
}

fn current_document_base_url(vm: &mut Vm, env_id: u64) -> Result<Option<String>, VmError> {
  if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
    return Ok(data.base_url.clone());
  }
  with_env_state(env_id, |state| Ok(state.env.document_url.clone()))
}

fn resolve_websocket_url(vm: &mut Vm, env_id: u64, url: &str) -> Result<url::Url, VmError> {
  let base = current_document_base_url(vm, env_id)?;
  let resolved_href = resolve_url(url, base.as_deref()).map_err(|err| match err {
    UrlResolveError::RelativeUrlWithoutBase => VmError::TypeError(
      "WebSocket URL is relative, but the current document has no base URL",
    ),
    UrlResolveError::Url(_) => VmError::TypeError("WebSocket URL is invalid"),
  })?;

  let mut resolved =
    url::Url::parse(&resolved_href).map_err(|_| VmError::TypeError("WebSocket URL is invalid"))?;

  if resolved.fragment().is_some() {
    return Err(VmError::TypeError(
      "WebSocket URL must not include a fragment",
    ));
  }

  match resolved.scheme() {
    "ws" | "wss" => {}
    "http" => {
      let _ = resolved.set_scheme("ws");
    }
    "https" => {
      let _ = resolved.set_scheme("wss");
    }
    _ => {
      return Err(VmError::TypeError(
        "WebSocket URL must use ws: or wss: scheme",
      ))
    }
  }

  Ok(resolved)
}

fn parse_protocols(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<Vec<String>, VmError> {
  if matches!(value, Value::Undefined | Value::Null) {
    return Ok(Vec::new());
  }

  match value {
    Value::String(s) => {
      let s = js_string_to_rust_string_limited(scope.heap(), s, MAX_WEBSOCKET_PROTOCOL_BYTES, "WebSocket protocol too long")?;
      if s.is_empty() {
        return Ok(Vec::new());
      }
      Ok(vec![s])
    }
    Value::Object(_) => {
      let iterable = value;
      let mut record = iterator::get_iterator(vm, host, hooks, scope, iterable)?;
      let mut out: Vec<String> = Vec::new();
      let mut count: u32 = 0;
      let collect_result: Result<(), VmError> = (|| {
        loop {
          vm.tick()?;
          let Some(item) = iterator::iterator_step_value(vm, host, hooks, scope, &mut record)? else {
            break;
          };
          count += 1;
          if count > MAX_WEBSOCKET_PROTOCOLS {
            return Err(VmError::TypeError("WebSocket protocols list is too large"));
          }
          let s = to_rust_string_limited(scope.heap_mut(), item, MAX_WEBSOCKET_PROTOCOL_BYTES, "WebSocket protocol too long")?;
          if s.is_empty() {
            return Err(VmError::TypeError("WebSocket protocol must not be empty"));
          }
          if out.iter().any(|p| p == &s) {
            return Err(VmError::TypeError(
              "WebSocket protocols list must not contain duplicates",
            ));
          }
          out.push(s);
        }
        Ok(())
      })();

      if let Err(err) = collect_result {
        if !record.done {
          // `iterator_close` can allocate / run user code (and thus GC). If we're returning a thrown
          // value, root it so the handle stays valid until we bubble it up.
          let original_is_throw = err.is_throw_completion();
          let pending_root = err
            .thrown_value()
            .map(|v| scope.heap_mut().add_root(v))
            .transpose()?;
          let close_res =
            iterator::iterator_close(vm, host, hooks, scope, &record, CloseCompletionKind::Throw);
          if let Some(root) = pending_root {
            scope.heap_mut().remove_root(root);
          }
          // IteratorClose errors override non-throw completions, but must never suppress fatal VM
          // errors (termination/OOM). `CloseCompletionKind::Throw` already suppresses throw
          // completions from `return`; if we still get an error here, treat it as fatal.
          if let Err(close_err) = close_res {
            if original_is_throw {
              return Err(close_err);
            }
          }
        }
        return Err(err);
      }
      if !record.done {
        iterator::iterator_close(vm, host, hooks, scope, &record, CloseCompletionKind::NonThrow)?;
      }
      Ok(out)
    }
    other => {
      // Union conversion fallback: treat as DOMString.
      let s = to_rust_string_limited(scope.heap_mut(), other, MAX_WEBSOCKET_PROTOCOL_BYTES, "WebSocket protocol too long")?;
      if s.is_empty() {
        return Ok(Vec::new());
      }
      Ok(vec![s])
    }
  }
}

fn websocket_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "WebSocket constructor cannot be invoked without 'new'",
  ))
}

fn websocket_ctor_construct<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;

  let url_value = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(url_value, Value::Undefined) {
    return Err(VmError::TypeError("WebSocket URL is required"));
  }
  let url_str = to_rust_string_limited(
    scope.heap_mut(),
    url_value,
    MAX_WEBSOCKET_URL_BYTES,
    "WebSocket URL exceeds maximum length",
  )?;

  let resolved_url = resolve_websocket_url(vm, env_id, &url_str).map_err(|err| match err {
    VmError::TypeError(_) => err,
    other => other,
  })?;

  let protocols_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let protocols = parse_protocols(vm, scope, host, hooks, protocols_value)?;
  let protocols_header = if protocols.is_empty() {
    None
  } else {
    Some(protocols.join(", "))
  };

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(hooks) else {
    return Err(VmError::TypeError(
      "WebSocket constructed without an active EventLoop",
    ));
  };
  let task_queue: ExternalTaskQueueHandle<Host> = event_loop.external_task_queue_handle();

  // Create the JS wrapper object with `newTarget.prototype`.
  let ctor = match new_target {
    Value::Object(obj) => obj,
    _ => callee,
  };
  let prototype_key = alloc_key(scope, "prototype")?;
  let proto = scope
    .heap()
    .object_get_own_data_property_value(ctor, &prototype_key)?
    .and_then(|v| match v {
      Value::Object(obj) => Some(obj),
      _ => None,
    });

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  if let Some(proto) = proto {
    scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  }

  // Brand the object as an EventTarget so `addEventListener` works.
  let brand_key = alloc_key(scope, EVENT_TARGET_BRAND_KEY)?;
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

  // Hidden ids.
  let env_key = alloc_key(scope, ENV_ID_KEY)?;
  scope.define_property(
    obj,
    env_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(env_id as f64),
        writable: false,
      },
    },
  )?;

  // Default handler attributes.
  set_data_prop(scope, obj, "onopen", Value::Null, true)?;
  set_data_prop(scope, obj, "onmessage", Value::Null, true)?;
  set_data_prop(scope, obj, "onerror", Value::Null, true)?;
  set_data_prop(scope, obj, "onclose", Value::Null, true)?;

  let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WsCommand>(MAX_QUEUED_WEBSOCKET_SEND_COMMANDS);

  let ws_id = with_env_state_mut(env_id, |state| {
    let id = state.alloc_id();
    state.sockets.insert(
      id,
      WebSocketState {
        weak_obj: WeakGcObject::from(obj),
        url: resolved_url.to_string(),
        protocol: String::new(),
        ready_state: WS_CONNECTING,
        buffered_amount: 0,
        pending_events: 0,
        cmd_tx: cmd_tx.clone(),
        thread: None,
      },
    );
    Ok(id)
  })?;

  let ws_key = alloc_key(scope, WS_ID_KEY)?;
  scope.define_property(
    obj,
    ws_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(ws_id as f64),
        writable: false,
      },
    },
  )?;

  let resolved_url_string = resolved_url.to_string();

  let handle = std::thread::spawn(move || {
    websocket_thread_main::<Host>(
      env_id,
      ws_id,
      resolved_url_string,
      protocols_header,
      cmd_rx,
      task_queue,
    )
  });

  let _ = with_env_state_mut(env_id, |state| {
    if let Some(ws) = state.sockets.get_mut(&ws_id) {
      ws.thread = Some(handle);
    }
    Ok(())
  });

  Ok(Value::Object(obj))
}

fn websocket_ready_state_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, ws_id, _obj) = parse_env_and_ws_id(scope, this)?;
  with_env_state(env_id, |state| {
    let ws = state
      .sockets
      .get(&ws_id)
      .ok_or(VmError::TypeError("WebSocket is not initialized"))?;
    Ok(Value::Number(ws.ready_state as f64))
  })
}

fn websocket_url_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, ws_id, _obj) = parse_env_and_ws_id(scope, this)?;
  with_env_state(env_id, |state| {
    let ws = state
      .sockets
      .get(&ws_id)
      .ok_or(VmError::TypeError("WebSocket is not initialized"))?;
    let s = scope.alloc_string(&ws.url)?;
    scope.push_root(Value::String(s))?;
    Ok(Value::String(s))
  })
}

fn websocket_protocol_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, ws_id, _obj) = parse_env_and_ws_id(scope, this)?;
  with_env_state(env_id, |state| {
    let ws = state
      .sockets
      .get(&ws_id)
      .ok_or(VmError::TypeError("WebSocket is not initialized"))?;
    let s = scope.alloc_string(&ws.protocol)?;
    scope.push_root(Value::String(s))?;
    Ok(Value::String(s))
  })
}

fn websocket_buffered_amount_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, ws_id, _obj) = parse_env_and_ws_id(scope, this)?;
  with_env_state(env_id, |state| {
    let ws = state
      .sockets
      .get(&ws_id)
      .ok_or(VmError::TypeError("WebSocket is not initialized"))?;
    Ok(Value::Number(ws.buffered_amount as f64))
  })
}

fn websocket_send<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, ws_id, _obj) = parse_env_and_ws_id(scope, this)?;
  let data = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(data, Value::Undefined) {
    return Err(VmError::TypeError("WebSocket.send requires an argument"));
  }

  let mut kind: Option<WsCommand> = None;
  let mut byte_len: usize = 0;

  match data {
    Value::Object(obj) if scope.heap().is_array_buffer_object(obj) => {
      let bytes = scope.heap().array_buffer_data(obj)?.to_vec();
      byte_len = bytes.len();
      kind = Some(WsCommand::SendBinary(bytes));
    }
    Value::Object(obj) if scope.heap().is_uint8_array_object(obj) => {
      let bytes = scope.heap().uint8_array_data(obj)?.to_vec();
      byte_len = bytes.len();
      kind = Some(WsCommand::SendBinary(bytes));
    }
    other => {
      if let Some(blob) = window_blob::clone_blob_data_for_fetch(vm, scope.heap(), other)? {
        byte_len = blob.bytes.len();
        kind = Some(WsCommand::SendBinary(blob.bytes));
      } else {
        let s = to_rust_string_limited(scope.heap_mut(), other, MAX_WEBSOCKET_MESSAGE_BYTES, "WebSocket message too large")?;
        byte_len = s.as_bytes().len();
        kind = Some(WsCommand::SendText(s));
      }
    }
  }

  if byte_len > MAX_WEBSOCKET_MESSAGE_BYTES {
    return Err(VmError::TypeError("WebSocket message too large"));
  }

  let cmd = kind.ok_or(VmError::TypeError("WebSocket data unsupported"))?;

  let cmd_tx = with_env_state_mut(env_id, |state| {
    let ws = state
      .sockets
      .get_mut(&ws_id)
      .ok_or(VmError::TypeError("WebSocket is not initialized"))?;
    if ws.ready_state != WS_OPEN {
      return Err(VmError::TypeError("WebSocket is not open"));
    }
    ws.buffered_amount = ws.buffered_amount.saturating_add(byte_len);
    Ok(ws.cmd_tx.clone())
  })?;

  match cmd_tx.try_send(cmd) {
    Ok(()) => Ok(Value::Undefined),
    Err(mpsc::TrySendError::Full(_)) => Err(VmError::TypeError("WebSocket send queue is full")),
    Err(mpsc::TrySendError::Disconnected(_)) => Err(VmError::TypeError("WebSocket is closed")),
  }
}

fn number_to_u16_wrapping(n: f64) -> u16 {
  if !n.is_finite() {
    return 0;
  }
  let n = n.trunc();
  (n.rem_euclid(65536.0)) as u16
}

fn websocket_close<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, ws_id, _obj) = parse_env_and_ws_id(scope, this)?;

  let code_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let reason_value = args.get(1).copied().unwrap_or(Value::Undefined);

  let code: Option<u16> = if matches!(code_value, Value::Undefined | Value::Null) {
    None
  } else {
    let n = scope.to_number(vm, host, hooks, code_value)?;
    Some(number_to_u16_wrapping(n))
  };

  let reason: Option<String> = if matches!(reason_value, Value::Undefined | Value::Null) {
    None
  } else {
    let s = to_rust_string_limited(
      scope.heap_mut(),
      reason_value,
      MAX_WEBSOCKET_CLOSE_REASON_BYTES,
      "WebSocket close reason too long",
    )?;
    Some(s)
  };

  let cmd_tx = with_env_state_mut(env_id, |state| {
    let ws = state
      .sockets
      .get_mut(&ws_id)
      .ok_or(VmError::TypeError("WebSocket is not initialized"))?;
    if ws.ready_state == WS_CLOSING || ws.ready_state == WS_CLOSED {
      return Ok(None);
    }
    ws.ready_state = WS_CLOSING;
    Ok(Some(ws.cmd_tx.clone()))
  })?;

  let Some(cmd_tx) = cmd_tx else {
    return Ok(Value::Undefined);
  };

  let cmd = WsCommand::Close { code, reason };
  let _ = cmd_tx.try_send(cmd);
  Ok(Value::Undefined)
}

fn decrement_pending_event_count(env_id: u64, ws_id: u64) {
  let _ = with_env_state_mut(env_id, |state| {
    if let Some(ws) = state.sockets.get_mut(&ws_id) {
      ws.pending_events = ws.pending_events.saturating_sub(1);
    }
    Ok(())
  });
}

fn queue_ws_task<Host: WindowRealmHost + 'static>(
  queue: &ExternalTaskQueueHandle<Host>,
  env_id: u64,
  ws_id: u64,
  f: impl FnOnce(
      &mut dyn VmHost,
      &mut vm_js::Heap,
      &mut vm_js::Vm,
      &mut VmJsEventLoopHooks<Host>,
      GcObject,
    ) -> Result<(), VmError>
    + Send
    + 'static,
) {
  // Enforce per-socket cap first.
  let allowed = with_env_state_mut(env_id, |state| {
    let Some(ws) = state.sockets.get_mut(&ws_id) else {
      return Ok(false);
    };
    if ws.pending_events >= MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET {
      return Ok(false);
    }
    ws.pending_events += 1;
    Ok(true)
  })
  .unwrap_or(false);
  if !allowed {
    return;
  }

  let queue_result = queue.queue_task(TaskSource::Networking, move |host, event_loop| {
    struct PendingGuard {
      env_id: u64,
      ws_id: u64,
    }
    impl Drop for PendingGuard {
      fn drop(&mut self) {
        decrement_pending_event_count(self.env_id, self.ws_id);
      }
    }
    let _pending = PendingGuard { env_id, ws_id };

    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
    hooks.set_event_loop(event_loop);
    let (vm_host, window_realm) = host.vm_host_and_window_realm();
    window_realm.reset_interrupt();
    let budget = window_realm.vm_budget_now();
    let (vm, heap) = window_realm.vm_and_heap_mut();
    let mut vm = vm.push_budget(budget);

    vm.tick()
      .map_err(|err| vm_error_to_event_loop_error(heap, err))?;

    // Resolve WS object. If the wrapper is no longer reachable, skip dispatch.
    let ws_obj: Option<GcObject> = with_env_state(env_id, |state| {
      Ok(state.sockets.get(&ws_id).and_then(|ws| ws.weak_obj.upgrade(heap)))
    })
    .unwrap_or(None);
    let Some(ws_obj) = ws_obj else {
      return Ok(());
    };

    let call_result = (|| {
      let result = f(vm_host, heap, &mut vm, &mut hooks, ws_obj);
      match result {
        Ok(()) => Ok(()),
        Err(err) => match err {
          VmError::Throw(_) | VmError::ThrowWithStack { .. } => Ok(()),
          other => Err(other),
        },
      }
    })();

    if let Some(err) = hooks.finish(heap) {
      return Err(err);
    }

    match call_result {
      Ok(()) => Ok(()),
      Err(err) => Err(vm_error_to_event_loop_error(heap, err)),
    }
  });

  if queue_result.is_err() {
    decrement_pending_event_count(env_id, ws_id);
  }
}

fn dispatch_ws_event(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  ws_obj: GcObject,
  event_obj: Value,
  handler_prop: &str,
) -> Result<(), VmError> {
  scope.push_root(Value::Object(ws_obj))?;
  scope.push_root(event_obj)?;

  let dispatch_key = alloc_key(scope, "dispatchEvent")?;
  let dispatch_fn = vm.get_with_host_and_hooks(host, scope, hooks, ws_obj, dispatch_key)?;
  if scope.heap().is_callable(dispatch_fn).unwrap_or(false) {
    let _ = vm.call_with_host_and_hooks(host, scope, hooks, dispatch_fn, Value::Object(ws_obj), &[event_obj]);
  }

  let handler_key = alloc_key(scope, handler_prop)?;
  let handler_val = vm.get_with_host_and_hooks(host, scope, hooks, ws_obj, handler_key)?;
  if scope.heap().is_callable(handler_val).unwrap_or(false) {
    let _ = vm.call_with_host_and_hooks(host, scope, hooks, handler_val, Value::Object(ws_obj), &[event_obj]);
  }
  Ok(())
}

fn make_simple_event(scope: &mut Scope<'_>, event_type: &str) -> Result<GcObject, VmError> {
  let ev = scope.alloc_object()?;
  scope.push_root(Value::Object(ev))?;
  let type_s = scope.alloc_string(event_type)?;
  scope.push_root(Value::String(type_s))?;
  let type_key = alloc_key(scope, "type")?;
  scope.define_property(ev, type_key, data_desc(Value::String(type_s), false))?;
  Ok(ev)
}

fn websocket_thread_main<Host: WindowRealmHost + 'static>(
  env_id: u64,
  ws_id: u64,
  url: String,
  protocols_header: Option<String>,
  cmd_rx: mpsc::Receiver<WsCommand>,
  task_queue: ExternalTaskQueueHandle<Host>,
) {
  let mut request = match url.clone().into_client_request() {
    Ok(req) => req,
    Err(_) => {
      // URL parsing should already have been validated at construction time.
      with_env_state_mut(env_id, |state| {
        if let Some(ws) = state.sockets.get_mut(&ws_id) {
          ws.ready_state = WS_CLOSED;
        }
        Ok(())
      })
      .ok();
      queue_ws_task::<Host>(&task_queue, env_id, ws_id, |vm_host, heap, vm, hooks, ws_obj| {
        let mut scope = heap.scope();
        let ev = make_simple_event(&mut scope, "error")?;
        dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onerror")?;
        let close_ev = make_simple_event(&mut scope, "close")?;
        dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(close_ev), "onclose")?;
        Ok(())
      });
      return;
    }
  };

  if let Some(header) = protocols_header.as_deref() {
    request
      .headers_mut()
      .insert("Sec-WebSocket-Protocol", header.parse().unwrap());
  }

  let connect_result = connect(request);
  let (mut socket, response) = match connect_result {
    Ok(pair) => pair,
    Err(_err) => {
      with_env_state_mut(env_id, |state| {
        if let Some(ws) = state.sockets.get_mut(&ws_id) {
          ws.ready_state = WS_CLOSED;
        }
        Ok(())
      })
      .ok();
      queue_ws_task::<Host>(&task_queue, env_id, ws_id, |vm_host, heap, vm, hooks, ws_obj| {
        let mut scope = heap.scope();
        let ev = make_simple_event(&mut scope, "error")?;
        dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onerror")?;
        let close_ev = make_simple_event(&mut scope, "close")?;
        dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(close_ev), "onclose")?;
        Ok(())
      });
      return;
    }
  };

  let selected_protocol = response
    .headers()
    .get("Sec-WebSocket-Protocol")
    .and_then(|h| h.to_str().ok())
    .unwrap_or("")
    .to_string();

  with_env_state_mut(env_id, |state| {
    if let Some(ws) = state.sockets.get_mut(&ws_id) {
      ws.ready_state = WS_OPEN;
      ws.protocol = selected_protocol.clone();
    }
    Ok(())
  })
  .ok();

  queue_ws_task::<Host>(&task_queue, env_id, ws_id, |vm_host, heap, vm, hooks, ws_obj| {
    let mut scope = heap.scope();
    let ev = make_simple_event(&mut scope, "open")?;
    dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onopen")?;
    Ok(())
  });

  // Keep the socket responsive to shutdown by using a small read timeout.
  //
  // NOTE: This is best-effort. For TLS streams we still attempt to apply the timeout to the
  // underlying TCP socket when accessible.
  match socket.get_ref() {
    tungstenite::stream::MaybeTlsStream::Plain(stream) => {
      let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    }
    tungstenite::stream::MaybeTlsStream::Rustls(stream) => {
      let _ = stream
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(50)));
    }
    #[allow(unreachable_patterns)]
    _ => {}
  }

  let mut closing: Option<(u16, String)> = None;

  loop {
    // Drain commands.
    loop {
      match cmd_rx.try_recv() {
        Ok(WsCommand::SendText(s)) => {
          let len = s.as_bytes().len();
          let write_res = socket.write_message(Message::Text(s));
          with_env_state_mut(env_id, |state| {
            if let Some(ws) = state.sockets.get_mut(&ws_id) {
              ws.buffered_amount = ws.buffered_amount.saturating_sub(len);
            }
            Ok(())
          })
          .ok();
          if write_res.is_err() {
            closing = Some((1006, "".to_string()));
            break;
          }
        }
        Ok(WsCommand::SendBinary(bytes)) => {
          let len = bytes.len();
          let write_res = socket.write_message(Message::Binary(bytes));
          with_env_state_mut(env_id, |state| {
            if let Some(ws) = state.sockets.get_mut(&ws_id) {
              ws.buffered_amount = ws.buffered_amount.saturating_sub(len);
            }
            Ok(())
          })
          .ok();
          if write_res.is_err() {
            closing = Some((1006, "".to_string()));
            break;
          }
        }
        Ok(WsCommand::Close { code, reason }) => {
          let code = code.unwrap_or(1000);
          let reason = reason.unwrap_or_default();
          closing = Some((code, reason.clone()));
          let frame = CloseFrame {
            code: tungstenite::protocol::frame::coding::CloseCode::from(code),
            reason: Cow::Owned(reason),
          };
          let _ = socket.close(Some(frame));
          break;
        }
        Ok(WsCommand::Shutdown) => {
          closing = Some((1001, "shutdown".to_string()));
          let _ = socket.close(None);
          break;
        }
        Err(mpsc::TryRecvError::Empty) => break,
        Err(mpsc::TryRecvError::Disconnected) => {
          closing = Some((1001, "shutdown".to_string()));
          let _ = socket.close(None);
          break;
        }
      }
    }

    if closing.is_some() {
      break;
    }

    match socket.read_message() {
      Ok(Message::Text(text)) => {
        if text.as_bytes().len() > MAX_WEBSOCKET_MESSAGE_BYTES {
          closing = Some((1009, "message too large".to_string()));
          let _ = socket.close(None);
          break;
        }

        queue_ws_task::<Host>(&task_queue, env_id, ws_id, move |vm_host, heap, vm, hooks, ws_obj| {
          let mut scope = heap.scope();
          let ev = make_simple_event(&mut scope, "message")?;
          let data_s = scope.alloc_string(&text)?;
          scope.push_root(Value::String(data_s))?;
          let data_key = alloc_key(&mut scope, "data")?;
          scope.define_property(ev, data_key, data_desc(Value::String(data_s), false))?;
          dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onmessage")?;
          Ok(())
        });
      }
      Ok(Message::Binary(bytes)) => {
        if bytes.len() > MAX_WEBSOCKET_MESSAGE_BYTES {
          closing = Some((1009, "message too large".to_string()));
          let _ = socket.close(None);
          break;
        }

        queue_ws_task::<Host>(&task_queue, env_id, ws_id, move |vm_host, heap, vm, hooks, ws_obj| {
          let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
            "WebSocket message dispatch requires intrinsics",
          ))?;
          let mut scope = heap.scope();
          let ev = make_simple_event(&mut scope, "message")?;
          let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
          scope.push_root(Value::Object(ab))?;
          scope
            .heap_mut()
            .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
          let data_key = alloc_key(&mut scope, "data")?;
          scope.define_property(ev, data_key, data_desc(Value::Object(ab), false))?;
          dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onmessage")?;
          Ok(())
        });
      }
      Ok(Message::Close(frame)) => {
        let (code, reason) = frame
          .as_ref()
          .map(|f| (u16::from(f.code), f.reason.to_string()))
          .unwrap_or((1000, "".to_string()));
        closing = Some((code, reason));
        // Reply close.
        let _ = socket.close(frame);
        break;
      }
      Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
      Err(tungstenite::Error::Io(ref io)) if io.kind() == std::io::ErrorKind::TimedOut => {}
      Err(tungstenite::Error::Io(ref io)) if io.kind() == std::io::ErrorKind::WouldBlock => {}
      Err(_) => {
        closing = Some((1006, "".to_string()));
        break;
      }
      _ => {}
    }
  }

  with_env_state_mut(env_id, |state| {
    if let Some(ws) = state.sockets.get_mut(&ws_id) {
      ws.ready_state = WS_CLOSED;
      ws.buffered_amount = 0;
    }
    Ok(())
  })
  .ok();

  let (code, reason) = closing.unwrap_or((1000, "".to_string()));

  queue_ws_task::<Host>(&task_queue, env_id, ws_id, move |vm_host, heap, vm, hooks, ws_obj| {
    let mut scope = heap.scope();
    let ev = make_simple_event(&mut scope, "close")?;
    let code_key = alloc_key(&mut scope, "code")?;
    scope.define_property(ev, code_key, data_desc(Value::Number(code as f64), false))?;
    let reason_key = alloc_key(&mut scope, "reason")?;
    let reason_s = scope.alloc_string(&reason)?;
    scope.push_root(Value::String(reason_s))?;
    scope.define_property(ev, reason_key, data_desc(Value::String(reason_s), false))?;
    dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onclose")?;
    Ok(())
  });
}

/// Install WebSocket bindings onto the window global object.
///
/// Returns an env id that can be passed to [`unregister_window_websocket_env`] to tear down the
/// backing Rust state when the realm/host is dropped.
pub fn install_window_websocket_bindings<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  env: WindowWebSocketEnv,
) -> Result<u64, VmError> {
  let bindings = install_window_websocket_bindings_with_guard::<Host>(vm, realm, heap, env)?;
  Ok(bindings.disarm())
}

/// Install WebSocket bindings onto the window global object, returning an RAII guard that
/// automatically unregisters the backing Rust state when dropped.
pub fn install_window_websocket_bindings_with_guard<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  env: WindowWebSocketEnv,
) -> Result<WindowWebSocketBindings, VmError> {
  let env_id = NEXT_ENV_ID.fetch_add(1, Ordering::Relaxed);
  {
    let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.insert(env_id, EnvState::new(env));
  }
  let bindings = WindowWebSocketBindings::new(env_id);

  let intr = realm.intrinsics();
  let func_proto = intr.function_prototype();

  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // Look up `EventTarget.prototype` installed by `WindowRealm`.
  let event_target_proto = {
    let event_target_key = alloc_key(&mut scope, "EventTarget")?;
    let ctor = scope
      .heap()
      .object_get_own_data_property_value(global, &event_target_key)?
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      })
      .ok_or(VmError::Unimplemented(
        "EventTarget is not installed on the global object",
      ))?;

    let proto_key = alloc_key(&mut scope, "prototype")?;
    scope
      .heap()
      .object_get_own_data_property_value(ctor, &proto_key)?
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      })
      .ok_or(VmError::Unimplemented("EventTarget.prototype is missing"))?
  };

  let call_id: NativeFunctionId = vm.register_native_call(websocket_ctor_call)?;
  let construct_id: NativeConstructId = vm.register_native_construct(websocket_ctor_construct::<Host>)?;
  let name_s = scope.alloc_string("WebSocket")?;
  scope.push_root(Value::String(name_s))?;
  let ctor = scope.alloc_native_function_with_slots(
    call_id,
    Some(construct_id),
    name_s,
    1,
    &[Value::Number(env_id as f64)],
  )?;
  scope.push_root(Value::Object(ctor))?;
  scope.heap_mut().object_set_prototype(ctor, Some(func_proto))?;

  // Prototype created by vm-js.
  let Value::Object(proto) = get_data_prop(&mut scope, ctor, "prototype")? else {
    return Err(VmError::InvariantViolation("WebSocket.prototype missing"));
  };
  scope.push_root(Value::Object(proto))?;
  scope
    .heap_mut()
    .object_set_prototype(proto, Some(event_target_proto))?;

  // Constants (also available on instances via prototype).
  for (name, value) in [
    ("CONNECTING", WS_CONNECTING),
    ("OPEN", WS_OPEN),
    ("CLOSING", WS_CLOSING),
    ("CLOSED", WS_CLOSED),
  ] {
    set_data_prop(&mut scope, ctor, name, Value::Number(value as f64), false)?;
    set_data_prop(&mut scope, proto, name, Value::Number(value as f64), false)?;
  }

  // Methods.
  let send_id = vm.register_native_call(websocket_send::<Host>)?;
  let send_name = scope.alloc_string("send")?;
  scope.push_root(Value::String(send_name))?;
  let send_fn = scope.alloc_native_function(send_id, None, send_name, 1)?;
  scope.heap_mut().object_set_prototype(send_fn, Some(func_proto))?;
  set_data_prop(&mut scope, proto, "send", Value::Object(send_fn), true)?;

  let close_id = vm.register_native_call(websocket_close::<Host>)?;
  let close_name = scope.alloc_string("close")?;
  scope.push_root(Value::String(close_name))?;
  let close_fn = scope.alloc_native_function(close_id, None, close_name, 2)?;
  scope.heap_mut().object_set_prototype(close_fn, Some(func_proto))?;
  set_data_prop(&mut scope, proto, "close", Value::Object(close_fn), true)?;

  // Accessors.
  let ready_get_id = vm.register_native_call(websocket_ready_state_get)?;
  let ready_get_name = scope.alloc_string("get readyState")?;
  scope.push_root(Value::String(ready_get_name))?;
  let ready_get_fn = scope.alloc_native_function(ready_get_id, None, ready_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(ready_get_fn, Some(func_proto))?;
  let ready_key = alloc_key(&mut scope, "readyState")?;
  scope.define_property(proto, ready_key, accessor_desc(Value::Object(ready_get_fn), Value::Undefined))?;

  let url_get_id = vm.register_native_call(websocket_url_get)?;
  let url_get_name = scope.alloc_string("get url")?;
  scope.push_root(Value::String(url_get_name))?;
  let url_get_fn = scope.alloc_native_function(url_get_id, None, url_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(url_get_fn, Some(func_proto))?;
  let url_key = alloc_key(&mut scope, "url")?;
  scope.define_property(proto, url_key, accessor_desc(Value::Object(url_get_fn), Value::Undefined))?;

  let protocol_get_id = vm.register_native_call(websocket_protocol_get)?;
  let protocol_get_name = scope.alloc_string("get protocol")?;
  scope.push_root(Value::String(protocol_get_name))?;
  let protocol_get_fn = scope.alloc_native_function(protocol_get_id, None, protocol_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(protocol_get_fn, Some(func_proto))?;
  let protocol_key = alloc_key(&mut scope, "protocol")?;
  scope.define_property(
    proto,
    protocol_key,
    accessor_desc(Value::Object(protocol_get_fn), Value::Undefined),
  )?;

  let buffered_get_id = vm.register_native_call(websocket_buffered_amount_get)?;
  let buffered_get_name = scope.alloc_string("get bufferedAmount")?;
  scope.push_root(Value::String(buffered_get_name))?;
  let buffered_get_fn = scope.alloc_native_function(buffered_get_id, None, buffered_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(buffered_get_fn, Some(func_proto))?;
  let buffered_key = alloc_key(&mut scope, "bufferedAmount")?;
  scope.define_property(
    proto,
    buffered_key,
    accessor_desc(Value::Object(buffered_get_fn), Value::Undefined),
  )?;

  // Expose on global.
  let ctor_key = alloc_key(&mut scope, "WebSocket")?;
  scope.define_property(global, ctor_key, data_desc(Value::Object(ctor), true))?;

  Ok(bindings)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom2;
  use crate::error::Result;
  use crate::js::{RunLimits, WindowHost};
  use selectors::context::QuirksMode;
  use std::net::TcpListener;
  use std::time::Instant;

  fn get_global_prop_utf8(host: &mut WindowHost, name: &str) -> Option<String> {
    let window = host.host_mut().window_mut();
    let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global)).ok()?;
    let key_s = scope.alloc_string(name).ok()?;
    scope.push_root(Value::String(key_s)).ok()?;
    let key = PropertyKey::from_string(key_s);
    let val = scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .ok()
      .flatten()
      .unwrap_or(Value::Undefined);
    match val {
      Value::String(s) => scope.heap().get_string(s).ok().map(|s| s.to_utf8_lossy()),
      Value::Bool(b) => Some(b.to_string()),
      Value::Number(n) => Some(n.to_string()),
      Value::Undefined => None,
      _ => Some(format!("{val:?}")),
    }
  }

  #[test]
  fn websocket_connect_send_echo_close() -> Result<()> {
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
      // Some sandboxed CI environments may forbid binding sockets; skip in that case.
      return Ok(());
    };
    listener.set_nonblocking(true).expect("set_nonblocking");
    let addr = listener.local_addr().expect("local_addr");

    let server = std::thread::spawn(move || {
      let deadline = Instant::now() + Duration::from_secs(5);
      loop {
        match listener.accept() {
          Ok((stream, _)) => {
            let mut stream = stream;
            // Make the test deterministic: if the client never completes the handshake or never
            // sends a message, we want a bounded failure instead of hanging forever.
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let mut ws = tungstenite::accept(stream).expect("accept websocket");
            let read_deadline = Instant::now() + Duration::from_secs(5);
            let msg = loop {
              match ws.read_message() {
                Ok(msg) => break msg,
                Err(tungstenite::Error::Io(ref err))
                  if matches!(err.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) =>
                {
                  if Instant::now() >= read_deadline {
                    panic!("server read timed out");
                  }
                }
                Err(err) => panic!("server read failed: {err}"),
              }
            };
            ws.write_message(msg).expect("echo");
            let _ = ws.close(None);
            break;
          }
          Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            if Instant::now() >= deadline {
              panic!("accept timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
          }
          Err(e) => panic!("accept failed: {e}"),
        }
      }
    });

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script(&format!(
      r#"
      globalThis.__done = false;
      globalThis.__err = "";
      globalThis.__msg = "";
      globalThis.__ws = new WebSocket("ws://{addr}/");
      const ws = globalThis.__ws;
      ws.onopen = function () {{
        ws.send("hello");
      }};
      ws.onmessage = function (e) {{
        globalThis.__msg = String(e && e.data);
        ws.close();
      }};
      ws.onerror = function (e) {{
        globalThis.__err = "error";
        globalThis.__done = true;
      }};
      ws.onclose = function () {{
        globalThis.__done = true;
      }};
      "#,
    ))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 100,
        max_microtasks: 1000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;

      let done = get_global_prop_utf8(&mut host, "__done").unwrap_or_default();
      if done == "true" {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      "",
      "unexpected websocket error: {:?}",
      get_global_prop_utf8(&mut host, "__err")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__msg").as_deref(),
      Some("hello")
    );

    server.join().expect("server thread panicked");
    Ok(())
  }
}
