//! Minimal `XMLHttpRequest` binding for the `vm-js` Window realm.
//!
//! This is intentionally an MVP implementation that targets the subset commonly assumed by
//! real-world scripts and analytics/instrumentation libraries:
//!
//! - `new XMLHttpRequest()`
//! - `open()` / `setRequestHeader()` / `send()` / `abort()`
//! - `readyState`, `status`, `statusText`, `responseType`, `responseText`, `response`
//! - Event handler properties (`onload`, `onerror`, etc) and `addEventListener`/`removeEventListener`
//!
//! The primary goal is to avoid `ReferenceError: XMLHttpRequest is not defined` crashes when
//! executing offline fixtures.

use crate::js::event_loop::TaskSource;
use crate::js::url_resolve::resolve_url;
use crate::js::window_realm::{WindowRealmHost, WindowRealmUserData};
use crate::js::window_timers::{
  event_loop_mut_from_hooks, vm_error_to_event_loop_error, VmJsEventLoopHooks,
};
use crate::resource::web_fetch::WebFetchLimits;
use crate::resource::{
  origin_from_url, DocumentOrigin, FetchCredentialsMode, FetchDestination, FetchRequest,
  FetchedResource, HttpRequest, ReferrerPolicy, ResourceFetcher,
};
use http::StatusCode;
use std::char::decode_utf16;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use vm_js::{
  GcObject, Heap, NativeConstructId, NativeFunctionId, PropertyDescriptor, PropertyKey,
  PropertyKind, Realm, RootId, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
};

const XHR_UNSENT: u8 = 0;
const XHR_OPENED: u8 = 1;
const XHR_HEADERS_RECEIVED: u8 = 2;
const XHR_LOADING: u8 = 3;
const XHR_DONE: u8 = 4;

const ENV_ID_KEY: &str = "__fastrender_xhr_env_id";
const XHR_ID_KEY: &str = "__fastrender_xhr_id";
const LISTENERS_KEY: &str = "__fastrender_xhr_listeners";

// Conservative, defensive limits.
const XHR_METHOD_MAX_BYTES: usize = 64;
const XHR_EVENT_TYPE_MAX_BYTES: usize = 128;
const XHR_STATUS_TEXT_MAX_BYTES: usize = 128;

const XHR_URL_TOO_LONG_ERROR: &str = "XMLHttpRequest.open URL exceeds maximum length";
const XHR_METHOD_TOO_LONG_ERROR: &str = "XMLHttpRequest.open method exceeds maximum length";
const XHR_HEADER_NAME_TOO_LONG_ERROR: &str =
  "XMLHttpRequest.setRequestHeader name exceeds maximum length";
const XHR_HEADER_VALUE_TOO_LONG_ERROR: &str =
  "XMLHttpRequest.setRequestHeader value exceeds maximum length";
const XHR_BODY_TOO_LONG_ERROR: &str = "XMLHttpRequest.send body exceeds maximum length";
const XHR_RESPONSE_TYPE_TOO_LONG_ERROR: &str = "XMLHttpRequest.responseType exceeds maximum length";
const XHR_EVENT_TYPE_TOO_LONG_ERROR: &str = "XMLHttpRequest event type exceeds maximum length";

#[derive(Clone)]
pub struct WindowXhrEnv {
  pub fetcher: Arc<dyn ResourceFetcher>,
  pub document_url: Option<String>,
  pub document_origin: Option<DocumentOrigin>,
  pub referrer_policy: ReferrerPolicy,
  pub limits: WebFetchLimits,
}

impl WindowXhrEnv {
  pub fn for_document(fetcher: Arc<dyn ResourceFetcher>, document_url: Option<String>) -> Self {
    let document_origin = document_url.as_deref().and_then(origin_from_url);
    Self {
      fetcher,
      document_url,
      document_origin,
      referrer_policy: ReferrerPolicy::default(),
      limits: WebFetchLimits::default(),
    }
  }

  pub fn with_limits(mut self, limits: WebFetchLimits) -> Self {
    self.limits = limits;
    self
  }
}

struct EnvState {
  env: WindowXhrEnv,
  next_xhr_id: u64,
  xhrs: HashMap<u64, XhrState>,
}

impl EnvState {
  fn new(env: WindowXhrEnv) -> Self {
    Self {
      env,
      next_xhr_id: 1,
      xhrs: HashMap::new(),
    }
  }

  fn alloc_xhr_id(&mut self) -> u64 {
    let id = self.next_xhr_id;
    self.next_xhr_id = self.next_xhr_id.saturating_add(1);
    id
  }
}

#[derive(Debug, Clone)]
struct RequestSnapshot {
  method: String,
  url: String,
  headers: Vec<(String, String)>,
  body: Option<Vec<u8>>,
}

#[derive(Debug)]
struct XhrState {
  ready_state: u8,
  response_type: String,
  response_bytes: Vec<u8>,
  response_text: String,
  status: u16,
  status_text: String,
  with_credentials: bool,
  async_flag: bool,

  request: Option<RequestSnapshot>,
  // Monotonic counter incremented for each `send()`. Used to ignore stale tasks.
  request_seq: u64,
  send_in_progress: bool,
  aborted: bool,

  // Root holding the JS wrapper alive while an async request is pending.
  root: Option<RootId>,
}

impl Default for XhrState {
  fn default() -> Self {
    Self {
      ready_state: XHR_UNSENT,
      response_type: String::new(),
      response_bytes: Vec::new(),
      response_text: String::new(),
      status: 0,
      status_text: String::new(),
      with_credentials: false,
      async_flag: true,
      request: None,
      request_seq: 0,
      send_in_progress: false,
      aborted: false,
      root: None,
    }
  }
}

static NEXT_ENV_ID: AtomicU64 = AtomicU64::new(1);
static ENVS: OnceLock<Mutex<HashMap<u64, EnvState>>> = OnceLock::new();

fn envs() -> &'static Mutex<HashMap<u64, EnvState>> {
  ENVS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn unregister_window_xhr_env(env_id: u64) {
  let mut lock = envs()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  lock.remove(&env_id);
}

/// RAII guard returned by [`install_window_xhr_bindings_with_guard`].
#[derive(Debug)]
#[must_use = "XHR bindings are only valid while the returned WindowXhrBindings is kept alive"]
pub struct WindowXhrBindings {
  env_id: u64,
  active: bool,
}

impl WindowXhrBindings {
  fn new(env_id: u64) -> Self {
    Self {
      env_id,
      active: true,
    }
  }

  pub fn env_id(&self) -> u64 {
    self.env_id
  }

  fn disarm(mut self) -> u64 {
    self.active = false;
    self.env_id
  }
}

impl Drop for WindowXhrBindings {
  fn drop(&mut self) {
    if self.active {
      unregister_window_xhr_env(self.env_id);
    }
  }
}

fn with_env_state<R>(
  env_id: u64,
  f: impl FnOnce(&EnvState) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let lock = envs()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = lock.get(&env_id).ok_or(VmError::Unimplemented(
    "XMLHttpRequest env id not registered",
  ))?;
  f(state)
}

fn with_env_state_mut<R>(
  env_id: u64,
  f: impl FnOnce(&mut EnvState) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let mut lock = envs()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = lock.get_mut(&env_id).ok_or(VmError::Unimplemented(
    "XMLHttpRequest env id not registered",
  ))?;
  f(state)
}

fn data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data { value, writable },
  }
}

fn read_only_data_desc(value: Value) -> PropertyDescriptor {
  data_desc(value, false)
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

fn set_data_prop(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
  writable: bool,
) -> Result<(), VmError> {
  // Root `obj` and `value` while allocating the property key: `alloc_key` can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, data_desc(value, writable))
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

fn number_to_u64(value: Value) -> Result<u64, VmError> {
  let Value::Number(n) = value else {
    return Err(VmError::TypeError("expected number"));
  };
  if !n.is_finite() || n < 0.0 || n > u64::MAX as f64 {
    return Err(VmError::TypeError("number out of range"));
  }
  Ok(n as u64)
}

fn js_string_to_rust_string_limited(
  heap: &Heap,
  handle: vm_js::GcString,
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

fn throw_error(scope: &mut Scope<'_>, message: &str) -> VmError {
  match scope.alloc_string(message) {
    Ok(s) => VmError::Throw(Value::String(s)),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

fn env_id_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<u64, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let value = slots.get(0).copied().unwrap_or(Value::Undefined);
  number_to_u64(value)
}

fn xhr_info_from_this(scope: &mut Scope<'_>, this: Value) -> Result<(u64, u64, GcObject), VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("XMLHttpRequest: illegal invocation"));
  };
  let env_id_val = get_data_prop(scope, obj, ENV_ID_KEY)?;
  let xhr_id_val = get_data_prop(scope, obj, XHR_ID_KEY)?;
  let env_id = number_to_u64(env_id_val)?;
  let xhr_id = number_to_u64(xhr_id_val)?;
  Ok((env_id, xhr_id, obj))
}

fn current_document_base_url(vm: &mut Vm, env_id: u64) -> Result<Option<String>, VmError> {
  if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
    return Ok(data.base_url.clone());
  }
  with_env_state(env_id, |state| Ok(state.env.document_url.clone()))
}

fn xhr_constructor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "XMLHttpRequest constructor requires 'new'",
  ))
}

fn xhr_constructor_construct(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;

  let proto = {
    let key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "XMLHttpRequest.prototype missing",
        ))
      }
    }
  };

  let xhr_id = with_env_state_mut(env_id, |state| {
    let id = state.alloc_xhr_id();
    state.xhrs.insert(id, XhrState::default());
    Ok(id)
  })?;

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;

  // Hidden association to Rust state.
  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(scope, obj, XHR_ID_KEY, Value::Number(xhr_id as f64), false)?;

  // Listener registry.
  let listeners = scope.alloc_object()?;
  scope.push_root(Value::Object(listeners))?;
  set_data_prop(scope, obj, LISTENERS_KEY, Value::Object(listeners), false)?;

  Ok(Value::Object(obj))
}

fn xhr_ready_state_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, xhr_id, _) = xhr_info_from_this(scope, this)?;
  let ready_state = with_env_state(env_id, |state| {
    let xhr = state
      .xhrs
      .get(&xhr_id)
      .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
    Ok(xhr.ready_state)
  })?;
  Ok(Value::Number(ready_state as f64))
}

fn xhr_status_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, xhr_id, _) = xhr_info_from_this(scope, this)?;
  let status = with_env_state(env_id, |state| {
    let xhr = state
      .xhrs
      .get(&xhr_id)
      .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
    Ok(xhr.status)
  })?;
  Ok(Value::Number(status as f64))
}

fn xhr_status_text_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, xhr_id, _) = xhr_info_from_this(scope, this)?;
  let text = with_env_state(env_id, |state| {
    let xhr = state
      .xhrs
      .get(&xhr_id)
      .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
    Ok(xhr.status_text.clone())
  })?;
  let s = scope.alloc_string(&text)?;
  Ok(Value::String(s))
}

fn xhr_response_type_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, xhr_id, _) = xhr_info_from_this(scope, this)?;
  let response_type = with_env_state(env_id, |state| {
    let xhr = state
      .xhrs
      .get(&xhr_id)
      .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
    Ok(xhr.response_type.clone())
  })?;
  let s = scope.alloc_string(&response_type)?;
  Ok(Value::String(s))
}

fn xhr_response_type_set(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, xhr_id, _) = xhr_info_from_this(scope, this)?;
  let raw = args.get(0).copied().unwrap_or(Value::Undefined);
  let raw = match raw {
    Value::Undefined | Value::Null => String::new(),
    other => to_rust_string_limited(
      scope.heap_mut(),
      other,
      XHR_EVENT_TYPE_MAX_BYTES,
      XHR_RESPONSE_TYPE_TOO_LONG_ERROR,
    )?,
  };

  let normalized = match raw.as_str() {
    "" => "",
    "text" => "text",
    "arraybuffer" => "arraybuffer",
    _ => {
      // Ignore unknown values (MVP behavior).
      return Ok(Value::Undefined);
    }
  };

  with_env_state_mut(env_id, |state| {
    let xhr = state
      .xhrs
      .get_mut(&xhr_id)
      .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
    xhr.response_type = normalized.to_string();
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn xhr_with_credentials_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, xhr_id, _) = xhr_info_from_this(scope, this)?;
  let value = with_env_state(env_id, |state| {
    let xhr = state
      .xhrs
      .get(&xhr_id)
      .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
    Ok(xhr.with_credentials)
  })?;
  Ok(Value::Bool(value))
}

fn xhr_with_credentials_set(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, xhr_id, _) = xhr_info_from_this(scope, this)?;
  let raw = args.get(0).copied().unwrap_or(Value::Undefined);
  let value = scope.heap().to_boolean(raw)?;
  with_env_state_mut(env_id, |state| {
    let xhr = state
      .xhrs
      .get_mut(&xhr_id)
      .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
    xhr.with_credentials = value;
    Ok(())
  })?;
  Ok(Value::Undefined)
}

fn xhr_response_text_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, xhr_id, _) = xhr_info_from_this(scope, this)?;
  let (response_type, text) = with_env_state(env_id, |state| {
    let xhr = state
      .xhrs
      .get(&xhr_id)
      .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
    Ok((xhr.response_type.clone(), xhr.response_text.clone()))
  })?;

  if response_type == "arraybuffer" {
    let s = scope.alloc_string("")?;
    return Ok(Value::String(s));
  }

  let s = scope.alloc_string(&text)?;
  Ok(Value::String(s))
}

fn xhr_response_get(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, xhr_id, _) = xhr_info_from_this(scope, this)?;
  let (response_type, bytes, text) = with_env_state(env_id, |state| {
    let xhr = state
      .xhrs
      .get(&xhr_id)
      .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
    Ok((
      xhr.response_type.clone(),
      xhr.response_bytes.clone(),
      xhr.response_text.clone(),
    ))
  })?;

  if response_type == "arraybuffer" {
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
    scope
      .heap_mut()
      .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
    return Ok(Value::Object(ab));
  }

  let s = scope.alloc_string(&text)?;
  Ok(Value::String(s))
}

fn xhr_open_native<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, xhr_id, xhr_obj) = xhr_info_from_this(scope, this)?;
  let limits = with_env_state(env_id, |state| Ok(state.env.limits.clone()))?;

  let method_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let url_val = args.get(1).copied().unwrap_or(Value::Undefined);
  // WebIDL: `open(..., optional boolean async = true, ...)` => `undefined` uses the default.
  let async_val = args.get(2).copied().unwrap_or(Value::Undefined);
  let async_flag = match async_val {
    Value::Undefined => true,
    other => scope.heap().to_boolean(other)?,
  };

  let method = to_rust_string_limited(
    scope.heap_mut(),
    method_val,
    XHR_METHOD_MAX_BYTES,
    XHR_METHOD_TOO_LONG_ERROR,
  )?;
  let url_input = to_rust_string_limited(
    scope.heap_mut(),
    url_val,
    limits.max_url_bytes,
    XHR_URL_TOO_LONG_ERROR,
  )?;
  let base_url = current_document_base_url(vm, env_id)?;
  let url = resolve_url(&url_input, base_url.as_deref())
    .map_err(|_| VmError::TypeError("XMLHttpRequest.open failed to resolve URL"))?;

  let old_root = with_env_state_mut(env_id, |state| {
    let xhr = state
      .xhrs
      .get_mut(&xhr_id)
      .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
    let old_root = xhr.root.take();
    // Invalidate any in-flight request tasks associated with the previous `send()`.
    xhr.request_seq = xhr.request_seq.saturating_add(1);
    xhr.ready_state = XHR_OPENED;
    xhr.status = 0;
    xhr.status_text.clear();
    xhr.response_bytes.clear();
    xhr.response_text.clear();
    xhr.send_in_progress = false;
    xhr.aborted = false;
    xhr.async_flag = async_flag;
    xhr.request = Some(RequestSnapshot {
      method,
      url,
      headers: Vec::new(),
      body: None,
    });
    Ok(old_root)
  })?;

  if let Some(root) = old_root {
    // Drop the host-owned keepalive for the previous request (if any).
    scope.heap_mut().remove_root(root);
  }

  // `open()` synchronously fires `readystatechange` in browsers. Keep this synchronous so we don't
  // need to hold a root just to deliver the OPENED transition.
  dispatch_xhr_event(vm, scope, host, hooks, xhr_obj, "readystatechange")?;
  Ok(Value::Undefined)
}

fn xhr_set_request_header_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, xhr_id, _) = xhr_info_from_this(scope, this)?;
  let limits = with_env_state(env_id, |state| Ok(state.env.limits.clone()))?;

  let name_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let value_val = args.get(1).copied().unwrap_or(Value::Undefined);

  let name = to_rust_string_limited(
    scope.heap_mut(),
    name_val,
    limits.max_total_header_bytes,
    XHR_HEADER_NAME_TOO_LONG_ERROR,
  )?;
  let value = to_rust_string_limited(
    scope.heap_mut(),
    value_val,
    limits.max_total_header_bytes,
    XHR_HEADER_VALUE_TOO_LONG_ERROR,
  )?;

  with_env_state_mut(env_id, |state| {
    let xhr = state
      .xhrs
      .get_mut(&xhr_id)
      .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
    if xhr.ready_state != XHR_OPENED || xhr.send_in_progress {
      return Err(VmError::TypeError(
        "XMLHttpRequest.setRequestHeader invalid state",
      ));
    }
    let req = xhr.request.as_mut().ok_or(VmError::TypeError(
      "XMLHttpRequest.open must be called first",
    ))?;
    if req.headers.len() >= limits.max_header_count {
      return Err(VmError::TypeError(
        "XMLHttpRequest.setRequestHeader exceeded header count limit",
      ));
    }
    let current_bytes: usize = req.headers.iter().map(|(k, v)| k.len() + v.len()).sum();
    let add_bytes = name.len() + value.len();
    if current_bytes.saturating_add(add_bytes) > limits.max_total_header_bytes {
      return Err(VmError::TypeError(
        "XMLHttpRequest.setRequestHeader exceeded total header bytes limit",
      ));
    }
    req.headers.push((name, value));
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn xhr_send_native<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, xhr_id, xhr_obj) = xhr_info_from_this(scope, this)?;

  let (fetcher, document_url, document_origin, referrer_policy, limits) =
    with_env_state(env_id, |state| {
      let env = &state.env;
      Ok((
        Arc::clone(&env.fetcher),
        env.document_url.clone(),
        env.document_origin.clone(),
        env.referrer_policy,
        env.limits.clone(),
      ))
    })?;

  // Parse body eagerly in the native call so we can enforce limits and fail fast.
  let body_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let body: Option<Vec<u8>> = match body_val {
    Value::Undefined | Value::Null => None,
    Value::Object(obj) if scope.heap().is_array_buffer_object(obj) => {
      let bytes = scope.heap().array_buffer_data(obj)?.to_vec();
      Some(bytes)
    }
    Value::Object(obj) if scope.heap().is_uint8_array_object(obj) => {
      let bytes = scope.heap().uint8_array_data(obj)?.to_vec();
      Some(bytes)
    }
    other => Some(
      to_rust_string_limited(
        scope.heap_mut(),
        other,
        limits.max_request_body_bytes,
        XHR_BODY_TOO_LONG_ERROR,
      )?
      .into_bytes(),
    ),
  };

  if body
    .as_ref()
    .is_some_and(|b| b.len() > limits.max_request_body_bytes)
  {
    return Err(VmError::TypeError(XHR_BODY_TOO_LONG_ERROR));
  }
  // Snapshot request data and transition to "in-flight" under the env lock.
  let (request_seq, request, async_flag, credentials_mode) = with_env_state_mut(env_id, |state| {
    let xhr = state
      .xhrs
      .get_mut(&xhr_id)
      .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
    if xhr.ready_state != XHR_OPENED || xhr.send_in_progress {
      return Err(VmError::TypeError("XMLHttpRequest.send invalid state"));
    }
    let credentials_mode = if xhr.with_credentials {
      FetchCredentialsMode::Include
    } else {
      FetchCredentialsMode::SameOrigin
    };
    let async_flag = xhr.async_flag;
    let mut req = xhr.request.clone().ok_or(VmError::TypeError(
      "XMLHttpRequest.open must be called first",
    ))?;
    req.body = body;
    xhr.request = Some(req.clone());
    xhr.send_in_progress = true;
    xhr.aborted = false;
    xhr.request_seq = xhr.request_seq.saturating_add(1);
    let seq = xhr.request_seq;
    Ok((seq, req, async_flag, credentials_mode))
  })?;

  if !async_flag {
    // Synchronous XHR: run the fetch inline and dispatch events synchronously. This keeps FastRender
    // compatible with scripts that still rely on `open(..., false)`.
    let fetch_req = {
      let mut fr = FetchRequest::new(&request.url, FetchDestination::Fetch)
        .with_credentials_mode(credentials_mode);
      if let Some(referrer) = document_url.as_deref() {
        fr = fr.with_referrer_url(referrer);
      }
      if let Some(origin) = document_origin.as_ref() {
        fr = fr.with_client_origin(origin);
      }
      fr = fr.with_referrer_policy(referrer_policy);
      fr
    };

    let http_req = HttpRequest {
      fetch: fetch_req,
      method: &request.method,
      redirect: crate::resource::web_fetch::RequestRedirect::Follow,
      headers: request.headers.as_slice(),
      body: request.body.as_deref(),
    };

    let result: crate::error::Result<FetchedResource> = fetcher.fetch_http_request(http_req);

    let mut is_error = false;
    let mut status: u16 = 0;
    let mut status_text: String = String::new();
    let mut bytes: Vec<u8> = Vec::new();
    match result {
      Ok(res) => {
        bytes = res.bytes;
        if bytes.len() > limits.max_response_body_bytes {
          is_error = true;
        } else {
          status = res.status.unwrap_or(200);
          status_text = StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or("")
            .to_string();
          if status_text.len() > XHR_STATUS_TEXT_MAX_BYTES {
            status_text.truncate(XHR_STATUS_TEXT_MAX_BYTES);
          }
        }
      }
      Err(_) => {
        is_error = true;
      }
    }

    let should_dispatch = with_env_state_mut(env_id, |state| {
      let xhr = state
        .xhrs
        .get_mut(&xhr_id)
        .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
      xhr.send_in_progress = false;
      xhr.ready_state = XHR_DONE;
      if is_error {
        xhr.status = 0;
        xhr.status_text.clear();
        xhr.response_bytes.clear();
        xhr.response_text.clear();
      } else {
        xhr.status = status;
        xhr.status_text = status_text;
        xhr.response_text = String::from_utf8_lossy(&bytes).to_string();
        xhr.response_bytes = bytes;
      }
      Ok(true)
    })?;

    if !should_dispatch {
      return Ok(Value::Undefined);
    }

    let outcome_event: &'static str = if is_error { "error" } else { "load" };
    let events = ["readystatechange", outcome_event, "loadend"];
    for event_type in events {
      dispatch_xhr_event(vm, scope, host, hooks, xhr_obj, event_type)?;
    }
    return Ok(Value::Undefined);
  }

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(hooks) else {
    return Err(VmError::TypeError(
      "XMLHttpRequest.send called without an active EventLoop",
    ));
  };

  // Keep the wrapper alive until the final `loadend`/`abort` event runs.
  let root = scope.heap_mut().add_root(Value::Object(xhr_obj))?;
  with_env_state_mut(env_id, |state| {
    let xhr = state
      .xhrs
      .get_mut(&xhr_id)
      .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
    xhr.root = Some(root);
    Ok(())
  })?;

  let queue_result = event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
    // Check the request is still current (not aborted/re-opened) before doing any network work.
    let should_run = with_env_state(env_id, |state| {
      let xhr = state
        .xhrs
        .get(&xhr_id)
        .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
      Ok(xhr.send_in_progress && !xhr.aborted && xhr.request_seq == request_seq)
    })
    .unwrap_or(false);
    if !should_run {
      return Ok(());
    }

    let fetch_req = {
      let mut fr = FetchRequest::new(&request.url, FetchDestination::Fetch)
        .with_credentials_mode(credentials_mode);
      if let Some(referrer) = document_url.as_deref() {
        fr = fr.with_referrer_url(referrer);
      }
      if let Some(origin) = document_origin.as_ref() {
        fr = fr.with_client_origin(origin);
      }
      fr = fr.with_referrer_policy(referrer_policy);
      fr
    };

    let http_req = HttpRequest {
      fetch: fetch_req,
      method: &request.method,
      redirect: crate::resource::web_fetch::RequestRedirect::Follow,
      headers: request.headers.as_slice(),
      body: request.body.as_deref(),
    };

    let result: crate::error::Result<FetchedResource> = fetcher.fetch_http_request(http_req);

    // Update XHR state (no JS execution here).
    let mut is_error = false;
    let mut status: u16 = 0;
    let mut status_text: String = String::new();
    let mut bytes: Vec<u8> = Vec::new();
    match result {
      Ok(res) => {
        bytes = res.bytes;
        if bytes.len() > limits.max_response_body_bytes {
          is_error = true;
        } else {
          status = res.status.unwrap_or(200);
          status_text = StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or("")
            .to_string();
          if status_text.len() > XHR_STATUS_TEXT_MAX_BYTES {
            status_text.truncate(XHR_STATUS_TEXT_MAX_BYTES);
          }
        }
      }
      Err(_) => {
        is_error = true;
      }
    }

    let should_dispatch = match with_env_state_mut(env_id, |state| {
      let xhr = state
        .xhrs
        .get_mut(&xhr_id)
        .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
      // If the request was aborted/reopened while the fetcher ran, ignore the result.
      if xhr.request_seq != request_seq || xhr.aborted {
        return Ok(false);
      }
      xhr.send_in_progress = false;
      xhr.ready_state = XHR_DONE;
      if is_error {
        xhr.status = 0;
        xhr.status_text.clear();
        xhr.response_bytes.clear();
        xhr.response_text.clear();
      } else {
        xhr.status = status;
        xhr.status_text = status_text;
        xhr.response_text = String::from_utf8_lossy(&bytes).to_string();
        xhr.response_bytes = bytes;
      }
      Ok(true)
    }) {
      Ok(value) => value,
      Err(_) => return Ok(()),
    };

    if !should_dispatch {
      return Ok(());
    }

    // Dispatch events on a follow-up task so user callbacks never run inside the networking work.
    let outcome_event: &'static str = if is_error { "error" } else { "load" };
    let events = ["readystatechange", outcome_event, "loadend"];
    let queue_dispatch = event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
      dispatch_xhr_events::<Host>(host, event_loop, env_id, xhr_id, request_seq, &events, true)
    });

    if let Err(queue_err) = queue_dispatch {
      // If we couldn't enqueue event dispatch, tear down the persistent root so we don't leak.
      let root = with_env_state_mut(env_id, |state| {
        let xhr = state
          .xhrs
          .get_mut(&xhr_id)
          .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
        Ok(xhr.root.take())
      })
      .ok()
      .flatten();
      if let Some(root) = root {
        let window_realm = host.window_realm();
        window_realm.heap_mut().remove_root(root);
      }
      return Err(queue_err);
    }

    Ok(())
  });

  if let Err(e) = queue_result {
    // If queueing fails, ensure we don't leak the persistent root.
    scope.heap_mut().remove_root(root);
    let _ = with_env_state_mut(env_id, |state| {
      if let Some(xhr) = state.xhrs.get_mut(&xhr_id) {
        xhr.root = None;
        xhr.send_in_progress = false;
      }
      Ok(())
    });
    return Err(throw_error(scope, &format!("{e}")));
  }

  Ok(Value::Undefined)
}

fn xhr_abort_native<Host: WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, xhr_id, _) = xhr_info_from_this(scope, this)?;

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(hooks) else {
    return Err(VmError::TypeError(
      "XMLHttpRequest.abort called without an active EventLoop",
    ));
  };

  // Mark as aborted synchronously so any queued networking task can observe the flag and skip work.
  let (request_seq, should_dispatch) = with_env_state_mut(env_id, |state| {
    let xhr = state
      .xhrs
      .get_mut(&xhr_id)
      .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
    if !xhr.send_in_progress {
      return Ok((xhr.request_seq, false));
    }
    xhr.aborted = true;
    xhr.send_in_progress = false;
    xhr.ready_state = XHR_DONE;
    xhr.status = 0;
    xhr.status_text.clear();
    xhr.response_bytes.clear();
    xhr.response_text.clear();
    xhr.request_seq = xhr.request_seq.saturating_add(1);
    Ok((xhr.request_seq, true))
  })?;

  if !should_dispatch {
    return Ok(Value::Undefined);
  }

  let events = ["readystatechange", "abort", "loadend"];
  let queue_result = event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
    dispatch_xhr_events::<Host>(host, event_loop, env_id, xhr_id, request_seq, &events, true)
  });

  if let Err(e) = queue_result {
    return Err(throw_error(scope, &format!("{e}")));
  }

  Ok(Value::Undefined)
}

fn get_or_create_listener_array(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  listeners_obj: GcObject,
  event_type: &str,
) -> Result<GcObject, VmError> {
  let key = alloc_key(scope, event_type)?;
  let existing = scope
    .heap()
    .object_get_own_data_property_value(listeners_obj, &key)?
    .unwrap_or(Value::Undefined);
  match existing {
    Value::Object(obj) => Ok(obj),
    _ => {
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
      let arr = scope.alloc_array(0)?;
      scope.push_root(Value::Object(arr))?;
      scope
        .heap_mut()
        .object_set_prototype(arr, Some(intr.array_prototype()))?;
      scope.define_property(listeners_obj, key, data_desc(Value::Object(arr), true))?;
      Ok(arr)
    }
  }
}

fn array_length(scope: &mut Scope<'_>, obj: GcObject) -> Result<usize, VmError> {
  let len_key = alloc_key(scope, "length")?;
  let len_val = scope
    .heap()
    .object_get_own_data_property_value(obj, &len_key)?
    .unwrap_or(Value::Undefined);
  let len_u64 = number_to_u64(len_val)?;
  usize::try_from(len_u64).map_err(|_| VmError::TypeError("array length out of range"))
}

fn xhr_add_event_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (_env_id, _xhr_id, xhr_obj) = xhr_info_from_this(scope, this)?;
  let event_type_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let listener = args.get(1).copied().unwrap_or(Value::Undefined);

  if matches!(listener, Value::Undefined | Value::Null) {
    return Ok(Value::Undefined);
  }
  if !scope.heap().is_callable(listener).unwrap_or(false) {
    return Ok(Value::Undefined);
  }

  let event_type = to_rust_string_limited(
    scope.heap_mut(),
    event_type_val,
    XHR_EVENT_TYPE_MAX_BYTES,
    XHR_EVENT_TYPE_TOO_LONG_ERROR,
  )?;
  if event_type.len() > XHR_EVENT_TYPE_MAX_BYTES {
    return Err(VmError::TypeError(XHR_EVENT_TYPE_TOO_LONG_ERROR));
  }

  let listeners_val = get_data_prop(scope, xhr_obj, LISTENERS_KEY)?;
  let Value::Object(listeners_obj) = listeners_val else {
    return Err(VmError::InvariantViolation(
      "XMLHttpRequest listener registry missing",
    ));
  };
  scope.push_root(Value::Object(listeners_obj))?;

  let arr = get_or_create_listener_array(vm, scope, listeners_obj, &event_type)?;
  scope.push_root(Value::Object(arr))?;

  // Prevent duplicates (common in libs that patch XHR).
  let len = array_length(scope, arr)?;
  for idx in 0..len {
    let key = alloc_key(scope, &idx.to_string())?;
    let existing = scope
      .heap()
      .object_get_own_data_property_value(arr, &key)?
      .unwrap_or(Value::Undefined);
    if existing == listener {
      return Ok(Value::Undefined);
    }
  }

  // Push listener at `length`.
  let idx = len;
  // Root listener while allocating the property key (`alloc_key` can GC).
  scope.push_root(listener)?;
  let key = alloc_key(scope, &idx.to_string())?;
  scope.define_property(arr, key, data_desc(listener, true))?;

  Ok(Value::Undefined)
}

fn xhr_remove_event_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (_env_id, _xhr_id, xhr_obj) = xhr_info_from_this(scope, this)?;
  let event_type_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let listener = args.get(1).copied().unwrap_or(Value::Undefined);

  let event_type = to_rust_string_limited(
    scope.heap_mut(),
    event_type_val,
    XHR_EVENT_TYPE_MAX_BYTES,
    XHR_EVENT_TYPE_TOO_LONG_ERROR,
  )?;

  let listeners_val = get_data_prop(scope, xhr_obj, LISTENERS_KEY)?;
  let Value::Object(listeners_obj) = listeners_val else {
    return Ok(Value::Undefined);
  };
  scope.push_root(Value::Object(listeners_obj))?;

  let key = alloc_key(scope, &event_type)?;
  let Some(Value::Object(arr)) = scope
    .heap()
    .object_get_own_data_property_value(listeners_obj, &key)?
  else {
    return Ok(Value::Undefined);
  };
  scope.push_root(Value::Object(arr))?;

  let len = array_length(scope, arr)?;
  let mut removed = false;
  let mut remaining: Vec<Value> = Vec::new();
  remaining
    .try_reserve(len)
    .map_err(|_| VmError::OutOfMemory)?;
  for idx in 0..len {
    let k = alloc_key(scope, &idx.to_string())?;
    let v = scope
      .heap()
      .object_get_own_data_property_value(arr, &k)?
      .unwrap_or(Value::Undefined);
    if !removed && v == listener {
      removed = true;
      continue;
    }
    remaining.push(v);
  }

  if !removed {
    return Ok(Value::Undefined);
  }

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let new_arr = scope.alloc_array(remaining.len())?;
  scope.push_root(Value::Object(new_arr))?;
  scope
    .heap_mut()
    .object_set_prototype(new_arr, Some(intr.array_prototype()))?;

  for (idx, v) in remaining.into_iter().enumerate() {
    scope.push_root(v)?;
    let k = alloc_key(scope, &idx.to_string())?;
    scope.define_property(new_arr, k, data_desc(v, true))?;
  }

  let key = alloc_key(scope, &event_type)?;
  scope.define_property(listeners_obj, key, data_desc(Value::Object(new_arr), true))?;

  Ok(Value::Undefined)
}

fn xhr_dispatch_event_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (_env_id, _xhr_id, xhr_obj) = xhr_info_from_this(scope, this)?;
  let event_val = args.get(0).copied().unwrap_or(Value::Undefined);

  let event_type = match event_val {
    Value::Object(ev) => {
      scope.push_root(Value::Object(ev))?;
      let type_key = alloc_key(scope, "type")?;
      let t = vm.get_with_host_and_hooks(host, scope, hooks, ev, type_key)?;
      to_rust_string_limited(
        scope.heap_mut(),
        t,
        XHR_EVENT_TYPE_MAX_BYTES,
        XHR_EVENT_TYPE_TOO_LONG_ERROR,
      )?
    }
    other => to_rust_string_limited(
      scope.heap_mut(),
      other,
      XHR_EVENT_TYPE_MAX_BYTES,
      XHR_EVENT_TYPE_TOO_LONG_ERROR,
    )?,
  };

  dispatch_xhr_event(vm, scope, host, hooks, xhr_obj, &event_type)?;
  Ok(Value::Bool(true))
}

fn dispatch_xhr_event(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  xhr_obj: GcObject,
  event_type: &str,
) -> Result<(), VmError> {
  // Build a minimal event object.
  let event_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(event_obj))?;
  let type_s = scope.alloc_string(event_type)?;
  scope.push_root(Value::String(type_s))?;
  set_data_prop(scope, event_obj, "type", Value::String(type_s), true)?;
  set_data_prop(scope, event_obj, "target", Value::Object(xhr_obj), true)?;
  set_data_prop(
    scope,
    event_obj,
    "currentTarget",
    Value::Object(xhr_obj),
    true,
  )?;

  // Event handler property (`onload`, ...).
  let handler_prop = match event_type {
    "readystatechange" => "onreadystatechange",
    "load" => "onload",
    "error" => "onerror",
    "abort" => "onabort",
    "timeout" => "ontimeout",
    "loadend" => "onloadend",
    other => {
      // Fallback: `on${type}`.
      // This allocation is bounded by `XHR_EVENT_TYPE_MAX_BYTES`.
      let mut s = String::with_capacity("on".len() + other.len());
      s.push_str("on");
      s.push_str(other);
      // Avoid holding `s` across allocations by creating the key immediately.
      let key = alloc_key(scope, &s)?;
      let value = vm.get_with_host_and_hooks(host, scope, hooks, xhr_obj, key)?;
      if scope.heap().is_callable(value).unwrap_or(false) {
        let _ = vm.call_with_host_and_hooks(
          host,
          scope,
          hooks,
          value,
          Value::Object(xhr_obj),
          &[Value::Object(event_obj)],
        )?;
      }
      // Still run listeners below.
      ""
    }
  };

  if !handler_prop.is_empty() {
    let key = alloc_key(scope, handler_prop)?;
    let value = vm.get_with_host_and_hooks(host, scope, hooks, xhr_obj, key)?;
    if scope.heap().is_callable(value).unwrap_or(false) {
      let _ = vm.call_with_host_and_hooks(
        host,
        scope,
        hooks,
        value,
        Value::Object(xhr_obj),
        &[Value::Object(event_obj)],
      )?;
    }
  }

  // Listener list from `addEventListener`.
  let listeners_val = get_data_prop(scope, xhr_obj, LISTENERS_KEY)?;
  let Value::Object(listeners_obj) = listeners_val else {
    return Ok(());
  };
  scope.push_root(Value::Object(listeners_obj))?;

  let key = alloc_key(scope, event_type)?;
  let Some(Value::Object(arr)) = scope
    .heap()
    .object_get_own_data_property_value(listeners_obj, &key)?
  else {
    return Ok(());
  };
  scope.push_root(Value::Object(arr))?;

  let len = array_length(scope, arr)?;
  for idx in 0..len {
    let k = alloc_key(scope, &idx.to_string())?;
    let listener = scope
      .heap()
      .object_get_own_data_property_value(arr, &k)?
      .unwrap_or(Value::Undefined);
    if scope.heap().is_callable(listener).unwrap_or(false) {
      let _ = vm.call_with_host_and_hooks(
        host,
        scope,
        hooks,
        listener,
        Value::Object(xhr_obj),
        &[Value::Object(event_obj)],
      )?;
    }
  }

  Ok(())
}

fn dispatch_xhr_events<Host: WindowRealmHost + 'static>(
  host: &mut Host,
  event_loop: &mut crate::js::EventLoop<Host>,
  env_id: u64,
  xhr_id: u64,
  request_seq: u64,
  events: &[&'static str],
  finalize: bool,
) -> crate::error::Result<()> {
  let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
  hooks.set_event_loop(event_loop);
  let (vm_host, window_realm) = host.vm_host_and_window_realm();
  window_realm.reset_interrupt();
  let budget = window_realm.vm_budget_now();
  let (vm, heap) = window_realm.vm_and_heap_mut();

  let mut vm = vm.push_budget(budget);
  let tick_result = vm.tick();
  let call_result: Result<(), VmError> = tick_result.and_then(|_| {
    // Take the keepalive root (for `send()`/`abort()` completion) up-front so it is always
    // released, even if dispatch throws.
    let (xhr_value, root_to_remove) = with_env_state_mut(env_id, |state| {
      let xhr = state
        .xhrs
        .get_mut(&xhr_id)
        .ok_or(VmError::TypeError("XMLHttpRequest: invalid backing state"))?;
      if xhr.request_seq != request_seq {
        return Ok((Value::Undefined, None));
      }
      let root = if finalize { xhr.root.take() } else { None };
      let value = root
        .and_then(|id| heap.get_root(id))
        .unwrap_or(Value::Undefined);
      Ok((value, root))
    })?;

    let result = (|| {
      let Value::Object(xhr_obj) = xhr_value else {
        return Ok(());
      };

      // Root receiver during dispatch: the callbacks can allocate/GC.
      let mut scope = heap.scope();
      scope.push_root(Value::Object(xhr_obj))?;

      for &event_type in events {
        dispatch_xhr_event(
          &mut vm, &mut scope, vm_host, &mut hooks, xhr_obj, event_type,
        )?;
      }

      Ok(())
    })();

    if let Some(root) = root_to_remove {
      heap.remove_root(root);
    }

    result
  });

  let finish_err = hooks.finish(heap);
  if let Some(err) = finish_err {
    return Err(err);
  }

  call_result
    .map_err(|err| vm_error_to_event_loop_error(heap, err))
    .map(|_| ())
}

/// Install XHR bindings onto the window global object.
///
/// Returns an env id that can be passed to [`unregister_window_xhr_env`] to tear down the backing
/// Rust state when the realm/host is dropped.
pub fn install_window_xhr_bindings<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  env: WindowXhrEnv,
) -> Result<u64, VmError> {
  let bindings = install_window_xhr_bindings_with_guard::<Host>(vm, realm, heap, env)?;
  Ok(bindings.disarm())
}

/// Install XHR bindings onto the window global object, returning an RAII guard that automatically
/// unregisters the backing Rust state when dropped.
pub fn install_window_xhr_bindings_with_guard<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  env: WindowXhrEnv,
) -> Result<WindowXhrBindings, VmError> {
  let env_id = NEXT_ENV_ID.fetch_add(1, Ordering::Relaxed);
  {
    let mut lock = envs()
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.insert(env_id, EnvState::new(env));
  }
  let bindings = WindowXhrBindings::new(env_id);

  let intr = realm.intrinsics();
  let func_proto = intr.function_prototype();
  let obj_proto = intr.object_prototype();

  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // --- XMLHttpRequest constructor -------------------------------------------
  let call_id: NativeFunctionId = vm.register_native_call(xhr_constructor_call)?;
  let construct_id: NativeConstructId = vm.register_native_construct(xhr_constructor_construct)?;
  let name_s = scope.alloc_string("XMLHttpRequest")?;
  scope.push_root(Value::String(name_s))?;
  let ctor = scope.alloc_native_function_with_slots(
    call_id,
    Some(construct_id),
    name_s,
    0,
    &[Value::Number(env_id as f64)],
  )?;
  scope.push_root(Value::Object(ctor))?;
  scope
    .heap_mut()
    .object_set_prototype(ctor, Some(func_proto))?;

  // Static constants.
  set_data_prop(
    &mut scope,
    ctor,
    "UNSENT",
    Value::Number(XHR_UNSENT as f64),
    false,
  )?;
  set_data_prop(
    &mut scope,
    ctor,
    "OPENED",
    Value::Number(XHR_OPENED as f64),
    false,
  )?;
  set_data_prop(
    &mut scope,
    ctor,
    "HEADERS_RECEIVED",
    Value::Number(XHR_HEADERS_RECEIVED as f64),
    false,
  )?;
  set_data_prop(
    &mut scope,
    ctor,
    "LOADING",
    Value::Number(XHR_LOADING as f64),
    false,
  )?;
  set_data_prop(
    &mut scope,
    ctor,
    "DONE",
    Value::Number(XHR_DONE as f64),
    false,
  )?;

  // Prototype object created by vm-js; install methods + accessors.
  let Value::Object(proto) = get_data_prop(&mut scope, ctor, "prototype")? else {
    return Err(VmError::InvariantViolation(
      "XMLHttpRequest.prototype missing",
    ));
  };
  scope.push_root(Value::Object(proto))?;
  scope
    .heap_mut()
    .object_set_prototype(proto, Some(obj_proto))?;

  // Methods.
  let open_id = vm.register_native_call(xhr_open_native::<Host>)?;
  let open_name = scope.alloc_string("open")?;
  scope.push_root(Value::String(open_name))?;
  let open_fn = scope.alloc_native_function(open_id, None, open_name, 2)?;
  scope
    .heap_mut()
    .object_set_prototype(open_fn, Some(func_proto))?;
  set_data_prop(&mut scope, proto, "open", Value::Object(open_fn), true)?;

  let set_request_header_id = vm.register_native_call(xhr_set_request_header_native)?;
  let set_request_header_name = scope.alloc_string("setRequestHeader")?;
  scope.push_root(Value::String(set_request_header_name))?;
  let set_request_header_fn =
    scope.alloc_native_function(set_request_header_id, None, set_request_header_name, 2)?;
  scope
    .heap_mut()
    .object_set_prototype(set_request_header_fn, Some(func_proto))?;
  set_data_prop(
    &mut scope,
    proto,
    "setRequestHeader",
    Value::Object(set_request_header_fn),
    true,
  )?;

  let send_id = vm.register_native_call(xhr_send_native::<Host>)?;
  let send_name = scope.alloc_string("send")?;
  scope.push_root(Value::String(send_name))?;
  let send_fn = scope.alloc_native_function(send_id, None, send_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(send_fn, Some(func_proto))?;
  set_data_prop(&mut scope, proto, "send", Value::Object(send_fn), true)?;

  let abort_id = vm.register_native_call(xhr_abort_native::<Host>)?;
  let abort_name = scope.alloc_string("abort")?;
  scope.push_root(Value::String(abort_name))?;
  let abort_fn = scope.alloc_native_function(abort_id, None, abort_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(abort_fn, Some(func_proto))?;
  set_data_prop(&mut scope, proto, "abort", Value::Object(abort_fn), true)?;

  let add_listener_id = vm.register_native_call(xhr_add_event_listener_native)?;
  let add_listener_name = scope.alloc_string("addEventListener")?;
  scope.push_root(Value::String(add_listener_name))?;
  let add_listener_fn = scope.alloc_native_function(add_listener_id, None, add_listener_name, 2)?;
  scope
    .heap_mut()
    .object_set_prototype(add_listener_fn, Some(func_proto))?;
  set_data_prop(
    &mut scope,
    proto,
    "addEventListener",
    Value::Object(add_listener_fn),
    true,
  )?;

  let remove_listener_id = vm.register_native_call(xhr_remove_event_listener_native)?;
  let remove_listener_name = scope.alloc_string("removeEventListener")?;
  scope.push_root(Value::String(remove_listener_name))?;
  let remove_listener_fn =
    scope.alloc_native_function(remove_listener_id, None, remove_listener_name, 2)?;
  scope
    .heap_mut()
    .object_set_prototype(remove_listener_fn, Some(func_proto))?;
  set_data_prop(
    &mut scope,
    proto,
    "removeEventListener",
    Value::Object(remove_listener_fn),
    true,
  )?;

  let dispatch_event_id = vm.register_native_call(xhr_dispatch_event_native)?;
  let dispatch_event_name = scope.alloc_string("dispatchEvent")?;
  scope.push_root(Value::String(dispatch_event_name))?;
  let dispatch_event_fn =
    scope.alloc_native_function(dispatch_event_id, None, dispatch_event_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(dispatch_event_fn, Some(func_proto))?;
  set_data_prop(
    &mut scope,
    proto,
    "dispatchEvent",
    Value::Object(dispatch_event_fn),
    true,
  )?;

  // Accessors.
  let ready_get_id = vm.register_native_call(xhr_ready_state_get)?;
  let ready_get_name = scope.alloc_string("get readyState")?;
  scope.push_root(Value::String(ready_get_name))?;
  let ready_get_fn = scope.alloc_native_function(ready_get_id, None, ready_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(ready_get_fn, Some(func_proto))?;
  let ready_key = alloc_key(&mut scope, "readyState")?;
  scope.define_property(
    proto,
    ready_key,
    accessor_desc(Value::Object(ready_get_fn), Value::Undefined),
  )?;

  let status_get_id = vm.register_native_call(xhr_status_get)?;
  let status_get_name = scope.alloc_string("get status")?;
  scope.push_root(Value::String(status_get_name))?;
  let status_get_fn = scope.alloc_native_function(status_get_id, None, status_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(status_get_fn, Some(func_proto))?;
  let status_key = alloc_key(&mut scope, "status")?;
  scope.define_property(
    proto,
    status_key,
    accessor_desc(Value::Object(status_get_fn), Value::Undefined),
  )?;

  let status_text_get_id = vm.register_native_call(xhr_status_text_get)?;
  let status_text_get_name = scope.alloc_string("get statusText")?;
  scope.push_root(Value::String(status_text_get_name))?;
  let status_text_get_fn =
    scope.alloc_native_function(status_text_get_id, None, status_text_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(status_text_get_fn, Some(func_proto))?;
  let status_text_key = alloc_key(&mut scope, "statusText")?;
  scope.define_property(
    proto,
    status_text_key,
    accessor_desc(Value::Object(status_text_get_fn), Value::Undefined),
  )?;

  let rt_get_id = vm.register_native_call(xhr_response_type_get)?;
  let rt_get_name = scope.alloc_string("get responseType")?;
  scope.push_root(Value::String(rt_get_name))?;
  let rt_get_fn = scope.alloc_native_function(rt_get_id, None, rt_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(rt_get_fn, Some(func_proto))?;
  let rt_set_id = vm.register_native_call(xhr_response_type_set)?;
  let rt_set_name = scope.alloc_string("set responseType")?;
  scope.push_root(Value::String(rt_set_name))?;
  let rt_set_fn = scope.alloc_native_function(rt_set_id, None, rt_set_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(rt_set_fn, Some(func_proto))?;
  let rt_key = alloc_key(&mut scope, "responseType")?;
  scope.define_property(
    proto,
    rt_key,
    accessor_desc(Value::Object(rt_get_fn), Value::Object(rt_set_fn)),
  )?;

  let wc_get_id = vm.register_native_call(xhr_with_credentials_get)?;
  let wc_get_name = scope.alloc_string("get withCredentials")?;
  scope.push_root(Value::String(wc_get_name))?;
  let wc_get_fn = scope.alloc_native_function(wc_get_id, None, wc_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(wc_get_fn, Some(func_proto))?;
  let wc_set_id = vm.register_native_call(xhr_with_credentials_set)?;
  let wc_set_name = scope.alloc_string("set withCredentials")?;
  scope.push_root(Value::String(wc_set_name))?;
  let wc_set_fn = scope.alloc_native_function(wc_set_id, None, wc_set_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(wc_set_fn, Some(func_proto))?;
  let wc_key = alloc_key(&mut scope, "withCredentials")?;
  scope.define_property(
    proto,
    wc_key,
    accessor_desc(Value::Object(wc_get_fn), Value::Object(wc_set_fn)),
  )?;

  let response_text_get_id = vm.register_native_call(xhr_response_text_get)?;
  let response_text_get_name = scope.alloc_string("get responseText")?;
  scope.push_root(Value::String(response_text_get_name))?;
  let response_text_get_fn =
    scope.alloc_native_function(response_text_get_id, None, response_text_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(response_text_get_fn, Some(func_proto))?;
  let response_text_key = alloc_key(&mut scope, "responseText")?;
  scope.define_property(
    proto,
    response_text_key,
    accessor_desc(Value::Object(response_text_get_fn), Value::Undefined),
  )?;

  let response_get_id = vm.register_native_call(xhr_response_get)?;
  let response_get_name = scope.alloc_string("get response")?;
  scope.push_root(Value::String(response_get_name))?;
  let response_get_fn = scope.alloc_native_function(response_get_id, None, response_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(response_get_fn, Some(func_proto))?;
  let response_key = alloc_key(&mut scope, "response")?;
  scope.define_property(
    proto,
    response_key,
    accessor_desc(Value::Object(response_get_fn), Value::Undefined),
  )?;

  // Event handler properties.
  set_data_prop(&mut scope, proto, "onreadystatechange", Value::Null, true)?;
  set_data_prop(&mut scope, proto, "onload", Value::Null, true)?;
  set_data_prop(&mut scope, proto, "onerror", Value::Null, true)?;
  set_data_prop(&mut scope, proto, "onabort", Value::Null, true)?;
  set_data_prop(&mut scope, proto, "ontimeout", Value::Null, true)?;
  set_data_prop(&mut scope, proto, "onloadend", Value::Null, true)?;

  // Expose global constructor.
  let xhr_key = alloc_key(&mut scope, "XMLHttpRequest")?;
  scope.define_property(global, xhr_key, read_only_data_desc(Value::Object(ctor)))?;

  Ok(bindings)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::event_loop::{EventLoop, RunLimits, RunUntilIdleOutcome, TaskSource};
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use std::sync::Mutex;
  use vm_js::PropertyKey;

  #[derive(Debug, Clone)]
  struct RecordedRequest {
    method: String,
    url: String,
    body: Option<Vec<u8>>,
  }

  #[derive(Default)]
  struct MockFetcher {
    requests: Mutex<Vec<RecordedRequest>>,
  }

  impl ResourceFetcher for MockFetcher {
    fn fetch(&self, _url: &str) -> crate::Result<FetchedResource> {
      Err(crate::error::Error::Other(
        "MockFetcher.fetch not implemented".to_string(),
      ))
    }

    fn fetch_http_request(&self, req: HttpRequest<'_>) -> crate::Result<FetchedResource> {
      self.requests.lock().unwrap().push(RecordedRequest {
        method: req.method.to_string(),
        url: req.fetch.url.to_string(),
        body: req.body.map(|b| b.to_vec()),
      });

      if req.fetch.url.contains("err") {
        return Err(crate::error::Error::Other("network error".to_string()));
      }

      let mut res = FetchedResource::new(b"hello".to_vec(), Some("text/plain".to_string()));
      res.status = Some(200);
      Ok(res)
    }
  }

  struct Host {
    host_ctx: (),
    window: WindowRealm,
    _xhr_bindings: WindowXhrBindings,
  }

  impl Host {
    fn new(fetcher: Arc<dyn ResourceFetcher>) -> Self {
      let mut window =
        WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).unwrap();
      // Install XHR.
      let xhr_bindings = {
        let (vm, realm, heap) = window.vm_realm_and_heap_mut();
        install_window_xhr_bindings_with_guard::<Host>(
          vm,
          realm,
          heap,
          WindowXhrEnv::for_document(fetcher, Some("https://example.invalid/".to_string())),
        )
        .unwrap()
      };
      Self {
        host_ctx: (),
        window,
        _xhr_bindings: xhr_bindings,
      }
    }
  }

  impl WindowRealmHost for Host {
    fn vm_host_and_window_realm(&mut self) -> (&mut dyn VmHost, &mut WindowRealm) {
      (&mut self.host_ctx, &mut self.window)
    }
  }

  fn get_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Value {
    let key_s = scope.alloc_string(name).unwrap();
    scope.push_root(Value::String(key_s)).unwrap();
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)
      .unwrap()
      .unwrap_or(Value::Undefined)
  }

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string");
    };
    heap.get_string(s).unwrap().to_utf8_lossy().to_string()
  }

  fn read_log(vm: &mut Vm, scope: &mut Scope<'_>, arr: GcObject) -> Vec<String> {
    let len_key = alloc_key(scope, "length").unwrap();
    let len_val = vm.get(scope, arr, len_key).unwrap();
    let len = number_to_u64(len_val).unwrap() as usize;
    let mut out = Vec::new();
    for idx in 0..len {
      let k = alloc_key(scope, &idx.to_string()).unwrap();
      let v = vm.get(scope, arr, k).unwrap();
      out.push(get_string(scope.heap(), v));
    }
    out
  }

  #[test]
  fn xhr_constructor_and_constants_exist() -> crate::Result<()> {
    let fetcher = Arc::new(MockFetcher::default());
    let mut host = Host::new(fetcher);
    let mut event_loop = EventLoop::<Host>::new();

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
      hooks.set_event_loop(event_loop);
      let (vm_host, window) = host.vm_host_and_window_realm();
      window.reset_interrupt();
      let result = window.exec_script_with_host_and_hooks(
        vm_host,
        &mut hooks,
        "globalThis.__t = typeof XMLHttpRequest;\n\
         globalThis.__u = XMLHttpRequest.UNSENT;\n\
         globalThis.__d = XMLHttpRequest.DONE;",
      );
      if let Some(err) = hooks.finish(window.heap_mut()) {
        return Err(err);
      }
      result
        .map(|_| ())
        .map_err(|e| vm_error_to_event_loop_error(window.heap_mut(), e))
    })?;

    let outcome = event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);

    let (_vm, realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    let t_val = get_prop(&mut scope, global, "__t");
    assert_eq!(get_string(scope.heap(), t_val), "function");
    assert_eq!(get_prop(&mut scope, global, "__u"), Value::Number(0.0));
    assert_eq!(get_prop(&mut scope, global, "__d"), Value::Number(4.0));
    Ok(())
  }

  #[test]
  fn xhr_open_fires_readystatechange() -> crate::Result<()> {
    let fetcher = Arc::new(MockFetcher::default());
    let mut host = Host::new(fetcher);
    let mut event_loop = EventLoop::<Host>::new();

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
      hooks.set_event_loop(event_loop);
      let (vm_host, window) = host.vm_host_and_window_realm();
      window.reset_interrupt();
      let result = window.exec_script_with_host_and_hooks(
        vm_host,
        &mut hooks,
        "globalThis.__log = [];\n\
         var xhr = new XMLHttpRequest();\n\
         xhr.addEventListener('readystatechange', function(){ globalThis.__log.push('listener:' + xhr.readyState); });\n\
         xhr.onreadystatechange = function(){ globalThis.__log.push('handler:' + xhr.readyState); };\n\
         xhr.open('GET', '/ok', true);\n\
         globalThis.__rs = xhr.readyState;",
      );
      if let Some(err) = hooks.finish(window.heap_mut()) {
        return Err(err);
      }
      result.map(|_| ()).map_err(|e| vm_error_to_event_loop_error(window.heap_mut(), e))
    })?;

    let outcome = event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);

    let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    assert_eq!(get_prop(&mut scope, global, "__rs"), Value::Number(1.0));
    let Value::Object(arr) = get_prop(&mut scope, global, "__log") else {
      panic!("expected array");
    };
    let log = read_log(vm, &mut scope, arr);
    assert!(log.iter().any(|s| s == "listener:1"), "log={log:?}");
    assert!(log.iter().any(|s| s == "handler:1"), "log={log:?}");
    Ok(())
  }

  #[test]
  fn xhr_send_success_load_and_loadend() -> crate::Result<()> {
    let fetcher = Arc::new(MockFetcher::default());
    let mut host = Host::new(fetcher.clone());
    let mut event_loop = EventLoop::<Host>::new();

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
      hooks.set_event_loop(event_loop);
      let (vm_host, window) = host.vm_host_and_window_realm();
      window.reset_interrupt();
      let result = window.exec_script_with_host_and_hooks(
        vm_host,
        &mut hooks,
        "globalThis.__log = [];\n\
         var xhr = new XMLHttpRequest();\n\
         xhr.onload = function(){ globalThis.__log.push('load:' + xhr.status + ':' + xhr.responseText); };\n\
         xhr.addEventListener('loadend', function(){ globalThis.__log.push('loadend'); });\n\
         xhr.open('GET', '/ok', true);\n\
         xhr.send();",
      );
      if let Some(err) = hooks.finish(window.heap_mut()) {
        return Err(err);
      }
      result.map(|_| ()).map_err(|e| vm_error_to_event_loop_error(window.heap_mut(), e))
    })?;

    let outcome = event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);

    // Assert JS-observable behavior.
    let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    let Value::Object(arr) = get_prop(&mut scope, global, "__log") else {
      panic!("expected array");
    };
    let log = read_log(vm, &mut scope, arr);
    assert!(log.iter().any(|s| s == "load:200:hello"), "log={log:?}");
    assert!(log.iter().any(|s| s == "loadend"), "log={log:?}");

    // Assert fetcher was called.
    let reqs = fetcher.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "GET");
    assert_eq!(reqs[0].url, "https://example.invalid/ok");
    Ok(())
  }

  #[test]
  fn xhr_send_error_calls_onerror() -> crate::Result<()> {
    let fetcher = Arc::new(MockFetcher::default());
    let mut host = Host::new(fetcher);
    let mut event_loop = EventLoop::<Host>::new();

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
      hooks.set_event_loop(event_loop);
      let (vm_host, window) = host.vm_host_and_window_realm();
      window.reset_interrupt();
      let result = window.exec_script_with_host_and_hooks(
        vm_host,
        &mut hooks,
        "globalThis.__log = [];\n\
         var xhr = new XMLHttpRequest();\n\
         xhr.onerror = function(){ globalThis.__log.push('error:' + xhr.status); };\n\
         xhr.open('GET', '/err', true);\n\
         xhr.send();",
      );
      if let Some(err) = hooks.finish(window.heap_mut()) {
        return Err(err);
      }
      result
        .map(|_| ())
        .map_err(|e| vm_error_to_event_loop_error(window.heap_mut(), e))
    })?;

    let outcome = event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);

    let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    let Value::Object(arr) = get_prop(&mut scope, global, "__log") else {
      panic!("expected array");
    };
    let log = read_log(vm, &mut scope, arr);
    assert!(log.iter().any(|s| s == "error:0"), "log={log:?}");
    Ok(())
  }

  #[test]
  fn xhr_remove_event_listener_prevents_dispatch() -> crate::Result<()> {
    let fetcher = Arc::new(MockFetcher::default());
    let mut host = Host::new(fetcher);
    let mut event_loop = EventLoop::<Host>::new();

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
      hooks.set_event_loop(event_loop);
      let (vm_host, window) = host.vm_host_and_window_realm();
      window.reset_interrupt();
      let result = window.exec_script_with_host_and_hooks(
        vm_host,
        &mut hooks,
        "globalThis.__log = [];\n\
         var xhr = new XMLHttpRequest();\n\
         function listener(){ globalThis.__log.push('listener'); }\n\
         xhr.addEventListener('load', listener);\n\
         xhr.removeEventListener('load', listener);\n\
         xhr.onload = function(){ globalThis.__log.push('onload'); };\n\
         xhr.open('GET', '/ok', true);\n\
         xhr.send();",
      );
      if let Some(err) = hooks.finish(window.heap_mut()) {
        return Err(err);
      }
      result
        .map(|_| ())
        .map_err(|e| vm_error_to_event_loop_error(window.heap_mut(), e))
    })?;

    let outcome = event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(outcome, RunUntilIdleOutcome::Idle);

    let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    let Value::Object(arr) = get_prop(&mut scope, global, "__log") else {
      panic!("expected array");
    };
    let log = read_log(vm, &mut scope, arr);
    assert!(log.iter().any(|s| s == "onload"), "log={log:?}");
    assert!(!log.iter().any(|s| s == "listener"), "log={log:?}");
    Ok(())
  }
}
